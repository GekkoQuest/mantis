//! The client simulation: the [`TickHandler`] the simulation thread runs.
//!
//! Each tick, in order:
//! 1. Drain authoritative snapshots: map server ticks to host time, extend remote entity
//!    histories, and keep the newest local-avatar state.
//! 2. Reconcile the local avatar against it (rewind and replay), turning any correction
//!    into a decaying visual offset for the render world.
//! 3. Take the input the render thread accumulated, build the move intent with the
//!    quantized look, predict it, and send it.
//! 4. Publish the render world: the local avatar as latest predicted state plus velocity,
//!    remote entities as interpolation windows behind render time.
//!
//! Nothing here allocates after construction; the snapshot channel recycles preallocated
//! frames, and remote tracks live in fixed-capacity storage.
//!
//! Snapshots arrive through [`crate::snapshot`], the single module boundary behind which
//! the decoded snapshot view is a placeholder until `mantis-adapter-contract` ships it.
//!
//! [`ClientSim::state_hash`] hashes everything the tick loop carries forward (predicted
//! avatar, local id, modifiers, correction, every remote track, last server tick) by IEEE
//! bit pattern. In dev builds (`debug_assertions`), `ClientSim::start_recording` captures
//! each tick's inputs, intent, and that hash into an MCRC recording ([`crate::recording`])
//! for the editor's replay viewer; release builds compile neither the hook nor its field.

use std::sync::Arc;
use std::time::Duration;

use mantis_core::hash::{StableHasher, StateHash};
#[cfg(debug_assertions)]
use mantis_core::{content::ContentHash, log::BuildId};

use crate::core_api::{
    AvatarKinematics, EntityId, InputSeq, MotionModifiers, MotionStep, MoveInput, Tick, TickRate,
};
use crate::input::accumulator::{InputAccumulator, TickInput};
use crate::input::intent::MoveIntentMap;
use crate::predict::metrics::CorrectionHistogram;
use crate::predict::{Predictor, Reconciliation};
use crate::recording::RecordState;
#[cfg(debug_assertions)]
use crate::recording::{Recorder, SimRecorder};
use crate::render_world::{Correction, LocalAvatar, REMOTE_WINDOW, RemoteEntity, RemoteSample, RenderWorld};
use crate::snapshot::{MarkerSender, ScheduledMarker, SnapshotInbox};
use crate::threads::sim_thread::TickHandler;
use crate::time::{HostInstant, ServerTimeline};

// ---------------------------------------------------------------------------------------
// Intents out
// ---------------------------------------------------------------------------------------

/// Where move intents go (the native adapter's input datagrams in production).
pub trait IntentSink: Send + 'static {
    /// Sends one move intent.
    fn send_move(&mut self, input: &MoveInput);
}

// ---------------------------------------------------------------------------------------
// Remote tracks
// ---------------------------------------------------------------------------------------

/// Samples of history kept per remote entity.
pub const REMOTE_HISTORY: usize = 8;

#[derive(Clone, Copy, Debug)]
struct Track {
    id: EntityId,
    samples: [RemoteSample; REMOTE_HISTORY],
    len: usize,
    last_arrival: HostInstant,
}

impl Track {
    fn samples(&self) -> &[RemoteSample] {
        self.samples.get(..self.len).unwrap_or(&[])
    }

    /// Appends a sample newer than every held one; older or equal ones are ignored.
    fn push(&mut self, s: RemoteSample) {
        if let Some(last) = self.samples().last()
            && s.time <= last.time
        {
            return;
        }
        if self.len == REMOTE_HISTORY {
            self.samples.copy_within(1.., 0);
            self.len -= 1;
        }
        if let Some(slot) = self.samples.get_mut(self.len) {
            *slot = s;
            self.len += 1;
        }
    }

    /// The window covering interpolation instant `t` and the following samples.
    fn window(&self, t: HostInstant) -> &[RemoteSample] {
        let s = self.samples();
        let start = s.iter().rposition(|x| x.time <= t).unwrap_or(0);
        let end = (start + REMOTE_WINDOW).min(s.len());
        s.get(start..end).unwrap_or(&[])
    }
}

/// Remote entity histories in fixed storage. The index is a vector sorted by entity id
/// (binary search; insertion shifts within preallocated capacity), so lookups are
/// deterministic and nothing allocates after construction.
#[derive(Debug)]
struct RemoteTracks {
    index: Vec<(EntityId, usize)>,
    slots: Vec<Option<Track>>,
    free: Vec<usize>,
    refused: u64,
}

impl RemoteTracks {
    fn with_capacity(n: usize) -> Self {
        Self {
            index: Vec::with_capacity(n),
            slots: vec![None; n],
            free: (0..n).rev().collect(),
            refused: 0,
        }
    }

    fn upsert(&mut self, id: EntityId, sample: RemoteSample, arrival: HostInstant) {
        let slot = match self.index.binary_search_by_key(&id, |e| e.0) {
            Ok(pos) => self.index.get(pos).map_or(usize::MAX, |e| e.1),
            Err(pos) => {
                let Some(i) = self.free.pop() else {
                    self.refused = self.refused.saturating_add(1);
                    return;
                };
                self.index.insert(pos, (id, i));
                if let Some(s) = self.slots.get_mut(i) {
                    *s = Some(Track {
                        id,
                        samples: [RemoteSample::default(); REMOTE_HISTORY],
                        len: 0,
                        last_arrival: arrival,
                    });
                }
                i
            }
        };
        if let Some(Some(t)) = self.slots.get_mut(slot) {
            t.push(sample);
            t.last_arrival = arrival;
        }
    }

    fn remove(&mut self, id: EntityId) {
        if let Ok(pos) = self.index.binary_search_by_key(&id, |e| e.0) {
            let (_, i) = self.index.remove(pos);
            if let Some(s) = self.slots.get_mut(i) {
                *s = None;
            }
            self.free.push(i);
        }
    }

    fn evict_older_than(&mut self, cutoff: HostInstant) {
        for i in 0..self.slots.len() {
            let stale = matches!(self.slots.get(i), Some(Some(t)) if t.last_arrival < cutoff);
            if stale && let Some(Some(t)) = self.slots.get(i) {
                let id = t.id;
                self.remove(id);
            }
        }
    }

    fn iter(&self) -> impl Iterator<Item = &Track> {
        self.slots.iter().filter_map(Option::as_ref)
    }

    /// Tracks in entity id order (independent of slot assignment).
    fn iter_by_id(&self) -> impl Iterator<Item = &Track> {
        self.index
            .iter()
            .filter_map(|&(_, i)| self.slots.get(i).and_then(Option::as_ref))
    }

    fn len(&self) -> usize {
        self.index.len()
    }
}

// ---------------------------------------------------------------------------------------
// The client simulation
// ---------------------------------------------------------------------------------------

/// The newest authoritative avatar facts from one tick's snapshots.
#[derive(Clone, Copy, Debug)]
struct Authoritative<S> {
    ack: Option<InputSeq>,
    id: EntityId,
    state: S,
    mods: MotionModifiers,
}

/// Client simulation tuning.
#[derive(Clone, Copy, PartialEq, Debug)]
pub struct ClientSimConfig {
    /// The server's tick rate (snapshots are stamped in server ticks).
    pub server_rate: TickRate,
    /// Decay window for visual corrections.
    pub correction_window: Duration,
    /// Corrections larger than this snap instead of smoothing (teleports).
    pub snap_distance: f32,
    /// Maximum remote entities tracked.
    pub remote_capacity: usize,
    /// Remote entities with no update for this long are dropped.
    pub remote_timeout: Duration,
    /// Interpolation delay (must match the render world's presentation config).
    pub interpolation_delay: Duration,
    /// Unacknowledged inputs kept for replay.
    pub input_buffer: usize,
    /// How slowly the server timeline estimate rises (see [`ServerTimeline::new`]).
    pub timeline_rise_shift: u32,
}

/// Counters for diagnostics and tests.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct ClientSimStats {
    /// Ticks run.
    pub ticks: u64,
    /// Snapshots applied.
    pub snapshots: u64,
    /// Snapshots ignored as older than one already applied.
    pub stale_snapshots: u64,
    /// Move intents sent.
    pub moves_sent: u64,
    /// Hard resets of the local avatar (spawn, avatar change, oversized correction).
    pub resets: u64,
    /// Remote entities refused because tracking was full.
    pub remotes_refused: u64,
}

/// The client simulation for one world session.
pub struct ClientSim<M: MotionStep, O: IntentSink> {
    config: ClientSimConfig,
    predictor: Predictor<M>,
    ground: Arc<M::Ground>,
    mods: MotionModifiers,
    accumulator: Arc<InputAccumulator>,
    intents: MoveIntentMap,
    inbox: SnapshotInbox<M::State>,
    outbox: O,
    markers: Option<MarkerSender>,
    timeline: ServerTimeline,
    local: Option<EntityId>,
    correction: Correction,
    remotes: RemoteTracks,
    corrections: CorrectionHistogram,
    last_server_tick: Option<Tick>,
    stats: ClientSimStats,
    #[cfg(debug_assertions)]
    recorder: Option<Box<dyn SimRecorder<M::State>>>,
}

impl<M: MotionStep, O: IntentSink> core::fmt::Debug for ClientSim<M, O> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("ClientSim")
            .field("local", &self.local)
            .field("stats", &self.stats)
            .finish_non_exhaustive()
    }
}

/// Construction parameters for [`ClientSim`].
pub struct ClientSimParts<M: MotionStep, O: IntentSink> {
    /// Tuning.
    pub config: ClientSimConfig,
    /// The motion model (identical to the server's).
    pub motion: M,
    /// The ground model.
    pub ground: Arc<M::Ground>,
    /// Client tick duration in seconds, exactly as the server integrates.
    pub dt: f32,
    /// Initial avatar state, replaced by the first authoritative snapshot.
    pub initial_state: M::State,
    /// Input hand-off from the render thread.
    pub accumulator: Arc<InputAccumulator>,
    /// Action-to-intent mapping.
    pub intents: MoveIntentMap,
    /// Snapshot inbox.
    pub inbox: SnapshotInbox<M::State>,
    /// Intent outbox.
    pub outbox: O,
    /// Where timeline markers go (the render thread's presentation), if anywhere.
    pub markers: Option<MarkerSender>,
}

impl<M: MotionStep, O: IntentSink> ClientSim<M, O> {
    /// Builds the simulation.
    pub fn new(parts: ClientSimParts<M, O>) -> Self {
        let c = parts.config;
        Self {
            config: c,
            predictor: Predictor::new(
                parts.motion,
                parts.initial_state,
                InputSeq(0),
                c.input_buffer,
                parts.dt,
            ),
            ground: parts.ground,
            mods: MotionModifiers::default(),
            accumulator: parts.accumulator,
            intents: parts.intents,
            inbox: parts.inbox,
            outbox: parts.outbox,
            markers: parts.markers,
            timeline: ServerTimeline::new(c.server_rate, c.timeline_rise_shift),
            local: None,
            correction: Correction::NONE,
            remotes: RemoteTracks::with_capacity(c.remote_capacity),
            corrections: CorrectionHistogram::new(),
            last_server_tick: None,
            stats: ClientSimStats::default(),
            #[cfg(debug_assertions)]
            recorder: None,
        }
    }

    /// Counters.
    pub fn stats(&self) -> ClientSimStats {
        let mut s = self.stats;
        s.remotes_refused = self.remotes.refused;
        s
    }

    /// The predictor.
    pub fn predictor(&self) -> &Predictor<M> {
        &self.predictor
    }

    /// Correction magnitudes recorded at every reconciliation.
    pub fn corrections(&self) -> &CorrectionHistogram {
        &self.corrections
    }

    /// The local avatar entity, once known.
    pub fn local(&self) -> Option<EntityId> {
        self.local
    }

    /// Remote entities tracked.
    pub fn remote_count(&self) -> usize {
        self.remotes.len()
    }

    /// The intent outbox.
    pub fn outbox(&self) -> &O {
        &self.outbox
    }

    /// The movement modifiers future predictions integrate with.
    pub fn modifiers(&self) -> MotionModifiers {
        self.mods
    }

    /// The visual correction being decayed for the local avatar.
    pub fn correction(&self) -> Correction {
        self.correction
    }

    /// The newest server tick applied from a snapshot.
    pub fn last_server_tick(&self) -> Option<Tick> {
        self.last_server_tick
    }

    /// Stable hash of the simulation state the tick loop carries forward, by IEEE bit
    /// pattern: the predicted avatar state, next input sequence and last acknowledgement,
    /// the local avatar id, the prediction modifiers, the correction, the last server
    /// tick, and every remote track (in entity id order: id, samples, last arrival).
    ///
    /// Replay compares it tick by tick against a recording.
    pub fn state_hash(&self) -> u64
    where
        M::State: RecordState,
    {
        self.hash_with(RecordState::hash_state)
    }

    fn hash_with(&self, state: impl FnOnce(&M::State, &mut StableHasher)) -> u64 {
        let mut h = StableHasher::new();
        state(self.predictor.state(), &mut h);
        h.write_u32(self.predictor.next_seq().0);
        self.predictor.last_ack().map(|s| s.0).state_hash(&mut h);
        self.local.state_hash(&mut h);
        h.write_f32(self.mods.speed_scale);
        h.write_f32(self.mods.jump_scale);
        h.write_f32(self.mods.gravity_scale);
        self.correction.offset.state_hash(&mut h);
        h.write_u64(self.correction.start.as_nanos());
        h.write_u64(self.correction.window.as_secs());
        h.write_u32(self.correction.window.subsec_nanos());
        self.last_server_tick.state_hash(&mut h);
        h.write_u64(self.remotes.len() as u64);
        for t in self.remotes.iter_by_id() {
            t.id.state_hash(&mut h);
            let samples = t.samples();
            h.write_u64(samples.len() as u64);
            for s in samples {
                h.write_u64(s.time.as_nanos());
                s.position.state_hash(&mut h);
                s.velocity.state_hash(&mut h);
                h.write_u16(s.yaw.0);
            }
            h.write_u64(t.last_arrival.as_nanos());
        }
        h.finish()
    }

    /// Starts an MCRC recording (dev builds only; see [`crate::recording`]) of every
    /// following tick for a session of `build` against `content`, holding at most `cap`
    /// bytes, all reserved now. Replaces (and discards) a recording already running.
    ///
    /// For a replayable recording, start before the first tick: a replay starts from a
    /// freshly built simulation.
    #[cfg(debug_assertions)]
    pub fn start_recording(&mut self, build: BuildId, content: ContentHash, cap: usize)
    where
        M::State: RecordState,
    {
        self.recorder = Some(Box::new(Recorder::<M::State>::new(build, content, cap)));
    }

    /// True while a recording runs (dev builds only).
    #[cfg(debug_assertions)]
    pub fn is_recording(&self) -> bool {
        self.recorder.is_some()
    }

    /// Stops recording and returns the finished MCRC bytes, or `None` when not recording
    /// (dev builds only).
    #[cfg(debug_assertions)]
    pub fn take_recording(&mut self) -> Option<Vec<u8>> {
        self.recorder.take().map(SimRecorder::finish)
    }

    /// Recording hook at the start of a tick. Compiles to nothing in release builds.
    #[inline]
    fn record_begin(&mut self, tick: Tick, tick_time: HostInstant) {
        #[cfg(debug_assertions)]
        if let Some(r) = self.recorder.as_mut() {
            r.begin_tick(tick, tick_time);
        }
        #[cfg(not(debug_assertions))]
        let _ = (self, tick, tick_time);
    }

    /// Recording hook at the end of a tick. Compiles to nothing in release builds.
    #[inline]
    fn record_end(&mut self, input: &TickInput, sent: Option<&MoveInput>) {
        #[cfg(debug_assertions)]
        if let Some(r) = self.recorder.as_deref() {
            let hash = self.hash_with(|s, h| r.hash_state(s, h));
            if let Some(r) = self.recorder.as_mut() {
                r.end_tick(input, sent, hash);
            }
        }
        #[cfg(not(debug_assertions))]
        let _ = (self, input, sent);
    }

    fn drain_snapshots(&mut self) -> Option<Authoritative<M::State>> {
        let mut newest_local = None;
        let (last_server_tick, stats, timeline, remotes, markers) = (
            &mut self.last_server_tick,
            &mut self.stats,
            &mut self.timeline,
            &mut self.remotes,
            &mut self.markers,
        );
        #[cfg(debug_assertions)]
        let recorder = &mut self.recorder;
        self.inbox.drain(|frame| {
            #[cfg(debug_assertions)]
            if let Some(r) = recorder.as_mut() {
                r.frame(frame);
            }
            if last_server_tick.is_some_and(|t| frame.server_tick <= t) {
                stats.stale_snapshots = stats.stale_snapshots.saturating_add(1);
                return;
            }
            *last_server_tick = Some(frame.server_tick);
            stats.snapshots = stats.snapshots.saturating_add(1);
            timeline.observe(frame.server_tick, frame.received_at);
            for r in &frame.remotes {
                if !r.position.is_finite() || !r.velocity.is_finite() || r.tick > frame.server_tick {
                    continue; // fail closed on corrupt state
                }
                let Some(at) = timeline.host_time(r.tick) else {
                    continue;
                };
                let sample = RemoteSample {
                    time: at,
                    position: r.position,
                    velocity: r.velocity,
                    yaw: r.yaw,
                };
                remotes.upsert(r.id, sample, frame.received_at);
            }
            for id in &frame.removed {
                remotes.remove(*id);
            }
            if let Some(out) = markers.as_mut() {
                for m in &frame.markers {
                    let at = timeline.host_time(m.at).unwrap_or(frame.received_at);
                    let _ = out.send(ScheduledMarker { marker: *m, at });
                }
            }
            if let Some((id, state)) = frame.local {
                newest_local = Some(Authoritative {
                    ack: frame.ack,
                    id,
                    state,
                    mods: frame.local_mods,
                });
            }
        });
        newest_local
    }

    fn apply_authoritative(
        &mut self,
        tick_time: HostInstant,
        ack: Option<InputSeq>,
        id: EntityId,
        state: &M::State,
    ) {
        if self.local != Some(id) {
            // Spawn or avatar change: adopt outright.
            self.local = Some(id);
            self.predictor.reset(*state, self.predictor.next_seq());
            self.correction = Correction::NONE;
            self.stats.resets = self.stats.resets.saturating_add(1);
            return;
        }
        let Some(ack) = ack else {
            // The server has not applied any of our inputs yet; nothing to reconcile.
            return;
        };
        let r = self.predictor.reconcile(&self.ground, ack, state);
        match r {
            Reconciliation::Matched => self.corrections.record(0.0),
            Reconciliation::Corrected { correction, .. } => {
                self.corrections.record(correction.length());
                let residual = self.correction.at(tick_time) + correction;
                self.correction = if residual.length() > self.config.snap_distance || !residual.is_finite() {
                    self.stats.resets = self.stats.resets.saturating_add(1);
                    Correction::NONE
                } else {
                    Correction {
                        offset: residual,
                        start: tick_time,
                        window: self.config.correction_window,
                    }
                };
            }
            Reconciliation::Stale | Reconciliation::Invalid => {}
        }
    }

    fn publish(&self, tick_time: HostInstant, world: &mut RenderWorld) {
        if let Some(id) = self.local {
            let s = self.predictor.state();
            world.set_local(Some(LocalAvatar {
                id,
                state_time: tick_time,
                position: s.position(),
                velocity: s.velocity(),
                yaw: s.yaw(),
                correction: self.correction,
            }));
        }
        let t = tick_time.saturating_sub(self.config.interpolation_delay);
        for track in self.remotes.iter() {
            if let Some(entity) = RemoteEntity::new(track.id, track.window(t)) {
                // Overflow is counted by the world.
                let _ = world.push_remote(entity);
            }
        }
    }
}

impl<M: MotionStep, O: IntentSink> TickHandler for ClientSim<M, O> {
    fn tick(&mut self, tick: Tick, tick_time: HostInstant, world: &mut RenderWorld) {
        self.stats.ticks = self.stats.ticks.saturating_add(1);
        self.record_begin(tick, tick_time);
        if let Some(auth) = self.drain_snapshots() {
            // Future predictions integrate with the authoritative modifiers; replays use
            // the modifiers each input was originally predicted with.
            self.mods = auth.mods;
            self.apply_authoritative(tick_time, auth.ack, auth.id, &auth.state);
        }
        self.remotes
            .evict_older_than(tick_time.saturating_sub(self.config.remote_timeout));

        let input = self.accumulator.take();
        let mut sent = None;
        if self.local.is_some() {
            let mv = self.intents.build(&input, self.predictor.next_seq(), tick);
            if self.predictor.predict(&self.ground, &self.mods, mv).is_ok() {
                self.outbox.send_move(&mv);
                self.stats.moves_sent = self.stats.moves_sent.saturating_add(1);
                sent = Some(mv);
            }
        }
        self.publish(tick_time, world);
        self.record_end(&input, sent.as_ref());
    }
}

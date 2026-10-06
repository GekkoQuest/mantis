//! Presentation graphs (decision 0006): effects, sounds, camera shakes, and animation
//! triggers bound to the timeline markers gameplay graphs emit.
//!
//! Markers arrive with snapshots. [`Presenter::on_marker`] looks the marker id up in the
//! loaded [`PresentationLibrary`] and schedules every matching action at the marker's host
//! time plus the action's delay; [`Presenter::update`] fires due actions, in time order,
//! into a [`PresentationSink`] (the client glue that owns particles, audio, the camera,
//! and animation instances).
//!
//! Presentation is best-effort by design. Nothing here is an error:
//! - a marker id no presentation graph binds is dropped and counted
//!   ([`PresentationStats::unbound`]): gameplay content may legitimately have markers only
//!   a legacy adapter or nobody consumes;
//! - a bound id whose filters reject the kind is counted ([`PresentationStats::filtered`]);
//! - a marker delivered twice is fired once ([`PresentationStats::duplicates`]);
//! - an action due further in the past than [`PresentationConfig::max_lateness`] is
//!   dropped ([`PresentationStats::late`]) rather than playing a stale burst;
//! - a full schedule drops the new action ([`PresentationStats::overflow`]);
//! - a target-anchored action on a marker without a target is skipped
//!   ([`PresentationStats::missing_target`]).
//!
//! After warm-up, scheduling and firing allocate nothing: the schedule and the duplicate
//! window are fixed-capacity.

use std::time::Duration;

use mantis_core::content::ContentHash;
use mantis_core::ecs::EntityId;
use mantis_core::graph::{GraphInstanceId, MarkerId, MarkerKind, TimelineMarker};
use mantis_formats::presentation::{Action, ActionOp, Anchor, MarkerFilter, PresentationGraph};

use crate::time::HostInstant;

/// One loaded binding, flattened.
#[derive(Clone, Copy, Debug)]
struct Entry {
    id: MarkerId,
    filter: MarkerFilter,
    /// Range into [`PresentationLibrary::actions`].
    first: u32,
    count: u32,
    /// The content hash of the graph it came from (for [`PresentationLibrary::unload`]).
    source: ContentHash,
}

/// Every loaded presentation graph, indexed by marker id.
#[derive(Clone, Debug, Default)]
pub struct PresentationLibrary {
    entries: Vec<Entry>,
    actions: Vec<Action>,
}

impl PresentationLibrary {
    /// An empty library.
    pub fn new() -> Self {
        Self::default()
    }

    /// Adds every binding of `graph`. Bindings of several graphs for the same marker id
    /// all run, in load order.
    pub fn load(&mut self, graph: &PresentationGraph) {
        let source = graph.content_hash();
        for b in &graph.bindings {
            let first = u32::try_from(self.actions.len()).unwrap_or(u32::MAX);
            self.actions.extend_from_slice(&b.actions);
            self.entries.push(Entry {
                id: MarkerId {
                    graph: mantis_core::graph::GraphId(b.graph),
                    node: mantis_core::graph::NodeKey(b.node),
                },
                filter: b.filter,
                first,
                count: u32::try_from(b.actions.len()).unwrap_or(0),
                source,
            });
        }
        // Stable: equal ids keep load order.
        self.entries.sort_by_key(|e| e.id);
    }

    /// Removes every binding `graph` added (hot reload unloads the old graph, then
    /// loads the new one). Returns the bindings removed.
    pub fn unload(&mut self, graph: &PresentationGraph) -> usize {
        let source = graph.content_hash();
        let before = self.entries.len();
        let old_actions = core::mem::take(&mut self.actions);
        self.entries.retain(|e| e.source != source);
        for e in &mut self.entries {
            let first = e.first as usize;
            let range = old_actions.get(first..first + e.count as usize).unwrap_or(&[]);
            e.first = u32::try_from(self.actions.len()).unwrap_or(u32::MAX);
            self.actions.extend_from_slice(range);
        }
        before - self.entries.len()
    }

    /// Removes everything (scope exit).
    pub fn clear(&mut self) {
        self.entries.clear();
        self.actions.clear();
    }

    /// Bindings loaded.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Whether nothing is loaded.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    fn entries_for(&self, id: MarkerId) -> &[Entry] {
        let start = self.entries.partition_point(|e| e.id < id);
        let end = self.entries.partition_point(|e| e.id <= id);
        self.entries.get(start..end).unwrap_or(&[])
    }

    fn actions_of(&self, e: &Entry) -> &[Action] {
        let first = e.first as usize;
        self.actions.get(first..first + e.count as usize).unwrap_or(&[])
    }
}

/// Whether `filter` accepts `kind`.
pub fn filter_matches(filter: MarkerFilter, kind: MarkerKind) -> bool {
    match (filter, kind) {
        (MarkerFilter::Any, _)
        | (MarkerFilter::CastStart, MarkerKind::CastStart)
        | (MarkerFilter::Impact, MarkerKind::Impact { .. })
        | (MarkerFilter::Expire, MarkerKind::Expire) => true,
        (MarkerFilter::Tick(n), MarkerKind::TickN(t)) => n == 0 || n == t,
        (MarkerFilter::Package(p), MarkerKind::Package(k)) => p == k.0,
        _ => false,
    }
}

/// Limits.
#[derive(Clone, Copy, Debug)]
pub struct PresentationConfig {
    /// Most actions scheduled at once.
    pub max_pending: usize,
    /// Markers remembered for duplicate suppression.
    pub dedupe_window: usize,
    /// Actions due further in the past than this are dropped.
    pub max_lateness: Duration,
}

impl Default for PresentationConfig {
    fn default() -> Self {
        Self {
            max_pending: 1024,
            dedupe_window: 256,
            max_lateness: Duration::from_millis(250),
        }
    }
}

/// Counters. Nothing in presentation is an error; everything unusual is counted.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct PresentationStats {
    /// Markers received.
    pub markers: u64,
    /// Markers whose id no loaded graph binds (dropped).
    pub unbound: u64,
    /// Markers whose id is bound but no binding's filter accepted the kind.
    pub filtered: u64,
    /// Markers received again (ignored).
    pub duplicates: u64,
    /// Actions scheduled.
    pub scheduled: u64,
    /// Actions fired into the sink.
    pub fired: u64,
    /// Actions dropped because they were already too late.
    pub late: u64,
    /// Actions dropped because the schedule was full.
    pub overflow: u64,
    /// Target-anchored actions on markers without a target.
    pub missing_target: u64,
}

/// A fired action, resolved to an entity.
#[derive(Clone, Copy, PartialEq, Debug)]
pub struct Cue {
    /// The entity the action is anchored on.
    pub anchor: EntityId,
    /// Offset from the anchor, meters.
    pub offset: [f32; 3],
    /// The marker's source entity.
    pub source: EntityId,
    /// The graph instance that emitted the marker.
    pub instance: GraphInstanceId,
    /// When it was due.
    pub due: HostInstant,
}

/// Where fired actions go: the client glue that owns effects, audio, camera, animation.
pub trait PresentationSink {
    /// Spawn a particle effect.
    fn spawn_effect(
        &mut self,
        cue: &Cue,
        effect: mantis_core::content::ContentHash,
        scale: f32,
        follow: bool,
    );
    /// Play a sound.
    fn play_sound(&mut self, cue: &Cue, sound: u32, volume: f32, pitch: f32, follow: bool);
    /// Shake the camera if it is within `radius` of the anchor.
    fn camera_shake(&mut self, cue: &Cue, amplitude: f32, frequency: f32, duration: f32, radius: f32);
    /// Fire an animation trigger on the anchor.
    fn anim_trigger(&mut self, cue: &Cue, parameter: u32);
}

#[derive(Clone, Copy, Debug)]
struct Pending {
    due: HostInstant,
    seq: u64,
    action: Action,
    cue: Cue,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
struct MarkerKey {
    instance: GraphInstanceId,
    id: MarkerId,
    kind: MarkerKind,
    at: mantis_core::time::Tick,
}

/// Schedules and fires presentation actions.
#[derive(Debug)]
pub struct Presenter {
    library: PresentationLibrary,
    config: PresentationConfig,
    /// Unordered; `update` fires the earliest due first.
    pending: Vec<Pending>,
    recent: Vec<MarkerKey>,
    recent_next: usize,
    seq: u64,
    stats: PresentationStats,
}

impl Presenter {
    /// A presenter over `library`.
    pub fn new(library: PresentationLibrary, config: PresentationConfig) -> Self {
        Self {
            library,
            pending: Vec::with_capacity(config.max_pending),
            recent: Vec::with_capacity(config.dedupe_window),
            recent_next: 0,
            seq: 0,
            stats: PresentationStats::default(),
            config,
        }
    }

    /// Counters.
    pub fn stats(&self) -> PresentationStats {
        self.stats
    }

    /// Actions waiting to fire.
    pub fn pending(&self) -> usize {
        self.pending.len()
    }

    /// Drops every scheduled action (scope exit, teleport).
    pub fn clear(&mut self) {
        self.pending.clear();
        self.recent.clear();
        self.recent_next = 0;
    }

    fn seen(&mut self, key: MarkerKey) -> bool {
        if self.recent.contains(&key) {
            return true;
        }
        if self.config.dedupe_window == 0 {
            return false;
        }
        if self.recent.len() < self.config.dedupe_window {
            self.recent.push(key);
        } else if let Some(slot) = self.recent.get_mut(self.recent_next) {
            *slot = key;
        }
        self.recent_next = (self.recent_next + 1) % self.config.dedupe_window;
        false
    }

    /// Schedules the actions bound to `marker`, which happens at host time `at` (the
    /// caller maps `marker.at` through its server timeline). `now` is the current host
    /// time, for the lateness check.
    pub fn on_marker(&mut self, marker: &TimelineMarker, at: HostInstant, now: HostInstant) {
        self.stats.markers += 1;
        let key = MarkerKey {
            instance: marker.instance,
            id: marker.id,
            kind: marker.kind,
            at: marker.at,
        };
        let library = core::mem::take(&mut self.library);
        let entries = library.entries_for(marker.id);
        if entries.is_empty() {
            self.stats.unbound += 1;
        } else if !entries.iter().any(|e| filter_matches(e.filter, marker.kind)) {
            self.stats.filtered += 1;
        } else if self.seen(key) {
            self.stats.duplicates += 1;
        } else {
            for e in entries.iter().filter(|e| filter_matches(e.filter, marker.kind)) {
                for action in library.actions_of(e) {
                    self.schedule(marker, *action, at, now);
                }
            }
        }
        self.library = library;
    }

    fn schedule(&mut self, marker: &TimelineMarker, action: Action, at: HostInstant, now: HostInstant) {
        let anchor = match action.anchor {
            Anchor::Source => marker.source,
            Anchor::Target => {
                let target = match marker.kind {
                    MarkerKind::Impact { target } => Some(target),
                    _ => marker.target,
                };
                let Some(t) = target else {
                    self.stats.missing_target += 1;
                    return;
                };
                t
            }
        };
        let delay = Duration::try_from_secs_f32(action.delay).unwrap_or_default();
        let due = at.saturating_add(delay);
        if now.saturating_since(due) > self.config.max_lateness {
            self.stats.late += 1;
            return;
        }
        if self.pending.len() >= self.config.max_pending {
            self.stats.overflow += 1;
            return;
        }
        self.seq += 1;
        self.pending.push(Pending {
            due,
            seq: self.seq,
            action,
            cue: Cue {
                anchor,
                offset: action.offset,
                source: marker.source,
                instance: marker.instance,
                due,
            },
        });
        self.stats.scheduled += 1;
    }

    /// Fires every action due at or before `now`, earliest first (ties in scheduling
    /// order). Returns how many fired.
    pub fn update(&mut self, now: HostInstant, sink: &mut impl PresentationSink) -> u32 {
        let mut fired = 0u32;
        loop {
            let next = self
                .pending
                .iter()
                .enumerate()
                .filter(|(_, p)| p.due <= now)
                .min_by_key(|(_, p)| (p.due, p.seq))
                .map(|(i, _)| i);
            let Some(index) = next else { break };
            let p = self.pending.swap_remove(index);
            if now.saturating_since(p.due) > self.config.max_lateness {
                self.stats.late += 1;
                continue;
            }
            match p.action.op {
                ActionOp::SpawnEffect {
                    effect,
                    scale,
                    follow,
                } => sink.spawn_effect(&p.cue, effect, scale, follow),
                ActionOp::PlaySound {
                    sound,
                    volume,
                    pitch,
                    follow,
                } => {
                    sink.play_sound(&p.cue, sound, volume, pitch, follow);
                }
                ActionOp::CameraShake {
                    amplitude,
                    frequency,
                    duration,
                    radius,
                } => {
                    sink.camera_shake(&p.cue, amplitude, frequency, duration, radius);
                }
                ActionOp::AnimTrigger { parameter } => sink.anim_trigger(&p.cue, parameter),
            }
            fired += 1;
            self.stats.fired += 1;
        }
        fired
    }
}

/// Camera shakes in flight, summed into a view offset. Fixed capacity; a new shake
/// replaces the weakest when full.
#[derive(Clone, Debug)]
pub struct CameraShakes {
    active: Vec<Shake>,
    capacity: usize,
}

#[derive(Clone, Copy, Debug)]
struct Shake {
    start: HostInstant,
    amplitude: f32,
    frequency: f32,
    duration: f32,
    phase: f32,
}

impl CameraShakes {
    /// Room for `capacity` simultaneous shakes.
    pub fn new(capacity: usize) -> Self {
        Self {
            active: Vec::with_capacity(capacity),
            capacity,
        }
    }

    /// Starts a shake. `scale` (0 to 1) attenuates it, for example by distance.
    pub fn add(&mut self, now: HostInstant, amplitude: f32, frequency: f32, duration: f32, scale: f32) {
        let amplitude = amplitude * scale.clamp(0.0, 1.0);
        if !(amplitude > 0.0 && duration > 0.0 && frequency.is_finite()) {
            return;
        }
        #[expect(clippy::cast_precision_loss)] // Only decorrelates phases.
        let phase = (now.as_nanos() % 1_000_003) as f32 * 1e-3;
        let shake = Shake {
            start: now,
            amplitude,
            frequency,
            duration,
            phase,
        };
        if self.active.len() < self.capacity {
            self.active.push(shake);
        } else if let Some(weakest) = self
            .active
            .iter_mut()
            .min_by(|a, b| a.amplitude.total_cmp(&b.amplitude))
            && weakest.amplitude < amplitude
        {
            *weakest = shake;
        }
    }

    /// The summed (yaw, pitch) offset in degrees at `now`; expired shakes are removed.
    #[expect(clippy::cast_possible_truncation)] // Seconds since a recent start fit f32.
    pub fn sample(&mut self, now: HostInstant) -> (f32, f32) {
        self.active
            .retain(|s| now.seconds_since(s.start) < f64::from(s.duration));
        let mut yaw = 0.0f32;
        let mut pitch = 0.0f32;
        for s in &self.active {
            let t = now.seconds_since(s.start) as f32;
            let decay = 1.0 - t / s.duration;
            let envelope = s.amplitude * decay * decay;
            let w = core::f32::consts::TAU * s.frequency * t + s.phase;
            yaw += envelope * mantis_core::math::sin(w);
            pitch += envelope * mantis_core::math::sin(w * 1.31 + 1.7);
        }
        (yaw, pitch)
    }

    /// Shakes in flight.
    pub fn len(&self) -> usize {
        self.active.len()
    }

    /// Whether none are in flight.
    pub fn is_empty(&self) -> bool {
        self.active.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use mantis_core::content::ContentHash;
    use mantis_core::graph::{GraphId, NodeKey, PackageMarker};
    use mantis_core::time::Tick;
    use mantis_formats::presentation::Binding;

    #[derive(Default)]
    struct Log {
        events: Vec<(u64, &'static str, EntityId)>,
    }

    impl PresentationSink for Log {
        fn spawn_effect(&mut self, cue: &Cue, _: ContentHash, _: f32, _: bool) {
            self.events.push((cue.due.as_nanos(), "effect", cue.anchor));
        }
        fn play_sound(&mut self, cue: &Cue, _: u32, _: f32, _: f32, _: bool) {
            self.events.push((cue.due.as_nanos(), "sound", cue.anchor));
        }
        fn camera_shake(&mut self, cue: &Cue, _: f32, _: f32, _: f32, _: f32) {
            self.events.push((cue.due.as_nanos(), "shake", cue.anchor));
        }
        fn anim_trigger(&mut self, cue: &Cue, _: u32) {
            self.events.push((cue.due.as_nanos(), "anim", cue.anchor));
        }
    }

    const CASTER: EntityId = EntityId::new(1, 0);
    const VICTIM: EntityId = EntityId::new(2, 0);

    fn action(delay: f32, anchor: Anchor, op: ActionOp) -> Action {
        Action {
            delay,
            anchor,
            offset: [0.0; 3],
            op,
        }
    }

    fn sound(id: u32) -> ActionOp {
        ActionOp::PlaySound {
            sound: id,
            volume: 1.0,
            pitch: 1.0,
            follow: false,
        }
    }

    fn library() -> PresentationLibrary {
        let effect = ActionOp::SpawnEffect {
            effect: ContentHash::from_bytes([3; 32]),
            scale: 1.0,
            follow: true,
        };
        let mut lib = PresentationLibrary::new();
        lib.load(&PresentationGraph {
            bindings: vec![
                Binding {
                    graph: 5,
                    node: 1,
                    filter: MarkerFilter::CastStart,
                    actions: vec![
                        action(0.2, Anchor::Source, sound(1)),
                        action(0.0, Anchor::Source, effect),
                    ],
                },
                Binding {
                    graph: 5,
                    node: 2,
                    filter: MarkerFilter::Impact,
                    actions: vec![action(
                        0.0,
                        Anchor::Target,
                        ActionOp::AnimTrigger { parameter: 9 },
                    )],
                },
                Binding {
                    graph: 5,
                    node: 3,
                    filter: MarkerFilter::Tick(2),
                    actions: vec![action(0.0, Anchor::Target, sound(3))],
                },
            ],
        });
        lib
    }

    fn marker(node: u16, kind: MarkerKind, instance: u64) -> TimelineMarker {
        TimelineMarker {
            id: MarkerId {
                graph: GraphId(5),
                node: NodeKey(node),
            },
            kind,
            at: Tick(100),
            offset: 0,
            source: CASTER,
            target: Some(VICTIM),
            instance: GraphInstanceId(instance),
        }
    }

    const MS: u64 = 1_000_000;

    #[test]
    fn a_graph_unloads_and_reloads_without_touching_the_others() {
        let mut lib = library();
        let other = PresentationGraph {
            bindings: vec![Binding {
                graph: 7,
                node: 1,
                filter: MarkerFilter::Any,
                actions: vec![action(0.0, Anchor::Source, sound(70))],
            }],
        };
        let old = PresentationGraph {
            bindings: vec![Binding {
                graph: 5,
                node: 4,
                filter: MarkerFilter::Expire,
                actions: vec![action(0.0, Anchor::Source, sound(40))],
            }],
        };
        lib.load(&other);
        lib.load(&old);
        assert_eq!(lib.len(), 5);
        assert_eq!(lib.unload(&old), 1);
        assert_eq!(lib.len(), 4);
        // The survivors still fire their own actions.
        let id = |graph: u32, node: u16| MarkerId {
            graph: GraphId(graph),
            node: NodeKey(node),
        };
        let first = lib.entries_for(id(5, 1)).first().copied();
        let cast: Vec<Action> = first.map(|e| lib.actions_of(&e).to_vec()).unwrap_or_default();
        assert_eq!(cast.len(), 2);
        assert_eq!(cast.first().map(|a| a.delay), Some(0.2));
        let seventy = lib.entries_for(id(7, 1)).first().copied();
        assert_eq!(
            seventy.map(|e| lib.actions_of(&e).to_vec()),
            Some(vec![action(0.0, Anchor::Source, sound(70))])
        );
        assert!(lib.entries_for(id(5, 4)).is_empty());
        // Unloading a graph that is not loaded removes nothing.
        assert_eq!(lib.unload(&old), 0);
    }

    #[test]
    fn unbound_marker_ids_are_dropped_and_counted_never_errors() {
        let mut p = Presenter::new(library(), PresentationConfig::default());
        let t = HostInstant::from_nanos(1000 * MS);
        // A marker id no graph binds: a node of a bound graph, and a whole unknown graph.
        p.on_marker(&marker(77, MarkerKind::CastStart, 1), t, t);
        let mut other = marker(1, MarkerKind::CastStart, 2);
        other.id.graph = GraphId(6);
        p.on_marker(&other, t, t);
        let mut log = Log::default();
        assert_eq!(p.update(t, &mut log), 0);
        let s = p.stats();
        assert_eq!((s.markers, s.unbound, s.scheduled, s.fired), (2, 2, 0, 0));
        assert!(log.events.is_empty());
        // A bound id still works afterwards.
        p.on_marker(&marker(1, MarkerKind::CastStart, 3), t, t);
        assert_eq!(p.update(t, &mut log), 1);
    }

    #[test]
    fn actions_fire_in_time_order_on_the_right_anchor() {
        let mut p = Presenter::new(library(), PresentationConfig::default());
        let t = HostInstant::from_nanos(1000 * MS);
        p.on_marker(&marker(1, MarkerKind::CastStart, 1), t, t);
        p.on_marker(
            &marker(2, MarkerKind::Impact { target: VICTIM }, 1),
            t.saturating_add(Duration::from_millis(50)),
            t,
        );
        let mut log = Log::default();
        assert_eq!(p.update(t, &mut log), 1, "only the immediate effect is due");
        assert_eq!(
            p.update(t.saturating_add(Duration::from_millis(300)), &mut log),
            2
        );
        let names: Vec<_> = log.events.iter().map(|e| (e.1, e.2)).collect();
        assert_eq!(names, [("effect", CASTER), ("anim", VICTIM), ("sound", CASTER)]);
        assert!(log.events.windows(2).all(|w| matches!(w, [a, b] if a.0 <= b.0)));
    }

    #[test]
    fn filters_duplicates_lateness_and_capacity_are_counted() {
        let mut p = Presenter::new(
            library(),
            PresentationConfig {
                max_pending: 2,
                dedupe_window: 4,
                max_lateness: Duration::from_millis(100),
            },
        );
        let t = HostInstant::from_nanos(1000 * MS);
        let mut log = Log::default();
        // Kind filters: tick 1 is filtered, tick 2 fires; a package kind on a cast binding
        // is filtered.
        p.on_marker(&marker(3, MarkerKind::TickN(1), 1), t, t);
        p.on_marker(&marker(1, MarkerKind::Package(PackageMarker(4)), 1), t, t);
        p.on_marker(&marker(3, MarkerKind::TickN(2), 1), t, t);
        assert_eq!(p.stats().filtered, 2);
        assert_eq!(p.update(t, &mut log), 1);
        // The same marker again is a duplicate.
        p.on_marker(&marker(3, MarkerKind::TickN(2), 1), t, t);
        assert_eq!(p.stats().duplicates, 1);
        // Too late on arrival.
        p.on_marker(
            &marker(3, MarkerKind::TickN(2), 2),
            t,
            t.saturating_add(Duration::from_millis(500)),
        );
        assert_eq!(p.stats().late, 1);
        // Capacity two: the cast's two actions fill it; the next marker's action overflows.
        p.on_marker(&marker(1, MarkerKind::CastStart, 3), t, t);
        p.on_marker(&marker(3, MarkerKind::TickN(2), 4), t, t);
        assert_eq!(p.stats().overflow, 1);
        // A target-anchored action without a target is skipped.
        let mut lonely = marker(3, MarkerKind::TickN(2), 5);
        lonely.target = None;
        p.clear();
        p.on_marker(&lonely, t, t);
        assert_eq!(p.stats().missing_target, 1);
        // Actions that became too late while waiting are dropped at fire time.
        p.on_marker(&marker(1, MarkerKind::CastStart, 6), t, t);
        assert_eq!(p.update(t.saturating_add(Duration::from_secs(5)), &mut log), 0);
        assert_eq!(p.stats().late, 3);
    }

    #[test]
    fn camera_shakes_decay_to_nothing() {
        let mut shakes = CameraShakes::new(2);
        let t = HostInstant::from_nanos(5 * MS);
        shakes.add(t, 2.0, 10.0, 0.5, 1.0);
        shakes.add(t, 1.0, 7.0, 0.5, 0.5);
        shakes.add(t, 0.1, 7.0, 0.5, 1.0);
        assert_eq!(
            shakes.len(),
            2,
            "the weakest new shake does not displace stronger ones"
        );
        let mut peak = 0.0f32;
        for ms in 0..500 {
            let (yaw, pitch) = shakes.sample(t.saturating_add(Duration::from_millis(ms)));
            assert!(yaw.abs() <= 2.5 && pitch.abs() <= 2.5);
            peak = peak.max(yaw.abs());
        }
        assert!(peak > 0.5, "peak {peak}");
        assert_eq!(
            shakes.sample(t.saturating_add(Duration::from_millis(600))),
            (0.0, 0.0)
        );
        assert!(shakes.is_empty());
    }
}

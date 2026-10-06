//! The mixer: applies events, manages the voice pool, and renders the bus graph.

use mantis_formats::mixer_graph::MixerGraph;
use mantis_formats::sound_bank::{ClipSelection, Sound, SoundBank, StealPolicy};

use crate::bus::BusState;
use crate::config::{MixerConfig, MixerError};
use crate::dsp::{GAIN_RAMP_MS, MIN_FADE_MS, Ramp, Rng, STEAL_FADE_MS, samples_for_ms};
use crate::event::AudioEvent;
use crate::ids::{BusId, EmitterId, SoundId, VoiceHandle};
use crate::spatial::{Listener, attenuation, equal_power};
use crate::voice::{ClipData, Voice};

/// Highest volume or bus gain an event may set.
const MAX_EVENT_GAIN: f32 = 4.0;
/// Pitch range an event may ask for (multiplied with the sound's own pitch).
const EVENT_PITCH: core::ops::RangeInclusive<f32> = 0.25..=4.0;

/// Counters for diagnostics and tests.
#[derive(Clone, Copy, PartialEq, Eq, Default, Debug)]
pub struct MixerStats {
    /// Plays that started a voice.
    pub plays: u64,
    /// Plays dropped by a steal policy or by priority.
    pub refused: u64,
    /// Voices faded out to make room for a new play.
    pub stolen: u64,
    /// Fading voices cut short because every slot was busy.
    pub cut: u64,
    /// Events ignored: unknown sound or bus, stale or duplicate handle, or an invalid
    /// value.
    pub ignored: u64,
}

/// What a handle's voice is doing.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum VoiceState {
    /// Audible and counted against the voice limits.
    Playing,
    /// Fading out after a stop, a steal, or stop-all.
    Stopping,
}

/// One sound's definition, resolved bus, and round-robin cursor.
#[derive(Clone, Debug)]
struct SoundState {
    def: Sound,
    bus: usize,
    next_clip: usize,
}

/// A remembered emitter position.
#[derive(Clone, Copy, Debug)]
struct EmitterSlot {
    id: EmitterId,
    position: [f32; 3],
    stamp: u64,
}

/// The fields of [`AudioEvent::Play`].
#[derive(Clone, Copy, Debug)]
struct PlayArgs {
    sound: SoundId,
    emitter: Option<EmitterId>,
    position: Option<[f32; 3]>,
    volume: f32,
    pitch: f32,
    handle: VoiceHandle,
}

/// The event-driven mixer the audio thread hosts.
///
/// Built once from a sound bank and a mixer graph; after construction, [`Mixer::handle`]
/// and [`Mixer::render`] never allocate.
#[derive(Clone, Debug)]
pub struct Mixer {
    config: MixerConfig,
    sounds: Vec<SoundState>,
    /// `(sound id, index into sounds)`, sorted by id.
    lookup: Vec<(u32, usize)>,
    clips: Vec<ClipData>,
    /// Bank sample rate over output sample rate.
    rate_ratio: f64,
    /// Twice `max_voices` slots: the audible voices plus room for fade-outs.
    voices: Vec<Option<Voice>>,
    buses: Vec<BusState>,
    /// Bus indices, every child before its parent; the master is last.
    order: Vec<usize>,
    listener: Listener,
    emitters: Vec<EmitterSlot>,
    emitter_clock: u64,
    rng: Rng,
    serial: u64,
    gain_ramp: u32,
    stats: MixerStats,
}

impl Mixer {
    /// Builds a mixer, checking the bank, the graph, and that every sound's bus exists,
    /// and allocating every buffer it will use.
    ///
    /// # Errors
    /// [`MixerError`] for a bad config, a bank or graph that breaks its format rules, or a
    /// sound naming a bus the graph lacks.
    pub fn new(bank: &SoundBank, graph: &MixerGraph, config: MixerConfig) -> Result<Mixer, MixerError> {
        config.validate()?;
        bank.validate().map_err(MixerError::Bank)?;
        graph.validate().map_err(MixerError::Graph)?;
        let mut sounds = Vec::with_capacity(bank.sounds.len());
        for def in &bank.sounds {
            let bus = graph.index_of(def.bus).ok_or(MixerError::UnknownBus {
                sound: SoundId(def.id),
                bus: BusId(def.bus),
            })?;
            sounds.push(SoundState {
                def: def.clone(),
                bus,
                next_clip: 0,
            });
        }
        let mut lookup: Vec<(u32, usize)> = sounds.iter().enumerate().map(|(i, s)| (s.def.id, i)).collect();
        lookup.sort_unstable();
        let parents: Vec<Option<usize>> = graph
            .buses
            .iter()
            .map(|b| b.parent.and_then(|p| graph.index_of(p)))
            .collect();
        let buses = graph
            .buses
            .iter()
            .zip(&parents)
            .map(|(b, p)| BusState::new(b, *p, config.sample_rate, config.block_frames))
            .collect();
        Ok(Mixer {
            sounds,
            lookup,
            clips: bank.clips.iter().map(ClipData::new).collect(),
            rate_ratio: f64::from(bank.sample_rate) / f64::from(config.sample_rate),
            voices: vec![None; config.max_voices * 2],
            buses,
            order: processing_order(&parents),
            listener: Listener::default(),
            emitters: Vec::with_capacity(config.max_emitters),
            emitter_clock: 0,
            rng: Rng::new(config.seed),
            serial: 0,
            gain_ramp: samples_for_ms(GAIN_RAMP_MS, config.sample_rate),
            stats: MixerStats::default(),
            config,
        })
    }

    /// The configuration.
    pub fn config(&self) -> &MixerConfig {
        &self.config
    }

    /// Counters since construction.
    pub fn stats(&self) -> MixerStats {
        self.stats
    }

    /// The listener.
    pub fn listener(&self) -> &Listener {
        &self.listener
    }

    /// Voices playing and not stopping (the count the voice limit applies to).
    pub fn active_voices(&self) -> usize {
        self.voices.iter().flatten().filter(|v| !v.stopping).count()
    }

    /// Voices in the pool, including those fading out.
    pub fn voices_in_use(&self) -> usize {
        self.voices.iter().flatten().count()
    }

    /// What the voice with `handle` is doing; `None` once it has ended (or never started).
    pub fn voice_state(&self, handle: VoiceHandle) -> Option<VoiceState> {
        let v = self
            .find(handle)
            .and_then(|i| self.voices.get(i))
            .and_then(Option::as_ref)?;
        Some(if v.stopping {
            VoiceState::Stopping
        } else {
            VoiceState::Playing
        })
    }

    /// A bus's current (possibly mid-fade) linear gain.
    pub fn bus_gain(&self, bus: BusId) -> Option<f32> {
        self.buses.iter().find(|b| b.id == bus.0).map(|b| b.gain.value())
    }

    /// Applies one event. Never allocates and never fails: invalid events are ignored and
    /// counted.
    pub fn handle(&mut self, event: AudioEvent) {
        match event {
            AudioEvent::Play {
                sound,
                emitter,
                position,
                volume,
                pitch,
                handle,
            } => self.play(PlayArgs {
                sound,
                emitter,
                position,
                volume,
                pitch,
                handle,
            }),
            AudioEvent::Stop { handle, fade_ms } => {
                let samples = samples_for_ms(fade_ms.max(MIN_FADE_MS), self.config.sample_rate);
                match self
                    .find(handle)
                    .and_then(|i| self.voices.get_mut(i))
                    .and_then(Option::as_mut)
                {
                    Some(v) => v.stop(samples),
                    None => self.stats.ignored += 1,
                }
            }
            AudioEvent::SetListener {
                position,
                forward,
                up,
            } => match Listener::new(position, forward, up) {
                Some(l) => self.listener = l,
                None => self.stats.ignored += 1,
            },
            AudioEvent::SetEmitter { emitter, position } => self.set_emitter(emitter, position),
            AudioEvent::SetBusGain { bus, gain, fade_ms } => self.set_bus_gain(bus, gain, fade_ms),
            AudioEvent::StopAll => {
                let samples = samples_for_ms(MIN_FADE_MS, self.config.sample_rate);
                for v in self.voices.iter_mut().flatten() {
                    v.stop(samples);
                }
            }
        }
    }

    /// Renders interleaved stereo into `out`, overwriting it. Any length is accepted: it
    /// is mixed in blocks of at most `block_frames`, and a trailing odd sample is zeroed.
    pub fn render(&mut self, out: &mut [f32]) {
        let (frames, rest) = out.as_chunks_mut::<2>();
        rest.fill(0.0);
        for block in frames.chunks_mut(self.config.block_frames) {
            self.render_block(block);
        }
    }

    fn render_block(&mut self, out: &mut [[f32; 2]]) {
        let n = out.len();
        for bus in &mut self.buses {
            if let Some(b) = bus.buffer.get_mut(..n) {
                b.fill([0.0; 2]);
            }
        }
        self.mix_voices(n);
        out.fill([0.0; 2]);
        for &bi in &self.order {
            // Take the buffer out so the parent's buffer can be borrowed alongside it;
            // `take` leaves an empty Vec, which does not allocate.
            let Some(bus) = self.buses.get_mut(bi) else {
                continue;
            };
            let mut buf = core::mem::take(&mut bus.buffer);
            let parent = bus.parent;
            if let Some(frames) = buf.get_mut(..n) {
                bus.process(frames);
                let dest = match parent {
                    Some(p) => self.buses.get_mut(p).and_then(|b| b.buffer.get_mut(..n)),
                    None => Some(&mut *out),
                };
                if let Some(dest) = dest {
                    for (d, s) in dest.iter_mut().zip(frames.iter()) {
                        d[0] += s[0];
                        d[1] += s[1];
                    }
                }
            }
            if let Some(bus) = self.buses.get_mut(bi) {
                bus.buffer = buf;
            }
        }
    }

    fn mix_voices(&mut self, n: usize) {
        for slot in &mut self.voices {
            let Some(v) = slot else {
                continue;
            };
            let alive = match self.sounds.get(v.sound) {
                Some(sound) => {
                    let (target, level) = target_gains(&self.listener, &sound.def, v.volume, v.position);
                    v.audibility = level * v.fade.value();
                    let clip = self.clips.get(v.clip);
                    let buf = self.buses.get_mut(v.bus).and_then(|b| b.buffer.get_mut(..n));
                    match (clip, buf) {
                        (Some(clip), Some(buf)) => v.mix(clip, target, self.gain_ramp, buf),
                        _ => false,
                    }
                }
                None => false,
            };
            if !alive {
                *slot = None;
            }
        }
    }

    fn find(&self, handle: VoiceHandle) -> Option<usize> {
        if handle.is_none() {
            return None;
        }
        self.voices
            .iter()
            .position(|v| v.is_some_and(|v| v.handle == handle))
    }

    fn set_bus_gain(&mut self, bus: BusId, gain: f32, fade_ms: f32) {
        let samples = samples_for_ms(fade_ms.max(MIN_FADE_MS), self.config.sample_rate);
        let valid = gain.is_finite() && (0.0..=MAX_EVENT_GAIN).contains(&gain);
        match self.buses.iter_mut().find(|b| b.id == bus.0) {
            Some(b) if valid => b.gain.set(gain, samples),
            _ => self.stats.ignored += 1,
        }
    }

    fn set_emitter(&mut self, emitter: EmitterId, position: [f32; 3]) {
        if position.iter().any(|v| !v.is_finite()) {
            self.stats.ignored += 1;
            return;
        }
        self.remember_emitter(emitter, position);
        for v in self.voices.iter_mut().flatten() {
            if v.emitter == Some(emitter) {
                v.position = Some(position);
            }
        }
    }

    /// Records an emitter's position, forgetting the least recently moved emitter when
    /// the table is full. Pushes stay within the preallocated capacity.
    fn remember_emitter(&mut self, id: EmitterId, position: [f32; 3]) {
        self.emitter_clock += 1;
        let stamp = self.emitter_clock;
        let slot = EmitterSlot { id, position, stamp };
        if let Some(e) = self.emitters.iter_mut().find(|e| e.id == id) {
            *e = slot;
        } else if self.emitters.len() < self.config.max_emitters {
            self.emitters.push(slot);
        } else if let Some(e) = self.emitters.iter_mut().min_by_key(|e| e.stamp) {
            *e = slot;
        }
    }

    fn emitter_position(&self, id: EmitterId) -> Option<[f32; 3]> {
        self.emitters.iter().find(|e| e.id == id).map(|e| e.position)
    }

    fn play(&mut self, a: PlayArgs) {
        let valid = a.volume.is_finite()
            && (0.0..=MAX_EVENT_GAIN).contains(&a.volume)
            && EVENT_PITCH.contains(&a.pitch)
            && a.position.is_none_or(|p| p.iter().all(|v| v.is_finite()));
        let known = self
            .lookup
            .binary_search_by_key(&a.sound.0, |(id, _)| *id)
            .ok()
            .and_then(|i| self.lookup.get(i))
            .map(|(_, si)| *si);
        let (Some(si), true, None) = (known, valid, self.find(a.handle)) else {
            self.stats.ignored += 1;
            return;
        };
        if let (Some(e), Some(p)) = (a.emitter, a.position) {
            self.remember_emitter(e, p);
        }
        let position = a
            .position
            .or_else(|| a.emitter.and_then(|e| self.emitter_position(e)));
        let Some(sound) = self.sounds.get(si) else {
            return;
        };
        let volume = sound.def.volume * a.volume;
        let (gains, level) = target_gains(&self.listener, &sound.def, volume, position);
        let (priority, looping, spatial, bus) = (
            sound.def.priority,
            sound.def.looping,
            sound.def.spatial,
            sound.bus,
        );
        if !self.make_room_for_sound(si) || !self.make_room_globally(priority, level) {
            self.stats.refused += 1;
            return;
        }
        let Some((clip, pitch)) = self.pick(si) else {
            return;
        };
        let Some(index) = self.free_slot() else {
            self.stats.refused += 1;
            return;
        };
        self.serial += 1;
        let voice = Voice {
            handle: a.handle,
            sound: si,
            clip,
            bus,
            serial: self.serial,
            priority,
            emitter: a.emitter,
            position,
            volume,
            looping,
            spatial,
            pos: 0.0,
            step: f64::from(pitch * a.pitch) * self.rate_ratio,
            gains: [Ramp::new(gains[0]), Ramp::new(gains[1])],
            fade: Ramp::new(1.0),
            stopping: false,
            audibility: level,
        };
        if let Some(slot) = self.voices.get_mut(index) {
            *slot = Some(voice);
            self.stats.plays += 1;
        }
    }

    /// Applies the sound's instance limit and steal policy. False when the play must be
    /// refused.
    fn make_room_for_sound(&mut self, si: usize) -> bool {
        let Some(def) = self.sounds.get(si).map(|s| &s.def) else {
            return false;
        };
        let (limit, policy) = (usize::from(def.max_instances), def.steal);
        let mine = || {
            self.voices
                .iter()
                .enumerate()
                .filter_map(|(i, v)| v.map(|v| (i, v)))
        };
        let instances = mine().filter(|(_, v)| !v.stopping && v.sound == si);
        if instances.clone().count() < limit {
            return true;
        }
        let victim = match policy {
            StealPolicy::Refuse => return false,
            StealPolicy::Oldest => instances.min_by_key(|(_, v)| v.serial),
            StealPolicy::Quietest => instances.min_by(|(_, a), (_, b)| {
                a.audibility
                    .total_cmp(&b.audibility)
                    .then(a.serial.cmp(&b.serial))
            }),
        };
        let Some((victim, _)) = victim else {
            return false;
        };
        self.steal(victim);
        true
    }

    /// Applies the global voice limit: the least important active voice (lowest priority,
    /// then least audible, then oldest) is stolen if the new play outranks or equals it.
    fn make_room_globally(&mut self, priority: u8, level: f32) -> bool {
        if self.active_voices() < self.config.max_voices {
            return true;
        }
        let victim = self
            .voices
            .iter()
            .enumerate()
            .filter_map(|(i, v)| v.filter(|v| !v.stopping).map(|v| (i, v)))
            .min_by(|(_, a), (_, b)| {
                a.priority
                    .cmp(&b.priority)
                    .then(a.audibility.total_cmp(&b.audibility))
                    .then(a.serial.cmp(&b.serial))
            });
        match victim {
            Some((i, v)) if v.priority < priority || (v.priority == priority && v.audibility <= level) => {
                self.steal(i);
                true
            }
            _ => false,
        }
    }

    fn steal(&mut self, index: usize) {
        let samples = samples_for_ms(STEAL_FADE_MS, self.config.sample_rate);
        if let Some(Some(v)) = self.voices.get_mut(index) {
            v.stop(samples);
            self.stats.stolen += 1;
        }
    }

    /// An empty slot, or failing that the quietest fading voice's slot (cut short).
    fn free_slot(&mut self) -> Option<usize> {
        if let Some(i) = self.voices.iter().position(Option::is_none) {
            return Some(i);
        }
        let (i, _) = self
            .voices
            .iter()
            .enumerate()
            .filter_map(|(i, v)| v.filter(|v| v.stopping).map(|v| (i, v)))
            .min_by(|(_, a), (_, b)| a.fade.value().total_cmp(&b.fade.value()))?;
        self.stats.cut += 1;
        Some(i)
    }

    /// The clip and random pitch for one play of sound `si`.
    fn pick(&mut self, si: usize) -> Option<(usize, f32)> {
        let sound = self.sounds.get_mut(si)?;
        let count = sound.def.clips.len().max(1);
        let slot = match sound.def.selection {
            ClipSelection::RoundRobin => {
                let i = sound.next_clip % count;
                sound.next_clip = (i + 1) % count;
                i
            }
            ClipSelection::Random => self.rng.below(u32::try_from(count).unwrap_or(1)) as usize,
        };
        let clip = sound.def.clips.get(slot).copied()? as usize;
        let (lo, hi) = (sound.def.pitch_min, sound.def.pitch_max);
        let pitch = if hi > lo {
            lo + (hi - lo) * self.rng.unit()
        } else {
            lo
        };
        Some((clip, pitch))
    }
}

/// Per-channel target gains and the pre-pan level of a voice. Non-spatial sounds play at
/// `volume` on both channels; spatial sounds are attenuated by distance and panned with
/// equal power (centered at the listener or without a position).
fn target_gains(
    listener: &Listener,
    sound: &Sound,
    volume: f32,
    position: Option<[f32; 3]>,
) -> ([f32; 2], f32) {
    if !sound.spatial {
        return ([volume; 2], volume);
    }
    let (pan, level) = match position {
        Some(p) => {
            let (distance, pan) = listener.locate(p, sound.attenuation.min_distance);
            (pan, volume * attenuation(&sound.attenuation, distance))
        }
        None => (0.0, volume),
    };
    let [l, r] = equal_power(pan);
    ([l * level, r * level], level)
}

/// Bus indices ordered so every child comes before its parent (deepest first).
fn processing_order(parents: &[Option<usize>]) -> Vec<usize> {
    let depth = |mut i: usize| {
        let mut d = 0usize;
        while let Some(p) = parents.get(i).copied().flatten() {
            i = p;
            d += 1;
            if d > parents.len() {
                break;
            }
        }
        d
    };
    let mut order: Vec<usize> = (0..parents.len()).collect();
    order.sort_by_key(|i| core::cmp::Reverse(depth(*i)));
    order
}

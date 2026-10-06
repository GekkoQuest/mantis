//! Render-thread media glue: the audio mixer behind the audio thread, and the
//! presentation sink that turns fired presentation actions ([`crate::presentation`]) into
//! sounds, particle effects, camera shakes, and animation triggers.
//!
//! Positions come from the render world through [`EntityPositions`]; effects go to an
//! [`EffectSpawner`]: the renderer's particle system through [`ParticleEffects`], or a
//! test double; effect scale is the particle instance's uniform scale. Sounds anchored
//! with `follow` play on an emitter per entity, and [`MediaState::follow`] moves those
//! emitters every frame while the sound can still be playing.

use glam::Vec3;
use mantis_audio::{AudioEvent, EmitterId, HandleAllocator, Mixer, SoundId};
use mantis_core::content::ContentHash;
use mantis_core::ecs::EntityId;
use mantis_formats::particle_effect::ParticleEffect;
use mantis_render::particles::{EffectId, EmitterHandle, EmitterTransform, ParticleError};
use mantis_render::renderer::Renderer;

use crate::characters::Characters;
use crate::presentation::{CameraShakes, Cue, PresentationSink};
use crate::threads::audio::{AudioBackend, AudioSender};
use crate::time::HostInstant;

impl AudioBackend for Mixer {
    type Event = AudioEvent;

    fn handle(&mut self, event: AudioEvent) {
        Mixer::handle(self, event);
    }

    fn render(&mut self, out: &mut [f32]) {
        Mixer::render(self, out);
    }
}

/// The listener update for a camera: its position and basis (decision 0019: the audio
/// listener derives from the same camera basis the renderer draws with).
pub fn listener_event(camera: &mantis_render::math::Camera) -> AudioEvent {
    AudioEvent::SetListener {
        position: camera.position.to_array(),
        forward: camera.forward().to_array(),
        up: [0.0, 1.0, 0.0],
    }
}

/// World positions of entities, as the render world currently shows them.
pub trait EntityPositions {
    /// Where `entity` is drawn, if it is known.
    fn position(&self, entity: EntityId) -> Option<Vec3>;
}

/// Where particle effects are spawned.
pub trait EffectSpawner {
    /// Spawns `effect` at `position`, scaled, optionally following `follow`. Returns
    /// whether it was spawned.
    fn spawn_effect(
        &mut self,
        effect: ContentHash,
        position: Vec3,
        scale: f32,
        follow: Option<EntityId>,
    ) -> bool;
}

/// An effect spawner that drops everything (no particle system attached).
#[derive(Clone, Copy, Debug, Default)]
pub struct NoEffects;

impl EffectSpawner for NoEffects {
    fn spawn_effect(&mut self, _: ContentHash, _: Vec3, _: f32, _: Option<EntityId>) -> bool {
        false
    }
}

/// Particle effects by content hash, registered with the renderer's particle system.
#[derive(Clone, Debug, Default)]
pub struct EffectLibrary {
    /// Sorted by hash.
    effects: Vec<(ContentHash, EffectId)>,
}

impl EffectLibrary {
    /// Registers `effect` (its content hash is the key presentation graphs use).
    ///
    /// # Errors
    /// The particle system's refusal.
    pub fn register(
        &mut self,
        renderer: &mut Renderer,
        queue: &wgpu::Queue,
        hash: ContentHash,
        effect: &ParticleEffect,
    ) -> Result<EffectId, ParticleError> {
        let id = renderer.particles_mut().register(queue, effect)?;
        match self.effects.binary_search_by_key(&hash, |e| e.0) {
            Ok(i) => {
                if let Some(slot) = self.effects.get_mut(i) {
                    slot.1 = id;
                }
            }
            Err(i) => self.effects.insert(i, (hash, id)),
        }
        Ok(id)
    }

    /// The effect registered under `hash`.
    pub fn get(&self, hash: &ContentHash) -> Option<EffectId> {
        self.effects
            .binary_search_by_key(hash, |e| e.0)
            .ok()
            .and_then(|i| self.effects.get(i))
            .map(|e| e.1)
    }
}

/// Spawns effects into the renderer's particle system and keeps following instances on
/// their entities.
#[derive(Debug)]
pub struct ParticleEffects {
    library: EffectLibrary,
    followers: Vec<(EmitterHandle, EntityId)>,
    follower_capacity: usize,
    seed: u32,
}

impl ParticleEffects {
    /// Effects from `library`; at most `followers` following instances are tracked
    /// (beyond that, new instances stay where they spawned).
    pub fn new(library: EffectLibrary, followers: usize) -> Self {
        Self {
            library,
            followers: Vec::with_capacity(followers),
            follower_capacity: followers,
            seed: 1,
        }
    }

    /// One frame's spawner over `renderer`.
    pub fn spawner<'a>(&'a mut self, renderer: &'a mut Renderer) -> RendererEffects<'a> {
        RendererEffects {
            effects: self,
            renderer,
        }
    }

    /// Moves following instances to their entities and forgets finished ones.
    /// Allocation-free.
    pub fn follow(&mut self, renderer: &mut Renderer, positions: &impl EntityPositions) {
        let particles = renderer.particles_mut();
        self.followers.retain(|(h, entity)| {
            if !particles.is_live(*h) {
                return false;
            }
            if let Some(p) = positions.position(*entity) {
                let _ = particles.set_transform(*h, EmitterTransform::at(p));
            }
            true
        });
    }
}

/// [`EffectSpawner`] over the renderer's particle system.
pub struct RendererEffects<'a> {
    effects: &'a mut ParticleEffects,
    renderer: &'a mut Renderer,
}

impl EffectSpawner for RendererEffects<'_> {
    fn spawn_effect(
        &mut self,
        effect: ContentHash,
        position: Vec3,
        scale: f32,
        follow: Option<EntityId>,
    ) -> bool {
        let Some(id) = self.effects.library.get(&effect) else {
            return false;
        };
        self.effects.seed = self
            .effects
            .seed
            .wrapping_mul(747_796_405)
            .wrapping_add(2_891_336_453);
        let Ok(h) = self.renderer.particles_mut().spawn(
            id,
            EmitterTransform::at(position).scaled(scale),
            self.effects.seed,
        ) else {
            return false;
        };
        if let Some(entity) = follow
            && self.effects.followers.len() < self.effects.follower_capacity
        {
            self.effects.followers.push((h, entity));
        }
        true
    }
}

/// Counters.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct MediaStats {
    /// Effects spawned.
    pub effects: u64,
    /// Sounds started.
    pub sounds: u64,
    /// Camera shakes started (within reach).
    pub shakes: u64,
    /// Animation triggers delivered.
    pub triggers: u64,
    /// Actions skipped because the anchor's position is unknown.
    pub no_position: u64,
    /// Actions the target system refused (full queue, unknown trigger, no spawner).
    pub refused: u64,
}

/// How long a following sound's emitter keeps tracking its entity.
const FOLLOW_SECONDS: u64 = 10;

/// State the sink keeps across frames.
#[derive(Debug)]
pub struct MediaState {
    /// Camera shakes in flight.
    pub shakes: CameraShakes,
    followers: Vec<(EntityId, HostInstant)>,
    follower_capacity: usize,
    /// Counters.
    pub stats: MediaStats,
}

impl MediaState {
    /// Room for `shakes` simultaneous shakes and `followers` entities with following
    /// sounds.
    pub fn new(shakes: usize, followers: usize) -> Self {
        Self {
            shakes: CameraShakes::new(shakes),
            followers: Vec::with_capacity(followers),
            follower_capacity: followers,
            stats: MediaStats::default(),
        }
    }

    /// The emitter a following sound of `entity` plays on.
    pub fn emitter_of(entity: EntityId) -> EmitterId {
        EmitterId(entity.index())
    }

    /// Moves every followed entity's emitter to its current position and forgets
    /// followers whose sounds have certainly ended. Allocation-free.
    pub fn follow(
        &mut self,
        now: HostInstant,
        positions: &impl EntityPositions,
        audio: &AudioSender<AudioEvent>,
    ) {
        self.followers.retain(|(_, until)| *until > now);
        for (entity, _) in &self.followers {
            if let Some(p) = positions.position(*entity) {
                let _ = audio.send(AudioEvent::SetEmitter {
                    emitter: Self::emitter_of(*entity),
                    position: p.to_array(),
                });
            }
        }
    }

    fn track(&mut self, entity: EntityId, until: HostInstant) {
        if let Some(slot) = self.followers.iter_mut().find(|(e, _)| *e == entity) {
            slot.1 = slot.1.max(until);
        } else if self.followers.len() < self.follower_capacity {
            self.followers.push((entity, until));
        }
    }
}

/// One frame's presentation sink over the client's media systems.
pub struct MediaSink<'a, P: EntityPositions, E: EffectSpawner> {
    /// Entity positions.
    pub positions: &'a P,
    /// Particle effects.
    pub effects: &'a mut E,
    /// The audio queue, when audio runs.
    pub audio: Option<&'a AudioSender<AudioEvent>>,
    /// Voice handle allocator of the mixer.
    pub voices: &'a HandleAllocator,
    /// Characters (animation triggers), when any are present.
    pub characters: Option<&'a mut Characters>,
    /// Shakes, followers, counters.
    pub state: &'a mut MediaState,
    /// The camera position (shake reach).
    pub camera: Vec3,
    /// Now.
    pub now: HostInstant,
}

impl<P: EntityPositions, E: EffectSpawner> MediaSink<'_, P, E> {
    fn place(&mut self, cue: &Cue) -> Option<Vec3> {
        let p = self
            .positions
            .position(cue.anchor)
            .map(|p| p + Vec3::from(cue.offset));
        if p.is_none() {
            self.state.stats.no_position += 1;
        }
        p
    }
}

impl<P: EntityPositions, E: EffectSpawner> PresentationSink for MediaSink<'_, P, E> {
    fn spawn_effect(&mut self, cue: &Cue, effect: ContentHash, scale: f32, follow: bool) {
        let Some(p) = self.place(cue) else { return };
        if self
            .effects
            .spawn_effect(effect, p, scale, follow.then_some(cue.anchor))
        {
            self.state.stats.effects += 1;
        } else {
            self.state.stats.refused += 1;
        }
    }

    fn play_sound(&mut self, cue: &Cue, sound: u32, volume: f32, pitch: f32, follow: bool) {
        let Some(p) = self.place(cue) else { return };
        let Some(audio) = self.audio else {
            self.state.stats.refused += 1;
            return;
        };
        let emitter = follow.then(|| MediaState::emitter_of(cue.anchor));
        let event = AudioEvent::Play {
            sound: SoundId(sound),
            emitter,
            position: Some(p.to_array()),
            volume,
            pitch,
            handle: self.voices.allocate(),
        };
        if audio.send(event).is_ok() {
            self.state.stats.sounds += 1;
            if follow {
                self.state.track(
                    cue.anchor,
                    self.now
                        .saturating_add(std::time::Duration::from_secs(FOLLOW_SECONDS)),
                );
            }
        } else {
            self.state.stats.refused += 1;
        }
    }

    fn camera_shake(&mut self, cue: &Cue, amplitude: f32, frequency: f32, duration: f32, radius: f32) {
        let Some(p) = self.place(cue) else { return };
        let distance = p.distance(self.camera);
        if distance > radius {
            return;
        }
        // Linear falloff to the edge of the reach.
        let scale = 1.0 - distance / radius;
        self.state
            .shakes
            .add(self.now, amplitude, frequency, duration, scale);
        self.state.stats.shakes += 1;
    }

    fn anim_trigger(&mut self, cue: &Cue, parameter: u32) {
        let delivered = self
            .characters
            .as_mut()
            .is_some_and(|c| c.trigger(cue.anchor, parameter));
        if delivered {
            self.state.stats.triggers += 1;
        } else {
            self.state.stats.refused += 1;
        }
    }
}

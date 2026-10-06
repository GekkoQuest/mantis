//! Animated characters on the render thread: crowd tiers, animation, and the renderer's
//! deformed instances (plan 8.4).
//!
//! Every frame [`Characters::update`]:
//! 1. assigns a crowd tier to every character from the camera ([`CrowdSelector`]: range
//!    bands, per-tier budgets, hysteresis);
//! 2. moves each character's renderer instance to its tier: near characters are skinned
//!    meshes posed by their animation graph; mid and far characters play baked vertex
//!    animations (far on a lower-detail mesh at a reduced frame rate); culled characters
//!    have no instance;
//! 3. advances near characters' animation graphs on the persistent worker pool
//!    ([`crate::anim_pool`], the calling thread plus `workers - 1` threads) and writes
//!    their skinning palettes; advances vertex animation playback for the others.
//!
//! Characters are keyed by entity and kept sorted, so lookups are binary searches and the
//! per-frame passes are allocation-free.

use std::sync::Arc;

use glam::{Mat4, Vec3};
use mantis_anim::{AnimGraph, AnimInstance};

use crate::anim_pool::{AnimPool, Slot};
use mantis_core::ecs::EntityId;
use mantis_render::crowd::{CrowdAgent, CrowdConfig, CrowdSelector, CrowdStats, CrowdTier};
use mantis_render::deform::VatId;
use mantis_render::math::{Frustum, Sphere};
use mantis_render::renderer::{Renderer, RendererError};
use mantis_render::scene::{InstanceHandle, MaterialId, MeshId};

/// Everything characters of one kind share.
#[derive(Debug)]
pub struct CharacterKind {
    /// The animation graph (near tier).
    pub graph: Arc<AnimGraph>,
    /// The material (must support skinning and vertex animation).
    pub material: MaterialId,
    /// Skinned mesh for the near tier.
    pub near_mesh: MeshId,
    /// Mesh for the mid tier (vertex count of `mid_vat`).
    pub mid_mesh: MeshId,
    /// Lower-detail mesh for the far tier (vertex count of `far_vat`).
    pub far_mesh: MeshId,
    /// Baked animation for the mid tier.
    pub mid_vat: VatId,
    /// Baked animation for the far tier.
    pub far_vat: VatId,
    /// Model-space bounds of every animated pose.
    pub bounds: Sphere,
    /// Far-tier playback steps per second (the far tier skips frames).
    pub far_frame_rate: f32,
}

/// Errors.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum CharacterError {
    /// An animation worker thread could not start (the OS error).
    Workers(String),
    /// The character capacity is reached.
    Full,
    /// The entity already has a character.
    Duplicate,
    /// The entity has no character.
    Unknown,
}

impl core::fmt::Display for CharacterError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str(match self {
            CharacterError::Workers(_) => "an animation worker thread could not start",
            CharacterError::Full => "character capacity reached",
            CharacterError::Duplicate => "entity already has a character",
            CharacterError::Unknown => "entity has no character",
        })
    }
}

impl std::error::Error for CharacterError {}

/// Per-frame counters.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct CharacterStats {
    /// Tier assignment.
    pub crowd: CrowdStats,
    /// Characters that changed tier this frame.
    pub tier_changes: u32,
    /// Renderer refusals (instance or palette capacity); the character is skipped.
    pub refused: u32,
}

#[derive(Debug)]
struct Character {
    entity: EntityId,
    slot: Slot,
    kind: Arc<CharacterKind>,
    model: Mat4,
    moved: bool,
    tier: CrowdTier,
    instance: Option<InstanceHandle>,
    /// Seconds of vertex animation playback.
    clock: f32,
}

/// The characters.
#[derive(Debug)]
pub struct Characters {
    list: Vec<Character>,
    pool: AnimPool,
    capacity: usize,
    selector: CrowdSelector,
    agents: Vec<CrowdAgent>,
    tiers: Vec<CrowdTier>,
}

impl Characters {
    /// Room for `capacity` characters; near-tier animation runs on `workers` threads (0
    /// or 1: the calling thread only), started here and kept for the characters' lifetime.
    ///
    /// # Errors
    /// [`CharacterError::Workers`] when a thread cannot start.
    pub fn new(config: CrowdConfig, capacity: usize, workers: usize) -> Result<Self, CharacterError> {
        Self::with_worker_wrapper(config, capacity, workers, |f| f())
    }

    /// As [`Characters::new`], with each animation worker's share of a frame run inside
    /// `wrap` (see [`AnimPool::with_wrapper`]).
    ///
    /// # Errors
    /// [`CharacterError::Workers`] when a thread cannot start.
    pub fn with_worker_wrapper(
        config: CrowdConfig,
        capacity: usize,
        workers: usize,
        wrap: crate::anim_pool::WorkerWrapper,
    ) -> Result<Self, CharacterError> {
        Ok(Self {
            list: Vec::with_capacity(capacity),
            pool: AnimPool::with_wrapper(workers, capacity, wrap)
                .map_err(|e| CharacterError::Workers(e.to_string()))?,
            capacity,
            selector: CrowdSelector::new(config, capacity),
            agents: Vec::with_capacity(capacity),
            tiers: vec![CrowdTier::Culled; capacity],
        })
    }

    fn find(&self, entity: EntityId) -> Result<usize, usize> {
        self.list.binary_search_by_key(&entity, |c| c.entity)
    }

    /// Adds a character (culled until the next update places it).
    ///
    /// # Errors
    /// [`CharacterError::Full`] or [`CharacterError::Duplicate`].
    pub fn add(
        &mut self,
        entity: EntityId,
        kind: Arc<CharacterKind>,
        model: Mat4,
    ) -> Result<(), CharacterError> {
        let Err(at) = self.find(entity) else {
            return Err(CharacterError::Duplicate);
        };
        if self.list.len() >= self.capacity {
            return Err(CharacterError::Full);
        }
        let slot = self.pool.add(entity, AnimInstance::new(Arc::clone(&kind.graph)));
        self.list.insert(
            at,
            Character {
                entity,
                slot,
                kind,
                model,
                moved: false,
                tier: CrowdTier::Culled,
                instance: None,
                clock: 0.0,
            },
        );
        Ok(())
    }

    /// Removes a character and its renderer instance.
    ///
    /// # Errors
    /// [`CharacterError::Unknown`].
    pub fn remove(&mut self, renderer: &mut Renderer, entity: EntityId) -> Result<(), CharacterError> {
        let at = self.find(entity).map_err(|_| CharacterError::Unknown)?;
        let c = self.list.remove(at);
        if let Some(moved) = self.pool.remove(c.slot)
            && let Ok(i) = self.find(moved)
            && let Some(m) = self.list.get_mut(i)
        {
            m.slot = c.slot;
        }
        if let Some(h) = c.instance {
            let _ = renderer.despawn(h);
        }
        Ok(())
    }

    /// Sets a character's world transform.
    ///
    /// # Errors
    /// [`CharacterError::Unknown`].
    pub fn set_transform(&mut self, entity: EntityId, model: Mat4) -> Result<(), CharacterError> {
        let at = self.find(entity).map_err(|_| CharacterError::Unknown)?;
        let c = self.list.get_mut(at).ok_or(CharacterError::Unknown)?;
        c.moved |= c.model != model;
        c.model = model;
        Ok(())
    }

    /// Fires a trigger parameter (by name hash) on a character's animation graph, for
    /// example from a presentation graph. Returns whether the graph has that trigger.
    pub fn trigger(&mut self, entity: EntityId, name_hash: u32) -> bool {
        let Some(slot) = self
            .find(entity)
            .ok()
            .and_then(|at| self.list.get(at))
            .map(|c| c.slot)
        else {
            return false;
        };
        self.pool
            .with(slot, |anim| {
                anim.graph()
                    .parameter(name_hash)
                    .is_some_and(|id| anim.set_trigger(id).is_ok())
            })
            .unwrap_or(false)
    }

    /// Sets a float parameter (by name hash). Returns whether the graph has it.
    pub fn set_float(&mut self, entity: EntityId, name_hash: u32, value: f32) -> bool {
        let Some(slot) = self
            .find(entity)
            .ok()
            .and_then(|at| self.list.get(at))
            .map(|c| c.slot)
        else {
            return false;
        };
        self.pool
            .with(slot, |anim| {
                anim.graph()
                    .parameter(name_hash)
                    .is_some_and(|id| anim.set_float(id, value).is_ok())
            })
            .unwrap_or(false)
    }

    /// A character's current tier.
    pub fn tier(&self, entity: EntityId) -> Option<CrowdTier> {
        self.find(entity)
            .ok()
            .and_then(|i| self.list.get(i))
            .map(|c| c.tier)
    }

    /// Characters.
    pub fn len(&self) -> usize {
        self.list.len()
    }

    /// Whether there are none.
    pub fn is_empty(&self) -> bool {
        self.list.is_empty()
    }

    /// Runs one frame (see the module docs). `camera` and `frustum` are the view's
    /// position and frustum.
    ///
    /// # Errors
    /// None today: renderer refusals are counted in [`CharacterStats::refused`]. The
    /// result type leaves room for device loss.
    pub fn update(
        &mut self,
        renderer: &mut Renderer,
        camera: Vec3,
        frustum: &Frustum,
        dt: f32,
    ) -> Result<CharacterStats, RendererError> {
        let mut stats = CharacterStats::default();
        self.agents.clear();
        for c in &self.list {
            let scale = c.model.x_axis.truncate().length();
            self.agents.push(CrowdAgent {
                position: c.model.transform_point3(c.kind.bounds.center),
                radius: c.kind.bounds.radius * scale,
                priority: 1.0,
            });
        }
        stats.crowd = self
            .selector
            .select(camera, frustum, &self.agents, &mut self.tiers);
        let dt = if dt.is_finite() { dt.max(0.0) } else { 0.0 };
        for (c, tier) in self.list.iter_mut().zip(self.tiers.iter().copied()) {
            c.clock += dt;
            if tier != c.tier || (c.instance.is_none() && tier != CrowdTier::Culled) {
                stats.tier_changes += u32::from(tier != c.tier);
                if let Some(h) = c.instance.take() {
                    let _ = renderer.despawn(h);
                }
                c.tier = tier;
                c.instance = spawn(renderer, c, tier);
                stats.refused += u32::from(c.instance.is_none() && tier != CrowdTier::Culled);
                c.moved = false;
            } else if c.moved {
                if let Some(h) = c.instance {
                    let _ = renderer.scene_mut().set_transform(h, c.model);
                }
                c.moved = false;
            }
        }
        self.animate(dt);
        for (c, tier) in self.list.iter().zip(self.tiers.iter().copied()) {
            let Some(h) = c.instance else { continue };
            let result = match tier {
                CrowdTier::Near => self
                    .pool
                    .with(c.slot, |anim| renderer.set_palette(h, anim.palette()))
                    .unwrap_or(Ok(())),
                CrowdTier::Mid => renderer.set_vat_time(h, c.kind.mid_vat, c.clock),
                CrowdTier::Far => {
                    renderer.set_vat_time(h, c.kind.far_vat, stepped(c.clock, c.kind.far_frame_rate))
                }
                CrowdTier::Culled => Ok(()),
            };
            stats.refused += u32::from(result.is_err());
        }
        Ok(stats)
    }

    /// Advances the near tier's animation graphs on the worker pool.
    fn animate(&mut self, dt: f32) {
        for (c, tier) in self.list.iter().zip(&self.tiers) {
            self.pool.set_near(c.slot, *tier == CrowdTier::Near);
        }
        self.pool.run(dt);
    }

    /// Animation workers, the calling thread included.
    pub fn workers(&self) -> usize {
        self.pool.workers()
    }
}

/// Playback time snapped down to `rate` steps per second.
fn stepped(seconds: f32, rate: f32) -> f32 {
    if rate > 0.0 && rate.is_finite() {
        (seconds * rate).floor() / rate
    } else {
        seconds
    }
}

fn spawn(renderer: &mut Renderer, c: &Character, tier: CrowdTier) -> Option<InstanceHandle> {
    let k = &c.kind;
    let result = match tier {
        CrowdTier::Near => {
            let bones = u32::try_from(k.graph.skeleton().bone_count()).unwrap_or(u32::MAX);
            renderer.spawn_skinned(k.near_mesh, k.material, c.model, bones, k.bounds)
        }
        CrowdTier::Mid => renderer.spawn_vat(k.mid_mesh, k.material, c.model, k.mid_vat, c.clock),
        CrowdTier::Far => renderer.spawn_vat(
            k.far_mesh,
            k.material,
            c.model,
            k.far_vat,
            stepped(c.clock, k.far_frame_rate),
        ),
        CrowdTier::Culled => return None,
    };
    result.ok()
}

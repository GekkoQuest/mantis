//! The snapshot model: what a client learns about the world each tick.
//!
//! One snapshot per session per server tick, sent unreliably; clients drop
//! any snapshot whose `server_tick` is not newer than the last applied one.
//!
//! **Contract clauses.**
//! - `ack` is the newest `Move` seq the server has applied for this session's
//!   avatar. The server applies exactly one input per avatar per tick, in seq
//!   order; when none is available it repeats the last input under the next
//!   seq and marks that seq consumed. `ack` therefore counts `Motion::step`
//!   calls one for one, and `local.state` is exactly the result of applying
//!   the inputs up to and including `ack`.
//! - `local` is the owner's avatar at **full precision** (never the quantized
//!   broadcast form), with `local_mods` the modifiers in effect for it. A new
//!   or changed avatar id means the client resets its predictor (spawn,
//!   teleport, zone change).
//! - `entered` announces an entity entering interest, with its appearance. It
//!   repeats in every snapshot until the client acknowledges a snapshot that
//!   carried it, which makes it reliable over unreliable delivery.
//! - `remotes` carries only entities updated in this snapshot (interest tiers
//!   update at different rates); an absent entity keeps its history. Each
//!   sample carries its own `tick`. At most one sample per id; order is
//!   meaningless.
//! - `removed` lists entities that left interest or despawned. An entity that
//!   is removed and later re-enters with the same `EntityId` is a fresh entity
//!   to the client (its history is cleared); a despawned entity whose slot is
//!   reused has a different generation and is distinct anyway.
//! - Decoders validate the whole frame before visiting it, so a
//!   [`SnapshotVisitor`] never sees a partial frame. Visit order: `header`
//!   once, then `entered`, `remote`, `removed`, `marker`.

use mantis_core::ecs::EntityId;
use mantis_core::graph::TimelineMarker;
use mantis_core::kinematics::{Angle16, InputSeq, MotionModifiers, MotionState};
use mantis_core::math::Vec3;
use mantis_core::mem::BoundedVec;
use mantis_core::time::Tick;

use crate::AppearanceId;

/// The owner's avatar at full precision.
#[derive(Clone, Copy, PartialEq, Debug)]
pub struct LocalAvatar {
    /// The avatar.
    pub id: EntityId,
    /// Its authoritative state after applying inputs through `ack`.
    pub state: MotionState,
}

/// Per-snapshot facts.
#[derive(Clone, Copy, PartialEq, Debug)]
pub struct SnapshotHeader {
    /// The server tick this snapshot describes.
    pub server_tick: Tick,
    /// The newest applied `Move` seq for this session's avatar.
    pub ack: Option<InputSeq>,
    /// The owner's avatar, if it has one.
    pub local: Option<LocalAvatar>,
    /// Modifiers in effect for the avatar.
    pub local_mods: MotionModifiers,
}

impl Default for SnapshotHeader {
    fn default() -> Self {
        Self {
            server_tick: Tick::ZERO,
            ack: None,
            local: None,
            local_mods: MotionModifiers::NONE,
        }
    }
}

/// One remote entity's kinematic sample.
#[derive(Clone, Copy, PartialEq, Debug)]
pub struct RemoteSample {
    /// The entity.
    pub id: EntityId,
    /// The server tick this sample describes.
    pub tick: Tick,
    /// Position.
    pub position: Vec3,
    /// Velocity.
    pub velocity: Vec3,
    /// Facing.
    pub yaw: Angle16,
}

/// Receives a decoded, already validated snapshot.
pub trait SnapshotVisitor {
    /// Called once, first.
    fn header(&mut self, header: &SnapshotHeader);
    /// An entity entered interest.
    fn entered(&mut self, id: EntityId, appearance: AppearanceId);
    /// A remote sample.
    fn remote(&mut self, sample: &RemoteSample);
    /// An entity left interest or despawned.
    fn removed(&mut self, id: EntityId);
    /// A timeline marker for presentation.
    fn marker(&mut self, marker: &TimelineMarker);
}

/// A complete snapshot in fixed-capacity storage. The server fills one per
/// client per tick (from pooled storage); clients may decode into one. Every
/// push past capacity is refused and counted in `overflowed` (fail closed).
#[derive(Clone, Debug)]
pub struct SnapshotFrame {
    /// Per-snapshot facts.
    pub header: SnapshotHeader,
    /// Entities that entered interest.
    pub entered: BoundedVec<(EntityId, AppearanceId)>,
    /// Remote samples.
    pub remotes: BoundedVec<RemoteSample>,
    /// Entities removed.
    pub removed: BoundedVec<EntityId>,
    /// Timeline markers.
    pub markers: BoundedVec<TimelineMarker>,
    /// Items refused because a list was full.
    pub overflowed: u32,
}

impl SnapshotFrame {
    /// A frame with the given capacities. The only allocating call.
    #[must_use]
    pub fn with_capacity(entered: usize, remotes: usize, removed: usize, markers: usize) -> Self {
        Self {
            header: SnapshotHeader::default(),
            entered: BoundedVec::with_capacity(entered),
            remotes: BoundedVec::with_capacity(remotes),
            removed: BoundedVec::with_capacity(removed),
            markers: BoundedVec::with_capacity(markers),
            overflowed: 0,
        }
    }

    /// Empties every list and resets the header, keeping capacity.
    pub fn clear(&mut self) {
        self.header = SnapshotHeader::default();
        self.entered.clear();
        self.remotes.clear();
        self.removed.clear();
        self.markers.clear();
        self.overflowed = 0;
    }

    /// Makes this frame a copy of `other` without allocating (items beyond
    /// this frame's capacities are dropped and counted in `overflowed`).
    pub fn copy_from(&mut self, other: &SnapshotFrame) {
        self.clear();
        other.visit(self);
        self.overflowed += other.overflowed;
    }

    /// Replays this frame into another visitor in contract order.
    pub fn visit(&self, v: &mut impl SnapshotVisitor) {
        v.header(&self.header);
        for (id, appearance) in self.entered.iter() {
            v.entered(*id, *appearance);
        }
        for r in self.remotes.iter() {
            v.remote(r);
        }
        for id in self.removed.iter() {
            v.removed(*id);
        }
        for m in self.markers.iter() {
            v.marker(m);
        }
    }

    /// The remote sample for `id`, if present.
    #[must_use]
    pub fn find_remote(&self, id: EntityId) -> Option<&RemoteSample> {
        self.remotes.iter().find(|r| r.id == id)
    }
}

/// Where an encoder finds a per-remote baseline: for a remote missing from
/// the frame-level baseline, the newest frame the client acknowledged that
/// carries it (native codec, `MASK_OWN_BASE`).
pub trait RemoteBases {
    /// The tick of an acknowledged frame carrying `id`, and its sample
    /// there; `None` when no such frame is held.
    fn base_for(&self, id: EntityId) -> Option<(Tick, &RemoteSample)>;
}

/// No per-remote baselines.
pub struct NoRemoteBases;

impl RemoteBases for NoRemoteBases {
    fn base_for(&self, _id: EntityId) -> Option<(Tick, &RemoteSample)> {
        None
    }
}

impl SnapshotVisitor for SnapshotFrame {
    fn header(&mut self, header: &SnapshotHeader) {
        self.header = *header;
    }

    fn entered(&mut self, id: EntityId, appearance: AppearanceId) {
        if self.entered.push((id, appearance)).is_err() {
            self.overflowed += 1;
        }
    }

    fn remote(&mut self, sample: &RemoteSample) {
        if self.remotes.push(*sample).is_err() {
            self.overflowed += 1;
        }
    }

    fn removed(&mut self, id: EntityId) {
        if self.removed.push(id).is_err() {
            self.overflowed += 1;
        }
    }

    fn marker(&mut self, marker: &TimelineMarker) {
        if self.markers.push(*marker).is_err() {
            self.overflowed += 1;
        }
    }
}

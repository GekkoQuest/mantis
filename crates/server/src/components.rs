//! Server components and identities.

use mantis_adapter_contract::AppearanceId;
use mantis_core::ecs::{Component, EntityId};
use mantis_core::hash::{StableHasher, StateHash};
use mantis_core::kinematics::{MotionModifiers, MotionState};
use mantis_core::log::SessionId;

/// The stable, world-wide identity clients see for a replicated entity. It
/// survives ownership transfer between cells (plan 7.1), unlike the cell-local
/// [`EntityId`]. Allocated by the realm; shaped like an `EntityId` so the
/// snapshot model needs no second id type.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct ReplicationId(pub EntityId);

impl StateHash for ReplicationId {
    fn state_hash(&self, h: &mut StableHasher) {
        self.0.state_hash(h);
    }
}

impl Component for ReplicationId {
    const NAME: &'static str = "server.replication_id";

    fn save(&self, e: &mut mantis_adapter_contract::core_types::Encoder<'_>) -> bool {
        e.u64(self.0.to_bits());
        true
    }

    fn load(
        d: &mut mantis_adapter_contract::core_types::Decoder<'_>,
    ) -> Result<Self, mantis_adapter_contract::core_types::DecodeError> {
        Ok(Self(EntityId::from_bits(d.u64()?)))
    }
}

/// A mover's kinematic state.
#[derive(Clone, Copy, PartialEq, Debug)]
pub struct Body(pub MotionState);

impl StateHash for Body {
    fn state_hash(&self, h: &mut StableHasher) {
        self.0.state_hash(h);
    }
}

impl Component for Body {
    const NAME: &'static str = "server.body";

    fn save(&self, e: &mut mantis_adapter_contract::core_types::Encoder<'_>) -> bool {
        crate::snapshot::motion(e, &self.0);
        true
    }

    fn load(
        d: &mut mantis_adapter_contract::core_types::Decoder<'_>,
    ) -> Result<Self, mantis_adapter_contract::core_types::DecodeError> {
        Ok(Self(crate::snapshot::motion_of(d)?))
    }
}

/// Movement modifiers in effect (from effects).
#[derive(Clone, Copy, PartialEq, Debug)]
pub struct Mods(pub MotionModifiers);

impl StateHash for Mods {
    fn state_hash(&self, h: &mut StableHasher) {
        h.write_f32(self.0.speed_scale);
        h.write_f32(self.0.jump_scale);
        h.write_f32(self.0.gravity_scale);
    }
}

impl Component for Mods {
    const NAME: &'static str = "server.mods";

    fn save(&self, e: &mut mantis_adapter_contract::core_types::Encoder<'_>) -> bool {
        crate::snapshot::mods(e, &self.0);
        true
    }

    fn load(
        d: &mut mantis_adapter_contract::core_types::Decoder<'_>,
    ) -> Result<Self, mantis_adapter_contract::core_types::DecodeError> {
        Ok(Self(crate::snapshot::mods_of(d)?))
    }
}

/// What an entity looks like to clients.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Look(pub AppearanceId);

impl StateHash for Look {
    fn state_hash(&self, h: &mut StableHasher) {
        h.write_u32(self.0.0);
    }
}

impl Component for Look {
    const NAME: &'static str = "server.look";

    fn save(&self, e: &mut mantis_adapter_contract::core_types::Encoder<'_>) -> bool {
        e.u32(self.0.0);
        true
    }

    fn load(
        d: &mut mantis_adapter_contract::core_types::Decoder<'_>,
    ) -> Result<Self, mantis_adapter_contract::core_types::DecodeError> {
        Ok(Self(AppearanceId(d.u32()?)))
    }
}

/// The avatar of a session.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Controlled(pub SessionId);

impl StateHash for Controlled {
    fn state_hash(&self, h: &mut StableHasher) {
        h.write_u64(self.0.0);
    }
}

impl Component for Controlled {
    const NAME: &'static str = "server.controlled";

    fn save(&self, e: &mut mantis_adapter_contract::core_types::Encoder<'_>) -> bool {
        e.u64(self.0.0);
        true
    }

    fn load(
        d: &mut mantis_adapter_contract::core_types::Decoder<'_>,
    ) -> Result<Self, mantis_adapter_contract::core_types::DecodeError> {
        Ok(Self(SessionId(d.u64()?)))
    }
}

/// Allocates replication ids. One per realm; monotonic, never reused.
#[derive(Debug, Default)]
pub struct ReplicationIds {
    next: std::sync::atomic::AtomicU32,
}

impl ReplicationIds {
    /// Allocation starts at index `next` (tests of the id range).
    #[must_use]
    pub fn starting_at(next: u32) -> Self {
        Self {
            next: std::sync::atomic::AtomicU32::new(next),
        }
    }

    /// A fresh id.
    pub fn allocate(&self) -> ReplicationId {
        let n = self.next.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        ReplicationId(EntityId::new(n, 0))
    }

    /// A fresh id inside `range`, or `None` (fail closed) when the next id
    /// is outside it: ids are never reused, so the range is used up.
    pub fn allocate_within(&self, range: mantis_adapter_contract::EntityIdRange) -> Option<ReplicationId> {
        let id = self.allocate();
        range.contains(id.0).then_some(id)
    }
}

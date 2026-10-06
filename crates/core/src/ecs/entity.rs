//! Generational entity ids and the slot allocator (principle 4).

use core::fmt;

use crate::hash::{StableHasher, StateHash};

/// A generational entity id: `(index, generation)`.
///
/// Despawning an entity bumps its slot's generation, so a stale id never
/// aliases a newer entity in the same slot; lookups with it return `None`.
/// Ordering is by `(index, generation)`.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct EntityId {
    index: u32,
    generation: u32,
}

impl EntityId {
    /// An id from its parts. Constructing an id never makes it live; the world
    /// decides whether it refers to anything.
    #[must_use]
    pub const fn new(index: u32, generation: u32) -> Self {
        Self { index, generation }
    }

    /// The slot index.
    #[must_use]
    pub const fn index(self) -> u32 {
        self.index
    }

    /// The slot generation.
    #[must_use]
    pub const fn generation(self) -> u32 {
        self.generation
    }

    /// Packs into 64 bits: generation in the high 32, index in the low 32.
    /// This is the packing scripts see as lightuserdata (decision 0001).
    #[must_use]
    pub const fn to_bits(self) -> u64 {
        ((self.generation as u64) << 32) | self.index as u64
    }

    /// Unpacks [`EntityId::to_bits`].
    #[must_use]
    #[allow(clippy::cast_possible_truncation)] // deliberate split of the two halves
    pub const fn from_bits(bits: u64) -> Self {
        Self {
            index: bits as u32,
            generation: (bits >> 32) as u32,
        }
    }
}

impl fmt::Debug for EntityId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "EntityId({}v{})", self.index, self.generation)
    }
}

impl fmt::Display for EntityId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}v{}", self.index, self.generation)
    }
}

impl StateHash for EntityId {
    fn state_hash(&self, h: &mut StableHasher) {
        h.write_u64(self.to_bits());
    }
}

/// Where a live entity's components are stored.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) struct Location {
    pub(crate) archetype: u32,
    pub(crate) row: u32,
}

#[derive(Clone, Copy, Debug)]
struct Slot {
    generation: u32,
    /// `Some` while live.
    location: Option<Location>,
}

/// Slot allocator with a LIFO free list.
///
/// A slot whose generation would overflow is retired permanently instead of
/// wrapping, so an id can never repeat. Behaviour is a pure function of the
/// sequence of calls, which keeps ids deterministic under replay.
#[derive(Debug, Default)]
pub(crate) struct EntityAllocator {
    slots: Vec<Slot>,
    free: Vec<u32>,
    live: u32,
    retired: u32,
}

impl EntityAllocator {
    pub(crate) fn reserve(&mut self, additional: usize) {
        self.slots.reserve(additional);
        self.free.reserve(additional);
    }

    pub(crate) fn live(&self) -> u32 {
        self.live
    }

    pub(crate) fn retired(&self) -> u32 {
        self.retired
    }

    #[cfg(test)]
    pub(crate) fn slot_count(&self) -> usize {
        self.slots.len()
    }

    /// Allocates an id. The location must be set with [`Self::set_location`]
    /// before the entity is used. `None` when all `u32` indices are in use.
    pub(crate) fn alloc(&mut self, location: Location) -> Option<EntityId> {
        let index = if let Some(index) = self.free.pop() {
            index
        } else {
            // Index u32::MAX is never handed out, so the live count cannot overflow.
            let index = u32::try_from(self.slots.len()).ok().filter(|i| *i < u32::MAX)?;
            self.slots.push(Slot {
                generation: 0,
                location: None,
            });
            index
        };
        let slot = self.slots.get_mut(index as usize)?;
        slot.location = Some(location);
        self.live += 1;
        Some(EntityId::new(index, slot.generation))
    }

    /// The location of `id` if it is live.
    pub(crate) fn location(&self, id: EntityId) -> Option<Location> {
        let slot = self.slots.get(id.index as usize)?;
        if slot.generation == id.generation {
            slot.location
        } else {
            None
        }
    }

    /// Updates the location of a live entity. Returns false if `id` is not live.
    pub(crate) fn set_location(&mut self, id: EntityId, location: Location) -> bool {
        match self.slots.get_mut(id.index as usize) {
            Some(slot) if slot.generation == id.generation && slot.location.is_some() => {
                slot.location = Some(location);
                true
            }
            _ => false,
        }
    }

    /// Frees a live id, bumping its generation. Returns false if `id` is not live.
    pub(crate) fn free(&mut self, id: EntityId) -> bool {
        let Some(slot) = self.slots.get_mut(id.index as usize) else {
            return false;
        };
        if slot.generation != id.generation || slot.location.is_none() {
            return false;
        }
        slot.location = None;
        self.live -= 1;
        if let Some(next) = slot.generation.checked_add(1) {
            slot.generation = next;
            self.free.push(id.index);
        } else {
            self.retired += 1; // generation exhausted: never reuse this slot
        }
        true
    }

    /// Live ids in ascending index order.
    /// Writes the allocator: every slot's generation and liveness, then the
    /// free list in order, so ids allocated after a restore match.
    pub(crate) fn save(&self, e: &mut crate::wire::Encoder<'_>) {
        e.u32(u32::try_from(self.slots.len()).unwrap_or(u32::MAX));
        for s in &self.slots {
            e.u32(s.generation);
            e.bool(s.location.is_some());
        }
        e.u32(u32::try_from(self.free.len()).unwrap_or(u32::MAX));
        for f in &self.free {
            e.u32(*f);
        }
        e.u32(self.retired);
    }

    /// Reads an allocator written by [`EntityAllocator::save`]; live slots
    /// get their locations as their rows are restored. Returns the live ids.
    pub(crate) fn load(
        &mut self,
        d: &mut crate::wire::Decoder<'_>,
    ) -> Result<Vec<EntityId>, crate::wire::DecodeError> {
        let n = d.u32()? as usize;
        let mut slots = Vec::with_capacity(n.min(1 << 20));
        let mut live = Vec::new();
        for index in 0..n {
            let generation = d.u32()?;
            let alive = d.bool()?;
            if alive {
                live.push(EntityId::new(
                    u32::try_from(index).unwrap_or(u32::MAX),
                    generation,
                ));
            }
            slots.push(Slot {
                generation,
                location: None,
            });
        }
        let f = d.u32()? as usize;
        let mut free = Vec::with_capacity(f.min(1 << 20));
        for _ in 0..f {
            free.push(d.u32()?);
        }
        self.retired = d.u32()?;
        self.slots = slots;
        self.free = free;
        self.live = 0;
        Ok(live)
    }

    /// Places a restored live entity.
    pub(crate) fn place(&mut self, id: EntityId, location: Location) -> bool {
        match self.slots.get_mut(id.index as usize) {
            Some(slot) if slot.generation == id.generation && slot.location.is_none() => {
                slot.location = Some(location);
                self.live += 1;
                true
            }
            _ => false,
        }
    }

    pub(crate) fn iter_live(&self) -> impl Iterator<Item = (EntityId, Location)> + '_ {
        self.slots.iter().zip(0u32..).filter_map(|(slot, index)| {
            slot.location
                .map(|loc| (EntityId::new(index, slot.generation), loc))
        })
    }

    #[cfg(test)]
    pub(crate) fn force_generation(&mut self, index: u32, generation: u32) {
        if let Some(slot) = self.slots.get_mut(index as usize) {
            slot.generation = generation;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const LOC: Location = Location { archetype: 0, row: 0 };

    #[test]
    fn bits_round_trip_and_ordering() {
        let id = EntityId::new(0xDEAD_BEEF, 0x0102_0304);
        assert_eq!(id.to_bits(), 0x0102_0304_DEAD_BEEF);
        assert_eq!(EntityId::from_bits(id.to_bits()), id);
        assert_eq!(EntityId::from_bits(u64::MAX), EntityId::new(u32::MAX, u32::MAX));
        assert!(EntityId::new(1, 9) < EntityId::new(2, 0));
        assert!(EntityId::new(2, 0) < EntityId::new(2, 1));
        assert_eq!(format!("{id}"), "3735928559v16909060");
        assert_eq!(format!("{:?}", EntityId::new(3, 1)), "EntityId(3v1)");
    }

    #[test]
    fn despawn_bumps_generation_and_stale_ids_fail() {
        let mut a = EntityAllocator::default();
        let e0 = a.alloc(LOC).unwrap();
        let e1 = a.alloc(LOC).unwrap();
        assert_eq!((e0, e1), (EntityId::new(0, 0), EntityId::new(1, 0)));
        assert!(a.free(e0));
        assert!(!a.free(e0), "double free must fail");
        assert_eq!(a.location(e0), None);
        let e0b = a.alloc(LOC).unwrap();
        assert_eq!(e0b, EntityId::new(0, 1), "LIFO reuse with bumped generation");
        assert_eq!(a.location(e0), None, "stale id never aliases");
        assert_eq!(a.location(e0b), Some(LOC));
        assert_eq!(a.live(), 2);
        assert!(!a.set_location(e0, LOC));
        assert_eq!(a.location(EntityId::new(99, 0)), None);
    }

    #[test]
    fn exhausted_generation_retires_slot() {
        let mut a = EntityAllocator::default();
        let e = a.alloc(LOC).unwrap();
        a.force_generation(0, u32::MAX);
        let e = EntityId::new(e.index(), u32::MAX);
        assert!(a.free(e));
        assert_eq!(a.retired(), 1);
        let next = a.alloc(LOC).unwrap();
        assert_eq!(next, EntityId::new(1, 0), "retired slot is never reused");
    }

    #[test]
    fn iter_live_is_index_ordered() {
        let mut a = EntityAllocator::default();
        let ids: Vec<_> = (0..5).map(|_| a.alloc(LOC).unwrap()).collect();
        assert!(a.free(ids[1]));
        assert!(a.free(ids[3]));
        let live: Vec<_> = a.iter_live().map(|(id, _)| id).collect();
        assert_eq!(live, vec![ids[0], ids[2], ids[4]]);
        assert_eq!(a.slot_count(), 5);
    }
}

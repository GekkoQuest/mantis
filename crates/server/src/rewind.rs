//! Lag compensation: a bounded rewind buffer of replicated positions per tick
//! (plan 7.4). Hit resolution and range checks run against what the client
//! saw, never further back than the buffer holds.

use mantis_adapter_contract::core_types::{Tick, Vec3};
use mantis_core::mem::BoundedVec;

use crate::components::ReplicationId;

/// Why a rewind query failed (fail closed: the action is refused).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum RewindError {
    /// Older than the buffer.
    TooOld,
    /// Newer than the newest recorded tick.
    TooNew,
    /// The entity was not recorded at that tick.
    Unknown,
}

struct Slot {
    tick: Option<Tick>,
    entries: BoundedVec<(ReplicationId, Vec3)>,
}

/// A ring of per-tick position records, sorted by id for lookup.
pub struct RewindBuffer {
    slots: Vec<Slot>,
    newest: Option<Tick>,
}

impl RewindBuffer {
    /// Holds `ticks` ticks of up to `entities` positions each. The only
    /// allocating call.
    #[must_use]
    pub fn with_capacity(ticks: usize, entities: usize) -> Self {
        Self {
            slots: (0..ticks.max(2))
                .map(|_| Slot {
                    tick: None,
                    entries: BoundedVec::with_capacity(entities),
                })
                .collect(),
            newest: None,
        }
    }

    fn slot_index(&self, tick: Tick) -> usize {
        usize::try_from(tick.0 % self.slots.len() as u64).unwrap_or(0)
    }

    /// Records the positions for `tick`, overwriting the oldest slot.
    /// Positions past capacity are dropped (and later lookups fail closed).
    pub fn record(&mut self, tick: Tick, positions: impl Iterator<Item = (ReplicationId, Vec3)>) {
        let i = self.slot_index(tick);
        let Some(slot) = self.slots.get_mut(i) else { return };
        slot.tick = Some(tick);
        slot.entries.clear();
        for p in positions {
            if slot.entries.push(p).is_err() {
                break;
            }
        }
        slot.entries.sort_unstable_by_key(|(id, _)| *id);
        self.newest = Some(tick);
    }

    /// Writes every slot exactly (snapshots).
    pub(crate) fn save(&self, e: &mut mantis_adapter_contract::core_types::Encoder<'_>) {
        e.bool(self.newest.is_some());
        e.u64(self.newest.map_or(0, |t| t.0));
        e.u32(u32::try_from(self.slots.len()).unwrap_or(u32::MAX));
        for s in &self.slots {
            e.bool(s.tick.is_some());
            e.u64(s.tick.map_or(0, |t| t.0));
            e.u32(u32::try_from(s.entries.len()).unwrap_or(u32::MAX));
            for (id, p) in s.entries.iter() {
                e.u64(id.0.to_bits());
                crate::snapshot::vec3(e, *p);
            }
        }
    }

    /// Reads what [`RewindBuffer::save`] wrote into a buffer of the same
    /// shape.
    pub(crate) fn load(
        &mut self,
        d: &mut mantis_adapter_contract::core_types::Decoder<'_>,
    ) -> Result<(), mantis_adapter_contract::core_types::DecodeError> {
        use mantis_adapter_contract::core_types::{DecodeError, EntityId};
        let has = d.bool()?;
        let newest = Tick(d.u64()?);
        self.newest = has.then_some(newest);
        let n = d.u32()? as usize;
        if n != self.slots.len() {
            return Err(DecodeError::Invalid("rewind buffer length differs"));
        }
        for s in &mut self.slots {
            let has = d.bool()?;
            let t = Tick(d.u64()?);
            s.tick = has.then_some(t);
            s.entries.clear();
            for _ in 0..d.u32()? {
                let id = ReplicationId(EntityId::from_bits(d.u64()?));
                let p = crate::snapshot::vec3_of(d)?;
                s.entries
                    .push((id, p))
                    .map_err(|_| DecodeError::Invalid("rewind entries over capacity"))?;
            }
        }
        Ok(())
    }

    /// The oldest tick still held.
    #[must_use]
    pub fn oldest(&self) -> Option<Tick> {
        let newest = self.newest?;
        let span = self.slots.len() as u64 - 1;
        Some(Tick(newest.0.saturating_sub(span)))
    }

    fn at(&self, repl: ReplicationId, tick: Tick) -> Result<Vec3, RewindError> {
        let slot = self.slots.get(self.slot_index(tick)).ok_or(RewindError::TooOld)?;
        if slot.tick != Some(tick) {
            return Err(RewindError::TooOld);
        }
        let i = slot
            .entries
            .binary_search_by_key(&repl, |(id, _)| *id)
            .map_err(|_| RewindError::Unknown)?;
        slot.entries.get(i).map(|(_, p)| *p).ok_or(RewindError::Unknown)
    }

    /// Where `repl` was at `tick + frac/65536`, interpolated linearly.
    ///
    /// # Errors
    /// [`RewindError`] outside the buffer or for an unrecorded entity.
    pub fn position_at(&self, repl: ReplicationId, tick: Tick, frac: u16) -> Result<Vec3, RewindError> {
        let newest = self.newest.ok_or(RewindError::TooNew)?;
        if tick > newest || (tick == newest && frac > 0) {
            return Err(RewindError::TooNew);
        }
        if self.oldest().is_some_and(|o| tick < o) {
            return Err(RewindError::TooOld);
        }
        let a = self.at(repl, tick)?;
        if frac == 0 {
            return Ok(a);
        }
        let b = self.at(repl, tick.next())?;
        let t = f32::from(frac) / 65_536.0;
        Ok(a + (b - a) * t)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use mantis_adapter_contract::core_types::EntityId;

    #[test]
    fn records_interpolates_and_bounds() {
        let a = ReplicationId(EntityId::new(1, 0));
        let b = ReplicationId(EntityId::new(2, 0));
        let mut r = RewindBuffer::with_capacity(4, 8);
        assert_eq!(r.position_at(a, Tick(0), 0), Err(RewindError::TooNew));
        for t in 0..10u64 {
            let x = f32::from(u16::try_from(t).unwrap());
            r.record(
                Tick(t),
                [(b, Vec3::ZERO), (a, Vec3::new(x, 0.0, 0.0))].into_iter(),
            );
        }
        assert_eq!(r.oldest(), Some(Tick(6)));
        assert_eq!(r.position_at(a, Tick(8), 0), Ok(Vec3::new(8.0, 0.0, 0.0)));
        assert_eq!(r.position_at(a, Tick(8), 32_768), Ok(Vec3::new(8.5, 0.0, 0.0)));
        assert_eq!(r.position_at(a, Tick(5), 0), Err(RewindError::TooOld));
        assert_eq!(r.position_at(a, Tick(9), 1), Err(RewindError::TooNew));
        assert_eq!(r.position_at(a, Tick(10), 0), Err(RewindError::TooNew));
        assert_eq!(
            r.position_at(ReplicationId(EntityId::new(7, 0)), Tick(8), 0),
            Err(RewindError::Unknown)
        );
    }
}

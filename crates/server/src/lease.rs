//! Character ownership leases with epochs (plan 7.2). Exactly one cell owns a
//! character at a time. Every transfer bumps the epoch; economy commands and
//! movement claims carry the epoch and are refused when stale.
//!
//! In Milestone 3 the table is in-process (the realm service replaces it in a
//! later step, behind the same interface).

use std::collections::BTreeMap;
use std::sync::Mutex;

use mantis_core::log::CellId;

/// A persistent character identity.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct CharacterId(pub u64);

/// Who owns a character, and since which transfer.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Lease {
    /// The owning cell.
    pub cell: CellId,
    /// Bumped on every acquisition and transfer.
    pub epoch: u64,
}

/// Lease refused.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum LeaseError {
    /// Another cell owns the character.
    HeldBy(CellId),
    /// The caller's epoch is stale.
    StaleEpoch {
        /// The current epoch.
        current: u64,
    },
    /// No lease exists.
    NotHeld,
}

/// The lease table.
#[derive(Debug, Default)]
pub struct LeaseTable {
    leases: Mutex<BTreeMap<CharacterId, Lease>>,
}

impl LeaseTable {
    fn guard(&self) -> std::sync::MutexGuard<'_, BTreeMap<CharacterId, Lease>> {
        crate::lock(&self.leases)
    }

    /// Takes ownership for `cell` if nobody holds it.
    ///
    /// # Errors
    /// [`LeaseError::HeldBy`] when another cell owns it.
    pub fn acquire(&self, ch: CharacterId, cell: CellId) -> Result<Lease, LeaseError> {
        let mut leases = self.guard();
        let epoch = match leases.get(&ch) {
            Some(l) if l.cell != cell => return Err(LeaseError::HeldBy(l.cell)),
            Some(l) => l.epoch + 1,
            None => 1,
        };
        let lease = Lease { cell, epoch };
        leases.insert(ch, lease);
        Ok(lease)
    }

    /// Moves ownership from `from` (holding `epoch`) to `to`; idempotent for
    /// a repeated identical transfer.
    ///
    /// # Errors
    /// [`LeaseError::StaleEpoch`], [`LeaseError::HeldBy`], [`LeaseError::NotHeld`].
    pub fn transfer(
        &self,
        ch: CharacterId,
        from: CellId,
        epoch: u64,
        to: CellId,
    ) -> Result<Lease, LeaseError> {
        let mut leases = self.guard();
        let current = leases.get(&ch).copied().ok_or(LeaseError::NotHeld)?;
        if current.cell == to && current.epoch == epoch + 1 {
            return Ok(current); // already done: idempotent
        }
        if current.cell != from {
            return Err(LeaseError::HeldBy(current.cell));
        }
        if current.epoch != epoch {
            return Err(LeaseError::StaleEpoch {
                current: current.epoch,
            });
        }
        let lease = Lease {
            cell: to,
            epoch: epoch + 1,
        };
        leases.insert(ch, lease);
        Ok(lease)
    }

    /// True when `cell` owns `ch` at exactly `epoch`.
    #[must_use]
    pub fn check(&self, ch: CharacterId, cell: CellId, epoch: u64) -> bool {
        self.guard()
            .get(&ch)
            .is_some_and(|l| l.cell == cell && l.epoch == epoch)
    }

    /// Releases every lease held by `cell` (it missed its heartbeats).
    pub fn expire_cell(&self, cell: CellId) -> usize {
        let mut leases = self.guard();
        let before = leases.len();
        leases.retain(|_, l| l.cell != cell);
        before - leases.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn single_owner_epochs_and_idempotent_transfer() {
        let t = LeaseTable::default();
        let ch = CharacterId(9);
        let a = CellId(1);
        let b = CellId(2);
        let l1 = t.acquire(ch, a).unwrap();
        assert_eq!(l1, Lease { cell: a, epoch: 1 });
        assert_eq!(t.acquire(ch, b), Err(LeaseError::HeldBy(a)));
        let l2 = t.transfer(ch, a, 1, b).unwrap();
        assert_eq!(l2, Lease { cell: b, epoch: 2 });
        assert_eq!(t.transfer(ch, a, 1, b), Ok(l2), "retry is idempotent");
        assert_eq!(
            t.transfer(ch, b, 1, a),
            Err(LeaseError::StaleEpoch { current: 2 })
        );
        assert!(t.check(ch, b, 2));
        assert!(!t.check(ch, a, 1), "the old owner's epoch is stale");
        assert_eq!(t.expire_cell(b), 1);
        assert_eq!(t.transfer(ch, b, 2, a), Err(LeaseError::NotHeld));
        assert_eq!(t.acquire(ch, a).map(|l| l.epoch), Ok(1));
    }
}

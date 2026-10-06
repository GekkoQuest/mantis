//! std.containers contract: characters' bags and gold, shared by every
//! economy module through the [`Inventories`] resource, and the ledger rows
//! every economy outcome carries.
//!
//! Economy modules change bags only while executing an economy command
//! (logged, executed on delivery, outcome logged; decision 0007), through
//! [`Inventories::transaction`], so a refused command changes nothing.

#![forbid(unsafe_code)]

pub mod generated {
    //! Generated from `schema/containers.idl`.
    #[rustfmt::skip]
    pub mod containers;
}

pub use generated::containers::*;

use core::fmt;
use std::collections::BTreeMap;

use mantis_core::ecs::Resource;
use mantis_core::hash::{StableHasher, StateHash};

/// Slots in a bag.
pub const SLOTS: usize = 20;

/// Largest stack of one item.
pub const MAX_STACK: u32 = 99;

pub use mantis_core::ledger::{GOLD, Ledger, LedgerRow};

/// A stack of one item.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Stack {
    /// The item (content id, never 0).
    pub item: u32,
    /// How many, 1 to [`MAX_STACK`].
    pub count: u32,
}

/// One character's bag contents.
#[derive(Clone, PartialEq, Eq, Debug, Default)]
pub struct Contents {
    /// Gold.
    pub gold: u64,
    /// Slots.
    pub slots: [Option<Stack>; SLOTS],
}

/// Why an economy operation was refused.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum EconomyError {
    /// Not enough gold or items.
    Insufficient,
    /// No room in the bag.
    Full,
    /// No such slot, or the slot does not hold what was asked.
    BadSlot,
    /// The amount would overflow.
    Overflow,
}

impl fmt::Display for EconomyError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Insufficient => "insufficient",
            Self::Full => "bag full",
            Self::BadSlot => "bad slot",
            Self::Overflow => "overflow",
        })
    }
}

impl std::error::Error for EconomyError {}

/// A gold amount as a ledger row's delta: an amount the ledger cannot
/// record exactly is refused, never clipped.
fn delta(n: u64) -> Result<i64, EconomyError> {
    i64::try_from(n).map_err(|_| EconomyError::Overflow)
}

/// Every character's bag in the cell. Simulation state.
#[derive(Debug, Default)]
pub struct Inventories {
    bags: BTreeMap<u64, Contents>,
}

impl Inventories {
    /// `character`'s bag, if it has one.
    #[must_use]
    pub fn bag(&self, character: u64) -> Option<&Contents> {
        self.bags.get(&character)
    }

    fn bag_mut(&mut self, character: u64) -> &mut Contents {
        self.bags.entry(character).or_default()
    }

    /// The [`Bag`] message describing `character`'s bag.
    #[must_use]
    pub fn bag_message(&self, character: u64) -> Bag {
        let empty = Contents::default();
        let c = self.bag(character).unwrap_or(&empty);
        let items: Vec<u32> = c.slots.iter().map(|s| s.map_or(0, |s| s.item)).collect();
        let counts: Vec<u32> = c.slots.iter().map(|s| s.map_or(0, |s| s.count)).collect();
        Bag {
            gold: c.gold,
            items: mantis_core::wire::BoundedArray::from_slice(&items).unwrap_or_default(),
            counts: mantis_core::wire::BoundedArray::from_slice(&counts).unwrap_or_default(),
        }
    }

    /// `character`'s gold.
    #[must_use]
    pub fn gold(&self, character: u64) -> u64 {
        self.bag(character).map_or(0, |b| b.gold)
    }

    /// How many of `item` `character` holds.
    #[must_use]
    pub fn count_of(&self, character: u64, item: u32) -> u64 {
        self.bag(character).map_or(0, |b| {
            b.slots
                .iter()
                .flatten()
                .filter(|s| s.item == item)
                .map(|s| u64::from(s.count))
                .sum()
        })
    }

    /// Runs `f` as one transaction over `characters`' bags: if it fails,
    /// those bags are restored exactly. The ledger collects its rows.
    ///
    /// # Errors
    /// `f`'s error, after the rollback.
    pub fn transaction(
        &mut self,
        characters: &[u64],
        f: impl FnOnce(&mut Tx<'_>) -> Result<(), EconomyError>,
    ) -> Result<Ledger, EconomyError> {
        let saved: Vec<(u64, Option<Contents>)> = characters
            .iter()
            .map(|c| (*c, self.bags.get(c).cloned()))
            .collect();
        let mut tx = Tx {
            inv: self,
            ledger: Ledger::default(),
            allowed: characters,
        };
        match f(&mut tx) {
            Ok(()) => Ok(tx.ledger),
            Err(e) => {
                for (c, bag) in saved {
                    match bag {
                        Some(b) => {
                            self.bags.insert(c, b);
                        }
                        None => {
                            self.bags.remove(&c);
                        }
                    }
                }
                Err(e)
            }
        }
    }
}

/// An open transaction. Every change is recorded as a ledger row.
pub struct Tx<'a> {
    inv: &'a mut Inventories,
    ledger: Ledger,
    allowed: &'a [u64],
}

impl Tx<'_> {
    fn record(&mut self, row: LedgerRow) -> Result<(), EconomyError> {
        self.ledger.push(row).map_err(|_| EconomyError::Overflow)
    }

    fn bag(&mut self, character: u64) -> Result<&mut Contents, EconomyError> {
        if !self.allowed.contains(&character) {
            return Err(EconomyError::BadSlot);
        }
        Ok(self.inv.bag_mut(character))
    }

    /// Read access to everything.
    #[must_use]
    pub fn view(&self) -> &Inventories {
        self.inv
    }

    /// Adds gold.
    ///
    /// # Errors
    /// [`EconomyError::Overflow`].
    pub fn credit(&mut self, character: u64, gold: u64) -> Result<(), EconomyError> {
        if gold == 0 {
            return Ok(());
        }
        let d = delta(gold)?;
        let b = self.bag(character)?;
        b.gold = b.gold.checked_add(gold).ok_or(EconomyError::Overflow)?;
        self.record(LedgerRow {
            character,
            item: GOLD,
            delta: d,
        })
    }

    /// Removes gold.
    ///
    /// # Errors
    /// [`EconomyError::Insufficient`].
    pub fn debit(&mut self, character: u64, gold: u64) -> Result<(), EconomyError> {
        if gold == 0 {
            return Ok(());
        }
        let d = delta(gold)?;
        let b = self.bag(character)?;
        b.gold = b.gold.checked_sub(gold).ok_or(EconomyError::Insufficient)?;
        self.record(LedgerRow {
            character,
            item: GOLD,
            delta: -d,
        })
    }

    /// Adds `count` of `item`, topping up existing stacks first.
    ///
    /// # Errors
    /// [`EconomyError::Full`] when it does not all fit.
    pub fn add(&mut self, character: u64, item: u32, count: u32) -> Result<(), EconomyError> {
        if item == GOLD || count == 0 {
            return Err(EconomyError::BadSlot);
        }
        let b = self.bag(character)?;
        let mut left = count;
        for s in b.slots.iter_mut().flatten().filter(|s| s.item == item) {
            let room = MAX_STACK - s.count.min(MAX_STACK);
            let n = room.min(left);
            s.count += n;
            left -= n;
        }
        for slot in b.slots.iter_mut().filter(|s| s.is_none()) {
            if left == 0 {
                break;
            }
            let n = left.min(MAX_STACK);
            *slot = Some(Stack { item, count: n });
            left -= n;
        }
        if left > 0 {
            return Err(EconomyError::Full);
        }
        self.record(LedgerRow {
            character,
            item,
            delta: i64::from(count),
        })
    }

    /// Takes `count` items from `slot`.
    ///
    /// # Errors
    /// [`EconomyError::BadSlot`] or [`EconomyError::Insufficient`].
    pub fn take(&mut self, character: u64, slot: u8, count: u32) -> Result<Stack, EconomyError> {
        let b = self.bag(character)?;
        let s = b.slots.get_mut(usize::from(slot)).ok_or(EconomyError::BadSlot)?;
        let stack = s.ok_or(EconomyError::BadSlot)?;
        if count == 0 || count > stack.count {
            return Err(EconomyError::Insufficient);
        }
        *s = (count < stack.count).then_some(Stack {
            item: stack.item,
            count: stack.count - count,
        });
        self.record(LedgerRow {
            character,
            item: stack.item,
            delta: -i64::from(count),
        })?;
        Ok(Stack {
            item: stack.item,
            count,
        })
    }

    /// Rearranges `character`'s own slots (no ledger rows: nothing changes
    /// hands). `f` may not change totals; that is checked.
    ///
    /// # Errors
    /// `f`'s error, or [`EconomyError::BadSlot`] if totals changed.
    pub fn rearrange(
        &mut self,
        character: u64,
        f: impl FnOnce(&mut [Option<Stack>; SLOTS]) -> Result<(), EconomyError>,
    ) -> Result<(), EconomyError> {
        let b = self.bag(character)?;
        let before = totals(&b.slots);
        f(&mut b.slots)?;
        let ok = b
            .slots
            .iter()
            .flatten()
            .all(|s| s.count > 0 && s.count <= MAX_STACK && s.item != GOLD);
        if !ok || totals(&b.slots) != before {
            return Err(EconomyError::BadSlot);
        }
        Ok(())
    }
}

fn totals(slots: &[Option<Stack>; SLOTS]) -> BTreeMap<u32, u64> {
    let mut t = BTreeMap::new();
    for s in slots.iter().flatten() {
        *t.entry(s.item).or_insert(0) += u64::from(s.count);
    }
    t
}

impl StateHash for Inventories {
    fn state_hash(&self, h: &mut StableHasher) {
        h.write_u64(self.bags.len() as u64);
        for (c, b) in &self.bags {
            h.write_u64(*c);
            h.write_u64(b.gold);
            for s in &b.slots {
                match s {
                    Some(s) => {
                        h.write_u32(s.item);
                        h.write_u32(s.count);
                    }
                    None => h.write_u32(0),
                }
            }
        }
    }
}

impl Resource for Inventories {
    const NAME: &'static str = "std.containers.inventories";

    fn save(&self, e: &mut mantis_core::wire::Encoder<'_>) -> mantis_core::ecs::Saved {
        e.u32(u32::try_from(self.bags.len()).unwrap_or(u32::MAX));
        for (c, b) in &self.bags {
            e.u64(*c);
            e.u64(b.gold);
            for s in &b.slots {
                e.u32(s.map_or(0, |s| s.item));
                e.u32(s.map_or(0, |s| s.count));
            }
        }
        mantis_core::ecs::Saved::Written
    }

    fn load(&mut self, d: &mut mantis_core::wire::Decoder<'_>) -> Result<(), mantis_core::wire::DecodeError> {
        self.bags.clear();
        for _ in 0..d.u32()? {
            let c = d.u64()?;
            let mut b = Contents {
                gold: d.u64()?,
                ..Contents::default()
            };
            for slot in &mut b.slots {
                let item = d.u32()?;
                let count = d.u32()?;
                *slot = (count > 0).then_some(Stack { item, count });
            }
            self.bags.insert(c, b);
        }
        Ok(())
    }
}

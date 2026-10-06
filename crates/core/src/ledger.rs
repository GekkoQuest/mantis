//! Ledger rows: the durable record of what changed hands (decision 0005).
//!
//! Every economy outcome carries a [`Ledger`] as (the start of) its payload;
//! the persistence writer stores its rows in the monthly ledger partitions.
//! The format is the engine's, so the writer needs no knowledge of the
//! modules that produced the rows.

use core::fmt;

use crate::wire::{DecodeError, Decoder, Encoder, Wire};

/// The ledger's item id for gold.
pub const GOLD: u32 = 0;

/// One ledger row: `character`'s holding of `item` changed by `delta`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct LedgerRow {
    /// The character.
    pub character: u64,
    /// The item, or [`GOLD`].
    pub item: u32,
    /// The change.
    pub delta: i64,
}

/// A ledger already holds [`Ledger::MAX`] rows.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct LedgerFull;

impl fmt::Display for LedgerFull {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("ledger full")
    }
}

impl std::error::Error for LedgerFull {}

/// The rows of one economy outcome.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct Ledger {
    rows: [Option<LedgerRow>; Ledger::MAX],
}

impl Ledger {
    /// Rows one outcome may carry.
    pub const MAX: usize = 16;

    /// Adds a row.
    ///
    /// # Errors
    /// [`LedgerFull`].
    pub fn push(&mut self, row: LedgerRow) -> Result<(), LedgerFull> {
        let slot = self.rows.iter_mut().find(|r| r.is_none()).ok_or(LedgerFull)?;
        *slot = Some(row);
        Ok(())
    }

    /// The rows.
    pub fn rows(&self) -> impl Iterator<Item = &LedgerRow> {
        self.rows.iter().flatten()
    }

    /// The net change for `character` and `item`.
    #[must_use]
    pub fn net(&self, character: u64, item: u32) -> i64 {
        self.rows()
            .filter(|r| r.character == character && r.item == item)
            .map(|r| r.delta)
            .sum()
    }

    /// True when it has no rows.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.rows().next().is_none()
    }
}

/// The tag a ledger starts with, so the writer never mistakes another
/// payload for ledger rows.
pub const LEDGER_TAG: u32 = 0x4C45_4447;

impl Wire for Ledger {
    fn encode(&self, e: &mut Encoder<'_>) {
        e.u32(LEDGER_TAG);
        let n = self.rows().count();
        e.u8(u8::try_from(n).unwrap_or(0));
        for r in self.rows() {
            e.u64(r.character);
            e.u32(r.item);
            e.i64(r.delta);
        }
    }
    fn decode(d: &mut Decoder<'_>) -> Result<Self, DecodeError> {
        if d.u32()? != LEDGER_TAG {
            return Err(DecodeError::Invalid("ledger tag"));
        }
        let n = usize::from(d.u8()?);
        if n > Self::MAX {
            return Err(DecodeError::Invalid("ledger rows"));
        }
        let mut l = Self::default();
        for _ in 0..n {
            l.push(LedgerRow {
                character: d.u64()?,
                item: d.u32()?,
                delta: d.i64()?,
            })
            .map_err(|_| DecodeError::Invalid("ledger rows"))?;
        }
        Ok(l)
    }
}

/// Reads the ledger at the start of an outcome payload, if it holds one
/// (it starts with [`LEDGER_TAG`]).
#[must_use]
pub fn ledger_of(payload: &[u8]) -> Option<Ledger> {
    let mut d = Decoder::new(payload);
    Ledger::decode(&mut d).ok()
}

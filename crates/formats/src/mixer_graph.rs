//! Mixer graph v1: the bus tree sounds mix into, with per-bus gain and effect chains
//! (plan 8.5).
//!
//! Buses form a tree rooted at exactly one master bus. Each bus scales its input by its
//! gain, runs its effects in order, and sums into its parent; the master's output is the
//! mix. Sounds name their bus by id ([`crate::sound_bank`]).
//!
//! # Layout (little-endian)
//!
//! | offset | size | field |
//! |---|---|---|
//! | 0 | 4 | magic `"MMIX"` |
//! | 4 | 2 | version `u16` = 1 |
//! | 6 | 2 | flags `u16` = 0 |
//! | 8 | 4 | bus count `u32`, 1 to [`MAX_BUSES`] |
//! | 12 | 4 | reserved, 0 |
//! | 16 | 64 each | bus records |
//!
//! Bus record (64 bytes):
//!
//! | offset | size | field |
//! |---|---|---|
//! | 0 | 4 | id `u32`, unique, not [`NO_PARENT`] |
//! | 4 | 4 | parent id `u32`; [`NO_PARENT`] for the master |
//! | 8 | 4 | gain `f32` (linear), 0 to [`MAX_GAIN`] |
//! | 12 | 1 | effect count `u8`, 0 to [`MAX_EFFECTS`] |
//! | 13 | 3 | reserved, 0 |
//! | 16 | 48 | effect slots, 4 x 12 bytes; slots past the count are all zero |
//!
//! Effect slot (12 bytes): `kind: u8`, 3 reserved zero bytes, `p0: f32`, `p1: f32`.
//!
//! | kind | effect | `p0` | `p1` |
//! |---|---|---|---|
//! | 0 | gain | decibels, [`MIN_GAIN_DB`] to [`MAX_GAIN_DB`] | 0 |
//! | 1 | one-pole low-pass | cutoff Hz, > 0 and at most [`MAX_CUTOFF_HZ`] | 0 |
//! | 2 | one-pole high-pass | cutoff Hz, > 0 and at most [`MAX_CUTOFF_HZ`] | 0 |
//! | 3 | peak limiter | threshold (linear), > 0 and at most 1 | release ms, > 0 and at most [`MAX_RELEASE_MS`] |
//!
//! An unused parameter must be +0.0 (all bits zero). Every `f32` must be finite.
//!
//! # Graph rules
//!
//! Exactly one bus has parent [`NO_PARENT`] (the master). Every other parent names a bus
//! in the graph, and following parents from any bus reaches the master (no cycles).
//! Records may appear in any order.
//!
//! # Errors by rule
//!
//! A value out of its range is [`FormatError::Dimensions`]; an unknown effect kind is
//! [`FormatError::Encoding`]; a repeated id is [`FormatError::DuplicateId`]; a missing or
//! second master, a missing parent, a cycle, or an id equal to [`NO_PARENT`] is
//! [`FormatError::Inconsistent`].

use crate::bytes::{FormatError, Reader, Writer};

#[cfg(test)]
mod tests;

/// Magic bytes.
pub const MAGIC: [u8; 4] = *b"MMIX";
/// Parent id of the master bus.
pub const NO_PARENT: u32 = u32::MAX;
/// Most buses per graph.
pub const MAX_BUSES: u32 = 256;
/// Most effects per bus.
pub const MAX_EFFECTS: usize = 4;
/// Highest linear bus gain.
pub const MAX_GAIN: f32 = 4.0;
/// Lowest gain effect, in decibels.
pub const MIN_GAIN_DB: f32 = -96.0;
/// Highest gain effect, in decibels.
pub const MAX_GAIN_DB: f32 = 24.0;
/// Highest filter cutoff.
pub const MAX_CUTOFF_HZ: f32 = 96_000.0;
/// Longest limiter release.
pub const MAX_RELEASE_MS: f32 = 10_000.0;

const HEADER: u64 = 16;
const BUS_RECORD: u64 = 64;

/// One effect in a bus chain.
#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Effect {
    /// Kind 0: a fixed gain.
    Gain {
        /// Decibels.
        db: f32,
    },
    /// Kind 1: one-pole low-pass (6 dB per octave).
    LowPass {
        /// Cutoff frequency in Hz.
        cutoff_hz: f32,
    },
    /// Kind 2: one-pole high-pass (6 dB per octave).
    HighPass {
        /// Cutoff frequency in Hz.
        cutoff_hz: f32,
    },
    /// Kind 3: peak limiter (instant attack, exponential release).
    Limiter {
        /// Linear output ceiling.
        threshold: f32,
        /// Release time constant in milliseconds.
        release_ms: f32,
    },
}

impl Effect {
    fn kind(self) -> u8 {
        match self {
            Effect::Gain { .. } => 0,
            Effect::LowPass { .. } => 1,
            Effect::HighPass { .. } => 2,
            Effect::Limiter { .. } => 3,
        }
    }

    fn params(self) -> [f32; 2] {
        match self {
            Effect::Gain { db } => [db, 0.0],
            Effect::LowPass { cutoff_hz } | Effect::HighPass { cutoff_hz } => [cutoff_hz, 0.0],
            Effect::Limiter {
                threshold,
                release_ms,
            } => [threshold, release_ms],
        }
    }
}

/// One bus.
#[derive(Clone, Debug, PartialEq)]
pub struct Bus {
    /// Stable id sounds and events name.
    pub id: u32,
    /// Parent bus id; `None` for the master.
    pub parent: Option<u32>,
    /// Linear gain, 0 to [`MAX_GAIN`].
    pub gain: f32,
    /// Effects run in order after the gain, 0 to [`MAX_EFFECTS`].
    pub effects: Vec<Effect>,
}

/// A parsed, validated mixer graph.
#[derive(Clone, Debug, PartialEq)]
pub struct MixerGraph {
    /// Buses in file order.
    pub buses: Vec<Bus>,
}

impl MixerGraph {
    /// The bus with `id`.
    pub fn bus(&self, id: u32) -> Option<&Bus> {
        self.buses.iter().find(|b| b.id == id)
    }

    /// Index of the bus with `id` in [`MixerGraph::buses`].
    pub fn index_of(&self, id: u32) -> Option<usize> {
        self.buses.iter().position(|b| b.id == id)
    }

    /// The master bus (present in every parsed graph).
    pub fn master(&self) -> Option<&Bus> {
        self.buses.iter().find(|b| b.parent.is_none())
    }

    /// Parses and validates a mixer graph.
    ///
    /// # Errors
    /// [`FormatError`] describing the first problem found (see the module docs).
    pub fn parse(bytes: &[u8]) -> Result<MixerGraph, FormatError> {
        let mut r = Reader::new(bytes);
        if r.array::<4>()? != MAGIC {
            return Err(FormatError::Magic);
        }
        let version = r.u16()?;
        if version != 1 {
            return Err(FormatError::Version(version));
        }
        let flags = r.u16()?;
        if flags != 0 {
            return Err(FormatError::Flags(u32::from(flags)));
        }
        let count = r.u32()?;
        if count == 0 || count > MAX_BUSES {
            return Err(FormatError::Dimensions);
        }
        if r.u32()? != 0 {
            return Err(FormatError::Reserved);
        }
        let expected = HEADER + u64::from(count) * BUS_RECORD;
        if bytes.len() as u64 != expected {
            return Err(FormatError::Length {
                expected,
                actual: bytes.len() as u64,
            });
        }
        let mut buses = Vec::with_capacity(count as usize);
        for _ in 0..count {
            buses.push(read_bus(&mut r)?);
        }
        r.finish()?;
        let graph = MixerGraph { buses };
        graph.check_tree()?;
        Ok(graph)
    }

    /// Checks ids, the single master, parents, and acyclicity.
    fn check_tree(&self) -> Result<(), FormatError> {
        let mut ids: Vec<u32> = self.buses.iter().map(|b| b.id).collect();
        ids.sort_unstable();
        if let Some(w) = ids.windows(2).find(|w| w.first() == w.get(1)) {
            return Err(FormatError::DuplicateId(w.first().copied().unwrap_or(0)));
        }
        if self.buses.iter().filter(|b| b.parent.is_none()).count() != 1 {
            return Err(FormatError::Inconsistent);
        }
        for bus in &self.buses {
            // With one master and every parent present, a walk that has not reached the
            // master after `len` steps is in a cycle.
            let mut at = bus;
            let mut steps = 0;
            while let Some(parent) = at.parent {
                at = self.bus(parent).ok_or(FormatError::Inconsistent)?;
                steps += 1;
                if steps > self.buses.len() {
                    return Err(FormatError::Inconsistent);
                }
            }
        }
        Ok(())
    }

    /// Checks every rule [`MixerGraph::parse`] enforces, for a graph built in memory rather
    /// than parsed (it round-trips through the encoder).
    ///
    /// # Errors
    /// The [`FormatError`] parsing the encoded graph would report.
    pub fn validate(&self) -> Result<(), FormatError> {
        MixerGraph::parse(&self.encode()).map(|_| ())
    }

    /// Reference encoder (the exact inverse of [`MixerGraph::parse`]).
    pub fn encode(&self) -> Vec<u8> {
        let mut w = Writer::new();
        w.bytes(&MAGIC);
        w.u16(1);
        w.u16(0);
        w.count(self.buses.len());
        w.u32(0);
        for bus in &self.buses {
            w.u32(bus.id);
            w.u32(bus.parent.unwrap_or(NO_PARENT));
            w.f32(bus.gain);
            w.u8(u8::try_from(bus.effects.len()).unwrap_or(u8::MAX));
            w.bytes(&[0; 3]);
            for slot in 0..MAX_EFFECTS {
                match bus.effects.get(slot) {
                    Some(e) => {
                        w.u8(e.kind());
                        w.bytes(&[0; 3]);
                        for p in e.params() {
                            w.f32(p);
                        }
                    }
                    None => w.bytes(&[0; 12]),
                }
            }
        }
        w.into_bytes()
    }
}

fn read_bus(r: &mut Reader<'_>) -> Result<Bus, FormatError> {
    let id = r.u32()?;
    if id == NO_PARENT {
        return Err(FormatError::Inconsistent);
    }
    let parent = r.u32()?;
    let gain = r.f32()?;
    if !(0.0..=MAX_GAIN).contains(&gain) {
        return Err(FormatError::Dimensions);
    }
    let count = usize::from(r.u8()?);
    if count > MAX_EFFECTS {
        return Err(FormatError::Dimensions);
    }
    if r.array::<3>()? != [0; 3] {
        return Err(FormatError::Reserved);
    }
    let mut effects = Vec::with_capacity(count);
    for slot in 0..MAX_EFFECTS {
        if slot < count {
            effects.push(read_effect(r)?);
        } else if r.array::<12>()? != [0; 12] {
            return Err(FormatError::Reserved);
        }
    }
    Ok(Bus {
        id,
        parent: (parent != NO_PARENT).then_some(parent),
        gain,
        effects,
    })
}

fn read_effect(r: &mut Reader<'_>) -> Result<Effect, FormatError> {
    let kind = r.u8()?;
    if r.array::<3>()? != [0; 3] {
        return Err(FormatError::Reserved);
    }
    let p0 = r.f32()?;
    let p1 = r.f32()?;
    let unused_p1 = || {
        if p1.to_bits() == 0 {
            Ok(())
        } else {
            Err(FormatError::Reserved)
        }
    };
    let in_range = |ok: bool| if ok { Ok(()) } else { Err(FormatError::Dimensions) };
    match kind {
        0 => {
            in_range((MIN_GAIN_DB..=MAX_GAIN_DB).contains(&p0))?;
            unused_p1()?;
            Ok(Effect::Gain { db: p0 })
        }
        1 | 2 => {
            in_range(p0 > 0.0 && p0 <= MAX_CUTOFF_HZ)?;
            unused_p1()?;
            Ok(if kind == 1 {
                Effect::LowPass { cutoff_hz: p0 }
            } else {
                Effect::HighPass { cutoff_hz: p0 }
            })
        }
        3 => {
            in_range(p0 > 0.0 && p0 <= 1.0)?;
            in_range(p1 > 0.0 && p1 <= MAX_RELEASE_MS)?;
            Ok(Effect::Limiter {
                threshold: p0,
                release_ms: p1,
            })
        }
        other => Err(FormatError::Encoding(u32::from(other))),
    }
}

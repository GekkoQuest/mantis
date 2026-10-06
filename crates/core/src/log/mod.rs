//! The unified per-cell log (plan 6.8, decision 0007).
//!
//! One append-only log per cell holds `(tick, intents, seeds, economy
//! commands, economy outcomes)`, plus one `TickEnd` record per tick carrying
//! the world state hash at the end of that tick, which replay compares.
//!
//! - Inbound intents are appended on enqueue.
//! - Economy outcomes are appended, then [`LogWriter::commit`] flushes and
//!   `fsync`s, and only then may the acknowledgement be sent.
//! - `Persist` calls [`LogWriter::flush_segment`]: it writes the tick's
//!   segment and `fsync`s exactly when the segment carries an economy command
//!   or outcome.
//! - The header carries the build id and the content hash. [`LogReader::open`]
//!   refuses a log written by another build or against other content.
//!
//! # Format (version 1)
//!
//! All integers are little-endian.
//!
//! Header, 106 bytes: magic `MNTSLOG\0` (8), version `u16`, build id (32),
//! content hash (32), cell id `u64`, seed `u64`, start tick `u64`, then the
//! XXH64 of the preceding 98 bytes as `u64`.
//!
//! Each record is `len: u32`, then `body` (`len` bytes), then the XXH64 of
//! `body` as `u64`. A body is `kind: u8`, `tick: u64`, then a payload:
//!
//! | kind | record    | payload                                  |
//! |------|-----------|------------------------------------------|
//! | 1    | `Intent`  | session `u64`, intent (schema codec)     |
//! | 2    | `Seed`    | seed `u64`                               |
//! | 3    | `Command` | command (schema codec)                   |
//! | 4    | `Outcome` | outcome (schema codec)                   |
//! | 5    | `TickEnd` | world state hash `u64`                   |
//!
//! A record longer than [`MAX_RECORD`] is corrupt by definition. A final
//! record cut short (a crash mid-write) is reported as
//! [`LogError::TruncatedTail`], distinct from corruption.

mod reader;
mod writer;

pub use crate::wire::{DecodeError, Decoder, Encoder, Wire};
pub use reader::LogReader;
pub use writer::{FlushReport, LogSink, LogWriter};

use core::fmt;

use crate::content::ContentHash;
use crate::rng::Seed;
use crate::time::Tick;

/// File magic.
pub const MAGIC: [u8; 8] = *b"MNTSLOG\0";
/// Format version written by this build.
pub const FORMAT_VERSION: u16 = 1;
/// Header size in bytes.
pub const HEADER_LEN: usize = 106;
/// Largest record body accepted (1 MiB).
pub const MAX_RECORD: u32 = 1 << 20;

/// Identity of an engine build (for example a hash of the source revision
/// and toolchain), supplied by the host.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct BuildId(pub [u8; 32]);

/// Identity of a cell.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct CellId(pub u64);

/// Identity of a client session.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct SessionId(pub u64);

/// The log header.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct LogHeader {
    /// The build that wrote the log.
    pub build: BuildId,
    /// The gameplay content the cell ran.
    pub content: ContentHash,
    /// The cell.
    pub cell: CellId,
    /// The cell's random seed.
    pub seed: Seed,
    /// The first tick recorded.
    pub start_tick: Tick,
}

/// The record types a cell's log carries. Each is encoded with its
/// [`Wire`] codec; hosts define them (the intent type, for instance, comes
/// from the adapter contract).
pub trait LogSchema {
    /// An inbound intent.
    type Intent: Wire;
    /// An economy command.
    type Command: Wire;
    /// An economy outcome.
    type Outcome: Wire;
}

/// One record of the log.
///
/// Every record's `tick` is a **cell** tick. For an intent it is the tick
/// whose `Inbound` phase delivers it to the simulation, not any tick the
/// client claims. Inputs that arrive early wait in a buffer that is part of
/// simulation state, so replay feeds each intent at its delivery tick and the
/// buffer behaves identically. This keeps the log in non-decreasing tick
/// order.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum LogEntry<I, C, O> {
    /// An inbound intent delivered at `tick`.
    Intent {
        /// The cell tick whose `Inbound` phase delivers it.
        tick: Tick,
        /// The sending session.
        session: SessionId,
        /// The intent.
        intent: I,
    },
    /// Extra entropy injected for `tick`.
    Seed {
        /// The tick.
        tick: Tick,
        /// The seed value.
        seed: u64,
    },
    /// An economy command applied at `tick`.
    Command {
        /// The tick.
        tick: Tick,
        /// The command.
        command: C,
    },
    /// The outcome of an economy command at `tick`.
    Outcome {
        /// The tick.
        tick: Tick,
        /// The outcome.
        outcome: O,
    },
    /// The end of `tick`, with the world state hash after it.
    TickEnd {
        /// The tick.
        tick: Tick,
        /// The world state hash.
        state_hash: u64,
    },
}

/// The entry type of a schema.
pub type SchemaEntry<S> =
    LogEntry<<S as LogSchema>::Intent, <S as LogSchema>::Command, <S as LogSchema>::Outcome>;

impl<I, C, O> LogEntry<I, C, O> {
    /// The tick of the record.
    pub const fn tick(&self) -> Tick {
        match self {
            Self::Intent { tick, .. }
            | Self::Seed { tick, .. }
            | Self::Command { tick, .. }
            | Self::Outcome { tick, .. }
            | Self::TickEnd { tick, .. } => *tick,
        }
    }
}

pub(crate) const KIND_INTENT: u8 = 1;
pub(crate) const KIND_SEED: u8 = 2;
pub(crate) const KIND_COMMAND: u8 = 3;
pub(crate) const KIND_OUTCOME: u8 = 4;
pub(crate) const KIND_TICK_END: u8 = 5;

/// Log failure. Every reader and writer operation reports one; none panics.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum LogError {
    /// The sink or source failed.
    Io(std::io::ErrorKind),
    /// Not a Mantis log.
    BadMagic,
    /// A format version this build cannot read.
    UnsupportedVersion(u16),
    /// The header checksum does not match.
    HeaderCorrupt,
    /// Written by another build: replay and recovery refuse it.
    BuildMismatch {
        /// The running build.
        expected: BuildId,
        /// The build in the header.
        found: BuildId,
    },
    /// Written against other content: replay and recovery refuse it.
    ContentMismatch {
        /// The loaded content.
        expected: ContentHash,
        /// The content in the header.
        found: ContentHash,
    },
    /// The last record is incomplete (a crash mid-write). Everything before
    /// `offset` is intact.
    TruncatedTail {
        /// Byte offset of the incomplete record.
        offset: usize,
    },
    /// A record failed its checksum, has an unknown kind, or claims an
    /// impossible length.
    Corrupt {
        /// Byte offset of the record.
        offset: usize,
    },
    /// A record's payload did not decode.
    Decode {
        /// Byte offset of the record.
        offset: usize,
        /// Why.
        error: DecodeError,
    },
    /// An append went backwards in time, or after its tick had ended.
    OutOfOrder {
        /// The earliest tick the writer accepts now.
        expected_at_least: Tick,
        /// The tick given.
        got: Tick,
    },
    /// An encoded record exceeds [`MAX_RECORD`].
    RecordTooLarge,
}

impl From<std::io::Error> for LogError {
    fn from(e: std::io::Error) -> Self {
        Self::Io(e.kind())
    }
}

impl fmt::Display for LogError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(k) => write!(f, "log I/O failed: {k}"),
            Self::BadMagic => f.write_str("not a Mantis log"),
            Self::UnsupportedVersion(v) => write!(f, "unsupported log format version {v}"),
            Self::HeaderCorrupt => f.write_str("log header checksum mismatch"),
            Self::BuildMismatch { .. } => f.write_str("log was written by a different build"),
            Self::ContentMismatch { expected, found } => {
                write!(f, "log was written against content {found}, running {expected}")
            }
            Self::TruncatedTail { offset } => write!(f, "log ends in an incomplete record at byte {offset}"),
            Self::Corrupt { offset } => write!(f, "corrupt log record at byte {offset}"),
            Self::Decode { offset, error } => write!(f, "undecodable log record at byte {offset}: {error}"),
            Self::OutOfOrder {
                expected_at_least,
                got,
            } => {
                write!(f, "log append for {got} after {expected_at_least} was required")
            }
            Self::RecordTooLarge => write!(f, "log record exceeds {MAX_RECORD} bytes"),
        }
    }
}

impl std::error::Error for LogError {}

#[cfg(test)]
mod tests;

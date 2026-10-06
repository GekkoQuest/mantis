//! Reading the unified log.

use core::marker::PhantomData;

use super::{
    BuildId, CellId, FORMAT_VERSION, HEADER_LEN, KIND_COMMAND, KIND_INTENT, KIND_OUTCOME, KIND_SEED,
    KIND_TICK_END, LogEntry, LogError, LogHeader, LogSchema, MAGIC, MAX_RECORD, SchemaEntry, SessionId,
};
use crate::content::ContentHash;
use crate::hash::StableHasher;
use crate::rng::Seed;
use crate::time::Tick;
use crate::wire::{Decoder, Wire};

/// Reads a log held in memory, verifying every record.
pub struct LogReader<'a, S: LogSchema> {
    bytes: &'a [u8],
    pos: usize,
    header: LogHeader,
    _schema: PhantomData<fn() -> S>,
}

impl<'a, S: LogSchema> LogReader<'a, S> {
    /// Opens a log, refusing anything this build must not replay: a bad
    /// magic, version, or header checksum, another build, or other content
    /// (decision 0007).
    ///
    /// # Errors
    /// [`LogError::BadMagic`], [`LogError::UnsupportedVersion`],
    /// [`LogError::HeaderCorrupt`], [`LogError::BuildMismatch`],
    /// [`LogError::ContentMismatch`], or [`LogError::TruncatedTail`] for a
    /// short header.
    pub fn open(bytes: &'a [u8], build: BuildId, content: ContentHash) -> Result<Self, LogError> {
        let header_bytes = bytes
            .get(..HEADER_LEN)
            .ok_or(LogError::TruncatedTail { offset: 0 })?;
        let (body, check) = header_bytes.split_at(HEADER_LEN - 8);
        let mut d = Decoder::new(body);
        let corrupt = |_| LogError::HeaderCorrupt;
        if d.array::<8>().map_err(corrupt)? != MAGIC {
            return Err(LogError::BadMagic);
        }
        let version = d.u16().map_err(corrupt)?;
        if version != FORMAT_VERSION {
            return Err(LogError::UnsupportedVersion(version));
        }
        if Decoder::new(check).u64().map_err(corrupt)? != StableHasher::hash_bytes(body) {
            return Err(LogError::HeaderCorrupt);
        }
        let header = LogHeader {
            build: BuildId(d.array().map_err(corrupt)?),
            content: ContentHash::from_bytes(d.array().map_err(corrupt)?),
            cell: CellId(d.u64().map_err(corrupt)?),
            seed: Seed(d.u64().map_err(corrupt)?),
            start_tick: Tick(d.u64().map_err(corrupt)?),
        };
        if header.build != build {
            return Err(LogError::BuildMismatch {
                expected: build,
                found: header.build,
            });
        }
        if header.content != content {
            return Err(LogError::ContentMismatch {
                expected: content,
                found: header.content,
            });
        }
        Ok(Self {
            bytes,
            pos: HEADER_LEN,
            header,
            _schema: PhantomData,
        })
    }

    /// The verified header.
    #[must_use]
    pub fn header(&self) -> &LogHeader {
        &self.header
    }

    /// Byte offset of the next record.
    #[must_use]
    pub fn position(&self) -> usize {
        self.pos
    }

    /// The next record, or `Ok(None)` at a clean end.
    ///
    /// # Errors
    /// [`LogError::TruncatedTail`] for an incomplete final record,
    /// [`LogError::Corrupt`] for a bad length, checksum, or kind, and
    /// [`LogError::Decode`] for a payload that does not decode. After an
    /// error the reader stays at the failing record.
    pub fn next_entry(&mut self) -> Result<Option<SchemaEntry<S>>, LogError> {
        let offset = self.pos;
        let rest = self.bytes.get(offset..).unwrap_or(&[]);
        if rest.is_empty() {
            return Ok(None);
        }
        let Some((len_bytes, after_len)) = rest.split_first_chunk::<4>() else {
            return Err(LogError::TruncatedTail { offset });
        };
        let len = u32::from_le_bytes(*len_bytes);
        if !(9..=MAX_RECORD).contains(&len) {
            return Err(LogError::Corrupt { offset });
        }
        let len = len as usize;
        let (Some(body), Some(check)) = (after_len.get(..len), after_len.get(len..len + 8)) else {
            return Err(LogError::TruncatedTail { offset });
        };
        let mut c = Decoder::new(check);
        if c.u64().map_err(|_| LogError::Corrupt { offset })? != StableHasher::hash_bytes(body) {
            return Err(LogError::Corrupt { offset });
        }
        let entry = Self::decode_body(body).map_err(|e| match e {
            DecodeOrKind::Kind => LogError::Corrupt { offset },
            DecodeOrKind::Decode(error) => LogError::Decode { offset, error },
        })?;
        self.pos = offset + 4 + len + 8;
        Ok(Some(entry))
    }

    fn decode_body(body: &[u8]) -> Result<SchemaEntry<S>, DecodeOrKind> {
        let mut d = Decoder::new(body);
        let kind = d.u8()?;
        let tick = Tick(d.u64()?);
        let entry = match kind {
            KIND_INTENT => LogEntry::Intent {
                tick,
                session: SessionId(d.u64()?),
                intent: S::Intent::decode(&mut d)?,
            },
            KIND_SEED => LogEntry::Seed { tick, seed: d.u64()? },
            KIND_COMMAND => LogEntry::Command {
                tick,
                command: S::Command::decode(&mut d)?,
            },
            KIND_OUTCOME => LogEntry::Outcome {
                tick,
                outcome: S::Outcome::decode(&mut d)?,
            },
            KIND_TICK_END => LogEntry::TickEnd {
                tick,
                state_hash: d.u64()?,
            },
            _ => return Err(DecodeOrKind::Kind),
        };
        d.finish()?;
        Ok(entry)
    }
}

enum DecodeOrKind {
    Kind,
    Decode(super::DecodeError),
}

impl From<super::DecodeError> for DecodeOrKind {
    fn from(e: super::DecodeError) -> Self {
        Self::Decode(e)
    }
}

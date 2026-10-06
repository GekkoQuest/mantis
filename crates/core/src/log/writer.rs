//! Appending to the unified log.

use core::marker::PhantomData;
use std::io::Write as _;

use super::{
    FORMAT_VERSION, KIND_COMMAND, KIND_INTENT, KIND_OUTCOME, KIND_SEED, KIND_TICK_END, LogError, LogHeader,
    LogSchema, MAGIC, MAX_RECORD, SessionId,
};
use crate::hash::StableHasher;
use crate::time::Tick;
use crate::wire::{Encoder, Wire};

/// Where log bytes go.
pub trait LogSink {
    /// Appends `bytes`.
    ///
    /// # Errors
    /// The underlying I/O error.
    fn append(&mut self, bytes: &[u8]) -> std::io::Result<()>;

    /// Makes everything appended so far durable (`fsync`).
    ///
    /// # Errors
    /// The underlying I/O error.
    fn sync(&mut self) -> std::io::Result<()>;
}

/// A boxed sink (for hosts that choose the sink at runtime).
impl<T: LogSink + ?Sized> LogSink for Box<T> {
    fn append(&mut self, bytes: &[u8]) -> std::io::Result<()> {
        (**self).append(bytes)
    }

    fn sync(&mut self) -> std::io::Result<()> {
        (**self).sync()
    }
}

/// In-memory sink for tests and tools; `sync` is a no-op.
impl LogSink for Vec<u8> {
    fn append(&mut self, bytes: &[u8]) -> std::io::Result<()> {
        self.extend_from_slice(bytes);
        Ok(())
    }

    fn sync(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// File sink: `write_all`, and `sync_data` for durability.
impl LogSink for std::fs::File {
    fn append(&mut self, bytes: &[u8]) -> std::io::Result<()> {
        self.write_all(bytes)
    }

    fn sync(&mut self) -> std::io::Result<()> {
        self.sync_data()
    }
}

/// What a flush did.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct FlushReport {
    /// Bytes written to the sink.
    pub bytes: usize,
    /// True when the sink was `fsync`ed.
    pub synced: bool,
}

/// Appends records to a cell's log. Records accumulate in a segment buffer
/// until [`LogWriter::flush_segment`] or [`LogWriter::commit`].
///
/// Ticks are enforced: records arrive in non-decreasing tick order, and once
/// `TickEnd(t)` is written, every later record is for a tick after `t`.
pub struct LogWriter<S: LogSchema, K: LogSink> {
    sink: K,
    segment: Vec<u8>,
    durable_pending: bool,
    min_tick: Tick,
    _schema: PhantomData<fn() -> S>,
}

impl<S: LogSchema, K: LogSink> LogWriter<S, K> {
    /// Writes and syncs the header, then returns a writer whose segment
    /// buffer holds `segment_capacity` bytes before it must grow.
    ///
    /// # Errors
    /// [`LogError::Io`].
    pub fn create(mut sink: K, header: &LogHeader, segment_capacity: usize) -> Result<Self, LogError> {
        let mut buf = Vec::with_capacity(super::HEADER_LEN);
        {
            let mut e = Encoder::new(&mut buf);
            e.bytes(&MAGIC);
            e.u16(FORMAT_VERSION);
            e.bytes(&header.build.0);
            e.bytes(header.content.as_bytes());
            e.u64(header.cell.0);
            e.u64(header.seed.0);
            e.u64(header.start_tick.0);
        }
        let check = StableHasher::hash_bytes(&buf);
        Encoder::new(&mut buf).u64(check);
        sink.append(&buf)?;
        sink.sync()?;
        Ok(Self {
            sink,
            segment: Vec::with_capacity(segment_capacity),
            durable_pending: false,
            min_tick: header.start_tick,
            _schema: PhantomData,
        })
    }

    fn check_tick(&self, tick: Tick) -> Result<(), LogError> {
        if tick < self.min_tick {
            return Err(LogError::OutOfOrder {
                expected_at_least: self.min_tick,
                got: tick,
            });
        }
        Ok(())
    }

    /// Frames one record: `len`, body (kind, tick, payload), checksum.
    fn record(
        &mut self,
        kind: u8,
        tick: Tick,
        payload: impl FnOnce(&mut Encoder<'_>),
    ) -> Result<(), LogError> {
        self.check_tick(tick)?;
        let start = self.segment.len();
        let mut e = Encoder::new(&mut self.segment);
        e.u32(0); // length, patched below
        e.u8(kind);
        e.u64(tick.0);
        payload(&mut e);
        let body_start = start + 4;
        let body_len = self.segment.len() - body_start;
        let Ok(len) = u32::try_from(body_len) else {
            self.segment.truncate(start);
            return Err(LogError::RecordTooLarge);
        };
        if len > MAX_RECORD {
            self.segment.truncate(start);
            return Err(LogError::RecordTooLarge);
        }
        if let Some(slot) = self.segment.get_mut(start..body_start) {
            slot.copy_from_slice(&len.to_le_bytes());
        }
        let check = StableHasher::hash_bytes(self.segment.get(body_start..).unwrap_or(&[]));
        Encoder::new(&mut self.segment).u64(check);
        self.min_tick = tick;
        Ok(())
    }

    /// Appends an inbound intent (on enqueue).
    ///
    /// # Errors
    /// [`LogError::OutOfOrder`], [`LogError::RecordTooLarge`].
    pub fn append_intent(
        &mut self,
        tick: Tick,
        session: SessionId,
        intent: &S::Intent,
    ) -> Result<(), LogError> {
        self.record(KIND_INTENT, tick, |e| {
            e.u64(session.0);
            intent.encode(e);
        })
    }

    /// Appends injected entropy for `tick`.
    ///
    /// # Errors
    /// [`LogError::OutOfOrder`].
    pub fn append_seed(&mut self, tick: Tick, seed: u64) -> Result<(), LogError> {
        self.record(KIND_SEED, tick, |e| e.u64(seed))
    }

    /// Appends an economy command. The segment now needs an `fsync`.
    ///
    /// # Errors
    /// [`LogError::OutOfOrder`], [`LogError::RecordTooLarge`].
    pub fn append_command(&mut self, tick: Tick, command: &S::Command) -> Result<(), LogError> {
        self.record(KIND_COMMAND, tick, |e| command.encode(e))?;
        self.durable_pending = true;
        Ok(())
    }

    /// Appends an economy outcome. The segment now needs an `fsync`; call
    /// [`LogWriter::commit`] before acknowledging the outcome to anyone.
    ///
    /// # Errors
    /// [`LogError::OutOfOrder`], [`LogError::RecordTooLarge`].
    pub fn append_outcome(&mut self, tick: Tick, outcome: &S::Outcome) -> Result<(), LogError> {
        self.record(KIND_OUTCOME, tick, |e| outcome.encode(e))?;
        self.durable_pending = true;
        Ok(())
    }

    /// Ends `tick` with the world state hash after it. Later records must be
    /// for later ticks.
    ///
    /// # Errors
    /// [`LogError::OutOfOrder`].
    pub fn end_tick(&mut self, tick: Tick, state_hash: u64) -> Result<(), LogError> {
        self.record(KIND_TICK_END, tick, |e| e.u64(state_hash))?;
        self.min_tick = tick.next();
        Ok(())
    }

    /// The `Persist` step: writes the segment to the sink and `fsync`s when it
    /// carries an economy command or outcome. Clears the segment, keeping its
    /// capacity.
    ///
    /// # Errors
    /// [`LogError::Io`]; the segment is kept for a retry.
    pub fn flush_segment(&mut self) -> Result<FlushReport, LogError> {
        self.flush(false)
    }

    /// Writes the segment and `fsync`s unconditionally: the economy
    /// acknowledgement path (decision 0007).
    ///
    /// # Errors
    /// [`LogError::Io`]; the segment is kept for a retry.
    pub fn commit(&mut self) -> Result<FlushReport, LogError> {
        self.flush(true)
    }

    fn flush(&mut self, force_sync: bool) -> Result<FlushReport, LogError> {
        let bytes = self.segment.len();
        if bytes > 0 {
            self.sink.append(&self.segment)?;
            self.segment.clear();
        }
        let synced = force_sync || self.durable_pending;
        if synced {
            self.sink.sync()?;
            self.durable_pending = false;
        }
        Ok(FlushReport { bytes, synced })
    }

    /// Bytes buffered and not yet flushed.
    #[must_use]
    pub fn pending_bytes(&self) -> usize {
        self.segment.len()
    }

    /// The sink.
    pub fn sink(&self) -> &K {
        &self.sink
    }

    /// Consumes the writer, returning the sink. Unflushed records are lost;
    /// flush first.
    pub fn into_sink(self) -> K {
        self.sink
    }
}

//! Sequence-numbered input history for rewind-and-replay.

use std::collections::VecDeque;

use crate::core_api::{InputSeq, MotionModifiers, MoveInput};

/// One predicted input: what was sent, the modifiers it was integrated with, and the
/// state prediction produced from it.
#[derive(Clone, Copy, PartialEq, Debug)]
pub struct InputRecord<S> {
    /// The input as sent.
    pub input: MoveInput,
    /// Modifiers it was integrated with.
    pub mods: MotionModifiers,
    /// Predicted state after applying it.
    pub predicted: S,
}

/// Fixed-capacity ring of contiguous input records, oldest first.
///
/// Sequence numbers are contiguous: record `i` has sequence `front + i`. Pushing to a
/// full buffer evicts the oldest record and counts it; it never grows.
#[derive(Clone, Debug)]
pub struct InputBuffer<S> {
    records: VecDeque<InputRecord<S>>,
    capacity: usize,
    evicted: u64,
}

/// Errors from [`InputBuffer::push`].
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum BufferError {
    /// The pushed sequence does not follow the newest record.
    NotContiguous {
        /// The sequence expected next.
        expected: InputSeq,
        /// The sequence pushed.
        got: InputSeq,
    },
}

impl core::fmt::Display for BufferError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            BufferError::NotContiguous { expected, got } => {
                write!(
                    f,
                    "input sequence {} pushed where {} was expected",
                    got.0, expected.0
                )
            }
        }
    }
}

impl std::error::Error for BufferError {}

impl<S: Copy> InputBuffer<S> {
    /// A buffer holding up to `capacity` records (at least one) without allocating.
    pub fn with_capacity(capacity: usize) -> Self {
        let capacity = capacity.max(1);
        Self {
            records: VecDeque::with_capacity(capacity),
            capacity,
            evicted: 0,
        }
    }

    /// Records held.
    pub fn len(&self) -> usize {
        self.records.len()
    }

    /// True when empty.
    pub fn is_empty(&self) -> bool {
        self.records.is_empty()
    }

    /// Capacity.
    pub fn capacity(&self) -> usize {
        self.capacity
    }

    /// Records evicted unacknowledged because the buffer was full.
    pub fn evicted(&self) -> u64 {
        self.evicted
    }

    /// Sequence of the oldest record.
    pub fn oldest_seq(&self) -> Option<InputSeq> {
        self.records.front().map(|r| r.input.seq)
    }

    /// Sequence of the newest record.
    pub fn newest_seq(&self) -> Option<InputSeq> {
        self.records.back().map(|r| r.input.seq)
    }

    /// Appends a record. Its sequence must follow the newest record (any sequence is
    /// accepted into an empty buffer).
    ///
    /// # Errors
    /// [`BufferError::NotContiguous`].
    pub fn push(&mut self, record: InputRecord<S>) -> Result<(), BufferError> {
        if let Some(newest) = self.newest_seq()
            && record.input.seq != newest.next()
        {
            return Err(BufferError::NotContiguous {
                expected: newest.next(),
                got: record.input.seq,
            });
        }
        if self.records.len() >= self.capacity {
            let _ = self.records.pop_front();
            self.evicted = self.evicted.saturating_add(1);
        }
        self.records.push_back(record);
        Ok(())
    }

    fn offset_of(&self, seq: InputSeq) -> Option<usize> {
        let oldest = self.oldest_seq()?;
        let d = usize::try_from(seq.0.wrapping_sub(oldest.0)).ok()?;
        (d < self.records.len()).then_some(d)
    }

    /// The record with sequence `seq`.
    pub fn get(&self, seq: InputSeq) -> Option<&InputRecord<S>> {
        self.offset_of(seq).and_then(|i| self.records.get(i))
    }

    /// Drops every record at or before `seq`. A `seq` older than the buffer drops nothing;
    /// one newer than the buffer drops everything.
    pub fn ack(&mut self, seq: InputSeq) {
        while let Some(front) = self.records.front() {
            if seq.is_newer_than(front.input.seq) || seq == front.input.seq {
                let _ = self.records.pop_front();
            } else {
                break;
            }
        }
    }

    /// Records newer than `seq`, oldest first, mutable (replay rewrites predictions).
    pub fn after_mut(&mut self, seq: InputSeq) -> impl Iterator<Item = &mut InputRecord<S>> {
        self.records
            .iter_mut()
            .filter(move |r| r.input.seq.is_newer_than(seq))
    }

    /// Drops every record.
    pub fn clear(&mut self) {
        self.records.clear();
    }
}

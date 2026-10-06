//! The audio thread: drains a bounded event queue and mixes blocks for an output.
//!
//! Game threads post events with [`AudioSender::send`], which never blocks: when the
//! queue is full the event is dropped and counted, because a late sound is worse than a
//! missing one and a blocked sim or render thread is worse than both. The mixer itself
//! ([`AudioBackend`]) is data-driven and lives in `mantis-audio`; this module is only the
//! thread, the queue, and the pacing.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, SyncSender, TrySendError, sync_channel};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use super::sim_thread::ThreadPanicked;

/// The mixer: consumes events, renders interleaved sample blocks.
pub trait AudioBackend: Send + 'static {
    /// Event type posted by game threads.
    type Event: Send + 'static;
    /// Applies one event.
    fn handle(&mut self, event: Self::Event);
    /// Renders the next block of interleaved samples into `out`.
    fn render(&mut self, out: &mut [f32]);
}

/// Where mixed blocks go.
pub trait AudioOutput: Send + 'static {
    /// Interleaved samples per block the output wants.
    fn block_len(&self) -> usize;
    /// Accepts one block; returns how long to wait before the next is needed.
    fn write(&mut self, block: &[f32]) -> Duration;
}

/// An output that discards blocks at a fixed cadence (no device).
#[derive(Clone, Copy, Debug)]
pub struct NullOutput {
    /// Samples per block.
    pub block_len: usize,
    /// Wall time one block represents.
    pub block_duration: Duration,
}

impl AudioOutput for NullOutput {
    fn block_len(&self) -> usize {
        self.block_len
    }
    fn write(&mut self, _block: &[f32]) -> Duration {
        self.block_duration
    }
}

/// Posting end of the audio queue. Cheap to clone.
#[derive(Debug)]
pub struct AudioSender<E> {
    tx: SyncSender<E>,
    dropped: Arc<AtomicU64>,
}

impl<E> Clone for AudioSender<E> {
    fn clone(&self) -> Self {
        Self {
            tx: self.tx.clone(),
            dropped: Arc::clone(&self.dropped),
        }
    }
}

/// Returned when an event could not be queued.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum AudioSendError {
    /// The queue was full; the event was dropped and counted.
    Full,
    /// The audio thread has stopped.
    Stopped,
}

impl core::fmt::Display for AudioSendError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            AudioSendError::Full => f.write_str("audio queue full; event dropped"),
            AudioSendError::Stopped => f.write_str("audio thread stopped"),
        }
    }
}

impl std::error::Error for AudioSendError {}

impl<E> AudioSender<E> {
    /// Posts an event without blocking.
    ///
    /// # Errors
    /// [`AudioSendError::Full`] (counted) or [`AudioSendError::Stopped`].
    pub fn send(&self, event: E) -> Result<(), AudioSendError> {
        match self.tx.try_send(event) {
            Ok(()) => Ok(()),
            Err(TrySendError::Full(_)) => {
                self.dropped.fetch_add(1, Ordering::Relaxed);
                Err(AudioSendError::Full)
            }
            Err(TrySendError::Disconnected(_)) => Err(AudioSendError::Stopped),
        }
    }

    /// Events dropped because the queue was full.
    pub fn dropped(&self) -> u64 {
        self.dropped.load(Ordering::Relaxed)
    }
}

/// A running audio thread.
#[derive(Debug)]
pub struct AudioThread<B: AudioBackend, O: AudioOutput> {
    stop: Arc<AtomicBool>,
    handle: Option<JoinHandle<(B, O)>>,
}

impl<B: AudioBackend, O: AudioOutput> AudioThread<B, O> {
    /// Spawns the thread with a queue of `queue_capacity` events (at least one). The
    /// block buffer is allocated once here.
    ///
    /// # Errors
    /// The OS error if the thread cannot be spawned.
    pub fn spawn(
        backend: B,
        output: O,
        queue_capacity: usize,
    ) -> std::io::Result<(Self, AudioSender<B::Event>)> {
        let (tx, rx) = sync_channel(queue_capacity.max(1));
        let stop = Arc::new(AtomicBool::new(false));
        let stop_flag = Arc::clone(&stop);
        let handle = thread::Builder::new()
            .name("mantis-audio".into())
            .spawn(move || run(backend, output, &rx, &stop_flag))?;
        Ok((
            Self {
                stop,
                handle: Some(handle),
            },
            AudioSender {
                tx,
                dropped: Arc::new(AtomicU64::new(0)),
            },
        ))
    }

    /// Stops and joins the thread, returning the backend and output.
    ///
    /// # Errors
    /// [`ThreadPanicked`] if the thread panicked.
    pub fn stop(mut self) -> Result<(B, O), ThreadPanicked> {
        self.stop_and_join().ok_or(ThreadPanicked("audio"))
    }

    fn stop_and_join(&mut self) -> Option<(B, O)> {
        self.stop.store(true, Ordering::Release);
        let h = self.handle.take()?;
        h.thread().unpark();
        h.join().ok()
    }
}

impl<B: AudioBackend, O: AudioOutput> Drop for AudioThread<B, O> {
    fn drop(&mut self) {
        let _ = self.stop_and_join();
    }
}

fn run<B: AudioBackend, O: AudioOutput>(
    mut backend: B,
    mut output: O,
    rx: &Receiver<B::Event>,
    stop: &AtomicBool,
) -> (B, O) {
    let mut block = vec![0.0f32; output.block_len()];
    while !stop.load(Ordering::Acquire) {
        while let Ok(ev) = rx.try_recv() {
            backend.handle(ev);
        }
        block.fill(0.0);
        backend.render(&mut block);
        let wait = output.write(&block);
        if !wait.is_zero() {
            thread::park_timeout(wait);
        }
    }
    // Apply events that arrived before stop so nothing posted is silently lost.
    while let Ok(ev) = rx.try_recv() {
        backend.handle(ev);
    }
    (backend, output)
}

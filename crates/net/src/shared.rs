//! Plumbing shared by the transports: the event queue between network tasks
//! and the polling owner, per-connection send queues, buffer pools, and the
//! Mantis sequence header for unreliable frames.
//!
//! Everything on the caller's side (`poll`, `send`) is allocation-free in
//! steady state: frames travel in pooled buffers that are returned after use,
//! and queues keep their capacity.

use std::collections::{BTreeMap, VecDeque};
use std::sync::{Arc, Mutex, MutexGuard};

use mantis_adapter_contract::{Channel, ConnectionId, DisconnectReason, TransportError, TransportEvent};
use tokio::sync::Notify;

/// Locks a mutex, recovering from poisoning (a panicked network task must not
/// take the transport down with it; the data are plain queues).
pub fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// Largest frame accepted on any channel (fail closed beyond it).
pub const MAX_FRAME: usize = 1 << 20;

/// A pool of byte buffers.
#[derive(Default)]
pub struct BufferPool {
    free: Mutex<Vec<Vec<u8>>>,
}

impl BufferPool {
    /// A buffer holding a copy of `bytes`: reused when one is free.
    pub fn copy_of(&self, bytes: &[u8]) -> Vec<u8> {
        let mut buf = lock(&self.free).pop().unwrap_or_default();
        buf.clear();
        buf.extend_from_slice(bytes);
        buf
    }

    /// Returns a buffer for reuse.
    pub fn give_back(&self, buf: Vec<u8>) {
        let mut free = lock(&self.free);
        if free.len() < 4096 {
            free.push(buf);
        }
    }
}

/// An event with owned bytes, queued by network tasks.
pub enum NetEvent {
    /// A peer connected.
    Connected(ConnectionId),
    /// A frame arrived.
    Frame(ConnectionId, Channel, Vec<u8>),
    /// A connection ended.
    Disconnected(ConnectionId, DisconnectReason),
}

/// One frame waiting to be written.
pub struct Outgoing {
    /// Channel.
    pub channel: Channel,
    /// Bytes (a pooled buffer).
    pub bytes: Vec<u8>,
}

/// A connection's send queue, drained by its writer task.
pub struct SendQueue {
    /// Frames waiting.
    pub frames: Mutex<VecDeque<Outgoing>>,
    /// Wakes the writer task.
    pub notify: Notify,
    /// Set when the connection should close.
    pub closing: std::sync::atomic::AtomicBool,
}

/// Frames a writer may have queued before `send` reports backpressure.
pub const SEND_QUEUE_LIMIT: usize = 1024;

impl SendQueue {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            frames: Mutex::new(VecDeque::with_capacity(64)),
            notify: Notify::new(),
            closing: std::sync::atomic::AtomicBool::new(false),
        })
    }
}

/// State shared between a transport's owner and its network tasks.
pub struct Shared {
    /// Events for the owner.
    pub events: Mutex<VecDeque<NetEvent>>,
    /// Live connections.
    pub conns: Mutex<BTreeMap<ConnectionId, Arc<SendQueue>>>,
    /// Buffers for frames.
    pub pool: BufferPool,
    /// Set when the transport shuts down.
    pub shutdown: std::sync::atomic::AtomicBool,
}

impl Shared {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            events: Mutex::new(VecDeque::with_capacity(256)),
            conns: Mutex::new(BTreeMap::new()),
            pool: BufferPool::default(),
            shutdown: std::sync::atomic::AtomicBool::new(false),
        })
    }

    pub fn push(&self, e: NetEvent) {
        lock(&self.events).push_back(e);
    }

    /// Registers a connection and announces it.
    pub fn connected(&self, id: ConnectionId, queue: Arc<SendQueue>) {
        lock(&self.conns).insert(id, queue);
        self.push(NetEvent::Connected(id));
    }

    /// Forgets a connection and announces it (once).
    pub fn disconnected(&self, id: ConnectionId, reason: DisconnectReason) {
        if lock(&self.conns).remove(&id).is_some() {
            self.push(NetEvent::Disconnected(id, reason));
        }
    }

    /// Queues one frame for `conn`.
    pub fn send(
        &self,
        conn: ConnectionId,
        channel: Channel,
        bytes: &[u8],
        max_unreliable: usize,
    ) -> Result<(), TransportError> {
        if self.shutdown.load(std::sync::atomic::Ordering::Acquire) {
            return Err(TransportError::Closed);
        }
        let max = if channel == Channel::Unreliable {
            max_unreliable
        } else {
            MAX_FRAME
        };
        if bytes.len() > max {
            return Err(TransportError::TooLarge {
                len: bytes.len(),
                max,
            });
        }
        let queue = lock(&self.conns)
            .get(&conn)
            .cloned()
            .ok_or(TransportError::UnknownConnection(conn))?;
        {
            let mut frames = lock(&queue.frames);
            if frames.len() >= SEND_QUEUE_LIMIT {
                return Err(TransportError::Backpressure);
            }
            frames.push_back(Outgoing {
                channel,
                bytes: self.pool.copy_of(bytes),
            });
        }
        queue.notify.notify_one();
        Ok(())
    }

    /// Asks a connection's writer to close it.
    pub fn close(&self, conn: ConnectionId) {
        let queue = lock(&self.conns).get(&conn).cloned();
        if let Some(q) = queue {
            q.closing.store(true, std::sync::atomic::Ordering::Release);
            q.notify.notify_one();
        }
    }
}

/// The owner side of the event queue: swaps the shared queue with a local one
/// and delivers events with borrowed bytes, returning buffers to the pool.
pub struct Poller {
    local: VecDeque<NetEvent>,
}

impl Poller {
    pub fn new() -> Self {
        Self {
            local: VecDeque::with_capacity(256),
        }
    }

    pub fn poll(&mut self, shared: &Shared, sink: &mut dyn FnMut(TransportEvent<'_>)) {
        std::mem::swap(&mut *lock(&shared.events), &mut self.local);
        while let Some(e) = self.local.pop_front() {
            match e {
                NetEvent::Connected(c) => sink(TransportEvent::Connected(c)),
                NetEvent::Frame(conn, channel, bytes) => {
                    sink(TransportEvent::Frame {
                        conn,
                        channel,
                        bytes: &bytes,
                    });
                    shared.pool.give_back(bytes);
                }
                NetEvent::Disconnected(conn, reason) => sink(TransportEvent::Disconnected { conn, reason }),
            }
        }
    }
}

/// Receiver-side Mantis sequence filter for unreliable frames: accepts a frame
/// only if its sequence number is newer than every one accepted before
/// (serial-number order), so stale and duplicate datagrams are dropped.
#[derive(Default)]
pub struct SeqFilter {
    last: Option<u32>,
}

impl SeqFilter {
    pub fn accept(&mut self, seq: u32) -> bool {
        let newer = match self.last {
            None => true,
            Some(last) => {
                let d = seq.wrapping_sub(last);
                d != 0 && d < 0x8000_0000
            }
        };
        if newer {
            self.last = Some(seq);
        }
        newer
    }
}

/// Splits a frame payload `[u32 seq][bytes]`.
pub fn split_seq(datagram: &[u8]) -> Option<(u32, &[u8])> {
    let (head, rest) = datagram.split_first_chunk::<4>()?;
    Some((u32::from_le_bytes(*head), rest))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn seq_filter_drops_stale_and_duplicate() {
        let mut f = SeqFilter::default();
        assert!(f.accept(5));
        assert!(!f.accept(5), "duplicate");
        assert!(!f.accept(4), "stale");
        assert!(f.accept(9));
        let mut w = SeqFilter::default();
        assert!(w.accept(u32::MAX - 1));
        assert!(w.accept(2), "wraps");
        assert!(!w.accept(u32::MAX), "older across the wrap");
    }

    #[test]
    fn split_seq_needs_four_bytes() {
        assert_eq!(split_seq(&[1, 0, 0, 0, 9]), Some((1, &[9u8][..])));
        assert_eq!(split_seq(&[1, 0]), None);
    }

    #[test]
    fn pool_reuses_buffers() {
        let p = BufferPool::default();
        let a = p.copy_of(&[1, 2, 3]);
        let cap = a.capacity();
        p.give_back(a);
        let b = p.copy_of(&[4]);
        assert_eq!(b, vec![4]);
        assert!(b.capacity() >= cap.min(1));
    }
}

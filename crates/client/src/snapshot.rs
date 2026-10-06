//! The decoded authoritative snapshot as the client simulation consumes it, and the
//! preallocated channel that carries it from the network thread to the simulation thread.
//!
//! The network thread ([`crate::net`]) decodes the wire with `mantis-adapter-contract`
//! and fills these frames; the simulation never sees the wire types. The frame is generic
//! over the avatar state so the simulation can run with any motion model in tests. Also
//! here: the bounded channel that carries timeline markers on to the render thread's
//! presentation.

use std::sync::mpsc::{Receiver, SyncSender, TrySendError, sync_channel};

use mantis_core::graph::TimelineMarker;

use crate::core_api::{Angle16, EntityId, InputSeq, MotionModifiers, Tick, Vec3};
use crate::time::HostInstant;

/// A remote entity's state in a snapshot.
#[derive(Clone, Copy, PartialEq, Debug)]
pub struct RemoteState {
    /// The entity.
    pub id: EntityId,
    /// Server tick this sample describes (lower interest tiers may send older samples).
    pub tick: Tick,
    /// Position.
    pub position: Vec3,
    /// Velocity.
    pub velocity: Vec3,
    /// Facing.
    pub yaw: Angle16,
}

/// One decoded authoritative snapshot.
#[derive(Clone, Debug)]
pub struct SnapshotFrame<S> {
    /// Server tick the snapshot describes.
    pub server_tick: Tick,
    /// Host instant it arrived.
    pub received_at: HostInstant,
    /// Last input sequence the server applied for this client's avatar.
    pub ack: Option<InputSeq>,
    /// The avatar and its full-precision state: exactly the result of applying every
    /// input up to and including `ack`, one step per input, and nothing else.
    pub local: Option<(EntityId, S)>,
    /// Movement modifiers in effect for the avatar at that state.
    pub local_mods: MotionModifiers,
    /// Remote entities in interest.
    pub remotes: Vec<RemoteState>,
    /// Remote entities that left interest or despawned.
    pub removed: Vec<EntityId>,
    /// Timeline markers for presentation.
    pub markers: Vec<TimelineMarker>,
    /// Entities that entered interest, with their appearance id.
    pub entered: Vec<(EntityId, u32)>,
    remote_capacity: usize,
}

impl<S> SnapshotFrame<S> {
    fn with_capacity(remote_capacity: usize) -> Self {
        Self {
            server_tick: Tick::ZERO,
            received_at: HostInstant::ZERO,
            ack: None,
            local: None,
            local_mods: MotionModifiers::default(),
            remotes: Vec::with_capacity(remote_capacity),
            removed: Vec::with_capacity(remote_capacity),
            markers: Vec::with_capacity(remote_capacity),
            entered: Vec::with_capacity(remote_capacity),
            remote_capacity,
        }
    }

    /// Clears the frame for reuse, keeping capacity.
    pub fn clear(&mut self) {
        self.server_tick = Tick::ZERO;
        self.received_at = HostInstant::ZERO;
        self.ack = None;
        self.local = None;
        self.local_mods = MotionModifiers::default();
        self.remotes.clear();
        self.removed.clear();
        self.markers.clear();
        self.entered.clear();
    }

    /// Adds a remote state; false (and nothing added) when the frame is full.
    pub fn push_remote(&mut self, r: RemoteState) -> bool {
        if self.remotes.len() >= self.remote_capacity {
            return false;
        }
        self.remotes.push(r);
        true
    }

    /// Adds an entered entity; false (and nothing added) when the frame is full.
    pub fn push_entered(&mut self, id: EntityId, appearance: u32) -> bool {
        if self.entered.len() >= self.remote_capacity {
            return false;
        }
        self.entered.push((id, appearance));
        true
    }

    /// Adds a timeline marker; false (and nothing added) when the frame is full.
    pub fn push_marker(&mut self, m: TimelineMarker) -> bool {
        if self.markers.len() >= self.remote_capacity {
            return false;
        }
        self.markers.push(m);
        true
    }

    /// Adds a removal; false (and nothing added) when the frame is full.
    pub fn push_removed(&mut self, id: EntityId) -> bool {
        if self.removed.len() >= self.remote_capacity {
            return false;
        }
        self.removed.push(id);
        true
    }
}

/// Network end of the snapshot channel.
#[derive(Debug)]
pub struct SnapshotSender<S> {
    tx: SyncSender<Box<SnapshotFrame<S>>>,
    free: Receiver<Box<SnapshotFrame<S>>>,
    dropped: u64,
}

/// Simulation end of the snapshot channel.
#[derive(Debug)]
pub struct SnapshotInbox<S> {
    rx: Receiver<Box<SnapshotFrame<S>>>,
    recycle: SyncSender<Box<SnapshotFrame<S>>>,
}

/// Creates a snapshot channel with `frames` preallocated frames.
pub fn snapshot_channel<S: Send>(
    frames: usize,
    remote_capacity: usize,
) -> (SnapshotSender<S>, SnapshotInbox<S>) {
    let frames = frames.max(1);
    let (tx, rx) = sync_channel(frames);
    let (recycle, free) = sync_channel(frames);
    for _ in 0..frames {
        let _ = recycle.try_send(Box::new(SnapshotFrame::with_capacity(remote_capacity)));
    }
    (
        SnapshotSender { tx, free, dropped: 0 },
        SnapshotInbox { rx, recycle },
    )
}

impl<S> SnapshotInbox<S> {
    /// Calls `f` on every delivered frame, oldest first, recycling each afterward.
    /// Returns the number of frames drained. Never blocks, never allocates.
    pub fn drain(&mut self, mut f: impl FnMut(&SnapshotFrame<S>)) -> u32 {
        let mut n = 0u32;
        while let Ok(frame) = self.rx.try_recv() {
            f(&frame);
            n = n.saturating_add(1);
            let _ = self.recycle.try_send(frame);
        }
        n
    }
}

impl<S> SnapshotSender<S> {
    /// A cleared frame to fill, or `None` when every frame is in flight (the snapshot is
    /// then dropped and counted; the next one supersedes it).
    pub fn acquire(&mut self) -> Option<Box<SnapshotFrame<S>>> {
        if let Ok(mut f) = self.free.try_recv() {
            f.clear();
            Some(f)
        } else {
            self.dropped = self.dropped.saturating_add(1);
            None
        }
    }

    /// Delivers a filled frame. Returns false if the simulation side is gone.
    pub fn send(&mut self, frame: Box<SnapshotFrame<S>>) -> bool {
        match self.tx.try_send(frame) {
            Ok(()) => true,
            Err(TrySendError::Full(_)) => {
                // Unreachable: frames in flight never exceed channel capacity.
                self.dropped = self.dropped.saturating_add(1);
                true
            }
            Err(TrySendError::Disconnected(_)) => false,
        }
    }

    /// Snapshots dropped for lack of a free frame.
    pub fn dropped(&self) -> u64 {
        self.dropped
    }
}

/// A timeline marker scheduled on the host timeline: the simulation maps the marker's
/// server tick through its server timeline; presentation fires at `at`.
#[derive(Clone, Copy, PartialEq, Debug)]
pub struct ScheduledMarker {
    /// The marker.
    pub marker: TimelineMarker,
    /// Host instant of the marker's tick.
    pub at: HostInstant,
}

/// Simulation end of the marker channel (simulation thread to render thread).
#[derive(Debug)]
pub struct MarkerSender {
    tx: SyncSender<ScheduledMarker>,
    dropped: u64,
}

/// Render-thread end of the marker channel.
#[derive(Debug)]
pub struct MarkerReceiver {
    rx: Receiver<ScheduledMarker>,
}

/// Creates a bounded marker channel. Sending never blocks or allocates; when the render
/// thread falls `capacity` markers behind, new markers are dropped and counted
/// (presentation is best-effort).
pub fn marker_channel(capacity: usize) -> (MarkerSender, MarkerReceiver) {
    let (tx, rx) = sync_channel(capacity.max(1));
    (MarkerSender { tx, dropped: 0 }, MarkerReceiver { rx })
}

impl MarkerSender {
    /// Queues a marker. Returns false when it was dropped (queue full or receiver gone).
    pub fn send(&mut self, m: ScheduledMarker) -> bool {
        if self.tx.try_send(m).is_ok() {
            true
        } else {
            self.dropped = self.dropped.saturating_add(1);
            false
        }
    }

    /// Markers dropped so far.
    pub fn dropped(&self) -> u64 {
        self.dropped
    }
}

impl MarkerReceiver {
    /// Calls `f` on every queued marker, oldest first. Never blocks, never allocates.
    pub fn drain(&mut self, mut f: impl FnMut(&ScheduledMarker)) -> u32 {
        let mut n = 0u32;
        while let Ok(m) = self.rx.try_recv() {
            f(&m);
            n = n.saturating_add(1);
        }
        n
    }
}

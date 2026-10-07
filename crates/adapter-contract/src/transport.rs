//! The transport trait (decision 0002). Every adapter's sessions run over a
//! `Transport`; `mantis-net` provides QUIC and TCP implementations.
//!
//! The interface is a synchronous polling facade: network I/O runs on the
//! transport's own threads, and the owner (a server network thread or a
//! client simulation thread) polls for events and sends without any async
//! runtime leaking into it.

use core::fmt;

/// Identity of one connection within a transport.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct ConnectionId(pub u64);

/// Delivery class of a frame.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum Channel {
    /// Reliable and ordered (session messages, commands).
    Reliable,
    /// Unreliable and sequenced, oldest dropped (inputs, snapshots).
    Unreliable,
}

/// Which transport an adapter's clients speak.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
#[non_exhaustive]
pub enum TransportKind {
    /// QUIC: reliable streams plus RFC 9221 datagrams.
    Quic,
    /// TCP: one reliable stream; unreliable frames are sent reliably.
    Tcp,
}

/// Why a connection ended.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
#[non_exhaustive]
pub enum DisconnectReason {
    /// The peer closed cleanly.
    Closed,
    /// The local side closed it.
    Local,
    /// No traffic within the timeout.
    TimedOut,
    /// The peer violated the framing or protocol.
    ProtocolViolation,
    /// An I/O error.
    Io,
}

/// Something happened on the transport.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum TransportEvent<'a> {
    /// A peer connected.
    Connected(ConnectionId),
    /// A whole frame arrived.
    Frame {
        /// From whom.
        conn: ConnectionId,
        /// On which channel.
        channel: Channel,
        /// The frame (valid only during the callback).
        bytes: &'a [u8],
    },
    /// A connection ended.
    Disconnected {
        /// Which.
        conn: ConnectionId,
        /// Why.
        reason: DisconnectReason,
    },
}

/// A send failed.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[non_exhaustive]
pub enum TransportError {
    /// No such connection (closed or never existed).
    UnknownConnection(ConnectionId),
    /// The frame exceeds what the channel can carry.
    TooLarge {
        /// The frame size.
        len: usize,
        /// The channel limit.
        max: usize,
    },
    /// The send queue is full; the frame was dropped.
    Backpressure,
    /// The transport has shut down.
    Closed,
}

impl fmt::Display for TransportError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnknownConnection(c) => write!(f, "unknown connection {}", c.0),
            Self::TooLarge { len, max } => write!(f, "frame of {len} bytes exceeds {max}"),
            Self::Backpressure => f.write_str("send queue full"),
            Self::Closed => f.write_str("transport closed"),
        }
    }
}

impl std::error::Error for TransportError {}

/// A message transport.
pub trait Transport: Send {
    /// Delivers every pending event to `sink`, in arrival order.
    fn poll(&mut self, sink: &mut dyn FnMut(TransportEvent<'_>));

    /// Queues `bytes` as one frame on `channel` to `conn`.
    ///
    /// # Errors
    /// [`TransportError`].
    fn send(&mut self, conn: ConnectionId, channel: Channel, bytes: &[u8]) -> Result<(), TransportError>;

    /// Closes `conn`.
    fn disconnect(&mut self, conn: ConnectionId);

    /// The kind of transport.
    fn kind(&self) -> TransportKind;

    /// The largest frame [`Channel::Unreliable`] carries.
    fn max_unreliable_payload(&self) -> usize;
}

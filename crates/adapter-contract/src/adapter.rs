//! The wire adapter trait: translation between a client's bytes and the
//! Mantis-native models.

use core::fmt;

use mantis_core::wire::{DecodeError, WireError};

use crate::snapshot::SnapshotFrame;
use crate::transport::TransportKind;
use crate::{Inbound, MovementMode, Outbound};

/// An adapter could not translate.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum AdapterError {
    /// The frame did not decode.
    Decode(DecodeError),
    /// A decoded message was refused.
    Wire(WireError),
    /// The client sent something this adapter's protocol does not allow now.
    Protocol(&'static str),
    /// The model has no representation in this protocol.
    Unsupported(&'static str),
    /// A value of this message cannot be carried by the protocol (an id
    /// beyond its range): the message is refused whole, never altered.
    Unrepresentable(&'static str),
}

impl From<DecodeError> for AdapterError {
    fn from(e: DecodeError) -> Self {
        Self::Decode(e)
    }
}

impl From<WireError> for AdapterError {
    fn from(e: WireError) -> Self {
        Self::Wire(e)
    }
}

impl fmt::Display for AdapterError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Decode(e) => write!(f, "adapter decode: {e}"),
            Self::Wire(e) => write!(f, "adapter: {e}"),
            Self::Protocol(what) => write!(f, "adapter protocol violation: {what}"),
            Self::Unsupported(what) => write!(f, "not representable in this protocol: {what}"),
            Self::Unrepresentable(what) => write!(f, "a value beyond this protocol's range: {what}"),
        }
    }
}

impl std::error::Error for AdapterError {}

/// A wire adapter: translates one client protocol to and from the
/// Mantis-native models. Implemented by packages; depends only on this crate.
///
/// Every `Inbound` an adapter produces is validated by the server with the
/// same [`crate::Validators`] as natively decoded messages, and dispatched
/// against the session's allowed-state list. An adapter is never trusted.
pub trait WireAdapter: Send + Sync + 'static {
    /// A stable name, for logs and configuration.
    fn name(&self) -> &'static str;

    /// Who owns positions for sessions on this adapter (decision 0011).
    /// Required: there is no default.
    fn movement_mode(&self) -> MovementMode;

    /// The transport this adapter's clients speak.
    fn transport(&self) -> TransportKind;

    /// Translates one received frame into zero or more inbound messages.
    ///
    /// # Errors
    /// [`AdapterError`]; the server counts it against the session.
    fn decode(&self, frame: &[u8], out: &mut dyn FnMut(Inbound)) -> Result<(), AdapterError>;

    /// Encodes one outbound session message.
    ///
    /// # Errors
    /// [`AdapterError::Unsupported`] when the protocol has no form for it.
    fn encode_outbound(&self, msg: &Outbound, out: &mut Vec<u8>) -> Result<(), AdapterError>;

    /// Encodes a snapshot. `baseline` is the client's last acknowledged
    /// snapshot, for delta encoding; adapters without deltas ignore it.
    ///
    /// # Errors
    /// [`AdapterError`].
    fn encode_snapshot(
        &self,
        frame: &SnapshotFrame,
        baseline: Option<&SnapshotFrame>,
        out: &mut Vec<u8>,
    ) -> Result<(), AdapterError>;
}

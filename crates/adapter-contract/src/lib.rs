//! mantis-adapter-contract: the only crate a wire adapter may depend on
//! (plan 9, decisions 0002, 0004, 0011).
//!
//! It holds the Mantis-native models every adapter translates to and from:
//!
//! - the **intent and session messages** ([`Inbound`], [`Outbound`]), generated
//!   from `schema/contract.idl` (decision 0017), with the hand-written
//!   validator trait [`Validators`] the server implements;
//! - the **snapshot model** ([`snapshot`]): what a client learns each tick;
//! - [`MovementMode`], a required property of every adapter;
//! - the [`WireAdapter`] and [`Transport`] traits. QUIC and TCP transports
//!   live in `mantis-net`, which depends on this crate, so a package adapter
//!   that brings its own transport pulls in neither.
//!
//! Nothing here exists only because one protocol needs it; gaps become
//! decisions, never patches (decision 0004).

#![forbid(unsafe_code)]

/// Code generated from `schema/` by `mantis-idl` (decision 0017).
pub mod generated {
    #[rustfmt::skip]
    pub mod contract;
}

pub mod adapter;
pub mod native;
pub mod snapshot;
pub mod transport;

pub use adapter::{AdapterError, WireAdapter};
pub use generated::contract::{
    AbilityId, AppearanceId, Cast, Choose, Extension, ExtensionKind, ExtensionMessage, ExtensionRefusal,
    ExtensionRefused, FeatureState, Goodbye, Hello, Inbound, Interact, ModTier, ModuleEntry, Move, MoveClaim,
    MovementMode, Outbound, PermittedModules, PromptId, Refuse, RefuseReason, SetPosition, SnapshotAck,
    Validators, Welcome, decode_inbound, decode_outbound, parse_inbound,
};
pub use snapshot::{LocalAvatar, RemoteSample, SnapshotFrame, SnapshotHeader, SnapshotVisitor};
pub use transport::{
    Channel, ConnectionId, DisconnectReason, Transport, TransportError, TransportEvent, TransportKind,
};

/// The protocol version this build speaks.
pub const PROTOCOL_VERSION: u16 = 1;

/// Re-exports of the core types the contract is expressed in, so an adapter
/// needs no direct dependency on `mantis-core`.
pub mod core_types {
    pub use mantis_core::content::ContentHash;
    pub use mantis_core::ecs::EntityId;
    pub use mantis_core::graph::{
        GraphId, GraphInstanceId, MarkerId, MarkerKind, NodeKey, PackageMarker, TimelineMarker,
    };
    pub use mantis_core::kinematics::{
        AimAngles, Angle16, InputSeq, MotionModifiers, MotionState, MoveButtons, MoveInput,
    };
    pub use mantis_core::math::Vec3;
    pub use mantis_core::mem::BoundedVec;
    pub use mantis_core::time::Tick;
    pub use mantis_core::wire::{
        BoundedArray, DecodeError, Decoder, Encoder, FuzzSample, Message, MessageId, ValidationError, Wire,
        WireError, WireString, decode_exact, encode_into,
    };
}

#[cfg(test)]
mod tests;

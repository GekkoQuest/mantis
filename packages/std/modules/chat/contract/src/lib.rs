//! std.chat contract. Extension messages are generated from
//! `schema/chat.idl` (their ids are the extension kinds).

#![forbid(unsafe_code)]

pub mod generated {
    //! Generated from `schema/chat.idl`.
    #[rustfmt::skip]
    pub mod chat;
}

pub use generated::chat::*;

/// The local channel: characters within [`LOCAL_RANGE`] metres.
pub const LOCAL: u8 = 0;
/// The party channel.
pub const PARTY: u8 = 1;
/// One character.
pub const WHISPER: u8 = 2;
/// The speaker's guild, wherever its members are.
pub const GUILD: u8 = 3;

/// Range of the local channel, in metres.
pub const LOCAL_RANGE: f32 = 40.0;

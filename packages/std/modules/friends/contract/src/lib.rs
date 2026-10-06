//! std.friends contract. Extension messages are generated from
//! `schema/friends.idl` (their ids are the extension kinds).

#![forbid(unsafe_code)]

pub mod generated {
    //! Generated from `schema/friends.idl`.
    #[rustfmt::skip]
    pub mod friends;
}

pub use generated::friends::*;

use mantis_core::hash::{StableHasher, StateHash};
use mantis_core::module::{Event, Query};

/// Friends one character may have.
pub const MAX_FRIENDS: usize = mantis_core::social::FRIENDS_MAX;

/// Two characters became, or stopped being, friends.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct FriendshipChanged {
    /// One character.
    pub a: u64,
    /// The other.
    pub b: u64,
    /// True when they are friends now.
    pub friends: bool,
}

impl StateHash for FriendshipChanged {
    fn state_hash(&self, h: &mut StableHasher) {
        h.write_u64(self.a);
        h.write_u64(self.b);
        h.write_u8(u8::from(self.friends));
    }
}

impl Event for FriendshipChanged {
    const NAME: &'static str = "std.friends.changed";

    fn save(&self, e: &mut mantis_core::wire::Encoder<'_>) -> bool {
        e.u64(self.a);
        e.u64(self.b);
        e.bool(self.friends);
        true
    }

    fn load(d: &mut mantis_core::wire::Decoder<'_>) -> Result<Self, mantis_core::wire::DecodeError> {
        Ok(Self {
            a: d.u64()?,
            b: d.u64()?,
            friends: d.bool()?,
        })
    }
}

/// Are these two characters friends?
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct AreFriends(pub u64, pub u64);

impl Query for AreFriends {
    type Response = bool;
    const NAME: &'static str = "std.friends.are_friends";
}

/// `FriendRefused.op`: a request.
pub const OP_REQUEST: u8 = mantis_core::social::friend_op::REQUEST;
/// `FriendRefused.op`: an answer.
pub const OP_RESPOND: u8 = mantis_core::social::friend_op::RESPOND;
/// `FriendRefused.op`: a removal.
pub const OP_REMOVE: u8 = mantis_core::social::friend_op::REMOVE;

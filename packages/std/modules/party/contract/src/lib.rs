//! std.party contract: what other modules and clients may know about
//! parties. Extension messages are generated from `schema/party.idl` (their
//! ids are the extension kinds); events and queries are declared here.

#![forbid(unsafe_code)]

pub mod generated {
    //! Generated from `schema/party.idl`.
    #[rustfmt::skip]
    pub mod party;
}

pub use generated::party::*;

use mantis_core::hash::{StableHasher, StateHash};
use mantis_core::module::{Event, Query};

/// Members a party may have.
pub const MAX_MEMBERS: usize = mantis_core::social::PARTY_MAX;

/// A character joined or left a party.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct PartyChanged {
    /// The party.
    pub party: u32,
    /// The character.
    pub character: u64,
    /// True when it joined, false when it left.
    pub joined: bool,
}

impl StateHash for PartyChanged {
    fn state_hash(&self, h: &mut StableHasher) {
        h.write_u32(self.party);
        h.write_u64(self.character);
        h.write_u8(u8::from(self.joined));
    }
}

impl Event for PartyChanged {
    const NAME: &'static str = "std.party.changed";

    fn save(&self, e: &mut mantis_core::wire::Encoder<'_>) -> bool {
        e.u32(self.party);
        e.u64(self.character);
        e.bool(self.joined);
        true
    }

    fn load(d: &mut mantis_core::wire::Decoder<'_>) -> Result<Self, mantis_core::wire::DecodeError> {
        Ok(Self {
            party: d.u32()?,
            character: d.u64()?,
            joined: d.bool()?,
        })
    }
}

/// A party as other modules see it.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct PartyView {
    /// Party id.
    pub id: u32,
    /// The leader's character.
    pub leader: u64,
    members: [u64; MAX_MEMBERS],
    len: usize,
}

impl PartyView {
    /// A view of a party with `members`, the leader first.
    #[must_use]
    pub fn new(id: u32, leader: u64, members: &[u64]) -> Self {
        let mut all = [0; MAX_MEMBERS];
        let len = members.len().min(MAX_MEMBERS);
        for (slot, m) in all.iter_mut().zip(members) {
            *slot = *m;
        }
        Self {
            id,
            leader,
            members: all,
            len,
        }
    }

    /// The members' characters, the leader first.
    #[must_use]
    pub fn members(&self) -> &[u64] {
        self.members.get(..self.len).unwrap_or(&[])
    }
}

/// Which party is `character` in?
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct PartyOf(pub u64);

impl Query for PartyOf {
    type Response = Option<PartyView>;
    const NAME: &'static str = "std.party.party_of";
}

/// `PartyRefused.op`: an invitation.
pub const OP_INVITE: u8 = mantis_core::social::party_op::INVITE;
/// `PartyRefused.op`: an accept.
pub const OP_ACCEPT: u8 = mantis_core::social::party_op::ACCEPT;
/// `PartyRefused.op`: leaving.
pub const OP_LEAVE: u8 = mantis_core::social::party_op::LEAVE;
/// `PartyRefused.op`: a kick.
pub const OP_KICK: u8 = mantis_core::social::party_op::KICK;

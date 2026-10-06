//! std.guild contract: what other modules and clients may know about
//! guilds. Extension messages are generated from `schema/guild.idl` (their
//! ids are the extension kinds); events and queries are declared here.

#![forbid(unsafe_code)]

pub mod generated {
    //! Generated from `schema/guild.idl`.
    #[rustfmt::skip]
    pub mod guild;
}

pub use generated::guild::*;

use mantis_core::hash::{StableHasher, StateHash};
use mantis_core::module::{Event, Query};

/// Members a guild may have.
pub const MAX_MEMBERS: usize = mantis_core::social::GUILD_MAX;
/// Members per `GuildRoster` page.
pub const ROSTER_PAGE: usize = mantis_core::social::ROSTER_CHUNK;

/// The leader rank (`SetGuildRank.rank`, `GuildJoined.rank`).
pub const RANK_LEADER: u8 = mantis_core::social::guild_rank::LEADER;
/// The officer rank: may invite and remove members.
pub const RANK_OFFICER: u8 = mantis_core::social::guild_rank::OFFICER;
/// The member rank.
pub const RANK_MEMBER: u8 = mantis_core::social::guild_rank::MEMBER;

/// `GuildRefused.op`: founding.
pub const OP_CREATE: u8 = mantis_core::social::guild_op::CREATE;
/// `GuildRefused.op`: an invitation.
pub const OP_INVITE: u8 = mantis_core::social::guild_op::INVITE;
/// `GuildRefused.op`: an accept.
pub const OP_ACCEPT: u8 = mantis_core::social::guild_op::ACCEPT;
/// `GuildRefused.op`: leaving.
pub const OP_LEAVE: u8 = mantis_core::social::guild_op::LEAVE;
/// `GuildRefused.op`: a removal.
pub const OP_REMOVE: u8 = mantis_core::social::guild_op::KICK;
/// `GuildRefused.op`: a rank change.
pub const OP_SET_RANK: u8 = mantis_core::social::guild_op::SET_RANK;
/// `GuildRefused.op`: disbanding.
pub const OP_DISBAND: u8 = mantis_core::social::guild_op::DISBAND;

/// True for a valid guild name (see `RULES.md`).
#[must_use]
pub fn name_ok(name: &str) -> bool {
    mantis_core::social::guild_name_ok(name)
}

/// A character joined or left a guild in this cell's projection.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct GuildChanged {
    /// The guild.
    pub guild: u32,
    /// The character.
    pub character: u64,
    /// True when it joined, false when it left.
    pub joined: bool,
}

impl StateHash for GuildChanged {
    fn state_hash(&self, h: &mut StableHasher) {
        h.write_u32(self.guild);
        h.write_u64(self.character);
        h.write_u8(u8::from(self.joined));
    }
}

impl Event for GuildChanged {
    const NAME: &'static str = "std.guild.changed";

    fn save(&self, e: &mut mantis_core::wire::Encoder<'_>) -> bool {
        e.u32(self.guild);
        e.u64(self.character);
        e.bool(self.joined);
        true
    }

    fn load(d: &mut mantis_core::wire::Decoder<'_>) -> Result<Self, mantis_core::wire::DecodeError> {
        Ok(Self {
            guild: d.u32()?,
            character: d.u64()?,
            joined: d.bool()?,
        })
    }
}

/// A character's guild as other modules see it.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct GuildView {
    /// The guild.
    pub id: u32,
    /// The character's rank.
    pub rank: u8,
}

/// Which guild is `character` in?
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct GuildOf(pub u64);

impl Query for GuildOf {
    type Response = Option<GuildView>;
    const NAME: &'static str = "std.guild.guild_of";
}

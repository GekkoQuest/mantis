//! std.guild, server half. See `RULES.md` beside the manifest.
//!
//! The social role owns every guild and keeps it durable (lead ruling, M8,
//! and M9): this module checks what a cell can check, relays the operation
//! through the service outbox, and keeps a read-only projection of the
//! guilds its characters are in, fed by logged service updates. A guild
//! therefore survives any transfer and any restart: no cell owns it, and
//! the social role writes it through the persistence writer before telling
//! anyone.

#![forbid(unsafe_code)]

use std::collections::BTreeMap;

use mantis_core::ecs::{Resource, World};
use mantis_core::hash::{StableHasher, StateHash};
use mantis_core::log::SessionId;
use mantis_core::module::Events;
use mantis_core::schedule::{Phase, SystemDesc, SystemError, TickContext};
use mantis_core::social::{GUILD_OP, GUILD_UPDATE, GuildOp, GuildUpdate};
use mantis_core::wire::{BoundedArray, Message, ValidationError, WireString, decode_exact};
use mantis_server::modules::{
    ExtensionKind, ExtensionRefusal, Registrar, RegistryError, Require, ServerModule, caller, decode,
    session_of, session_of_entity, tell_character,
};
use mantis_server::service::{ServiceOutbox, to_service_encoded};
use std_guild_contract::{
    AcceptGuild, CreateGuild, DisbandGuild, GuildChanged, GuildDisbanded, GuildInvited, GuildJoined,
    GuildMember, GuildMemberChanged, GuildMemberGone, GuildOf, GuildRefused, GuildRoster, GuildView,
    InviteToGuild, LeaveGuild, RANK_MEMBER, RemoveFromGuild, SetGuildRank,
};

/// How often, in seconds, guilds with no member left in the cell are
/// dropped from the projection.
pub const UPKEEP_SECONDS: u64 = 1;

/// One projected guild.
#[derive(Clone, PartialEq, Eq, Debug, Default)]
pub struct Guild {
    /// Its name.
    pub name: String,
    /// Members and ranks, as the social role last told them.
    pub members: BTreeMap<u64, u8>,
}

/// The guilds of characters in this cell, as the social role last told
/// them. Simulation state (fed only by logged updates).
#[derive(Debug, Default)]
pub struct Guilds {
    /// By guild id.
    pub guilds: BTreeMap<u32, Guild>,
}

impl Guilds {
    /// The guild `character` is in, and its rank.
    #[must_use]
    pub fn guild_of(&self, character: u64) -> Option<(u32, u8)> {
        self.guilds
            .iter()
            .find_map(|(id, g)| g.members.get(&character).map(|r| (*id, *r)))
    }
}

impl StateHash for Guilds {
    fn state_hash(&self, h: &mut StableHasher) {
        h.write_u64(self.guilds.len() as u64);
        for (id, g) in &self.guilds {
            h.write_u32(*id);
            h.write_u64(g.name.len() as u64);
            h.write(g.name.as_bytes());
            h.write_u64(g.members.len() as u64);
            for (m, r) in &g.members {
                h.write_u64(*m);
                h.write_u8(*r);
            }
        }
    }
}

impl Resource for Guilds {
    const NAME: &'static str = "std.guild.guilds";

    fn save(&self, e: &mut mantis_core::wire::Encoder<'_>) -> mantis_core::ecs::Saved {
        e.u32(u32::try_from(self.guilds.len()).unwrap_or(u32::MAX));
        for (id, g) in &self.guilds {
            e.u32(*id);
            let name = g.name.as_bytes();
            e.u8(u8::try_from(name.len()).unwrap_or(0));
            e.bytes(
                name.get(..usize::from(u8::try_from(name.len()).unwrap_or(0)))
                    .unwrap_or(&[]),
            );
            e.u32(u32::try_from(g.members.len()).unwrap_or(u32::MAX));
            for (m, r) in &g.members {
                e.u64(*m);
                e.u8(*r);
            }
        }
        mantis_core::ecs::Saved::Written
    }

    fn load(&mut self, d: &mut mantis_core::wire::Decoder<'_>) -> Result<(), mantis_core::wire::DecodeError> {
        self.guilds.clear();
        for _ in 0..d.u32()? {
            let id = d.u32()?;
            let n = usize::from(d.u8()?);
            let name = String::from_utf8(d.take(n)?.to_vec())
                .map_err(|_| mantis_core::wire::DecodeError::Invalid("guild name"))?;
            let mut members = BTreeMap::new();
            for _ in 0..d.u32()? {
                members.insert(d.u64()?, d.u8()?);
            }
            self.guilds.insert(id, Guild { name, members });
        }
        Ok(())
    }
}

/// Validators: every inbound guild message is checked before it is handled.
struct Checks;

impl std_guild_contract::Validators for Checks {
    fn validate_create_guild(&self, msg: &CreateGuild) -> Result<(), ValidationError> {
        if !std_guild_contract::name_ok(msg.name.as_str()) {
            return Err(ValidationError(
                "guild names are 3 to 24 letters, digits, spaces, or hyphens",
            ));
        }
        Ok(())
    }
    fn validate_invite_to_guild(&self, _msg: &InviteToGuild) -> Result<(), ValidationError> {
        Ok(())
    }
    fn validate_accept_guild(&self, msg: &AcceptGuild) -> Result<(), ValidationError> {
        if msg.guild == 0 {
            return Err(ValidationError("no guild"));
        }
        Ok(())
    }
    fn validate_leave_guild(&self, _msg: &LeaveGuild) -> Result<(), ValidationError> {
        Ok(())
    }
    fn validate_remove_from_guild(&self, msg: &RemoveFromGuild) -> Result<(), ValidationError> {
        if msg.character == 0 {
            return Err(ValidationError("no member"));
        }
        Ok(())
    }
    fn validate_set_guild_rank(&self, msg: &SetGuildRank) -> Result<(), ValidationError> {
        if msg.character == 0 || msg.rank > RANK_MEMBER {
            return Err(ValidationError("a member and a rank of 0 to 2"));
        }
        Ok(())
    }
    fn validate_disband_guild(&self, _msg: &DisbandGuild) -> Result<(), ValidationError> {
        Ok(())
    }
}

/// Queues an operation for the social role (an output: the answer comes
/// back as a logged update).
fn relay(world: &mut World, op: &GuildOp) {
    let _ = to_service_encoded(world, GUILD_OP, op);
}

fn create(
    world: &mut World,
    _: &TickContext,
    session: SessionId,
    payload: &[u8],
) -> Result<(), ExtensionRefusal> {
    let msg: CreateGuild = decode(payload)?;
    let me = caller(world, session)?;
    relay(
        world,
        &GuildOp::Create {
            me,
            name: msg.name.as_str().to_owned(),
        },
    );
    Ok(())
}

fn invite(
    world: &mut World,
    _: &TickContext,
    session: SessionId,
    payload: &[u8],
) -> Result<(), ExtensionRefusal> {
    let msg: InviteToGuild = decode(payload)?;
    let from = caller(world, session)?;
    let target_session = session_of_entity(world, msg.target).ok_or(ExtensionRefusal::NotAllowed)?;
    let to = caller(world, target_session)?;
    // Read when the invite is made, so a live change applies from the tick
    // it lands on.
    if !mantis_server::modules::flag(world, "std.guild.invites") || to == from {
        return Err(ExtensionRefusal::NotAllowed);
    }
    relay(world, &GuildOp::Invite { me: from, to });
    Ok(())
}

fn accept(
    world: &mut World,
    _: &TickContext,
    session: SessionId,
    payload: &[u8],
) -> Result<(), ExtensionRefusal> {
    let msg: AcceptGuild = decode(payload)?;
    let me = caller(world, session)?;
    relay(world, &GuildOp::Accept { me, guild: msg.guild });
    Ok(())
}

fn leave(
    world: &mut World,
    _: &TickContext,
    session: SessionId,
    payload: &[u8],
) -> Result<(), ExtensionRefusal> {
    let _: LeaveGuild = decode(payload)?;
    let me = caller(world, session)?;
    relay(world, &GuildOp::Leave { me });
    Ok(())
}

fn remove(
    world: &mut World,
    _: &TickContext,
    session: SessionId,
    payload: &[u8],
) -> Result<(), ExtensionRefusal> {
    let msg: RemoveFromGuild = decode(payload)?;
    let me = caller(world, session)?;
    if msg.character == me {
        return Err(ExtensionRefusal::NotAllowed);
    }
    relay(
        world,
        &GuildOp::Kick {
            me,
            who: msg.character,
        },
    );
    Ok(())
}

fn set_rank(
    world: &mut World,
    _: &TickContext,
    session: SessionId,
    payload: &[u8],
) -> Result<(), ExtensionRefusal> {
    let msg: SetGuildRank = decode(payload)?;
    let me = caller(world, session)?;
    if msg.character == me {
        return Err(ExtensionRefusal::NotAllowed);
    }
    relay(
        world,
        &GuildOp::SetRank {
            me,
            who: msg.character,
            rank: msg.rank,
        },
    );
    Ok(())
}

fn disband(
    world: &mut World,
    _: &TickContext,
    session: SessionId,
    payload: &[u8],
) -> Result<(), ExtensionRefusal> {
    let _: DisbandGuild = decode(payload)?;
    let me = caller(world, session)?;
    relay(world, &GuildOp::Disband { me });
    Ok(())
}

fn changed(world: &mut World, guild: u32, character: u64, joined: bool) {
    if let Some(q) = world.resource_mut::<Events<GuildChanged>>() {
        q.send(GuildChanged {
            guild,
            character,
            joined,
        });
    }
}

fn name(text: &str) -> WireString<24> {
    WireString::new(text).unwrap_or_default()
}

/// Leaves `who` out of `guild` in the projection; true when it was in.
fn drop_member(world: &mut World, guild: u32, who: u64) -> Result<bool, &'static str> {
    let guilds = world.resource_mut::<Guilds>().ok_or("guilds")?;
    Ok(guilds
        .guilds
        .get_mut(&guild)
        .is_some_and(|g| g.members.remove(&who).is_some()))
}

/// `to` is in `guild` at `rank`.
fn joined(world: &mut World, to: u64, guild: u32, text: &str, rank: u8) -> Result<(), &'static str> {
    let guilds = world.resource_mut::<Guilds>().ok_or("guilds")?;
    let g = guilds.guilds.entry(guild).or_default();
    text.clone_into(&mut g.name);
    if g.members.insert(to, rank).is_none() {
        changed(world, guild, to, true);
    }
    tell_character(
        world,
        to,
        &GuildJoined {
            guild,
            name: name(text),
            rank,
        },
    );
    Ok(())
}

/// A page of `guild`'s roster for `to`.
fn roster(
    world: &mut World,
    to: u64,
    guild: u32,
    first: bool,
    members: &[(u64, u8)],
) -> Result<(), &'static str> {
    let guilds = world.resource_mut::<Guilds>().ok_or("guilds")?;
    let g = guilds.guilds.entry(guild).or_default();
    if first {
        g.members.clear();
    }
    g.members.extend(members.iter().copied());
    let page: Vec<GuildMember> = members
        .iter()
        .map(|(character, rank)| GuildMember {
            character: *character,
            rank: *rank,
        })
        .collect();
    tell_character(
        world,
        to,
        &GuildRoster {
            guild,
            first,
            members: BoundedArray::from_slice(&page).unwrap_or_default(),
        },
    );
    Ok(())
}

/// A logged update from the social role: the projection follows it and the
/// character's client is told.
fn updated(world: &mut World, _: &TickContext, payload: &[u8]) -> Result<(), &'static str> {
    let u: GuildUpdate = decode_exact(payload).map_err(|_| "not a guild update")?;
    let to = u.to();
    if session_of(world, to).is_none() {
        return Ok(());
    }
    match u {
        GuildUpdate::Joined {
            guild,
            name: text,
            rank,
            ..
        } => joined(world, to, guild, &text, rank)?,
        GuildUpdate::Members {
            guild,
            first,
            members,
            ..
        } => roster(world, to, guild, first, &members)?,
        GuildUpdate::Member {
            guild, member, rank, ..
        } => {
            let guilds = world.resource_mut::<Guilds>().ok_or("guilds")?;
            if let Some(g) = guilds.guilds.get_mut(&guild) {
                g.members.insert(member, rank);
            }
            tell_character(
                world,
                to,
                &GuildMemberChanged {
                    guild,
                    character: member,
                    rank,
                },
            );
        }
        GuildUpdate::Gone { guild, member, .. } => {
            if drop_member(world, guild, member)? && member == to {
                changed(world, guild, to, false);
            }
            tell_character(
                world,
                to,
                &GuildMemberGone {
                    guild,
                    character: member,
                },
            );
        }
        GuildUpdate::Disbanded { guild, .. } => {
            if drop_member(world, guild, to)? {
                changed(world, guild, to, false);
            }
            tell_character(world, to, &GuildDisbanded { guild });
        }
        GuildUpdate::Invited {
            guild,
            name: text,
            from,
            ..
        } => {
            tell_character(
                world,
                to,
                &GuildInvited {
                    guild,
                    name: name(&text),
                    from,
                },
            );
        }
        GuildUpdate::Refused { op, .. } => {
            tell_character(world, to, &GuildRefused { op });
        }
    }
    Ok(())
}

/// Drops projected guilds with no member in this cell, once a second.
fn upkeep(world: &mut World, ctx: &TickContext) -> Result<(), SystemError> {
    let every = UPKEEP_SECONDS * u64::from(ctx.rate.hz());
    if !ctx.tick.0.is_multiple_of(every.max(1)) {
        return Ok(());
    }
    let local: Vec<u32> = {
        let g = world
            .resource::<Guilds>()
            .ok_or(SystemError::Invariant("guilds"))?;
        g.guilds
            .iter()
            .filter(|(_, guild)| guild.members.keys().any(|m| session_of(world, *m).is_some()))
            .map(|(id, _)| *id)
            .collect()
    };
    if let Some(g) = world.resource_mut::<Guilds>() {
        g.guilds.retain(|id, _| local.contains(id));
    }
    Ok(())
}

/// The module.
pub struct Module;

impl ServerModule for Module {
    fn key(&self) -> &'static str {
        "std.guild"
    }

    fn register(&self, r: &mut Registrar<'_>) -> Result<(), RegistryError> {
        r.resource(Guilds::default())?;
        r.event::<GuildChanged>(64)?;
        let validated: [(u16, mantis_server::modules::HandlerFn); 7] = [
            (CreateGuild::ID.0, |w, c, s, p| {
                check(CreateGuild::ID.0, p)?;
                create(w, c, s, p)
            }),
            (InviteToGuild::ID.0, |w, c, s, p| {
                check(InviteToGuild::ID.0, p)?;
                invite(w, c, s, p)
            }),
            (AcceptGuild::ID.0, |w, c, s, p| {
                check(AcceptGuild::ID.0, p)?;
                accept(w, c, s, p)
            }),
            (LeaveGuild::ID.0, |w, c, s, p| {
                check(LeaveGuild::ID.0, p)?;
                leave(w, c, s, p)
            }),
            (RemoveFromGuild::ID.0, |w, c, s, p| {
                check(RemoveFromGuild::ID.0, p)?;
                remove(w, c, s, p)
            }),
            (SetGuildRank::ID.0, |w, c, s, p| {
                check(SetGuildRank::ID.0, p)?;
                set_rank(w, c, s, p)
            }),
            (DisbandGuild::ID.0, |w, c, s, p| {
                check(DisbandGuild::ID.0, p)?;
                disband(w, c, s, p)
            }),
        ];
        for (kind, run) in validated {
            r.handler(ExtensionKind(kind), Require::Joined, run)?;
        }
        r.service(GUILD_UPDATE, updated)?;
        r.query::<GuildOf>(|w, q| {
            w.resource::<Guilds>()?
                .guild_of(q.0)
                .map(|(id, rank)| GuildView { id, rank })
        })?;
        let access = r
            .access()
            .write_resource::<Guilds>()
            .write_resource::<ServiceOutbox>()
            .build()?;
        r.system(
            SystemDesc {
                name: "std.guild.upkeep",
                phase: Phase::Timers,
                priority: 0,
                access,
            },
            upkeep,
        )?;
        Ok(())
    }
}

/// Decodes by kind and runs the generated validators: every inbound guild
/// message is checked before it is handled.
fn check(kind: u16, payload: &[u8]) -> Result<(), ExtensionRefusal> {
    std_guild_contract::parse_inbound(mantis_core::wire::MessageId(kind), payload)
        .and_then(|m| m.validate(&Checks))
        .map_err(|_| ExtensionRefusal::Invalid)
}

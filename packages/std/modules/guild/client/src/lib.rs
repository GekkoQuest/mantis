//! std.guild client: the guild view model (roster with ranks, invitations), the guild
//! screen, and the guild intents (plan 13).
//!
//! **View model** (bindable properties, all under `std.guild.`):
//!
//! | property | value |
//! |---|---|
//! | `in_guild` / `solo` | whether the player is in a guild, and its negation |
//! | `id` | the guild id (0 when none) |
//! | `name` | the guild's name |
//! | `rank` | your rank: `leader`, `officer`, or `member` (empty when none) |
//! | `is_leader` | whether you lead (shows the leader's controls) |
//! | `can_invite` | whether your rank may invite (leader or officer) |
//! | `size` | members on the roster |
//! | `members` | list, leader first, then officers, then members (each group in the server's order); each item has `character` (text), `rank` (text), `you` (flag), and the controls your rank allows on that member: `removable`, `can_promote` (to officer), `can_demote` (to member), `can_lead` (pass the lead) |
//! | `invited` | whether an invitation is open |
//! | `invite_name`, `invite_from` | the inviting guild's name and the inviting character (text) |
//! | `pending` | the request waiting for its answer, in words (empty when none) |
//! | `refusal` | why the last request was refused, in words (empty when none) |
//! | `open` | whether the guild screen is shown (toggled by `std.guild.toggle`) |
//!
//! The registry adds `std.guild.enabled`, `std.guild.unavailable`, and
//! `std.guild.flag.invites`; the screen binds `enabled=` to them, so a disabled module or
//! switched-off invitations show as unavailable rather than as dead buttons.
//!
//! **Messages**: `GuildJoined` sets the guild and your rank (on founding, joining, and
//! arriving in a cell); `GuildRoster` pages fill the roster (`first` starts a new one);
//! `GuildMemberChanged` adds a member or changes a rank (yours too: passing the lead
//! arrives as two changes); `GuildMemberGone` removes a member (you: you left or were
//! removed); `GuildDisbanded` clears the guild; `GuildInvited` opens the invitation;
//! `GuildRefused` names the refused operation.
//!
//! **Intents**: `std.guild.create` (payload: the name, as the input submits it),
//! `std.guild.invite` (payload: the target entity, as `EntityId::to_bits`),
//! `std.guild.accept`, `std.guild.decline` (local), `std.guild.leave`,
//! `std.guild.remove` (payload: the member's character), `std.guild.rank` (payload:
//! `<character> <rank>`, rank 0 leader, 1 officer, 2 member), `std.guild.disband`, and
//! `std.guild.dismiss` (local: clears the refusal).
//!
//! **Requests and their answers.** The guild contract carries no request numbers, and
//! each operation is its own extension kind, so the module answers a refusal by the
//! envelope's echo of the refused kind: a cell's `ExtensionRefused` (not allowed,
//! invalid) and the authority's `GuildRefused { op }` both clear `pending` and set
//! `refusal` to the operation and the reason ("Could not found the guild: not
//! allowed"). Any guild message that answers a request (joined, changed, gone,
//! disbanded) clears `pending`.
//!
//! The social role holds every rule (`RULES.md`): names, ranks, who may invite, remove,
//! and set ranks, and the member limit. This half only refuses what cannot be sent at
//! all: a name the contract's `name_ok` rejects, a malformed character or rank, an
//! acceptance with no invitation open, and guild requests while not in a guild.
//! Characters are shown by id until a name service exists.

#![forbid(unsafe_code)]

use mantis_client::input::device::KeyCode;
use mantis_client::modules::{
    ClientModule, ClientRegistrar, ClientRegistryError, ExtensionRefusal, ModuleContext, ModuleError, decode,
};
use mantis_core::ecs::EntityId;
use mantis_core::wire::{Message, WireString};
use mantis_ui::{ListItem, Value};
use std_guild_contract::{
    AcceptGuild, CreateGuild, DisbandGuild, GuildDisbanded, GuildInvited, GuildJoined, GuildMemberChanged,
    GuildMemberGone, GuildRefused, GuildRoster, InviteToGuild, LeaveGuild, MAX_MEMBERS, OP_ACCEPT, OP_CREATE,
    OP_DISBAND, OP_INVITE, OP_LEAVE, OP_REMOVE, OP_SET_RANK, RANK_LEADER, RANK_MEMBER, RANK_OFFICER,
    RemoveFromGuild, SetGuildRank, name_ok,
};

/// The module key.
pub const KEY: &str = "std.guild";

/// The guild screen: founding, the roster with rank controls, leaving and disbanding,
/// the open invitation, and the request's progress or refusal.
pub const GUILD_SCREEN: &str = r#"
panel id=std_guild_panel style=std_guild_panel visible="std.guild.open" enabled="std.guild.enabled" {
  text id=std_guild_unavailable text="Guild unavailable" visible="std.guild.unavailable"
  panel id=std_guild_none visible="std.guild.solo" {
    text id=std_guild_none_text text="Not in a guild"
    input id=std_guild_create placeholder="Found a guild: name" submit="std.guild.create" max_length=24
  }
  panel id=std_guild_info visible="std.guild.in_guild" {
    text id=std_guild_title template="{std.guild.name} ({std.guild.size} members)"
    text id=std_guild_rank template="Your rank: {std.guild.rank}"
    list id=std_guild_members bind="std.guild.members" height=fit {
      row gap=6 {
        text template="{item.character}"
        text template="{item.rank}"
        button text="Officer" intent="std.guild.rank" payload="{item.character} 1" visible="item.can_promote"
        button text="Member" intent="std.guild.rank" payload="{item.character} 2" visible="item.can_demote"
        button text="Lead" intent="std.guild.rank" payload="{item.character} 0" visible="item.can_lead"
        button text="Remove" intent="std.guild.remove" payload="{item.character}" visible="item.removable"
      }
    }
    row id=std_guild_actions gap=8 {
      button id=std_guild_leave text="Leave" intent="std.guild.leave"
      button id=std_guild_disband text="Disband" intent="std.guild.disband" visible="std.guild.is_leader"
    }
  }
  text id=std_guild_pending bind="std.guild.pending"
  text id=std_guild_refusal bind="std.guild.refusal"
  panel id=std_guild_invite visible="std.guild.invited" enabled="std.guild.flag.invites" {
    text id=std_guild_invite_text template="{std.guild.invite_from} invites you to {std.guild.invite_name}"
    row id=std_guild_invite_buttons gap=8 {
      button id=std_guild_accept text="Join" intent="std.guild.accept" disabled_text="Invitations off"
      button id=std_guild_decline text="Decline" intent="std.guild.decline"
    }
  }
}
"#;

/// Styles for the guild screen.
pub const THEME: &str = "
theme {
  style std_guild_panel { background = #1a1e26e0 radius = 6 padding = 8 gap = 4 width = 300 }
}
";

/// The client half.
#[derive(Clone, Copy, Debug, Default)]
pub struct Module;

/// The guild the player is in.
#[derive(Clone, Debug)]
struct Membership {
    id: u32,
    name: String,
    rank: u8,
    /// (character, rank), in the server's order; at most [`MAX_MEMBERS`].
    members: Vec<(u64, u8)>,
}

/// An open invitation.
#[derive(Clone, Debug)]
struct Invitation {
    guild: u32,
    name: String,
    from: u64,
}

/// View-model state.
#[derive(Clone, Debug)]
struct Guild {
    current: Option<Membership>,
    invitation: Option<Invitation>,
    open: bool,
}

impl Default for Guild {
    fn default() -> Self {
        Self {
            current: None,
            invitation: None,
            open: true,
        }
    }
}

fn state<'a>(ctx: &'a mut ModuleContext<'_>) -> Result<&'a mut Guild, ModuleError> {
    ctx.state::<Guild>().ok_or(ModuleError::Invalid)
}

fn rank_name(rank: u8) -> &'static str {
    match rank {
        RANK_LEADER => "leader",
        RANK_OFFICER => "officer",
        _ => "member",
    }
}

/// What a request does, for `pending` and `refusal`.
fn op_text(op: u8) -> (&'static str, &'static str) {
    match op {
        OP_CREATE => ("Founding the guild", "Could not found the guild"),
        OP_INVITE => ("Inviting", "Could not invite"),
        OP_ACCEPT => ("Joining the guild", "Could not join the guild"),
        OP_LEAVE => ("Leaving the guild", "Could not leave the guild"),
        OP_REMOVE => ("Removing the member", "Could not remove the member"),
        OP_SET_RANK => ("Changing the rank", "Could not change the rank"),
        OP_DISBAND => ("Disbanding the guild", "Could not disband the guild"),
        _ => ("Waiting", "The guild request was refused"),
    }
}

/// The operation a request kind carries (the echo a refusal names).
fn op_of_kind(kind: u16) -> Option<u8> {
    Some(match kind {
        k if k == CreateGuild::ID.0 => OP_CREATE,
        k if k == InviteToGuild::ID.0 => OP_INVITE,
        k if k == AcceptGuild::ID.0 => OP_ACCEPT,
        k if k == LeaveGuild::ID.0 => OP_LEAVE,
        k if k == RemoveFromGuild::ID.0 => OP_REMOVE,
        k if k == SetGuildRank::ID.0 => OP_SET_RANK,
        k if k == DisbandGuild::ID.0 => OP_DISBAND,
        _ => return None,
    })
}

/// The player's character, from the session's Welcome (`client.character`).
fn me(ctx: &mut ModuleContext<'_>) -> Option<u64> {
    let props = ctx.props();
    let id = props.id("client.character")?;
    match props.get(id)? {
        Value::Text(t) => t.parse().ok(),
        _ => None,
    }
}

fn count(n: usize) -> i64 {
    i64::try_from(n).unwrap_or(i64::MAX)
}

/// One roster item, with the controls `mine` (your rank) allows on it.
fn member_item(character: u64, rank: u8, you: bool, mine: u8) -> ListItem {
    let leading = mine == RANK_LEADER && !you;
    ListItem::new()
        .with("character", Value::Text(character.to_string()))
        .with("rank", Value::text(rank_name(rank)))
        .with("you", Value::Bool(you))
        .with(
            "removable",
            Value::Bool(!you && mine <= RANK_OFFICER && mine < rank),
        )
        .with("can_promote", Value::Bool(leading && rank == RANK_MEMBER))
        .with("can_demote", Value::Bool(leading && rank == RANK_OFFICER))
        .with("can_lead", Value::Bool(leading))
}

fn publish_guild(ctx: &mut ModuleContext<'_>) -> Result<(), ModuleError> {
    let me = me(ctx);
    let current = state(ctx)?.current.clone();
    let Some(g) = current else {
        ctx.set_bool("in_guild", false);
        ctx.set_bool("solo", true);
        ctx.set_int("id", 0);
        ctx.set_text("name", "");
        ctx.set_text("rank", "");
        ctx.set_bool("is_leader", false);
        ctx.set_bool("can_invite", false);
        ctx.set_int("size", 0);
        ctx.set("members", Value::List(Vec::new()));
        return Ok(());
    };
    let mut ordered = g.members.clone();
    ordered.sort_by_key(|(_, rank)| *rank);
    let items = ordered
        .iter()
        .map(|(c, r)| member_item(*c, *r, Some(*c) == me, g.rank))
        .collect();
    ctx.set_bool("in_guild", true);
    ctx.set_bool("solo", false);
    ctx.set_int("id", i64::from(g.id));
    ctx.set_text("name", &g.name);
    ctx.set_text("rank", rank_name(g.rank));
    ctx.set_bool("is_leader", g.rank == RANK_LEADER);
    ctx.set_bool("can_invite", g.rank <= RANK_OFFICER);
    ctx.set_int("size", count(g.members.len()));
    ctx.set("members", Value::List(items));
    Ok(())
}

fn publish_invitation(ctx: &mut ModuleContext<'_>) -> Result<(), ModuleError> {
    let invitation = state(ctx)?.invitation.clone();
    ctx.set_bool("invited", invitation.is_some());
    ctx.set_text("invite_name", invitation.as_ref().map_or("", |i| i.name.as_str()));
    ctx.set_text(
        "invite_from",
        &invitation.map(|i| i.from.to_string()).unwrap_or_default(),
    );
    Ok(())
}

/// A request was answered: nothing is pending.
fn answered(ctx: &mut ModuleContext<'_>) {
    ctx.set_text("pending", "");
}

fn refused(ctx: &mut ModuleContext<'_>, op: u8, reason: &str) {
    answered(ctx);
    let (_, what) = op_text(op);
    ctx.set_text("refusal", &format!("{what}: {reason}"));
}

/// Sends `msg` for operation `op` and shows it as pending.
fn request<M: Message>(ctx: &mut ModuleContext<'_>, op: u8, msg: &M) -> Result<(), ModuleError> {
    ctx.send(msg)?;
    ctx.set_text("refusal", "");
    ctx.set_text("pending", &format!("{}...", op_text(op).0));
    Ok(())
}

/// Applies a message about guild `guild` to the current membership, if it is that
/// guild's; returns whether it was.
fn with_current(
    ctx: &mut ModuleContext<'_>,
    guild: u32,
    f: impl FnOnce(&mut Membership),
) -> Result<bool, ModuleError> {
    match state(ctx)?.current.as_mut() {
        Some(g) if g.id == guild => {
            f(g);
            Ok(true)
        }
        _ => Ok(false),
    }
}

fn on_joined(ctx: &mut ModuleContext<'_>, _kind: u16, payload: &[u8]) -> Result<(), ModuleError> {
    let m: GuildJoined = decode(payload)?;
    let g = state(ctx)?;
    let keep = g
        .current
        .as_ref()
        .filter(|c| c.id == m.guild)
        .map(|c| c.members.clone());
    g.current = Some(Membership {
        id: m.guild,
        name: m.name.as_str().to_owned(),
        rank: m.rank,
        members: keep.unwrap_or_default(),
    });
    if g.invitation.as_ref().is_some_and(|i| i.guild == m.guild) {
        g.invitation = None;
    }
    answered(ctx);
    publish_invitation(ctx)?;
    publish_guild(ctx)
}

fn on_roster(ctx: &mut ModuleContext<'_>, _kind: u16, payload: &[u8]) -> Result<(), ModuleError> {
    let m: GuildRoster = decode(payload)?;
    let applied = with_current(ctx, m.guild, |g| {
        if m.first {
            g.members.clear();
        }
        for member in m.members.iter() {
            if let Some(slot) = g.members.iter_mut().find(|(c, _)| *c == member.character) {
                slot.1 = member.rank;
            } else if g.members.len() < MAX_MEMBERS {
                g.members.push((member.character, member.rank));
            }
        }
    })?;
    if applied { publish_guild(ctx) } else { Ok(()) }
}

fn on_member_changed(ctx: &mut ModuleContext<'_>, _kind: u16, payload: &[u8]) -> Result<(), ModuleError> {
    let m: GuildMemberChanged = decode(payload)?;
    let me = me(ctx);
    let applied = with_current(ctx, m.guild, |g| {
        if let Some(slot) = g.members.iter_mut().find(|(c, _)| *c == m.character) {
            slot.1 = m.rank;
        } else if g.members.len() < MAX_MEMBERS {
            g.members.push((m.character, m.rank));
        }
        if Some(m.character) == me {
            g.rank = m.rank;
        }
    })?;
    if !applied {
        return Ok(());
    }
    answered(ctx);
    publish_guild(ctx)
}

fn on_member_gone(ctx: &mut ModuleContext<'_>, _kind: u16, payload: &[u8]) -> Result<(), ModuleError> {
    let m: GuildMemberGone = decode(payload)?;
    let me = me(ctx);
    let applied = with_current(ctx, m.guild, |g| g.members.retain(|(c, _)| *c != m.character))?;
    if !applied {
        return Ok(());
    }
    if Some(m.character) == me {
        state(ctx)?.current = None;
    }
    answered(ctx);
    publish_guild(ctx)
}

fn on_disbanded(ctx: &mut ModuleContext<'_>, _kind: u16, payload: &[u8]) -> Result<(), ModuleError> {
    let m: GuildDisbanded = decode(payload)?;
    let g = state(ctx)?;
    if g.current.as_ref().is_some_and(|c| c.id == m.guild) {
        g.current = None;
        answered(ctx);
    }
    if state(ctx)?
        .invitation
        .as_ref()
        .is_some_and(|i| i.guild == m.guild)
    {
        state(ctx)?.invitation = None;
        publish_invitation(ctx)?;
    }
    publish_guild(ctx)
}

fn on_invited(ctx: &mut ModuleContext<'_>, _kind: u16, payload: &[u8]) -> Result<(), ModuleError> {
    let m: GuildInvited = decode(payload)?;
    state(ctx)?.invitation = Some(Invitation {
        guild: m.guild,
        name: m.name.as_str().to_owned(),
        from: m.from,
    });
    publish_invitation(ctx)
}

fn on_guild_refused(ctx: &mut ModuleContext<'_>, _kind: u16, payload: &[u8]) -> Result<(), ModuleError> {
    let m: GuildRefused = decode(payload)?;
    refused(ctx, m.op, "refused");
    Ok(())
}

/// A cell refused one of the guild requests: the echoed kind names the operation.
/// `FeatureDisabled` is left to the registry (the module greys out).
fn on_refused(ctx: &mut ModuleContext<'_>, kind: u16, _request: u32, reason: ExtensionRefusal) -> bool {
    let Some(op) = op_of_kind(kind) else {
        return false;
    };
    let why = match reason {
        ExtensionRefusal::NotAllowed => "not allowed",
        ExtensionRefusal::Invalid => "invalid",
        ExtensionRefusal::FeatureDisabled => return false,
        _ => "refused",
    };
    refused(ctx, op, why);
    true
}

fn parse_character(text: Option<&str>) -> Result<u64, ModuleError> {
    text.and_then(|p| p.trim().parse::<u64>().ok())
        .filter(|c| *c != 0)
        .ok_or(ModuleError::Invalid)
}

fn in_guild(ctx: &mut ModuleContext<'_>) -> Result<(), ModuleError> {
    if state(ctx)?.current.is_some() {
        Ok(())
    } else {
        Err(ModuleError::Invalid)
    }
}

fn on_create(ctx: &mut ModuleContext<'_>, payload: Option<&str>) -> Result<(), ModuleError> {
    let name = payload
        .map(str::trim)
        .filter(|n| name_ok(n))
        .ok_or(ModuleError::Invalid)?;
    if state(ctx)?.current.is_some() {
        return Err(ModuleError::Invalid);
    }
    let name = WireString::new(name).ok_or(ModuleError::Invalid)?;
    request(ctx, OP_CREATE, &CreateGuild { name })
}

fn on_invite(ctx: &mut ModuleContext<'_>, payload: Option<&str>) -> Result<(), ModuleError> {
    if !ctx.flag("invites") {
        return Err(ModuleError::Invalid);
    }
    in_guild(ctx)?;
    let bits: u64 = payload
        .and_then(|p| p.trim().parse().ok())
        .ok_or(ModuleError::Invalid)?;
    request(
        ctx,
        OP_INVITE,
        &InviteToGuild {
            target: EntityId::from_bits(bits),
        },
    )
}

fn on_accept(ctx: &mut ModuleContext<'_>, _payload: Option<&str>) -> Result<(), ModuleError> {
    if !ctx.flag("invites") {
        return Err(ModuleError::Invalid);
    }
    let guild = state(ctx)?
        .invitation
        .as_ref()
        .map(|i| i.guild)
        .ok_or(ModuleError::Invalid)?;
    request(ctx, OP_ACCEPT, &AcceptGuild { guild })?;
    state(ctx)?.invitation = None;
    publish_invitation(ctx)
}

fn on_decline(ctx: &mut ModuleContext<'_>, _payload: Option<&str>) -> Result<(), ModuleError> {
    state(ctx)?.invitation = None;
    publish_invitation(ctx)
}

fn on_leave(ctx: &mut ModuleContext<'_>, _payload: Option<&str>) -> Result<(), ModuleError> {
    in_guild(ctx)?;
    request(ctx, OP_LEAVE, &LeaveGuild {})
}

fn on_remove(ctx: &mut ModuleContext<'_>, payload: Option<&str>) -> Result<(), ModuleError> {
    in_guild(ctx)?;
    let character = parse_character(payload)?;
    request(ctx, OP_REMOVE, &RemoveFromGuild { character })
}

fn on_rank(ctx: &mut ModuleContext<'_>, payload: Option<&str>) -> Result<(), ModuleError> {
    in_guild(ctx)?;
    let mut parts = payload.unwrap_or("").split_whitespace();
    let character = parse_character(parts.next())?;
    let rank: u8 = parts
        .next()
        .and_then(|r| r.parse().ok())
        .filter(|r| *r <= RANK_MEMBER)
        .ok_or(ModuleError::Invalid)?;
    if parts.next().is_some() {
        return Err(ModuleError::Invalid);
    }
    request(ctx, OP_SET_RANK, &SetGuildRank { character, rank })
}

fn on_disband(ctx: &mut ModuleContext<'_>, _payload: Option<&str>) -> Result<(), ModuleError> {
    in_guild(ctx)?;
    request(ctx, OP_DISBAND, &DisbandGuild {})
}

fn on_dismiss(ctx: &mut ModuleContext<'_>, _payload: Option<&str>) -> Result<(), ModuleError> {
    state(ctx)?;
    ctx.set_text("refusal", "");
    Ok(())
}

fn on_toggle(ctx: &mut ModuleContext<'_>) {
    let open = match state(ctx) {
        Ok(g) => {
            g.open = !g.open;
            g.open
        }
        Err(_) => false,
    };
    ctx.set_bool("open", open);
}

/// Publishes the starting view model, and resets it when the module is disabled. The
/// server sends the guild again whenever the player arrives in a cell.
fn on_enabled(ctx: &mut ModuleContext<'_>, enabled: bool) {
    let open = match state(ctx) {
        Ok(g) => {
            if !enabled {
                g.current = None;
                g.invitation = None;
            }
            g.open
        }
        Err(_) => false,
    };
    ctx.set_bool("open", open);
    ctx.set_text("pending", "");
    ctx.set_text("refusal", "");
    let _ = publish_guild(ctx);
    let _ = publish_invitation(ctx);
}

impl ClientModule for Module {
    fn key(&self) -> &'static str {
        KEY
    }

    fn register(&self, r: &mut ClientRegistrar<'_>) -> Result<(), ClientRegistryError> {
        r.kinds(1080..=1099)?;
        r.state(Guild::default());
        r.on_message(GuildJoined::ID.0, on_joined)?;
        r.on_message(GuildRoster::ID.0, on_roster)?;
        r.on_message(GuildMemberChanged::ID.0, on_member_changed)?;
        r.on_message(GuildMemberGone::ID.0, on_member_gone)?;
        r.on_message(GuildDisbanded::ID.0, on_disbanded)?;
        r.on_message(GuildInvited::ID.0, on_invited)?;
        r.on_message(GuildRefused::ID.0, on_guild_refused)?;
        r.on_intent("std.guild.create", on_create)?;
        r.on_intent("std.guild.invite", on_invite)?;
        r.on_intent("std.guild.accept", on_accept)?;
        r.on_intent("std.guild.decline", on_decline)?;
        r.on_intent("std.guild.leave", on_leave)?;
        r.on_intent("std.guild.remove", on_remove)?;
        r.on_intent("std.guild.rank", on_rank)?;
        r.on_intent("std.guild.disband", on_disband)?;
        r.on_intent("std.guild.dismiss", on_dismiss)?;
        r.on_refused(on_refused);
        r.on_enabled(on_enabled);
        r.screen("std.guild.roster", GUILD_SCREEN, Some(THEME))?;
        r.action("std.guild.toggle", Some(KeyCode::G), on_toggle)?;
        Ok(())
    }
}

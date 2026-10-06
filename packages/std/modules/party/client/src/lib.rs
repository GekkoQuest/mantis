//! std.party client: the party view model, roster and invitation screens, and the
//! party intents (plan 13).
//!
//! **View model** (bindable properties, all under `std.party.`):
//!
//! | property | value |
//! |---|---|
//! | `in_party` | whether the player is in a party |
//! | `id` | party id |
//! | `leader` | the leader's character, as text |
//! | `size` | member count |
//! | `members` | list; each item has `character` (text) and `leader` (flag) |
//! | `invited` | whether an invitation is open |
//! | `invite_from` | the inviting character, as text |
//! | `open` | whether the roster screen is shown (toggled by `std.party.toggle`) |
//!
//! The registry adds `std.party.enabled`, `std.party.flag.invites`, and
//! `std.party.refusal`; the screens bind `enabled=` to them, so a disabled module or
//! switched-off invitations show as unavailable rather than as dead buttons.
//!
//! **Intents**: `std.party.accept`, `std.party.decline` (local), `std.party.leave`,
//! `std.party.kick` (payload: the member's character), `std.party.invite` (payload: the
//! target entity, as `EntityId::to_bits`).
//!
//! The server holds every rule (`RULES.md`); this half only shows state and sends
//! requests. Characters are shown by id until a name service exists.

#![forbid(unsafe_code)]

use mantis_client::input::device::KeyCode;
use mantis_client::modules::{
    ClientModule, ClientRegistrar, ClientRegistryError, ModuleContext, ModuleError, decode,
};
use mantis_core::ecs::EntityId;
use mantis_core::wire::Message;
use mantis_ui::{ListItem, Value};
use std_party_contract::{Accept, Disbanded, Invite, Invited, Kick, Leave, Roster};

/// The module key.
pub const KEY: &str = "std.party";

/// The roster screen: members, a leave button, and the open invitation.
pub const ROSTER_SCREEN: &str = r#"
panel id=std_party_panel style=std_party_panel visible="std.party.open" enabled="std.party.enabled" {
  text id=std_party_title text="Party"
  text id=std_party_unavailable text="Party unavailable" visible="std.party.unavailable"
  list id=std_party_members bind="std.party.members" height=fit {
    row {
      text template="{item.character}"
      button text="Remove" intent="std.party.kick" payload="{item.character}" disabled_text="Remove"
    }
  }
  text id=std_party_refusal bind="std.party.refusal"
  button id=std_party_leave text="Leave" intent="std.party.leave" visible="std.party.in_party"
  panel id=std_party_invite visible="std.party.invited" enabled="std.party.flag.invites" {
    text id=std_party_invite_text template="Invitation from {std.party.invite_from}"
    row id=std_party_invite_buttons gap=8 {
      button id=std_party_accept text="Accept" intent="std.party.accept" disabled_text="Invitations off"
      button id=std_party_decline text="Decline" intent="std.party.decline"
    }
  }
}
"#;

/// Styles for the party screens.
pub const THEME: &str = "
theme {
  style std_party_panel { background = #1a1e26e0 radius = 6 padding = 8 gap = 4 width = 240 }
}
";

/// The client half.
#[derive(Clone, Copy, Debug, Default)]
pub struct Module;

/// View-model state.
#[derive(Clone, Debug, Default)]
struct Party {
    invite_from: Option<u64>,
    open: bool,
}

fn state<'a>(ctx: &'a mut ModuleContext<'_>) -> Result<&'a mut Party, ModuleError> {
    ctx.state::<Party>().ok_or(ModuleError::Invalid)
}

fn on_invited(ctx: &mut ModuleContext<'_>, _kind: u16, payload: &[u8]) -> Result<(), ModuleError> {
    let m: Invited = decode(payload)?;
    state(ctx)?.invite_from = Some(m.from);
    ctx.set_bool("invited", true);
    ctx.set_text("invite_from", &m.from.to_string());
    Ok(())
}

fn on_roster(ctx: &mut ModuleContext<'_>, _kind: u16, payload: &[u8]) -> Result<(), ModuleError> {
    let m: Roster = decode(payload)?;
    let members: Vec<ListItem> = m
        .members
        .iter()
        .map(|c| {
            ListItem::new()
                .with("character", Value::Text(c.to_string()))
                .with("leader", Value::Bool(*c == m.leader))
        })
        .collect();
    ctx.set_bool("in_party", true);
    ctx.set_int("id", i64::from(m.party));
    ctx.set_text("leader", &m.leader.to_string());
    ctx.set_int("size", i64::try_from(members.len()).unwrap_or(0));
    ctx.set("members", Value::List(members));
    Ok(())
}

fn clear_roster(ctx: &mut ModuleContext<'_>) {
    ctx.set_bool("in_party", false);
    ctx.set_int("id", 0);
    ctx.set_text("leader", "");
    ctx.set_int("size", 0);
    ctx.set("members", Value::List(Vec::new()));
}

fn on_disbanded(ctx: &mut ModuleContext<'_>, _kind: u16, payload: &[u8]) -> Result<(), ModuleError> {
    let _: Disbanded = decode(payload)?;
    clear_roster(ctx);
    Ok(())
}

fn clear_invite(ctx: &mut ModuleContext<'_>) -> Result<(), ModuleError> {
    state(ctx)?.invite_from = None;
    ctx.set_bool("invited", false);
    ctx.set_text("invite_from", "");
    Ok(())
}

fn on_accept(ctx: &mut ModuleContext<'_>, _payload: Option<&str>) -> Result<(), ModuleError> {
    if !ctx.flag("invites") {
        return Err(ModuleError::Invalid);
    }
    let from = state(ctx)?.invite_from.ok_or(ModuleError::Invalid)?;
    ctx.send(&Accept { from })?;
    clear_invite(ctx)
}

fn on_decline(ctx: &mut ModuleContext<'_>, _payload: Option<&str>) -> Result<(), ModuleError> {
    clear_invite(ctx)
}

fn on_leave(ctx: &mut ModuleContext<'_>, _payload: Option<&str>) -> Result<(), ModuleError> {
    ctx.send(&Leave {})
}

fn on_kick(ctx: &mut ModuleContext<'_>, payload: Option<&str>) -> Result<(), ModuleError> {
    let character = payload
        .and_then(|p| p.trim().parse().ok())
        .ok_or(ModuleError::Invalid)?;
    ctx.send(&Kick { character })
}

fn on_invite(ctx: &mut ModuleContext<'_>, payload: Option<&str>) -> Result<(), ModuleError> {
    if !ctx.flag("invites") {
        return Err(ModuleError::Invalid);
    }
    let bits: u64 = payload
        .and_then(|p| p.trim().parse().ok())
        .ok_or(ModuleError::Invalid)?;
    ctx.send(&Invite {
        target: EntityId::from_bits(bits),
    })
}

fn on_toggle(ctx: &mut ModuleContext<'_>) {
    let open = match state(ctx) {
        Ok(p) => {
            p.open = !p.open;
            p.open
        }
        Err(_) => false,
    };
    ctx.set_bool("open", open);
}

fn on_enabled(ctx: &mut ModuleContext<'_>, enabled: bool) {
    ctx.set_bool("unavailable", !enabled);
    if !enabled {
        let _ = clear_invite(ctx);
        clear_roster(ctx);
    }
}

impl ClientModule for Module {
    fn key(&self) -> &'static str {
        KEY
    }

    fn register(&self, r: &mut ClientRegistrar<'_>) -> Result<(), ClientRegistryError> {
        r.kinds(1000..=1009)?;
        r.state(Party {
            invite_from: None,
            open: true,
        });
        r.on_message(Invited::ID.0, on_invited)?;
        r.on_message(Roster::ID.0, on_roster)?;
        r.on_message(Disbanded::ID.0, on_disbanded)?;
        r.on_intent("std.party.accept", on_accept)?;
        r.on_intent("std.party.decline", on_decline)?;
        r.on_intent("std.party.leave", on_leave)?;
        r.on_intent("std.party.kick", on_kick)?;
        r.on_intent("std.party.invite", on_invite)?;
        r.on_enabled(on_enabled);
        r.screen("std.party.roster", ROSTER_SCREEN, Some(THEME))?;
        r.action("std.party.toggle", Some(KeyCode::P), on_toggle)?;
        Ok(())
    }
}

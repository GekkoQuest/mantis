//! std.friends client: the friend-list view model, the friends screen with its
//! request prompt, and the friends intents (plan 13).
//!
//! **View model** (bindable properties, all under `std.friends.`):
//!
//! | property | value |
//! |---|---|
//! | `friends` | list, as the server orders it; each item has `character` (text), `present` (flag: in this cell), and `presence` (`here` or `away`) |
//! | `count` | friends on the list |
//! | `here` | friends present in this cell |
//! | `incoming` | list of requests waiting for you, as the server orders them; each item has `character` (text) |
//! | `outgoing` | list of requests you made that wait for an answer; each item has `character` (text) |
//! | `pending` | requests waiting for you |
//! | `asked` | requests you made that wait for an answer |
//! | `requested` | whether a request waits for you (the prompt is shown) |
//! | `request_from` | the first waiting request's character, as text (empty when none) |
//! | `declined` | whether a "request declined" notice is shown |
//! | `declined_by` | the character who declined your request, as text |
//! | `open` | whether the friends screen is shown (toggled by `std.friends.toggle`) |
//!
//! The registry adds `std.friends.enabled`, `std.friends.unavailable`, and
//! `std.friends.refusal`; the screen binds `enabled=` to them, so a disabled module
//! shows as unavailable rather than as dead buttons.
//!
//! **Messages**: `FriendList` replaces the list; `Pending` replaces both request lists
//! (it is the source of truth, sent after every request and answer); `Requested` shows
//! the prompt at once, before its `Pending` arrives; `Declined` shows a notice.
//!
//! **Intents**: `std.friends.add` (payload: the character to ask, as the input submits
//! it), `std.friends.remove` (payload: the friend's character), `std.friends.accept` and
//! `std.friends.decline` (payload: the asking character; without one, the first
//! waiting request), `std.friends.show` (asks the server for the list and the requests
//! again), and `std.friends.dismiss` (local: hides the declined notice).
//!
//! When the module starts (or is enabled again) it sends `ShowFriends`, so the list and
//! the open requests survive a reconnect.
//!
//! The server holds every rule (`RULES.md`): who may be asked, list and request limits,
//! and mutual friendship. This half only refuses what cannot be sent at all (no
//! character, or an answer to a request that is not waiting). Characters are shown by
//! id until a name service exists.

#![forbid(unsafe_code)]

use mantis_client::input::device::KeyCode;
use mantis_client::modules::{
    ClientModule, ClientRegistrar, ClientRegistryError, ModuleContext, ModuleError, decode,
};
use mantis_core::wire::Message;
use mantis_ui::{ListItem, Value};
use std_friends_contract::{Declined, FriendList, Pending, Remove, Request, Requested, Respond, ShowFriends};

/// The module key.
pub const KEY: &str = "std.friends";

/// Requests each way the view model keeps (the contract's `Pending` capacity).
pub const PROMPTS: usize = 20;

/// The friends screen: the list, the add input, the request prompt, and the declined
/// notice.
pub const FRIENDS_SCREEN: &str = r#"
panel id=std_friends_panel style=std_friends_panel visible="std.friends.open" enabled="std.friends.enabled" {
  text id=std_friends_title template="Friends ({std.friends.here} / {std.friends.count} here)"
  text id=std_friends_unavailable text="Friends unavailable" visible="std.friends.unavailable"
  list id=std_friends_list bind="std.friends.friends" height=fit {
    row gap=8 {
      text template="{item.character}"
      text template="{item.presence}"
      button text="Remove" intent="std.friends.remove" payload="{item.character}" disabled_text="Remove"
    }
  }
  text id=std_friends_refusal bind="std.friends.refusal"
  input id=std_friends_add placeholder="Add a character" submit="std.friends.add"
  text id=std_friends_asked template="Asked: {std.friends.asked}"
  list id=std_friends_outgoing bind="std.friends.outgoing" height=fit {
    text template="Waiting for {item.character}"
  }
  panel id=std_friends_request visible="std.friends.requested" {
    text id=std_friends_request_text template="Friend request from {std.friends.request_from}"
    row id=std_friends_request_buttons gap=8 {
      button id=std_friends_accept text="Accept" intent="std.friends.accept" payload="{std.friends.request_from}"
      button id=std_friends_decline text="Decline" intent="std.friends.decline" payload="{std.friends.request_from}"
    }
  }
  row id=std_friends_declined visible="std.friends.declined" gap=8 {
    text id=std_friends_declined_text template="{std.friends.declined_by} declined your request"
    button id=std_friends_dismiss text="OK" intent="std.friends.dismiss"
  }
}
"#;

/// Styles for the friends screen.
pub const THEME: &str = "
theme {
  style std_friends_panel { background = #1a1e26e0 radius = 6 padding = 8 gap = 4 width = 260 }
}
";

/// The client half.
#[derive(Clone, Copy, Debug, Default)]
pub struct Module;

/// View-model state.
#[derive(Clone, Debug)]
struct Friends {
    /// Requests waiting for this character; at most [`PROMPTS`].
    incoming: Vec<u64>,
    /// Requests this character made; at most [`PROMPTS`].
    outgoing: Vec<u64>,
    open: bool,
}

impl Default for Friends {
    fn default() -> Self {
        Self {
            incoming: Vec::with_capacity(PROMPTS),
            outgoing: Vec::with_capacity(PROMPTS),
            open: true,
        }
    }
}

fn state<'a>(ctx: &'a mut ModuleContext<'_>) -> Result<&'a mut Friends, ModuleError> {
    ctx.state::<Friends>().ok_or(ModuleError::Invalid)
}

fn parse_character(payload: Option<&str>) -> Option<u64> {
    payload
        .and_then(|p| p.trim().parse::<u64>().ok())
        .filter(|c| *c != 0)
}

fn count(n: usize) -> i64 {
    i64::try_from(n).unwrap_or(i64::MAX)
}

fn characters(list: &[u64]) -> Value {
    Value::List(
        list.iter()
            .map(|c| ListItem::new().with("character", Value::Text(c.to_string())))
            .collect(),
    )
}

fn publish_requests(ctx: &mut ModuleContext<'_>) -> Result<(), ModuleError> {
    let f = state(ctx)?;
    let (pending, asked) = (f.incoming.len(), f.outgoing.len());
    let first = f.incoming.first().map(u64::to_string).unwrap_or_default();
    let (incoming, outgoing) = (characters(&f.incoming), characters(&f.outgoing));
    ctx.set_bool("requested", pending > 0);
    ctx.set_text("request_from", &first);
    ctx.set_int("pending", count(pending));
    ctx.set_int("asked", count(asked));
    ctx.set("incoming", incoming);
    ctx.set("outgoing", outgoing);
    Ok(())
}

fn on_requested(ctx: &mut ModuleContext<'_>, _kind: u16, payload: &[u8]) -> Result<(), ModuleError> {
    let m: Requested = decode(payload)?;
    let f = state(ctx)?;
    // `Pending` follows with the full lists; this shows the prompt at once.
    if !f.incoming.contains(&m.from) && f.incoming.len() < PROMPTS {
        f.incoming.push(m.from);
    }
    publish_requests(ctx)
}

fn on_pending(ctx: &mut ModuleContext<'_>, _kind: u16, payload: &[u8]) -> Result<(), ModuleError> {
    let m: Pending = decode(payload)?;
    let f = state(ctx)?;
    f.incoming.clear();
    f.incoming.extend(m.incoming.iter().copied().take(PROMPTS));
    f.outgoing.clear();
    f.outgoing.extend(m.outgoing.iter().copied().take(PROMPTS));
    publish_requests(ctx)
}

fn on_declined(ctx: &mut ModuleContext<'_>, _kind: u16, payload: &[u8]) -> Result<(), ModuleError> {
    let m: Declined = decode(payload)?;
    state(ctx)?.outgoing.retain(|c| *c != m.by);
    ctx.set_bool("declined", true);
    ctx.set_text("declined_by", &m.by.to_string());
    publish_requests(ctx)
}

fn on_friend_list(ctx: &mut ModuleContext<'_>, _kind: u16, payload: &[u8]) -> Result<(), ModuleError> {
    let m: FriendList = decode(payload)?;
    let mut here = 0usize;
    let items: Vec<ListItem> = m
        .friends
        .iter()
        .enumerate()
        .map(|(i, c)| {
            let present = u32::try_from(i)
                .ok()
                .and_then(|i| m.present.checked_shr(i))
                .is_some_and(|bits| bits & 1 == 1);
            here += usize::from(present);
            ListItem::new()
                .with("character", Value::Text(c.to_string()))
                .with("present", Value::Bool(present))
                .with("presence", Value::text(if present { "here" } else { "away" }))
        })
        .collect();
    ctx.set_int("count", count(items.len()));
    ctx.set_int("here", count(here));
    ctx.set("friends", Value::List(items));
    Ok(())
}

/// Answers the waiting request from the payload's character (the first without one).
fn answer(ctx: &mut ModuleContext<'_>, payload: Option<&str>, accept: bool) -> Result<(), ModuleError> {
    let named = payload.filter(|p| !p.trim().is_empty());
    let f = state(ctx)?;
    let character = match named {
        Some(p) => parse_character(Some(p)).ok_or(ModuleError::Invalid)?,
        None => f.incoming.first().copied().ok_or(ModuleError::Invalid)?,
    };
    if !f.incoming.contains(&character) {
        return Err(ModuleError::Invalid);
    }
    ctx.send(&Respond { character, accept })?;
    // Hidden at once; the server's `Pending` confirms.
    state(ctx)?.incoming.retain(|c| *c != character);
    publish_requests(ctx)
}

fn on_accept(ctx: &mut ModuleContext<'_>, payload: Option<&str>) -> Result<(), ModuleError> {
    answer(ctx, payload, true)
}

fn on_decline(ctx: &mut ModuleContext<'_>, payload: Option<&str>) -> Result<(), ModuleError> {
    answer(ctx, payload, false)
}

fn on_add(ctx: &mut ModuleContext<'_>, payload: Option<&str>) -> Result<(), ModuleError> {
    let character = parse_character(payload).ok_or(ModuleError::Invalid)?;
    ctx.send(&Request { character })
}

fn on_remove(ctx: &mut ModuleContext<'_>, payload: Option<&str>) -> Result<(), ModuleError> {
    let character = parse_character(payload).ok_or(ModuleError::Invalid)?;
    ctx.send(&Remove { character })
}

fn on_show(ctx: &mut ModuleContext<'_>, _payload: Option<&str>) -> Result<(), ModuleError> {
    ctx.send(&ShowFriends {})
}

fn dismiss(ctx: &mut ModuleContext<'_>) {
    ctx.set_bool("declined", false);
    ctx.set_text("declined_by", "");
}

fn on_dismiss(ctx: &mut ModuleContext<'_>, _payload: Option<&str>) -> Result<(), ModuleError> {
    state(ctx)?;
    dismiss(ctx);
    Ok(())
}

fn on_toggle(ctx: &mut ModuleContext<'_>) {
    let open = match state(ctx) {
        Ok(f) => {
            f.open = !f.open;
            f.open
        }
        Err(_) => false,
    };
    ctx.set_bool("open", open);
}

/// Publishes an empty view model; when enabled (including at start), asks the server
/// for the list and the open requests.
fn on_enabled(ctx: &mut ModuleContext<'_>, enabled: bool) {
    let open = match state(ctx) {
        Ok(f) => {
            f.incoming.clear();
            f.outgoing.clear();
            f.open
        }
        Err(_) => false,
    };
    ctx.set_bool("open", open);
    ctx.set_int("count", 0);
    ctx.set_int("here", 0);
    ctx.set("friends", Value::List(Vec::new()));
    dismiss(ctx);
    let _ = publish_requests(ctx);
    if enabled {
        // A full queue drops the ask; `std.friends.show` asks again.
        let _ = ctx.send(&ShowFriends {});
    }
}

impl ClientModule for Module {
    fn key(&self) -> &'static str {
        KEY
    }

    fn register(&self, r: &mut ClientRegistrar<'_>) -> Result<(), ClientRegistryError> {
        r.kinds(1020..=1029)?;
        r.state(Friends::default());
        r.on_message(Requested::ID.0, on_requested)?;
        r.on_message(FriendList::ID.0, on_friend_list)?;
        r.on_message(Pending::ID.0, on_pending)?;
        r.on_message(Declined::ID.0, on_declined)?;
        r.on_intent("std.friends.add", on_add)?;
        r.on_intent("std.friends.remove", on_remove)?;
        r.on_intent("std.friends.accept", on_accept)?;
        r.on_intent("std.friends.decline", on_decline)?;
        r.on_intent("std.friends.show", on_show)?;
        r.on_intent("std.friends.dismiss", on_dismiss)?;
        r.on_enabled(on_enabled);
        r.screen("std.friends.list", FRIENDS_SCREEN, Some(THEME))?;
        r.action("std.friends.toggle", Some(KeyCode::O), on_toggle)?;
        Ok(())
    }
}

//! std.chat client: the chat view model, the chat screen, and the chat intents
//! (plan 13).
//!
//! **View model** (bindable properties, all under `std.chat.`):
//!
//! | property | value |
//! |---|---|
//! | `lines` | list, newest first, at most [`HISTORY`]; each item has `channel` (`local`, `party`, `guild`, or `whisper`), `from` (the speaking character, as text), `to` (a whisper's recipient, as text; empty on other channels), `directed` (flag: the line names a recipient), and `text` |
//! | `count` | lines held (at most [`HISTORY`]) |
//! | `channel` | the selected channel's name |
//! | `whispering` | whether the selected channel is `whisper` |
//! | `target` | the whisper target's character, as text (empty when none) |
//! | `open` | whether the chat screen is shown (toggled by `std.chat.toggle`) |
//!
//! The registry adds `std.chat.enabled`, `std.chat.unavailable`,
//! `std.chat.flag.whispers`, and `std.chat.refusal`; the screen binds `enabled=` to
//! them, so a disabled module or switched-off whispers show as unavailable rather than
//! as dead controls. Whether whispers are available comes from the `whispers` flag at
//! start; the server refuses a whisper, a party line with no party, or a guild line
//! with no guild as not allowed, which shows in `refusal` and leaves the rest of chat
//! working. Only a `FeatureDisabled` refusal (the whole module off) greys out the screen.
//!
//! **The guild channel.** Guild lines reach every online member wherever they are, and
//! show on the `guild` channel. The Guild button binds `enabled=` to the guild module's
//! `std.guild.in_guild`, so it is greyed out ("No guild") while the player has none.
//!
//! **Intents**: `std.chat.say` (payload: the line, as the input submits it on Enter),
//! `std.chat.channel` (local; payload: `local`, `party`, `guild`, or `whisper`),
//! `std.chat.whisper` (local; payload: the target's character; selects the whisper
//! channel, so clicking a sender or submitting the target input starts a whisper).
//!
//! The history keeps the last [`HISTORY`] lines, so memory never grows with the
//! session. Lines are listed newest first, so the newest is always in view.
//!
//! The server holds every rule (`RULES.md`): channel reach, line length and content,
//! and the rate limit. This half only refuses what cannot be sent at all (a blank line,
//! a line over the contract's 200 bytes, a whisper with no target or with whispers
//! off). Characters are shown by id until a name service exists.

#![forbid(unsafe_code)]

use std::collections::VecDeque;

use mantis_client::input::device::KeyCode;
use mantis_client::modules::{
    ClientModule, ClientRegistrar, ClientRegistryError, ModuleContext, ModuleError, decode,
};
use mantis_core::wire::{Message, WireString};
use mantis_ui::{ListItem, Value};
use std_chat_contract::{GUILD, LOCAL, Line, PARTY, Say, WHISPER};

/// The module key.
pub const KEY: &str = "std.chat";

/// Lines the history keeps; older lines are dropped.
pub const HISTORY: usize = 100;

/// The chat screen: the history, the channel selector, and the input line.
pub const CHAT_SCREEN: &str = r#"
panel id=std_chat_panel style=std_chat_panel visible="std.chat.open" enabled="std.chat.enabled" {
  text id=std_chat_unavailable text="Chat unavailable" visible="std.chat.unavailable"
  list id=std_chat_lines bind="std.chat.lines" height=160 clip=true {
    row gap=4 {
      text template="[{item.channel}]"
      button style=std_chat_sender template="{item.from}" intent="std.chat.whisper" payload="{item.from}" enabled="std.chat.flag.whispers"
      text template="to {item.to}" visible="item.directed"
      text template="{item.text}"
    }
  }
  text id=std_chat_refusal bind="std.chat.refusal"
  row id=std_chat_channels gap=4 {
    button id=std_chat_local text="Local" intent="std.chat.channel" payload="local"
    button id=std_chat_party text="Party" intent="std.chat.channel" payload="party"
    button id=std_chat_guild text="Guild" intent="std.chat.channel" payload="guild" enabled="std.guild.in_guild" disabled_text="No guild"
    button id=std_chat_whisper text="Whisper" intent="std.chat.channel" payload="whisper" enabled="std.chat.flag.whispers" disabled_text="Whispers off"
  }
  row id=std_chat_compose gap=4 {
    text id=std_chat_channel bind="std.chat.channel"
    input id=std_chat_target placeholder="Character" submit="std.chat.whisper" visible="std.chat.whispering" enabled="std.chat.flag.whispers" width=80
    input id=std_chat_input placeholder="Say something" submit="std.chat.say" max_length=200 width=grow
  }
}
"#;

/// Styles for the chat screen.
pub const THEME: &str = "
theme {
  style std_chat_panel { background = #1a1e26c0 radius = 6 padding = 8 gap = 4 width = 360 }
  style std_chat_sender { background = #00000000 padding_x = 2 padding_y = 0 }
}
";

/// The client half.
#[derive(Clone, Copy, Debug, Default)]
pub struct Module;

/// View-model state.
#[derive(Clone, Debug)]
struct Chat {
    /// Oldest first; at most [`HISTORY`].
    lines: VecDeque<ListItem>,
    channel: u8,
    target: Option<u64>,
    open: bool,
}

impl Default for Chat {
    fn default() -> Self {
        Self {
            lines: VecDeque::with_capacity(HISTORY),
            channel: LOCAL,
            target: None,
            open: true,
        }
    }
}

fn state<'a>(ctx: &'a mut ModuleContext<'_>) -> Result<&'a mut Chat, ModuleError> {
    ctx.state::<Chat>().ok_or(ModuleError::Invalid)
}

/// The channel's name, or `None` for a channel the contract does not define.
fn channel_name(channel: u8) -> Option<&'static str> {
    match channel {
        LOCAL => Some("local"),
        PARTY => Some("party"),
        WHISPER => Some("whisper"),
        GUILD => Some("guild"),
        _ => None,
    }
}

fn parse_character(payload: Option<&str>) -> Result<u64, ModuleError> {
    payload
        .and_then(|p| p.trim().parse::<u64>().ok())
        .filter(|c| *c != 0)
        .ok_or(ModuleError::Invalid)
}

fn publish_lines(ctx: &mut ModuleContext<'_>) -> Result<(), ModuleError> {
    let items: Vec<ListItem> = state(ctx)?.lines.iter().rev().cloned().collect();
    ctx.set_int("count", i64::try_from(items.len()).unwrap_or(0));
    ctx.set("lines", Value::List(items));
    Ok(())
}

fn publish_selection(ctx: &mut ModuleContext<'_>) -> Result<(), ModuleError> {
    let (channel, target) = {
        let chat = state(ctx)?;
        (chat.channel, chat.target)
    };
    ctx.set_text("channel", channel_name(channel).unwrap_or(""));
    ctx.set_bool("whispering", channel == WHISPER);
    ctx.set_text("target", &target.map(|t| t.to_string()).unwrap_or_default());
    Ok(())
}

fn on_line(ctx: &mut ModuleContext<'_>, _kind: u16, payload: &[u8]) -> Result<(), ModuleError> {
    let m: Line = decode(payload)?;
    let channel = channel_name(m.channel).ok_or(ModuleError::Malformed)?;
    let to = if m.to == 0 {
        String::new()
    } else {
        m.to.to_string()
    };
    let item = ListItem::new()
        .with("channel", Value::text(channel))
        .with("from", Value::Text(m.from.to_string()))
        .with("directed", Value::Bool(m.to != 0))
        .with("to", Value::Text(to))
        .with("text", Value::text(m.text.as_str()));
    let chat = state(ctx)?;
    while chat.lines.len() >= HISTORY {
        chat.lines.pop_front();
    }
    chat.lines.push_back(item);
    publish_lines(ctx)
}

fn on_say(ctx: &mut ModuleContext<'_>, payload: Option<&str>) -> Result<(), ModuleError> {
    let text = payload
        .filter(|t| !t.trim().is_empty())
        .ok_or(ModuleError::Invalid)?;
    let text = WireString::new(text).ok_or(ModuleError::Invalid)?;
    let (channel, target) = {
        let chat = state(ctx)?;
        (chat.channel, chat.target)
    };
    let target = if channel == WHISPER {
        if !ctx.flag("whispers") {
            return Err(ModuleError::Invalid);
        }
        target.ok_or(ModuleError::Invalid)?
    } else {
        0
    };
    ctx.send(&Say {
        channel,
        target,
        text,
    })
}

fn on_channel(ctx: &mut ModuleContext<'_>, payload: Option<&str>) -> Result<(), ModuleError> {
    let channel = match payload.map(str::trim) {
        Some("local") => LOCAL,
        Some("party") => PARTY,
        Some("guild") => GUILD,
        Some("whisper") if ctx.flag("whispers") => WHISPER,
        _ => return Err(ModuleError::Invalid),
    };
    state(ctx)?.channel = channel;
    publish_selection(ctx)
}

fn on_whisper(ctx: &mut ModuleContext<'_>, payload: Option<&str>) -> Result<(), ModuleError> {
    if !ctx.flag("whispers") {
        return Err(ModuleError::Invalid);
    }
    let target = parse_character(payload)?;
    let chat = state(ctx)?;
    chat.target = Some(target);
    chat.channel = WHISPER;
    publish_selection(ctx)
}

fn on_toggle(ctx: &mut ModuleContext<'_>) {
    let open = match state(ctx) {
        Ok(c) => {
            c.open = !c.open;
            c.open
        }
        Err(_) => false,
    };
    ctx.set_bool("open", open);
}

/// Publishes the starting view model when the module starts or is enabled, and
/// resets it when the module is disabled.
fn on_enabled(ctx: &mut ModuleContext<'_>, enabled: bool) {
    let open = match state(ctx) {
        Ok(chat) => {
            if !enabled {
                chat.lines.clear();
                chat.channel = LOCAL;
                chat.target = None;
            }
            chat.open
        }
        Err(_) => false,
    };
    ctx.set_bool("open", open);
    let _ = publish_lines(ctx);
    let _ = publish_selection(ctx);
}

impl ClientModule for Module {
    fn key(&self) -> &'static str {
        KEY
    }

    fn register(&self, r: &mut ClientRegistrar<'_>) -> Result<(), ClientRegistryError> {
        r.kinds(1010..=1019)?;
        r.state(Chat::default());
        r.on_message(Line::ID.0, on_line)?;
        r.on_intent("std.chat.say", on_say)?;
        r.on_intent("std.chat.channel", on_channel)?;
        r.on_intent("std.chat.whisper", on_whisper)?;
        r.on_enabled(on_enabled);
        r.screen("std.chat.window", CHAT_SCREEN, Some(THEME))?;
        r.action("std.chat.toggle", Some(KeyCode::Slash), on_toggle)?;
        Ok(())
    }
}

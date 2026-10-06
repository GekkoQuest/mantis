//! std.titles client: the titles view model, the titles screen, and the title intents
//! (plan 13).
//!
//! **View model** (bindable properties, all under `std.titles.`):
//!
//! | property | value |
//! |---|---|
//! | `titles` | list, ascending as the server sends it; each item has `title` (int title id) and `active` (flag: the one shown) |
//! | `count` | titles held (int) |
//! | `active` | the title shown (int, 0 for none) |
//! | `has_active` | whether a title is shown |
//! | `open` | whether the titles screen is shown: false at start, toggled by `std.titles.toggle` |
//!
//! Every property gets its starting value from `on_enabled`, which the registry runs at
//! start. The registry adds `std.titles.enabled`, `std.titles.unavailable`,
//! `std.titles.flag.enabled`, and `std.titles.refusal`; the screen binds `enabled=` to
//! them, so a disabled module shows as unavailable rather than as dead buttons.
//!
//! **Messages**: `Titles` replaces the list and the shown title (the server sends it
//! after every change).
//!
//! **Intents**: `std.titles.set` (payload: a held title id, or `0` to show none; sends
//! `SetActiveTitle`) and `std.titles.show` (sends `ShowTitles`). Pressing
//! `std.titles.toggle` (default key `T`) opens or closes the screen. When the module
//! starts (or is enabled again) it sends `ShowTitles`, so the list survives a reconnect.
//!
//! The contract's `GrantTitle` has no intent: titles are awarded only by services (Ops,
//! events, restores) or other modules, and the server refuses a grant carrying a client
//! session.
//!
//! The server holds every rule (`RULES.md`): which titles exist and who holds them. This
//! half only refuses what cannot mean anything (a title the player does not hold).
//! Titles are shown by id until the client has the title names.

#![forbid(unsafe_code)]

use mantis_client::input::device::KeyCode;
use mantis_client::modules::{
    ClientModule, ClientRegistrar, ClientRegistryError, ModuleContext, ModuleError, decode,
};
use mantis_core::wire::Message;
use mantis_ui::{ListItem, Value};
use std_titles_contract::{SetActiveTitle, ShowTitles, Titles};

/// The module key.
pub const KEY: &str = "std.titles";

/// The titles screen: the held titles, which one is shown, and a way to show none.
pub const TITLES_SCREEN: &str = r#"
panel id=std_titles_panel style=std_titles_panel visible="std.titles.open" enabled="std.titles.enabled" {
  text id=std_titles_heading text="Titles"
  text id=std_titles_unavailable text="Titles unavailable" visible="std.titles.unavailable"
  text id=std_titles_active template="Showing title {std.titles.active}" visible="std.titles.has_active"
  list id=std_titles_list bind="std.titles.titles" height=fit {
    row gap=4 {
      text template="Title {item.title}"
      text text="(shown)" visible="item.active"
      button text="Show" intent="std.titles.set" payload="{item.title}"
    }
  }
  row id=std_titles_buttons gap=8 {
    button id=std_titles_none text="Show none" intent="std.titles.set" payload="0" enabled="std.titles.has_active" disabled_text="Show none"
    button id=std_titles_refresh text="Refresh" intent="std.titles.show"
  }
  text id=std_titles_refusal bind="std.titles.refusal"
}
"#;

/// Styles for the titles screen.
pub const THEME: &str = "
theme {
  style std_titles_panel { background = #1a1e26e0 radius = 6 padding = 8 gap = 4 width = 260 }
}
";

/// The client half.
#[derive(Clone, Copy, Debug, Default)]
pub struct Module;

/// View-model state.
#[derive(Clone, Debug, Default)]
struct View {
    open: bool,
    owned: Vec<u32>,
    active: u32,
}

fn state<'a>(ctx: &'a mut ModuleContext<'_>) -> Result<&'a mut View, ModuleError> {
    ctx.state::<View>().ok_or(ModuleError::Invalid)
}

fn publish(ctx: &mut ModuleContext<'_>, owned: &[u32], active: u32) {
    let items: Vec<ListItem> = owned
        .iter()
        .map(|t| {
            ListItem::new()
                .with("title", Value::Int(i64::from(*t)))
                .with("active", Value::Bool(*t == active))
        })
        .collect();
    ctx.set_int("count", i64::try_from(items.len()).unwrap_or(0));
    ctx.set("titles", Value::List(items));
    ctx.set_int("active", i64::from(active));
    ctx.set_bool("has_active", active != 0);
}

fn on_titles(ctx: &mut ModuleContext<'_>, _kind: u16, payload: &[u8]) -> Result<(), ModuleError> {
    let m: Titles = decode(payload)?;
    let owned: Vec<u32> = m.owned.iter().copied().collect();
    publish(ctx, &owned, m.active);
    let v = state(ctx)?;
    v.owned = owned;
    v.active = m.active;
    Ok(())
}

fn on_set(ctx: &mut ModuleContext<'_>, payload: Option<&str>) -> Result<(), ModuleError> {
    let title: u32 = payload
        .and_then(|p| p.trim().parse().ok())
        .ok_or(ModuleError::Invalid)?;
    let v = state(ctx)?;
    if title != 0 && !v.owned.contains(&title) {
        return Err(ModuleError::Invalid);
    }
    ctx.send(&SetActiveTitle { title })
}

fn on_show(ctx: &mut ModuleContext<'_>, _payload: Option<&str>) -> Result<(), ModuleError> {
    ctx.send(&ShowTitles {})
}

fn on_toggle(ctx: &mut ModuleContext<'_>) {
    let open = match state(ctx) {
        Ok(v) => {
            v.open = !v.open;
            v.open
        }
        Err(_) => false,
    };
    ctx.set_bool("open", open);
}

/// Runs at start and whenever the module is switched: publishes every starting value
/// and, when enabled, asks for the titles.
fn on_enabled(ctx: &mut ModuleContext<'_>, enabled: bool) {
    let open = match state(ctx) {
        Ok(v) => {
            v.owned.clear();
            v.active = 0;
            v.open
        }
        Err(_) => false,
    };
    ctx.set_bool("open", open);
    publish(ctx, &[], 0);
    if enabled {
        // A full queue drops the ask; `std.titles.show` asks again.
        let _ = ctx.send(&ShowTitles {});
    }
}

impl ClientModule for Module {
    fn key(&self) -> &'static str {
        KEY
    }

    fn register(&self, r: &mut ClientRegistrar<'_>) -> Result<(), ClientRegistryError> {
        r.kinds(1070..=1079)?;
        r.state(View::default());
        r.on_message(Titles::ID.0, on_titles)?;
        r.on_intent("std.titles.set", on_set)?;
        r.on_intent("std.titles.show", on_show)?;
        r.on_enabled(on_enabled);
        r.screen("std.titles.list", TITLES_SCREEN, Some(THEME))?;
        r.action("std.titles.toggle", Some(KeyCode::T), on_toggle)?;
        Ok(())
    }
}

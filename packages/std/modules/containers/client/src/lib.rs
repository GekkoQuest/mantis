//! std.containers client: the bag view model, the bag screen, and the bag intents
//! (plan 13).
//!
//! **View model** (bindable properties, all under `std.containers.`):
//!
//! | property | value |
//! |---|---|
//! | `gold` | the gold balance, as text, verbatim from the server's `Bag` |
//! | `items` | list, one item per slot; each has `slot` (int), `item` (int content id, 0 for an empty slot), `count` (int), and `filled` (flag) |
//! | `capacity` | slots in the bag (int) |
//! | `selected` | the selected slot (int, -1 for none) |
//! | `has_selection` | whether a slot is selected |
//! | `quantity` | how many a split or destroy takes (int, at least 1) |
//! | `confirming` | whether a destroy waits for its confirmation |
//! | `confirm_slot`, `confirm_count` | what the armed destroy removes (ints) |
//! | `open` | whether the bag screen is shown: false at start, toggled by `std.containers.toggle` |
//!
//! Every property gets its starting value from `on_enabled`, which the registry runs at
//! start, so no confirmation or empty list shows before the first `Bag`. The registry
//! adds `std.containers.enabled`, `std.containers.unavailable`,
//! `std.containers.flag.enabled`, and `std.containers.refusal`; the screen binds
//! `enabled=` to them, so a disabled module shows as unavailable rather than as dead
//! buttons.
//!
//! **Intents**: `std.containers.show` (asks for the bag), `std.containers.select`
//! (payload: a filled slot), `std.containers.move` and `std.containers.split` (payload:
//! the target slot; they act on the selected slot, a split takes `quantity`),
//! `std.containers.quantity` (payload: a positive number), `std.containers.destroy`
//! (no payload; the first arms a destroy of `quantity` from the selected slot, the second
//! confirms and sends it), and `std.containers.cancel` (drops the selection and any armed
//! destroy). Pressing `std.containers.toggle` (default key `I`) opens or closes the
//! screen; opening asks for the bag.
//!
//! The contract's `Grant` has no intent: grants come only from services (quests, Ops,
//! restores) and the server refuses one carrying a client session.
//!
//! The server holds every rule (`RULES.md`): stack limits, merging, and whether a split
//! fits are checked there. This half only refuses requests that cannot mean anything
//! (no selection, a slot outside the bag, a zero quantity). Items are shown by content
//! id until an item-name table exists.

#![forbid(unsafe_code)]

use mantis_client::input::device::KeyCode;
use mantis_client::modules::{
    ClientModule, ClientRegistrar, ClientRegistryError, ModuleContext, ModuleError, decode,
};
use mantis_core::wire::Message;
use mantis_ui::{ListItem, Value};
use std_containers_contract::{Bag, DestroyItem, MoveItem, ShowBag, SplitStack};

/// The module key.
pub const KEY: &str = "std.containers";

/// The bag screen: gold, the slots, the move/split/destroy controls, and the destroy
/// confirmation.
pub const BAG_SCREEN: &str = r#"
panel id=std_containers_panel style=std_containers_panel visible="std.containers.open" enabled="std.containers.enabled" {
  text id=std_containers_title text="Bag"
  text id=std_containers_unavailable text="Bag unavailable" visible="std.containers.unavailable"
  text id=std_containers_gold template="Gold {std.containers.gold}"
  list id=std_containers_slots bind="std.containers.items" height=fit {
    row gap=4 {
      text template="{item.slot}"
      text template="Item {item.item} x {item.count}" visible="item.filled"
      button text="Select" intent="std.containers.select" payload="{item.slot}" visible="item.filled"
      button text="Move here" intent="std.containers.move" payload="{item.slot}" enabled="std.containers.has_selection" disabled_text="Move here"
      button text="Split here" intent="std.containers.split" payload="{item.slot}" enabled="std.containers.has_selection" disabled_text="Split here"
    }
  }
  text id=std_containers_selected template="Selected slot {std.containers.selected}" visible="std.containers.has_selection"
  row id=std_containers_tools gap=8 {
    input id=std_containers_quantity placeholder="Quantity" submit="std.containers.quantity"
    text id=std_containers_quantity_text template="x {std.containers.quantity}"
    button id=std_containers_destroy text="Destroy" intent="std.containers.destroy" enabled="std.containers.has_selection" disabled_text="Destroy"
    button id=std_containers_refresh text="Refresh" intent="std.containers.show"
  }
  panel id=std_containers_confirm visible="std.containers.confirming" {
    text id=std_containers_confirm_text template="Destroy {std.containers.confirm_count} from slot {std.containers.confirm_slot}?"
    row id=std_containers_confirm_buttons gap=8 {
      button id=std_containers_confirm_destroy text="Confirm" intent="std.containers.destroy"
      button id=std_containers_cancel text="Cancel" intent="std.containers.cancel"
    }
  }
  text id=std_containers_refusal bind="std.containers.refusal"
}
"#;

/// Styles for the bag screen.
pub const THEME: &str = "
theme {
  style std_containers_panel { background = #1a1e26e0 radius = 6 padding = 8 gap = 4 width = 320 }
}
";

/// The client half.
#[derive(Clone, Copy, Debug, Default)]
pub struct Module;

/// View-model state.
#[derive(Clone, Debug)]
struct View {
    open: bool,
    /// (item, count) per slot from the last `Bag`; item 0 is an empty slot.
    slots: Vec<(u32, u32)>,
    selected: Option<u8>,
    quantity: u32,
    /// An armed destroy: (slot, count).
    armed: Option<(u8, u32)>,
}

impl View {
    fn new(open: bool) -> Self {
        Self {
            open,
            slots: Vec::new(),
            selected: None,
            quantity: 1,
            armed: None,
        }
    }

    fn filled(&self, slot: u8) -> bool {
        self.slots
            .get(usize::from(slot))
            .is_some_and(|(item, _)| *item != 0)
    }

    fn in_bag(&self, slot: u8) -> bool {
        usize::from(slot) < self.slots.len()
    }
}

fn state<'a>(ctx: &'a mut ModuleContext<'_>) -> Result<&'a mut View, ModuleError> {
    ctx.state::<View>().ok_or(ModuleError::Invalid)
}

fn parse<T: core::str::FromStr>(payload: Option<&str>) -> Result<T, ModuleError> {
    payload
        .and_then(|p| p.trim().parse().ok())
        .ok_or(ModuleError::Invalid)
}

fn publish_selection(ctx: &mut ModuleContext<'_>, selected: Option<u8>) {
    ctx.set_int("selected", selected.map_or(-1, i64::from));
    ctx.set_bool("has_selection", selected.is_some());
}

fn publish_armed(ctx: &mut ModuleContext<'_>, armed: Option<(u8, u32)>) {
    ctx.set_bool("confirming", armed.is_some());
    ctx.set_int("confirm_slot", armed.map_or(-1, |(s, _)| i64::from(s)));
    ctx.set_int("confirm_count", armed.map_or(0, |(_, c)| i64::from(c)));
}

/// Drops the selection and any armed destroy.
fn clear_selection(ctx: &mut ModuleContext<'_>) -> Result<(), ModuleError> {
    let v = state(ctx)?;
    v.selected = None;
    v.armed = None;
    publish_selection(ctx, None);
    publish_armed(ctx, None);
    Ok(())
}

fn on_bag(ctx: &mut ModuleContext<'_>, _kind: u16, payload: &[u8]) -> Result<(), ModuleError> {
    let m: Bag = decode(payload)?;
    if m.items.len() != m.counts.len() {
        return Err(ModuleError::Malformed);
    }
    let slots: Vec<(u32, u32)> = m
        .items
        .iter()
        .zip(m.counts.iter())
        .map(|(item, count)| if *item == 0 { (0, 0) } else { (*item, *count) })
        .collect();
    let items: Vec<ListItem> = slots
        .iter()
        .enumerate()
        .map(|(slot, (item, count))| {
            ListItem::new()
                .with("slot", Value::Int(i64::try_from(slot).unwrap_or(0)))
                .with("item", Value::Int(i64::from(*item)))
                .with("count", Value::Int(i64::from(*count)))
                .with("filled", Value::Bool(*item != 0))
        })
        .collect();
    let capacity = i64::try_from(slots.len()).unwrap_or(0);
    let v = state(ctx)?;
    v.slots = slots;
    // A selection survives a refresh only while its slot still holds something; an
    // armed destroy never does (the stack it described may have changed).
    let selected = v.selected.filter(|s| v.filled(*s));
    v.selected = selected;
    v.armed = None;
    ctx.set_text("gold", &m.gold.to_string());
    ctx.set_int("capacity", capacity);
    ctx.set("items", Value::List(items));
    publish_selection(ctx, selected);
    publish_armed(ctx, None);
    Ok(())
}

fn on_show(ctx: &mut ModuleContext<'_>, _payload: Option<&str>) -> Result<(), ModuleError> {
    ctx.send(&ShowBag {})
}

fn on_select(ctx: &mut ModuleContext<'_>, payload: Option<&str>) -> Result<(), ModuleError> {
    let slot: u8 = parse(payload)?;
    let v = state(ctx)?;
    if !v.filled(slot) {
        return Err(ModuleError::Invalid);
    }
    v.selected = Some(slot);
    v.armed = None;
    publish_selection(ctx, Some(slot));
    publish_armed(ctx, None);
    Ok(())
}

/// The selected slot and a distinct target slot inside the bag.
fn from_to(ctx: &mut ModuleContext<'_>, payload: Option<&str>) -> Result<(u8, u8, u32), ModuleError> {
    let to: u8 = parse(payload)?;
    let v = state(ctx)?;
    let from = v.selected.ok_or(ModuleError::Invalid)?;
    if to == from || !v.in_bag(to) {
        return Err(ModuleError::Invalid);
    }
    Ok((from, to, v.quantity))
}

fn on_move(ctx: &mut ModuleContext<'_>, payload: Option<&str>) -> Result<(), ModuleError> {
    let (from, to, _) = from_to(ctx, payload)?;
    ctx.send(&MoveItem { from, to })?;
    clear_selection(ctx)
}

fn on_split(ctx: &mut ModuleContext<'_>, payload: Option<&str>) -> Result<(), ModuleError> {
    let (from, to, count) = from_to(ctx, payload)?;
    if count == 0 {
        return Err(ModuleError::Invalid);
    }
    ctx.send(&SplitStack { from, to, count })?;
    clear_selection(ctx)
}

fn on_quantity(ctx: &mut ModuleContext<'_>, payload: Option<&str>) -> Result<(), ModuleError> {
    let quantity: u32 = parse(payload)?;
    if quantity == 0 {
        return Err(ModuleError::Invalid);
    }
    let v = state(ctx)?;
    v.quantity = quantity;
    // A changed quantity no longer matches the confirmation on screen.
    v.armed = None;
    ctx.set_int("quantity", i64::from(quantity));
    publish_armed(ctx, None);
    Ok(())
}

fn on_destroy(ctx: &mut ModuleContext<'_>, _payload: Option<&str>) -> Result<(), ModuleError> {
    let v = state(ctx)?;
    let (armed, selected, quantity) = (v.armed, v.selected, v.quantity);
    if let Some((slot, count)) = armed {
        ctx.send(&DestroyItem { slot, count })?;
        return clear_selection(ctx);
    }
    let slot = selected.ok_or(ModuleError::Invalid)?;
    if quantity == 0 {
        return Err(ModuleError::Invalid);
    }
    let armed = Some((slot, quantity));
    state(ctx)?.armed = armed;
    publish_armed(ctx, armed);
    Ok(())
}

fn on_cancel(ctx: &mut ModuleContext<'_>, _payload: Option<&str>) -> Result<(), ModuleError> {
    clear_selection(ctx)
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
    if open {
        // A full queue only means the shown bag is older; the next refresh fixes it.
        let _ = ctx.send(&ShowBag {});
    }
}

/// Runs at start and whenever the module is switched: publishes every starting value.
/// The screen stays as open or closed as it was, so a disabled bag says why.
fn on_enabled(ctx: &mut ModuleContext<'_>, _enabled: bool) {
    let open = match state(ctx) {
        Ok(v) => {
            *v = View::new(v.open);
            v.open
        }
        Err(_) => false,
    };
    ctx.set_bool("open", open);
    ctx.set_text("gold", "");
    ctx.set_int("capacity", 0);
    ctx.set("items", Value::List(Vec::new()));
    ctx.set_int("quantity", 1);
    publish_selection(ctx, None);
    publish_armed(ctx, None);
}

impl ClientModule for Module {
    fn key(&self) -> &'static str {
        KEY
    }

    fn register(&self, r: &mut ClientRegistrar<'_>) -> Result<(), ClientRegistryError> {
        r.kinds(1040..=1049)?;
        r.state(View::new(false));
        r.on_message(Bag::ID.0, on_bag)?;
        r.on_intent("std.containers.show", on_show)?;
        r.on_intent("std.containers.select", on_select)?;
        r.on_intent("std.containers.move", on_move)?;
        r.on_intent("std.containers.split", on_split)?;
        r.on_intent("std.containers.quantity", on_quantity)?;
        r.on_intent("std.containers.destroy", on_destroy)?;
        r.on_intent("std.containers.cancel", on_cancel)?;
        r.on_enabled(on_enabled);
        r.screen("std.containers.bag", BAG_SCREEN, Some(THEME))?;
        r.action("std.containers.toggle", Some(KeyCode::I), on_toggle)?;
        Ok(())
    }
}

//! std.vendor client: the vendor view model, the shop screen, and the trade intents
//! (plan 13).
//!
//! **View model** (bindable properties, all under `std.vendor.`):
//!
//! | property | value |
//! |---|---|
//! | `open` | whether the shop screen is shown: false at start, set by a `Stock`, cleared by `std.vendor.close` |
//! | `vendor` | the open vendor's id (int, 0 for none) |
//! | `stock` | list, one item per listing; each has `item` (int content id), `price` (text: gold the vendor charges per item), and `sell_price` (text: gold vendors pay per item, `0` when they do not buy it), both verbatim from the server's `Stock`, and `total` (text: what buying `quantity` of it costs, `quantity` x `price`) |
//! | `listings` | how many listings the open vendor has (int) |
//! | `quantity` | how many a buy or sell takes (int, at least 1) |
//! | `result` | the last answered trade's outcome as text, from the server's `Traded`: what moved and the gold paid or received, or why it failed (empty until one arrives, and again once a new trade is sent) |
//! | `result_ok` | whether the last answered trade happened (flag) |
//! | `pending` | trades sent and not yet answered (int) |
//! | `unmatched` | `Traded` answers that matched no pending trade, ignored (int) |
//!
//! Every property gets its starting value from `on_enabled`, which the registry runs at
//! start, so the shop stays hidden until its first `Stock`. The registry adds
//! `std.vendor.enabled`, `std.vendor.unavailable`, `std.vendor.flag.enabled`,
//! `std.vendor.flag.selling`, and `std.vendor.refusal`; the shop binds `enabled=` to
//! them, so a disabled module or switched-off selling show as unavailable rather than
//! as dead buttons. Selling's availability comes from the start-up flag: the server
//! refuses a sale while selling is off as not allowed (`FeatureDisabled` means the whole
//! module is off). The `Traded` answer is the authoritative outcome shown as `result`.
//! The module answers the server's refusals of its trades itself (the registry's
//! `refusal` is left alone): a refused trade whose `Traded` has arrived needs nothing
//! more; one refused before any answer (a malformed request) marks the oldest pending
//! trade of that kind as failed in `result`, with the refusal as its reason.
//!
//! The shop also binds two properties of the `std.containers` view model, the module
//! this one depends on: `std.containers.gold` (the balance) and `std.containers.items`
//! (the bag slots offered for sale). After a trade the server sends its `Traded` here
//! and, when it happened, the new bag as a containers `Bag`.
//!
//! **Intents**: `std.vendor.browse` (payload: the vendor id; the shop opens when its
//! `Stock` arrives), `std.vendor.quantity` (payload: a positive number),
//! `std.vendor.buy` (payload: a listed item of the open vendor; sends `BuyItems` for
//! `quantity`), `std.vendor.sell` (payload: a bag slot; sends `SellItems` for
//! `quantity`, only while the shop is open and the `selling` flag is on), and
//! `std.vendor.close`. Each trade names the open vendor (from its `Stock`) and carries a
//! request number from a counter the module keeps (never 0, kept across a reset so a
//! late answer never matches a newer trade); its `Traded` is matched by that number.
//!
//! The server holds every rule (`RULES.md`): stack limits, gold, bag room, and what a
//! vendor pays are checked and computed there. This half shows both prices exactly as
//! the server sent them; the only arithmetic is the purchase preview `quantity` x
//! `price`, computed without overflow for every price (turning a `Traded` reason code
//! into its text is presentation, not a rule). It only refuses requests that cannot
//! mean anything (no open vendor, an unlisted item, a slot outside the bag, a zero
//! quantity).

#![forbid(unsafe_code)]

use std::collections::VecDeque;

use mantis_client::modules::{
    ClientModule, ClientRegistrar, ClientRegistryError, ExtensionRefusal, ModuleContext, ModuleError, decode,
};
use mantis_core::wire::Message;
use mantis_ui::{ListItem, Value};
use std_containers_contract::SLOTS;
use std_vendor_contract::{
    Browse, BuyItems, SellItems, Stock, TRADE_BAG_FULL, TRADE_INSUFFICIENT, TRADE_MALFORMED,
    TRADE_NOT_BOUGHT, TRADE_NOT_LISTED, TRADE_NOTHING, TRADE_SELLING_OFF, Traded,
};

/// The module key.
pub const KEY: &str = "std.vendor";

/// The shop screen: the vendor's listings with their prices and the cost of buying the
/// chosen quantity, the quantity, the bag's stacks to sell, and a close button.
pub const SHOP_SCREEN: &str = r#"
panel id=std_vendor_panel style=std_vendor_panel visible="std.vendor.open" enabled="std.vendor.enabled" {
  text id=std_vendor_title template="Vendor {std.vendor.vendor}"
  text id=std_vendor_unavailable text="Vendor unavailable" visible="std.vendor.unavailable"
  text id=std_vendor_gold template="Gold {std.containers.gold}"
  panel id=std_vendor_buy {
    list id=std_vendor_stock bind="std.vendor.stock" height=fit {
      row gap=4 {
        text template="Item {item.item}"
        text template="{item.price} gold each, vendors pay {item.sell_price}"
        button template="Buy {std.vendor.quantity} for {item.total} gold" intent="std.vendor.buy" payload="{item.item}" disabled_text="Buy"
      }
    }
  }
  row id=std_vendor_tools gap=8 {
    input id=std_vendor_quantity placeholder="Quantity" submit="std.vendor.quantity"
    text id=std_vendor_quantity_text template="x {std.vendor.quantity}"
    button id=std_vendor_close text="Close" intent="std.vendor.close"
  }
  panel id=std_vendor_sell enabled="std.vendor.flag.selling" {
    text id=std_vendor_sell_title text="Sell"
    list id=std_vendor_bag bind="std.containers.items" height=fit {
      row gap=4 visible="item.filled" {
        text template="Item {item.item} x {item.count}"
        button text="Sell" intent="std.vendor.sell" payload="{item.slot}" disabled_text="Not buying"
      }
    }
  }
  text id=std_vendor_result bind="std.vendor.result"
  text id=std_vendor_refusal bind="std.vendor.refusal"
}
"#;

/// Styles for the shop screen.
pub const THEME: &str = "
theme {
  style std_vendor_panel { background = #1a1e26e0 radius = 6 padding = 8 gap = 4 width = 360 }
}
";

/// Trades sent and not yet answered, kept at most (older ones are forgotten).
const PENDING: usize = 16;

/// The client half.
#[derive(Clone, Copy, Debug, Default)]
pub struct Module;

/// A trade sent and not yet answered.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Trade {
    Buy,
    Sell,
}

/// View-model state.
#[derive(Clone, Debug)]
struct View {
    /// The open vendor.
    vendor: Option<u32>,
    /// The open vendor's listings: item, price, and what vendors pay for it.
    stock: Vec<(u32, u64, u64)>,
    quantity: u32,
    /// The next request number (never 0).
    next_request: u32,
    /// Trades waiting for their `Traded`, oldest first.
    pending: VecDeque<(u32, Trade)>,
    unmatched: u32,
}

impl View {
    fn new(next_request: u32) -> Self {
        Self {
            vendor: None,
            stock: Vec::new(),
            quantity: 1,
            next_request: next_request.max(1),
            pending: VecDeque::new(),
            unmatched: 0,
        }
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

/// What buying `quantity` at `price` each costs, as text (exact for every price).
#[must_use]
pub fn purchase_total(quantity: u32, price: u64) -> String {
    (u128::from(quantity) * u128::from(price)).to_string()
}

/// The text a failed trade shows for a `Traded` reason (codes from the contract).
#[must_use]
pub fn reason_text(reason: u8) -> &'static str {
    match reason {
        TRADE_INSUFFICIENT => "not enough gold or items",
        TRADE_BAG_FULL => "bag full",
        TRADE_NOT_LISTED => "not for sale here",
        TRADE_NOTHING => "nothing to sell",
        TRADE_NOT_BOUGHT => "vendors do not buy it",
        TRADE_SELLING_OFF => "selling is switched off",
        TRADE_MALFORMED => "malformed request",
        _ => "refused",
    }
}

fn result_text(trade: Trade, m: &Traded) -> String {
    match (trade, m.ok) {
        (Trade::Buy, true) => format!("Bought {} of item {} for {} gold", m.count, m.item, m.total),
        (Trade::Sell, true) => format!("Sold {} of item {} for {} gold", m.count, m.item, m.total),
        (Trade::Buy, false) => format!("Purchase failed: {}", reason_text(m.reason)),
        (Trade::Sell, false) => format!("Sale failed: {}", reason_text(m.reason)),
    }
}

/// The text a refusal shows as a reason.
fn refusal_text(reason: ExtensionRefusal) -> &'static str {
    match reason {
        ExtensionRefusal::NotAllowed => "not allowed by the server",
        ExtensionRefusal::Invalid => "the server could not read the request",
        ExtensionRefusal::FeatureDisabled => "the vendor is switched off",
    }
}

/// Answers the server's refusals of the module's trades (`request` is the session's
/// envelope number, not the module's, so the kind decides). A refused trade still
/// waiting for its `Traded` is the oldest pending one of its kind: it failed.
fn on_refused(ctx: &mut ModuleContext<'_>, kind: u16, _request: u32, reason: ExtensionRefusal) -> bool {
    let trade = match kind {
        k if k == BuyItems::ID.0 => Trade::Buy,
        k if k == SellItems::ID.0 => Trade::Sell,
        _ => return false,
    };
    let Ok(v) = state(ctx) else {
        return false;
    };
    let failed = v
        .pending
        .iter()
        .position(|(_, t)| *t == trade)
        .and_then(|i| v.pending.remove(i));
    let v = v.clone();
    publish_requests(ctx, &v);
    if failed.is_some() {
        let what = match trade {
            Trade::Buy => "Purchase",
            Trade::Sell => "Sale",
        };
        ctx.set_text("result", &format!("{what} failed: {}", refusal_text(reason)));
        ctx.set_bool("result_ok", false);
    }
    true
}

fn publish_stock(ctx: &mut ModuleContext<'_>, v: &View) {
    let listings: Vec<ListItem> = v
        .stock
        .iter()
        .map(|(item, price, sell_price)| {
            ListItem::new()
                .with("item", Value::Int(i64::from(*item)))
                .with("price", Value::Text(price.to_string()))
                .with("sell_price", Value::Text(sell_price.to_string()))
                .with("total", Value::Text(purchase_total(v.quantity, *price)))
        })
        .collect();
    ctx.set_bool("open", v.vendor.is_some());
    ctx.set_int("vendor", v.vendor.map_or(0, i64::from));
    ctx.set_int("listings", i64::try_from(listings.len()).unwrap_or(0));
    ctx.set("stock", Value::List(listings));
    ctx.set_int("quantity", i64::from(v.quantity));
}

fn publish_requests(ctx: &mut ModuleContext<'_>, v: &View) {
    ctx.set_int("pending", i64::try_from(v.pending.len()).unwrap_or(0));
    ctx.set_int("unmatched", i64::from(v.unmatched));
}

fn clear_result(ctx: &mut ModuleContext<'_>) {
    ctx.set_text("result", "");
    ctx.set_bool("result_ok", false);
}

fn on_stock(ctx: &mut ModuleContext<'_>, _kind: u16, payload: &[u8]) -> Result<(), ModuleError> {
    let m: Stock = decode(payload)?;
    if m.items.len() != m.prices.len() || m.items.len() != m.sell_prices.len() {
        return Err(ModuleError::Malformed);
    }
    let v = state(ctx)?;
    v.vendor = Some(m.vendor);
    v.stock = m
        .items
        .iter()
        .zip(m.prices.iter())
        .zip(m.sell_prices.iter())
        .map(|((item, price), sell_price)| (*item, *price, *sell_price))
        .collect();
    let v = v.clone();
    publish_stock(ctx, &v);
    clear_result(ctx);
    Ok(())
}

fn on_traded(ctx: &mut ModuleContext<'_>, _kind: u16, payload: &[u8]) -> Result<(), ModuleError> {
    let m: Traded = decode(payload)?;
    let v = state(ctx)?;
    let matched = v
        .pending
        .iter()
        .position(|(r, _)| *r == m.request)
        .and_then(|i| v.pending.remove(i));
    if matched.is_none() {
        v.unmatched = v.unmatched.saturating_add(1);
    }
    let v = v.clone();
    publish_requests(ctx, &v);
    if let Some((_, trade)) = matched {
        ctx.set_text("result", &result_text(trade, &m));
        ctx.set_bool("result_ok", m.ok);
    }
    Ok(())
}

/// Sends a trade with the next request number and remembers it.
fn trade<M: Message>(
    ctx: &mut ModuleContext<'_>,
    kind: Trade,
    build: impl FnOnce(u32) -> M,
) -> Result<(), ModuleError> {
    let request = state(ctx)?.next_request.max(1);
    ctx.send(&build(request))?;
    let v = state(ctx)?;
    // Never 0, also after a wrap.
    v.next_request = request.wrapping_add(1).max(1);
    if v.pending.len() >= PENDING {
        v.pending.pop_front();
    }
    v.pending.push_back((request, kind));
    let v = v.clone();
    publish_requests(ctx, &v);
    clear_result(ctx);
    Ok(())
}

/// Closes the shop and forgets the vendor (the quantity stays).
fn close(ctx: &mut ModuleContext<'_>) -> Result<(), ModuleError> {
    let v = state(ctx)?;
    v.vendor = None;
    v.stock.clear();
    let v = v.clone();
    publish_stock(ctx, &v);
    clear_result(ctx);
    Ok(())
}

fn on_browse(ctx: &mut ModuleContext<'_>, payload: Option<&str>) -> Result<(), ModuleError> {
    let vendor: u32 = parse(payload)?;
    ctx.send(&Browse { vendor })
}

fn on_quantity(ctx: &mut ModuleContext<'_>, payload: Option<&str>) -> Result<(), ModuleError> {
    let quantity: u32 = parse(payload)?;
    if quantity == 0 {
        return Err(ModuleError::Invalid);
    }
    let v = state(ctx)?;
    v.quantity = quantity;
    // The purchase totals follow the quantity.
    let v = v.clone();
    publish_stock(ctx, &v);
    Ok(())
}

fn on_buy(ctx: &mut ModuleContext<'_>, payload: Option<&str>) -> Result<(), ModuleError> {
    let item: u32 = parse(payload)?;
    let v = state(ctx)?;
    let vendor = v.vendor.ok_or(ModuleError::Invalid)?;
    if !v.stock.iter().any(|(i, _, _)| *i == item) || v.quantity == 0 {
        return Err(ModuleError::Invalid);
    }
    let count = v.quantity;
    trade(ctx, Trade::Buy, |request| BuyItems {
        request,
        vendor,
        item,
        count,
    })
}

fn on_sell(ctx: &mut ModuleContext<'_>, payload: Option<&str>) -> Result<(), ModuleError> {
    if !ctx.flag("selling") {
        return Err(ModuleError::Invalid);
    }
    let slot: u8 = parse(payload)?;
    let v = state(ctx)?;
    let vendor = v.vendor.ok_or(ModuleError::Invalid)?;
    if usize::from(slot) >= SLOTS || v.quantity == 0 {
        return Err(ModuleError::Invalid);
    }
    let count = v.quantity;
    trade(ctx, Trade::Sell, |request| SellItems {
        request,
        vendor,
        slot,
        count,
    })
}

fn on_close(ctx: &mut ModuleContext<'_>, _payload: Option<&str>) -> Result<(), ModuleError> {
    close(ctx)
}

/// Runs at start and whenever the module is switched: publishes every starting value.
/// A disabled module refuses every intent, close included, so its shop closes here too.
/// The request counter carries on, so a late answer never matches a newer trade.
fn on_enabled(ctx: &mut ModuleContext<'_>, _enabled: bool) {
    if let Ok(v) = state(ctx) {
        *v = View::new(v.next_request);
        let v = v.clone();
        publish_stock(ctx, &v);
        publish_requests(ctx, &v);
    }
    clear_result(ctx);
}

impl ClientModule for Module {
    fn key(&self) -> &'static str {
        KEY
    }

    fn register(&self, r: &mut ClientRegistrar<'_>) -> Result<(), ClientRegistryError> {
        r.kinds(1050..=1059)?;
        r.state(View::new(1));
        r.on_message(Stock::ID.0, on_stock)?;
        r.on_message(Traded::ID.0, on_traded)?;
        r.on_intent("std.vendor.browse", on_browse)?;
        r.on_intent("std.vendor.quantity", on_quantity)?;
        r.on_intent("std.vendor.buy", on_buy)?;
        r.on_intent("std.vendor.sell", on_sell)?;
        r.on_intent("std.vendor.close", on_close)?;
        r.on_enabled(on_enabled);
        r.on_refused(on_refused);
        r.screen("std.vendor.shop", SHOP_SCREEN, Some(THEME))?;
        Ok(())
    }
}

//! std.auction client: the auction view model, the auction screen, and the auction
//! intents (plan 13).
//!
//! **View model** (bindable properties, all under `std.auction.`):
//!
//! | property | value |
//! |---|---|
//! | `listings` | list, the shown page, cheapest first as the server sends it; each item has `listing` (int listing id), `item` (int content id), `count` (int), `seller` (text: the seller's character), `price` and `deposit` (text: gold, verbatim from the server's `LotPage`), `expires_in` (text: the time left, as `2d 3h`, `4h 5m`, `6m 7s`, or `8s`), `own` (flag: the player's own listing, which offers Withdraw), and `other` (its negation, which offers Buy) |
//! | `searched` | whether a page has arrived |
//! | `query` | the item the shown page lists (int, 0 for any) |
//! | `mine` | whether the shown page lists only the player's own listings |
//! | `total` | listings matching the shown page's search (int) |
//! | `offset` | listings before the shown page (int) |
//! | `shown` | listings on the shown page (int) |
//! | `first`, `last` | the shown page's first and last position, counted from 1 (ints, 0 and 0 for an empty page) |
//! | `has_prev`, `has_next` | whether `std.auction.prev` and `std.auction.next` have a page to show |
//! | `has_rules` | whether the house rules arrived (with any page) |
//! | `cut_percent`, `deposit_percent` | the house cut and the deposit, in percent of the price (ints, from the last page) |
//! | `duration` | how long a listing stays up (text, formatted like `expires_in`) |
//! | `post_slot` | the bag slot to post from (int, -1 for none) |
//! | `has_post_slot` | whether a slot to post from is selected |
//! | `post_count` | how many items the post lists (int, at least 1) |
//! | `post_price` | the price the player entered for the lot (text, empty until entered) |
//! | `has_preview` | whether a price and the house rules are known, so the two below are shown |
//! | `post_proceeds` | gold the seller receives for a sale at `post_price`, after the house cut (text) |
//! | `post_deposit` | gold the post costs as a deposit, returned with a sale (text) |
//! | `confirming` | whether a purchase waits for its confirmation |
//! | `confirm_listing`, `confirm_item`, `confirm_count` | the armed purchase (ints) |
//! | `confirm_price`, `confirm_seller` | its price and seller (text, verbatim from the listing) |
//! | `result` | the outcome of the last answered command (text, empty until one arrives) |
//! | `result_ok` | whether that command succeeded |
//! | `pending` | commands sent and not yet answered (int) |
//! | `unmatched` | `AuctionResult`s that answered no pending command, ignored (int) |
//! | `untagged` | `LotPage`s that arrived without a `LotPageOf` before them, matched by order instead (int) |
//! | `open` | whether the auction screen is shown: false at start, toggled by `std.auction.toggle` |
//!
//! Every property gets its starting value from `on_enabled`, which the registry runs at
//! start. The registry adds `std.auction.enabled`, `std.auction.unavailable`,
//! `std.auction.flag.enabled` (the manifest's only flag), and `std.auction.refusal`; the
//! screen binds `enabled=` to them, so a disabled module shows as unavailable rather
//! than as dead buttons. The server answers every command with an `AuctionResult`, the
//! authoritative outcome shown as `result`. The module answers the server's refusals of
//! its commands and searches itself (the registry's `refusal` is left alone): a refused
//! command whose `AuctionResult` has arrived needs nothing more; one refused before any
//! result (a malformed request) marks the oldest pending command of that kind as failed
//! in `result`, with the refusal as its reason; a refused search forgets the oldest
//! search waiting for its page, so an untagged page (below) still matches by order.
//!
//! The screen also binds `std.containers.items`, the bag the `std.containers` client
//! half publishes (this module depends on containers), to offer stacks for posting.
//!
//! **Which listings are the player's.** A listing is the player's when its seller is the
//! character the registry publishes as `client.character` (read when a page arrives).
//! Before that is known, the listings of a `mine` page are the player's and no others.
//!
//! **Messages** (extension kinds from the contract):
//!
//! | message | way | what the module does |
//! |---|---|---|
//! | `SearchLots` | sent | asks for a page; remembered until a page answers it |
//! | `PostLot`, `BuyLot`, `CancelLot` | sent | a command with its request number, pending until its result |
//! | `LotPageOf` | received | the search the next `LotPage` answers (`item`, `mine`, `offset`); recorded until that page |
//! | `LotPage` | received | shown as the page of the search its `LotPageOf` named |
//! | `AuctionResult` | received | shown as `result` when it answers a pending command |
//!
//! **Which search a page answers.** The server sends a `LotPageOf` immediately before
//! every `LotPage`, echoing the `item`, `mine`, and `offset` of the search it answers; a
//! page that follows a requested command (the player's own listings, sent after its
//! `AuctionResult`) is tagged `mine` with item 0 and offset 0. The page is shown as that
//! search, whatever else is still waiting, and the echo is used once. The page's own
//! `offset` is the shown position. A `LotPage` with no `LotPageOf` before it is counted
//! in `untagged` and matched by order, as the server sends pages: the page after a
//! result is the `mine` view, any other answers the oldest search not yet answered
//! (tagged pages also take their search off that list, so the order stays right).
//!
//! **Intents**:
//!
//! - `std.auction.search` (payload: an item id; empty or absent searches everything)
//!   sends `SearchLots` from the first page, keeping the `mine` view as it is; opening
//!   the screen repeats the shown search and page.
//! - `std.auction.mine` switches between all listings and only the player's own, and
//!   searches again from the first page.
//! - `std.auction.next` and `std.auction.prev` ask for the next or previous page of the
//!   shown search (pages of [`std_auction_contract::PAGE`]).
//! - `std.auction.select` (payload: a bag slot), `std.auction.count` (payload: a
//!   positive number), and `std.auction.price` (payload: a positive number of gold) set
//!   up a post; `std.auction.post` sends `PostLot` and clears the slot and price.
//! - `std.auction.buy` (payload: a listing id from the shown page) arms a purchase;
//!   `std.auction.buy` again for the same listing, or without a payload, confirms and
//!   sends `BuyLot`. `std.auction.back` drops the armed purchase.
//! - `std.auction.cancel` (payload: a listing id) sends `CancelLot`; the server
//!   refuses listings that are not yours.
//!
//! Each command carries a request number from a counter the module keeps (never 0,
//! kept across a reset so a late answer never matches a newer command); its
//! `AuctionResult` is matched by that number.
//!
//! The server holds every rule (`RULES.md`): the house cut, deposits, listing limits,
//! durations, and who may buy or withdraw what. This half shows prices exactly as the
//! server sent them. The only arithmetic is the seller's preview, from the rules of the
//! last page: the cut is `price * cut_percent / 100` rounded down (as the server takes
//! it), so the proceeds `price - cut` round up; the deposit is
//! `price * deposit_percent / 100` rounded down. Both are computed without overflow for
//! every price. It only refuses requests that cannot mean anything (no slot selected,
//! no price, a zero count, an unknown listing, no page to turn to).

#![forbid(unsafe_code)]

use std::collections::VecDeque;

use mantis_client::input::device::KeyCode;
use mantis_client::modules::{
    ClientModule, ClientRegistrar, ClientRegistryError, ExtensionRefusal, ModuleContext, ModuleError, decode,
};
use mantis_core::wire::Message;
use mantis_ui::{ListItem, Value};
use std_auction_contract::{
    AUCTION_INVALID, AUCTION_MAILBOX_FULL, AUCTION_NO_GOLD, AUCTION_NO_ITEM, AUCTION_NOT_FOUND,
    AUCTION_NOT_YOURS, AUCTION_OWN_LISTING, AUCTION_TOO_MANY, AuctionResult, BuyLot, CancelLot, LotPage,
    LotPageOf, PAGE, PostLot, SearchLots,
};
use std_containers_contract::SLOTS;

/// The module key.
pub const KEY: &str = "std.auction";

/// The property the client registry publishes the player's character under (text).
pub const CHARACTER_PROPERTY: &str = "client.character";

/// The auction screen: search and paging, the listings with buy and withdraw buttons,
/// the purchase confirmation, and posting from the bag with the seller's preview.
pub const AUCTION_SCREEN: &str = r#"
panel id=std_auction_panel style=std_auction_panel visible="std.auction.open" enabled="std.auction.enabled" {
  text id=std_auction_heading text="Auction house"
  text id=std_auction_unavailable text="Auction house unavailable" visible="std.auction.unavailable"
  panel id=std_auction_browse enabled="std.auction.flag.enabled" {
    row id=std_auction_search_row gap=8 {
      input id=std_auction_query placeholder="Item to find (empty for all)" submit="std.auction.search"
      button id=std_auction_mine text="Mine or all" intent="std.auction.mine"
    }
    text id=std_auction_mine_text text="Your listings" visible="std.auction.mine"
    text id=std_auction_results template="{std.auction.first} to {std.auction.last} of {std.auction.total} listings" visible="std.auction.searched"
    list id=std_auction_listings bind="std.auction.listings" height=fit {
      row gap=4 {
        text template="Item {item.item} x {item.count}"
        text template="{item.price} gold"
        text template="seller {item.seller}, deposit {item.deposit}, {item.expires_in} left"
        button text="Buy" intent="std.auction.buy" payload="{item.listing}" visible="item.other"
        button text="Withdraw" intent="std.auction.cancel" payload="{item.listing}" visible="item.own"
      }
    }
    row id=std_auction_paging gap=8 {
      button id=std_auction_prev text="Previous" intent="std.auction.prev" enabled="std.auction.has_prev" disabled_text="Previous"
      button id=std_auction_next text="Next" intent="std.auction.next" enabled="std.auction.has_next" disabled_text="Next"
    }
  }
  panel id=std_auction_confirm visible="std.auction.confirming" {
    text id=std_auction_confirm_text template="Buy item {std.auction.confirm_item} x {std.auction.confirm_count} for {std.auction.confirm_price} gold from {std.auction.confirm_seller}?"
    row id=std_auction_confirm_buttons gap=8 {
      button id=std_auction_confirm_buy text="Confirm" intent="std.auction.buy"
      button id=std_auction_back text="Back" intent="std.auction.back"
    }
  }
  panel id=std_auction_sell enabled="std.auction.flag.enabled" {
    text id=std_auction_sell_heading text="Post from your bag"
    text id=std_auction_rules template="House cut {std.auction.cut_percent} percent, deposit {std.auction.deposit_percent} percent, listed for {std.auction.duration}" visible="std.auction.has_rules"
    list id=std_auction_bag bind="std.containers.items" height=fit {
      row gap=4 visible="item.filled" {
        text template="Item {item.item} x {item.count}"
        button text="Choose" intent="std.auction.select" payload="{item.slot}"
      }
    }
    text id=std_auction_post_slot template="Posting from slot {std.auction.post_slot}" visible="std.auction.has_post_slot"
    row id=std_auction_post_inputs gap=8 {
      input id=std_auction_count placeholder="How many" submit="std.auction.count"
      input id=std_auction_price placeholder="Price for the lot" submit="std.auction.price"
    }
    text id=std_auction_post_text template="{std.auction.post_count} for {std.auction.post_price} gold"
    text id=std_auction_preview template="You receive {std.auction.post_proceeds} gold after the cut; deposit {std.auction.post_deposit} gold, returned with a sale" visible="std.auction.has_preview"
    button id=std_auction_post text="Post" intent="std.auction.post" enabled="std.auction.has_post_slot" disabled_text="Choose a stack"
  }
  text id=std_auction_result bind="std.auction.result"
  text id=std_auction_refusal bind="std.auction.refusal"
}
"#;

/// Styles for the auction screen.
pub const THEME: &str = "
theme {
  style std_auction_panel { background = #1a1e26e0 radius = 6 padding = 8 gap = 4 width = 400 }
}
";

/// Searches sent and not yet answered, kept at most (older ones are forgotten).
const AWAITING: usize = 8;
/// Commands sent and not yet answered, kept at most (older ones are forgotten).
const PENDING: usize = 16;

/// The client half.
#[derive(Clone, Copy, Debug, Default)]
pub struct Module;

/// One listing of the shown page.
#[derive(Clone, Copy, Debug)]
struct Lot {
    listing: u32,
    seller: u64,
    item: u32,
    count: u32,
    price: u64,
    deposit: u64,
    expires_in: u32,
    own: bool,
}

/// What a search asks for.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
struct Query {
    item: u32,
    mine: bool,
}

impl Query {
    /// The player's own listings, any item: the page that follows a command.
    const MINE: Self = Self { item: 0, mine: true };
}

/// The house rules a page carries.
#[derive(Clone, Copy, Debug)]
struct Rules {
    cut_percent: u8,
    deposit_percent: u8,
    duration_seconds: u32,
}

/// A command sent and not yet answered.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Command {
    Post,
    Buy,
    Cancel,
}

/// View-model state.
#[derive(Clone, Debug)]
struct View {
    open: bool,
    /// The shown page's search.
    query: Query,
    offset: u16,
    total: u16,
    lots: Vec<Lot>,
    rules: Option<Rules>,
    post_slot: Option<u8>,
    post_count: u32,
    post_price: Option<u64>,
    /// The armed purchase.
    armed: Option<Lot>,
    /// The next request number (never 0).
    next_request: u32,
    /// Commands waiting for their `AuctionResult`, oldest first.
    pending: VecDeque<(u32, Command)>,
    /// Searches (and their offsets) waiting for their page, oldest first: the order an
    /// untagged page is matched by.
    awaiting: VecDeque<(Query, u16)>,
    /// An `AuctionResult` arrived and no page has followed it yet: an untagged page is
    /// the player's own listings.
    next_is_mine: bool,
    /// The last `LotPageOf`, waiting for its `LotPage`.
    tag: Option<(Query, u16)>,
    unmatched: u32,
    /// Pages that arrived without a `LotPageOf`.
    untagged: u32,
}

impl View {
    fn new(open: bool, next_request: u32) -> Self {
        Self {
            open,
            query: Query::default(),
            offset: 0,
            total: 0,
            lots: Vec::new(),
            rules: None,
            post_slot: None,
            post_count: 1,
            post_price: None,
            armed: None,
            next_request: next_request.max(1),
            pending: VecDeque::new(),
            awaiting: VecDeque::new(),
            next_is_mine: false,
            tag: None,
            unmatched: 0,
            untagged: 0,
        }
    }

    /// The search a page answers: the `LotPageOf` before it, used once, or (without one)
    /// the order the server answers in.
    fn answered(&mut self) -> Query {
        let Some((query, offset)) = self.tag.take() else {
            self.untagged = self.untagged.saturating_add(1);
            return if core::mem::take(&mut self.next_is_mine) {
                Query::MINE
            } else {
                self.awaiting.pop_front().map_or(self.query, |(q, _)| q)
            };
        };
        // Keep the order right for a later untagged page: the page after a command
        // answers it, any other answers its own search.
        if self.next_is_mine && query == Query::MINE && offset == 0 {
            self.next_is_mine = false;
        } else if let Some(i) = self.awaiting.iter().position(|a| *a == (query, offset)) {
            self.awaiting.remove(i);
        }
        query
    }

    fn has_next(&self) -> bool {
        u32::from(self.offset) + u32::try_from(self.lots.len()).unwrap_or(u32::MAX) < u32::from(self.total)
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

/// The player's character, as the registry publishes it.
fn character(ctx: &mut ModuleContext<'_>) -> Option<u64> {
    let props = ctx.props();
    let id = props.id(CHARACTER_PROPERTY)?;
    match props.get(id)? {
        Value::Text(t) => t.trim().parse().ok(),
        Value::Int(i) => u64::try_from(*i).ok(),
        _ => None,
    }
}

/// A span of seconds as text: `2d 3h`, `4h 5m`, `6m 7s`, or `8s`.
#[must_use]
pub fn duration_text(seconds: u32) -> String {
    let days = seconds / 86_400;
    let hours = seconds / 3_600 % 24;
    let minutes = seconds / 60 % 60;
    let secs = seconds % 60;
    if days > 0 {
        format!("{days}d {hours}h")
    } else if hours > 0 {
        format!("{hours}h {minutes}m")
    } else if minutes > 0 {
        format!("{minutes}m {secs}s")
    } else {
        format!("{secs}s")
    }
}

/// `price * percent / 100`, rounded down, without overflow.
fn percent_of(price: u64, percent: u8) -> u64 {
    let part = u128::from(price) * u128::from(percent) / 100;
    u64::try_from(part).unwrap_or(u64::MAX)
}

/// What a seller receives for a sale at `price` after a `cut_percent` house cut: the
/// cut is rounded down (as the server takes it), so the proceeds round up. A cut over
/// 100 percent leaves nothing.
#[must_use]
pub fn proceeds(price: u64, cut_percent: u8) -> u64 {
    price.saturating_sub(percent_of(price, cut_percent))
}

/// The deposit for a listing at `price`: `deposit_percent` of it, rounded down.
#[must_use]
pub fn deposit(price: u64, deposit_percent: u8) -> u64 {
    percent_of(price, deposit_percent)
}

/// The text a failed command shows for an `AuctionResult` reason.
#[must_use]
pub fn reason_text(reason: u8) -> &'static str {
    match reason {
        AUCTION_INVALID => "the request is not valid",
        AUCTION_TOO_MANY => "you have too many listings up",
        AUCTION_NO_GOLD => "not enough gold",
        AUCTION_MAILBOX_FULL => "a mailbox is full",
        AUCTION_OWN_LISTING => "it is your own listing",
        AUCTION_NOT_FOUND => "the listing is gone",
        AUCTION_NOT_YOURS => "it is not your listing",
        AUCTION_NO_ITEM => "the bag slot does not hold that many",
        _ => "refused",
    }
}

fn result_text(command: Command, m: &AuctionResult) -> String {
    match (command, m.ok) {
        (Command::Post, true) => format!("Posted listing {}, deposit {} gold", m.listing, m.gold),
        (Command::Buy, true) => format!("Bought listing {} for {} gold", m.listing, m.gold),
        (Command::Cancel, true) => format!("Withdrew listing {} ({} gold)", m.listing, m.gold),
        (Command::Post, false) => format!("Post failed: {}", reason_text(m.reason)),
        (Command::Buy, false) => format!("Purchase failed: {}", reason_text(m.reason)),
        (Command::Cancel, false) => format!("Withdrawal failed: {}", reason_text(m.reason)),
    }
}

/// The text a refusal shows as a reason.
fn refusal_text(reason: ExtensionRefusal) -> &'static str {
    match reason {
        ExtensionRefusal::NotAllowed => "not allowed by the server",
        ExtensionRefusal::Invalid => "the server could not read the request",
        ExtensionRefusal::FeatureDisabled => "the auction house is switched off",
        _ => "refused by the server",
    }
}

/// The command a refused kind was.
fn command_of(kind: u16) -> Option<Command> {
    match kind {
        k if k == PostLot::ID.0 => Some(Command::Post),
        k if k == BuyLot::ID.0 => Some(Command::Buy),
        k if k == CancelLot::ID.0 => Some(Command::Cancel),
        _ => None,
    }
}

fn lot_item(l: &Lot) -> ListItem {
    ListItem::new()
        .with("listing", Value::Int(i64::from(l.listing)))
        .with("seller", Value::Text(l.seller.to_string()))
        .with("item", Value::Int(i64::from(l.item)))
        .with("count", Value::Int(i64::from(l.count)))
        .with("price", Value::Text(l.price.to_string()))
        .with("deposit", Value::Text(l.deposit.to_string()))
        .with("expires_in", Value::Text(duration_text(l.expires_in)))
        .with("own", Value::Bool(l.own))
        .with("other", Value::Bool(!l.own))
}

fn publish_page(ctx: &mut ModuleContext<'_>, v: &View, searched: bool) {
    let items: Vec<ListItem> = v.lots.iter().map(lot_item).collect();
    let shown = i64::try_from(items.len()).unwrap_or(0);
    let offset = i64::from(v.offset);
    ctx.set("listings", Value::List(items));
    ctx.set_bool("searched", searched);
    ctx.set_int("query", i64::from(v.query.item));
    ctx.set_bool("mine", v.query.mine);
    ctx.set_int("total", i64::from(v.total));
    ctx.set_int("offset", offset);
    ctx.set_int("shown", shown);
    ctx.set_int("first", if shown > 0 { offset + 1 } else { 0 });
    ctx.set_int("last", if shown > 0 { offset + shown } else { 0 });
    ctx.set_bool("has_prev", v.offset > 0);
    ctx.set_bool("has_next", v.has_next());
    ctx.set_int("untagged", i64::from(v.untagged));
    let rules = v.rules;
    ctx.set_bool("has_rules", rules.is_some());
    ctx.set_int("cut_percent", rules.map_or(0, |r| i64::from(r.cut_percent)));
    ctx.set_int(
        "deposit_percent",
        rules.map_or(0, |r| i64::from(r.deposit_percent)),
    );
    ctx.set_text(
        "duration",
        &rules
            .map(|r| duration_text(r.duration_seconds))
            .unwrap_or_default(),
    );
}

fn publish_post(ctx: &mut ModuleContext<'_>, v: &View) {
    ctx.set_int("post_slot", v.post_slot.map_or(-1, i64::from));
    ctx.set_bool("has_post_slot", v.post_slot.is_some());
    ctx.set_int("post_count", i64::from(v.post_count));
    ctx.set_text(
        "post_price",
        &v.post_price.map(|p| p.to_string()).unwrap_or_default(),
    );
    let preview = v.post_price.zip(v.rules);
    ctx.set_bool("has_preview", preview.is_some());
    ctx.set_text(
        "post_proceeds",
        &preview
            .map(|(p, r)| proceeds(p, r.cut_percent).to_string())
            .unwrap_or_default(),
    );
    ctx.set_text(
        "post_deposit",
        &preview
            .map(|(p, r)| deposit(p, r.deposit_percent).to_string())
            .unwrap_or_default(),
    );
}

fn publish_armed(ctx: &mut ModuleContext<'_>, armed: Option<Lot>) {
    ctx.set_bool("confirming", armed.is_some());
    ctx.set_int("confirm_listing", armed.map_or(0, |l| i64::from(l.listing)));
    ctx.set_int("confirm_item", armed.map_or(0, |l| i64::from(l.item)));
    ctx.set_int("confirm_count", armed.map_or(0, |l| i64::from(l.count)));
    ctx.set_text(
        "confirm_price",
        &armed.map(|l| l.price.to_string()).unwrap_or_default(),
    );
    ctx.set_text(
        "confirm_seller",
        &armed.map(|l| l.seller.to_string()).unwrap_or_default(),
    );
}

fn publish_requests(ctx: &mut ModuleContext<'_>, v: &View) {
    ctx.set_int("pending", i64::try_from(v.pending.len()).unwrap_or(0));
    ctx.set_int("unmatched", i64::from(v.unmatched));
}

/// Publishes everything derived from the state.
fn publish_all(ctx: &mut ModuleContext<'_>, searched: bool) -> Result<(), ModuleError> {
    let v = state(ctx)?.clone();
    publish_page(ctx, &v, searched);
    publish_post(ctx, &v);
    publish_armed(ctx, v.armed);
    publish_requests(ctx, &v);
    Ok(())
}

/// Records the search the next `LotPage` answers.
fn on_page_of(ctx: &mut ModuleContext<'_>, _kind: u16, payload: &[u8]) -> Result<(), ModuleError> {
    let m: LotPageOf = decode(payload)?;
    state(ctx)?.tag = Some((
        Query {
            item: m.item,
            mine: m.mine,
        },
        m.offset,
    ));
    Ok(())
}

fn on_page(ctx: &mut ModuleContext<'_>, _kind: u16, payload: &[u8]) -> Result<(), ModuleError> {
    let m: LotPage = decode(payload)?;
    let me = character(ctx);
    let v = state(ctx)?;
    let query = v.answered();
    v.lots = m
        .entries
        .iter()
        .map(|e| Lot {
            listing: e.listing,
            seller: e.seller,
            item: e.item,
            count: e.count,
            price: e.price,
            deposit: e.deposit,
            expires_in: e.expires_in,
            own: me.map_or(query.mine, |c| c == e.seller),
        })
        .collect();
    v.query = query;
    v.offset = m.offset;
    v.total = m.total;
    v.rules = Some(Rules {
        cut_percent: m.cut_percent,
        deposit_percent: m.deposit_percent,
        duration_seconds: m.duration_seconds,
    });
    // The confirmation showed a listing from the old page; it may have changed.
    v.armed = None;
    publish_all(ctx, true)
}

fn on_result(ctx: &mut ModuleContext<'_>, _kind: u16, payload: &[u8]) -> Result<(), ModuleError> {
    let m: AuctionResult = decode(payload)?;
    let v = state(ctx)?;
    // Every result is followed by a page of the player's own listings (what an untagged
    // page is taken for; a tagged one names its search).
    v.next_is_mine = true;
    let matched = v
        .pending
        .iter()
        .position(|(r, _)| *r == m.request)
        .and_then(|i| v.pending.remove(i));
    let Some((_, command)) = matched else {
        v.unmatched = v.unmatched.saturating_add(1);
        let v = v.clone();
        publish_requests(ctx, &v);
        return Ok(());
    };
    let v = v.clone();
    publish_requests(ctx, &v);
    ctx.set_text("result", &result_text(command, &m));
    ctx.set_bool("result_ok", m.ok);
    Ok(())
}

/// Answers the server's refusals of the module's own sends (`request` is the session's
/// envelope number, not the module's, so the kind decides). A refused command still
/// waiting for its result is the oldest pending one of its kind: it failed. A refused
/// search never gets its page: the oldest waiting search is forgotten. A tagged page
/// needs none of this, but an untagged one is matched by that order: pages come in the
/// order the searches went out and tagged pages take theirs off the list, so the oldest
/// one left is the refused one.
fn on_refused(ctx: &mut ModuleContext<'_>, kind: u16, _request: u32, reason: ExtensionRefusal) -> bool {
    let Ok(v) = state(ctx) else {
        return false;
    };
    if kind == SearchLots::ID.0 {
        v.awaiting.pop_front();
        return true;
    }
    let Some(command) = command_of(kind) else {
        return false;
    };
    let failed = v
        .pending
        .iter()
        .position(|(_, c)| *c == command)
        .and_then(|i| v.pending.remove(i));
    let v = v.clone();
    publish_requests(ctx, &v);
    if let Some((_, command)) = failed {
        let what = match command {
            Command::Post => "Post",
            Command::Buy => "Purchase",
            Command::Cancel => "Withdrawal",
        };
        ctx.set_text("result", &format!("{what} failed: {}", refusal_text(reason)));
        ctx.set_bool("result_ok", false);
    }
    true
}

/// Sends a search and remembers it, so an untagged page is still recognised.
fn search(ctx: &mut ModuleContext<'_>, query: Query, offset: u16) -> Result<(), ModuleError> {
    ctx.send(&SearchLots {
        item: query.item,
        mine: query.mine,
        offset,
    })?;
    let v = state(ctx)?;
    if v.awaiting.len() >= AWAITING {
        v.awaiting.pop_front();
    }
    v.awaiting.push_back((query, offset));
    Ok(())
}

/// Sends a command with the next request number and remembers it.
fn command<M: Message>(
    ctx: &mut ModuleContext<'_>,
    kind: Command,
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
    Ok(())
}

fn on_search(ctx: &mut ModuleContext<'_>, payload: Option<&str>) -> Result<(), ModuleError> {
    let item: u32 = match payload.map(str::trim) {
        None | Some("") => 0,
        Some(p) => p.parse().map_err(|_| ModuleError::Invalid)?,
    };
    let mine = state(ctx)?.query.mine;
    search(ctx, Query { item, mine }, 0)
}

fn on_mine(ctx: &mut ModuleContext<'_>, _payload: Option<&str>) -> Result<(), ModuleError> {
    let query = state(ctx)?.query;
    search(
        ctx,
        Query {
            item: query.item,
            mine: !query.mine,
        },
        0,
    )
}

fn on_next(ctx: &mut ModuleContext<'_>, _payload: Option<&str>) -> Result<(), ModuleError> {
    let v = state(ctx)?;
    if !v.has_next() {
        return Err(ModuleError::Invalid);
    }
    let offset = u16::try_from(usize::from(v.offset) + PAGE).map_err(|_| ModuleError::Invalid)?;
    let query = v.query;
    search(ctx, query, offset)
}

fn on_prev(ctx: &mut ModuleContext<'_>, _payload: Option<&str>) -> Result<(), ModuleError> {
    let v = state(ctx)?;
    if v.offset == 0 {
        return Err(ModuleError::Invalid);
    }
    let back = u16::try_from(PAGE).unwrap_or(u16::MAX);
    let (query, offset) = (v.query, v.offset.saturating_sub(back));
    search(ctx, query, offset)
}

/// Edits the post set-up and republishes it.
fn edit_post(ctx: &mut ModuleContext<'_>, edit: impl FnOnce(&mut View)) -> Result<(), ModuleError> {
    let v = state(ctx)?;
    edit(v);
    let v = v.clone();
    publish_post(ctx, &v);
    Ok(())
}

fn on_select(ctx: &mut ModuleContext<'_>, payload: Option<&str>) -> Result<(), ModuleError> {
    let slot: u8 = parse(payload)?;
    if usize::from(slot) >= SLOTS {
        return Err(ModuleError::Invalid);
    }
    edit_post(ctx, |v| v.post_slot = Some(slot))
}

fn on_count(ctx: &mut ModuleContext<'_>, payload: Option<&str>) -> Result<(), ModuleError> {
    let count: u32 = parse(payload)?;
    if count == 0 {
        return Err(ModuleError::Invalid);
    }
    edit_post(ctx, |v| v.post_count = count)
}

fn on_price(ctx: &mut ModuleContext<'_>, payload: Option<&str>) -> Result<(), ModuleError> {
    let price: u64 = parse(payload)?;
    if price == 0 {
        return Err(ModuleError::Invalid);
    }
    edit_post(ctx, |v| v.post_price = Some(price))
}

fn on_post(ctx: &mut ModuleContext<'_>, _payload: Option<&str>) -> Result<(), ModuleError> {
    let v = state(ctx)?;
    let slot = v.post_slot.ok_or(ModuleError::Invalid)?;
    let price = v.post_price.ok_or(ModuleError::Invalid)?;
    let count = v.post_count;
    if count == 0 {
        return Err(ModuleError::Invalid);
    }
    command(ctx, Command::Post, |request| PostLot {
        request,
        slot,
        count,
        price,
    })?;
    edit_post(ctx, |v| {
        v.post_slot = None;
        v.post_price = None;
    })
}

fn confirm(ctx: &mut ModuleContext<'_>, listing: u32) -> Result<(), ModuleError> {
    command(ctx, Command::Buy, |request| BuyLot { request, listing })?;
    state(ctx)?.armed = None;
    publish_armed(ctx, None);
    Ok(())
}

fn on_buy(ctx: &mut ModuleContext<'_>, payload: Option<&str>) -> Result<(), ModuleError> {
    let wanted: Option<u32> = match payload {
        None => None,
        Some(p) => Some(p.trim().parse().map_err(|_| ModuleError::Invalid)?),
    };
    let v = state(ctx)?;
    match (v.armed, wanted) {
        // Confirm: the confirmation's own button, or the armed listing again.
        (Some(lot), None) => confirm(ctx, lot.listing),
        (Some(lot), Some(id)) if lot.listing == id => confirm(ctx, id),
        // Arm (or re-arm on another listing): nothing is sent yet.
        (_, Some(id)) => {
            let lot = v
                .lots
                .iter()
                .find(|l| l.listing == id)
                .copied()
                .ok_or(ModuleError::Invalid)?;
            v.armed = Some(lot);
            publish_armed(ctx, Some(lot));
            Ok(())
        }
        (None, None) => Err(ModuleError::Invalid),
    }
}

fn on_back(ctx: &mut ModuleContext<'_>, _payload: Option<&str>) -> Result<(), ModuleError> {
    state(ctx)?.armed = None;
    publish_armed(ctx, None);
    Ok(())
}

fn on_cancel(ctx: &mut ModuleContext<'_>, payload: Option<&str>) -> Result<(), ModuleError> {
    let listing: u32 = parse(payload)?;
    command(ctx, Command::Cancel, |request| CancelLot { request, listing })
}

fn on_toggle(ctx: &mut ModuleContext<'_>) {
    let (open, query, offset) = match state(ctx) {
        Ok(v) => {
            v.open = !v.open;
            (v.open, v.query, v.offset)
        }
        Err(_) => (false, Query::default(), 0),
    };
    ctx.set_bool("open", open);
    if open {
        // A full queue only means the shown page is older; searching again fixes it.
        let _ = search(ctx, query, offset);
    }
}

/// Runs at start and whenever the module is switched: publishes every starting value.
/// The screen stays as open or closed as it was, so a disabled auction says why; the
/// request counter carries on, so a late answer never matches a newer command.
fn on_enabled(ctx: &mut ModuleContext<'_>, _enabled: bool) {
    let open = match state(ctx) {
        Ok(v) => {
            *v = View::new(v.open, v.next_request);
            v.open
        }
        Err(_) => false,
    };
    ctx.set_bool("open", open);
    ctx.set_text("result", "");
    ctx.set_bool("result_ok", false);
    let _ = publish_all(ctx, false);
}

impl ClientModule for Module {
    fn key(&self) -> &'static str {
        KEY
    }

    fn register(&self, r: &mut ClientRegistrar<'_>) -> Result<(), ClientRegistryError> {
        r.kinds(1060..=1069)?;
        r.kinds(1160..=1169)?;
        r.state(View::new(false, 1));
        r.on_message(LotPageOf::ID.0, on_page_of)?;
        r.on_message(LotPage::ID.0, on_page)?;
        r.on_message(AuctionResult::ID.0, on_result)?;
        r.on_intent("std.auction.search", on_search)?;
        r.on_intent("std.auction.mine", on_mine)?;
        r.on_intent("std.auction.next", on_next)?;
        r.on_intent("std.auction.prev", on_prev)?;
        r.on_intent("std.auction.select", on_select)?;
        r.on_intent("std.auction.count", on_count)?;
        r.on_intent("std.auction.price", on_price)?;
        r.on_intent("std.auction.post", on_post)?;
        r.on_intent("std.auction.buy", on_buy)?;
        r.on_intent("std.auction.back", on_back)?;
        r.on_intent("std.auction.cancel", on_cancel)?;
        r.on_enabled(on_enabled);
        r.on_refused(on_refused);
        r.screen("std.auction.house", AUCTION_SCREEN, Some(THEME))?;
        r.action("std.auction.toggle", Some(KeyCode::H), on_toggle)?;
        Ok(())
    }
}

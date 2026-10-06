//! The std.auction client half against its contract codec: `LotPage` drives the view
//! model with the server's prices and paging, shown as the search the `LotPageOf` before
//! it names (by order when there is none); intents send `SearchLots` and the requested
//! commands (a purchase only after a confirmation); `AuctionResult` reports each command,
//! matched by its request; the module's flag and run-time refusals gate it.

use std::collections::BTreeMap;
use std::sync::Arc;

use mantis_client::input::device::KeyCode;
use mantis_client::modules::{ClientModule, ClientModules, ExtensionKind, ExtensionRefusal, IntentRoute};
use mantis_core::module::{Discovered, ModuleGraph, parse_manifest, parse_package, resolve};
use mantis_core::wire::{BoundedArray, Message, decode_message, encode_into};
use mantis_ui::{ListItem, Properties, Value};
use std_auction_client::{deposit, duration_text, proceeds};
use std_auction_contract::{
    AUCTION_INVALID, AUCTION_MAILBOX_FULL, AUCTION_NO_GOLD, AUCTION_NO_ITEM, AUCTION_NOT_FOUND,
    AUCTION_NOT_YOURS, AUCTION_OK, AUCTION_OWN_LISTING, AUCTION_TOO_MANY, AuctionResult, BuyLot, CancelLot,
    LotEntry, LotPage, LotPageOf, PAGE, PostLot, SearchLots,
};

type TestResult = Result<(), Box<dyn std::error::Error>>;

const MANIFEST: &str = include_str!("../../manifest.toml");
const CONTAINERS: &str = include_str!("../../../containers/manifest.toml");
const MAIL: &str = include_str!("../../../mail/manifest.toml");

/// The player's character in these tests, and another seller.
const ME: u64 = 70;
const OTHER: u64 = 80;

fn graph(flags: &str) -> Result<ModuleGraph, Box<dyn std::error::Error>> {
    let mut found = Vec::new();
    for text in [CONTAINERS, MAIL, MANIFEST] {
        found.push(Discovered {
            origin: "std".to_owned(),
            manifest: parse_manifest(text)?,
        });
    }
    let package = parse_package(&format!("[package]\nname = \"std\"\n[flags]\n{flags}"))?;
    Ok(resolve(&package, &found, &BTreeMap::new())?)
}

fn install(flags: &str) -> Result<(ClientModules, Properties), Box<dyn std::error::Error>> {
    let mut modules = ClientModules::new(
        &graph(flags)?,
        &[Arc::new(std_auction_client::Module) as Arc<dyn ClientModule>],
    )?;
    let mut props = Properties::new();
    modules.start(&mut props);
    Ok((modules, props))
}

fn deliver<M: Message>(m: &mut ClientModules, props: &mut Properties, msg: &M) {
    let mut bytes = Vec::new();
    encode_into(msg, &mut bytes);
    m.on_message(ExtensionKind(M::ID.0), &bytes, props);
}

/// A page as the server sends it: the `LotPageOf` naming its search (`item`, `mine`, and
/// the page's offset), then the page.
fn answer(m: &mut ClientModules, props: &mut Properties, (item, mine): (u32, bool), page: &LotPage) {
    let of = LotPageOf {
        item,
        mine,
        offset: page.offset,
    };
    deliver(m, props, &of);
    deliver(m, props, page);
}

fn prop(props: &mut Properties, name: &str) -> Option<Value> {
    let id = props.intern(name);
    props.get(id).cloned()
}

#[expect(clippy::unnecessary_wraps)] // Compared with `prop`, which is an `Option`.
fn text(s: &str) -> Option<Value> {
    Some(Value::Text(s.to_owned()))
}

#[expect(clippy::unnecessary_wraps)] // Compared with `prop`, which is an `Option`.
fn int(v: i64) -> Option<Value> {
    Some(Value::Int(v))
}

#[expect(clippy::unnecessary_wraps)] // Compared with `prop`, which is an `Option`.
fn flag(v: bool) -> Option<Value> {
    Some(Value::Bool(v))
}

fn sent(m: &mut ClientModules) -> Vec<(u16, Vec<u8>)> {
    let mut out = Vec::new();
    m.drain_outbound(|k, b| out.push((k.0, b.to_vec())));
    out
}

fn intent(m: &mut ClientModules, props: &mut Properties, name: &str, payload: Option<&str>) -> IntentRoute {
    m.on_intent(name, payload, props)
}

fn only<M: Message + std::fmt::Debug>(m: &mut ClientModules) -> Result<M, Box<dyn std::error::Error>> {
    let out = sent(m);
    let [(kind, bytes)] = out.as_slice() else {
        return Err(format!("expected one message, got {out:?}").into());
    };
    assert_eq!(*kind, M::ID.0);
    Ok(decode_message::<M>(bytes)?)
}

fn entry(listing: u32, seller: u64, price: u64) -> LotEntry {
    LotEntry {
        listing,
        seller,
        item: 11,
        count: 3,
        price,
        deposit: price / 10,
        expires_in: 3_725,
    }
}

/// A page of `entries` at `offset` of `total`, with a 5 percent cut, a 10 percent
/// deposit, and a day's duration.
fn page(total: u16, offset: u16, entries: &[LotEntry]) -> Result<LotPage, Box<dyn std::error::Error>> {
    Ok(LotPage {
        total,
        offset,
        cut_percent: 5,
        deposit_percent: 10,
        duration_seconds: 86_400,
        entries: BoundedArray::from_slice(entries).ok_or("entries")?,
    })
}

/// Listing 40 (90 gold) by another seller and listing 41 (a price past an int
/// property's range) by the player.
fn two() -> Result<LotPage, Box<dyn std::error::Error>> {
    page(2, 0, &[entry(40, OTHER, 90), entry(41, ME, u64::MAX)])
}

/// A full page of listings from `first`, all by another seller.
fn full(total: u16, offset: u16) -> Result<LotPage, Box<dyn std::error::Error>> {
    let entries: Vec<LotEntry> = (0..10u32)
        .map(|i| entry(100 + u32::from(offset) + i, OTHER, 10 + u64::from(i)))
        .collect();
    page(total, offset, &entries)
}

fn listings(props: &mut Properties) -> Result<Vec<ListItem>, Box<dyn std::error::Error>> {
    match prop(props, "std.auction.listings") {
        Some(Value::List(items)) => Ok(items),
        other => Err(format!("listings is not a list: {other:?}").into()),
    }
}

fn result(listing: u32, request: u32, reason: u8, gold: u64) -> AuctionResult {
    AuctionResult {
        request,
        ok: reason == AUCTION_OK,
        reason,
        listing,
        gold,
    }
}

#[test]
fn start_publishes_every_starting_value() -> TestResult {
    let (mut m, mut props) = install("")?;
    assert!(sent(&mut m).is_empty(), "searching is the player's choice");
    let expected = [
        ("std.auction.open", Value::Bool(false)),
        ("std.auction.unavailable", Value::Bool(false)),
        ("std.auction.listings", Value::List(Vec::new())),
        ("std.auction.searched", Value::Bool(false)),
        ("std.auction.query", Value::Int(0)),
        ("std.auction.mine", Value::Bool(false)),
        ("std.auction.total", Value::Int(0)),
        ("std.auction.offset", Value::Int(0)),
        ("std.auction.shown", Value::Int(0)),
        ("std.auction.first", Value::Int(0)),
        ("std.auction.last", Value::Int(0)),
        ("std.auction.has_prev", Value::Bool(false)),
        ("std.auction.has_next", Value::Bool(false)),
        ("std.auction.has_rules", Value::Bool(false)),
        ("std.auction.duration", Value::Text(String::new())),
        ("std.auction.post_slot", Value::Int(-1)),
        ("std.auction.has_post_slot", Value::Bool(false)),
        ("std.auction.post_count", Value::Int(1)),
        ("std.auction.post_price", Value::Text(String::new())),
        ("std.auction.has_preview", Value::Bool(false)),
        ("std.auction.post_proceeds", Value::Text(String::new())),
        ("std.auction.post_deposit", Value::Text(String::new())),
        ("std.auction.confirming", Value::Bool(false)),
        ("std.auction.confirm_listing", Value::Int(0)),
        ("std.auction.confirm_price", Value::Text(String::new())),
        ("std.auction.result", Value::Text(String::new())),
        ("std.auction.result_ok", Value::Bool(false)),
        ("std.auction.pending", Value::Int(0)),
        ("std.auction.unmatched", Value::Int(0)),
        ("std.auction.untagged", Value::Int(0)),
    ];
    for (name, value) in expected {
        assert_eq!(prop(&mut props, name), Some(value), "{name}");
    }
    let (_, mut props) = install("\"std.auction.enabled\" = false\n")?;
    assert_eq!(prop(&mut props, "std.auction.unavailable"), flag(true));
    assert_eq!(prop(&mut props, "std.auction.open"), flag(false));
    Ok(())
}

#[test]
fn a_page_shows_each_listing_as_the_server_sent_it() -> TestResult {
    let (mut m, mut props) = install("")?;
    assert_eq!(
        intent(&mut m, &mut props, "std.auction.search", Some(" 11 ")),
        IntentRoute::Handled
    );
    assert_eq!(
        only::<SearchLots>(&mut m)?,
        SearchLots {
            item: 11,
            mine: false,
            offset: 0
        }
    );
    for any in [None, Some(""), Some("  ")] {
        assert_eq!(
            intent(&mut m, &mut props, "std.auction.search", any),
            IntentRoute::Handled
        );
        assert_eq!(only::<SearchLots>(&mut m)?.item, 0, "{any:?} is any");
    }
    assert_eq!(
        intent(&mut m, &mut props, "std.auction.search", Some("sword")),
        IntentRoute::Refused
    );
    assert!(sent(&mut m).is_empty());

    answer(&mut m, &mut props, (11, false), &two()?);
    assert_eq!(prop(&mut props, "std.auction.searched"), flag(true));
    assert_eq!(
        prop(&mut props, "std.auction.query"),
        int(11),
        "the page answers the oldest search"
    );
    assert_eq!(prop(&mut props, "std.auction.total"), int(2));
    let fields: Vec<_> = listings(&mut props)?
        .iter()
        .map(|i| {
            [
                "listing",
                "seller",
                "item",
                "count",
                "price",
                "deposit",
                "expires_in",
            ]
            .map(|f| i.field(f).cloned())
        })
        .collect();
    assert_eq!(
        fields,
        [
            [
                int(40),
                text("80"),
                int(11),
                int(3),
                text("90"),
                text("9"),
                text("1h 2m")
            ],
            [
                int(41),
                text("70"),
                int(11),
                int(3),
                text("18446744073709551615"),
                text("1844674407370955161"),
                text("1h 2m")
            ],
        ],
        "prices and deposits are shown verbatim"
    );

    m.on_message(ExtensionKind(LotPage::ID.0), &[3], &mut props);
    assert_eq!(
        m.stats().malformed,
        1,
        "a malformed page is counted, never an error"
    );
    m.on_message(ExtensionKind(LotPageOf::ID.0), &[3], &mut props);
    assert_eq!(m.stats().malformed, 2, "and so is a malformed tag");
    assert_eq!(
        prop(&mut props, "std.auction.total"),
        int(2),
        "and changes nothing"
    );
    Ok(())
}

#[test]
fn paging_asks_for_the_next_and_previous_offsets() -> TestResult {
    let (mut m, mut props) = install("")?;
    assert_eq!(
        intent(&mut m, &mut props, "std.auction.next", None),
        IntentRoute::Refused,
        "no page yet"
    );
    assert_eq!(
        intent(&mut m, &mut props, "std.auction.search", Some("11")),
        IntentRoute::Handled
    );
    let _ = sent(&mut m);
    answer(&mut m, &mut props, (11, false), &full(25, 0)?);
    let at = |props: &mut Properties| {
        [
            "offset", "shown", "first", "last", "total", "has_prev", "has_next",
        ]
        .map(|f| prop(props, &format!("std.auction.{f}")))
    };
    assert_eq!(
        at(&mut props),
        [int(0), int(10), int(1), int(10), int(25), flag(false), flag(true)]
    );
    assert_eq!(
        intent(&mut m, &mut props, "std.auction.prev", None),
        IntentRoute::Refused,
        "already on the first page"
    );

    assert_eq!(
        intent(&mut m, &mut props, "std.auction.next", None),
        IntentRoute::Handled
    );
    assert_eq!(
        only::<SearchLots>(&mut m)?,
        SearchLots {
            item: 11,
            mine: false,
            offset: 10
        }
    );
    answer(&mut m, &mut props, (11, false), &full(25, 10)?);
    assert_eq!(
        at(&mut props),
        [
            int(10),
            int(10),
            int(11),
            int(20),
            int(25),
            flag(true),
            flag(true)
        ]
    );
    assert_eq!(
        intent(&mut m, &mut props, "std.auction.next", None),
        IntentRoute::Handled
    );
    assert_eq!(only::<SearchLots>(&mut m)?.offset, 20);
    let last: Vec<LotEntry> = (0..5u32).map(|i| entry(200 + i, OTHER, 50)).collect();
    answer(&mut m, &mut props, (11, false), &page(25, 20, &last)?);
    assert_eq!(
        at(&mut props),
        [
            int(20),
            int(5),
            int(21),
            int(25),
            int(25),
            flag(true),
            flag(false)
        ]
    );
    assert_eq!(
        intent(&mut m, &mut props, "std.auction.next", None),
        IntentRoute::Refused,
        "no page after the last"
    );
    assert!(sent(&mut m).is_empty());
    assert_eq!(
        intent(&mut m, &mut props, "std.auction.prev", None),
        IntentRoute::Handled
    );
    assert_eq!(
        only::<SearchLots>(&mut m)?,
        SearchLots {
            item: 11,
            mine: false,
            offset: 10
        }
    );
    assert_eq!(PAGE, 10, "pages are of ten listings");
    Ok(())
}

#[test]
fn the_mine_view_marks_the_players_listings() -> TestResult {
    let (mut m, mut props) = install("")?;
    // Before the character is known, a page that is not the mine view has nothing
    // the player could withdraw.
    answer(&mut m, &mut props, (0, false), &two()?);
    let owns = |props: &mut Properties| -> Result<Vec<_>, Box<dyn std::error::Error>> {
        Ok(listings(props)?
            .iter()
            .map(|i| (i.field("own").cloned(), i.field("other").cloned()))
            .collect())
    };
    assert_eq!(
        owns(&mut props)?,
        [(flag(false), flag(true)), (flag(false), flag(true))]
    );

    // The mine view: every listing in it is the player's.
    assert_eq!(
        intent(&mut m, &mut props, "std.auction.mine", None),
        IntentRoute::Handled
    );
    assert_eq!(
        only::<SearchLots>(&mut m)?,
        SearchLots {
            item: 0,
            mine: true,
            offset: 0
        }
    );
    answer(&mut m, &mut props, (0, true), &page(1, 0, &[entry(41, ME, 70)])?);
    assert_eq!(prop(&mut props, "std.auction.mine"), flag(true));
    assert_eq!(owns(&mut props)?, [(flag(true), flag(false))]);
    // A search keeps the view; toggling leaves it.
    assert_eq!(
        intent(&mut m, &mut props, "std.auction.search", Some("11")),
        IntentRoute::Handled
    );
    assert_eq!(
        only::<SearchLots>(&mut m)?,
        SearchLots {
            item: 11,
            mine: true,
            offset: 0
        }
    );
    answer(&mut m, &mut props, (11, true), &page(1, 0, &[entry(41, ME, 70)])?);
    assert_eq!(
        intent(&mut m, &mut props, "std.auction.mine", None),
        IntentRoute::Handled
    );
    assert_eq!(
        only::<SearchLots>(&mut m)?,
        SearchLots {
            item: 11,
            mine: false,
            offset: 0
        }
    );

    // Once the registry knows the character, each listing's seller decides.
    m.set_character(ME, &mut props);
    answer(&mut m, &mut props, (11, false), &two()?);
    assert_eq!(prop(&mut props, "std.auction.mine"), flag(false));
    assert_eq!(
        owns(&mut props)?,
        [(flag(false), flag(true)), (flag(true), flag(false))]
    );
    Ok(())
}

#[test]
fn the_post_screen_previews_proceeds_and_deposit() -> TestResult {
    assert_eq!(proceeds(1999, 5), 1900, "the cut (99.95) rounds down");
    assert_eq!(deposit(1999, 10), 199, "the deposit (199.9) rounds down");
    assert_eq!(proceeds(19, 5), 19, "a cut under one gold is nothing");
    assert_eq!(proceeds(500, 0), 500);
    assert_eq!(proceeds(500, 100), 0);
    assert_eq!(proceeds(500, 250), 0, "a cut past the price leaves nothing");
    assert_eq!(deposit(500, 0), 0);
    let big = u128::from(u64::MAX);
    assert_eq!(
        u128::from(proceeds(u64::MAX, 5)),
        big - big * 5 / 100,
        "exact for every price"
    );
    assert_eq!(u128::from(deposit(u64::MAX, 10)), big * 10 / 100);
    assert_eq!(duration_text(86_400), "1d 0h");
    assert_eq!(duration_text(90_061), "1d 1h");
    assert_eq!(duration_text(3_725), "1h 2m");
    assert_eq!(duration_text(61), "1m 1s");
    assert_eq!(duration_text(0), "0s");

    let (mut m, mut props) = install("")?;
    assert_eq!(
        intent(&mut m, &mut props, "std.auction.price", Some("1999")),
        IntentRoute::Handled
    );
    assert_eq!(
        prop(&mut props, "std.auction.has_preview"),
        flag(false),
        "no house rules yet"
    );
    answer(&mut m, &mut props, (0, false), &two()?);
    let rules = ["has_rules", "cut_percent", "deposit_percent", "duration"]
        .map(|f| prop(&mut props, &format!("std.auction.{f}")));
    assert_eq!(rules, [flag(true), int(5), int(10), text("1d 0h")]);
    let preview = ["has_preview", "post_proceeds", "post_deposit"]
        .map(|f| prop(&mut props, &format!("std.auction.{f}")));
    assert_eq!(preview, [flag(true), text("1900"), text("199")]);
    assert_eq!(
        intent(&mut m, &mut props, "std.auction.price", Some("100")),
        IntentRoute::Handled
    );
    assert_eq!(prop(&mut props, "std.auction.post_proceeds"), text("95"));
    assert_eq!(prop(&mut props, "std.auction.post_deposit"), text("10"));
    assert!(sent(&mut m).is_empty(), "the preview is local");
    Ok(())
}

#[test]
fn posting_needs_a_slot_and_a_price() -> TestResult {
    let (mut m, mut props) = install("")?;
    assert_eq!(
        intent(&mut m, &mut props, "std.auction.post", None),
        IntentRoute::Refused,
        "nothing chosen"
    );
    for bad in ["20", "x", "-1"] {
        assert_eq!(
            intent(&mut m, &mut props, "std.auction.select", Some(bad)),
            IntentRoute::Refused,
            "not a bag slot: {bad}"
        );
    }
    assert_eq!(
        intent(&mut m, &mut props, "std.auction.select", Some("4")),
        IntentRoute::Handled
    );
    assert_eq!(prop(&mut props, "std.auction.post_slot"), int(4));
    assert_eq!(prop(&mut props, "std.auction.has_post_slot"), flag(true));
    assert_eq!(
        intent(&mut m, &mut props, "std.auction.post", None),
        IntentRoute::Refused,
        "no price yet"
    );
    for (name, bad) in [
        ("std.auction.price", "0"),
        ("std.auction.price", "cheap"),
        ("std.auction.count", "0"),
        ("std.auction.count", "-2"),
    ] {
        assert_eq!(
            intent(&mut m, &mut props, name, Some(bad)),
            IntentRoute::Refused,
            "{name} {bad}"
        );
    }
    assert!(sent(&mut m).is_empty());
    assert_eq!(
        intent(&mut m, &mut props, "std.auction.count", Some("2")),
        IntentRoute::Handled
    );
    assert_eq!(
        intent(
            &mut m,
            &mut props,
            "std.auction.price",
            Some("18446744073709551615")
        ),
        IntentRoute::Handled
    );
    assert_eq!(
        prop(&mut props, "std.auction.post_price"),
        text("18446744073709551615")
    );
    assert_eq!(prop(&mut props, "std.auction.post_count"), int(2));
    assert_eq!(
        intent(&mut m, &mut props, "std.auction.post", None),
        IntentRoute::Handled
    );
    let post = only::<PostLot>(&mut m)?;
    assert_ne!(post.request, 0);
    assert_eq!((post.slot, post.count, post.price), (4, 2, u64::MAX));
    assert_eq!(prop(&mut props, "std.auction.pending"), int(1));
    assert_eq!(
        prop(&mut props, "std.auction.has_post_slot"),
        flag(false),
        "a sent post clears the slot"
    );
    assert_eq!(
        prop(&mut props, "std.auction.post_price"),
        text(""),
        "and the price"
    );
    assert_eq!(
        intent(&mut m, &mut props, "std.auction.post", None),
        IntentRoute::Refused
    );
    Ok(())
}

#[test]
fn buying_needs_a_confirmation() -> TestResult {
    let (mut m, mut props) = install("")?;
    assert_eq!(
        intent(&mut m, &mut props, "std.auction.buy", Some("40")),
        IntentRoute::Refused,
        "no listings yet"
    );
    answer(&mut m, &mut props, (0, false), &two()?);
    assert_eq!(
        intent(&mut m, &mut props, "std.auction.buy", None),
        IntentRoute::Refused,
        "nothing armed"
    );
    assert_eq!(
        intent(&mut m, &mut props, "std.auction.buy", Some("77")),
        IntentRoute::Refused,
        "not on the page"
    );

    // The first buy arms; nothing is sent.
    assert_eq!(
        intent(&mut m, &mut props, "std.auction.buy", Some("40")),
        IntentRoute::Handled
    );
    assert!(sent(&mut m).is_empty(), "arming sends nothing");
    let armed = ["confirming", "confirm_listing", "confirm_price", "confirm_seller"]
        .map(|f| prop(&mut props, &format!("std.auction.{f}")));
    assert_eq!(armed, [flag(true), int(40), text("90"), text("80")]);

    // Back disarms; another listing re-arms; a new page disarms.
    assert_eq!(
        intent(&mut m, &mut props, "std.auction.back", None),
        IntentRoute::Handled
    );
    assert_eq!(prop(&mut props, "std.auction.confirming"), flag(false));
    assert_eq!(
        intent(&mut m, &mut props, "std.auction.buy", Some("41")),
        IntentRoute::Handled
    );
    assert_eq!(
        intent(&mut m, &mut props, "std.auction.buy", Some("40")),
        IntentRoute::Handled
    );
    assert_eq!(
        prop(&mut props, "std.auction.confirm_listing"),
        int(40),
        "re-armed on the other listing"
    );
    answer(&mut m, &mut props, (0, false), &two()?);
    assert_eq!(
        prop(&mut props, "std.auction.confirming"),
        flag(false),
        "a new page disarms"
    );
    assert!(sent(&mut m).is_empty());

    // Arm, then confirm with the confirmation's button (no payload).
    assert_eq!(
        intent(&mut m, &mut props, "std.auction.buy", Some("40")),
        IntentRoute::Handled
    );
    assert_eq!(
        intent(&mut m, &mut props, "std.auction.buy", None),
        IntentRoute::Handled
    );
    let first = only::<BuyLot>(&mut m)?;
    assert_eq!(first.listing, 40);
    assert_eq!(prop(&mut props, "std.auction.confirming"), flag(false));

    // Arm, then confirm by pressing the same listing again.
    assert_eq!(
        intent(&mut m, &mut props, "std.auction.buy", Some("40")),
        IntentRoute::Handled
    );
    assert_eq!(
        intent(&mut m, &mut props, "std.auction.buy", Some("40")),
        IntentRoute::Handled
    );
    let second = only::<BuyLot>(&mut m)?;
    assert_eq!(second.listing, 40);
    assert!(first.request != 0 && second.request != 0);
    assert_ne!(first.request, second.request, "each purchase has its own request");
    Ok(())
}

#[test]
fn withdrawing_sends_cancel_lot() -> TestResult {
    let (mut m, mut props) = install("")?;
    assert_eq!(
        intent(&mut m, &mut props, "std.auction.cancel", Some("41")),
        IntentRoute::Handled
    );
    let first = only::<CancelLot>(&mut m)?;
    assert_eq!(first.listing, 41);
    for bad in [Some("mine"), Some(""), None] {
        assert_eq!(
            intent(&mut m, &mut props, "std.auction.cancel", bad),
            IntentRoute::Refused,
            "{bad:?}"
        );
    }
    assert!(sent(&mut m).is_empty());
    assert_eq!(
        intent(&mut m, &mut props, "std.auction.cancel", Some("42")),
        IntentRoute::Handled
    );
    let second = only::<CancelLot>(&mut m)?;
    assert_eq!(second.listing, 42);
    assert!(first.request != 0 && second.request != 0);
    assert_ne!(first.request, second.request);
    Ok(())
}

#[test]
fn results_show_only_for_their_own_request() -> TestResult {
    let (mut m, mut props) = install("")?;
    let failures = [
        (AUCTION_INVALID, "the request is not valid"),
        (AUCTION_TOO_MANY, "you have too many listings up"),
        (AUCTION_NO_GOLD, "not enough gold"),
        (AUCTION_MAILBOX_FULL, "a mailbox is full"),
        (AUCTION_OWN_LISTING, "it is your own listing"),
        (AUCTION_NOT_FOUND, "the listing is gone"),
        (AUCTION_NOT_YOURS, "it is not your listing"),
        (AUCTION_NO_ITEM, "the bag slot does not hold that many"),
        (200, "refused"),
    ];
    let mut unmatched = 0;
    for (reason, why) in failures {
        assert_eq!(
            intent(&mut m, &mut props, "std.auction.cancel", Some("41")),
            IntentRoute::Handled
        );
        let request = only::<CancelLot>(&mut m)?.request;
        let before = prop(&mut props, "std.auction.result");
        // An answer to another request changes nothing and is counted.
        deliver(
            &mut m,
            &mut props,
            &result(0, request.wrapping_add(100), reason, 0),
        );
        unmatched += 1;
        assert_eq!(prop(&mut props, "std.auction.result"), before, "{reason}");
        assert_eq!(prop(&mut props, "std.auction.unmatched"), int(unmatched));
        deliver(&mut m, &mut props, &result(0, request, reason, 0));
        let shown = format!("Withdrawal failed: {why}");
        assert_eq!(prop(&mut props, "std.auction.result"), text(&shown), "{reason}");
        assert_eq!(prop(&mut props, "std.auction.result_ok"), flag(false));
        // Answered once: a repeat is ignored.
        deliver(&mut m, &mut props, &result(41, request, AUCTION_OK, 0));
        unmatched += 1;
        assert_eq!(prop(&mut props, "std.auction.result"), text(&shown));
        assert_eq!(prop(&mut props, "std.auction.pending"), int(0));
    }

    // Three commands in flight, answered out of order: each answer names its own.
    answer(&mut m, &mut props, (0, false), &two()?);
    assert_eq!(
        intent(&mut m, &mut props, "std.auction.select", Some("2")),
        IntentRoute::Handled
    );
    assert_eq!(
        intent(&mut m, &mut props, "std.auction.price", Some("500")),
        IntentRoute::Handled
    );
    assert_eq!(
        intent(&mut m, &mut props, "std.auction.post", None),
        IntentRoute::Handled
    );
    let post = only::<PostLot>(&mut m)?.request;
    assert_eq!(
        intent(&mut m, &mut props, "std.auction.buy", Some("40")),
        IntentRoute::Handled
    );
    assert_eq!(
        intent(&mut m, &mut props, "std.auction.buy", None),
        IntentRoute::Handled
    );
    let buy = only::<BuyLot>(&mut m)?.request;
    assert_eq!(
        intent(&mut m, &mut props, "std.auction.cancel", Some("41")),
        IntentRoute::Handled
    );
    let cancel = only::<CancelLot>(&mut m)?.request;
    assert_eq!(prop(&mut props, "std.auction.pending"), int(3));
    let requests = [post, buy, cancel];
    assert!(requests.iter().all(|r| *r != 0));
    assert!(post != buy && buy != cancel && post != cancel);

    deliver(&mut m, &mut props, &result(40, buy, AUCTION_OK, 90));
    assert_eq!(
        prop(&mut props, "std.auction.result"),
        text("Bought listing 40 for 90 gold")
    );
    assert_eq!(prop(&mut props, "std.auction.result_ok"), flag(true));
    deliver(&mut m, &mut props, &result(0, post, AUCTION_NO_GOLD, 0));
    assert_eq!(
        prop(&mut props, "std.auction.result"),
        text("Post failed: not enough gold")
    );
    assert_eq!(prop(&mut props, "std.auction.result_ok"), flag(false));
    deliver(&mut m, &mut props, &result(41, cancel, AUCTION_OK, 0));
    assert_eq!(
        prop(&mut props, "std.auction.result"),
        text("Withdrew listing 41 (0 gold)")
    );

    m.on_message(ExtensionKind(AuctionResult::ID.0), &[], &mut props);
    assert_eq!(m.stats().malformed, 1, "a malformed result is counted");
    assert_eq!(prop(&mut props, "std.auction.unmatched"), int(unmatched));
    Ok(())
}

#[test]
fn a_refusal_never_overwrites_the_result() -> TestResult {
    // The server refuses a failed command after its `AuctionResult`.
    let (mut m, mut props) = install("")?;
    assert_eq!(
        intent(&mut m, &mut props, "std.auction.select", Some("2")),
        IntentRoute::Handled
    );
    assert_eq!(
        intent(&mut m, &mut props, "std.auction.price", Some("500")),
        IntentRoute::Handled
    );
    assert_eq!(
        intent(&mut m, &mut props, "std.auction.post", None),
        IntentRoute::Handled
    );
    let post = only::<PostLot>(&mut m)?.request;
    deliver(&mut m, &mut props, &result(0, post, AUCTION_TOO_MANY, 0));
    assert_eq!(
        prop(&mut props, "std.auction.result"),
        text("Post failed: you have too many listings up")
    );
    for kind in [PostLot::ID.0, BuyLot::ID.0, CancelLot::ID.0] {
        m.on_refused_tracked(ExtensionKind(kind), 9, ExtensionRefusal::NotAllowed, &mut props);
        assert_eq!(
            prop(&mut props, "std.auction.result"),
            text("Post failed: you have too many listings up"),
            "{kind}"
        );
    }
    assert_eq!(
        prop(&mut props, "std.auction.refusal"),
        None,
        "the module answers its own refusals"
    );
    assert_eq!(prop(&mut props, "std.auction.result_ok"), flag(false));
    assert_eq!(m.stats().refusals_answered, 3);

    // A command refused before any result (the server could not read it) failed: the
    // oldest pending command of that kind, whatever envelope request the refusal names.
    answer(&mut m, &mut props, (0, false), &two()?);
    assert_eq!(
        intent(&mut m, &mut props, "std.auction.cancel", Some("41")),
        IntentRoute::Handled
    );
    let cancel = only::<CancelLot>(&mut m)?.request;
    assert_eq!(
        intent(&mut m, &mut props, "std.auction.buy", Some("40")),
        IntentRoute::Handled
    );
    assert_eq!(
        intent(&mut m, &mut props, "std.auction.buy", None),
        IntentRoute::Handled
    );
    let _ = sent(&mut m);
    assert_eq!(prop(&mut props, "std.auction.pending"), int(2));
    m.on_refused_tracked(
        ExtensionKind(BuyLot::ID.0),
        1,
        ExtensionRefusal::Invalid,
        &mut props,
    );
    assert_eq!(
        prop(&mut props, "std.auction.result"),
        text("Purchase failed: the server could not read the request")
    );
    assert_eq!(
        prop(&mut props, "std.auction.pending"),
        int(1),
        "the withdrawal waits on"
    );
    deliver(&mut m, &mut props, &result(41, cancel, AUCTION_OK, 0));
    assert_eq!(
        prop(&mut props, "std.auction.result"),
        text("Withdrew listing 41 (0 gold)")
    );
    assert_eq!(prop(&mut props, "std.auction.pending"), int(0));
    Ok(())
}

#[test]
fn a_refused_search_is_forgotten_so_pages_keep_matching() -> TestResult {
    let (mut m, mut props) = install("")?;
    for item in ["11", "12"] {
        assert_eq!(
            intent(&mut m, &mut props, "std.auction.search", Some(item)),
            IntentRoute::Handled
        );
    }
    let _ = sent(&mut m);
    // The first search is refused: its page never comes, so the next page answers the
    // second, tagged or not.
    m.on_refused_tracked(
        ExtensionKind(SearchLots::ID.0),
        3,
        ExtensionRefusal::Invalid,
        &mut props,
    );
    assert_eq!(prop(&mut props, "std.auction.refusal"), None);
    answer(&mut m, &mut props, (12, false), &two()?);
    assert_eq!(prop(&mut props, "std.auction.query"), int(12));

    // Untagged, the order decides: the refused search is no longer waiting.
    for item in ["13", "14"] {
        assert_eq!(
            intent(&mut m, &mut props, "std.auction.search", Some(item)),
            IntentRoute::Handled
        );
    }
    let _ = sent(&mut m);
    m.on_refused_tracked(
        ExtensionKind(SearchLots::ID.0),
        4,
        ExtensionRefusal::Invalid,
        &mut props,
    );
    deliver(&mut m, &mut props, &two()?);
    assert_eq!(prop(&mut props, "std.auction.query"), int(14));
    assert_eq!(prop(&mut props, "std.auction.untagged"), int(1));
    Ok(())
}

#[test]
fn the_page_after_a_result_is_the_players_own() -> TestResult {
    let (mut m, mut props) = install("")?;
    assert_eq!(
        intent(&mut m, &mut props, "std.auction.search", Some("11")),
        IntentRoute::Handled
    );
    let _ = sent(&mut m);
    answer(&mut m, &mut props, (11, false), &two()?);
    assert_eq!(prop(&mut props, "std.auction.mine"), flag(false));
    assert_eq!(
        intent(&mut m, &mut props, "std.auction.cancel", Some("41")),
        IntentRoute::Handled
    );
    let request = only::<CancelLot>(&mut m)?.request;
    // A search sent after the command is answered after the command's own page.
    assert_eq!(
        intent(&mut m, &mut props, "std.auction.search", Some("12")),
        IntentRoute::Handled
    );
    let _ = sent(&mut m);
    deliver(&mut m, &mut props, &result(41, request, AUCTION_OK, 0));
    answer(&mut m, &mut props, (0, true), &page(1, 0, &[entry(43, ME, 5)])?);
    assert_eq!(prop(&mut props, "std.auction.mine"), flag(true));
    assert_eq!(prop(&mut props, "std.auction.query"), int(0));
    let own: Vec<_> = listings(&mut props)?
        .iter()
        .map(|i| i.field("own").cloned())
        .collect();
    assert_eq!(own, [flag(true)]);
    answer(&mut m, &mut props, (12, false), &two()?);
    assert_eq!(prop(&mut props, "std.auction.mine"), flag(false));
    assert_eq!(prop(&mut props, "std.auction.query"), int(12));
    assert_eq!(prop(&mut props, "std.auction.untagged"), int(0));
    Ok(())
}

#[test]
fn a_tagged_page_shows_its_own_search_whatever_else_waits() -> TestResult {
    let (mut m, mut props) = install("")?;
    for item in ["11", "12"] {
        assert_eq!(
            intent(&mut m, &mut props, "std.auction.search", Some(item)),
            IntentRoute::Handled
        );
    }
    let _ = sent(&mut m);
    // A result arrives before the older search's page: by order the page would be the
    // mine view, but its tag says it answers the search for 11.
    deliver(&mut m, &mut props, &result(0, 9, AUCTION_OK, 0));
    answer(&mut m, &mut props, (11, false), &two()?);
    assert_eq!(prop(&mut props, "std.auction.query"), int(11));
    assert_eq!(prop(&mut props, "std.auction.mine"), flag(false));
    let own: Vec<_> = listings(&mut props)?
        .iter()
        .map(|i| i.field("own").cloned())
        .collect();
    assert_eq!(own, [flag(false), flag(false)], "not the mine view");
    assert_eq!(prop(&mut props, "std.auction.untagged"), int(0));

    // The tag is used once: the page after the result, tagged mine, then the newer
    // search's page, untagged, which the order still matches.
    answer(&mut m, &mut props, (0, true), &page(1, 0, &[entry(43, ME, 5)])?);
    assert_eq!(prop(&mut props, "std.auction.mine"), flag(true));
    deliver(&mut m, &mut props, &two()?);
    assert_eq!(prop(&mut props, "std.auction.query"), int(12));
    assert_eq!(prop(&mut props, "std.auction.mine"), flag(false));
    assert_eq!(prop(&mut props, "std.auction.untagged"), int(1));

    // A newer tag replaces an older one that never got its page.
    deliver(
        &mut m,
        &mut props,
        &LotPageOf {
            item: 30,
            mine: false,
            offset: 0,
        },
    );
    answer(&mut m, &mut props, (31, false), &two()?);
    assert_eq!(prop(&mut props, "std.auction.query"), int(31));
    Ok(())
}

#[test]
fn the_page_after_a_command_is_tagged_as_the_mine_view() -> TestResult {
    let (mut m, mut props) = install("")?;
    assert_eq!(
        intent(&mut m, &mut props, "std.auction.mine", None),
        IntentRoute::Handled
    );
    let _ = sent(&mut m);
    answer(&mut m, &mut props, (0, true), &full(25, 0)?);
    assert_eq!(
        intent(&mut m, &mut props, "std.auction.next", None),
        IntentRoute::Handled
    );
    let _ = sent(&mut m);
    answer(&mut m, &mut props, (0, true), &full(25, 10)?);
    assert_eq!(prop(&mut props, "std.auction.offset"), int(10));
    // The player asks for the next page, then withdraws a listing before it comes.
    assert_eq!(
        intent(&mut m, &mut props, "std.auction.next", None),
        IntentRoute::Handled
    );
    assert_eq!(only::<SearchLots>(&mut m)?.offset, 20);
    assert_eq!(
        intent(&mut m, &mut props, "std.auction.cancel", Some("105")),
        IntentRoute::Handled
    );
    let request = only::<CancelLot>(&mut m)?.request;
    // The server answers in order: the page, then the result and its mine page.
    let last: Vec<LotEntry> = (0..5u32).map(|i| entry(200 + i, ME, 50)).collect();
    answer(&mut m, &mut props, (0, true), &page(25, 20, &last)?);
    assert_eq!(prop(&mut props, "std.auction.offset"), int(20));
    deliver(&mut m, &mut props, &result(105, request, AUCTION_OK, 0));
    assert_eq!(
        prop(&mut props, "std.auction.result"),
        text("Withdrew listing 105 (0 gold)")
    );
    answer(&mut m, &mut props, (0, true), &page(24, 0, &[entry(43, ME, 5)])?);
    let shown = ["query", "mine", "offset", "total", "has_prev", "untagged"]
        .map(|f| prop(&mut props, &format!("std.auction.{f}")));
    assert_eq!(
        shown,
        [int(0), flag(true), int(0), int(24), flag(false), int(0)],
        "the mine view from the first page"
    );
    let own: Vec<_> = listings(&mut props)?
        .iter()
        .map(|i| i.field("own").cloned())
        .collect();
    assert_eq!(
        own,
        [flag(true)],
        "the player's own, before the character is known"
    );
    // Paging goes on from the mine view.
    assert_eq!(
        intent(&mut m, &mut props, "std.auction.mine", None),
        IntentRoute::Handled
    );
    assert_eq!(
        only::<SearchLots>(&mut m)?,
        SearchLots {
            item: 0,
            mine: false,
            offset: 0
        }
    );
    Ok(())
}

#[test]
fn an_untagged_page_falls_back_to_the_order_and_is_counted() -> TestResult {
    let (mut m, mut props) = install("")?;
    for item in ["11", "12"] {
        assert_eq!(
            intent(&mut m, &mut props, "std.auction.search", Some(item)),
            IntentRoute::Handled
        );
    }
    let _ = sent(&mut m);
    // The oldest search first.
    deliver(&mut m, &mut props, &two()?);
    assert_eq!(prop(&mut props, "std.auction.searched"), flag(true));
    assert_eq!(prop(&mut props, "std.auction.query"), int(11));
    assert_eq!(prop(&mut props, "std.auction.total"), int(2));
    assert_eq!(prop(&mut props, "std.auction.untagged"), int(1));
    // After a result, the player's own listings.
    assert_eq!(
        intent(&mut m, &mut props, "std.auction.cancel", Some("41")),
        IntentRoute::Handled
    );
    let request = only::<CancelLot>(&mut m)?.request;
    deliver(&mut m, &mut props, &result(41, request, AUCTION_OK, 0));
    deliver(&mut m, &mut props, &page(1, 0, &[entry(43, ME, 5)])?);
    assert_eq!(prop(&mut props, "std.auction.mine"), flag(true));
    assert_eq!(prop(&mut props, "std.auction.query"), int(0));
    assert_eq!(prop(&mut props, "std.auction.untagged"), int(2));
    // Then the search still waiting.
    deliver(&mut m, &mut props, &two()?);
    assert_eq!(prop(&mut props, "std.auction.query"), int(12));
    assert_eq!(prop(&mut props, "std.auction.mine"), flag(false));
    // Nothing waits: the page keeps the shown search.
    deliver(&mut m, &mut props, &full(25, 10)?);
    assert_eq!(prop(&mut props, "std.auction.query"), int(12));
    assert_eq!(prop(&mut props, "std.auction.offset"), int(10));
    assert_eq!(prop(&mut props, "std.auction.untagged"), int(4));
    assert_eq!(m.stats().malformed, 0, "an untagged page is no error");
    // A reset forgets the count.
    m.set_enabled("std.auction", false, &mut props);
    m.set_enabled("std.auction", true, &mut props);
    assert_eq!(prop(&mut props, "std.auction.untagged"), int(0));
    Ok(())
}

#[test]
fn the_request_counter_survives_a_reset() -> TestResult {
    let (mut m, mut props) = install("")?;
    assert_eq!(
        intent(&mut m, &mut props, "std.auction.cancel", Some("41")),
        IntentRoute::Handled
    );
    let before = only::<CancelLot>(&mut m)?.request;
    m.set_enabled("std.auction", false, &mut props);
    m.set_enabled("std.auction", true, &mut props);
    assert_eq!(prop(&mut props, "std.auction.pending"), int(0));
    assert_eq!(
        intent(&mut m, &mut props, "std.auction.cancel", Some("41")),
        IntentRoute::Handled
    );
    let after = only::<CancelLot>(&mut m)?.request;
    assert!(after != 0 && after != before, "a reset never reuses a request");
    deliver(&mut m, &mut props, &result(41, before, AUCTION_OK, 0));
    assert_eq!(
        prop(&mut props, "std.auction.result"),
        text(""),
        "a late answer from before the reset is ignored"
    );
    assert_eq!(prop(&mut props, "std.auction.unmatched"), int(1));
    Ok(())
}

#[test]
fn the_flag_and_refusals_gate_the_module() -> TestResult {
    let (mut m, mut props) = install("\"std.auction.enabled\" = false\n")?;
    answer(&mut m, &mut props, (0, false), &two()?);
    deliver(&mut m, &mut props, &result(0, 1, AUCTION_NO_GOLD, 0));
    assert_eq!(
        m.stats().disabled,
        3,
        "messages for a disabled module are dropped"
    );
    assert_eq!(prop(&mut props, "std.auction.total"), int(0));
    assert_eq!(prop(&mut props, "std.auction.result"), text(""));
    assert_eq!(
        intent(&mut m, &mut props, "std.auction.search", None),
        IntentRoute::Refused
    );
    assert!(sent(&mut m).is_empty());

    let (mut m, mut props) = install("")?;
    answer(&mut m, &mut props, (0, false), &two()?);
    m.on_refused(
        ExtensionKind(BuyLot::ID.0),
        ExtensionRefusal::NotAllowed,
        &mut props,
    );
    assert!(m.is_enabled("std.auction"), "not allowed keeps the module");
    assert_eq!(
        prop(&mut props, "std.auction.result"),
        text(""),
        "nothing was pending, so nothing failed"
    );
    assert_eq!(prop(&mut props, "std.auction.total"), int(2));
    m.on_refused(
        ExtensionKind(PostLot::ID.0),
        ExtensionRefusal::FeatureDisabled,
        &mut props,
    );
    assert_eq!(prop(&mut props, "std.auction.enabled"), flag(false));
    assert_eq!(prop(&mut props, "std.auction.unavailable"), flag(true));
    assert_eq!(
        prop(&mut props, "std.auction.listings"),
        Some(Value::List(Vec::new())),
        "the view model is reset"
    );
    assert_eq!(
        intent(&mut m, &mut props, "std.auction.cancel", Some("40")),
        IntentRoute::Refused
    );
    m.set_enabled("std.auction", true, &mut props);
    assert_eq!(
        intent(&mut m, &mut props, "std.auction.buy", Some("40")),
        IntentRoute::Refused,
        "re-enabled with no listings until the next search"
    );
    Ok(())
}

#[test]
fn the_auction_screen_composes_and_the_toggle_action_searches_again() -> TestResult {
    let (mut m, mut props) = install("")?;
    let (layout, theme) = m.compose(&["std.auction.house"]);
    let mut fonts = mantis_ui::FontLibrary::new();
    let latin = fonts.add_font(mantis_ui::test_font::latin())?;
    fonts.define_stack("ui", &[latin])?;
    let ui = mantis_ui::Ui::new(fonts, &layout, Some(&theme))?;
    for id in [
        "std_auction_post",
        "std_auction_next",
        "std_auction_prev",
        "std_auction_mine",
        "std_auction_preview",
    ] {
        assert!(ui.rect_of(id).is_some(), "{id}");
    }
    let actions: Vec<_> = m.actions().map(|a| (a.name, a.default_key)).collect();
    assert_eq!(actions, [("std.auction.toggle", Some(KeyCode::H))]);

    assert_eq!(
        intent(&mut m, &mut props, "std.auction.search", Some("11")),
        IntentRoute::Handled
    );
    let _ = sent(&mut m);
    answer(&mut m, &mut props, (11, false), &full(25, 10)?);
    m.on_actions(|name| name == "std.auction.toggle", &mut props);
    assert_eq!(prop(&mut props, "std.auction.open"), flag(true));
    assert_eq!(
        only::<SearchLots>(&mut m)?,
        SearchLots {
            item: 11,
            mine: false,
            offset: 10
        },
        "opening repeats the shown search and page"
    );
    m.on_actions(|name| name == "std.auction.toggle", &mut props);
    assert_eq!(prop(&mut props, "std.auction.open"), flag(false));
    assert!(sent(&mut m).is_empty(), "closing asks nothing");
    Ok(())
}

#[test]
fn a_disabled_auction_renders_unavailable_not_as_dead_buttons() -> TestResult {
    let (mut m, _) = install("")?;
    let (layout, theme) = m.compose(&["std.auction.house"]);
    let mut fonts = mantis_ui::FontLibrary::new();
    let latin = fonts.add_font(mantis_ui::test_font::latin())?;
    fonts.define_stack("ui", &[latin])?;
    // The registry writes straight into the UI's properties.
    let mut ui = mantis_ui::Ui::new(fonts, &layout, Some(&theme))?;
    m.start(ui.properties_mut());
    let _ = ui.frame([800.0, 600.0], 1.0);
    assert!(
        ui.rect_of("std_auction_panel").is_some_and(|r| r.h <= 0.0),
        "closed at start"
    );
    assert!(
        ui.rect_of("std_auction_confirm").is_some_and(|r| r.h <= 0.0),
        "no confirmation at start"
    );
    m.on_actions(|name| name == "std.auction.toggle", ui.properties_mut());
    answer(&mut m, ui.properties_mut(), (0, false), &full(25, 0)?);
    // The bag the containers half would publish: one stack to post.
    let bag = ui.properties_mut().intern("std.containers.items");
    let _ = ui.properties_mut().set(
        bag,
        Value::List(vec![
            ListItem::new()
                .with("slot", Value::Int(0))
                .with("item", Value::Int(11))
                .with("count", Value::Int(3))
                .with("filled", Value::Bool(true)),
        ]),
    );
    let _ = ui.frame([800.0, 600.0], 1.0);
    assert_eq!(ui.is_enabled("std_auction_query"), Some(true));
    assert_eq!(ui.is_enabled("std_auction_next"), Some(true));
    assert_eq!(
        ui.is_enabled("std_auction_prev"),
        Some(false),
        "no page before the first"
    );
    assert_eq!(
        ui.is_enabled("std_auction_post"),
        Some(false),
        "no stack chosen yet"
    );
    assert_eq!(
        m.on_intent("std.auction.select", Some("0"), ui.properties_mut()),
        IntentRoute::Handled
    );
    let _ = ui.frame([800.0, 600.0], 1.0);
    assert_eq!(ui.is_enabled("std_auction_post"), Some(true));

    m.on_refused(
        ExtensionKind(SearchLots::ID.0),
        ExtensionRefusal::FeatureDisabled,
        ui.properties_mut(),
    );
    let _ = ui.frame([800.0, 600.0], 1.0);
    for id in [
        "std_auction_query",
        "std_auction_post",
        "std_auction_price",
        "std_auction_next",
        "std_auction_mine",
    ] {
        assert_eq!(ui.is_enabled(id), Some(false), "{id} shown as unavailable");
    }
    assert!(
        ui.rect_of("std_auction_unavailable").is_some_and(|r| r.h > 0.0),
        "the panel says why"
    );
    Ok(())
}

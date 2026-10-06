//! The std.vendor client half against its contract codec: `Stock` opens the shop with
//! the server's prices and the purchase totals; buying and selling send `BuyItems` and
//! `SellItems` with the open vendor and a request number; `Traded` reports each trade,
//! matched by its request; the `selling` flag and run-time refusals gate it.

use std::collections::BTreeMap;
use std::sync::Arc;

use mantis_client::modules::{ClientModule, ClientModules, ExtensionKind, ExtensionRefusal, IntentRoute};
use mantis_core::module::{Discovered, ModuleGraph, parse_manifest, parse_package, resolve};
use mantis_core::wire::{BoundedArray, Message, decode_message, encode_into};
use mantis_ui::{ListItem, Properties, Value};
use std_vendor_contract::{
    Browse, BuyItems, SellItems, Stock, TRADE_BAG_FULL, TRADE_INSUFFICIENT, TRADE_MALFORMED,
    TRADE_NOT_BOUGHT, TRADE_NOT_LISTED, TRADE_NOTHING, TRADE_OK, TRADE_SELLING_OFF, Traded,
};

type TestResult = Result<(), Box<dyn std::error::Error>>;

const MANIFEST: &str = include_str!("../../manifest.toml");
const CONTAINERS: &str = include_str!("../../../containers/manifest.toml");

fn graph(flags: &str) -> Result<ModuleGraph, Box<dyn std::error::Error>> {
    let found = [
        Discovered {
            origin: "std".to_owned(),
            manifest: parse_manifest(CONTAINERS)?,
        },
        Discovered {
            origin: "std".to_owned(),
            manifest: parse_manifest(MANIFEST)?,
        },
    ];
    let package = parse_package(&format!("[package]\nname = \"std\"\n[flags]\n{flags}"))?;
    Ok(resolve(&package, &found, &BTreeMap::new())?)
}

fn install(flags: &str) -> Result<(ClientModules, Properties), Box<dyn std::error::Error>> {
    let mut modules = ClientModules::new(
        &graph(flags)?,
        &[Arc::new(std_vendor_client::Module) as Arc<dyn ClientModule>],
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

fn prop(props: &mut Properties, name: &str) -> Option<Value> {
    let id = props.intern(name);
    props.get(id).cloned()
}

#[expect(clippy::unnecessary_wraps)] // Compared with `prop`, which is an `Option`.
fn text(s: &str) -> Option<Value> {
    Some(Value::Text(s.to_owned()))
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

/// Vendor 3 lists a potion (item 11) at 25 gold, bought back at 6, and a sword
/// (item 12) at a price past an int property's range, which vendors do not buy (0).
fn stock() -> Result<Stock, Box<dyn std::error::Error>> {
    Ok(Stock {
        vendor: 3,
        items: BoundedArray::from_slice(&[11, 12]).ok_or("items")?,
        prices: BoundedArray::from_slice(&[25, u64::MAX]).ok_or("prices")?,
        sell_prices: BoundedArray::from_slice(&[6, 0]).ok_or("sell prices")?,
    })
}

/// A `Traded` answer for `request`.
fn traded(request: u32, reason: u8, count: u32, total: u64) -> Traded {
    Traded {
        request,
        vendor: 3,
        ok: reason == TRADE_OK,
        reason,
        item: if reason == TRADE_OK { 11 } else { 0 },
        count,
        total,
    }
}

/// Buys item 11 from the open shop and returns the request it carried.
fn buy(m: &mut ClientModules, props: &mut Properties) -> Result<u32, Box<dyn std::error::Error>> {
    assert_eq!(
        intent(m, props, "std.vendor.buy", Some("11")),
        IntentRoute::Handled
    );
    Ok(only::<BuyItems>(m)?.request)
}

#[test]
fn start_publishes_every_starting_value() -> TestResult {
    let (_, mut props) = install("")?;
    let expected = [
        ("std.vendor.open", Value::Bool(false)),
        ("std.vendor.unavailable", Value::Bool(false)),
        ("std.vendor.vendor", Value::Int(0)),
        ("std.vendor.listings", Value::Int(0)),
        ("std.vendor.stock", Value::List(Vec::new())),
        ("std.vendor.quantity", Value::Int(1)),
        ("std.vendor.result", Value::Text(String::new())),
        ("std.vendor.result_ok", Value::Bool(false)),
        ("std.vendor.pending", Value::Int(0)),
        ("std.vendor.unmatched", Value::Int(0)),
        ("std.vendor.flag.selling", Value::Bool(true)),
    ];
    for (name, value) in expected {
        assert_eq!(prop(&mut props, name), Some(value), "{name}");
    }
    let (_, mut props) = install("\"std.vendor.enabled\" = false\n")?;
    assert_eq!(
        prop(&mut props, "std.vendor.unavailable"),
        Some(Value::Bool(true)),
        "a module disabled at start says so"
    );
    assert_eq!(prop(&mut props, "std.vendor.open"), Some(Value::Bool(false)));
    Ok(())
}

#[test]
fn stock_opens_the_shop_with_the_server_prices_and_purchase_totals() -> TestResult {
    let (mut m, mut props) = install("")?;
    deliver(&mut m, &mut props, &stock()?);
    assert_eq!(prop(&mut props, "std.vendor.open"), Some(Value::Bool(true)));
    assert_eq!(prop(&mut props, "std.vendor.vendor"), Some(Value::Int(3)));
    assert_eq!(prop(&mut props, "std.vendor.listings"), Some(Value::Int(2)));
    let fields = |props: &mut Properties| -> Result<Vec<_>, Box<dyn std::error::Error>> {
        let Some(Value::List(listings)) = prop(props, "std.vendor.stock") else {
            return Err("stock is not a list".into());
        };
        Ok(listings
            .iter()
            .map(|l| {
                (
                    l.field("item").cloned(),
                    l.field("price").cloned(),
                    l.field("sell_price").cloned(),
                    l.field("total").cloned(),
                )
            })
            .collect())
    };
    assert_eq!(
        fields(&mut props)?,
        [
            (Some(Value::Int(11)), text("25"), text("6"), text("25")),
            (
                Some(Value::Int(12)),
                text("18446744073709551615"),
                text("0"),
                text("18446744073709551615")
            ),
        ],
        "both prices are shown verbatim; the total is one of each"
    );

    // The totals follow the quantity, exactly, past the range of a gold amount.
    assert_eq!(
        intent(&mut m, &mut props, "std.vendor.quantity", Some("4")),
        IntentRoute::Handled
    );
    let past = (u128::from(u64::MAX) * 4).to_string();
    let totals: Vec<_> = fields(&mut props)?.into_iter().map(|f| f.3).collect();
    assert_eq!(totals, [text("100"), text(&past)]);
    assert!(sent(&mut m).is_empty(), "the preview is local");

    m.on_message(ExtensionKind(Stock::ID.0), &[9], &mut props);
    let uneven_prices = Stock {
        vendor: 4,
        items: BoundedArray::from_slice(&[11, 12]).ok_or("items")?,
        prices: BoundedArray::from_slice(&[25]).ok_or("prices")?,
        sell_prices: BoundedArray::from_slice(&[6, 0]).ok_or("sell prices")?,
    };
    let uneven_sell_prices = Stock {
        vendor: 4,
        items: BoundedArray::from_slice(&[11, 12]).ok_or("items")?,
        prices: BoundedArray::from_slice(&[25, 30]).ok_or("prices")?,
        sell_prices: BoundedArray::from_slice(&[6]).ok_or("sell prices")?,
    };
    deliver(&mut m, &mut props, &uneven_prices);
    deliver(&mut m, &mut props, &uneven_sell_prices);
    assert_eq!(
        m.stats().malformed,
        3,
        "a malformed or uneven stock is counted, never an error"
    );
    assert_eq!(
        prop(&mut props, "std.vendor.vendor"),
        Some(Value::Int(3)),
        "and changes nothing"
    );

    assert_eq!(
        intent(&mut m, &mut props, "std.vendor.close", None),
        IntentRoute::Handled
    );
    assert_eq!(prop(&mut props, "std.vendor.open"), Some(Value::Bool(false)));
    assert_eq!(
        prop(&mut props, "std.vendor.stock"),
        Some(Value::List(Vec::new()))
    );
    assert_eq!(
        prop(&mut props, "std.vendor.quantity"),
        Some(Value::Int(4)),
        "closing keeps the quantity"
    );
    assert!(sent(&mut m).is_empty(), "closing is local");
    Ok(())
}

#[test]
fn buying_and_selling_send_requested_trades_for_the_open_vendor() -> TestResult {
    let (mut m, mut props) = install("")?;
    assert_eq!(
        intent(&mut m, &mut props, "std.vendor.browse", Some("3")),
        IntentRoute::Handled
    );
    assert_eq!(only::<Browse>(&mut m)?, Browse { vendor: 3 });
    assert_eq!(
        intent(&mut m, &mut props, "std.vendor.browse", Some("vendor")),
        IntentRoute::Refused
    );
    assert_eq!(
        intent(&mut m, &mut props, "std.vendor.buy", Some("11")),
        IntentRoute::Refused,
        "no vendor open"
    );

    deliver(&mut m, &mut props, &stock()?);
    assert_eq!(
        intent(&mut m, &mut props, "std.vendor.buy", Some("11")),
        IntentRoute::Handled
    );
    let first = only::<BuyItems>(&mut m)?;
    assert_eq!(
        (first.vendor, first.item, first.count),
        (3, 11, 1),
        "the open vendor; the quantity starts at one"
    );
    for bad in ["0", "-1", "many", ""] {
        assert_eq!(
            intent(&mut m, &mut props, "std.vendor.quantity", Some(bad)),
            IntentRoute::Refused,
            "quantity {bad:?}"
        );
    }
    assert_eq!(
        intent(&mut m, &mut props, "std.vendor.quantity", Some("4")),
        IntentRoute::Handled
    );
    assert_eq!(prop(&mut props, "std.vendor.quantity"), Some(Value::Int(4)));
    for bad in [Some("99"), Some("item"), None] {
        assert_eq!(
            intent(&mut m, &mut props, "std.vendor.buy", bad),
            IntentRoute::Refused,
            "not listed by this vendor: {bad:?}"
        );
    }
    assert!(sent(&mut m).is_empty());
    assert_eq!(
        intent(&mut m, &mut props, "std.vendor.buy", Some("12")),
        IntentRoute::Handled
    );
    let second = only::<BuyItems>(&mut m)?;
    assert_eq!((second.vendor, second.item, second.count), (3, 12, 4));
    assert_eq!(
        intent(&mut m, &mut props, "std.vendor.sell", Some("5")),
        IntentRoute::Handled
    );
    let third = only::<SellItems>(&mut m)?;
    assert_eq!((third.vendor, third.slot, third.count), (3, 5, 4));

    let requests = [first.request, second.request, third.request];
    assert!(requests.iter().all(|r| *r != 0), "requests are never 0");
    assert!(
        requests.windows(2).all(|w| matches!(w, [a, b] if a != b)),
        "every trade has its own request: {requests:?}"
    );
    assert_eq!(prop(&mut props, "std.vendor.pending"), Some(Value::Int(3)));
    Ok(())
}

#[test]
fn traded_answers_show_only_for_their_own_request() -> TestResult {
    let (mut m, mut props) = install("")?;
    deliver(&mut m, &mut props, &stock()?);
    let failures = [
        (TRADE_INSUFFICIENT, "Purchase failed: not enough gold or items"),
        (TRADE_BAG_FULL, "Purchase failed: bag full"),
        (TRADE_NOT_LISTED, "Purchase failed: not for sale here"),
        (TRADE_NOTHING, "Purchase failed: nothing to sell"),
        (TRADE_NOT_BOUGHT, "Purchase failed: vendors do not buy it"),
        (TRADE_SELLING_OFF, "Purchase failed: selling is switched off"),
        (TRADE_MALFORMED, "Purchase failed: malformed request"),
        (200, "Purchase failed: refused"),
    ];
    let mut unmatched = 0;
    for (reason, shown) in failures {
        let request = buy(&mut m, &mut props)?;
        assert_eq!(
            prop(&mut props, "std.vendor.result"),
            text(""),
            "a new trade clears the old outcome"
        );
        // An answer to another request changes nothing and is counted.
        deliver(
            &mut m,
            &mut props,
            &traded(request.wrapping_add(100), reason, 1, 0),
        );
        unmatched += 1;
        assert_eq!(prop(&mut props, "std.vendor.result"), text(""), "{reason}");
        assert_eq!(
            prop(&mut props, "std.vendor.unmatched"),
            Some(Value::Int(unmatched))
        );
        deliver(&mut m, &mut props, &traded(request, reason, 1, 0));
        assert_eq!(prop(&mut props, "std.vendor.result"), text(shown), "{reason}");
        assert_eq!(prop(&mut props, "std.vendor.result_ok"), Some(Value::Bool(false)));
        // The answered request is no longer pending: a repeat is ignored.
        deliver(&mut m, &mut props, &traded(request, TRADE_OK, 1, 25));
        unmatched += 1;
        assert_eq!(prop(&mut props, "std.vendor.result"), text(shown));
        assert_eq!(prop(&mut props, "std.vendor.pending"), Some(Value::Int(0)));
    }

    // Two trades in flight, answered out of order: each answer names its own trade.
    assert_eq!(
        intent(&mut m, &mut props, "std.vendor.quantity", Some("2")),
        IntentRoute::Handled
    );
    let bought = buy(&mut m, &mut props)?;
    assert_eq!(
        intent(&mut m, &mut props, "std.vendor.sell", Some("0")),
        IntentRoute::Handled
    );
    let sold = only::<SellItems>(&mut m)?.request;
    deliver(&mut m, &mut props, &traded(sold, TRADE_OK, 2, 12));
    assert_eq!(
        prop(&mut props, "std.vendor.result"),
        text("Sold 2 of item 11 for 12 gold")
    );
    assert_eq!(prop(&mut props, "std.vendor.result_ok"), Some(Value::Bool(true)));
    deliver(&mut m, &mut props, &traded(bought, TRADE_OK, 2, 50));
    assert_eq!(
        prop(&mut props, "std.vendor.result"),
        text("Bought 2 of item 11 for 50 gold")
    );

    m.on_message(ExtensionKind(Traded::ID.0), &[], &mut props);
    assert_eq!(m.stats().malformed, 1, "a malformed answer is counted");
    assert_eq!(
        prop(&mut props, "std.vendor.result"),
        text("Bought 2 of item 11 for 50 gold"),
        "and changes nothing"
    );
    assert_eq!(
        prop(&mut props, "std.vendor.unmatched"),
        Some(Value::Int(unmatched))
    );
    Ok(())
}

#[test]
fn a_refusal_never_overwrites_the_answer() -> TestResult {
    let (mut m, mut props) = install("")?;
    deliver(&mut m, &mut props, &stock()?);
    assert_eq!(
        intent(&mut m, &mut props, "std.vendor.quantity", Some("2")),
        IntentRoute::Handled
    );
    // The server's refusal of a failed trade never overwrites its answer.
    let request = buy(&mut m, &mut props)?;
    deliver(&mut m, &mut props, &traded(request, TRADE_BAG_FULL, 2, 0));
    m.on_refused_tracked(
        ExtensionKind(BuyItems::ID.0),
        9,
        ExtensionRefusal::NotAllowed,
        &mut props,
    );
    assert_eq!(
        prop(&mut props, "std.vendor.result"),
        text("Purchase failed: bag full")
    );
    assert_eq!(
        prop(&mut props, "std.vendor.refusal"),
        None,
        "the module answers its own refusals"
    );
    assert_eq!(m.stats().refusals_answered, 1);

    // A trade refused before any answer (the server could not read it) failed: the
    // oldest pending trade of that kind, whatever envelope request the refusal names.
    let bought = buy(&mut m, &mut props)?;
    assert_eq!(
        intent(&mut m, &mut props, "std.vendor.sell", Some("0")),
        IntentRoute::Handled
    );
    let _ = sent(&mut m);
    m.on_refused_tracked(
        ExtensionKind(SellItems::ID.0),
        1,
        ExtensionRefusal::Invalid,
        &mut props,
    );
    assert_eq!(
        prop(&mut props, "std.vendor.result"),
        text("Sale failed: the server could not read the request")
    );
    assert_eq!(prop(&mut props, "std.vendor.pending"), Some(Value::Int(1)));
    deliver(&mut m, &mut props, &traded(bought, TRADE_OK, 2, 50));
    assert_eq!(
        prop(&mut props, "std.vendor.result"),
        text("Bought 2 of item 11 for 50 gold")
    );
    assert_eq!(prop(&mut props, "std.vendor.pending"), Some(Value::Int(0)));

    Ok(())
}

#[test]
fn the_request_counter_survives_a_reset() -> TestResult {
    let (mut m, mut props) = install("")?;
    deliver(&mut m, &mut props, &stock()?);
    let before = buy(&mut m, &mut props)?;
    m.set_enabled("std.vendor", false, &mut props);
    m.set_enabled("std.vendor", true, &mut props);
    assert_eq!(prop(&mut props, "std.vendor.pending"), Some(Value::Int(0)));
    deliver(&mut m, &mut props, &stock()?);
    let after = buy(&mut m, &mut props)?;
    assert_ne!(before, after, "a reset never reuses a request");
    assert_ne!(after, 0);
    deliver(&mut m, &mut props, &traded(before, TRADE_OK, 1, 25));
    assert_eq!(
        prop(&mut props, "std.vendor.result"),
        text(""),
        "a late answer from before the reset is ignored"
    );
    assert_eq!(prop(&mut props, "std.vendor.unmatched"), Some(Value::Int(1)));
    Ok(())
}

#[test]
fn selling_needs_an_open_shop_and_the_flag() -> TestResult {
    let (mut m, mut props) = install("")?;
    assert_eq!(
        prop(&mut props, "std.vendor.flag.selling"),
        Some(Value::Bool(true))
    );
    assert_eq!(
        intent(&mut m, &mut props, "std.vendor.sell", Some("5")),
        IntentRoute::Refused,
        "no vendor open"
    );
    deliver(&mut m, &mut props, &stock()?);
    assert_eq!(
        intent(&mut m, &mut props, "std.vendor.quantity", Some("2")),
        IntentRoute::Handled
    );
    for bad in ["20", "x", "300"] {
        assert_eq!(
            intent(&mut m, &mut props, "std.vendor.sell", Some(bad)),
            IntentRoute::Refused,
            "not a bag slot: {bad}"
        );
    }
    assert!(sent(&mut m).is_empty());
    assert_eq!(
        intent(&mut m, &mut props, "std.vendor.sell", Some("5")),
        IntentRoute::Handled
    );
    let sell = only::<SellItems>(&mut m)?;
    assert_eq!((sell.vendor, sell.slot, sell.count), (3, 5, 2));
    assert_ne!(sell.request, 0);

    let (mut m, mut props) = install("\"std.vendor.selling\" = false\n")?;
    assert_eq!(
        prop(&mut props, "std.vendor.flag.selling"),
        Some(Value::Bool(false))
    );
    deliver(&mut m, &mut props, &stock()?);
    assert_eq!(
        intent(&mut m, &mut props, "std.vendor.sell", Some("5")),
        IntentRoute::Refused,
        "selling is off"
    );
    // The server refuses a sale while selling is off as not allowed: the module stays,
    // and with no sale pending nothing is marked failed.
    m.on_refused(
        ExtensionKind(SellItems::ID.0),
        ExtensionRefusal::NotAllowed,
        &mut props,
    );
    assert!(m.is_enabled("std.vendor"));
    assert_eq!(prop(&mut props, "std.vendor.result"), text(""));
    assert_eq!(
        intent(&mut m, &mut props, "std.vendor.buy", Some("11")),
        IntentRoute::Handled,
        "buying is not"
    );
    assert_eq!(only::<BuyItems>(&mut m)?.item, 11);
    Ok(())
}

#[test]
fn the_flag_and_refusals_gate_the_module() -> TestResult {
    let (mut m, mut props) = install("\"std.vendor.enabled\" = false\n")?;
    assert_eq!(prop(&mut props, "std.vendor.enabled"), Some(Value::Bool(false)));
    deliver(&mut m, &mut props, &stock()?);
    deliver(&mut m, &mut props, &traded(1, TRADE_OK, 1, 25));
    assert_eq!(
        m.stats().disabled,
        2,
        "messages for a disabled module are dropped"
    );
    // Start-up published the empty (reset) view model; the dropped messages changed nothing.
    assert_eq!(
        prop(&mut props, "std.vendor.stock"),
        Some(Value::List(Vec::new()))
    );
    assert_eq!(prop(&mut props, "std.vendor.result"), text(""));
    assert_eq!(
        intent(&mut m, &mut props, "std.vendor.browse", Some("3")),
        IntentRoute::Refused
    );
    assert!(sent(&mut m).is_empty());

    let (mut m, mut props) = install("")?;
    deliver(&mut m, &mut props, &stock()?);
    assert_eq!(
        intent(&mut m, &mut props, "std.vendor.quantity", Some("7")),
        IntentRoute::Handled
    );
    m.on_refused(
        ExtensionKind(SellItems::ID.0),
        ExtensionRefusal::FeatureDisabled,
        &mut props,
    );
    assert_eq!(prop(&mut props, "std.vendor.enabled"), Some(Value::Bool(false)));
    assert_eq!(
        prop(&mut props, "std.vendor.unavailable"),
        Some(Value::Bool(true))
    );
    assert_eq!(
        prop(&mut props, "std.vendor.open"),
        Some(Value::Bool(false)),
        "the shop closes: its close button would be refused"
    );
    assert_eq!(prop(&mut props, "std.vendor.quantity"), Some(Value::Int(1)));
    assert_eq!(
        intent(&mut m, &mut props, "std.vendor.buy", Some("11")),
        IntentRoute::Refused
    );

    m.set_enabled("std.vendor", true, &mut props);
    assert_eq!(
        intent(&mut m, &mut props, "std.vendor.buy", Some("11")),
        IntentRoute::Refused,
        "re-enabled with no vendor open"
    );
    Ok(())
}

#[test]
fn the_shop_composes_and_shows_switched_off_controls_as_unavailable() -> TestResult {
    let (mut m, _) = install("\"std.vendor.selling\" = false\n")?;
    assert_eq!(m.actions().count(), 0, "the shop opens on a stock, not a key");
    let (layout, theme) = m.compose(&["std.vendor.shop"]);
    let mut fonts = mantis_ui::FontLibrary::new();
    let latin = fonts.add_font(mantis_ui::test_font::latin())?;
    fonts.define_stack("ui", &[latin])?;
    // The registry writes straight into the UI's properties.
    let mut ui = mantis_ui::Ui::new(fonts, &layout, Some(&theme))?;
    m.start(ui.properties_mut());
    let _ = ui.frame([800.0, 600.0], 1.0);
    assert!(
        ui.rect_of("std_vendor_panel").is_some_and(|r| r.h <= 0.0),
        "hidden before the first stock"
    );
    deliver(&mut m, ui.properties_mut(), &stock()?);
    // The bag the containers half would publish: one stack to sell.
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
    assert!(ui.rect_of("std_vendor_panel").is_some_and(|r| r.h > 0.0));
    assert_eq!(ui.is_enabled("std_vendor_buy"), Some(true));
    assert_eq!(ui.is_enabled("std_vendor_close"), Some(true));
    assert_eq!(
        ui.is_enabled("std_vendor_sell"),
        Some(false),
        "selling is off, so its controls are unavailable"
    );

    m.on_refused(
        ExtensionKind(Browse::ID.0),
        ExtensionRefusal::FeatureDisabled,
        ui.properties_mut(),
    );
    let _ = ui.frame([800.0, 600.0], 1.0);
    for id in ["std_vendor_buy", "std_vendor_close", "std_vendor_quantity"] {
        assert_eq!(ui.is_enabled(id), Some(false), "{id} shown as unavailable");
    }
    Ok(())
}

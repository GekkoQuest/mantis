//! std.vendor end to end: browsing, buying, selling back, atomicity, the
//! stock table, and the `selling` flag. Bags come from a stand-in
//! implementing the `std.containers` contract.

#![expect(clippy::unwrap_used)]

use std::sync::Arc;

use mantis_core::wire::{Message, decode_exact, decode_message};
use mantis_server::harness::{Harness, bytes_of};
use mantis_server::modules::{ExtensionKind, ExtensionRefusal, Registrar, RegistryError, ServerModule};
use std_containers_contract::{GOLD, Inventories, Ledger};
use std_vendor_contract::{Browse, BuyItems, STOCK_TABLE, SellItems, Stock, Traded};

const VENDOR: &str = include_str!("../../manifest.toml");
const BAGS: &str = "[module]\nkey = \"test.bags\"\nversion = \"0.1.0\"\ncontract = \"std.containers\"\n";
const STOCK: &[u8] = b"# vendor item price\n1 501 10\n1 502 400\n2 501 12\n";

/// Implements the std.containers contract: bags, with 100 gold for 11.
struct Bags;

impl ServerModule for Bags {
    fn key(&self) -> &'static str {
        "test.bags"
    }
    fn register(&self, r: &mut Registrar<'_>) -> Result<(), RegistryError> {
        let mut inv = Inventories::default();
        inv.transaction(&[11], |tx| tx.credit(11, 100))
            .map_err(|_| RegistryError::Module("seed"))?;
        r.resource(inv)
    }
}

fn bed(flags: &[(&str, bool)]) -> Harness {
    let mut h = Harness::new(
        &[Arc::new(std_vendor_server::Module), Arc::new(Bags)],
        &[VENDOR, BAGS],
        flags,
        &[(STOCK_TABLE, STOCK)],
    )
    .unwrap();
    h.join(1, 11, 0.0, 0.0);
    h.tick().unwrap();
    h
}

fn inv(h: &Harness) -> &Inventories {
    h.world().resource::<Inventories>().unwrap()
}

#[test]
fn browse_buy_and_sell_back() {
    let mut h = bed(&[]);
    h.send(1, Browse::ID.0, &bytes_of(&Browse { vendor: 1 }));
    h.tick().unwrap();
    let stock = decode_message::<Stock>(&h.take(1).messages[0].1).unwrap();
    assert_eq!(stock.items.iter().copied().collect::<Vec<_>>(), [501, 502]);
    assert_eq!(stock.prices.iter().copied().collect::<Vec<_>>(), [10, 400]);
    h.send(
        1,
        BuyItems::ID.0,
        &bytes_of(&BuyItems {
            request: 0,
            vendor: 1,
            item: 501,
            count: 3,
        }),
    );
    h.tick().unwrap();
    assert_eq!((inv(&h).gold(11), inv(&h).count_of(11, 501)), (70, 3));
    let ledger: Ledger = decode_exact(h.outcomes[0].payload.as_slice()).unwrap();
    assert_eq!((ledger.net(11, GOLD), ledger.net(11, 501)), (-30, 3));
    // Selling back pays the best listed price (12) over 4.
    h.send(
        1,
        SellItems::ID.0,
        &bytes_of(&SellItems {
            request: 0,
            vendor: 1,
            slot: 0,
            count: 2,
        }),
    );
    h.tick().unwrap();
    assert_eq!((inv(&h).gold(11), inv(&h).count_of(11, 501)), (76, 1));
    assert!(h.replay().unwrap() > 0);
}

#[test]
fn unaffordable_unknown_and_disabled_trades_change_nothing() {
    let mut h = bed(&[("std.vendor.selling", false)]);
    h.send(
        1,
        BuyItems::ID.0,
        &bytes_of(&BuyItems {
            request: 0,
            vendor: 1,
            item: 502,
            count: 1,
        }),
    );
    h.send(
        1,
        BuyItems::ID.0,
        &bytes_of(&BuyItems {
            request: 0,
            vendor: 2,
            item: 502,
            count: 1,
        }),
    );
    h.send(
        1,
        BuyItems::ID.0,
        &bytes_of(&BuyItems {
            request: 0,
            vendor: 1,
            item: 501,
            count: 0,
        }),
    );
    h.tick().unwrap();
    assert_eq!(inv(&h).gold(11), 100);
    h.send(
        1,
        BuyItems::ID.0,
        &bytes_of(&BuyItems {
            request: 0,
            vendor: 1,
            item: 501,
            count: 1,
        }),
    );
    h.tick().unwrap();
    h.send(
        1,
        SellItems::ID.0,
        &bytes_of(&SellItems {
            request: 0,
            vendor: 1,
            slot: 0,
            count: 1,
        }),
    );
    h.tick().unwrap();
    let reasons: Vec<ExtensionRefusal> = h.take(1).refusals.iter().map(|(_, r)| *r).collect();
    assert_eq!(
        reasons,
        [
            ExtensionRefusal::NotAllowed,
            ExtensionRefusal::NotAllowed,
            ExtensionRefusal::Invalid,
            ExtensionRefusal::NotAllowed
        ]
    );
    assert_eq!((inv(&h).gold(11), inv(&h).count_of(11, 501)), (90, 1));
}

#[test]
fn a_missing_or_malformed_stock_table_refuses_to_start() {
    let missing = Harness::new(
        &[Arc::new(std_vendor_server::Module), Arc::new(Bags)],
        &[VENDOR, BAGS],
        &[],
        &[],
    );
    assert!(missing.err().unwrap().contains(STOCK_TABLE));
    let bad = Harness::new(
        &[Arc::new(std_vendor_server::Module), Arc::new(Bags)],
        &[VENDOR, BAGS],
        &[],
        &[(STOCK_TABLE, b"1 two 3\n")],
    );
    assert!(bad.is_err());
    let _ = ExtensionKind(0);
}

#[test]
fn every_trade_reports_its_result_and_stock_shows_sell_prices() {
    let mut h = bed(&[]);
    h.send(1, Browse::ID.0, &bytes_of(&Browse { vendor: 1 }));
    h.tick().unwrap();
    let stock = decode_message::<Stock>(&h.take(1).messages[0].1).unwrap();
    assert_eq!(
        stock.sell_prices.iter().copied().collect::<Vec<_>>(),
        [3, 100],
        "best price / 4"
    );
    let results = |h: &mut Harness| -> Vec<(bool, u8)> {
        h.take(1)
            .messages
            .iter()
            .filter(|(k, _)| *k == ExtensionKind(Traded::ID.0))
            .map(|(_, b)| {
                let r = decode_message::<Traded>(b).unwrap();
                (r.ok, r.reason)
            })
            .collect()
    };
    h.send(
        1,
        BuyItems::ID.0,
        &bytes_of(&BuyItems {
            request: 0,
            vendor: 1,
            item: 501,
            count: 1,
        }),
    );
    h.send(
        1,
        BuyItems::ID.0,
        &bytes_of(&BuyItems {
            request: 0,
            vendor: 1,
            item: 502,
            count: 1,
        }),
    );
    h.send(
        1,
        BuyItems::ID.0,
        &bytes_of(&BuyItems {
            request: 0,
            vendor: 3,
            item: 501,
            count: 1,
        }),
    );
    h.send(
        1,
        SellItems::ID.0,
        &bytes_of(&SellItems {
            request: 0,
            vendor: 1,
            slot: 5,
            count: 1,
        }),
    );
    h.tick().unwrap();
    assert_eq!(results(&mut h), [(true, 0), (false, 1), (false, 3), (false, 4)]);
}

#[test]
fn requested_trades_answer_with_totals_and_reasons() {
    use std_vendor_contract::{TRADE_INSUFFICIENT, TRADE_NOT_LISTED, TRADE_OK};
    let mut h = bed(&[]);
    let buy = |request: u32, item: u32, count: u32| {
        bytes_of(&BuyItems {
            request,
            vendor: 1,
            item,
            count,
        })
    };
    h.send(1, BuyItems::ID.0, &buy(1, 501, 4));
    h.send(1, BuyItems::ID.0, &buy(2, 502, 1));
    h.send(1, BuyItems::ID.0, &buy(3, 999, 1));
    h.send(
        1,
        SellItems::ID.0,
        &bytes_of(&SellItems {
            request: 4,
            vendor: 1,
            slot: 0,
            count: 2,
        }),
    );
    h.ticks(2).unwrap();
    let got: Vec<Traded> = h
        .take(1)
        .messages
        .iter()
        .filter(|(k, _)| *k == ExtensionKind(Traded::ID.0))
        .map(|(_, b)| decode_message::<Traded>(b).unwrap())
        .collect();
    let seen: Vec<(u32, u8, u32, u64)> = got
        .iter()
        .map(|t| (t.request, t.reason, t.item, t.total))
        .collect();
    assert_eq!(
        seen,
        vec![
            (1, TRADE_OK, 501, 40),
            (2, TRADE_INSUFFICIENT, 0, 0),
            (3, TRADE_NOT_LISTED, 0, 0),
            (4, TRADE_OK, 501, 6),
        ]
    );
    assert!(got.iter().all(|t| t.vendor == 1));
    assert_eq!(inv(&h).gold(11), 100 - 40 + 6);
    assert!(h.replay().unwrap() > 0);
}

/// A count and a price whose product does not fit in u64: refused, and
/// nothing changes.
#[test]
fn a_purchase_whose_total_overflows_u64_is_refused() {
    use std_vendor_contract::TRADE_MALFORMED;
    let mut h = Harness::new(
        &[Arc::new(std_vendor_server::Module), Arc::new(Bags)],
        &[VENDOR, BAGS],
        &[],
        &[(STOCK_TABLE, b"1 501 9223372036854775807\n")],
    )
    .unwrap();
    h.join(1, 11, 0.0, 0.0);
    h.tick().unwrap();
    assert!(9_223_372_036_854_775_807u64.checked_mul(3).is_none());
    h.send(
        1,
        BuyItems::ID.0,
        &bytes_of(&BuyItems {
            request: 9,
            vendor: 1,
            item: 501,
            count: 3,
        }),
    );
    h.tick().unwrap();
    let got = h.take(1);
    let t = got
        .messages
        .iter()
        .find(|(k, _)| *k == ExtensionKind(Traded::ID.0))
        .map(|(_, b)| decode_message::<Traded>(b).unwrap())
        .unwrap();
    assert_eq!(
        (t.request, t.ok, t.reason, t.total),
        (9, false, TRADE_MALFORMED, 0)
    );
    assert_eq!((inv(&h).gold(11), inv(&h).count_of(11, 501)), (100, 0));
    assert!(h.outcomes.iter().all(|o| o.result.is_err()));
}

//! std.auction end to end: posting into escrow, searching, buying with the
//! house cut paid by mail, cancelling, expiry, refusals, and replay. Bags
//! and mailboxes come from stand-ins implementing their contracts.

#![allow(clippy::unwrap_used, clippy::too_many_lines)]

use std::sync::Arc;

use mantis_core::wire::{Message, decode_message};
use mantis_server::harness::{Harness, bytes_of};
use mantis_server::modules::{ExtensionKind, ExtensionRefusal, Registrar, RegistryError, ServerModule};
use std_auction_contract::{BuyLot, CancelLot, LotPage, PostLot, RULES_TABLE, SearchLots};
use std_containers_contract::Inventories;
use std_mail_contract::Mailboxes;

const AUCTION: &str = include_str!("../../manifest.toml");
const BAGS: &str = "[module]\nkey = \"test.bags\"\nversion = \"0.1.0\"\ncontract = \"std.containers\"\n";
const MAIL: &str = "[module]\nkey = \"test.mail\"\nversion = \"0.1.0\"\ncontract = \"std.mail\"\n";
const RULES: &[u8] = b"duration_seconds 2\ncut_percent 10\nmax_listings 2\n";

/// 11 has 20 of item 7; 12 has 500 gold.
struct Bags;

impl ServerModule for Bags {
    fn key(&self) -> &'static str {
        "test.bags"
    }
    fn register(&self, r: &mut Registrar<'_>) -> Result<(), RegistryError> {
        let mut inv = Inventories::default();
        inv.transaction(&[11, 12], |tx| {
            tx.add(11, 7, 20)?;
            tx.credit(12, 500)
        })
        .map_err(|_| RegistryError::Module("seed"))?;
        r.resource(inv)
    }
}

struct Mail;

impl ServerModule for Mail {
    fn key(&self) -> &'static str {
        "test.mail"
    }
    fn register(&self, r: &mut Registrar<'_>) -> Result<(), RegistryError> {
        r.resource(Mailboxes::default())
    }
}

fn bed() -> Harness {
    bed_with(RULES)
}

fn bed_with(rules: &[u8]) -> Harness {
    let mut h = Harness::new(
        &[
            Arc::new(std_auction_server::Module),
            Arc::new(Bags),
            Arc::new(Mail),
        ],
        &[AUCTION, BAGS, MAIL],
        &[],
        &[(RULES_TABLE, rules)],
    )
    .unwrap();
    h.join(1, 11, 0.0, 0.0);
    h.join(2, 12, 0.0, 0.0);
    h.tick().unwrap();
    h
}

fn post(h: &mut Harness, count: u32, price: u64) {
    h.send(
        1,
        PostLot::ID.0,
        &bytes_of(&PostLot {
            request: 0,
            slot: 0,
            count,
            price,
        }),
    );
}

fn listings(h: &mut Harness, s: u64) -> Vec<(u32, u32, u64)> {
    h.send(
        s,
        SearchLots::ID.0,
        &bytes_of(&SearchLots {
            item: 7,
            mine: false,
            offset: 0,
        }),
    );
    h.tick().unwrap();
    let got = h.take(s);
    let msg = got
        .messages
        .iter()
        .rev()
        .find(|(k, _)| *k == ExtensionKind(LotPage::ID.0))
        .unwrap();
    let l = decode_message::<LotPage>(&msg.1).unwrap();
    l.entries.iter().map(|e| (e.listing, e.count, e.price)).collect()
}

fn letters(h: &Harness, c: u64) -> Vec<(u64, Option<(u32, u32)>)> {
    h.world()
        .resource::<Mailboxes>()
        .unwrap()
        .letters(c)
        .iter()
        .map(|l| (l.gold, l.item.map(|s| (s.item, s.count))))
        .collect()
}

#[test]
fn post_search_buy_and_get_paid() {
    let mut h = bed();
    post(&mut h, 5, 100);
    post(&mut h, 3, 40);
    h.tick().unwrap();
    assert_eq!(
        h.world().resource::<Inventories>().unwrap().count_of(11, 7),
        12,
        "in escrow"
    );
    assert_eq!(listings(&mut h, 2), [(2, 3, 40), (1, 5, 100)], "cheapest first");
    h.send(
        2,
        BuyLot::ID.0,
        &bytes_of(&BuyLot {
            request: 0,
            listing: 1,
        }),
    );
    h.tick().unwrap();
    assert_eq!(h.world().resource::<Inventories>().unwrap().gold(12), 400);
    assert_eq!(
        letters(&h, 12),
        [(0, Some((7, 5)))],
        "the buyer gets the lot by mail"
    );
    assert_eq!(
        letters(&h, 11),
        [(90, None)],
        "the seller gets the price less 10%"
    );
    // Gone from the house; buying it again, or your own, is refused.
    h.send(
        2,
        BuyLot::ID.0,
        &bytes_of(&BuyLot {
            request: 0,
            listing: 1,
        }),
    );
    h.send(
        1,
        BuyLot::ID.0,
        &bytes_of(&BuyLot {
            request: 0,
            listing: 2,
        }),
    );
    h.tick().unwrap();
    assert_eq!(
        h.take(2).refusals,
        [(ExtensionKind(BuyLot::ID.0), ExtensionRefusal::NotAllowed)]
    );
    assert_eq!(
        h.take(1).refusals,
        [(ExtensionKind(BuyLot::ID.0), ExtensionRefusal::NotAllowed)]
    );
    assert!(h.replay().unwrap() > 0);
}

#[test]
fn cancel_limits_and_expiry_return_the_items() {
    let mut h = bed();
    post(&mut h, 1, 10);
    post(&mut h, 1, 10);
    post(&mut h, 1, 10); // over the limit of two
    h.tick().unwrap();
    assert_eq!(
        h.take(1).refusals,
        [(ExtensionKind(PostLot::ID.0), ExtensionRefusal::NotAllowed)]
    );
    h.send(
        2,
        CancelLot::ID.0,
        &bytes_of(&CancelLot {
            request: 0,
            listing: 1,
        }),
    ); // not the seller
    h.send(
        1,
        CancelLot::ID.0,
        &bytes_of(&CancelLot {
            request: 0,
            listing: 1,
        }),
    );
    h.tick().unwrap();
    assert_eq!(h.take(2).refusals.len(), 1);
    assert_eq!(letters(&h, 11), [(0, Some((7, 1)))]);
    // Two seconds later the other listing expires and comes back too.
    h.ticks(2 * 30 + 1).unwrap();
    assert_eq!(letters(&h, 11).len(), 2);
    assert!(listings(&mut h, 2).is_empty());
    // Posting too many, nothing, or for free is refused.
    h.send(
        1,
        PostLot::ID.0,
        &bytes_of(&PostLot {
            request: 0,
            slot: 0,
            count: 99,
            price: 1,
        }),
    );
    h.send(
        1,
        PostLot::ID.0,
        &bytes_of(&PostLot {
            request: 0,
            slot: 0,
            count: 1,
            price: 0,
        }),
    );
    h.tick().unwrap();
    let reasons: Vec<ExtensionRefusal> = h.take(1).refusals.iter().map(|(_, r)| *r).collect();
    assert_eq!(reasons, [ExtensionRefusal::NotAllowed, ExtensionRefusal::Invalid]);
}

/// Lead ruling for Milestone 6: every system change to durable economy
/// state emits a logged outcome in the same tick, so the persistence writer
/// sees it and replay re-checks it.
#[test]
fn expiry_emits_a_logged_outcome() {
    let mut h = bed();
    post(&mut h, 1, 10);
    h.tick().unwrap();
    let before = h.outcomes.len();
    h.ticks(2 * 30 + 1).unwrap();
    let new = &h.outcomes[before..];
    assert_eq!(new.len(), 1, "expiry returned items without an outcome record");
    let o = new[0];
    assert_eq!(o.kind, ExtensionKind(CancelLot::ID.0));
    assert_eq!(o.session, None);
    assert!(o.result.is_ok());
    let ledger = mantis_core::ledger::ledger_of(o.payload.as_slice()).unwrap();
    assert!(ledger.is_empty(), "the lot went to mail, not to a bag");
    // Replay re-derives the system outcome and checks it against the log.
    assert!(h.replay().unwrap() > 0);
}

fn answers(
    h: &mut Harness,
    s: u64,
) -> (
    Vec<std_auction_contract::AuctionResult>,
    Vec<std_auction_contract::LotPage>,
) {
    use std_auction_contract::{AuctionResult, LotPage};
    let got = h.take(s);
    let r = got
        .messages
        .iter()
        .filter(|(k, _)| *k == ExtensionKind(AuctionResult::ID.0))
        .map(|(_, b)| decode_message::<AuctionResult>(b).unwrap())
        .collect();
    let p = got
        .messages
        .iter()
        .filter(|(k, _)| *k == ExtensionKind(LotPage::ID.0))
        .map(|(_, b)| decode_message::<LotPage>(b).unwrap())
        .collect();
    (r, p)
}

#[test]
fn requested_trades_answer_with_reasons_pages_deposits_and_rules() {
    use std_auction_contract::{
        AUCTION_NO_GOLD, AUCTION_NOT_FOUND, AUCTION_NOT_YOURS, AUCTION_OK, AUCTION_OWN_LISTING,
        AUCTION_TOO_MANY,
    };
    let mut h = bed_with(
        b"duration_seconds 60
cut_percent 10
deposit_percent 10
max_listings 2
",
    );
    // 11 has no gold: a deposit cannot be paid.
    let lot = |request: u32, price: u64| {
        bytes_of(&PostLot {
            request,
            slot: 0,
            count: 2,
            price,
        })
    };
    h.send(1, PostLot::ID.0, &lot(1, 100));
    h.tick().unwrap();
    let (r, _) = answers(&mut h, 1);
    assert_eq!((r[0].request, r[0].ok, r[0].reason), (1, false, AUCTION_NO_GOLD));
    // Free listings (deposit 0 on a price of 9) need no gold.
    h.send(1, PostLot::ID.0, &lot(2, 9));
    h.send(1, PostLot::ID.0, &lot(3, 9));
    h.send(1, PostLot::ID.0, &lot(4, 9));
    h.tick().unwrap();
    let (r, pages) = answers(&mut h, 1);
    let codes: Vec<(u32, u8)> = r.iter().map(|x| (x.request, x.reason)).collect();
    assert_eq!(
        codes,
        vec![(2, AUCTION_OK), (3, AUCTION_OK), (4, AUCTION_TOO_MANY)]
    );
    let mine = pages.last().unwrap();
    assert_eq!(mine.total, 2);
    assert_eq!(
        (mine.cut_percent, mine.deposit_percent, mine.duration_seconds),
        (10, 10, 60)
    );
    let first = mine.entries.iter().next().copied().unwrap();
    assert_eq!(
        (first.seller, first.item, first.count, first.price, first.deposit),
        (11, 7, 2, 9, 0)
    );
    assert!(
        first.expires_in > 55 && first.expires_in <= 60,
        "{}",
        first.expires_in
    );
    let (a, b) = (r[0].listing, r[1].listing);

    // Withdrawing someone else's, buying your own, a missing listing.
    h.send(
        2,
        CancelLot::ID.0,
        &bytes_of(&CancelLot {
            request: 5,
            listing: a,
        }),
    );
    h.send(
        1,
        BuyLot::ID.0,
        &bytes_of(&BuyLot {
            request: 6,
            listing: a,
        }),
    );
    h.send(
        2,
        BuyLot::ID.0,
        &bytes_of(&BuyLot {
            request: 7,
            listing: 999,
        }),
    );
    h.send(
        2,
        BuyLot::ID.0,
        &bytes_of(&BuyLot {
            request: 8,
            listing: b,
        }),
    );
    h.tick().unwrap();
    let (r1, _) = answers(&mut h, 1);
    let (r2, _) = answers(&mut h, 2);
    assert_eq!((r1[0].request, r1[0].reason), (6, AUCTION_OWN_LISTING));
    let codes: Vec<(u32, u8, u64)> = r2.iter().map(|x| (x.request, x.reason, x.gold)).collect();
    assert_eq!(
        codes,
        vec![
            (5, AUCTION_NOT_YOURS, 0),
            (7, AUCTION_NOT_FOUND, 0),
            (8, AUCTION_OK, 9)
        ]
    );

    // Search: anyone's, or only mine.
    h.send(
        2,
        SearchLots::ID.0,
        &bytes_of(&SearchLots {
            item: 0,
            mine: false,
            offset: 0,
        }),
    );
    h.send(
        2,
        SearchLots::ID.0,
        &bytes_of(&SearchLots {
            item: 0,
            mine: true,
            offset: 0,
        }),
    );
    h.tick().unwrap();
    let got = h.take(2);
    let echoes: Vec<(u32, bool, u16)> = got
        .messages
        .iter()
        .filter(|(kind, _)| *kind == ExtensionKind(std_auction_contract::LotPageOf::ID.0))
        .map(|(_, bytes)| {
            let of = decode_message::<std_auction_contract::LotPageOf>(bytes).unwrap();
            (of.item, of.mine, of.offset)
        })
        .collect();
    assert_eq!(
        echoes,
        [(0, false, 0), (0, true, 0)],
        "each page names its search"
    );
    let pages: Vec<LotPage> = got
        .messages
        .iter()
        .filter(|(kind, _)| *kind == ExtensionKind(LotPage::ID.0))
        .map(|(_, bytes)| decode_message::<LotPage>(bytes).unwrap())
        .collect();
    assert_eq!((pages[0].total, pages[1].total), (1, 0));
    assert!(h.replay().unwrap() > 0);
}

/// 11 has 20 of item 7 and 100 gold; 12 has 500 gold.
struct RichBags;

impl ServerModule for RichBags {
    fn key(&self) -> &'static str {
        "test.bags"
    }
    fn register(&self, r: &mut Registrar<'_>) -> Result<(), RegistryError> {
        let mut inv = Inventories::default();
        inv.transaction(&[11, 12], |tx| {
            tx.add(11, 7, 20)?;
            tx.credit(11, 100)?;
            tx.credit(12, 500)
        })
        .map_err(|_| RegistryError::Module("seed"))?;
        r.resource(inv)
    }
}

#[test]
fn a_deposit_is_paid_on_posting_returned_with_a_sale_and_kept_on_withdrawal() {
    let mut h = Harness::new(
        &[
            Arc::new(std_auction_server::Module),
            Arc::new(RichBags),
            Arc::new(Mail),
        ],
        &[AUCTION, BAGS, MAIL],
        &[],
        &[(
            RULES_TABLE,
            b"duration_seconds 60
cut_percent 10
deposit_percent 10
max_listings 2
",
        )],
    )
    .unwrap();
    h.join(1, 11, 0.0, 0.0);
    h.join(2, 12, 0.0, 0.0);
    h.tick().unwrap();
    let post = |request: u32| {
        bytes_of(&PostLot {
            request,
            slot: 0,
            count: 1,
            price: 200,
        })
    };
    h.send(1, PostLot::ID.0, &post(1));
    h.send(1, PostLot::ID.0, &post(2));
    h.tick().unwrap();
    let (r, _) = answers(&mut h, 1);
    assert_eq!(
        r.iter().map(|x| (x.ok, x.gold)).collect::<Vec<_>>(),
        vec![(true, 20), (true, 20)]
    );
    let gold = |h: &Harness, c: u64| h.world().resource::<Inventories>().unwrap().gold(c);
    assert_eq!(gold(&h, 11), 100 - 40);
    h.send(
        2,
        BuyLot::ID.0,
        &bytes_of(&BuyLot {
            request: 3,
            listing: r[0].listing,
        }),
    );
    h.send(
        1,
        CancelLot::ID.0,
        &bytes_of(&CancelLot {
            request: 4,
            listing: r[1].listing,
        }),
    );
    h.tick().unwrap();
    assert_eq!(gold(&h, 12), 500 - 200);
    // The sale pays the price less the cut, plus the deposit, by mail; the
    // withdrawal returns the item and keeps the deposit.
    let mail = letters(&h, 11);
    assert!(mail.contains(&(200 - 20 + 20, None)), "{mail:?}");
    assert!(mail.contains(&(0, Some((7, 1)))), "{mail:?}");
    assert_eq!(mail.iter().map(|l| l.0).sum::<u64>(), 200);
    assert!(h.replay().unwrap() > 0);
}

/// 11 has an item; 12 has gold near the top of what a ledger records.
struct WealthyBags;

const FORTUNE: u64 = 9_000_000_000_000_000_000;

impl ServerModule for WealthyBags {
    fn key(&self) -> &'static str {
        "test.bags"
    }
    fn register(&self, r: &mut Registrar<'_>) -> Result<(), RegistryError> {
        let mut inv = Inventories::default();
        inv.transaction(&[11, 12], |tx| {
            tx.add(11, 7, 1)?;
            tx.credit(12, FORTUNE)
        })
        .map_err(|_| RegistryError::Module("seed"))?;
        r.resource(inv)
    }
}

/// The reported bug: `price * cut_percent` overflowed u64 for a price this
/// large. The cut is now exact, and nothing wraps.
#[test]
fn a_price_whose_cut_would_overflow_u64_takes_an_exact_cut() {
    use std_auction_contract::AUCTION_OK;
    let mut h = Harness::new(
        &[
            Arc::new(std_auction_server::Module),
            Arc::new(WealthyBags),
            Arc::new(Mail),
        ],
        &[AUCTION, BAGS, MAIL],
        &[],
        &[(RULES_TABLE, b"cut_percent 5\n")],
    )
    .unwrap();
    h.join(1, 11, 0.0, 0.0);
    h.join(2, 12, 0.0, 0.0);
    h.tick().unwrap();
    assert!(FORTUNE.checked_mul(5).is_none(), "the old multiply overflowed");
    h.send(
        1,
        PostLot::ID.0,
        &bytes_of(&PostLot {
            request: 1,
            slot: 0,
            count: 1,
            price: FORTUNE,
        }),
    );
    h.tick().unwrap();
    let (r, _) = answers(&mut h, 1);
    h.send(
        2,
        BuyLot::ID.0,
        &bytes_of(&BuyLot {
            request: 2,
            listing: r[0].listing,
        }),
    );
    h.tick().unwrap();
    let (r, _) = answers(&mut h, 2);
    assert_eq!((r[0].reason, r[0].gold), (AUCTION_OK, FORTUNE));
    let cut = FORTUNE / 100 * 5;
    assert!(
        letters(&h, 11).contains(&(FORTUNE - cut, None)),
        "{:?}",
        letters(&h, 11)
    );
    assert!(h.replay().unwrap() > 0);
}

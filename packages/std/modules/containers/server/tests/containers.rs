//! std.containers end to end: grants from services, moving, merging,
//! splitting, destroying, atomic rollback, ledgers on outcomes, and replay.

#![expect(clippy::unwrap_used, clippy::cast_possible_truncation)]

use std::sync::Arc;

use mantis_core::wire::{Message, decode_exact, decode_message};
use mantis_server::harness::{Harness, bytes_of};
use mantis_server::modules::{ExtensionKind, ExtensionRefusal};
use std_containers_contract::{
    Bag, DestroyItem, GOLD, Grant, Inventories, Ledger, MAX_STACK, MoveItem, SLOTS, ShowBag, SplitStack,
};

const MANIFEST: &str = include_str!("../../manifest.toml");

fn bed() -> Harness {
    let mut h = Harness::new(&[Arc::new(std_containers_server::Module)], &[MANIFEST], &[], &[]).unwrap();
    h.join(1, 11, 0.0, 0.0);
    h.tick().unwrap();
    h
}

fn grant(h: &mut Harness, character: u64, item: u32, count: u32, gold: u64) {
    h.service(
        Grant::ID.0,
        &bytes_of(&Grant {
            character,
            item,
            count,
            gold,
        }),
    );
}

fn slots(h: &Harness) -> Vec<(u32, u32)> {
    let inv = h.world().resource::<Inventories>().unwrap();
    inv.bag(11)
        .unwrap()
        .slots
        .iter()
        .map(|s| s.map_or((0, 0), |s| (s.item, s.count)))
        .collect()
}

#[test]
fn grants_stack_and_carry_a_ledger() {
    let mut h = bed();
    grant(&mut h, 11, 7, 150, 30);
    h.tick().unwrap();
    assert_eq!(&slots(&h)[..3], [(7, MAX_STACK), (7, 150 - MAX_STACK), (0, 0)]);
    let ledger: Ledger = decode_exact(h.outcomes[0].payload.as_slice()).unwrap();
    assert_eq!((ledger.net(11, GOLD), ledger.net(11, 7)), (30, 150));
    // The client was told its bag.
    let bag = h
        .take(1)
        .messages
        .iter()
        .find(|(k, _)| *k == ExtensionKind(Bag::ID.0))
        .cloned()
        .unwrap();
    assert_eq!(decode_message::<Bag>(&bag.1).unwrap().gold, 30);
    // A client cannot grant itself.
    h.send(
        1,
        Grant::ID.0,
        &bytes_of(&Grant {
            character: 11,
            item: 0,
            count: 0,
            gold: 1_000,
        }),
    );
    h.tick().unwrap();
    assert_eq!(
        h.take(1).refusals,
        [(ExtensionKind(Grant::ID.0), ExtensionRefusal::NotAllowed)]
    );
    assert_eq!(h.world().resource::<Inventories>().unwrap().gold(11), 30);
}

#[test]
fn moving_merging_splitting_and_destroying() {
    let mut h = bed();
    grant(&mut h, 11, 7, 10, 0);
    grant(&mut h, 11, 8, 5, 0);
    h.tick().unwrap();
    h.send(
        1,
        SplitStack::ID.0,
        &bytes_of(&SplitStack {
            from: 0,
            to: 5,
            count: 4,
        }),
    );
    h.tick().unwrap();
    assert_eq!((slots(&h)[0], slots(&h)[5]), ((7, 6), (7, 4)));
    h.send(1, MoveItem::ID.0, &bytes_of(&MoveItem { from: 5, to: 0 }));
    h.tick().unwrap();
    assert_eq!((slots(&h)[0], slots(&h)[5]), ((7, 10), (0, 0)), "merged");
    h.send(1, MoveItem::ID.0, &bytes_of(&MoveItem { from: 0, to: 1 }));
    h.tick().unwrap();
    assert_eq!((slots(&h)[0], slots(&h)[1]), ((8, 5), (7, 10)), "swapped");
    h.send(
        1,
        DestroyItem::ID.0,
        &bytes_of(&DestroyItem { slot: 1, count: 3 }),
    );
    h.tick().unwrap();
    assert_eq!(slots(&h)[1], (7, 7));
    let destroyed: Ledger = decode_exact(h.outcomes.last().unwrap().payload.as_slice()).unwrap();
    assert_eq!(destroyed.net(11, 7), -3);
    // Bad requests change nothing.
    let before = slots(&h);
    h.send(
        1,
        DestroyItem::ID.0,
        &bytes_of(&DestroyItem { slot: 1, count: 8 }),
    );
    h.send(1, MoveItem::ID.0, &bytes_of(&MoveItem { from: 9, to: 0 }));
    h.send(
        1,
        SplitStack::ID.0,
        &bytes_of(&SplitStack {
            from: 1,
            to: 0,
            count: 1,
        }),
    );
    h.send(
        1,
        MoveItem::ID.0,
        &bytes_of(&MoveItem {
            from: 0,
            to: SLOTS as u8,
        }),
    );
    h.tick().unwrap();
    assert_eq!(slots(&h), before);
    let reasons: Vec<ExtensionRefusal> = h.take(1).refusals.iter().map(|(_, r)| *r).collect();
    assert_eq!(
        reasons,
        [
            ExtensionRefusal::NotAllowed,
            ExtensionRefusal::NotAllowed,
            ExtensionRefusal::NotAllowed,
            ExtensionRefusal::Invalid
        ]
    );
    assert!(h.replay().unwrap() > 0);
}

#[test]
fn a_failed_command_rolls_back_entirely() {
    let mut h = bed();
    // Fill the bag.
    for item in 1..=SLOTS as u32 {
        grant(&mut h, 11, 100 + item, MAX_STACK, 0);
    }
    h.tick().unwrap();
    // Gold and an item that cannot fit: neither happens.
    grant(&mut h, 11, 999, 1, 50);
    h.tick().unwrap();
    assert_eq!(
        h.outcomes.last().unwrap().result,
        Err(ExtensionRefusal::NotAllowed)
    );
    assert_eq!(h.world().resource::<Inventories>().unwrap().gold(11), 0);
    h.send(1, ShowBag::ID.0, &bytes_of(&ShowBag {}));
    h.tick().unwrap();
    let got = h.take(1);
    let last = decode_message::<Bag>(&got.messages.last().unwrap().1).unwrap();
    assert_eq!(last.items.iter().filter(|i| **i != 0).count(), SLOTS);
}

/// Gold beyond what a ledger row records exactly, or beyond u64 in a bag,
/// is refused: nothing is clipped or wrapped.
#[test]
fn gold_the_ledger_cannot_record_or_a_bag_cannot_hold_is_refused() {
    let mut h = bed();
    grant(&mut h, 11, 0, 0, u64::MAX);
    h.tick().unwrap();
    assert!(h.outcomes[0].result.is_err(), "more than a ledger row records");
    let max = u64::try_from(i64::MAX).unwrap();
    grant(&mut h, 11, 0, 0, max);
    grant(&mut h, 11, 0, 0, max);
    grant(&mut h, 11, 0, 0, max);
    h.tick().unwrap();
    let results: Vec<bool> = h.outcomes[1..].iter().map(|o| o.result.is_ok()).collect();
    assert_eq!(results, [true, true, false], "the third would wrap the bag");
    let inv = h.world().resource::<Inventories>().unwrap();
    assert_eq!(inv.gold(11), max * 2);
}

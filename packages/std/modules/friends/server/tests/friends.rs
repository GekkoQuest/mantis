//! std.friends end to end, with the social role run in-process by the test
//! bed: requests, answers, crossing requests, removal, presence, refusals
//! from the authority, a restarted authority, and replay. Every answer
//! from the social role arrives the tick after the request.

#![allow(clippy::unwrap_used)]

use std::sync::Arc;

use mantis_core::module::ask;
use mantis_core::wire::{Message, decode_message};
use mantis_server::harness::{Harness, bytes_of};
use mantis_server::modules::{ExtensionKind, ExtensionRefusal};
use std_friends_contract::{
    AreFriends, FriendList, FriendRefused, OP_REQUEST, OP_RESPOND, Remove, Request, Requested, Respond,
};

const MANIFEST: &str = include_str!("../../manifest.toml");

fn bed() -> Harness {
    let mut h = Harness::new(&[Arc::new(std_friends_server::Module)], &[MANIFEST], &[], &[]).unwrap();
    h.join(1, 11, 0.0, 0.0);
    h.join(2, 12, 0.0, 0.0);
    h.join(3, 13, 0.0, 0.0);
    h.ticks(2).unwrap();
    h
}

fn refused_ops(h: &mut Harness, s: u64) -> Vec<u8> {
    h.take(s)
        .messages
        .iter()
        .filter(|(k, _)| *k == ExtensionKind(FriendRefused::ID.0))
        .map(|(_, b)| decode_message::<FriendRefused>(b).unwrap().op)
        .collect()
}

fn list(h: &mut Harness, s: u64) -> Option<(Vec<u64>, u64)> {
    h.take(s)
        .messages
        .iter()
        .rev()
        .find(|(k, _)| *k == ExtensionKind(FriendList::ID.0))
        .map(|(_, b)| {
            let l = decode_message::<FriendList>(b).unwrap();
            (l.friends.iter().copied().collect(), l.present)
        })
}

#[test]
fn requests_answers_and_removal() {
    let mut h = bed();
    h.send(1, Request::ID.0, &bytes_of(&Request { character: 12 }));
    h.ticks(2).unwrap();
    let got = h.take(2);
    assert_eq!(decode_message::<Requested>(&got.messages[0].1).unwrap().from, 11);
    h.send(
        2,
        Respond::ID.0,
        &bytes_of(&Respond {
            character: 11,
            accept: true,
        }),
    );
    h.ticks(2).unwrap();
    assert!(ask(h.world(), &AreFriends(11, 12)).unwrap());
    assert_eq!(list(&mut h, 1), Some((vec![12], 1)), "12 is present");
    // A refused request leaves no friendship; answering twice is refused.
    h.send(3, Request::ID.0, &bytes_of(&Request { character: 11 }));
    h.ticks(2).unwrap();
    h.send(
        1,
        Respond::ID.0,
        &bytes_of(&Respond {
            character: 13,
            accept: false,
        }),
    );
    h.send(
        1,
        Respond::ID.0,
        &bytes_of(&Respond {
            character: 13,
            accept: true,
        }),
    );
    h.ticks(2).unwrap();
    assert!(!ask(h.world(), &AreFriends(11, 13)).unwrap());
    assert_eq!(refused_ops(&mut h, 1), [OP_RESPOND], "answering twice");
    h.send(2, Remove::ID.0, &bytes_of(&Remove { character: 11 }));
    h.ticks(2).unwrap();
    assert!(!ask(h.world(), &AreFriends(12, 11)).unwrap());
    assert_eq!(list(&mut h, 1), Some((vec![], 0)));
    assert_eq!(h.replay().unwrap(), 12);
}

#[test]
fn crossing_requests_make_friends_and_bad_ones_are_refused() {
    let mut h = bed();
    h.send(1, Request::ID.0, &bytes_of(&Request { character: 13 }));
    h.ticks(2).unwrap();
    h.send(3, Request::ID.0, &bytes_of(&Request { character: 11 }));
    h.ticks(2).unwrap();
    assert!(ask(h.world(), &AreFriends(13, 11)).unwrap());
    h.send(1, Request::ID.0, &bytes_of(&Request { character: 11 })); // self: here
    h.send(1, Request::ID.0, &bytes_of(&Request { character: 13 })); // a friend
    h.send(1, Request::ID.0, &bytes_of(&Request { character: 0 })); // malformed
    h.ticks(2).unwrap();
    assert_eq!(refused_ops(&mut h, 1), [OP_REQUEST]);
    let r: Vec<ExtensionRefusal> = h.take(1).refusals.iter().map(|(_, r)| *r).collect();
    assert!(r.is_empty());
}

#[test]
fn friends_anywhere_and_a_restarted_authority() {
    let mut h = bed();
    // Asking a character in no cell here is the social role's business.
    h.send(1, Request::ID.0, &bytes_of(&Request { character: 99 }));
    h.ticks(2).unwrap();
    assert!(h.take(1).refusals.is_empty());
    h.send(1, Request::ID.0, &bytes_of(&Request { character: 12 }));
    h.ticks(2).unwrap();
    h.send(
        2,
        Respond::ID.0,
        &bytes_of(&Respond {
            character: 11,
            accept: true,
        }),
    );
    h.ticks(2).unwrap();
    assert!(ask(h.world(), &AreFriends(11, 12)).unwrap());
    // The social role restarts with nothing; the projection rebuilds it.
    h.friends = mantis_core::social::FriendBook::default();
    h.ticks(10 * 30 + 1).unwrap();
    assert!(h.friends.are_friends(12, 11));
    assert!(h.replay().unwrap() > 0);
}

#[test]
fn pending_requests_survive_a_reconnect_and_declines_are_told() {
    use std_friends_contract::{Declined, Pending, ShowFriends};
    let mut h = bed();
    h.send(1, Request::ID.0, &bytes_of(&Request { character: 12 }));
    h.ticks(2).unwrap();
    h.take(1);
    h.take(2);
    // As after a login: ask for everything.
    h.send(2, ShowFriends::ID.0, &bytes_of(&ShowFriends {}));
    h.ticks(2).unwrap();
    let got = h.take(2);
    let pending = got
        .messages
        .iter()
        .find(|(k, _)| *k == ExtensionKind(Pending::ID.0))
        .map(|(_, b)| decode_message::<Pending>(b).unwrap())
        .unwrap();
    assert_eq!(pending.incoming.iter().copied().collect::<Vec<_>>(), [11]);
    h.send(
        2,
        Respond::ID.0,
        &bytes_of(&Respond {
            character: 11,
            accept: false,
        }),
    );
    h.ticks(2).unwrap();
    let got = h.take(1);
    let declined = got
        .messages
        .iter()
        .find(|(k, _)| *k == ExtensionKind(Declined::ID.0))
        .unwrap();
    assert_eq!(decode_message::<Declined>(&declined.1).unwrap().by, 12);
    let last = got
        .messages
        .iter()
        .rev()
        .find(|(k, _)| *k == ExtensionKind(Pending::ID.0))
        .map(|(_, b)| decode_message::<Pending>(b).unwrap())
        .unwrap();
    assert!(last.outgoing.iter().next().is_none(), "nothing pending any more");
}

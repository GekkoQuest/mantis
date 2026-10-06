//! std.party end to end in one cell, with the social role run in-process by
//! the test bed: invitations, rosters, leadership, disbanding, expiry,
//! refusals from the authority, the `invites` flag, disabling, and replay.
//! Every answer from the social role arrives the tick after the request,
//! as a logged service update.

#![allow(clippy::unwrap_used, clippy::cast_precision_loss)]

use std::sync::Arc;

use mantis_core::module::ask;
use mantis_core::wire::{Message, decode_message};
use mantis_server::harness::{Harness, bytes_of};
use mantis_server::modules::{ExtensionKind, ExtensionRefusal};
use std_party_contract::{
    Accept, Disbanded, Invite, Invited, Kick, Leave, OP_ACCEPT, OP_INVITE, PartyOf, PartyRefused, Roster,
};

const MANIFEST: &str = include_str!("../../manifest.toml");

fn bed(flags: &[(&str, bool)]) -> Harness {
    let mut h = Harness::new(&[Arc::new(std_party_server::Module)], &[MANIFEST], flags, &[]).unwrap();
    for s in 1..=4u64 {
        h.join(s, 100 + s, s as f32, 0.0);
    }
    h.tick().unwrap();
    h
}

/// The request's tick, and the tick its answer lands on.
fn round(h: &mut Harness) {
    h.ticks(2).unwrap();
}

fn invite(h: &mut Harness, from: u64, to: u64) {
    h.send(
        from,
        Invite::ID.0,
        &bytes_of(&Invite {
            target: Harness::avatar(to),
        }),
    );
}

fn accept(h: &mut Harness, who: u64, from_character: u64) {
    h.send(who, Accept::ID.0, &bytes_of(&Accept { from: from_character }));
}

fn last_roster(h: &mut Harness, s: u64) -> Option<Roster> {
    h.take(s)
        .messages
        .iter()
        .rev()
        .find(|(k, _)| *k == ExtensionKind(Roster::ID.0))
        .map(|(_, b)| decode_message::<Roster>(b).unwrap())
}

fn refused_ops(h: &mut Harness, s: u64) -> Vec<u8> {
    h.take(s)
        .messages
        .iter()
        .filter(|(k, _)| *k == ExtensionKind(PartyRefused::ID.0))
        .map(|(_, b)| decode_message::<PartyRefused>(b).unwrap().op)
        .collect()
}

#[test]
fn a_party_forms_grows_changes_leader_and_disbands() {
    let mut h = bed(&[]);
    invite(&mut h, 1, 2);
    round(&mut h);
    let got = h.take(2);
    assert_eq!(got.messages[0].0, ExtensionKind(Invited::ID.0));
    assert_eq!(decode_message::<Invited>(&got.messages[0].1).unwrap().from, 101);
    accept(&mut h, 2, 101);
    round(&mut h);
    let r = last_roster(&mut h, 1).unwrap();
    assert_eq!(
        (r.leader, r.members.iter().copied().collect::<Vec<_>>()),
        (101, vec![101, 102])
    );
    // Only the leader invites (the authority refuses); then a third joins.
    invite(&mut h, 2, 3);
    invite(&mut h, 1, 3);
    round(&mut h);
    assert_eq!(refused_ops(&mut h, 2), [OP_INVITE]);
    accept(&mut h, 3, 101);
    round(&mut h);
    let view = ask(h.world(), &PartyOf(103)).unwrap().unwrap();
    assert_eq!(view.members(), [101, 102, 103]);
    // The leader leaves: the next member leads.
    h.send(1, Leave::ID.0, &bytes_of(&Leave {}));
    round(&mut h);
    assert_eq!(ask(h.world(), &PartyOf(102)).unwrap().unwrap().leader, 102);
    assert!(
        h.take(1)
            .messages
            .iter()
            .any(|(k, _)| *k == ExtensionKind(Disbanded::ID.0))
    );
    // The new leader kicks the last other member: a party of one disbands.
    h.send(2, Kick::ID.0, &bytes_of(&Kick { character: 103 }));
    round(&mut h);
    assert_eq!(ask(h.world(), &PartyOf(102)).unwrap(), None);
    assert!(
        h.take(2)
            .messages
            .iter()
            .any(|(k, _)| *k == ExtensionKind(Disbanded::ID.0))
    );
    assert_eq!(h.replay().unwrap(), 13, "every tick replays");
}

#[test]
fn bad_requests_are_refused_locally_or_by_the_authority() {
    let mut h = bed(&[]);
    accept(&mut h, 2, 101); // no invitation: the authority refuses
    invite(&mut h, 1, 1); // self: refused here
    h.send(1, Accept::ID.0, &bytes_of(&Accept { from: 0 })); // fails validation
    h.send(1, Invite::ID.0, &[1, 2]); // malformed
    round(&mut h);
    assert_eq!(refused_ops(&mut h, 2), [OP_ACCEPT]);
    assert_eq!(
        h.take(1).refusals,
        [
            (ExtensionKind(Invite::ID.0), ExtensionRefusal::NotAllowed),
            (ExtensionKind(Accept::ID.0), ExtensionRefusal::Invalid),
            (ExtensionKind(Invite::ID.0), ExtensionRefusal::Invalid),
        ]
    );
}

#[test]
fn invitations_expire() {
    let mut h = bed(&[]);
    invite(&mut h, 1, 2);
    h.ticks(60 * 30 + 1).unwrap();
    accept(&mut h, 2, 101);
    round(&mut h);
    assert!(refused_ops(&mut h, 2).contains(&OP_ACCEPT));
}

#[test]
fn flags_switch_invitations_and_the_whole_module() {
    let mut h = bed(&[("std.party.invites", false)]);
    invite(&mut h, 1, 2);
    h.tick().unwrap();
    assert_eq!(
        h.take(1).refusals,
        [(ExtensionKind(Invite::ID.0), ExtensionRefusal::NotAllowed)]
    );
    let mut h = bed(&[]);
    h.set_enabled("std.party", false);
    h.tick().unwrap();
    invite(&mut h, 1, 2);
    h.tick().unwrap();
    assert_eq!(
        h.take(1).refusals,
        [(ExtensionKind(Invite::ID.0), ExtensionRefusal::FeatureDisabled)]
    );
    assert!(ask(h.world(), &PartyOf(101)).is_err(), "its queries are off too");
}

#[test]
fn a_restarted_authority_rebuilds_parties_from_the_projection() {
    let mut h = bed(&[]);
    invite(&mut h, 1, 2);
    round(&mut h);
    accept(&mut h, 2, 101);
    round(&mut h);
    // The social role restarts with nothing.
    h.parties = mantis_core::social::PartyBook::default();
    assert_eq!(h.parties.party_of(101), None);
    // Within the restore period the cell offers its projection back.
    h.ticks(10 * 30 + 1).unwrap();
    assert_eq!(h.parties.party_of(102), h.parties.party_of(101));
    assert!(h.parties.party_of(101).is_some());
    // And the rebuilt party works: the leader invites a third.
    invite(&mut h, 1, 3);
    round(&mut h);
    accept(&mut h, 3, 101);
    round(&mut h);
    assert_eq!(
        ask(h.world(), &PartyOf(103)).unwrap().unwrap().members(),
        [101, 102, 103]
    );
    assert!(h.replay().unwrap() > 0);
}

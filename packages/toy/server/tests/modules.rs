//! The package's modules through the whole server, whatever modules are
//! linked: the graph resolves and prints, clients on both adapters learn
//! every module's state, Ops switches a module off and on, intents and
//! commands reach modules through the host on both adapters, and kinds no
//! module registered are refused at the host.
//!
//! These tests name no module: removing any module folder (and running
//! `mantis-modsync`) leaves them green. Each module's own behaviour is
//! tested in its own crate.

#![expect(clippy::unwrap_used, clippy::indexing_slicing)]

use std::collections::BTreeMap;

use mantis_core::log::SessionId;
use mantis_server::bots::Profile;
use mantis_server::intent::CellIntent;
use mantis_server::modules::{ExtensionKind, ExtensionRefusal, ModuleStates, Route};
use mantis_server::simnet::LinkConfig;
use toy_server::sim::{Side, Sim};
use toy_server::tunables::Tunables;
use toy_server::world;

fn sim() -> Sim {
    let mut s = Sim::new(Tunables::defaults().unwrap(), 21, |_| None).unwrap();
    s.add_bot(Side::Native, Profile::Idle, LinkConfig::PERFECT)
        .unwrap();
    s.add_bot(Side::Legacy, Profile::Idle, LinkConfig::PERFECT)
        .unwrap();
    for _ in 0..5 {
        s.step().unwrap();
    }
    assert!(s.bots.iter().all(|b| b.bot.synced()));
    s
}

fn steps(s: &mut Sim, n: usize) {
    for _ in 0..n {
        s.step().unwrap();
    }
}

/// Switches a module in every cell, as Ops does: a logged intent.
fn switch(s: &mut Sim, key: &str, enabled: bool) {
    for i in 0..s.zone.cells().len() {
        let cell = s.zone.cell_mut(i).unwrap();
        let id = cell.world().resource::<ModuleStates>().unwrap().id(key).unwrap();
        cell.inbox().push(
            SessionId(0),
            CellIntent::SetModule {
                module: id.0,
                enabled,
            },
        );
    }
}

/// Every registered kind and its route, as the cell sees them.
fn routes(s: &Sim) -> BTreeMap<u16, Route> {
    let cell = &s.zone.cells()[0];
    (0..=u16::MAX)
        .filter_map(|k| cell.extension_route(ExtensionKind(k)).map(|r| (k, r)))
        .collect()
}

#[test]
fn the_graph_resolves_and_every_client_learns_every_module() {
    let set = world::module_set(&BTreeMap::new()).unwrap();
    let text = set.graph().render();
    let keys: Vec<String> = set.graph().modules.iter().map(|m| m.key.clone()).collect();
    for k in &keys {
        assert!(text.contains(k.as_str()), "{text}");
    }
    let s = sim();
    for b in &s.bots {
        let learned: Vec<&String> = b.bot.stats.features.keys().collect();
        assert_eq!(learned.len(), keys.len(), "both adapters carry feature states");
        assert!(b.bot.stats.features.values().all(|on| *on));
    }
}

#[test]
fn a_switched_off_module_answers_feature_disabled_on_both_adapters() {
    let mut s = sim();
    let routes = routes(&s);
    let Some((kind, _)) = routes.iter().find(|(_, r)| **r == Route::Intent) else {
        return; // no module with intents is linked
    };
    let _ = routes;
    let kind = ExtensionKind(*kind);
    for key in s.zone.cells()[0]
        .world()
        .resource::<ModuleStates>()
        .unwrap()
        .keys
        .clone()
    {
        switch(&mut s, &key, false);
    }
    steps(&mut s, 3);
    for b in &s.bots {
        assert!(
            b.bot.stats.features.values().all(|on| !*on),
            "every module is off"
        );
    }
    for i in 0..2 {
        s.bots[i].bot.feature(kind, &[]);
    }
    steps(&mut s, 3);
    for b in &s.bots {
        assert_eq!(
            b.bot.stats.extension_refusals,
            [(kind, ExtensionRefusal::FeatureDisabled)]
        );
    }
    for key in s.zone.cells()[0]
        .world()
        .resource::<ModuleStates>()
        .unwrap()
        .keys
        .clone()
    {
        switch(&mut s, &key, true);
    }
    steps(&mut s, 3);
    assert!(s.bots.iter().all(|b| b.bot.stats.features.values().all(|on| *on)));
}

#[test]
fn intents_and_commands_reach_modules_through_the_host() {
    let mut s = sim();
    let routes = routes(&s);
    // A three-byte payload is malformed for every module message: each kind
    // is routed to its module, which refuses it (as invalid, or as not
    // allowed for a client when it is a service-only command). Intents are
    // answered by handlers, commands by executors with a logged outcome.
    let mut commands = 0;
    for (kind, route) in &routes {
        s.bots[1].bot.feature(ExtensionKind(*kind), &[0xFF; 3]);
        commands += usize::from(*route == Route::Command);
    }
    steps(&mut s, 4);
    let refused = &s.bots[1].bot.stats.extension_refusals;
    assert_eq!(refused.len(), routes.len(), "every kind answered");
    assert!(
        refused
            .iter()
            .all(|(_, r)| matches!(r, ExtensionRefusal::Invalid | ExtensionRefusal::NotAllowed))
    );
    let route = s.zone.route(SessionId(2)).unwrap();
    let mut outcomes = 0;
    s.zone
        .cell_mut(route)
        .unwrap()
        .drain_outcomes(|_, _| outcomes += 1);
    assert_eq!(outcomes, commands, "commands went through the log path");
    // Bot 1 speaks the legacy protocol, which carries no request ids.
    assert!(s.bots[1].bot.stats.refused_requests.iter().all(|r| *r == 0));
    // A kind no module registered is refused at the host, echoing the
    // native client's request id.
    let request = s.bots[0].bot.feature(ExtensionKind(0xFFFF), &[1]);
    steps(&mut s, 3);
    assert_eq!(
        s.bots[0].bot.stats.extension_refusals,
        [(ExtensionKind(0xFFFF), ExtensionRefusal::Invalid)]
    );
    assert_eq!(s.bots[0].bot.stats.refused_requests, [request]);
    // Handler and command refusals echo it as well.
    let mut sent: Vec<u32> = routes
        .keys()
        .map(|kind| s.bots[0].bot.feature(ExtensionKind(*kind), &[0xFF; 3]))
        .collect();
    steps(&mut s, 4);
    let mut echoed = s.bots[0].bot.stats.refused_requests[1..].to_vec();
    sent.sort_unstable();
    echoed.sort_unstable();
    assert_eq!(echoed, sent);
}

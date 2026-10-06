//! The script runtime's guarantees, one test each (decision 0001).

#![allow(clippy::unwrap_used)]

use std::cell::RefCell;

use mantis_core::ecs::EntityId;
use mantis_core::hash::StableHasher;
use mantis_core::rng::{Rng, Salt, Seed};
use mantis_core::time::Tick;
use mantis_script::lint::check_server_source;
use mantis_script::{Api, Limits, ScriptError, ScriptValue, ScriptVm, Tier, Tiers};
use mantis_testkit::alloc::{CountingAllocator, count_allocs, exempt};

#[global_allocator]
static ALLOC: CountingAllocator = CountingAllocator;

fn vm(tier: Tier) -> ScriptVm {
    ScriptVm::new(tier, Limits::DEFAULT, Seed(7)).unwrap()
}

fn owner() -> EntityId {
    EntityId::new(3, 1)
}

#[test]
fn an_infinite_loop_is_stopped_and_other_scripts_run_next_tick() {
    let mut v = vm(Tier::Server);
    v.load("spin", "function on_tick() while true do end end", owner())
        .unwrap();
    v.load(
        "count",
        "state.n = 0 function on_tick() state.n = state.n + 1 end",
        owner(),
    )
    .unwrap();
    let r = v.tick(Tick(1), &mut Api::new(), &[]);
    assert!(r.budget_exhausted);
    assert!(r.errors.iter().any(|(s, e)| s == "spin" && e.contains("budget")));
    // The budget is per tick: next tick runs again (and is stopped again).
    let r2 = v.tick(Tick(2), &mut Api::new(), &[]);
    assert!(r2.budget_exhausted);
    // With the spinner gone, the counter runs every tick.
    v.unload("spin");
    for t in 3..6 {
        let r = v.tick(Tick(t), &mut Api::new(), &[]);
        assert!(r.errors.is_empty(), "{r:?}");
    }
}

#[test]
fn the_sandbox_has_no_os_and_no_escape_hatches() {
    let mut v = vm(Tier::Server);
    for global in [
        "os",
        "debug",
        "require",
        "loadstring",
        "getfenv",
        "setfenv",
        "collectgarbage",
        "newproxy",
        "io",
    ] {
        let src =
            format!("function on_tick() if {global} ~= nil then error('{global} is reachable') end end");
        v.load(global, &src, owner()).unwrap();
    }
    let r = v.tick(Tick(1), &mut Api::new(), &[]);
    assert!(r.errors.is_empty(), "{:?}", r.errors);
    // Libraries are read-only: one script cannot change another's math.
    v.load("vandal", "function on_tick() math.floor = nil end", owner())
        .unwrap();
    let r = v.tick(Tick(2), &mut Api::new(), &[]);
    assert!(
        r.errors
            .iter()
            .any(|(s, e)| s == "vandal" && e.contains("readonly")),
        "{:?}",
        r.errors
    );
    // tostring never shows an address.
    v.load(
        "addr",
        "function on_tick() assert(tostring({}) == 'table') assert(tostring(1.5) == '1.5') end",
        owner(),
    )
    .unwrap();
    let r = v.tick(Tick(3), &mut Api::new(), &[]);
    assert!(!r.errors.iter().any(|(s, _)| s == "addr"), "{:?}", r.errors);
}

#[test]
fn math_random_draws_from_the_entity_stream() {
    let mut v = vm(Tier::Server);
    let got = RefCell::new(Vec::new());
    let mut api = Api::new();
    api.function("record", Tiers::SERVER, |args| {
        got.borrow_mut().push(args[0].as_number().unwrap());
        Ok(vec![])
    });
    v.load(
        "dice",
        "function on_tick() host.record(math.random()) host.record(math.random(6)) host.record(math.random(10, 12)) end",
        owner(),
    )
    .unwrap();
    v.tick(Tick(9), &mut api, &[]);
    drop(api);
    let mut rng = Rng::for_entity(Seed(7), Tick(9), owner(), Salt::named("script.random"));
    let a = rng.next_f64();
    let b = f64::from(rng.below(6) + 1);
    let c = f64::from(10 + rng.below(3));
    assert_eq!(*got.borrow(), [a, b, c]);
    // The deterministic kernels replace the platform's.
    let vals = RefCell::new(Vec::new());
    let mut api = Api::new();
    api.function("record", Tiers::SERVER, |args| {
        vals.borrow_mut().push(args[0].as_number().unwrap());
        Ok(vec![])
    });
    v.load(
        "trig",
        "function on_tick() host.record(math.sin(1)) host.record(math.pow(2, 10)) end",
        owner(),
    )
    .unwrap();
    v.unload("dice");
    v.tick(Tick(10), &mut api, &[]);
    drop(api);
    assert_eq!(
        *vals.borrow(),
        [
            f64::from(mantis_core::math::sin(1.0)),
            f64::from(mantis_core::math::pow(2.0, 10.0))
        ]
    );
}

#[test]
fn server_sources_are_linted_and_state_keys_are_checked() {
    assert_eq!(check_server_source("local x = 2 ^ 3").len(), 1);
    assert_eq!(check_server_source("local t = {} t[{}] = 1").len(), 1);
    assert_eq!(check_server_source("local t = { [function() end] = 1 }").len(), 1);
    assert!(check_server_source("-- 2 ^ 3 in a comment\nlocal s = 'x ^ y [{' local l = [[ ^ ]]").is_empty());
    let mut v = vm(Tier::Server);
    assert!(matches!(
        v.load("pow", "local y = 2 ^ 2", owner()),
        Err(ScriptError::Lint(_))
    ));
    // A key computed at run time is caught by the state check; the script is
    // unloaded, others keep running.
    v.load(
        "sneaky",
        "function on_tick() local k = {} state[k] = true end",
        owner(),
    )
    .unwrap();
    v.load(
        "fine",
        "function on_tick() state.ok = true state[1] = 'a' end",
        owner(),
    )
    .unwrap();
    let r = v.tick(Tick(1), &mut Api::new(), &[]);
    assert_eq!(r.key_findings.len(), 1);
    assert_eq!(r.key_findings[0].0, "sneaky");
    assert_eq!(v.scripts().collect::<Vec<_>>(), ["fine"]);
    // Presentation mods are not server scripts: no lint at load.
    let mut p = vm(Tier::Presentation);
    p.load("mod", "local y = 2 ^ 2", owner()).unwrap();
}

#[test]
fn entities_cross_as_light_userdata_and_compare_by_value() {
    let mut v = vm(Tier::Server);
    let seen = RefCell::new(Vec::new());
    let mut api = Api::new();
    api.function("me", Tiers::SERVER, |_| {
        Ok(vec![ScriptValue::Entity(EntityId::new(42, 7))])
    })
    .function("record", Tiers::SERVER, |args| {
        seen.borrow_mut().push(args[0].clone());
        Ok(vec![])
    });
    v.load(
        "e",
        "function on_tick() local a, b = host.me(), host.me() state[a] = 'hp' host.record(a == b) host.record(state[b]) host.record(type(a)) host.record(a) end",
        owner(),
    )
    .unwrap();
    let r = v.tick(Tick(1), &mut api, &[]);
    drop(api);
    assert!(r.errors.is_empty() && r.key_findings.is_empty(), "{r:?}");
    assert_eq!(
        *seen.borrow(),
        [
            ScriptValue::Bool(true),
            ScriptValue::Str("hp".into()),
            ScriptValue::Str("userdata".into()),
            ScriptValue::Entity(EntityId::new(42, 7))
        ]
    );
}

#[test]
fn hot_reload_happens_at_a_tick_boundary_and_keeps_state() {
    const V1: &str = "state.n = state.n or 0 function on_tick() state.n = state.n + 1 host.n(state.n) end";
    const V2: &str = "state.n = state.n or 0 function on_tick() state.n = state.n + 100 host.n(state.n) end";
    let seen = RefCell::new(Vec::new());
    let run = |v: &mut ScriptVm, t: u64| {
        let mut api = Api::new();
        api.function("n", Tiers::SERVER, |args| {
            seen.borrow_mut().push(args[0].as_number().unwrap());
            Ok(vec![])
        });
        v.tick(Tick(t), &mut api, &[])
    };
    let mut v = vm(Tier::Server);
    v.load("q", V1, owner()).unwrap();
    run(&mut v, 1);
    v.reload("q", V2);
    // The reload waits for the tick boundary; then the new code runs on the
    // same state.
    let r = run(&mut v, 2);
    assert_eq!(r.reloaded, ["q"]);
    run(&mut v, 3);
    assert_eq!(*seen.borrow(), [1.0, 101.0, 201.0]);
    // Same code, same inputs: same script state hash.
    let mut fresh = vm(Tier::Server);
    fresh.load("q", V1, owner()).unwrap();
    run(&mut fresh, 1);
    fresh.reload("q", V2);
    run(&mut fresh, 2);
    run(&mut fresh, 3);
    let (mut h1, mut h2) = (StableHasher::new(), StableHasher::new());
    v.state_hash(&mut h1);
    fresh.state_hash(&mut h2);
    assert_eq!(h1.finish(), h2.finish());
    // A reload that fails to compile keeps the old code.
    v.reload("q", "function on_tick( end");
    let r = run(&mut v, 4);
    assert!(r.reloaded.is_empty() && !r.errors.is_empty());
    assert_eq!(seen.borrow().last(), Some(&301.0), "old code still runs");
}

#[test]
fn timers_and_events_run_in_a_fixed_order() {
    let mut v = vm(Tier::Server);
    let log = RefCell::new(Vec::new());
    let mut api = Api::new();
    api.function("log", Tiers::SERVER, |args| {
        log.borrow_mut().push(args[0].as_str().unwrap_or("?").to_owned());
        Ok(vec![])
    });
    v.load(
        "t",
        "mantis.on('door', 'opened') mantis.after(2, 'ring', 'b') mantis.after(2, 'ring', 'c') mantis.after(1, 'ring', 'a') \
         function ring(x) host.log(x) end function opened(who) host.log('door') end",
        owner(),
    )
    .unwrap();
    for t in 1..=3 {
        let events = if t == 3 {
            vec![("door".to_owned(), ScriptValue::Number(1.0))]
        } else {
            vec![]
        };
        v.tick(Tick(t), &mut api, &events);
    }
    drop(api);
    assert_eq!(*log.borrow(), ["a", "b", "c", "door"]);
}

#[test]
fn tiers_see_only_their_functions() {
    let make = || {
        let mut api = Api::new();
        api.function("spawn", Tiers::SERVER, |_| Ok(vec![]))
            .function("draw", Tiers::PRESENTATION, |_| Ok(vec![]))
            .function("act", Tiers::AUTOMATION, |_| Ok(vec![]));
        api
    };
    for (tier, expect) in [
        (Tier::Server, vec!["spawn"]),
        (Tier::Presentation, vec!["draw"]),
        (Tier::Automation, vec!["draw", "act"]),
    ] {
        let api = make();
        assert_eq!(api.names_for(tier).collect::<Vec<_>>(), expect, "{tier:?}");
        let mut v = vm(tier);
        v.load(
            "p",
            "function on_tick() if host.spawn then host.spawn() end if host.draw then host.draw() end if host.act then host.act() end end",
            owner(),
        )
        .unwrap();
        let mut api = make();
        let r = v.tick(Tick(1), &mut api, &[]);
        assert!(r.errors.is_empty(), "{tier:?}: {:?}", r.errors);
    }
    // A presentation mod calling an automation function fails: it is absent.
    let mut v = vm(Tier::Presentation);
    v.load("cheat", "function on_tick() host.act() end", owner())
        .unwrap();
    let r = v.tick(Tick(1), &mut make(), &[]);
    assert_eq!(r.errors.len(), 1);
}

#[test]
fn memory_is_capped_and_script_allocation_is_exempt_in_the_harness() {
    let limits = Limits {
        memory: 2 << 20,
        ..Limits::DEFAULT
    };
    let mut v = ScriptVm::new(Tier::Server, limits, Seed(1)).unwrap();
    v.set_wrapper(|f| exempt(f));
    v.load(
        "hog",
        "function on_tick() local t = {} for i = 1, 1e9 do t[i] = i end end",
        owner(),
    )
    .unwrap();
    let (r, stats) = count_allocs(|| v.tick(Tick(1), &mut Api::new(), &[]));
    assert!(
        r.errors.iter().any(|(s, _)| s == "hog"),
        "stopped by memory or budget"
    );
    assert_eq!(
        stats.total_ops(),
        0,
        "everything the VM did ran in the exempt scope"
    );
    assert!(v.memory() <= 2 << 20);
}

/// Lead ruling (M8): once loaded, a script's environment and every library
/// table are read-only, so `state` is the only table it can write; and
/// `state` holds only primitives, strings, entities, or tables of the same
/// (no metatables), so a snapshot holds all of it.
#[test]
fn environments_are_frozen_and_state_holds_only_saveable_values() {
    let mut v = vm(Tier::Server);
    v.load(
        "global",
        "counter = 0 function on_tick() counter = counter + 1 end",
        owner(),
    )
    .unwrap();
    v.load("library", "function on_tick() string.extra = 1 end", owner())
        .unwrap();
    v.load("fn", "function on_tick() state.f = function() end end", owner())
        .unwrap();
    v.load(
        "meta",
        "function on_tick() state.t = setmetatable({}, {}) end",
        owner(),
    )
    .unwrap();
    v.load(
        "fine",
        "function on_tick(t) state.n = (state.n or 0) + 1 state.nested = { a = { t, 'x', true } } end",
        owner(),
    )
    .unwrap();
    let r = v.tick(Tick(1), &mut Api::new(), &[]);
    let errored: Vec<&str> = r.errors.iter().map(|(s, _)| s.as_str()).collect();
    assert!(
        errored.contains(&"global"),
        "a global write errors at the line: {r:?}"
    );
    assert!(errored.contains(&"library"), "{r:?}");
    assert!(r.errors.iter().any(|(_, e)| e.contains("readonly")), "{r:?}");
    let unloaded: Vec<&str> = r.key_findings.iter().map(|(s, _)| s.as_str()).collect();
    assert_eq!(unloaded, ["fn", "meta"]);
    assert_eq!(v.scripts().collect::<Vec<_>>(), ["fine", "global", "library"]);
}

/// Saving and restoring a VM's script state reproduces its hash exactly.
#[test]
fn script_state_saves_and_restores_exactly() {
    let source = "state.n = state.n or 0 \
        function on_tick(t) state.n = state.n + 1 \
        if t % 3 == 0 then mantis.after(2, 'later', t) end end \
        function later(t) state.list = state.list or {} state.list[#state.list + 1] = { t, math.random(9) } end";
    let mut a = vm(Tier::Server);
    a.load("s", source, owner()).unwrap();
    for t in 1..20 {
        a.tick(Tick(t), &mut Api::new(), &[]);
    }
    let mut bytes = Vec::new();
    a.save(&mut mantis_core::wire::Encoder::new(&mut bytes)).unwrap();
    let mut b = vm(Tier::Server);
    b.load("s", source, owner()).unwrap();
    b.restore(&mut mantis_core::wire::Decoder::new(&bytes), &|_| None)
        .unwrap();
    let hash = |v: &ScriptVm| {
        let mut h = StableHasher::new();
        v.state_hash(&mut h);
        h.finish()
    };
    assert_eq!(hash(&a), hash(&b));
    for t in 20..40 {
        a.tick(Tick(t), &mut Api::new(), &[]);
        b.tick(Tick(t), &mut Api::new(), &[]);
        assert_eq!(hash(&a), hash(&b), "tick {t}");
    }
}

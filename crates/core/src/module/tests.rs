use std::collections::BTreeMap;
use std::fmt::Write as _;

use super::bus::{Event, Events, Queries, Query, QueryError};
use super::manifest::{ManifestError, Version, parse_manifest, parse_package};
use super::resolve::{Discovered, ResolveError, resolve};
use super::toml::{self, Value};
use crate::ecs::World;
use crate::hash::{StableHasher, StateHash};

fn manifest(key: &str, contract: Option<&str>, deps: &[(&str, &str)], flags: &[(&str, bool)]) -> String {
    let mut s = format!("[module]\nkey = \"{key}\"\nversion = \"0.1.0\"\n");
    if let Some(c) = contract {
        let _ = writeln!(s, "contract = \"{c}\"");
    }
    s.push_str("[dependencies]\n");
    for (c, v) in deps {
        let _ = writeln!(s, "\"{c}\" = \"{v}\"");
    }
    s.push_str("[flags]\n");
    for (f, v) in flags {
        let _ = writeln!(s, "{f} = {v}");
    }
    s
}

fn found(origin: &str, text: &str) -> Discovered {
    Discovered {
        origin: origin.to_owned(),
        manifest: parse_manifest(text).unwrap(),
    }
}

fn std_set() -> Vec<Discovered> {
    vec![
        found("std", &manifest("std.chat", None, &[], &[])),
        found(
            "std",
            &manifest("std.party", None, &[("std.chat", "0.1")], &[("invites", true)]),
        ),
        found("std", &manifest("std.mail", None, &[], &[])),
        found(
            "game",
            &manifest("game.raids", None, &[("std.party", "0.1")], &[]),
        ),
    ]
}

fn package(text: &str) -> super::manifest::PackageModules {
    parse_package(text).unwrap()
}

#[test]
fn toml_subset_reads_what_manifests_use_and_refuses_the_rest() {
    let doc = toml::parse(
        "# c\nroot = 1\n[a]\nk = \"v \\\"q\\\"\" # c\n\"x.y\" = true\nlist = [\"a\", \"b\",]\nf = 7.5\nn = -1_000\n",
    )
    .unwrap();
    assert_eq!(doc.table("").unwrap().get("root"), Some(&Value::Int(1)));
    let a = doc.table("a").unwrap();
    assert_eq!(a.get("k"), Some(&Value::Str("v \"q\"".into())));
    assert_eq!(a.get("x.y"), Some(&Value::Bool(true)));
    assert_eq!(
        a.get("list").and_then(Value::as_strings),
        Some(vec!["a".into(), "b".into()])
    );
    assert_eq!(a.get("f"), Some(&Value::Float("7.5".into())));
    assert_eq!(a.get("n"), Some(&Value::Int(-1000)));
    for bad in [
        "k = \"open",
        "k = [[1]]",
        "k = 1 2",
        "[a]\n[a]",
        "k = 1\nk = 2",
        "k = inf",
        "k = \"\\x\"",
        "= 1",
        "[a",
    ] {
        assert!(toml::parse(bad).is_err(), "{bad:?} must be refused");
    }
    assert_eq!(toml::parse("a = 1\nb = ?").unwrap_err().line, 2);
}

#[test]
fn manifests_parse_strictly() {
    let m = parse_manifest(
        "[module]\nkey = \"std.party\"\nversion = \"0.2.1\"\nschemas = [\"schema/party.idl\"]\ntables = [\"std.party.limits\"]\n[dependencies]\n\"std.chat\" = \"0.1\"\n[flags]\ninvites = false\n",
    )
    .unwrap();
    assert_eq!(m.key, "std.party");
    assert_eq!(m.contract, "std.party", "the contract defaults to the key");
    assert_eq!(
        m.version,
        Version {
            major: 0,
            minor: 2,
            patch: 1
        }
    );
    assert_eq!(m.dependencies[0].contract, "std.chat");
    assert_eq!(m.flags.get("enabled"), Some(&true), "enabled is implicit");
    assert_eq!(m.flags.get("invites"), Some(&false));
    assert_eq!(m.schemas, ["schema/party.idl"]);
    for (bad, why) in [
        ("[module]\nversion = \"1\"\n", "missing key"),
        (
            "[module]\nkey = \"Std.Party\"\nversion = \"1\"\n",
            "uppercase key",
        ),
        ("[module]\nkey = \"a\"\nversion = \"x\"\n", "bad version"),
        (
            "[module]\nkey = \"a\"\nversion = \"1\"\ntypo = 1\n",
            "unknown key",
        ),
        (
            "[module]\nkey = \"a\"\nversion = \"1\"\n[extras]\n",
            "unknown table",
        ),
        (
            "[module]\nkey = \"a\"\nversion = \"1\"\n[flags]\non = 1\n",
            "flag not a bool",
        ),
        (
            "[module]\nkey = \"a\"\nversion = \"1\"\n[dependencies]\na = \"1\"\n",
            "self dependency",
        ),
    ] {
        assert!(parse_manifest(bad).is_err(), "{why}");
    }
    assert_eq!(
        parse_manifest("[module]\nversion = \"1\"\n"),
        Err(ManifestError::Missing("module.key"))
    );
}

#[test]
fn caret_requirements() {
    let v = |s| Version::parse(s).unwrap();
    assert!(v("0.1.5").satisfies(v("0.1")));
    assert!(!v("0.2.0").satisfies(v("0.1")), "0.x minors are breaking");
    assert!(v("1.4.0").satisfies(v("1.2")));
    assert!(!v("2.0.0").satisfies(v("1.2")));
    assert!(!v("1.1.0").satisfies(v("1.2")));
    assert_eq!(Version::parse("1.2.3.4"), None);
}

#[test]
fn a_graph_resolves_in_dependency_order_and_prints() {
    let pkg =
        package("[package]\nname = \"game\"\nuses = [\"std\"]\n[flags]\n\"std.party.invites\" = false\n");
    let g = resolve(&pkg, &std_set(), &BTreeMap::new()).unwrap();
    let order: Vec<&str> = g.modules.iter().map(|m| m.key.as_str()).collect();
    assert_eq!(order, ["std.chat", "std.mail", "std.party", "game.raids"]);
    assert_eq!(g.flag("std.party.invites"), Some(false));
    assert!(g.is_enabled("std.mail"));
    let text = g.render();
    assert!(
        text.contains("game.raids 0.1.0 [game] enabled, needs std.party"),
        "{text}"
    );
    assert!(text.contains("off: invites"), "{text}");
    // A package that does not use std sees none of it.
    let alone = package("[package]\nname = \"std\"\n");
    assert_eq!(
        resolve(&alone, &std_set(), &BTreeMap::new())
            .unwrap()
            .modules
            .len(),
        3
    );
}

#[test]
fn missing_or_disabled_dependencies_refuse_to_start() {
    let set = std_set();
    let no_std = package("[package]\nname = \"game\"\n");
    assert_eq!(
        resolve(&no_std, &set, &BTreeMap::new()),
        Err(ResolveError::MissingDependency {
            module: "game.raids".into(),
            contract: "std.party".into()
        })
    );
    let pkg =
        package("[package]\nname = \"game\"\nuses = [\"std\"]\n[flags]\n\"std.chat.enabled\" = false\n");
    assert_eq!(
        resolve(&pkg, &set, &BTreeMap::new()),
        Err(ResolveError::DisabledDependency {
            module: "std.party".into(),
            dependency: "std.chat".into()
        })
    );
    // Disabling the dependents too is fine: nothing enabled needs chat.
    let live = BTreeMap::from([
        ("std.party.enabled".to_owned(), false),
        ("game.raids.enabled".to_owned(), false),
    ]);
    let g = resolve(&pkg, &set, &live).unwrap();
    assert!(!g.is_enabled("std.chat") && !g.is_enabled("std.party") && g.is_enabled("std.mail"));
    assert!(g.render().contains("std.chat 0.1.0 [std] DISABLED"));
    // A package default for a module it does not include is skipped (the
    // module may have been removed); a typo in an included module's flag,
    // or any unknown live flag, is refused.
    let absent = package(
        "[package]
name = \"game\"
uses = [\"std\"]
[flags]
\"std.guilds.enabled\" = false
",
    );
    assert!(resolve(&absent, &set, &BTreeMap::new()).is_ok());
    let typo_default = package(
        "[package]
name = \"game\"
uses = [\"std\"]
[flags]
\"std.party.invite\" = false
",
    );
    assert_eq!(
        resolve(&typo_default, &set, &BTreeMap::new()),
        Err(ResolveError::UnknownFlag("std.party.invite".into()))
    );
    let live_absent = BTreeMap::from([("std.guilds.enabled".to_owned(), true)]);
    assert!(resolve(&absent, &set, &live_absent).is_err());
    let typo = BTreeMap::from([("std.party.invite".to_owned(), true)]);
    assert_eq!(
        resolve(
            &package("[package]\nname = \"game\"\nuses = [\"std\"]\n"),
            &set,
            &typo
        ),
        Err(ResolveError::UnknownFlag("std.party.invite".into()))
    );
}

#[test]
fn versions_duplicates_and_cycles_are_refused() {
    let pkg = package("[package]\nname = \"p\"\n");
    let old = vec![
        found("p", &manifest("p.a", None, &[("p.b", "0.2")], &[])),
        found("p", &manifest("p.b", None, &[], &[])),
    ];
    assert!(matches!(
        resolve(&pkg, &old, &BTreeMap::new()),
        Err(ResolveError::VersionMismatch { .. })
    ));
    let dup = vec![
        found("p", &manifest("p.a", Some("p.shared"), &[], &[])),
        found("p", &manifest("p.b", Some("p.shared"), &[], &[])),
    ];
    assert!(matches!(
        resolve(&pkg, &dup, &BTreeMap::new()),
        Err(ResolveError::DuplicateContract { .. })
    ));
    let cycle = vec![
        found("p", &manifest("p.a", None, &[("p.b", "0.1")], &[])),
        found("p", &manifest("p.b", None, &[("p.a", "0.1")], &[])),
        found("p", &manifest("p.c", None, &[], &[])),
    ];
    assert_eq!(
        resolve(&pkg, &cycle, &BTreeMap::new()),
        Err(ResolveError::Cycle(vec!["p.a".into(), "p.b".into()]))
    );
}

#[test]
fn a_package_overrides_a_std_module_by_implementing_its_contract() {
    let mut set = std_set();
    set.push(found(
        "game",
        &manifest("game.party", Some("std.party"), &[("std.chat", "0.1")], &[]),
    ));
    // Without the override, two modules implement std.party.
    let plain = package("[package]\nname = \"game\"\nuses = [\"std\"]\n");
    assert!(matches!(
        resolve(&plain, &set, &BTreeMap::new()),
        Err(ResolveError::DuplicateContract { .. })
    ));
    let pkg = package("[package]\nname = \"game\"\nuses = [\"std\"]\noverrides = [\"std.party\"]\n");
    let g = resolve(&pkg, &set, &BTreeMap::new()).unwrap();
    assert!(g.get("std.party").is_none());
    let p = g.implementer("std.party").unwrap();
    assert_eq!(p.key, "game.party");
    assert_eq!(p.overrides.as_deref(), Some("std.party"));
    // game.raids depends on the contract, so it now runs on game.party.
    assert_eq!(g.get("game.raids").unwrap().depends_on, ["game.party"]);
    assert!(g.render().contains("implements std.party, overrides std.party"));
    let unknown = package("[package]\nname = \"game\"\nuses = [\"std\"]\noverrides = [\"std.guilds\"]\n");
    assert_eq!(
        resolve(&unknown, &set, &BTreeMap::new()),
        Err(ResolveError::OverrideUnknown("std.guilds".into()))
    );
    let unimplemented = package("[package]\nname = \"game\"\nuses = [\"std\"]\noverrides = [\"std.mail\"]\n");
    assert!(matches!(
        resolve(&unimplemented, &set, &BTreeMap::new()),
        Err(ResolveError::OverrideUnimplemented { .. })
    ));
}

#[derive(Clone, Copy, PartialEq, Debug)]
struct Pinged(u32);

impl StateHash for Pinged {
    fn state_hash(&self, h: &mut StableHasher) {
        h.write_u32(self.0);
    }
}

impl Event for Pinged {
    const NAME: &'static str = "test.pinged";
}

struct Double(u32);

impl Query for Double {
    type Response = u32;
    const NAME: &'static str = "test.double";
}

#[test]
fn events_are_delivered_next_tick_and_bounded() {
    let mut q = Events::<Pinged>::with_capacity(2);
    assert!(q.send(Pinged(1)) && q.send(Pinged(2)));
    assert!(!q.send(Pinged(3)), "full");
    assert_eq!(q.dropped(), 1);
    assert!(q.read().is_empty(), "not readable in the tick it was sent");
    q.advance();
    assert_eq!(q.read(), [Pinged(1), Pinged(2)]);
    q.advance();
    assert!(q.read().is_empty(), "consumed after one tick");
}

#[test]
fn queries_answer_synchronously_until_disabled() {
    let mut world = World::new();
    let mut queries = Queries::default();
    queries.provide::<Double>("test.math", |_, q| q.0 * 2).unwrap();
    assert!(queries.provide::<Double>("test.other", |_, q| q.0).is_err());
    world.insert_resource(queries).unwrap();
    assert_eq!(super::ask(&world, &Double(21)), Ok(42));
    let before = world.state_hash();
    world
        .resource_mut::<Queries>()
        .unwrap()
        .set_enabled("test.math", false);
    assert_eq!(
        super::ask(&world, &Double(21)),
        Err(QueryError::FeatureDisabled("test.double"))
    );
    assert_ne!(world.state_hash(), before, "availability is simulation state");
    assert_eq!(
        Queries::default().ask(&world, &Double(1)),
        Err(QueryError::NoProvider("test.double"))
    );
}

#[test]
fn graph_actions_are_declared_keys_without_repeats() {
    let m = parse_manifest(
        "[module]\nkey = \"std.party\"\nversion = \"0.1\"\ngraph_actions = [\"std.party.summon\", \"std.party.mark\"]\n",
    )
    .unwrap();
    assert_eq!(m.graph_actions, ["std.party.summon", "std.party.mark"]);
    let none = parse_manifest("[module]\nkey = \"a\"\nversion = \"1\"\n").unwrap();
    assert!(none.graph_actions.is_empty());
    for bad in [
        "[module]\nkey = \"a\"\nversion = \"1\"\ngraph_actions = [\"Bad Name\"]\n",
        "[module]\nkey = \"a\"\nversion = \"1\"\ngraph_actions = [\"a.x\", \"a.x\"]\n",
        "[module]\nkey = \"a\"\nversion = \"1\"\ngraph_actions = \"a.x\"\n",
    ] {
        assert!(parse_manifest(bad).is_err(), "{bad}");
    }
}

#[test]
fn a_package_lists_the_client_modules_it_permits() {
    let p = package("[package]\nname = \"p\"\nclient_mods = [\"p.hud\", \"p.helper\"]\n");
    assert_eq!(p.client_mods, ["p.hud", "p.helper"]);
    assert!(package("[package]\nname = \"p\"\n").client_mods.is_empty());
    let long = format!(
        "[package]\nname = \"p\"\nclient_mods = [\"p.{}\"]\n",
        "x".repeat(40)
    );
    let mut many = String::from("[package]\nname = \"p\"\nclient_mods = [");
    for i in 0..33 {
        let _ = write!(many, "\"p.m{i}\",");
    }
    many.push_str("]\n");
    for bad in [
        "[package]\nname = \"p\"\nclient_mods = [\"P.Hud\"]\n".to_owned(),
        "[package]\nname = \"p\"\nclient_mods = [\"p.a\", \"p.a\"]\n".to_owned(),
        long,
        many,
    ] {
        assert!(parse_package(&bad).is_err(), "{bad}");
    }
}

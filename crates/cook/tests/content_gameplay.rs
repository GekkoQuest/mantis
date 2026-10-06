//! Gameplay graph importer (decision 0021): `*.graph.toml` cooks to MGPH in the gameplay
//! domain, the core catalog's rules fail at their file and line, declared actions are
//! enforced, and the ability table names only cooked graphs.

use std::collections::BTreeSet;
use std::sync::Arc;

use mantis_cook::importer::{CookError, Importer};
use mantis_cook::importers::content::gameplay::{Abilities, Graphs};
use mantis_cook::tree::ContentTree;
use mantis_cook::{Cook, CookOutput, importers};
use mantis_core::graph::{GraphCatalog, GraphId, MarkerSpec, Target};
use mantis_formats::bundle::{AssetKind, Domain};
use mantis_formats::gameplay_graph::{GraphAsset, GraphNodeKind};

type TestResult = Result<(), Box<dyn std::error::Error>>;

const PATH: &str = "graphs/burn.graph.toml";

const BURN: &str = r#"# a burn: cast, three ticks of damage with a chance of a flare, expire
name = "pkg.ability.burn"
entry = "start"

[node.start]
key = 1
kind = "marker"
marker = "cast_start"
next = "burn"

[node.burn]
key = 2
kind = "repeat"
counter = 0
times = 3
body = "wait"
done = "end"

[node.wait]
key = 3
kind = "delay"
ticks = 5
next = "tick"

[node.tick]
key = 4
kind = "marker"
marker = "tick"
counter = 0
next = "hit"

[node.hit]
key = 5
kind = "action"
action = "pkg.damage"
target = "target"
params = [10, -2]
next = "flare"

[node.flare]
key = 7
kind = "chance"
numerator = 1
denominator = 4
then = "spark"
otherwise = "burn"

[node.spark]
key = 8
kind = "action"
action = "pkg.flare"
next = "burn"

[node.end]
key = 6
kind = "marker"
marker = "expire"
offset = 2
"#;

fn cook_with(importers: Vec<Arc<dyn Importer>>, tree: &ContentTree) -> Result<CookOutput, Vec<CookError>> {
    Cook::new(importers).map_err(|e| vec![e])?.run(tree)
}

fn cook(tree: &ContentTree) -> Result<CookOutput, Vec<CookError>> {
    cook_with(importers::builtin(), tree)
}

fn tree(graph: &str) -> ContentTree {
    let mut t = ContentTree::new();
    t.insert(PATH, graph);
    t
}

fn line_of(text: &str, needle: &str) -> usize {
    text.lines().position(|l| l.contains(needle)).map_or(0, |i| i + 1)
}

fn assert_at(graph: &str, needle: &str, contains: &str) -> TestResult {
    assert_at_line(graph, line_of(graph, needle), contains)
}

fn assert_at_line(graph: &str, line: usize, contains: &str) -> TestResult {
    let errors = cook(&tree(graph)).err().ok_or("expected the cook to fail")?;
    let [e] = <[CookError; 1]>::try_from(errors).map_err(|e| format!("expected one error: {e:?}"))?;
    assert_eq!(e.file, PATH, "{e}");
    assert_eq!(e.line, line, "{e} (expected at line {line})");
    assert!(e.message.contains(contains), "{e} (expected `{contains}`)");
    Ok(())
}

#[test]
fn a_graph_cooks_into_the_gameplay_domain_and_loads_into_a_catalog() -> TestResult {
    let out = cook(&tree(BURN)).map_err(|e| format!("{e:?}"))?;
    let asset = out.get("graphs/burn.graph").ok_or("graph output")?;
    assert_eq!(asset.kind, AssetKind::GameplayGraph);
    assert_eq!(asset.domain, Domain::Gameplay);
    let g = GraphAsset::parse(&asset.bytes)?;
    assert_eq!(g.id(), GraphId::named("pkg.ability.burn"));
    assert_eq!(g.entry, 1);
    assert_eq!(g.actions, ["pkg.damage", "pkg.flare"]);
    assert_eq!(
        g.nodes.iter().map(|n| n.key).collect::<Vec<_>>(),
        [1, 2, 3, 4, 5, 6, 7, 8],
        "nodes in key order"
    );
    assert_eq!(g.marker_keys().collect::<Vec<_>>(), [1, 4, 6]);
    assert_eq!(
        g.node(5).map(|n| n.kind),
        Some(GraphNodeKind::Action {
            action: 0,
            target: Target::Target,
            params: [10, -2, 0, 0],
            next: Some(7),
        })
    );
    assert_eq!(
        g.node(6).map(|n| n.kind),
        Some(GraphNodeKind::Marker {
            marker: MarkerSpec::Expire,
            offset: 2,
            next: None,
        })
    );
    // A cell's catalog, with its own action ids, accepts it.
    let mut catalog = GraphCatalog::new();
    let flare = catalog.register_action("pkg.flare")?;
    let damage = catalog.register_action("pkg.damage")?;
    catalog.insert(g.to_core(|n| match n {
        "pkg.damage" => Some(damage),
        "pkg.flare" => Some(flare),
        _ => None,
    })?)?;
    // Deterministic.
    let again = cook(&tree(BURN)).map_err(|e| format!("{e:?}"))?;
    assert_eq!(again.get("graphs/burn.graph").map(|a| a.hash), Some(asset.hash));
    Ok(())
}

#[test]
fn graph_rules_fail_at_their_line() -> TestResult {
    let r = |from: &str, to: &str| BURN.replacen(from, to, 1);
    assert_at(
        &r("next = \"burn\"", "next = \"brun\""),
        "next = \"brun\"",
        "names no node",
    )?;
    assert_at(
        &r("body = \"wait\"", "body = \"nap\""),
        "body = \"nap\"",
        "names no node",
    )?;
    assert_at(
        &r("entry = \"start\"", "entry = \"go\""),
        "entry = \"go\"",
        "names no node",
    )?;
    assert_at(&r("key = 8", "key = 65535"), "key = 65535", "reserved")?;
    let dup = r("key = 3", "key = 1");
    assert_at_line(&dup, line_of(&dup, "[node.wait]") + 1, "is also node `start`")?;
    assert_at(&r("ticks = 5", "ticks = 0"), "ticks = 0", "at least 1 tick")?;
    assert_at(
        &r("denominator = 4", "denominator = 0"),
        "denominator = 0",
        "nonzero denominator",
    )?;
    assert_at(
        &r("counter = 0\ntimes", "counter = 4\ntimes"),
        "counter = 4",
        "counter is out of range",
    )?;
    assert_at(
        &r("kind = \"delay\"", "kind = \"wait\""),
        "kind = \"wait\"",
        "is not one of",
    )?;
    assert_at(
        &r("marker = \"expire\"", "marker = \"expires\""),
        "marker = \"expires\"",
        "is not one of",
    )?;
    assert_at(
        &r("params = [10, -2]", "params = [1, 2, 3, 4, 5]"),
        "params = [1, 2, 3, 4, 5]",
        "at most 4",
    )?;
    assert_at(
        &r("offset = 2", "offset = 2\nticks = 1"),
        "ticks = 1",
        "unknown key `ticks`",
    )?;
    // A cycle that never waits: the repeat's body jumps straight back.
    let spin = r("ticks = 5\nnext = \"tick\"", "ticks = 5\nnext = \"tick\"").replacen(
        "body = \"wait\"",
        "body = \"tick\"",
        1,
    );
    assert_at(&spin, "[node.burn]", "never passes a delay")?;
    Ok(())
}

#[test]
fn declared_actions_are_enforced() -> TestResult {
    let mut importers = importers::builtin();
    importers.retain(|i| i.name() != "graph.toml");
    importers.push(Arc::new(Graphs::with_actions(BTreeSet::from([
        "pkg.damage".to_owned()
    ]))));
    let errors = cook_with(importers.clone(), &tree(BURN))
        .err()
        .ok_or("pkg.flare is not declared")?;
    let [e] = errors.as_slice() else {
        return Err(format!("one error: {errors:?}").into());
    };
    assert_eq!(e.line, line_of(BURN, "action = \"pkg.flare\""));
    assert!(e.message.contains("not a graph action any module"), "{e}");
    importers.retain(|i| i.name() != "graph.toml");
    importers.push(Arc::new(Graphs::with_actions(BTreeSet::from([
        "pkg.damage".to_owned(),
        "pkg.flare".to_owned(),
    ]))));
    cook_with(importers, &tree(BURN)).map_err(|e| format!("{e:?}"))?;
    Ok(())
}

#[test]
fn the_ability_table_names_cooked_graphs() -> TestResult {
    let abilities =
        "# ability id, graph\n1 pkg.ability.burn\n2 pkg.ability.burn  # a second ability, same graph\n";
    let mut t = tree(BURN);
    t.insert("tables/abilities", abilities);
    let out = cook(&t).map_err(|e| format!("{e:?}"))?;
    let table = out.get("tables/abilities").ok_or("table")?;
    assert_eq!((table.kind, table.domain), (AssetKind::Table, Domain::Gameplay));
    assert_eq!(table.bytes, abilities.as_bytes());
    let refused = |text: &str, needle: &str, contains: &str| -> TestResult {
        let mut t = tree(BURN);
        t.insert("tables/abilities", text);
        let errors = cook(&t).err().ok_or("expected a refusal")?;
        let [e] = errors.as_slice() else {
            return Err(format!("one error: {errors:?}").into());
        };
        assert_eq!(
            (e.file.as_str(), e.line),
            ("tables/abilities", line_of(text, needle)),
            "{e}"
        );
        assert!(e.message.contains(contains), "{e}");
        Ok(())
    };
    refused(
        "1 pkg.ability.burn\n3 pkg.ability.frost\n",
        "3 pkg",
        "not a gameplay graph cooked",
    )?;
    let mut t = tree(BURN);
    t.insert("tables/abilities", "1 pkg.ability.burn\n1 pkg.ability.burn\n");
    let errors = cook(&t).err().ok_or("expected a refusal")?;
    assert!(
        errors
            .iter()
            .any(|e| e.line == 2 && e.message.contains("listed twice")),
        "{errors:?}"
    );
    refused("one pkg.ability.burn\n", "one", "not an ability id")?;
    refused("1 pkg.ability.burn extra\n", "extra", "<ability id> <graph name>")?;
    // The importer is the only one that claims the table.
    assert!(Abilities.accepts("tables/abilities"));
    Ok(())
}

#[test]
fn a_package_cook_reads_the_actions_its_modules_declare() -> TestResult {
    use mantis_cook::package::declared_actions;
    let toy = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../packages/toy/content");
    let declared = declared_actions(&toy)?.ok_or("the toy package resolves its modules")?;
    // Whatever the modules declare, every name is a dotted key.
    assert!(declared.iter().all(|a| a.contains('.')), "{declared:?}");
    // A content tree outside a package has no declarations to check against.
    assert_eq!(declared_actions(&std::env::temp_dir().join("content"))?, None);
    Ok(())
}

//! Presentation graph importer: `*.presentation.toml` cooks to MPRS, effects resolve by
//! source path to the phase-5 particle outputs, every rule fails at its file and line,
//! and cooking is deterministic.

use mantis_cook::importer::CookError;
use mantis_cook::tree::ContentTree;
use mantis_cook::{Cook, CookOutput, importers};
use mantis_core::graph::GraphId;
use mantis_formats::bundle::{AssetKind, Domain};
use mantis_formats::presentation::{ActionOp, Anchor, MarkerFilter, PresentationGraph};
use mantis_formats::skeleton::bone_name_hash;

type TestResult = Result<(), Box<dyn std::error::Error>>;

const PATH: &str = "presentation/bolt.presentation.toml";

const EFFECT: &str = r"[emitter.flash]
capacity = 16
duration = 0.5
lifetime_min = 0.2
lifetime_max = 0.3
burst.0 = [0.0, 16]
color.0 = [0.0, 1.0, 1.0, 1.0, 1.0]
size.0 = [0.0, 0.5]
";

/// The gameplay graph the bindings name: a marker of every kind, then a delay.
const GRAPH: &str = r#"name = "ability.bolt"
entry = "cast"

[node.cast]
key = 1
kind = "marker"
marker = "cast_start"
next = "land"

[node.land]
key = 2
kind = "marker"
marker = "impact"
next = "pulse"

[node.pulse]
key = 3
kind = "marker"
marker = "tick"
next = "custom"

[node.custom]
key = 4
kind = "marker"
marker = "package"
package = 7
next = "rest"

[node.rest]
key = 5
kind = "delay"
ticks = 2
"#;

/// Bindings deliberately out of marker order.
const BOLT: &str = r#"# reactions to the bolt ability
[binding.land]
graph = "ability.bolt"
node = 2
filter = "impact"

[binding.cast]
graph = "ability.bolt"
node = 1
filter = "cast_start"

[binding.pulse]
graph = "ability.bolt"
node = 3
filter = "tick"
tick = 3

[binding.custom]
graph = "ability.bolt"
node = 4
filter = "package"
kind = 7

[action.cast.0]
op = "spawn_effect"
effect = "effects/flash.particles.toml"
follow = true
offset = [0.0, 1.0, 0.0]

[action.cast.1]
op = "play_sound"
sound = 42
volume = 0.8
pitch = 1.25
delay = 0.1

[action.land.0]
op = "camera_shake"
anchor = "target"
amplitude = 1.5
frequency = 12
duration = 0.4
radius = 20

[action.land.1]
op = "anim_trigger"
anchor = "target"
parameter = "flinch"

[action.pulse.0]
op = "play_sound"
sound = 43

[action.custom.0]
op = "anim_trigger"
parameter = "glow"
"#;

fn cook(tree: &ContentTree) -> Result<CookOutput, Vec<CookError>> {
    Cook::new(importers::builtin()).map_err(|e| vec![e])?.run(tree)
}

fn tree_with(graph: &str) -> ContentTree {
    let mut t = ContentTree::new();
    t.insert("effects/flash.particles.toml", EFFECT);
    t.insert("grading/dusk.grading.toml", "contrast = 1.1\n");
    t.insert("graphs/bolt.graph.toml", GRAPH);
    t.insert(PATH, graph);
    t
}

fn line_of(text: &str, needle: &str) -> usize {
    text.lines().position(|l| l.contains(needle)).map_or(0, |i| i + 1)
}

fn assert_at(graph: &str, needle: &str, contains: &str) -> TestResult {
    let errors = cook(&tree_with(graph)).err().ok_or("expected the cook to fail")?;
    let [e] = <[CookError; 1]>::try_from(errors).map_err(|e| format!("expected one error: {e:?}"))?;
    assert_eq!(e.file, PATH, "{e}");
    assert_eq!(e.line, line_of(graph, needle), "{e} (expected at `{needle}`)");
    assert!(e.message.contains(contains), "{e} (expected `{contains}`)");
    Ok(())
}

#[test]
fn a_presentation_graph_cooks_sorted_with_resolved_effects() -> TestResult {
    let out = cook(&tree_with(BOLT)).map_err(|e| format!("{e:?}"))?;
    let asset = out.get("presentation/bolt.prs").ok_or("graph output")?;
    assert_eq!(asset.kind, AssetKind::Presentation);
    assert_eq!(asset.domain, Domain::Presentation);
    let g = PresentationGraph::parse(&asset.bytes)?;
    let graph = GraphId::named("ability.bolt").0;
    let keys: Vec<(u32, u16, MarkerFilter)> =
        g.bindings.iter().map(|b| (b.graph, b.node, b.filter)).collect();
    assert_eq!(
        keys,
        [
            (graph, 1, MarkerFilter::CastStart),
            (graph, 2, MarkerFilter::Impact),
            (graph, 3, MarkerFilter::Tick(3)),
            (graph, 4, MarkerFilter::Package(7)),
        ]
    );
    let effect = out.get("effects/flash.pfx").ok_or("effect")?.hash;
    let cast = g.bindings.first().ok_or("cast")?;
    let [spawn, sound] = cast.actions.as_slice() else {
        return Err("two cast actions".into());
    };
    assert_eq!(
        spawn.op,
        ActionOp::SpawnEffect {
            effect,
            scale: 1.0,
            follow: true
        }
    );
    assert_eq!(spawn.offset, [0.0, 1.0, 0.0]);
    assert_eq!(spawn.anchor, Anchor::Source);
    assert_eq!(
        sound.op,
        ActionOp::PlaySound {
            sound: 42,
            volume: 0.8,
            pitch: 1.25,
            follow: false
        }
    );
    assert_eq!(sound.delay, 0.1);
    let land = g.bindings.get(1).ok_or("land")?;
    let [shake, trigger] = land.actions.as_slice() else {
        return Err("two land actions".into());
    };
    assert_eq!(
        shake.op,
        ActionOp::CameraShake {
            amplitude: 1.5,
            frequency: 12.0,
            duration: 0.4,
            radius: 20.0
        }
    );
    assert_eq!(shake.anchor, Anchor::Target);
    assert_eq!(
        trigger.op,
        ActionOp::AnimTrigger {
            parameter: bone_name_hash("flinch")
        }
    );
    // A pulse sound with every default.
    let pulse = g.bindings.get(2).and_then(|b| b.actions.first()).ok_or("pulse")?;
    assert_eq!(
        pulse.op,
        ActionOp::PlaySound {
            sound: 43,
            volume: 1.0,
            pitch: 1.0,
            follow: false
        }
    );
    assert_eq!((pulse.delay, pulse.offset), (0.0, [0.0; 3]));
    Ok(())
}

#[test]
fn effect_references_resolve_by_source_path() -> TestResult {
    let m = BOLT.replace("effects/flash.particles.toml", "effects/missing.particles.toml");
    assert_at(&m, "effect = \"effects/missing", "effects/missing.particles.toml")?;
    // A cooked source of another kind is not an effect.
    let m = BOLT.replace("effects/flash.particles.toml", "grading/dusk.grading.toml");
    assert_at(&m, "effect = \"grading/dusk", "ParticleEffect")?;
    Ok(())
}

#[test]
fn binding_rules_fail_at_their_line() -> TestResult {
    let r = |from: &str, to: &str| BOLT.replacen(from, to, 1);
    // Two bindings on one marker and filter: reported at the later one.
    assert_at(
        &r(
            "tick = 3",
            "tick = 3\n[binding.again]\ngraph = \"ability.bolt\"\nnode = 1\nfilter = \"cast_start\"\n[action.again.0]\nop = \"anim_trigger\"\nparameter = \"x\"",
        ),
        "[binding.again]",
        "repeats the marker",
    )?;
    assert_at(
        &r("[action.custom.0]", "[action.nobody.0]"),
        "[action.nobody.0]",
        "unknown binding `nobody`",
    )?;
    // A binding without actions.
    let m = r(
        "[action.custom.0]\nop = \"anim_trigger\"\nparameter = \"glow\"\n",
        "",
    );
    assert_at(&m, "[binding.custom]", "0 actions")?;
    assert_at(
        &r("[action.cast.1]", "[action.cast.2]"),
        "[action.cast.2]",
        "without gaps",
    )?;
    assert_at(
        &r("[action.cast.1]", "[action.cast.one]"),
        "[action.cast.one]",
        "not an action number",
    )?;
    assert_at(
        &r("[action.cast.1]", "[action.cast]"),
        "[action.cast]",
        "expected `[action.<binding>.<n>]`",
    )?;
    assert_at(
        &r("filter = \"impact\"", "filter = \"hit\""),
        "filter = \"hit\"",
        "is not one of",
    )?;
    assert_at(&r("kind = 7\n", ""), "[binding.custom]", "missing `kind`")?;
    assert_at(
        &r("node = 2", "node = 70000"),
        "node = 70000",
        "does not fit in u16",
    )?;
    assert_at(
        &r("node = 1", "node = 1\ntick = 1"),
        "tick = 1",
        "unknown key `tick`",
    )?;
    assert_at(
        &r("[binding.land]", "[binding.land.a]"),
        "[binding.land.a]",
        "may not contain",
    )?;
    assert_at(
        &r(
            "graph = \"ability.bolt\"\nnode = 2\nfilter = \"impact\"",
            "node = 2\nfilter = \"impact\"",
        ),
        "[binding.land]",
        "missing `graph`",
    )?;
    Ok(())
}

#[test]
fn action_rules_fail_at_their_line() -> TestResult {
    let r = |from: &str, to: &str| BOLT.replacen(from, to, 1);
    assert_at(&r("delay = 0.1", "delay = 61"), "delay = 61", "outside [0, 60]")?;
    assert_at(&r("volume = 0.8", "volume = 5"), "volume = 5", "outside [0, 4]")?;
    assert_at(
        &r("pitch = 1.25", "pitch = 0.1"),
        "pitch = 0.1",
        "outside [0.25, 4]",
    )?;
    assert_at(
        &r("amplitude = 1.5", "amplitude = 0"),
        "amplitude = 0",
        "outside (0, 10]",
    )?;
    assert_at(
        &r("frequency = 12", "frequency = 101"),
        "frequency = 101",
        "outside (0, 100]",
    )?;
    assert_at(
        &r("duration = 0.4", "duration = 11"),
        "duration = 11",
        "outside (0, 10]",
    )?;
    assert_at(&r("radius = 20", "radius = -1"), "radius = -1", "outside (0")?;
    assert_at(
        &r("follow = true", "follow = true\nscale = 0"),
        "scale = 0",
        "outside (0, 100]",
    )?;
    assert_at(
        &r("anchor = \"target\"", "anchor = \"camera\""),
        "anchor = \"camera\"",
        "is not one of",
    )?;
    assert_at(
        &r("op = \"play_sound\"", "op = \"play_music\""),
        "op = \"play_music\"",
        "is not one of",
    )?;
    assert_at(
        &r("sound = 43", "sound = 43\nradius = 3"),
        "radius = 3",
        "unknown key `radius`",
    )?;
    assert_at(
        &r("sound = 42", "sound = -1"),
        "sound = -1",
        "does not fit in u32",
    )?;
    assert_at(
        &r("parameter = \"flinch\"", "parameter = \"\""),
        "parameter = \"\"",
        "empty",
    )?;
    assert_at(
        &r("offset = [0.0, 1.0, 0.0]", "offset = [0.0, 1.0]"),
        "offset =",
        "3 numbers",
    )?;
    Ok(())
}

#[test]
fn presentation_graphs_cook_deterministically() -> TestResult {
    let a = cook(&tree_with(BOLT)).map_err(|e| format!("{e:?}"))?;
    let b = cook(&tree_with(BOLT)).map_err(|e| format!("{e:?}"))?;
    let bytes = |o: &CookOutput| o.get("presentation/bolt.prs").map(|x| x.bytes.clone());
    assert_eq!(bytes(&a), bytes(&b));
    assert_eq!(
        a.bundle(Domain::Presentation, 1).hash(),
        b.bundle(Domain::Presentation, 1).hash()
    );
    Ok(())
}

#[test]
fn bindings_name_marker_nodes_of_cooked_graphs() -> TestResult {
    let r = |from: &str, to: &str| BOLT.replacen(from, to, 1);
    assert_at(
        &r(
            "graph = \"ability.bolt\"
node = 2",
            "graph = \"ability.blot\"
node = 2",
        ),
        "graph = \"ability.blot\"",
        "not a gameplay graph cooked in this package",
    )?;
    assert_at(
        &r(
            "node = 1
filter = \"cast_start\"",
            "node = 5
filter = \"cast_start\"",
        ),
        "node = 5",
        "not a marker node (its marker nodes: 1, 2, 3, 4)",
    )?;
    assert_at(
        &r(
            "node = 1
filter = \"cast_start\"",
            "node = 9
filter = \"cast_start\"",
        ),
        "node = 9",
        "not a marker node",
    )?;
    assert_at(
        &r(
            "node = 1
filter = \"cast_start\"",
            "node = 1
filter = \"expire\"",
        ),
        "filter = \"expire\"",
        "never matches",
    )?;
    assert_at(
        &r("kind = 7", "kind = 8"),
        "filter = \"package\"",
        "never matches",
    )?;
    // `any` matches every marker kind.
    let any = r(
        "node = 1
filter = \"cast_start\"",
        "node = 1
filter = \"any\"",
    );
    cook(&tree_with(&any)).map_err(|e| format!("{e:?}"))?;
    Ok(())
}

//! Particle effect importer: `*.particles.toml` cooks to MPFX with every emitter field,
//! every rule fails at its file and line, and cooking is deterministic.

use std::fmt::Write as _;

use mantis_cook::importer::CookError;
use mantis_cook::tree::ContentTree;
use mantis_cook::{Cook, CookOutput, importers};
use mantis_formats::bundle::{AssetKind, Domain};
use mantis_formats::particle_effect::{BlendMode, Burst, EmitterShape, ParticleEffect, SimulationSpace};

type TestResult = Result<(), Box<dyn std::error::Error>>;

const PATH: &str = "effects/spark.particles.toml";

const SPARK: &str = r#"# sparks and a smoke trail
[emitter.sparks]
capacity = 256
duration = 1.0
looping = false
rate = 0
lifetime_min = 0.4
lifetime_max = 0.8
speed_min = 2.0
speed_max = 4.0
acceleration = [0.0, -9.8, 0.0]
drag = 0.5
shape = "cone"
angle = 0.6
radius = 0.1
blend = "additive"
space = "world"
burst.0 = [0.0, 64]
burst.1 = [0.5, 32]
color.0 = [0.0, 1.0, 0.8, 0.3, 1.0]
color.1 = [1.0, 1.0, 0.2, 0.0, 0.0]
size.0 = [0.0, 0.1]
size.1 = [1.0, 0.0]

[emitter.smoke]
capacity = 64
duration = 2.0
looping = true
rate = 12.5
lifetime_min = 1.0
lifetime_max = 1.5
shape = "sphere"
radius = 0.25
blend = "alpha"
space = "local"
color.0 = [0.0, 0.5, 0.5, 0.5, 0.6]
size.0 = [0.0, 0.0]
size.1 = [0.5, 0.4]
size.2 = [1.0, 0.8]
"#;

fn cook(tree: &ContentTree) -> Result<CookOutput, Vec<CookError>> {
    Cook::new(importers::builtin()).map_err(|e| vec![e])?.run(tree)
}

fn tree_with(effect: &str) -> ContentTree {
    let mut t = ContentTree::new();
    t.insert(PATH, effect);
    t
}

fn line_of(text: &str, needle: &str) -> usize {
    text.lines().position(|l| l.contains(needle)).map_or(0, |i| i + 1)
}

fn assert_at(effect: &str, needle: &str, contains: &str) -> TestResult {
    let errors = cook(&tree_with(effect))
        .err()
        .ok_or("expected the cook to fail")?;
    let [e] = <[CookError; 1]>::try_from(errors).map_err(|e| format!("expected one error: {e:?}"))?;
    assert_eq!(e.file, PATH, "{e}");
    assert_eq!(e.line, line_of(effect, needle), "{e} (expected at `{needle}`)");
    assert!(e.message.contains(contains), "{e} (expected `{contains}`)");
    Ok(())
}

#[test]
fn an_effect_cooks_with_every_emitter_field() -> TestResult {
    let out = cook(&tree_with(SPARK)).map_err(|e| format!("{e:?}"))?;
    let asset = out.get("effects/spark.pfx").ok_or("effect output")?;
    assert_eq!(asset.kind, AssetKind::ParticleEffect);
    assert_eq!(asset.domain, Domain::Presentation);
    let fx = ParticleEffect::parse(&asset.bytes)?;
    let [sparks, smoke] = fx.emitters.as_slice() else {
        return Err("two emitters, in file order".into());
    };
    assert_eq!(sparks.capacity, 256);
    assert_eq!(sparks.duration, 1.0);
    assert!(!sparks.looping);
    assert_eq!(sparks.rate, 0.0);
    assert_eq!((sparks.lifetime_min, sparks.lifetime_max), (0.4, 0.8));
    assert_eq!((sparks.speed_min, sparks.speed_max), (2.0, 4.0));
    assert_eq!(sparks.acceleration, [0.0, -9.8, 0.0]);
    assert_eq!(sparks.drag, 0.5);
    assert_eq!(
        sparks.shape,
        EmitterShape::Cone {
            angle: 0.6,
            radius: 0.1
        }
    );
    assert_eq!(sparks.blend, BlendMode::Additive);
    assert_eq!(sparks.space, SimulationSpace::World);
    assert_eq!(
        sparks.bursts,
        vec![Burst { time: 0.0, count: 64 }, Burst { time: 0.5, count: 32 }]
    );
    assert_eq!(sparks.color.len(), 2);
    assert_eq!(
        sparks.color.get(1).map(|k| (k.t, k.value)),
        Some((1.0, [1.0, 0.2, 0.0, 0.0]))
    );
    assert_eq!(
        sparks.size.iter().map(|k| (k.t, k.value[0])).collect::<Vec<_>>(),
        [(0.0, 0.1), (1.0, 0.0)]
    );
    assert!(smoke.looping);
    assert_eq!(smoke.rate, 12.5);
    assert_eq!(smoke.shape, EmitterShape::Sphere { radius: 0.25 });
    assert_eq!(smoke.blend, BlendMode::Alpha);
    assert_eq!(smoke.space, SimulationSpace::Local);
    // Defaults: speeds 0, no acceleration or drag.
    assert_eq!((smoke.speed_min, smoke.speed_max, smoke.drag), (0.0, 0.0, 0.0));
    assert_eq!(smoke.acceleration, [0.0; 3]);
    assert_eq!(smoke.size.len(), 3);
    Ok(())
}

#[test]
fn emitter_rules_fail_at_their_line() -> TestResult {
    let r = |from: &str, to: &str| SPARK.replacen(from, to, 1);
    assert_at(
        &r("capacity = 256", "capacity = 0"),
        "capacity = 0",
        "outside 1 to 65536",
    )?;
    assert_at(
        &r("duration = 1.0", "duration = 0"),
        "duration = 0",
        "must be > 0",
    )?;
    assert_at(&r("rate = 0", "rate = -1"), "rate = -1", "must be >= 0")?;
    assert_at(
        &r("lifetime_min = 0.4", "lifetime_min = 0"),
        "lifetime_min = 0",
        "must be > 0",
    )?;
    assert_at(
        &r("lifetime_max = 0.8", "lifetime_max = 0.2"),
        "lifetime_max = 0.2",
        "below",
    )?;
    assert_at(
        &r("speed_max = 4.0", "speed_max = 1.0"),
        "speed_max = 1.0",
        "below",
    )?;
    assert_at(&r("drag = 0.5", "drag = -0.5"), "drag = -0.5", ">= 0")?;
    assert_at(&r("angle = 0.6", "angle = 4"), "angle = 4", "(0, pi]")?;
    assert_at(&r("radius = 0.25", "radius = -1"), "radius = -1", ">= 0")?;
    assert_at(
        &r("shape = \"cone\"", "shape = \"point\""),
        "angle = 0.6",
        "a point emitter has no `angle`",
    )?;
    assert_at(
        &r("shape = \"cone\"", "shape = \"torus\""),
        "shape = \"torus\"",
        "is not one of",
    )?;
    assert_at(
        &r("blend = \"additive\"", "blend = \"screen\""),
        "blend = \"screen\"",
        "is not one of",
    )?;
    assert_at(
        &r("acceleration = [0.0, -9.8, 0.0]", "acceleration = [0.0, -9.8]"),
        "acceleration =",
        "3 numbers",
    )?;
    assert_at(
        &r("capacity = 256", "capacity = 256\nspin = 1"),
        "spin = 1",
        "unknown key `spin`",
    )?;
    Ok(())
}

#[test]
fn burst_and_curve_rules_fail_at_their_line() -> TestResult {
    let r = |from: &str, to: &str| SPARK.replacen(from, to, 1);
    assert_at(
        &r("burst.1 = [0.5, 32]", "burst.1 = [1.0, 32]"),
        "burst.1 =",
        "within 0 to the duration",
    )?;
    assert_at(
        &r("burst.1 = [0.5, 32]", "burst.1 = [0.5, 300]"),
        "burst.1 =",
        "1 to the capacity 256",
    )?;
    assert_at(
        &r("burst.1 = [0.5, 32]", "burst.1 = [0.5, 2.5]"),
        "burst.1 =",
        "whole number",
    )?;
    assert_at(
        &r("burst.1 = [0.5, 32]", "burst.2 = [0.5, 32]"),
        "burst.2 =",
        "without gaps",
    )?;
    assert_at(
        &r("burst.1 = [0.5, 32]", "burst.x = [0.5, 32]"),
        "burst.x =",
        "burst.<n>",
    )?;
    assert_at(
        &r("burst.1 = [0.5, 32]", "burst.1 = [0.5]"),
        "burst.1 =",
        "2 numbers",
    )?;
    // Never emits: rate 0 and no burst.
    let quiet = r("burst.0 = [0.0, 64]\nburst.1 = [0.5, 32]\n", "");
    assert_at(&quiet, "[emitter.sparks]", "never emits")?;
    assert_at(
        &r(
            "color.1 = [1.0, 1.0, 0.2, 0.0, 0.0]",
            "color.1 = [1.0, 1.0, 0.2, 0.0, 2.0]",
        ),
        "color.1 =",
        "alpha within 0 to 1",
    )?;
    assert_at(
        &r(
            "color.1 = [1.0, 1.0, 0.2, 0.0, 0.0]",
            "color.1 = [1.0, -1.0, 0.2, 0.0, 0.0]",
        ),
        "color.1 =",
        "RGB must be >= 0",
    )?;
    assert_at(
        &r(
            "color.1 = [1.0, 1.0, 0.2, 0.0, 0.0]",
            "color.1 = [0.0, 1.0, 0.2, 0.0, 0.0]",
        ),
        "color.1 =",
        "strictly increase",
    )?;
    assert_at(
        &r(
            "color.1 = [1.0, 1.0, 0.2, 0.0, 0.0]",
            "color.1 = [1.5, 1.0, 0.2, 0.0, 0.0]",
        ),
        "color.1 =",
        "outside 0 to 1",
    )?;
    let no_color = r(
        "color.0 = [0.0, 1.0, 0.8, 0.3, 1.0]\ncolor.1 = [1.0, 1.0, 0.2, 0.0, 0.0]\n",
        "",
    );
    assert_at(&no_color, "[emitter.sparks]", "1 to 4 keys")?;
    let five = r(
        "color.1 = [1.0, 1.0, 0.2, 0.0, 0.0]",
        "color.1 = [0.2, 1, 1, 1, 1]\ncolor.2 = [0.4, 1, 1, 1, 1]\ncolor.3 = [0.6, 1, 1, 1, 1]\ncolor.4 = [0.8, 1, 1, 1, 1]",
    );
    assert_at(&five, "[emitter.sparks]", "1 to 4 keys")?;
    // Sizes: 0 only at the ends, and not all 0.
    assert_at(
        &r("size.1 = [0.5, 0.4]", "size.1 = [0.5, 0.0]"),
        "size.1 = [0.5, 0.0]",
        "only at the first or last",
    )?;
    assert_at(
        &r("size.0 = [0.0, 0.1]", "size.0 = [0.0, 0.0]"),
        "[emitter.sparks]",
        "at least one must be positive",
    )?;
    assert_at(
        &r("size.0 = [0.0, 0.1]", "size.0 = [0.0, -0.1]"),
        "size.0 =",
        "sizes are > 0",
    )?;
    Ok(())
}

#[test]
fn effect_level_rules_fail() -> TestResult {
    let mut nine = String::new();
    for i in 0..9 {
        write!(
            nine,
            "[emitter.e{i}]\ncapacity = 1\nduration = 1\nrate = 1\nlifetime_min = 1\nlifetime_max = 1\ncolor.0 = [0, 1, 1, 1, 1]\nsize.0 = [0, 1]\n"
        )?;
    }
    assert_at(&nine, "[emitter.e8]", "1 to 8")?;
    let errors = cook(&tree_with("# nothing\n")).err().ok_or("no emitters")?;
    assert!(
        errors
            .iter()
            .any(|e| e.file == PATH && e.message.contains("0 emitters")),
        "{errors:?}"
    );
    assert_at(
        &format!("{SPARK}\n[emitters.extra]\n"),
        "[emitters.extra]",
        "unknown table",
    )?;
    assert_at(&format!("scale = 2\n{SPARK}"), "scale = 2", "unknown key")?;
    Ok(())
}

#[test]
fn effects_cook_deterministically() -> TestResult {
    let a = cook(&tree_with(SPARK)).map_err(|e| format!("{e:?}"))?;
    let b = cook(&tree_with(SPARK)).map_err(|e| format!("{e:?}"))?;
    assert_eq!(
        a.get("effects/spark.pfx").map(|x| x.hash),
        b.get("effects/spark.pfx").map(|x| x.hash)
    );
    assert_eq!(
        a.bundle(Domain::Presentation, 1).hash(),
        b.bundle(Domain::Presentation, 1).hash()
    );
    Ok(())
}

//! Animation importers: skeletons, clips, graphs, and VAT bakes cook, bind with
//! `mantis_anim`, and report errors at their lines; the VAT payload round-trips.

#![allow(clippy::too_many_lines, clippy::indexing_slicing)]

use std::sync::Arc;

use mantis_anim::skeleton::inverse_bind_matrices;
use mantis_anim::{AnimGraph, Clip, Skeleton, bake_vat};
use mantis_cook::importers::geometry::vat::{VatAsset, skinned_input, vat_asset};
use mantis_cook::tree::ContentTree;
use mantis_cook::{Cook, CookOutput, importers};
use mantis_formats::FormatError;
use mantis_formats::anim_clip::{Channel, ClipAsset, Interpolation};
use mantis_formats::anim_graph::{CompareOp, GraphAsset, LayerMode, NodeDef, ParameterKind};
use mantis_formats::bundle::{AssetKind, Domain};
use mantis_formats::mesh::MeshAsset;
use mantis_formats::skeleton::{SkeletonAsset, bone_name_hash};

type TestResult = Result<(), Box<dyn std::error::Error>>;

fn run(tree: &ContentTree) -> Result<CookOutput, String> {
    let cook = Cook::new(importers::builtin()).map_err(|e| e.to_string())?;
    cook.run(tree).map_err(|e| format!("{e:?}"))
}

fn errors(tree: &ContentTree) -> Result<Vec<String>, String> {
    let cook = Cook::new(importers::builtin()).map_err(|e| e.to_string())?;
    match cook.run(tree) {
        Ok(_) => Err("expected the cook to fail".into()),
        Err(e) => Ok(e.iter().map(ToString::to_string).collect()),
    }
}

/// Asserts that cooking `tree` plus `path = text` fails at `expect` (a `file:line:` prefix).
fn fails_at(base: &ContentTree, path: &str, text: &str, expect: &str) -> TestResult {
    let mut t = base.clone();
    t.insert(path, text);
    let shown = errors(&t)?;
    assert!(
        shown.iter().any(|e| e.starts_with(expect)),
        "expected `{expect}` for:\n{text}\ngot {shown:?}"
    );
    Ok(())
}

const BODY: &str = "\
[bone.root]
[bone.hip]
parent = \"root\"
translation = [0.0, 1.0, 0.0]
[bone.knee]
parent = \"hip\"
translation = [0.0, -0.5, 0.0]
rotation = [0.0, 0.0, 0.0, 1.0]
[bone.ankle]
parent = \"knee\"
translation = [0.0, -0.5, 0.0]
[bone.head]
parent = \"root\"
translation = [0.0, 1.6, 0.0]
rotation = [0.0, 0.7071068, 0.0, 0.7071068]
scale = [1.0, 1.0, 1.0]
";

const WALK: &str = "\
skeleton = \"skeletons/body.skeleton.toml\"
duration = 1.0
sample_rate = 30.0
looping = true
root_motion = true

[track.knee.rotation]
times = [0.0, 0.5, 1.0]
values = [0, 0, 0, 1, 0.3826834, 0, 0, 0.9238795, 0, 0, 0, 1]

[track.root.translation]
interpolation = \"linear\"
times = [0.0, 1.0]
values = [0, 0, 0, 0, 0, 1.5]
";

const RUN: &str = "\
skeleton = \"skeletons/body.skeleton.toml\"
duration = 0.5
looping = true

[track.hip.rotation]
interpolation = \"step\"
times = [0.0, 0.25]
values = [0, 0, 0, 1.0005, 0, 0, 0.5, 0.8660254]
";

const GRAPH: &str = "\
skeleton = \"skeletons/body.skeleton.toml\"

[clip.walk]
path = \"clips/walk.clip.toml\"
[clip.run]
path = \"clips/run.clip.toml\"
[clip.stroll]
path = \"clips/walk.clip.toml\"

[parameter.speed]
kind = \"float\"
default = 0.25
[parameter.alert]
kind = \"bool\"
default = true
[parameter.jump]
kind = \"trigger\"

[node.main]
kind = \"state_machine\"
states = [\"idle\", \"move\"]
entry = \"idle\"
[node.idle]
kind = \"clip\"
clip = \"stroll\"
speed = 0.0
[node.move]
kind = \"blend1d\"
parameter = \"speed\"
children = [\"slow\", \"fast\"]
thresholds = [0.0, 1.0]
[node.slow]
kind = \"clip\"
clip = \"walk\"
[node.fast]
kind = \"clip\"
clip = \"run\"
speed = 1.5
[node.lean]
kind = \"clip\"
clip = \"run\"

[transition.main.start]
from = \"idle\"
to = \"move\"
crossfade = 0.2
conditions = [\"speed > 0.1\", \"alert == true\"]
[transition.main.stop]
from = \"any\"
to = \"idle\"
exit_time = 0.9
conditions = [\"jump triggered\"]

[layer.base]
node = \"main\"
[layer.upper]
node = \"lean\"
weight = 0.5
mode = \"additive\"
reference = \"walk\"
mask = [\"head\", \"root\"]

[foot.left]
root = \"hip\"
mid = \"knee\"
tip = \"ankle\"
pole = [0.0, 0.0, 1.0]

[look_at]
head = \"head\"
axis = [0.0, 0.0, 2.0]
max_angle = 1.2
";

fn body_tree() -> ContentTree {
    let mut t = ContentTree::new();
    t.insert("skeletons/body.skeleton.toml", BODY);
    t.insert("clips/walk.clip.toml", WALK);
    t.insert("clips/run.clip.toml", RUN);
    t
}

#[test]
fn skeletons_cook_with_inverse_binds_and_bind() -> TestResult {
    let out = run(&body_tree())?;
    let asset = out.get("skeletons/body.skeleton").ok_or("skeleton output")?;
    assert_eq!(asset.kind, AssetKind::Skeleton);
    assert_eq!(asset.domain, Domain::Presentation);
    let skeleton = SkeletonAsset::parse(&asset.bytes)?;
    let parents: Vec<Option<u16>> = skeleton.bones.iter().map(|b| b.parent).collect();
    assert_eq!(parents, [None, Some(0), Some(1), Some(2), Some(0)]);
    assert_eq!(skeleton.find(bone_name_hash("ankle")), Some(3));
    let expected = inverse_bind_matrices(&skeleton.bones);
    for (bone, inverse) in skeleton.bones.iter().zip(&expected) {
        assert_eq!(&bone.inverse_bind, inverse);
    }
    // The head's quaternion was normalized; its inverse bind undoes a 1.6 lift.
    let head = &skeleton.bones[4];
    let len: f32 = head.rotation.iter().map(|c| c * c).sum::<f32>().sqrt();
    assert!((len - 1.0).abs() < 1e-6);
    let bound = Skeleton::new(&skeleton)?;
    assert_eq!(bound.bone_count(), 5);
    assert_eq!(bound.parent(3), Some(2));
    Ok(())
}

#[test]
fn skeleton_errors_name_their_lines() -> TestResult {
    let base = ContentTree::new();
    let path = "skeletons/bad.skeleton.toml";
    let at = |line: usize| format!("{path}:{line}:");
    fails_at(&base, path, "[bone.a]\n[bone.b]\nparent = \"c\"\n", &at(3))?;
    fails_at(&base, path, "[bone.b]\nparent = \"a\"\n[bone.a]\n", &at(2))?;
    fails_at(&base, path, "[bone.a]\nrotation = [0.0, 0.0, 0.5, 0.5]\n", &at(2))?;
    fails_at(&base, path, "[bone.a]\nrotation = [0.0, 0.0, 1.0]\n", &at(2))?;
    fails_at(&base, path, "[bone.a]\nscale = [1.0, 0.0, 1.0]\n", &at(2))?;
    fails_at(&base, path, "[bone.a]\nlength = 2.0\n", &at(2))?;
    fails_at(&base, path, "[bone.a]\n[joint.b]\n", &at(2))?;
    fails_at(&base, path, "[bone.a]\ntranslation = [0, 1, \"x\"]\n", &at(2))?;
    fails_at(&base, path, "# nothing\n", &format!("{path}:"))?;
    Ok(())
}

#[test]
fn clips_cook_sorted_normalized_and_bind() -> TestResult {
    let out = run(&body_tree())?;
    let asset = out.get("clips/walk.clip").ok_or("walk output")?;
    assert_eq!(asset.kind, AssetKind::AnimClip);
    let walk = ClipAsset::parse(&asset.bytes)?;
    assert!(walk.looping && walk.root_motion);
    assert_eq!(walk.bone_count, 5);
    let order: Vec<(u16, Channel)> = walk.tracks.iter().map(|t| (t.bone, t.channel)).collect();
    assert_eq!(order, [(0, Channel::Translation), (2, Channel::Rotation)]);
    let run_clip = ClipAsset::parse(&out.get("clips/run.clip").ok_or("run output")?.bytes)?;
    assert_eq!(run_clip.sample_rate, 30.0, "the default");
    let track = &run_clip.tracks[0];
    assert_eq!(track.interpolation, Interpolation::Step);
    // A key within tolerance of unit length is normalized.
    let q = &track.values[..4];
    assert_eq!(q, [0.0, 0.0, 0.0, 1.0]);
    let skeleton = Skeleton::new(&SkeletonAsset::parse(
        &out.get("skeletons/body.skeleton").ok_or("skeleton")?.bytes,
    )?)?;
    let clip = Clip::new(&walk)?;
    assert_eq!(clip.bone_count(), skeleton.bone_count());
    let mut pose = vec![mantis_anim::Transform::IDENTITY; 5];
    clip.sample(skeleton.bind_pose(), 0.5, &mut pose);
    assert!((pose[0].translation.z - 0.75).abs() < 1e-5);
    Ok(())
}

#[test]
fn clip_errors_name_their_lines() -> TestResult {
    let mut base = ContentTree::new();
    base.insert("skeletons/body.skeleton.toml", BODY);
    let path = "clips/bad.clip.toml";
    let at = |line: usize| format!("{path}:{line}:");
    let head = "skeleton = \"skeletons/body.skeleton.toml\"\nduration = 1.0\n";
    // Unknown bone: the track header line.
    fails_at(
        &base,
        path,
        &format!("{head}[track.tail.rotation]\ntimes = [0.0]\nvalues = [0, 0, 0, 1]\n"),
        &at(3),
    )?;
    // Non-increasing times.
    fails_at(
        &base,
        path,
        &format!(
            "{head}[track.hip.translation]\ntimes = [0.0, 0.5, 0.5]\nvalues = [0, 0, 0, 1, 1, 1, 2, 2, 2]\n"
        ),
        &at(4),
    )?;
    // A time beyond the duration.
    fails_at(
        &base,
        path,
        &format!("{head}[track.hip.translation]\ntimes = [0.0, 1.5]\nvalues = [0, 0, 0, 1, 1, 1]\n"),
        &at(4),
    )?;
    // Wrong value count.
    fails_at(
        &base,
        path,
        &format!("{head}[track.hip.scale]\ntimes = [0.0]\nvalues = [1, 1]\n"),
        &at(5),
    )?;
    // A rotation key that is not a unit quaternion.
    fails_at(
        &base,
        path,
        &format!("{head}[track.hip.rotation]\ntimes = [0.0, 1.0]\nvalues = [0, 0, 0, 1, 0, 0, 0, 3]\n"),
        &at(5),
    )?;
    // Unknown channel and interpolation.
    fails_at(
        &base,
        path,
        &format!("{head}[track.hip.shear]\ntimes = [0.0]\nvalues = [1]\n"),
        &at(3),
    )?;
    fails_at(
        &base,
        path,
        &format!("{head}[track.hip.scale]\ninterpolation = \"cubic\"\ntimes = [0.0]\nvalues = [1, 1, 1]\n"),
        &at(4),
    )?;
    // Root motion without a root track.
    fails_at(
        &base,
        path,
        &format!("{head}root_motion = true\n[track.hip.scale]\ntimes = [0.0]\nvalues = [1, 1, 1]\n"),
        &at(3),
    )?;
    // A skeleton that is not cooked, and a zero duration.
    fails_at(
        &base,
        path,
        "skeleton = \"skeletons/none.skeleton.toml\"\nduration = 1.0\n",
        &at(1),
    )?;
    fails_at(
        &base,
        path,
        "skeleton = \"skeletons/body.skeleton.toml\"\nduration = 0\n",
        &at(2),
    )?;
    Ok(())
}

#[test]
fn graphs_cook_resolve_clip_hashes_and_bind() -> TestResult {
    let mut t = body_tree();
    t.insert("graphs/body.animgraph.toml", GRAPH);
    let out = run(&t)?;
    let asset = out.get("graphs/body.animgraph").ok_or("graph output")?;
    assert_eq!(asset.kind, AssetKind::AnimGraph);
    let graph = GraphAsset::parse(&asset.bytes)?;
    let walk = out.get("clips/walk.clip").ok_or("walk")?;
    let run_clip = out.get("clips/run.clip").ok_or("run")?;
    // `stroll` names the same clip as `walk`, so it shares its entry.
    assert_eq!(graph.clips, [walk.hash, run_clip.hash]);
    assert_eq!(graph.bone_count, 5);
    let kinds: Vec<ParameterKind> = graph.parameters.iter().map(|p| p.kind).collect();
    assert_eq!(
        kinds,
        [ParameterKind::Float, ParameterKind::Bool, ParameterKind::Trigger]
    );
    assert_eq!(graph.parameters[0].default, 0.25);
    assert_eq!(graph.parameters[1].default, 1.0);
    assert_eq!(graph.nodes.len(), 6);
    let NodeDef::StateMachine(sm) = &graph.nodes[0] else {
        return Err("node 0 is the state machine".into());
    };
    assert_eq!(sm.states, [1, 2]);
    assert_eq!(sm.transitions.len(), 2);
    assert_eq!(sm.transitions[0].conditions[0].op, CompareOp::Greater);
    assert_eq!(sm.transitions[0].conditions[1].value, 1.0);
    assert_eq!(sm.transitions[1].from, None);
    assert_eq!(sm.transitions[1].exit_time, Some(0.9));
    assert_eq!(graph.nodes[1], NodeDef::Clip { clip: 0, speed: 0.0 });
    assert_eq!(graph.layers[1].mode, LayerMode::Additive);
    assert_eq!(graph.layers[1].reference_clip, Some(0));
    assert_eq!(graph.layers[1].mask, [0, 4]);
    assert_eq!((graph.foot_chains[0].root, graph.foot_chains[0].tip), (1, 3));
    let look = graph.look_at.ok_or("look-at")?;
    assert_eq!((look.head, look.axis), (4, [0.0, 0.0, 1.0]));
    // Binds with mantis_anim against the cooked skeleton and clips.
    let skeleton = Arc::new(Skeleton::new(&SkeletonAsset::parse(
        &out.get("skeletons/body.skeleton").ok_or("skeleton")?.bytes,
    )?)?);
    let clips = [walk, run_clip].map(|a| (a.hash, ClipAsset::parse(&a.bytes)));
    let bound = AnimGraph::new(skeleton, &graph, |h| {
        clips
            .iter()
            .find(|(k, _)| k == h)
            .and_then(|(_, c)| c.as_ref().ok())
            .and_then(|c| Clip::new(c).ok())
            .map(Arc::new)
    })?;
    assert_eq!(bound.layer_count(), 2);
    assert!(bound.parameter(bone_name_hash("speed")).is_some());
    Ok(())
}

#[test]
fn graph_errors_name_their_lines() -> TestResult {
    let base = body_tree();
    let path = "graphs/bad.animgraph.toml";
    let at = |line: usize| format!("{path}:{line}:");
    let head = "skeleton = \"skeletons/body.skeleton.toml\"\n[clip.walk]\npath = \"clips/walk.clip.toml\"\n[parameter.speed]\n[parameter.go]\nkind = \"trigger\"\n";
    // Lines 1 to 6 are the head.
    let clip_node = |name: &str| format!("[node.{name}]\nkind = \"clip\"\nclip = \"walk\"\n");
    let layer = |node: &str| format!("[layer.base]\nnode = \"{node}\"\n");
    // Unknown node in a layer (line 11).
    fails_at(
        &base,
        path,
        &format!("{head}{}{}", clip_node("a"), layer("b")),
        &at(11),
    )?;
    // A node used twice: the second reference.
    fails_at(
        &base,
        path,
        &format!(
            "{head}{}[node.m]\nkind = \"blend1d\"\nparameter = \"speed\"\nchildren = [\"a\", \"a\"]\nthresholds = [0, 1]\n{}",
            clip_node("a"),
            layer("m")
        ),
        &at(13),
    )?;
    // An unused node: its header.
    fails_at(
        &base,
        path,
        &format!("{head}{}{}{}", clip_node("a"), clip_node("b"), layer("a")),
        &at(10),
    )?;
    // An unknown clip.
    fails_at(
        &base,
        path,
        &format!("{head}[node.a]\nkind = \"clip\"\nclip = \"swim\"\n{}", layer("a")),
        &at(9),
    )?;
    // Conditions: unknown parameter, a trigger compared, a float triggered, bad syntax.
    let machine = |cond: &str| {
        format!(
            "{head}[node.m]\nkind = \"state_machine\"\nstates = [\"a\", \"b\"]\n{}{}[transition.m.go]\nfrom = \"a\"\nto = \"b\"\nconditions = [\"{cond}\"]\n{}",
            clip_node("a"),
            clip_node("b"),
            layer("m")
        )
    };
    // The conditions line is line 19.
    for cond in [
        "fly > 1",
        "go > 1",
        "speed triggered",
        "speed >> 1",
        "speed > fast",
    ] {
        fails_at(&base, path, &machine(cond), &at(19))?;
    }
    // A transition naming a state the machine does not have.
    fails_at(
        &base,
        path,
        &machine("speed > 1").replace("to = \"b\"", "to = \"c\""),
        &at(18),
    )?;
    // A transition for a node that is not a state machine.
    fails_at(
        &base,
        path,
        &format!(
            "{head}{}{}[transition.a.x]\nto = \"a\"\n",
            clip_node("a"),
            layer("a")
        ),
        &at(12),
    )?;
    // A blend on a trigger parameter.
    fails_at(
        &base,
        path,
        &format!(
            "{head}[node.m]\nkind = \"blend1d\"\nparameter = \"go\"\nchildren = [\"a\"]\nthresholds = [0]\n{}{}",
            clip_node("a"),
            layer("m")
        ),
        &at(9),
    )?;
    // A cycle that no layer reaches.
    fails_at(
        &base,
        path,
        &format!(
            "{head}{}{}[node.x]\nkind = \"blend1d\"\nparameter = \"speed\"\nchildren = [\"y\"]\nthresholds = [0]\n[node.y]\nkind = \"blend1d\"\nparameter = \"speed\"\nchildren = [\"x\"]\nthresholds = [0]\n",
            clip_node("a"),
            layer("a")
        ),
        &at(12),
    )?;
    // An unknown mask bone, an additive first layer.
    fails_at(
        &base,
        path,
        &format!("{head}{}{}mask = [\"tail\"]\n", clip_node("a"), layer("a")),
        &at(12),
    )?;
    fails_at(
        &base,
        path,
        &format!("{head}{}{}mode = \"additive\"\n", clip_node("a"), layer("a")),
        &at(12),
    )?;
    // A foot chain that is not an ancestor line (head is not below the knee): its header.
    fails_at(
        &base,
        path,
        &format!(
            "{head}{}{}[foot.left]\nroot = \"hip\"\nmid = \"knee\"\ntip = \"head\"\npole = [0, 0, 1]\n",
            clip_node("a"),
            layer("a")
        ),
        &at(12),
    )?;
    // A look-at with a zero axis.
    fails_at(
        &base,
        path,
        &format!(
            "{head}{}{}[look_at]\nhead = \"head\"\naxis = [0, 0, 0]\nmax_angle = 1.0\n",
            clip_node("a"),
            layer("a")
        ),
        &at(14),
    )?;
    // A clip that is not cooked: the path line.
    fails_at(
        &base,
        path,
        &format!(
            "{head}{}{}[clip.swim]\npath = \"clips/swim.clip.toml\"\n",
            clip_node("a"),
            layer("a")
        ),
        &at(13),
    )?;
    Ok(())
}

/// A two-bone arm along +Y: `root` and `upper` at the origin, `lower` one unit up.
const ARM: &str = "\
[bone.root]
[bone.upper]
parent = \"root\"
[bone.lower]
parent = \"upper\"
translation = [0.0, 1.0, 0.0]
";

/// The lower bone bends 90 degrees about +Z over one second.
const BEND: &str = "\
skeleton = \"skeletons/arm.skeleton.toml\"
duration = 1.0
[track.lower.rotation]
times = [0.0, 1.0]
values = [0, 0, 0, 1, 0, 0, 0.70710677, 0.70710677]
";

/// A thin strip from y = 0 to y = 2 in the XY plane.
const STRIP: &str = "\
v -0.1 0 0
v 0.1 0 0
v 0.1 1 0
v -0.1 1 0
v 0.1 2 0
v -0.1 2 0
f 1 2 3 4
f 4 3 5 6
";

const STRIP_SKIN: &str = "\
mesh = \"meshes/arm.skin.obj\"
[weights.1]
joints = [1]
weights = [1]
[weights.2]
joints = [1]
weights = [1]
[weights.3]
joints = [1, 2]
weights = [0.5, 0.5]
[weights.4]
joints = [1, 2]
weights = [0.5, 0.5]
[weights.5]
joints = [2]
weights = [1]
[weights.6]
joints = [2]
weights = [1]
";

fn arm_tree() -> ContentTree {
    let mut t = ContentTree::new();
    t.insert("skeletons/arm.skeleton.toml", ARM);
    t.insert("clips/bend.clip.toml", BEND);
    t.insert("meshes/arm.skin.obj", STRIP);
    t.insert("meshes/arm.skinmesh.toml", STRIP_SKIN);
    t.insert(
        "bakes/arm.vat.toml",
        "skeleton = \"skeletons/arm.skeleton.toml\"\nclip = \"clips/bend.clip.toml\"\nmesh = \"meshes/arm.skinmesh.toml\"\nfps = 4.0\n",
    );
    t
}

#[test]
fn a_vat_bake_of_a_two_bone_bend_matches_bake_vat() -> TestResult {
    let out = run(&arm_tree())?;
    let asset = out.get("bakes/arm.vat").ok_or("vat output")?;
    assert_eq!(asset.kind, AssetKind::VertexAnimation);
    assert_eq!(asset.domain, Domain::Presentation);
    let cooked = VatAsset::parse(&asset.bytes)?;
    // The same bake run directly on the cooked inputs.
    let skeleton = Skeleton::new(&SkeletonAsset::parse(
        &out.get("skeletons/arm.skeleton").ok_or("skeleton")?.bytes,
    )?)?;
    let clip = Clip::new(&ClipAsset::parse(
        &out.get("clips/bend.clip").ok_or("clip")?.bytes,
    )?)?;
    let mesh = MeshAsset::parse(&out.get("meshes/arm.skinmesh").ok_or("mesh")?.bytes)?;
    let input = skinned_input(&mesh)?;
    let direct = bake_vat(&skeleton, &clip, &input.mesh(), 4.0)?;
    let direct = vat_asset(direct);
    assert_eq!(cooked, direct);
    assert_eq!(direct.encode(), asset.bytes);
    // A clamped one-second clip at 4 fps: five frames, both ends included.
    assert_eq!((cooked.frame_count, cooked.vertex_count), (5, 6));
    assert!(!cooked.looping);
    assert!((cooked.seconds_per_frame - 0.25).abs() < 1e-6);
    // The tip vertex (0.1, 2, 0) swings about the elbow at (0, 1, 0) to (-1, 1.1, 0).
    let tip = mesh
        .vertices
        .iter()
        .position(|v| v.position == [0.1, 2.0, 0.0])
        .ok_or("tip vertex")?;
    let first = cooked.positions[tip];
    let last = cooked.positions[4 * 6 + tip];
    assert!((first[0] - 0.1).abs() < 1e-5 && (first[1] - 2.0).abs() < 1e-5);
    assert!(
        (last[0] + 1.0).abs() < 1e-4 && (last[1] - 1.1).abs() < 1e-4,
        "{last:?}"
    );
    assert_eq!(last[3], 1.0);
    Ok(())
}

#[test]
fn vat_errors_name_their_lines() -> TestResult {
    let mut base = arm_tree();
    base.insert("meshes/static.obj", STRIP);
    base.insert("skeletons/one.skeleton.toml", "[bone.only]\n");
    let path = "bakes/arm.vat.toml";
    let at = |line: usize| format!("{path}:{line}:");
    let good = "skeleton = \"skeletons/arm.skeleton.toml\"\nclip = \"clips/bend.clip.toml\"\nmesh = \"meshes/arm.skinmesh.toml\"\nfps = 4.0\n";
    fails_at(
        &base,
        path,
        &good.replace("arm.skinmesh.toml", "static.obj"),
        &at(3),
    )?;
    fails_at(&base, path, &good.replace("4.0", "0.0"), &at(4))?;
    fails_at(
        &base,
        path,
        &good.replace("skeletons/arm", "skeletons/one"),
        &at(2),
    )?;
    fails_at(&base, path, &good.replace("clips/bend", "clips/none"), &at(2))?;
    fails_at(&base, path, &format!("{good}speed = 2\n"), &at(5))?;
    Ok(())
}

fn sample_vat() -> VatAsset {
    VatAsset {
        vertex_count: 2,
        frame_count: 2,
        seconds_per_frame: 0.5,
        looping: true,
        bounds_min: [0.0, 0.0, 0.0],
        bounds_max: [1.0, 2.0, 0.0],
        positions: vec![
            [0.0, 0.0, 0.0, 1.0],
            [1.0, 0.0, 0.0, 1.0],
            [0.0, 1.0, 0.0, 1.0],
            [1.0, 2.0, 0.0, 1.0],
        ],
        normals: vec![[0.0, 0.0, -1.0, 0.0]; 4],
    }
}

#[test]
fn the_vat_payload_round_trips_and_rejects_corruption() -> TestResult {
    let vat = sample_vat();
    let bytes = vat.encode();
    assert_eq!(&bytes[..4], b"MVAT");
    assert_eq!(bytes.len(), 48 + 32 * 4);
    let back = VatAsset::parse(&bytes)?;
    assert_eq!(back, vat);
    assert_eq!(back.encode(), bytes);
    let corrupt = |at: usize, patch: &[u8]| {
        let mut b = bytes.clone();
        b[at..at + patch.len()].copy_from_slice(patch);
        VatAsset::parse(&b)
    };
    assert_eq!(corrupt(0, b"XVAT").err(), Some(FormatError::Magic));
    assert_eq!(
        corrupt(4, &2u16.to_le_bytes()).err(),
        Some(FormatError::Version(2))
    );
    assert_eq!(corrupt(6, &2u16.to_le_bytes()).err(), Some(FormatError::Flags(2)));
    assert_eq!(
        corrupt(8, &0u32.to_le_bytes()).err(),
        Some(FormatError::Dimensions)
    );
    assert_eq!(
        corrupt(12, &u32::MAX.to_le_bytes()).err(),
        Some(FormatError::Dimensions)
    );
    assert!(matches!(
        corrupt(12, &3u32.to_le_bytes()),
        Err(FormatError::Length { .. })
    ));
    assert_eq!(
        corrupt(16, &0.0f32.to_le_bytes()).err(),
        Some(FormatError::Keyframes)
    );
    assert_eq!(
        corrupt(44, &1u32.to_le_bytes()).err(),
        Some(FormatError::Reserved)
    );
    assert_eq!(
        corrupt(48 + 12, &0.5f32.to_le_bytes()).err(),
        Some(FormatError::Geometry),
        "w"
    );
    assert_eq!(
        corrupt(48 + 16, &5.0f32.to_le_bytes()).err(),
        Some(FormatError::Geometry),
        "outside"
    );
    assert_eq!(
        corrupt(48, &f32::NAN.to_le_bytes()).err(),
        Some(FormatError::NonFinite)
    );
    assert_eq!(
        corrupt(48 + 64, &0.5f32.to_le_bytes()).err(),
        Some(FormatError::Geometry),
        "normal"
    );
    assert!(VatAsset::parse(&bytes[..bytes.len() - 1]).is_err());
    for len in 0..bytes.len() {
        assert!(VatAsset::parse(&bytes[..len]).is_err());
    }
    Ok(())
}

#[test]
fn animation_cooking_is_deterministic() -> TestResult {
    let tree = || {
        let mut t = body_tree();
        t.insert("graphs/body.animgraph.toml", GRAPH);
        for (path, bytes) in arm_tree()
            .sources()
            .map(|s| (s.path.to_owned(), s.bytes.to_vec()))
        {
            t.insert(&path, bytes);
        }
        t
    };
    let a = run(&tree())?;
    let b = run(&tree())?;
    assert_eq!(a.assets.len(), 8);
    for (name, asset) in &a.assets {
        assert_eq!(Some(&asset.bytes), b.get(name).map(|x| &x.bytes), "{name}");
    }
    Ok(())
}

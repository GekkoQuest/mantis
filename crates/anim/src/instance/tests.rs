//! Graph and instance tests: blend trees, state machines (triggers, crossfades, exit
//! times, from-any transitions), layers, root motion, IK, and binding errors.

use super::*;
use crate::clip::Clip;
use crate::clip::tests::track;
use crate::skeleton::Skeleton;
use crate::skeleton::tests::{TestResult, asset, leg};
use glam::Quat;
use mantis_core::content::ContentHash;
use mantis_formats::anim_clip::{Channel, ClipAsset};
use mantis_formats::anim_graph::{
    BlendChildDef, CompareOp, ConditionDef, GraphAsset, LayerDef, LookAtDef, NodeDef, ParameterDef,
    StateMachineDef, TransitionDef, TwoBoneChainDef,
};
use mantis_formats::skeleton::bone_name_hash;

fn h(b: u8) -> ContentHash {
    ContentHash::from_bytes([b; 32])
}

fn clip(duration: f32, looping: bool, tracks: Vec<mantis_formats::anim_clip::TrackDef>) -> ClipAsset {
    ClipAsset {
        duration,
        sample_rate: 30.0,
        looping,
        root_motion: false,
        bone_count: 4,
        tracks,
    }
}

/// A clip that holds the ankle at local x = `x` (a marker that reveals blend weights).
fn marker(x: f32, duration: f32, looping: bool) -> ClipAsset {
    clip(
        duration,
        looping,
        vec![track(3, Channel::Translation, &[0.0], &[x, -1.0, 0.0])],
    )
}

fn hip_yaw(yaw: f32) -> ClipAsset {
    let q = Quat::from_rotation_y(yaw);
    clip(
        1.0,
        true,
        vec![track(1, Channel::Rotation, &[0.0], &[q.x, q.y, q.z, q.w])],
    )
}

fn walk() -> ClipAsset {
    let mut c = clip(
        1.0,
        true,
        vec![track(
            0,
            Channel::Translation,
            &[0.0, 1.0],
            &[0.0, 0.0, 0.0, 0.0, 0.0, 2.0],
        )],
    );
    c.root_motion = true;
    c
}

fn library() -> Vec<(ContentHash, ClipAsset)> {
    vec![
        (h(1), marker(0.0, 1.0, true)),
        (h(2), marker(2.0, 1.0, true)),
        (h(3), marker(10.0, 0.5, false)),
        (h(4), hip_yaw(0.3)),
        (h(5), hip_yaw(0.0)),
        (h(6), walk()),
    ]
}

fn cond(parameter: u32, op: CompareOp, value: f32) -> ConditionDef {
    ConditionDef { parameter, op, value }
}

fn layer(node: u32) -> LayerDef {
    LayerDef {
        node,
        weight: 1.0,
        mode: LayerMode::Override,
        reference_clip: None,
        mask: vec![],
    }
}

/// Parameters: 0 speed (float), 1 grounded (bool, true), 2 jump (trigger).
/// Node 0: machine { 0: blend(speed: 0 -> marker 0, 2 -> marker 2), 1: jump (marker 10,
/// 0.5 s, clamped) }; 0 -> 1 on jump (0.2 s fade); 1 -> 0 at exit time 1 when grounded;
/// any -> 0 when speed < -10 (0.5 s fade). Layer 1 adds a hip yaw of 0.3 at half weight
/// on the hip only. One foot chain (hip, knee, ankle) and a look-at on the knee.
fn locomotion() -> GraphAsset {
    GraphAsset {
        bone_count: 4,
        clips: vec![h(1), h(2), h(3), h(4), h(5)],
        parameters: vec![
            ParameterDef {
                name_hash: bone_name_hash("speed"),
                kind: ParameterKind::Float,
                default: 0.0,
            },
            ParameterDef {
                name_hash: bone_name_hash("grounded"),
                kind: ParameterKind::Bool,
                default: 1.0,
            },
            ParameterDef {
                name_hash: bone_name_hash("jump"),
                kind: ParameterKind::Trigger,
                default: 0.0,
            },
        ],
        nodes: vec![
            NodeDef::StateMachine(StateMachineDef {
                states: vec![1, 4],
                entry: 0,
                transitions: vec![
                    TransitionDef {
                        from: Some(0),
                        to: 1,
                        crossfade: 0.2,
                        exit_time: None,
                        conditions: vec![cond(2, CompareOp::Triggered, 0.0)],
                    },
                    TransitionDef {
                        from: Some(1),
                        to: 0,
                        crossfade: 0.0,
                        exit_time: Some(1.0),
                        conditions: vec![cond(1, CompareOp::Equal, 1.0)],
                    },
                    TransitionDef {
                        from: None,
                        to: 0,
                        crossfade: 0.5,
                        exit_time: None,
                        conditions: vec![cond(0, CompareOp::Less, -10.0)],
                    },
                ],
            }),
            NodeDef::Blend1D {
                parameter: 0,
                children: vec![
                    BlendChildDef {
                        node: 2,
                        threshold: 0.0,
                    },
                    BlendChildDef {
                        node: 3,
                        threshold: 2.0,
                    },
                ],
            },
            NodeDef::Clip { clip: 0, speed: 1.0 },
            NodeDef::Clip { clip: 1, speed: 1.0 },
            NodeDef::Clip { clip: 2, speed: 1.0 },
            NodeDef::Clip { clip: 3, speed: 1.0 },
        ],
        layers: vec![
            layer(0),
            LayerDef {
                node: 5,
                weight: 0.5,
                mode: LayerMode::Additive,
                reference_clip: Some(4),
                mask: vec![1],
            },
        ],
        foot_chains: vec![TwoBoneChainDef {
            root: 1,
            mid: 2,
            tip: 3,
            pole: [0.0, 0.0, 1.0],
        }],
        look_at: Some(LookAtDef {
            head: 2,
            axis: [0.0, 0.0, 1.0],
            max_angle: 1.0,
        }),
    }
}

fn bind_graph(skeleton: Skeleton, graph: &GraphAsset) -> Result<Arc<AnimGraph>, AnimError> {
    let clips: Vec<(ContentHash, Arc<Clip>)> = library()
        .into_iter()
        .map(|(hash, a)| Clip::new(&a).map(|c| (hash, Arc::new(c))))
        .collect::<Result<_, _>>()?;
    let graph = AnimGraph::new(Arc::new(skeleton), graph, |hash| {
        clips.iter().find(|(k, _)| k == hash).map(|(_, c)| Arc::clone(c))
    })?;
    Ok(Arc::new(graph))
}

fn instance() -> Result<AnimInstance, Box<dyn std::error::Error>> {
    Ok(AnimInstance::new(bind_graph(leg()?, &locomotion())?))
}

fn param(inst: &AnimInstance, name: &str) -> Result<ParamId, Box<dyn std::error::Error>> {
    Ok(inst.graph().parameter(bone_name_hash(name)).ok_or("parameter")?)
}

fn marker_x(inst: &AnimInstance) -> f32 {
    inst.pose().get(3).map_or(f32::NAN, |t| t.translation.x)
}

fn hip_yaw_of(inst: &AnimInstance) -> f32 {
    crate::root_motion::yaw_of(inst.pose().get(1).map_or(Quat::NAN, |t| t.rotation))
}

#[test]
fn starts_in_bind_pose_with_identity_palette() -> TestResult {
    let inst = instance()?;
    assert_eq!(inst.pose(), inst.graph().skeleton().bind_pose());
    assert_eq!(inst.palette_bytes().len(), 4 * 48);
    for entry in inst.palette() {
        assert!((entry[0][0] - 1.0).abs() < 1e-6 && entry[0][3].abs() < 1e-6);
    }
    assert_eq!(inst.current_state(0), Some(0));
    assert_eq!(inst.current_state(1), None);
    assert_eq!(inst.graph().node_count(), 6);
    assert_eq!(inst.graph().layer_count(), 2);
    assert_eq!(inst.graph().depth(), 2);
    Ok(())
}

#[test]
fn blend_follows_the_parameter() -> TestResult {
    let mut inst = instance()?;
    let speed = param(&inst, "speed")?;
    for (value, expected) in [(1.0, 1.0), (0.5, 0.5), (5.0, 2.0), (-3.0, 0.0)] {
        inst.set_float(speed, value)?;
        inst.update(0.1);
        assert!(
            (marker_x(&inst) - expected).abs() < 1e-5,
            "speed {value}: {}",
            marker_x(&inst)
        );
    }
    Ok(())
}

#[test]
fn trigger_crossfades_once_then_exit_time_returns() -> TestResult {
    let mut inst = instance()?;
    let jump = param(&inst, "jump")?;
    let grounded = param(&inst, "grounded")?;
    inst.set_trigger(jump)?;
    inst.update(0.1);
    assert_eq!((inst.current_state(0), inst.next_state(0)), (Some(0), Some(1)));
    assert_eq!(inst.parameter_value(jump), Some(0.0), "consumed");
    assert!(
        (marker_x(&inst) - 5.0).abs() < 1e-4,
        "halfway through the fade: {}",
        marker_x(&inst)
    );
    inst.update(0.15);
    assert_eq!((inst.current_state(0), inst.next_state(0)), (Some(1), None));
    assert!((marker_x(&inst) - 10.0).abs() < 1e-5);
    assert_eq!(inst.clip_time(4), Some(0.25));
    // The jump clip ends at 0.5 s; not grounded, so the machine waits at the last frame.
    inst.set_bool(grounded, false)?;
    inst.update(0.3);
    inst.update(0.1);
    assert_eq!(inst.current_state(0), Some(1));
    assert_eq!(inst.clip_time(4), Some(0.5));
    inst.set_bool(grounded, true)?;
    inst.update(0.01);
    assert_eq!((inst.current_state(0), inst.next_state(0)), (Some(0), None));
    assert!(marker_x(&inst).abs() < 1e-5);
    Ok(())
}

#[test]
fn exit_time_is_not_reached_early() -> TestResult {
    let mut inst = instance()?;
    inst.set_trigger(param(&inst, "jump")?)?;
    inst.update(0.2);
    assert_eq!(inst.current_state(0), Some(1));
    inst.update(0.1);
    assert_eq!(
        inst.current_state(0),
        Some(1),
        "normalized time 0.6 < exit time 1"
    );
    Ok(())
}

#[test]
fn from_any_transition_fades_but_never_into_the_current_state() -> TestResult {
    let mut inst = instance()?;
    let speed = param(&inst, "speed")?;
    inst.set_float(speed, -20.0)?;
    inst.update(0.1);
    assert_eq!((inst.current_state(0), inst.next_state(0)), (Some(0), None));
    inst.set_float(speed, 0.0)?;
    inst.set_trigger(param(&inst, "jump")?)?;
    inst.update(0.3);
    assert_eq!(inst.current_state(0), Some(1));
    inst.set_float(speed, -20.0)?;
    inst.update(0.1);
    assert_eq!((inst.current_state(0), inst.next_state(0)), (Some(1), Some(0)));
    Ok(())
}

#[test]
fn additive_layer_respects_weight_and_mask() -> TestResult {
    let mut inst = instance()?;
    inst.update(0.1);
    assert!((hip_yaw_of(&inst) - 0.15).abs() < 1e-4, "{}", hip_yaw_of(&inst));
    inst.set_layer_weight(1, 1.0)?;
    inst.update(0.1);
    assert!((hip_yaw_of(&inst) - 0.3).abs() < 1e-4);
    let knee = inst.pose().get(2).map(|t| t.rotation).ok_or("knee")?;
    assert!(knee.angle_between(Quat::IDENTITY) < 1e-6, "masked out");
    inst.set_layer_weight(1, 0.0)?;
    inst.update(0.1);
    assert!(hip_yaw_of(&inst).abs() < 1e-6);
    inst.set_layer_weight(0, 0.0)?;
    inst.set_float(param(&inst, "speed")?, 2.0)?;
    inst.update(0.1);
    assert_eq!(
        inst.pose(),
        inst.graph().skeleton().bind_pose(),
        "no layer contributes"
    );
    Ok(())
}

#[test]
fn root_motion_is_extracted_and_wraps() -> TestResult {
    let mut graph = locomotion();
    graph.clips = vec![h(6)];
    graph.parameters.clear();
    graph.nodes = vec![NodeDef::Clip { clip: 0, speed: 1.0 }];
    graph.layers = vec![layer(0)];
    let mut inst = AnimInstance::new(bind_graph(leg()?, &graph)?);
    let rm = inst.update(0.25);
    assert!(
        rm.translation.abs_diff_eq(glam::Vec3::new(0.0, 0.0, 0.5), 1e-5),
        "{rm:?}"
    );
    assert_eq!(inst.root_motion(), rm);
    let root = inst.pose().first().ok_or("root")?;
    assert_eq!(root.translation, glam::Vec3::ZERO, "removed from the pose");
    inst.update(0.65);
    let across = inst.update(0.2);
    assert!(
        across
            .translation
            .abs_diff_eq(glam::Vec3::new(0.0, 0.0, 0.4), 1e-4),
        "{across:?}"
    );
    assert_eq!(inst.update(0.0), crate::root_motion::RootMotion::IDENTITY);
    Ok(())
}

#[test]
fn foot_ik_reaches_and_palette_follows() -> TestResult {
    let mut inst = instance()?;
    let target = glam::Vec3::new(0.3, 0.8, 0.4);
    inst.set_foot_target(
        0,
        Some(IkTarget {
            position: target,
            weight: 1.0,
        }),
    )?;
    inst.update(0.1);
    let ankle = inst.model().get(3).ok_or("ankle")?.translation;
    assert!(ankle.distance(target) < 1e-4, "{ankle:?}");
    let entry = inst.palette().get(3).ok_or("palette")?;
    let column = glam::Vec3::new(entry[0][3], entry[1][3], entry[2][3]);
    assert!(
        column.distance(ankle) < 1e-4,
        "the bind ankle skins to the solved ankle"
    );
    inst.set_foot_target(0, None)?;
    inst.update(0.1);
    let ankle = inst.model().get(3).ok_or("ankle")?.translation;
    assert!(ankle.distance(glam::Vec3::ZERO) < 1e-5, "{ankle:?}");
    Ok(())
}

#[test]
fn look_at_turns_the_head() -> TestResult {
    let mut inst = instance()?;
    inst.set_layer_weight(1, 0.0)?;
    let target = glam::Vec3::new(0.5, 1.0, 2.0);
    inst.set_look_at_target(Some(IkTarget {
        position: target,
        weight: 1.0,
    }))?;
    inst.update(0.1);
    let knee = inst.model().get(2).ok_or("knee")?;
    let forward = knee.rotation * glam::Vec3::Z;
    assert!(
        forward.distance((target - knee.translation).normalize()) < 1e-4,
        "{forward:?}"
    );
    Ok(())
}

#[test]
fn requests_are_checked() -> TestResult {
    let mut inst = instance()?;
    let speed = param(&inst, "speed")?;
    let grounded = param(&inst, "grounded")?;
    let jump = param(&inst, "jump")?;
    assert_eq!(inst.set_bool(speed, true), Err(AnimError::ParameterKind));
    assert_eq!(inst.set_float(grounded, 1.0), Err(AnimError::ParameterKind));
    assert_eq!(inst.set_trigger(speed), Err(AnimError::ParameterKind));
    assert_eq!(inst.set_float(speed, f32::NAN), Err(AnimError::NonFinite));
    assert_eq!(inst.set_float(ParamId(9), 1.0), Err(AnimError::UnknownParameter));
    assert_eq!(inst.graph().parameter(bone_name_hash("missing")), None);
    assert_eq!(inst.graph().parameter_kind(jump), Some(ParameterKind::Trigger));
    assert_eq!(jump.index(), 2);
    inst.set_trigger(jump)?;
    inst.reset_trigger(jump)?;
    assert_eq!(inst.parameter_value(jump), Some(0.0));
    assert_eq!(inst.set_layer_weight(2, 1.0), Err(AnimError::OutOfRange));
    assert_eq!(inst.set_layer_weight(0, f32::INFINITY), Err(AnimError::NonFinite));
    assert_eq!(inst.set_foot_target(1, None), Err(AnimError::OutOfRange));
    let bad = Some(IkTarget {
        position: glam::Vec3::NAN,
        weight: 1.0,
    });
    assert_eq!(inst.set_foot_target(0, bad), Err(AnimError::NonFinite));
    let mut no_look = locomotion();
    no_look.look_at = None;
    let mut plain = AnimInstance::new(bind_graph(leg()?, &no_look)?);
    assert_eq!(plain.set_look_at_target(None), Err(AnimError::NoLookAt));
    // Non-finite and negative steps count as zero.
    plain.update(f32::NAN);
    plain.update(-1.0);
    assert_eq!(plain.clip_time(2), Some(0.0));
    Ok(())
}

#[test]
fn binding_errors() -> TestResult {
    let mut missing = locomotion();
    missing.clips.push(h(9));
    assert_eq!(
        bind_graph(leg()?, &missing).err(),
        Some(AnimError::MissingClip(h(9)))
    );
    let mut wide = locomotion();
    wide.bone_count = 5;
    assert_eq!(
        bind_graph(leg()?, &wide).err(),
        Some(AnimError::BoneCountMismatch {
            expected: 4,
            actual: 5
        })
    );
    // Bones 1 and 2 are siblings: the chain is not an ancestor line.
    let forked = Skeleton::new(&asset(&[
        (None, "root", [0.0; 3], Quat::IDENTITY),
        (Some(0), "a", [0.0, 1.0, 0.0], Quat::IDENTITY),
        (Some(0), "b", [0.0, 2.0, 0.0], Quat::IDENTITY),
        (Some(2), "c", [0.0, 1.0, 0.0], Quat::IDENTITY),
    ]))?;
    assert_eq!(
        bind_graph(forked, &locomotion()).err(),
        Some(AnimError::IkChain(0))
    );
    let mut broken = locomotion();
    broken.layers.clear();
    assert!(matches!(bind_graph(leg()?, &broken), Err(AnimError::Format(_))));
    Ok(())
}

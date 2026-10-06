//! Runtime integration: a full graph built from encoded assets updates without
//! allocating, and parallel batch evaluation matches serial evaluation bit for bit.

#![allow(clippy::cast_precision_loss)] // Test data generation from small loop indices.

use std::sync::Arc;

use glam::{Quat, Vec3};
use mantis_anim::skeleton::inverse_bind_matrices;
use mantis_anim::{AnimGraph, AnimInstance, Clip, IkTarget, Skeleton, evaluate_batch};
use mantis_core::content::ContentHash;
use mantis_formats::anim_clip::{Channel, ClipAsset, Interpolation, TrackDef};
use mantis_formats::anim_graph::{
    BlendChildDef, CompareOp, ConditionDef, GraphAsset, LayerDef, LayerMode, LookAtDef, NodeDef,
    ParameterDef, ParameterKind, StateMachineDef, TransitionDef, TwoBoneChainDef,
};
use mantis_formats::skeleton::{BoneDef, SkeletonAsset, bone_name_hash};

#[global_allocator]
static ALLOC: mantis_testkit::alloc::CountingAllocator = mantis_testkit::alloc::CountingAllocator;

type TestResult = Result<(), Box<dyn std::error::Error>>;

/// Root, pelvis, two legs (hip, knee, ankle), spine, head.
fn skeleton_asset() -> SkeletonAsset {
    let bones: [(Option<u16>, &str, [f32; 3]); 10] = [
        (None, "root", [0.0, 0.0, 0.0]),
        (Some(0), "pelvis", [0.0, 1.0, 0.0]),
        (Some(1), "hip.l", [0.2, 0.0, 0.0]),
        (Some(2), "knee.l", [0.0, -0.5, 0.05]),
        (Some(3), "ankle.l", [0.0, -0.45, -0.05]),
        (Some(1), "hip.r", [-0.2, 0.0, 0.0]),
        (Some(5), "knee.r", [0.0, -0.5, 0.05]),
        (Some(6), "ankle.r", [0.0, -0.45, -0.05]),
        (Some(1), "spine", [0.0, 0.3, 0.0]),
        (Some(8), "head", [0.0, 0.4, 0.0]),
    ];
    let mut defs: Vec<BoneDef> = bones
        .iter()
        .map(|(parent, name, t)| BoneDef {
            parent: *parent,
            name_hash: bone_name_hash(name),
            translation: *t,
            rotation: [0.0, 0.0, 0.0, 1.0],
            scale: [1.0; 3],
            inverse_bind: [0.0; 16],
        })
        .collect();
    let inverse = inverse_bind_matrices(&defs);
    for (d, m) in defs.iter_mut().zip(inverse) {
        d.inverse_bind = m;
    }
    SkeletonAsset { bones: defs }
}

fn rotation_track(bone: u16, axis: Vec3, angles: &[f32], duration: f32) -> TrackDef {
    let n = angles.len().max(2) - 1;
    let times = (0..angles.len())
        .map(|i| duration * i as f32 / n as f32)
        .collect();
    let values = angles
        .iter()
        .flat_map(|a| Quat::from_axis_angle(axis, *a).to_array())
        .collect();
    TrackDef {
        bone,
        channel: Channel::Rotation,
        interpolation: Interpolation::Linear,
        times,
        values,
    }
}

/// A gait: legs swing in opposition, the root travels `stride` per cycle with root motion.
fn gait(duration: f32, swing: f32, stride: f32) -> ClipAsset {
    ClipAsset {
        duration,
        sample_rate: 30.0,
        looping: true,
        root_motion: true,
        bone_count: 10,
        tracks: vec![
            TrackDef {
                bone: 0,
                channel: Channel::Translation,
                interpolation: Interpolation::Linear,
                times: vec![0.0, duration],
                values: vec![0.0, 0.0, 0.0, 0.0, 0.0, stride],
            },
            rotation_track(0, Vec3::Y, &[0.0, 0.1, 0.0], duration),
            rotation_track(2, Vec3::X, &[swing, -swing, swing], duration),
            rotation_track(3, Vec3::X, &[0.0, swing, 0.0, swing, 0.0], duration),
            rotation_track(5, Vec3::X, &[-swing, swing, -swing], duration),
            rotation_track(6, Vec3::X, &[swing, 0.0, swing, 0.0, swing], duration),
        ],
    }
}

fn pose_clip(bone: u16, axis: Vec3, angle: f32, looping: bool, duration: f32) -> ClipAsset {
    ClipAsset {
        duration,
        sample_rate: 30.0,
        looping,
        root_motion: false,
        bone_count: 10,
        tracks: vec![rotation_track(bone, axis, &[0.0, angle], duration)],
    }
}

fn clips() -> Vec<ClipAsset> {
    vec![
        gait(1.0, 0.4, 1.4),
        gait(0.7, 0.8, 2.8),
        pose_clip(1, Vec3::X, -0.6, false, 0.6),
        pose_clip(8, Vec3::Z, 0.3, true, 2.0),
        pose_clip(8, Vec3::Z, 0.0, true, 2.0),
    ]
}

fn cond(parameter: u32, op: CompareOp, value: f32) -> ConditionDef {
    ConditionDef { parameter, op, value }
}

fn graph_asset(hashes: Vec<ContentHash>) -> GraphAsset {
    let parameter = |name: &str, kind, default| ParameterDef {
        name_hash: bone_name_hash(name),
        kind,
        default,
    };
    let transition = |from, to, crossfade, exit_time, conditions| TransitionDef {
        from,
        to,
        crossfade,
        exit_time,
        conditions,
    };
    GraphAsset {
        bone_count: 10,
        clips: hashes,
        parameters: vec![
            parameter("speed", ParameterKind::Float, 0.0),
            parameter("grounded", ParameterKind::Bool, 1.0),
            parameter("jump", ParameterKind::Trigger, 0.0),
        ],
        nodes: vec![
            NodeDef::StateMachine(StateMachineDef {
                states: vec![1, 4],
                entry: 0,
                transitions: vec![
                    transition(Some(0), 1, 0.15, None, vec![cond(2, CompareOp::Triggered, 0.0)]),
                    transition(Some(1), 0, 0.2, Some(1.0), vec![cond(1, CompareOp::Equal, 1.0)]),
                ],
            }),
            NodeDef::Blend1D {
                parameter: 0,
                children: vec![
                    BlendChildDef {
                        node: 2,
                        threshold: 1.0,
                    },
                    BlendChildDef {
                        node: 3,
                        threshold: 3.0,
                    },
                ],
            },
            NodeDef::Clip { clip: 0, speed: 1.0 },
            NodeDef::Clip { clip: 1, speed: 1.0 },
            NodeDef::Clip { clip: 2, speed: 1.0 },
            NodeDef::Clip { clip: 3, speed: 1.0 },
        ],
        layers: vec![
            LayerDef {
                node: 0,
                weight: 1.0,
                mode: LayerMode::Override,
                reference_clip: None,
                mask: vec![],
            },
            LayerDef {
                node: 5,
                weight: 0.7,
                mode: LayerMode::Additive,
                reference_clip: Some(4),
                mask: vec![8, 9],
            },
        ],
        foot_chains: vec![
            TwoBoneChainDef {
                root: 2,
                mid: 3,
                tip: 4,
                pole: [0.0, 0.0, 1.0],
            },
            TwoBoneChainDef {
                root: 5,
                mid: 6,
                tip: 7,
                pole: [0.0, 0.0, 1.0],
            },
        ],
        look_at: Some(LookAtDef {
            head: 9,
            axis: [0.0, 0.0, 1.0],
            max_angle: 1.2,
        }),
    }
}

/// Builds the graph from encoded bytes, as a host loading cooked assets would.
fn build() -> Result<Arc<AnimGraph>, Box<dyn std::error::Error>> {
    let skeleton = Skeleton::new(&SkeletonAsset::parse(&skeleton_asset().encode())?)?;
    let mut library = Vec::new();
    for c in clips() {
        let bytes = c.encode();
        library.push((
            ContentHash::of(&bytes),
            Arc::new(Clip::new(&ClipAsset::parse(&bytes)?)?),
        ));
    }
    let hashes = library.iter().map(|(h, _)| *h).collect();
    let graph = GraphAsset::parse(&graph_asset(hashes).encode())?;
    let bound = AnimGraph::new(Arc::new(skeleton), &graph, |hash| {
        library
            .iter()
            .find(|(h, _)| h == hash)
            .map(|(_, c)| Arc::clone(c))
    })?;
    Ok(Arc::new(bound))
}

/// Per-instance inputs for frame `frame` (varied so instances diverge).
fn drive(inst: &mut AnimInstance, index: usize, frame: usize) -> TestResult {
    let graph = Arc::clone(inst.graph());
    let speed = graph.parameter(bone_name_hash("speed")).ok_or("speed")?;
    let jump = graph.parameter(bone_name_hash("jump")).ok_or("jump")?;
    let grounded = graph.parameter(bone_name_hash("grounded")).ok_or("grounded")?;
    inst.set_float(speed, (index % 7) as f32 * 0.5 + (frame % 5) as f32 * 0.1)?;
    if (frame + index).is_multiple_of(23) {
        inst.set_trigger(jump)?;
    }
    inst.set_bool(grounded, !(frame + index).is_multiple_of(9))?;
    let phase = (frame * 3 + index) as f32 * 0.05;
    inst.set_foot_target(
        0,
        Some(IkTarget {
            position: Vec3::new(0.2, 0.15 + 0.1 * phase.sin(), 0.1),
            weight: 1.0,
        }),
    )?;
    inst.set_foot_target(
        1,
        index.is_multiple_of(2).then_some(IkTarget {
            position: Vec3::new(-0.2, 0.1, -0.1 * phase.cos()),
            weight: 0.6,
        }),
    )?;
    inst.set_look_at_target(Some(IkTarget {
        position: Vec3::new(phase.cos() * 3.0, 1.7, phase.sin() * 3.0),
        weight: 1.0,
    }))?;
    Ok(())
}

fn dt(frame: usize) -> f32 {
    1.0 / 60.0 + (frame % 4) as f32 * 0.004
}

#[test]
fn update_allocates_nothing() -> TestResult {
    let graph = build()?;
    let mut inst = AnimInstance::new(graph);
    drive(&mut inst, 0, 0)?;
    inst.update(dt(0));
    let mut travelled = Vec3::ZERO;
    let mut jumps = 0;
    for frame in 1..240 {
        drive(&mut inst, 0, frame)?;
        let rm = mantis_testkit::alloc::assert_no_alloc("AnimInstance::update", || inst.update(dt(frame)));
        travelled += rm.translation;
        jumps += usize::from(inst.current_state(0) == Some(1));
    }
    assert!(travelled.z > 1.0, "root motion accumulates: {travelled:?}");
    assert!(jumps > 0, "the jump state was exercised");
    assert!(inst.palette().iter().flatten().flatten().all(|v| v.is_finite()));
    Ok(())
}

#[test]
fn parallel_matches_serial() -> TestResult {
    let graph = build()?;
    let count = 37;
    let fresh = || {
        (0..count)
            .map(|_| AnimInstance::new(Arc::clone(&graph)))
            .collect::<Vec<_>>()
    };
    let mut serial = fresh();
    for frame in 0..90 {
        for (i, inst) in serial.iter_mut().enumerate() {
            drive(inst, i, frame)?;
            inst.update(dt(frame));
        }
    }
    for workers in [1, 2, 3, 4, 8, 64] {
        let mut parallel = fresh();
        for frame in 0..90 {
            for (i, inst) in parallel.iter_mut().enumerate() {
                drive(inst, i, frame)?;
            }
            evaluate_batch(&mut parallel, dt(frame), workers);
        }
        for (a, b) in serial.iter().zip(&parallel) {
            assert_eq!(a.pose(), b.pose(), "workers {workers}");
            assert_eq!(a.palette(), b.palette(), "workers {workers}");
            assert_eq!(a.root_motion(), b.root_motion(), "workers {workers}");
            assert_eq!(a.current_state(0), b.current_state(0));
        }
    }
    // Instances really diverged, so the comparison is meaningful.
    let first = serial.first().ok_or("instance")?;
    assert!(serial.iter().any(|s| s.pose() != first.pose()));
    let mut empty: Vec<AnimInstance> = Vec::new();
    evaluate_batch(&mut empty, 0.1, 4);
    Ok(())
}

//! Animation graph tests: round trip, every rejection rule, and the corruption sweep.

use super::*;
use crate::skeleton::bone_name_hash;

type TestResult = Result<(), Box<dyn std::error::Error>>;

fn h(b: u8) -> ContentHash {
    ContentHash::from_bytes([b; 32])
}

fn cond(parameter: u32, op: CompareOp, value: f32) -> ConditionDef {
    ConditionDef { parameter, op, value }
}

/// A locomotion state machine (blend of two clips, a jump clip) under an additive layer,
/// with one foot chain and a look-at chain.
fn full() -> GraphAsset {
    GraphAsset {
        bone_count: 8,
        clips: vec![h(1), h(2), h(3), h(4)],
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
                        crossfade: 0.1,
                        exit_time: Some(0.9),
                        conditions: vec![cond(1, CompareOp::Equal, 1.0)],
                    },
                    TransitionDef {
                        from: None,
                        to: 0,
                        crossfade: 0.0,
                        exit_time: None,
                        conditions: vec![cond(0, CompareOp::Less, -1.0)],
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
            LayerDef {
                node: 0,
                weight: 1.0,
                mode: LayerMode::Override,
                reference_clip: None,
                mask: vec![],
            },
            LayerDef {
                node: 5,
                weight: 0.5,
                mode: LayerMode::Additive,
                reference_clip: Some(3),
                mask: vec![4, 5, 6],
            },
        ],
        foot_chains: vec![TwoBoneChainDef {
            root: 1,
            mid: 2,
            tip: 3,
            pole: [0.0, 0.0, 1.0],
        }],
        look_at: Some(LookAtDef {
            head: 7,
            axis: [0.0, 0.0, 1.0],
            max_angle: 1.0,
        }),
    }
}

// Byte offsets in `full().encode()`.
const PARAMS: usize = 36 + 4 * 32;
const NODES: usize = PARAMS + 3 * 12;
const SM_STATES: usize = NODES + 16;
const SM_T0: usize = SM_STATES + 8;
const SM_T0_COND: usize = SM_T0 + 20;
const SM_T1: usize = SM_T0 + 32;
const BLEND: usize = NODES + 24 + 3 * 32;
const CLIP2: usize = BLEND + 12 + 2 * 8;
const LAYERS: usize = CLIP2 + 4 * 12;
const LAYER1: usize = LAYERS + 16;
const CHAINS: usize = LAYER1 + 16 + 3 * 2;
const LOOK: usize = CHAINS + 20;
const TOTAL: usize = LOOK + 20;

fn corrupt(at: usize, bytes: &[u8]) -> Result<GraphAsset, FormatError> {
    let mut b = full().encode();
    if let Some(s) = b.get_mut(at..at + bytes.len()) {
        s.copy_from_slice(bytes);
    }
    GraphAsset::parse(&b)
}

/// Both `validate` and the parse of the encoding reject `g` with `err`.
fn rejects(g: &GraphAsset, err: FormatError) {
    assert_eq!(g.validate(), Err(err));
    assert_eq!(GraphAsset::parse(&g.encode()).err(), Some(err));
}

fn edit(f: impl FnOnce(&mut GraphAsset)) -> GraphAsset {
    let mut g = full();
    f(&mut g);
    g
}

fn machine(g: &mut GraphAsset) -> Option<&mut StateMachineDef> {
    match g.nodes.get_mut(0) {
        Some(NodeDef::StateMachine(sm)) => Some(sm),
        _ => None,
    }
}

fn transition(g: &mut GraphAsset, i: usize) -> Option<&mut TransitionDef> {
    machine(g).and_then(|sm| sm.transitions.get_mut(i))
}

#[test]
fn round_trips() -> TestResult {
    let g = full();
    let bytes = g.encode();
    assert_eq!(bytes.len(), TOTAL);
    let back = GraphAsset::parse(&bytes)?;
    assert_eq!(back, g);
    assert_eq!(back.encode(), bytes);
    assert_eq!(back.depth(), 2);
    let mut minimal = full();
    minimal.look_at = None;
    minimal.foot_chains.clear();
    assert_eq!(GraphAsset::parse(&minimal.encode())?, minimal);
    Ok(())
}

#[test]
fn byte_offsets_match_the_fixture() {
    // Spot checks that the constants above point where the tests think they do.
    let b = full().encode();
    assert_eq!(b.get(NODES), Some(&2), "state machine kind");
    assert_eq!(b.get(BLEND), Some(&1), "blend kind");
    assert_eq!(b.get(CLIP2), Some(&0), "clip kind");
    assert_eq!(
        b.get(SM_T1..SM_T1 + 4),
        Some(&1u32.to_le_bytes()[..]),
        "transition 1 from"
    );
    assert_eq!(
        b.get(LAYER1..LAYER1 + 4),
        Some(&5u32.to_le_bytes()[..]),
        "layer 1 node"
    );
    assert_eq!(
        b.get(LOOK..LOOK + 2),
        Some(&7u16.to_le_bytes()[..]),
        "look-at head"
    );
}

#[test]
fn rejects_malformed_header() {
    assert_eq!(corrupt(0, b"MAGX").err(), Some(FormatError::Magic));
    assert_eq!(
        corrupt(4, &3u16.to_le_bytes()).err(),
        Some(FormatError::Version(3))
    );
    assert_eq!(corrupt(6, &2u16.to_le_bytes()).err(), Some(FormatError::Flags(2)));
    for (at, v) in [
        (8, 0u32),
        (8, MAX_BONES + 1),
        (12, MAX_CLIPS + 1),
        (16, MAX_PARAMETERS + 1),
        (20, 0),
        (20, MAX_NODES + 1),
        (24, 0),
        (24, MAX_LAYERS + 1),
        (28, MAX_FOOT_CHAINS + 1),
    ] {
        assert_eq!(
            corrupt(at, &v.to_le_bytes()).err(),
            Some(FormatError::Dimensions),
            "{at}={v}"
        );
    }
    assert_eq!(
        corrupt(32, &1u32.to_le_bytes()).err(),
        Some(FormatError::Reserved)
    );
    // Look-at flag cleared: 20 bytes remain.
    assert!(matches!(
        corrupt(6, &0u16.to_le_bytes()),
        Err(FormatError::Length { .. })
    ));
    let good = full().encode();
    let mut longer = good.clone();
    longer.push(0);
    assert!(matches!(
        GraphAsset::parse(&longer),
        Err(FormatError::Length { .. })
    ));
}

#[test]
fn rejects_malformed_records() {
    // Parameters.
    assert_eq!(corrupt(PARAMS + 4, &[3]).err(), Some(FormatError::Encoding(3)));
    assert_eq!(corrupt(PARAMS + 5, &[1]).err(), Some(FormatError::Reserved));
    assert_eq!(
        corrupt(PARAMS + 8, &f32::NAN.to_le_bytes()).err(),
        Some(FormatError::NonFinite)
    );
    // Nodes.
    assert_eq!(corrupt(NODES, &[3]).err(), Some(FormatError::Encoding(3)));
    assert_eq!(corrupt(NODES + 1, &[1]).err(), Some(FormatError::Reserved));
    assert_eq!(
        corrupt(NODES + 4, &0u32.to_le_bytes()).err(),
        Some(FormatError::Dimensions)
    );
    assert_eq!(
        corrupt(NODES + 12, &(MAX_TRANSITIONS + 1).to_le_bytes()).err(),
        Some(FormatError::Dimensions)
    );
    assert_eq!(
        corrupt(SM_T0 + 16, &2u16.to_le_bytes()).err(),
        Some(FormatError::Flags(2))
    );
    assert_eq!(
        corrupt(SM_T0 + 12, &0.5f32.to_le_bytes()).err(),
        Some(FormatError::Reserved),
        "exit time without its flag"
    );
    assert_eq!(
        corrupt(SM_T0 + 18, &(MAX_CONDITIONS + 1).to_le_bytes()).err(),
        Some(FormatError::Dimensions)
    );
    assert_eq!(
        corrupt(SM_T0_COND + 4, &[7]).err(),
        Some(FormatError::Encoding(7))
    );
    assert_eq!(corrupt(SM_T0_COND + 5, &[1]).err(), Some(FormatError::Reserved));
    assert_eq!(
        corrupt(BLEND + 8, &0u32.to_le_bytes()).err(),
        Some(FormatError::Dimensions)
    );
    assert_eq!(
        corrupt(BLEND + 8, &(MAX_BLEND_CHILDREN + 1).to_le_bytes()).err(),
        Some(FormatError::Dimensions)
    );
    assert_eq!(
        corrupt(CLIP2 + 8, &f32::INFINITY.to_le_bytes()).err(),
        Some(FormatError::NonFinite)
    );
    // Layers.
    assert_eq!(corrupt(LAYER1 + 8, &[2]).err(), Some(FormatError::Encoding(2)));
    assert_eq!(corrupt(LAYER1 + 9, &[1]).err(), Some(FormatError::Reserved));
    assert_eq!(
        corrupt(LAYER1 + 10, &9u16.to_le_bytes()).err(),
        Some(FormatError::Dimensions)
    );
    // IK.
    assert_eq!(
        corrupt(CHAINS + 6, &1u16.to_le_bytes()).err(),
        Some(FormatError::Reserved)
    );
    assert_eq!(
        corrupt(LOOK + 2, &1u16.to_le_bytes()).err(),
        Some(FormatError::Reserved)
    );
}

#[test]
fn rejects_bad_clips_and_parameters() {
    rejects(&edit(|g| g.clips.push(h(2))), FormatError::Inconsistent);
    let dup = bone_name_hash("speed");
    rejects(
        &edit(|g| {
            if let Some(p) = g.parameters.get_mut(1) {
                p.name_hash = dup;
            }
        }),
        FormatError::DuplicateId(dup),
    );
    for (i, v) in [(1, 0.5f32), (2, 1.0)] {
        rejects(
            &edit(|g| {
                if let Some(p) = g.parameters.get_mut(i) {
                    p.default = v;
                }
            }),
            FormatError::Inconsistent,
        );
    }
}

#[test]
fn rejects_bad_nodes() {
    rejects(
        &edit(|g| {
            if let Some(n) = g.nodes.get_mut(2) {
                *n = NodeDef::Clip { clip: 4, speed: 1.0 };
            }
        }),
        FormatError::Inconsistent,
    );
    let blend = |parameter: u32, t0: f32, t1: f32, node1: u32| {
        edit(|g| {
            if let Some(n) = g.nodes.get_mut(1) {
                *n = NodeDef::Blend1D {
                    parameter,
                    children: vec![
                        BlendChildDef {
                            node: 2,
                            threshold: t0,
                        },
                        BlendChildDef {
                            node: node1,
                            threshold: t1,
                        },
                    ],
                };
            }
        })
    };
    rejects(&blend(1, 0.0, 1.0, 3), FormatError::Inconsistent); // bool parameter
    rejects(&blend(9, 0.0, 1.0, 3), FormatError::Inconsistent); // no such parameter
    rejects(&blend(0, 1.0, 1.0, 3), FormatError::Inconsistent); // thresholds not increasing
    rejects(&blend(0, 0.0, 1.0, 6), FormatError::Inconsistent); // no such node
    rejects(&blend(0, 0.0, f32::NAN, 3), FormatError::NonFinite);
}

#[test]
fn rejects_bad_state_machines() {
    let with = |f: fn(&mut StateMachineDef)| {
        edit(|g| {
            if let Some(sm) = machine(g) {
                f(sm);
            }
        })
    };
    rejects(&with(|sm| sm.entry = 2), FormatError::Inconsistent);
    rejects(&with(|sm| sm.states.clear()), FormatError::Dimensions);
    let with_t = |f: fn(&mut TransitionDef)| {
        edit(|g| {
            if let Some(t) = transition(g, 0) {
                f(t);
            }
        })
    };
    rejects(&with_t(|t| t.from = Some(2)), FormatError::Inconsistent);
    // In memory the sentinel is spelled `None`; on disk it decodes to `None`.
    assert_eq!(
        with_t(|t| t.from = Some(ANY_STATE)).validate(),
        Err(FormatError::Inconsistent)
    );
    rejects(&with_t(|t| t.to = 2), FormatError::Inconsistent);
    rejects(&with_t(|t| t.crossfade = -0.1), FormatError::Inconsistent);
    rejects(&with_t(|t| t.exit_time = Some(-1.0)), FormatError::Inconsistent);
    rejects(&with_t(|t| t.conditions.clear()), FormatError::Inconsistent);
    rejects(&with_t(|t| t.crossfade = f32::NAN), FormatError::NonFinite);
    rejects(
        &with_t(|t| t.conditions = vec![cond(0, CompareOp::Triggered, 0.0)]),
        FormatError::Inconsistent,
    );
    rejects(
        &with_t(|t| t.conditions = vec![cond(2, CompareOp::Triggered, 1.0)]),
        FormatError::Inconsistent,
    );
    rejects(
        &with_t(|t| t.conditions = vec![cond(2, CompareOp::Greater, 0.0)]),
        FormatError::Inconsistent,
    );
    rejects(
        &with_t(|t| t.conditions = vec![cond(3, CompareOp::Greater, 0.0)]),
        FormatError::Inconsistent,
    );
    // An exit time alone is enough.
    let g = with_t(|t| {
        t.conditions.clear();
        t.exit_time = Some(1.0);
    });
    assert_eq!(g.validate(), Ok(()));
}

#[test]
fn rejects_bad_layers() {
    let with = |f: fn(&mut LayerDef)| {
        edit(|g| {
            if let Some(l) = g.layers.get_mut(1) {
                f(l);
            }
        })
    };
    rejects(&with(|l| l.weight = 1.5), FormatError::Inconsistent);
    rejects(&with(|l| l.weight = -0.1), FormatError::Inconsistent);
    rejects(&with(|l| l.mask = vec![5, 4]), FormatError::Inconsistent);
    rejects(&with(|l| l.mask = vec![4, 4]), FormatError::Inconsistent);
    rejects(&with(|l| l.mask = vec![8]), FormatError::Inconsistent);
    rejects(&with(|l| l.reference_clip = Some(4)), FormatError::Inconsistent);
    rejects(
        &with(|l| {
            l.mode = LayerMode::Override;
        }),
        FormatError::Inconsistent,
    );
    rejects(
        &edit(|g| {
            if let Some(l) = g.layers.get_mut(0) {
                l.mode = LayerMode::Additive;
            }
        }),
        FormatError::Inconsistent,
    );
    let bind_reference = with(|l| l.reference_clip = None);
    assert_eq!(bind_reference.validate(), Ok(()));
}

#[test]
fn rejects_bad_structure() {
    // A node referenced twice (a state shared with the blend tree).
    rejects(
        &edit(|g| {
            if let Some(sm) = machine(g) {
                sm.states = vec![1, 2];
            }
        }),
        FormatError::Inconsistent,
    );
    // A node never referenced.
    rejects(
        &edit(|g| g.nodes.push(NodeDef::Clip { clip: 0, speed: 1.0 })),
        FormatError::Inconsistent,
    );
    // Two layers with the same root.
    rejects(
        &edit(|g| {
            if let Some(l) = g.layers.get_mut(1) {
                l.node = 0;
            }
        }),
        FormatError::Inconsistent,
    );
    // A cycle: the blend node references itself, its clip child is dropped from the tree.
    rejects(
        &edit(|g| {
            if let Some(NodeDef::Blend1D { children, .. }) = g.nodes.get_mut(1)
                && let Some(c) = children.get_mut(0)
            {
                c.node = 1;
            }
        }),
        FormatError::Inconsistent,
    );
    // An indirect cycle 6 -> 7 -> 6, each referenced exactly once, unreachable from layers.
    rejects(
        &edit(|g| {
            for child in [7u32, 6] {
                g.nodes.push(NodeDef::Blend1D {
                    parameter: 0,
                    children: vec![BlendChildDef {
                        node: child,
                        threshold: 0.0,
                    }],
                });
            }
        }),
        FormatError::Inconsistent,
    );
    // A chain deeper than MAX_DEPTH.
    let deep = edit(|g| {
        let first = u32::try_from(g.nodes.len()).unwrap_or(0);
        if let Some(l) = g.layers.get_mut(1) {
            l.node = first;
        }
        for i in 0..=u32::try_from(MAX_DEPTH).unwrap_or(0) {
            g.nodes.push(NodeDef::Blend1D {
                parameter: 0,
                children: vec![BlendChildDef {
                    node: first + i + 1,
                    threshold: 0.0,
                }],
            });
        }
        g.nodes.push(NodeDef::Clip { clip: 3, speed: 1.0 });
        // The old layer-1 node 5 is now unreferenced: hang it under the first blend.
        if let Some(NodeDef::Blend1D { children, .. }) = g.nodes.get_mut(first as usize) {
            children.push(BlendChildDef {
                node: 5,
                threshold: 1.0,
            });
        }
    });
    rejects(&deep, FormatError::Inconsistent);
}

#[test]
fn rejects_bad_ik() {
    let chain = |root: u16, mid: u16, tip: u16, pole: [f32; 3]| {
        edit(|g| {
            g.foot_chains = vec![TwoBoneChainDef { root, mid, tip, pole }];
        })
    };
    rejects(&chain(2, 1, 3, [0.0, 0.0, 1.0]), FormatError::Inconsistent);
    rejects(&chain(1, 2, 2, [0.0, 0.0, 1.0]), FormatError::Inconsistent);
    rejects(&chain(1, 2, 8, [0.0, 0.0, 1.0]), FormatError::Inconsistent);
    rejects(&chain(1, 2, 3, [0.0; 3]), FormatError::Geometry);
    rejects(&chain(1, 2, 3, [f32::NAN, 0.0, 1.0]), FormatError::NonFinite);
    let look = |head: u16, axis: [f32; 3], max_angle: f32| {
        edit(|g| {
            g.look_at = Some(LookAtDef {
                head,
                axis,
                max_angle,
            });
        })
    };
    rejects(&look(8, [0.0, 0.0, 1.0], 1.0), FormatError::Inconsistent);
    rejects(&look(7, [0.0, 0.0, 2.0], 1.0), FormatError::Geometry);
    rejects(&look(7, [0.0, 0.0, 1.0], 0.0), FormatError::Geometry);
    rejects(&look(7, [0.0, 0.0, 1.0], 3.2), FormatError::Geometry);
    rejects(&look(7, [0.0, 0.0, 1.0], f32::INFINITY), FormatError::NonFinite);
    assert_eq!(look(7, [0.0, 0.0, 1.0], std::f32::consts::PI).validate(), Ok(()));
}

#[test]
fn compare_ops_evaluate_and_round_trip() -> TestResult {
    let all = [
        CompareOp::Greater,
        CompareOp::GreaterOrEqual,
        CompareOp::Less,
        CompareOp::LessOrEqual,
        CompareOp::Equal,
        CompareOp::NotEqual,
        CompareOp::Triggered,
    ];
    for op in all {
        assert_eq!(CompareOp::from_u8(op.to_u8())?, op);
    }
    let results: Vec<bool> = all.iter().map(|op| op.holds(1.0, 1.0)).collect();
    assert_eq!(results, [false, true, false, true, true, false, true]);
    assert!(!CompareOp::Triggered.holds(0.0, 0.0));
    for k in [ParameterKind::Float, ParameterKind::Bool, ParameterKind::Trigger] {
        assert_eq!(ParameterKind::from_u8(k.to_u8())?, k);
    }
    Ok(())
}

#[test]
fn no_corruption_or_truncation_panics() {
    let bytes = full().encode();
    for i in 0..bytes.len() {
        for mask in [0x01u8, 0x80, 0xff] {
            let mut b = bytes.clone();
            if let Some(x) = b.get_mut(i) {
                *x ^= mask;
            }
            if let Ok(g) = GraphAsset::parse(&b) {
                assert!(GraphAsset::parse(&g.encode()).is_ok());
            }
        }
    }
    for len in 0..bytes.len() {
        assert!(
            GraphAsset::parse(bytes.get(..len).unwrap_or(&[])).is_err(),
            "truncated to {len}"
        );
    }
}

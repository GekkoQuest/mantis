//! The runtime animation graph: a validated [`GraphAsset`] bound to a skeleton and its
//! resolved clips. Immutable and shared (through `Arc`) by every instance that plays it.

use std::sync::Arc;

use glam::Vec3;
use mantis_core::content::ContentHash;
use mantis_formats::anim_graph::{
    CompareOp, GraphAsset, LayerMode, LookAtDef, NodeDef, ParameterKind, TwoBoneChainDef,
};

use crate::clip::Clip;
use crate::error::AnimError;
use crate::skeleton::{Pose, Skeleton};
use crate::transform::Transform;

/// A parameter handle, from [`AnimGraph::parameter`].
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct ParamId(pub(crate) usize);

impl ParamId {
    /// The parameter's index in the graph.
    pub fn index(self) -> usize {
        self.0
    }
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct Condition {
    pub(crate) parameter: usize,
    pub(crate) op: CompareOp,
    pub(crate) value: f32,
}

#[derive(Clone, Debug)]
pub(crate) struct Transition {
    pub(crate) from: Option<usize>,
    pub(crate) to: usize,
    pub(crate) crossfade: f32,
    pub(crate) exit_time: Option<f32>,
    pub(crate) conditions: Vec<Condition>,
}

#[derive(Clone, Debug)]
pub(crate) enum Node {
    Clip {
        clip: usize,
        speed: f32,
    },
    Blend1D {
        parameter: usize,
        /// `(node, threshold)`, thresholds strictly increasing.
        children: Vec<(usize, f32)>,
    },
    Machine {
        states: Vec<usize>,
        entry: usize,
        transitions: Vec<Transition>,
    },
}

#[derive(Clone, Debug)]
pub(crate) struct Layer {
    pub(crate) node: usize,
    pub(crate) weight: f32,
    pub(crate) mode: LayerMode,
    pub(crate) mask: Vec<u16>,
    /// Per-bone inverse of the additive reference pose (empty for override layers).
    pub(crate) reference_inverse: Vec<Transform>,
}

/// A two-bone IK chain bound to the skeleton.
#[derive(Clone, Copy, PartialEq, Debug)]
pub struct TwoBoneChain {
    /// First bone (for example the hip).
    pub root: usize,
    /// Bending bone (for example the knee), a descendant of `root`.
    pub mid: usize,
    /// End bone (for example the ankle), a descendant of `mid`.
    pub tip: usize,
    /// Model-space direction the middle joint bends toward.
    pub pole: Vec3,
}

/// A look-at chain bound to the skeleton.
#[derive(Clone, Copy, PartialEq, Debug)]
pub struct LookAt {
    /// The bone that turns.
    pub head: usize,
    /// The bone's local forward axis (unit).
    pub axis: Vec3,
    /// Largest correction, radians.
    pub max_angle: f32,
}

/// A bound animation graph.
#[derive(Clone, Debug)]
pub struct AnimGraph {
    skeleton: Arc<Skeleton>,
    clips: Vec<Arc<Clip>>,
    parameter_hashes: Vec<u32>,
    parameter_kinds: Vec<ParameterKind>,
    parameter_defaults: Vec<f32>,
    pub(crate) nodes: Vec<Node>,
    pub(crate) layers: Vec<Layer>,
    foot_chains: Vec<TwoBoneChain>,
    look_at: Option<LookAt>,
    depth: usize,
}

impl AnimGraph {
    /// Binds a graph asset to a skeleton, resolving each referenced clip by content hash.
    ///
    /// # Errors
    /// [`AnimError::Format`] when the asset breaks a format rule,
    /// [`AnimError::BoneCountMismatch`] when the graph or a clip targets another
    /// skeleton, [`AnimError::MissingClip`] when `resolve` has no clip for a hash, and
    /// [`AnimError::IkChain`] when a foot chain is not an ancestor line.
    pub fn new(
        skeleton: Arc<Skeleton>,
        asset: &GraphAsset,
        mut resolve: impl FnMut(&ContentHash) -> Option<Arc<Clip>>,
    ) -> Result<Self, AnimError> {
        asset.validate()?;
        let bones = skeleton.bone_count();
        check_bones(bones, asset.bone_count as usize)?;
        let mut clips = Vec::with_capacity(asset.clips.len());
        for hash in &asset.clips {
            let clip = resolve(hash).ok_or(AnimError::MissingClip(*hash))?;
            check_bones(bones, clip.bone_count())?;
            clips.push(clip);
        }
        let layers = asset
            .layers
            .iter()
            .map(|l| {
                let reference_inverse = match l.mode {
                    LayerMode::Override => Vec::new(),
                    LayerMode::Additive => {
                        reference_inverse(&skeleton, l.reference_clip.and_then(|c| clips.get(c as usize)))
                    }
                };
                Layer {
                    node: l.node as usize,
                    weight: l.weight,
                    mode: l.mode,
                    mask: l.mask.clone(),
                    reference_inverse,
                }
            })
            .collect();
        let foot_chains = asset
            .foot_chains
            .iter()
            .enumerate()
            .map(|(i, c)| bind_chain(&skeleton, i, c))
            .collect::<Result<Vec<_>, _>>()?;
        Ok(Self {
            clips,
            parameter_hashes: asset.parameters.iter().map(|p| p.name_hash).collect(),
            parameter_kinds: asset.parameters.iter().map(|p| p.kind).collect(),
            parameter_defaults: asset.parameters.iter().map(|p| p.default).collect(),
            nodes: asset.nodes.iter().map(bind_node).collect(),
            layers,
            foot_chains,
            look_at: asset.look_at.as_ref().map(bind_look_at),
            depth: asset.depth(),
            skeleton,
        })
    }

    /// The skeleton.
    pub fn skeleton(&self) -> &Arc<Skeleton> {
        &self.skeleton
    }

    /// The parameter with `name_hash` (FNV-1a 32 of its authored name).
    pub fn parameter(&self, name_hash: u32) -> Option<ParamId> {
        self.parameter_hashes
            .iter()
            .position(|h| *h == name_hash)
            .map(ParamId)
    }

    /// A parameter's kind.
    pub fn parameter_kind(&self, id: ParamId) -> Option<ParameterKind> {
        self.parameter_kinds.get(id.0).copied()
    }

    /// Parameter defaults, by index.
    pub fn parameter_defaults(&self) -> &[f32] {
        &self.parameter_defaults
    }

    /// Number of layers.
    pub fn layer_count(&self) -> usize {
        self.layers.len()
    }

    /// Number of nodes.
    pub fn node_count(&self) -> usize {
        self.nodes.len()
    }

    /// Foot IK chains.
    pub fn foot_chains(&self) -> &[TwoBoneChain] {
        &self.foot_chains
    }

    /// The look-at chain.
    pub fn look_at(&self) -> Option<&LookAt> {
        self.look_at.as_ref()
    }

    /// The clip at `index` in the graph's clip list.
    pub fn clip(&self, index: usize) -> Option<&Arc<Clip>> {
        self.clips.get(index)
    }

    /// Depth of the deepest layer tree (scratch poses an instance needs, minus one).
    pub fn depth(&self) -> usize {
        self.depth
    }
}

fn check_bones(expected: usize, actual: usize) -> Result<(), AnimError> {
    if expected == actual {
        Ok(())
    } else {
        Err(AnimError::BoneCountMismatch { expected, actual })
    }
}

fn reference_inverse(skeleton: &Skeleton, clip: Option<&Arc<Clip>>) -> Vec<Transform> {
    let mut pose = Pose::bind(skeleton);
    if let Some(c) = clip {
        c.sample(skeleton.bind_pose(), 0.0, pose.as_mut_slice());
    }
    pose.as_slice().iter().map(Transform::inverse).collect()
}

fn bind_node(def: &NodeDef) -> Node {
    match def {
        NodeDef::Clip { clip, speed } => Node::Clip {
            clip: *clip as usize,
            speed: *speed,
        },
        NodeDef::Blend1D { parameter, children } => Node::Blend1D {
            parameter: *parameter as usize,
            children: children.iter().map(|c| (c.node as usize, c.threshold)).collect(),
        },
        NodeDef::StateMachine(sm) => Node::Machine {
            states: sm.states.iter().map(|s| *s as usize).collect(),
            entry: sm.entry as usize,
            transitions: sm
                .transitions
                .iter()
                .map(|t| Transition {
                    from: t.from.map(|f| f as usize),
                    to: t.to as usize,
                    crossfade: t.crossfade,
                    exit_time: t.exit_time,
                    conditions: t
                        .conditions
                        .iter()
                        .map(|c| Condition {
                            parameter: c.parameter as usize,
                            op: c.op,
                            value: c.value,
                        })
                        .collect(),
                })
                .collect(),
        },
    }
}

fn bind_chain(skeleton: &Skeleton, index: usize, c: &TwoBoneChainDef) -> Result<TwoBoneChain, AnimError> {
    let chain = TwoBoneChain {
        root: usize::from(c.root),
        mid: usize::from(c.mid),
        tip: usize::from(c.tip),
        pole: Vec3::from_array(c.pole).normalize_or(Vec3::Z),
    };
    if skeleton.is_ancestor(chain.root, chain.mid) && skeleton.is_ancestor(chain.mid, chain.tip) {
        Ok(chain)
    } else {
        Err(AnimError::IkChain(index))
    }
}

fn bind_look_at(l: &LookAtDef) -> LookAt {
    LookAt {
        head: usize::from(l.head),
        axis: Vec3::from_array(l.axis).normalize_or(Vec3::Z),
        max_angle: l.max_angle,
    }
}

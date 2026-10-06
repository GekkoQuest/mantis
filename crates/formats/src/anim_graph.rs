//! Animation graph v1: the data-defined animation graph of one skeleton, produced by the
//! cook and evaluated by the animation runtime (plan 8.4).
//!
//! Spatial convention: every position, direction, and transform in this format is in the
//! left-handed world frame of decision 0019 (+X right, +Y up, +Z forward, as
//! `mantis_core::kinematics` defines it); IK poles and look-at axes are bone-local in that frame.
//!
//! A graph has typed parameters (float, bool, trigger), a node pool (clip players, 1D
//! blend trees, state machines), a layer stack (override or additive, weighted, optionally
//! masked to a bone list), and IK settings (two-bone foot chains and a look-at chain).
//! Clips are referenced by content hash; the runtime resolves them.
//!
//! # Structure rules
//!
//! Node references (blend children and state-machine states) and layer references form a
//! forest: every node is referenced exactly once, by one parent node or by one layer, and
//! every node is reachable from a layer. So each layer has exactly one root node, no node
//! references itself directly or indirectly, and no node is shared (nodes carry playback
//! state, so sharing one would advance it twice). The longest path from a layer's root to
//! a leaf is at most [`MAX_DEPTH`] references.
//!
//! # Layout (little-endian)
//!
//! | offset | size | field |
//! |---|---|---|
//! | 0 | 4 | magic `"MAGR"` |
//! | 4 | 2 | version `u16` = 1 |
//! | 6 | 2 | flags `u16`: bit 0 look-at chain present; other bits 0 |
//! | 8 | 4 | bone count `u32` of the target skeleton, 1 to [`MAX_BONES`] |
//! | 12 | 4 | clip count `u32`, 0 to [`MAX_CLIPS`] |
//! | 16 | 4 | parameter count `u32`, 0 to [`MAX_PARAMETERS`] |
//! | 20 | 4 | node count `u32`, 1 to [`MAX_NODES`] |
//! | 24 | 4 | layer count `u32`, 1 to [`MAX_LAYERS`] |
//! | 28 | 4 | foot chain count `u32`, 0 to [`MAX_FOOT_CHAINS`] |
//! | 32 | 4 | reserved, 0 |
//! | 36 | | clips: `[u8; 32]` content hashes, unique |
//! | | | parameters, 12 bytes each |
//! | | | nodes |
//! | | | layers |
//! | | | foot chains, 20 bytes each |
//! | | | look-at chain, 20 bytes, when flag bit 0 is set |
//!
//! Parameter (12 bytes): `name_hash: u32` (unique), `kind: u8` (0 float, 1 bool,
//! 2 trigger), 3 reserved bytes (0), `default: f32` (float: finite; bool: 0 or 1;
//! trigger: 0).
//!
//! Node: `kind: u8`, 3 reserved bytes (0), then by kind:
//! - 0 clip: `clip: u32` (index into the clip list), `speed: f32` (finite; negative plays
//!   backwards).
//! - 1 blend 1D: `parameter: u32` (a float parameter), `child_count: u32` (1 to
//!   [`MAX_BLEND_CHILDREN`]), then per child `node: u32`, `threshold: f32` (strictly
//!   increasing).
//! - 2 state machine: `state_count: u32` (1 to [`MAX_STATES`]), `entry: u32` (a state
//!   index), `transition_count: u32` (0 to [`MAX_TRANSITIONS`]), `states: [u32;
//!   state_count]` (node indices), then per transition `from: u32` (a state index or
//!   [`ANY_STATE`]), `to: u32` (a state index), `crossfade: f32` (seconds, >= 0),
//!   `exit_time: f32` (normalized time of the source state, >= 0; exactly 0 when unused),
//!   `flags: u16` (bit 0 exit time used; other bits 0), `condition_count: u16` (0 to
//!   [`MAX_CONDITIONS`]), then per condition `parameter: u32`, `op: u8` (0 `>`, 1 `>=`,
//!   2 `<`, 3 `<=`, 4 `==`, 5 `!=`, 6 triggered), 3 reserved bytes (0), `value: f32`.
//!   Comparisons apply to float and bool parameters (a bool reads as 0 or 1); triggered
//!   applies to trigger parameters only and has value 0. A transition needs an exit time
//!   or at least one condition.
//!
//! Layer: `node: u32`, `weight: f32` (0 to 1), `mode: u8` (0 override, 1 additive),
//! 1 reserved byte (0), `mask_count: u16` (0 to the bone count; 0 means every bone),
//! `reference_clip: u32` ([`NO_CLIP`] for the bind pose, else a clip index whose first
//! frame is the additive reference; always [`NO_CLIP`] for override layers), then
//! `mask: [u16; mask_count]`, strictly increasing bone indices. Layer 0 is an override
//! layer.
//!
//! Foot chain (20 bytes): `root: u16`, `mid: u16`, `tip: u16` (bone indices, `root < mid <
//! tip`), 2 reserved bytes (0), `pole: [f32; 3]` (model-space bend direction, non-zero).
//!
//! Look-at (20 bytes): `head: u16` (bone index), 2 reserved bytes (0), `axis: [f32; 3]`
//! (the head bone's local forward, unit within [`crate::skeleton::UNIT_TOLERANCE`]),
//! `max_angle: f32` (radians, in `(0, pi]`).
//!
//! Every `f32` must be finite, and the length must match exactly.

use mantis_core::content::ContentHash;

use crate::bytes::{FormatError, Reader, Writer};
use crate::skeleton::{MAX_BONES, UNIT_TOLERANCE};

/// Magic bytes.
pub const MAGIC: [u8; 4] = *b"MAGR";
/// Flag: a look-at chain follows the foot chains.
pub const FLAG_LOOK_AT: u16 = 1 << 0;
/// Most clips per graph.
pub const MAX_CLIPS: u32 = 1024;
/// Most parameters per graph.
pub const MAX_PARAMETERS: u32 = 256;
/// Most nodes per graph.
pub const MAX_NODES: u32 = 1024;
/// Most layers per graph.
pub const MAX_LAYERS: u32 = 8;
/// Most two-bone foot chains per graph.
pub const MAX_FOOT_CHAINS: u32 = 8;
/// Most children per blend node.
pub const MAX_BLEND_CHILDREN: u32 = 32;
/// Most states per state machine.
pub const MAX_STATES: u32 = 64;
/// Most transitions per state machine.
pub const MAX_TRANSITIONS: u32 = 256;
/// Most conditions per transition.
pub const MAX_CONDITIONS: u16 = 8;
/// Longest chain of references from a layer's root node to a leaf.
pub const MAX_DEPTH: usize = 16;
/// A transition's `from` value meaning "from any state".
pub const ANY_STATE: u32 = u32::MAX;
/// A layer's `reference_clip` value meaning "the skeleton's bind pose".
pub const NO_CLIP: u32 = u32::MAX;
const TRANSITION_EXIT_TIME: u16 = 1 << 0;

/// A parameter's type.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Hash)]
pub enum ParameterKind {
    /// A float.
    Float,
    /// A bool, stored as 0 or 1.
    Bool,
    /// A one-shot flag consumed by the transition it fires.
    Trigger,
}

impl ParameterKind {
    /// The on-disk value.
    pub fn to_u8(self) -> u8 {
        match self {
            ParameterKind::Float => 0,
            ParameterKind::Bool => 1,
            ParameterKind::Trigger => 2,
        }
    }

    /// Decodes an on-disk value.
    ///
    /// # Errors
    /// [`FormatError::Encoding`] for an unknown value.
    pub fn from_u8(v: u8) -> Result<Self, FormatError> {
        match v {
            0 => Ok(ParameterKind::Float),
            1 => Ok(ParameterKind::Bool),
            2 => Ok(ParameterKind::Trigger),
            _ => Err(FormatError::Encoding(u32::from(v))),
        }
    }
}

/// A graph parameter.
#[derive(Clone, Copy, PartialEq, Debug)]
pub struct ParameterDef {
    /// Stable id: [`crate::skeleton::bone_name_hash`] (FNV-1a 32) of the authored name.
    pub name_hash: u32,
    /// Type.
    pub kind: ParameterKind,
    /// Initial value.
    pub default: f32,
}

/// A condition's comparison.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Hash)]
pub enum CompareOp {
    /// `parameter > value`.
    Greater,
    /// `parameter >= value`.
    GreaterOrEqual,
    /// `parameter < value`.
    Less,
    /// `parameter <= value`.
    LessOrEqual,
    /// `parameter == value`.
    Equal,
    /// `parameter != value`.
    NotEqual,
    /// The trigger parameter is set.
    Triggered,
}

impl CompareOp {
    /// The on-disk value.
    pub fn to_u8(self) -> u8 {
        match self {
            CompareOp::Greater => 0,
            CompareOp::GreaterOrEqual => 1,
            CompareOp::Less => 2,
            CompareOp::LessOrEqual => 3,
            CompareOp::Equal => 4,
            CompareOp::NotEqual => 5,
            CompareOp::Triggered => 6,
        }
    }

    /// Decodes an on-disk value.
    ///
    /// # Errors
    /// [`FormatError::Encoding`] for an unknown value.
    pub fn from_u8(v: u8) -> Result<Self, FormatError> {
        Ok(match v {
            0 => CompareOp::Greater,
            1 => CompareOp::GreaterOrEqual,
            2 => CompareOp::Less,
            3 => CompareOp::LessOrEqual,
            4 => CompareOp::Equal,
            5 => CompareOp::NotEqual,
            6 => CompareOp::Triggered,
            _ => return Err(FormatError::Encoding(u32::from(v))),
        })
    }

    /// Evaluates the comparison on a parameter value (a trigger reads as 0 or 1).
    pub fn holds(self, parameter: f32, value: f32) -> bool {
        match self {
            CompareOp::Greater => parameter > value,
            CompareOp::GreaterOrEqual => parameter >= value,
            CompareOp::Less => parameter < value,
            CompareOp::LessOrEqual => parameter <= value,
            CompareOp::Equal => parameter == value,
            CompareOp::NotEqual => parameter != value,
            CompareOp::Triggered => parameter != 0.0,
        }
    }
}

/// One transition condition.
#[derive(Clone, Copy, PartialEq, Debug)]
pub struct ConditionDef {
    /// Parameter index.
    pub parameter: u32,
    /// Comparison.
    pub op: CompareOp,
    /// Right-hand side (0 for [`CompareOp::Triggered`]).
    pub value: f32,
}

/// A state-machine transition. It fires when every condition holds and, if set, the
/// source state's normalized time has reached the exit time.
#[derive(Clone, PartialEq, Debug)]
pub struct TransitionDef {
    /// Source state index; `None` for any state.
    pub from: Option<u32>,
    /// Destination state index.
    pub to: u32,
    /// Crossfade length in seconds (0 switches at once).
    pub crossfade: f32,
    /// Normalized source time the transition waits for.
    pub exit_time: Option<f32>,
    /// Conditions, all of which must hold.
    pub conditions: Vec<ConditionDef>,
}

/// A state machine node.
#[derive(Clone, PartialEq, Debug)]
pub struct StateMachineDef {
    /// Node index of each state.
    pub states: Vec<u32>,
    /// Initial state index.
    pub entry: u32,
    /// Transitions, checked in order; the first that fires wins.
    pub transitions: Vec<TransitionDef>,
}

/// One child of a 1D blend.
#[derive(Clone, Copy, PartialEq, Debug)]
pub struct BlendChildDef {
    /// Node index.
    pub node: u32,
    /// Parameter value at which this child has full weight.
    pub threshold: f32,
}

/// A graph node.
#[derive(Clone, PartialEq, Debug)]
pub enum NodeDef {
    /// Plays one clip.
    Clip {
        /// Clip index.
        clip: u32,
        /// Playback rate (negative plays backwards).
        speed: f32,
    },
    /// Blends children by a float parameter between neighboring thresholds.
    Blend1D {
        /// Float parameter index.
        parameter: u32,
        /// Children with strictly increasing thresholds.
        children: Vec<BlendChildDef>,
    },
    /// A state machine.
    StateMachine(StateMachineDef),
}

impl NodeDef {
    /// Calls `f` with every node this node references.
    pub fn for_each_child(&self, mut f: impl FnMut(u32)) {
        match self {
            NodeDef::Clip { .. } => {}
            NodeDef::Blend1D { children, .. } => children.iter().for_each(|c| f(c.node)),
            NodeDef::StateMachine(sm) => sm.states.iter().for_each(|s| f(*s)),
        }
    }
}

/// How a layer combines with the layers below it.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Hash)]
pub enum LayerMode {
    /// Blends toward the layer's pose by its weight.
    Override,
    /// Applies the layer's difference from its reference pose, scaled by its weight.
    Additive,
}

/// One layer of the stack.
#[derive(Clone, PartialEq, Debug)]
pub struct LayerDef {
    /// Root node index.
    pub node: u32,
    /// Default weight, 0 to 1.
    pub weight: f32,
    /// Combination mode.
    pub mode: LayerMode,
    /// Additive reference: a clip index (its first frame), or `None` for the bind pose.
    pub reference_clip: Option<u32>,
    /// Affected bones, strictly increasing; empty means every bone.
    pub mask: Vec<u16>,
}

/// A two-bone IK chain (for example hip, knee, ankle).
#[derive(Clone, Copy, PartialEq, Debug)]
pub struct TwoBoneChainDef {
    /// First bone.
    pub root: u16,
    /// Middle bone (the joint that bends).
    pub mid: u16,
    /// End bone (reaches the target).
    pub tip: u16,
    /// Model-space direction the middle joint bends toward.
    pub pole: [f32; 3],
}

/// A look-at chain.
#[derive(Clone, Copy, PartialEq, Debug)]
pub struct LookAtDef {
    /// The bone that turns.
    pub head: u16,
    /// The bone's local forward axis.
    pub axis: [f32; 3],
    /// Largest correction, radians.
    pub max_angle: f32,
}

/// A graph asset.
#[derive(Clone, PartialEq, Debug)]
pub struct GraphAsset {
    /// Bone count of the skeleton this graph targets.
    pub bone_count: u32,
    /// Referenced clips by content hash.
    pub clips: Vec<ContentHash>,
    /// Parameters.
    pub parameters: Vec<ParameterDef>,
    /// Node pool.
    pub nodes: Vec<NodeDef>,
    /// Layer stack, bottom first.
    pub layers: Vec<LayerDef>,
    /// Two-bone foot IK chains.
    pub foot_chains: Vec<TwoBoneChainDef>,
    /// Look-at chain.
    pub look_at: Option<LookAtDef>,
}

fn count_in(len: usize, min: u32, max: u32) -> bool {
    u32::try_from(len).is_ok_and(|n| (min..=max).contains(&n))
}

impl GraphAsset {
    /// Checks every rule of the format on an in-memory graph (the parser runs it too).
    ///
    /// # Errors
    /// [`FormatError::Dimensions`] for counts out of range, [`FormatError::DuplicateId`]
    /// for a repeated parameter name, [`FormatError::NonFinite`] for a non-finite value,
    /// [`FormatError::Geometry`] for a bad IK pole, axis, or angle, and
    /// [`FormatError::Inconsistent`] for every other broken rule.
    pub fn validate(&self) -> Result<(), FormatError> {
        let counts_ok = count_in(self.bone_count as usize, 1, MAX_BONES)
            && count_in(self.clips.len(), 0, MAX_CLIPS)
            && count_in(self.parameters.len(), 0, MAX_PARAMETERS)
            && count_in(self.nodes.len(), 1, MAX_NODES)
            && count_in(self.layers.len(), 1, MAX_LAYERS)
            && count_in(self.foot_chains.len(), 0, MAX_FOOT_CHAINS);
        if !counts_ok {
            return Err(FormatError::Dimensions);
        }
        let mut sorted = self.clips.clone();
        sorted.sort_unstable_by(|a, b| a.as_bytes().cmp(b.as_bytes()));
        if sorted.windows(2).any(|w| w.first() == w.get(1)) {
            return Err(FormatError::Inconsistent);
        }
        self.validate_parameters()?;
        for node in &self.nodes {
            self.validate_node(node)?;
        }
        for (i, layer) in self.layers.iter().enumerate() {
            self.validate_layer(i, layer)?;
        }
        self.validate_structure()?;
        self.validate_ik()
    }

    /// Depth of the deepest layer tree: 0 when every layer root is a clip node.
    ///
    /// Meaningful only on a validated graph; an invalid one returns `MAX_DEPTH + 1` or less.
    pub fn depth(&self) -> usize {
        let mut deepest = 0;
        let mut stack: Vec<(u32, usize)> = self.layers.iter().map(|l| (l.node, 0)).collect();
        let mut visits = 0usize;
        while let Some((node, depth)) = stack.pop() {
            visits += 1;
            deepest = deepest.max(depth);
            if depth > MAX_DEPTH || visits > self.nodes.len() {
                return MAX_DEPTH + 1;
            }
            if let Some(def) = self.nodes.get(node as usize) {
                def.for_each_child(|c| stack.push((c, depth + 1)));
            }
        }
        deepest
    }

    fn validate_parameters(&self) -> Result<(), FormatError> {
        let mut hashes: Vec<u32> = self.parameters.iter().map(|p| p.name_hash).collect();
        hashes.sort_unstable();
        if let Some(pair) = hashes.windows(2).find(|w| w.first() == w.get(1)) {
            return Err(FormatError::DuplicateId(pair.first().copied().unwrap_or(0)));
        }
        for p in &self.parameters {
            if !p.default.is_finite() {
                return Err(FormatError::NonFinite);
            }
            let ok = match p.kind {
                ParameterKind::Float => true,
                ParameterKind::Bool => p.default == 0.0 || p.default == 1.0,
                ParameterKind::Trigger => p.default == 0.0,
            };
            if !ok {
                return Err(FormatError::Inconsistent);
            }
        }
        Ok(())
    }

    fn parameter_kind(&self, index: u32) -> Result<ParameterKind, FormatError> {
        self.parameters
            .get(index as usize)
            .map(|p| p.kind)
            .ok_or(FormatError::Inconsistent)
    }

    fn validate_node(&self, node: &NodeDef) -> Result<(), FormatError> {
        let node_count = self.nodes.len();
        match node {
            NodeDef::Clip { clip, speed } => {
                if !speed.is_finite() {
                    return Err(FormatError::NonFinite);
                }
                if *clip as usize >= self.clips.len() {
                    return Err(FormatError::Inconsistent);
                }
            }
            NodeDef::Blend1D { parameter, children } => {
                if !count_in(children.len(), 1, MAX_BLEND_CHILDREN) {
                    return Err(FormatError::Dimensions);
                }
                if children.iter().any(|c| !c.threshold.is_finite()) {
                    return Err(FormatError::NonFinite);
                }
                let increasing = children.windows(2).all(|w| match w {
                    [a, b] => a.threshold < b.threshold,
                    _ => false,
                });
                if self.parameter_kind(*parameter)? != ParameterKind::Float
                    || !increasing
                    || children.iter().any(|c| c.node as usize >= node_count)
                {
                    return Err(FormatError::Inconsistent);
                }
            }
            NodeDef::StateMachine(sm) => self.validate_state_machine(sm)?,
        }
        Ok(())
    }

    fn validate_state_machine(&self, sm: &StateMachineDef) -> Result<(), FormatError> {
        if !count_in(sm.states.len(), 1, MAX_STATES) || !count_in(sm.transitions.len(), 0, MAX_TRANSITIONS) {
            return Err(FormatError::Dimensions);
        }
        let states = sm.states.len();
        if sm.entry as usize >= states || sm.states.iter().any(|s| *s as usize >= self.nodes.len()) {
            return Err(FormatError::Inconsistent);
        }
        for t in &sm.transitions {
            if t.conditions.len() > usize::from(MAX_CONDITIONS) {
                return Err(FormatError::Dimensions);
            }
            let floats = [t.crossfade, t.exit_time.unwrap_or(0.0)];
            if floats
                .iter()
                .chain(t.conditions.iter().map(|c| &c.value))
                .any(|v| !v.is_finite())
            {
                return Err(FormatError::NonFinite);
            }
            let from_ok = t.from.is_none_or(|f| f != ANY_STATE && (f as usize) < states);
            if !from_ok
                || t.to as usize >= states
                || t.crossfade < 0.0
                || t.exit_time.is_some_and(|e| e < 0.0)
                || (t.exit_time.is_none() && t.conditions.is_empty())
            {
                return Err(FormatError::Inconsistent);
            }
            for c in &t.conditions {
                let kind = self.parameter_kind(c.parameter)?;
                let ok = match c.op {
                    CompareOp::Triggered => kind == ParameterKind::Trigger && c.value == 0.0,
                    _ => kind != ParameterKind::Trigger,
                };
                if !ok {
                    return Err(FormatError::Inconsistent);
                }
            }
        }
        Ok(())
    }

    fn validate_layer(&self, index: usize, layer: &LayerDef) -> Result<(), FormatError> {
        if !layer.weight.is_finite() {
            return Err(FormatError::NonFinite);
        }
        if layer.mask.len() > self.bone_count as usize {
            return Err(FormatError::Dimensions);
        }
        let mask_ok = layer.mask.windows(2).all(|w| w.first() < w.get(1))
            && layer.mask.iter().all(|b| u32::from(*b) < self.bone_count);
        let reference_ok = match (layer.mode, layer.reference_clip) {
            (LayerMode::Override, None) => true,
            (LayerMode::Override, Some(_)) => false,
            (LayerMode::Additive, r) => index > 0 && r.is_none_or(|c| (c as usize) < self.clips.len()),
        };
        if layer.node as usize >= self.nodes.len()
            || !(0.0..=1.0).contains(&layer.weight)
            || !mask_ok
            || !reference_ok
        {
            return Err(FormatError::Inconsistent);
        }
        Ok(())
    }

    fn validate_structure(&self) -> Result<(), FormatError> {
        let mut references = vec![0u32; self.nodes.len()];
        let mut count = |n: u32| {
            if let Some(r) = references.get_mut(n as usize) {
                *r += 1;
            }
        };
        for layer in &self.layers {
            count(layer.node);
        }
        for node in &self.nodes {
            node.for_each_child(&mut count);
        }
        if references.iter().any(|r| *r != 1) {
            return Err(FormatError::Inconsistent);
        }
        // With every node referenced exactly once, a node on a cycle is unreachable from
        // the layers, so reachability rules out cycles.
        let mut reached = 0usize;
        let mut stack: Vec<(u32, usize)> = self.layers.iter().map(|l| (l.node, 0)).collect();
        while let Some((node, depth)) = stack.pop() {
            reached += 1;
            if depth > MAX_DEPTH || reached > self.nodes.len() {
                return Err(FormatError::Inconsistent);
            }
            if let Some(def) = self.nodes.get(node as usize) {
                def.for_each_child(|c| stack.push((c, depth + 1)));
            }
        }
        if reached != self.nodes.len() {
            return Err(FormatError::Inconsistent);
        }
        Ok(())
    }

    fn validate_ik(&self) -> Result<(), FormatError> {
        let bone_ok = |b: u16| u32::from(b) < self.bone_count;
        for chain in &self.foot_chains {
            if chain.pole.iter().any(|v| !v.is_finite()) {
                return Err(FormatError::NonFinite);
            }
            if !(chain.root < chain.mid && chain.mid < chain.tip && bone_ok(chain.tip)) {
                return Err(FormatError::Inconsistent);
            }
            let len_sq: f32 = chain.pole.iter().map(|v| v * v).sum();
            if len_sq <= 1e-12 {
                return Err(FormatError::Geometry);
            }
        }
        if let Some(look) = &self.look_at {
            if look.axis.iter().chain([&look.max_angle]).any(|v| !v.is_finite()) {
                return Err(FormatError::NonFinite);
            }
            if !bone_ok(look.head) {
                return Err(FormatError::Inconsistent);
            }
            let len: f32 = look.axis.iter().map(|v| v * v).sum::<f32>().sqrt();
            if (len - 1.0).abs() > UNIT_TOLERANCE
                || look.max_angle <= 0.0
                || look.max_angle > std::f32::consts::PI
            {
                return Err(FormatError::Geometry);
            }
        }
        Ok(())
    }

    /// Parses and validates a graph.
    ///
    /// # Errors
    /// [`FormatError`] describing the first problem found.
    pub fn parse(bytes: &[u8]) -> Result<GraphAsset, FormatError> {
        let mut r = Reader::new(bytes);
        if r.array::<4>()? != MAGIC {
            return Err(FormatError::Magic);
        }
        let version = r.u16()?;
        if version != 1 {
            return Err(FormatError::Version(version));
        }
        let flags = r.u16()?;
        if flags & !FLAG_LOOK_AT != 0 {
            return Err(FormatError::Flags(u32::from(flags)));
        }
        let bone_count = r.u32()?;
        let clip_count = r.u32()?;
        let parameter_count = r.u32()?;
        let node_count = r.u32()?;
        let layer_count = r.u32()?;
        let chain_count = r.u32()?;
        let counts_ok = (1..=MAX_BONES).contains(&bone_count)
            && clip_count <= MAX_CLIPS
            && parameter_count <= MAX_PARAMETERS
            && (1..=MAX_NODES).contains(&node_count)
            && (1..=MAX_LAYERS).contains(&layer_count)
            && chain_count <= MAX_FOOT_CHAINS;
        if !counts_ok {
            return Err(FormatError::Dimensions);
        }
        if r.u32()? != 0 {
            return Err(FormatError::Reserved);
        }
        let mut clips = Vec::with_capacity(clip_count as usize);
        for _ in 0..clip_count {
            clips.push(ContentHash::from_bytes(r.array::<32>()?));
        }
        let mut parameters = Vec::with_capacity(parameter_count as usize);
        for _ in 0..parameter_count {
            parameters.push(read_parameter(&mut r)?);
        }
        let mut nodes = Vec::with_capacity(node_count as usize);
        for _ in 0..node_count {
            nodes.push(read_node(&mut r)?);
        }
        let mut layers = Vec::with_capacity(layer_count as usize);
        for _ in 0..layer_count {
            layers.push(read_layer(&mut r, bone_count)?);
        }
        let mut foot_chains = Vec::with_capacity(chain_count as usize);
        for _ in 0..chain_count {
            foot_chains.push(read_chain(&mut r)?);
        }
        let look_at = if flags & FLAG_LOOK_AT != 0 {
            Some(read_look_at(&mut r)?)
        } else {
            None
        };
        r.finish()?;
        let graph = GraphAsset {
            bone_count,
            clips,
            parameters,
            nodes,
            layers,
            foot_chains,
            look_at,
        };
        graph.validate()?;
        Ok(graph)
    }

    /// Reference encoder (the exact inverse of [`GraphAsset::parse`]).
    pub fn encode(&self) -> Vec<u8> {
        let mut w = Writer::new();
        w.bytes(&MAGIC);
        w.u16(1);
        w.u16(if self.look_at.is_some() { FLAG_LOOK_AT } else { 0 });
        w.u32(self.bone_count);
        w.count(self.clips.len());
        w.count(self.parameters.len());
        w.count(self.nodes.len());
        w.count(self.layers.len());
        w.count(self.foot_chains.len());
        w.u32(0);
        for clip in &self.clips {
            w.bytes(clip.as_bytes());
        }
        for p in &self.parameters {
            w.u32(p.name_hash);
            w.u8(p.kind.to_u8());
            w.bytes(&[0; 3]);
            w.f32(p.default);
        }
        for node in &self.nodes {
            write_node(&mut w, node);
        }
        for layer in &self.layers {
            w.u32(layer.node);
            w.f32(layer.weight);
            w.u8(match layer.mode {
                LayerMode::Override => 0,
                LayerMode::Additive => 1,
            });
            w.u8(0);
            w.u16(u16::try_from(layer.mask.len()).unwrap_or(u16::MAX));
            w.u32(layer.reference_clip.unwrap_or(NO_CLIP));
            for b in &layer.mask {
                w.u16(*b);
            }
        }
        for chain in &self.foot_chains {
            w.u16(chain.root);
            w.u16(chain.mid);
            w.u16(chain.tip);
            w.u16(0);
            w.vec3(chain.pole);
        }
        if let Some(look) = &self.look_at {
            w.u16(look.head);
            w.u16(0);
            w.vec3(look.axis);
            w.f32(look.max_angle);
        }
        w.into_bytes()
    }
}

fn reserved(r: &mut Reader<'_>, n: usize) -> Result<(), FormatError> {
    if r.slice(n)?.iter().any(|b| *b != 0) {
        return Err(FormatError::Reserved);
    }
    Ok(())
}

fn read_parameter(r: &mut Reader<'_>) -> Result<ParameterDef, FormatError> {
    let name_hash = r.u32()?;
    let kind = ParameterKind::from_u8(r.u8()?)?;
    reserved(r, 3)?;
    let default = r.f32()?;
    Ok(ParameterDef {
        name_hash,
        kind,
        default,
    })
}

fn read_node(r: &mut Reader<'_>) -> Result<NodeDef, FormatError> {
    let kind = r.u8()?;
    reserved(r, 3)?;
    match kind {
        0 => Ok(NodeDef::Clip {
            clip: r.u32()?,
            speed: r.f32()?,
        }),
        1 => {
            let parameter = r.u32()?;
            let count = r.u32()?;
            if count == 0 || count > MAX_BLEND_CHILDREN {
                return Err(FormatError::Dimensions);
            }
            let mut children = Vec::with_capacity(count as usize);
            for _ in 0..count {
                children.push(BlendChildDef {
                    node: r.u32()?,
                    threshold: r.f32()?,
                });
            }
            Ok(NodeDef::Blend1D { parameter, children })
        }
        2 => Ok(NodeDef::StateMachine(read_state_machine(r)?)),
        _ => Err(FormatError::Encoding(u32::from(kind))),
    }
}

fn read_state_machine(r: &mut Reader<'_>) -> Result<StateMachineDef, FormatError> {
    let state_count = r.u32()?;
    let entry = r.u32()?;
    let transition_count = r.u32()?;
    if state_count == 0 || state_count > MAX_STATES || transition_count > MAX_TRANSITIONS {
        return Err(FormatError::Dimensions);
    }
    let mut states = Vec::with_capacity(state_count as usize);
    for _ in 0..state_count {
        states.push(r.u32()?);
    }
    let mut transitions = Vec::with_capacity(transition_count as usize);
    for _ in 0..transition_count {
        let from = r.u32()?;
        let to = r.u32()?;
        let crossfade = r.f32()?;
        let exit_time = r.f32()?;
        let flags = r.u16()?;
        if flags & !TRANSITION_EXIT_TIME != 0 {
            return Err(FormatError::Flags(u32::from(flags)));
        }
        if flags & TRANSITION_EXIT_TIME == 0 && exit_time != 0.0 {
            return Err(FormatError::Reserved);
        }
        let condition_count = r.u16()?;
        if condition_count > MAX_CONDITIONS {
            return Err(FormatError::Dimensions);
        }
        let mut conditions = Vec::with_capacity(usize::from(condition_count));
        for _ in 0..condition_count {
            let parameter = r.u32()?;
            let op = CompareOp::from_u8(r.u8()?)?;
            reserved(r, 3)?;
            let value = r.f32()?;
            conditions.push(ConditionDef { parameter, op, value });
        }
        transitions.push(TransitionDef {
            from: (from != ANY_STATE).then_some(from),
            to,
            crossfade,
            exit_time: (flags & TRANSITION_EXIT_TIME != 0).then_some(exit_time),
            conditions,
        });
    }
    Ok(StateMachineDef {
        states,
        entry,
        transitions,
    })
}

fn read_layer(r: &mut Reader<'_>, bone_count: u32) -> Result<LayerDef, FormatError> {
    let node = r.u32()?;
    let weight = r.f32()?;
    let mode = match r.u8()? {
        0 => LayerMode::Override,
        1 => LayerMode::Additive,
        m => return Err(FormatError::Encoding(u32::from(m))),
    };
    reserved(r, 1)?;
    let mask_count = r.u16()?;
    if u32::from(mask_count) > bone_count {
        return Err(FormatError::Dimensions);
    }
    let reference = r.u32()?;
    let mut mask = Vec::with_capacity(usize::from(mask_count));
    for _ in 0..mask_count {
        mask.push(r.u16()?);
    }
    Ok(LayerDef {
        node,
        weight,
        mode,
        reference_clip: (reference != NO_CLIP).then_some(reference),
        mask,
    })
}

fn read_chain(r: &mut Reader<'_>) -> Result<TwoBoneChainDef, FormatError> {
    let root = r.u16()?;
    let mid = r.u16()?;
    let tip = r.u16()?;
    reserved(r, 2)?;
    Ok(TwoBoneChainDef {
        root,
        mid,
        tip,
        pole: r.vec3()?,
    })
}

fn read_look_at(r: &mut Reader<'_>) -> Result<LookAtDef, FormatError> {
    let head = r.u16()?;
    reserved(r, 2)?;
    Ok(LookAtDef {
        head,
        axis: r.vec3()?,
        max_angle: r.f32()?,
    })
}

fn write_node(w: &mut Writer, node: &NodeDef) {
    match node {
        NodeDef::Clip { clip, speed } => {
            w.bytes(&[0, 0, 0, 0]);
            w.u32(*clip);
            w.f32(*speed);
        }
        NodeDef::Blend1D { parameter, children } => {
            w.bytes(&[1, 0, 0, 0]);
            w.u32(*parameter);
            w.count(children.len());
            for c in children {
                w.u32(c.node);
                w.f32(c.threshold);
            }
        }
        NodeDef::StateMachine(sm) => {
            w.bytes(&[2, 0, 0, 0]);
            w.count(sm.states.len());
            w.u32(sm.entry);
            w.count(sm.transitions.len());
            for s in &sm.states {
                w.u32(*s);
            }
            for t in &sm.transitions {
                w.u32(t.from.unwrap_or(ANY_STATE));
                w.u32(t.to);
                w.f32(t.crossfade);
                w.f32(t.exit_time.unwrap_or(0.0));
                w.u16(if t.exit_time.is_some() {
                    TRANSITION_EXIT_TIME
                } else {
                    0
                });
                w.u16(u16::try_from(t.conditions.len()).unwrap_or(u16::MAX));
                for c in &t.conditions {
                    w.u32(c.parameter);
                    w.u8(c.op.to_u8());
                    w.bytes(&[0; 3]);
                    w.f32(c.value);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests;

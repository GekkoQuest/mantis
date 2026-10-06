//! A playing animation graph: parameters, playback state, IK targets, and every buffer an
//! update writes, all allocated at construction. [`AnimInstance::update`] allocates
//! nothing.

use std::sync::Arc;

use glam::Vec3;
use mantis_formats::anim_graph::{LayerMode, ParameterKind};

use crate::blend::{apply_additive, apply_override};
use crate::error::AnimError;
use crate::eval::{self, NodeState};
use crate::graph::{AnimGraph, ParamId};
use crate::ik::{solve_look_at, solve_two_bone};
use crate::root_motion::{RootDelta, RootMotion};
use crate::skeleton::{PaletteEntry, Pose};
use crate::transform::Transform;

/// An IK goal in model space with a blend weight in `[0, 1]`.
#[derive(Clone, Copy, PartialEq, Debug)]
pub struct IkTarget {
    /// Target position in model space.
    pub position: Vec3,
    /// How much of the IK correction to apply.
    pub weight: f32,
}

/// One character's animation state and output.
#[derive(Clone, Debug)]
pub struct AnimInstance {
    graph: Arc<AnimGraph>,
    params: Vec<f32>,
    states: Vec<NodeState>,
    scratch: Vec<Pose>,
    layer_pose: Pose,
    pose: Pose,
    model: Vec<Transform>,
    palette: Vec<PaletteEntry>,
    layer_weights: Vec<f32>,
    foot_targets: Vec<Option<IkTarget>>,
    look_target: Option<IkTarget>,
    root_motion: RootMotion,
}

impl AnimInstance {
    /// A new instance at the graph's initial state, with parameters at their defaults,
    /// layer weights from the asset, and no IK targets. Its pose is the bind pose until
    /// the first [`AnimInstance::update`].
    pub fn new(graph: Arc<AnimGraph>) -> Self {
        let skeleton = Arc::clone(graph.skeleton());
        let bones = skeleton.bone_count();
        let mut model = vec![Transform::IDENTITY; bones];
        skeleton.model_space(skeleton.bind_pose(), &mut model);
        let mut palette = vec![[[0.0; 4]; 3]; bones];
        skeleton.skinning_palette(&model, &mut palette);
        Self {
            params: graph.parameter_defaults().to_vec(),
            states: eval::initial_states(&graph),
            scratch: (0..=graph.depth()).map(|_| Pose::bind(&skeleton)).collect(),
            layer_pose: Pose::bind(&skeleton),
            pose: Pose::bind(&skeleton),
            model,
            palette,
            layer_weights: graph.layers.iter().map(|l| l.weight).collect(),
            foot_targets: vec![None; graph.foot_chains().len()],
            look_target: None,
            root_motion: RootMotion::IDENTITY,
            graph,
        }
    }

    /// The graph.
    pub fn graph(&self) -> &Arc<AnimGraph> {
        &self.graph
    }

    fn parameter_slot(&mut self, id: ParamId, kind: ParameterKind) -> Result<&mut f32, AnimError> {
        match self.graph.parameter_kind(id) {
            None => Err(AnimError::UnknownParameter),
            Some(k) if k != kind => Err(AnimError::ParameterKind),
            Some(_) => self.params.get_mut(id.0).ok_or(AnimError::UnknownParameter),
        }
    }

    /// Sets a float parameter.
    ///
    /// # Errors
    /// [`AnimError::UnknownParameter`], [`AnimError::ParameterKind`] when it is not a
    /// float, [`AnimError::NonFinite`] for a non-finite value.
    pub fn set_float(&mut self, id: ParamId, value: f32) -> Result<(), AnimError> {
        if !value.is_finite() {
            return Err(AnimError::NonFinite);
        }
        *self.parameter_slot(id, ParameterKind::Float)? = value;
        Ok(())
    }

    /// Sets a bool parameter.
    ///
    /// # Errors
    /// [`AnimError::UnknownParameter`], or [`AnimError::ParameterKind`] when it is not a
    /// bool.
    pub fn set_bool(&mut self, id: ParamId, value: bool) -> Result<(), AnimError> {
        *self.parameter_slot(id, ParameterKind::Bool)? = if value { 1.0 } else { 0.0 };
        Ok(())
    }

    /// Sets a trigger. It stays set until a transition that tests it fires (which
    /// consumes it) or [`AnimInstance::reset_trigger`].
    ///
    /// # Errors
    /// [`AnimError::UnknownParameter`], or [`AnimError::ParameterKind`] when it is not a
    /// trigger.
    pub fn set_trigger(&mut self, id: ParamId) -> Result<(), AnimError> {
        *self.parameter_slot(id, ParameterKind::Trigger)? = 1.0;
        Ok(())
    }

    /// Clears a trigger.
    ///
    /// # Errors
    /// [`AnimError::UnknownParameter`], or [`AnimError::ParameterKind`] when it is not a
    /// trigger.
    pub fn reset_trigger(&mut self, id: ParamId) -> Result<(), AnimError> {
        *self.parameter_slot(id, ParameterKind::Trigger)? = 0.0;
        Ok(())
    }

    /// A parameter's current value (bools and triggers read as 0 or 1).
    pub fn parameter_value(&self, id: ParamId) -> Option<f32> {
        self.params.get(id.0).copied()
    }

    /// Overrides a layer's weight (clamped to `[0, 1]`).
    ///
    /// # Errors
    /// [`AnimError::OutOfRange`] for a layer index out of range, [`AnimError::NonFinite`]
    /// for a non-finite weight.
    pub fn set_layer_weight(&mut self, layer: usize, weight: f32) -> Result<(), AnimError> {
        if !weight.is_finite() {
            return Err(AnimError::NonFinite);
        }
        *self.layer_weights.get_mut(layer).ok_or(AnimError::OutOfRange)? = weight.clamp(0.0, 1.0);
        Ok(())
    }

    /// Sets or clears a foot chain's IK target.
    ///
    /// # Errors
    /// [`AnimError::OutOfRange`] for a chain index out of range, [`AnimError::NonFinite`]
    /// for a non-finite target.
    pub fn set_foot_target(&mut self, chain: usize, target: Option<IkTarget>) -> Result<(), AnimError> {
        check_target(target)?;
        *self.foot_targets.get_mut(chain).ok_or(AnimError::OutOfRange)? = target;
        Ok(())
    }

    /// Sets or clears the look-at target.
    ///
    /// # Errors
    /// [`AnimError::NoLookAt`] when the graph has no look-at chain,
    /// [`AnimError::NonFinite`] for a non-finite target.
    pub fn set_look_at_target(&mut self, target: Option<IkTarget>) -> Result<(), AnimError> {
        check_target(target)?;
        if self.graph.look_at().is_none() {
            return Err(AnimError::NoLookAt);
        }
        self.look_target = target;
        Ok(())
    }

    /// The current state index of the state machine at `node`, or `None` when `node` is
    /// not a state machine.
    pub fn current_state(&self, node: usize) -> Option<usize> {
        match self.states.get(node) {
            Some(NodeState::Machine { current, .. }) => Some(*current),
            _ => None,
        }
    }

    /// The state a machine node is crossfading into, if any.
    pub fn next_state(&self, node: usize) -> Option<usize> {
        match self.states.get(node) {
            Some(NodeState::Machine { next, .. }) => *next,
            _ => None,
        }
    }

    /// The local playback time of the clip node at `node`.
    pub fn clip_time(&self, node: usize) -> Option<f32> {
        match self.states.get(node) {
            Some(NodeState::Clip { time, .. }) => Some(*time),
            _ => None,
        }
    }

    /// Advances by `dt` seconds (non-finite or negative counts as 0) and recomputes the
    /// pose, model-space transforms, and skinning palette. Returns this update's root
    /// motion (from layer 0). Allocates nothing.
    pub fn update(&mut self, dt: f32) -> RootMotion {
        let dt = if dt.is_finite() && dt > 0.0 { dt } else { 0.0 };
        let graph: &AnimGraph = &self.graph;
        for layer in &graph.layers {
            eval::advance(graph, &mut self.params, &mut self.states, layer.node, dt);
        }
        let skeleton = graph.skeleton();
        self.pose.copy_from(skeleton.bind_pose());
        let mut root = RootDelta::ZERO;
        for (i, (layer, weight)) in graph.layers.iter().zip(&self.layer_weights).enumerate() {
            if *weight <= 0.0 {
                continue;
            }
            let delta = eval::sample(
                graph,
                &self.params,
                &self.states,
                layer.node,
                self.layer_pose.as_mut_slice(),
                &mut self.scratch,
            );
            if i == 0 {
                root = delta;
            }
            let (base, over) = (self.pose.as_mut_slice(), self.layer_pose.as_slice());
            match layer.mode {
                LayerMode::Override => apply_override(base, over, *weight, &layer.mask),
                LayerMode::Additive => {
                    apply_additive(base, over, &layer.reference_inverse, *weight, &layer.mask);
                }
            }
        }
        self.root_motion = root.to_motion();
        let local = self.pose.as_mut_slice();
        skeleton.model_space(local, &mut self.model);
        for (chain, target) in graph.foot_chains().iter().zip(&self.foot_targets) {
            if let Some(t) = target
                && solve_two_bone(skeleton, local, &self.model, chain, t.position, t.weight)
            {
                skeleton.model_space_from(chain.root, local, &mut self.model);
            }
        }
        if let (Some(look), Some(t)) = (graph.look_at(), self.look_target)
            && solve_look_at(skeleton, local, &self.model, look, t.position, t.weight)
        {
            skeleton.model_space_from(look.head, local, &mut self.model);
        }
        skeleton.skinning_palette(&self.model, &mut self.palette);
        self.root_motion
    }

    /// The local pose after the last update (root motion removed, IK applied).
    pub fn pose(&self) -> &[Transform] {
        self.pose.as_slice()
    }

    /// Model-space transforms after the last update.
    pub fn model(&self) -> &[Transform] {
        &self.model
    }

    /// The skinning palette after the last update, one 3x4 row-major matrix per bone.
    pub fn palette(&self) -> &[PaletteEntry] {
        &self.palette
    }

    /// The skinning palette as bytes for upload (48 bytes per bone).
    pub fn palette_bytes(&self) -> &[u8] {
        bytemuck::cast_slice(&self.palette)
    }

    /// The root motion of the last update.
    pub fn root_motion(&self) -> RootMotion {
        self.root_motion
    }
}

fn check_target(target: Option<IkTarget>) -> Result<(), AnimError> {
    match target {
        Some(t) if !t.position.is_finite() || !t.weight.is_finite() => Err(AnimError::NonFinite),
        _ => Ok(()),
    }
}

#[cfg(test)]
mod tests;

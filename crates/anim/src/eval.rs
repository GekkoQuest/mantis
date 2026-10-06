//! Graph evaluation: per-node playback state, advancing time (state-machine transitions,
//! crossfades, exit times, triggers), and sampling a node tree into a pose with its
//! root-motion delta. Recursion depth is bounded by the format's maximum depth, and
//! nothing here allocates.
//!
//! Order within one update: each state machine first checks its transitions against the
//! parameters and the current state's normalized time from the previous update, then
//! every active node advances by the step. A transition that fires this update therefore
//! contributes its destination's first step of motion. Rules:
//!
//! - Transitions are checked in order and the first whose conditions all hold (and whose
//!   exit time, if any, has been reached) fires. A from-any transition never fires into
//!   the state that is already current.
//! - A firing transition consumes (clears) every trigger its conditions test. Triggers
//!   not tested by a firing transition stay set until consumed or reset.
//! - While a crossfade runs no other transition of that machine fires; the source keeps
//!   playing and fades out linearly over the crossfade time.
//! - A transition into the current state restarts it at once (no crossfade with itself).
//! - Entering a state restarts its whole subtree (clips from their start, nested
//!   machines from their entry state).
//! - Blend children all advance every update, unsynchronized; only the two children
//!   around the parameter value are sampled.
//! - Normalized time (for exit times) is the clip's played length over its duration,
//!   accumulating across loops (1.5 is halfway through the second loop) and stopping at
//!   1 for a clamped clip; a blend reports its dominant child's, a machine its current
//!   state's.

use mantis_formats::anim_graph::CompareOp;

use crate::blend::lerp_into;
use crate::graph::{AnimGraph, Node, Transition};
use crate::root_motion::{RootDelta, strip_root_motion};
use crate::skeleton::Pose;
use crate::transform::Transform;

/// Playback state of one node.
#[derive(Clone, Copy, PartialEq, Debug)]
pub(crate) enum NodeState {
    Clip {
        /// Local clip time.
        time: f32,
        /// Played length over duration, accumulating across loops.
        normalized: f32,
        /// Root motion of the last step.
        delta: RootDelta,
    },
    Blend,
    Machine {
        current: usize,
        next: Option<usize>,
        fade_elapsed: f32,
        fade_duration: f32,
    },
}

fn clip_start(graph: &AnimGraph, clip: usize, speed: f32) -> f32 {
    match graph.clip(clip) {
        Some(c) if speed < 0.0 && !c.is_looping() => c.duration(),
        _ => 0.0,
    }
}

/// The initial state of every node (machines in their entry state, clips at their start).
pub(crate) fn initial_states(graph: &AnimGraph) -> Vec<NodeState> {
    graph
        .nodes
        .iter()
        .map(|n| match n {
            Node::Clip { clip, speed } => NodeState::Clip {
                time: clip_start(graph, *clip, *speed),
                normalized: 0.0,
                delta: RootDelta::ZERO,
            },
            Node::Blend1D { .. } => NodeState::Blend,
            Node::Machine { entry, .. } => NodeState::Machine {
                current: *entry,
                next: None,
                fade_elapsed: 0.0,
                fade_duration: 0.0,
            },
        })
        .collect()
}

/// Restarts `node`'s subtree.
pub(crate) fn reset(graph: &AnimGraph, states: &mut [NodeState], node: usize) {
    match graph.nodes.get(node) {
        Some(Node::Clip { clip, speed }) => {
            if let Some(s) = states.get_mut(node) {
                *s = NodeState::Clip {
                    time: clip_start(graph, *clip, *speed),
                    normalized: 0.0,
                    delta: RootDelta::ZERO,
                };
            }
        }
        Some(Node::Blend1D { children, .. }) => {
            for (child, _) in children {
                reset(graph, states, *child);
            }
        }
        Some(Node::Machine {
            states: machine_states,
            entry,
            ..
        }) => {
            if let Some(s) = states.get_mut(node) {
                *s = NodeState::Machine {
                    current: *entry,
                    next: None,
                    fade_elapsed: 0.0,
                    fade_duration: 0.0,
                };
            }
            if let Some(n) = machine_states.get(*entry) {
                reset(graph, states, *n);
            }
        }
        None => {}
    }
}

/// `(a, b, f)`: the blend is child `a` moved toward child `b` by `f`.
pub(crate) fn blend_weights(children: &[(usize, f32)], value: f32) -> (usize, usize, f32) {
    let (Some(&(first, t_first)), Some(&(last, t_last))) = (children.first(), children.last()) else {
        return (0, 0, 0.0);
    };
    if value.is_nan() || value <= t_first {
        return (first, first, 0.0);
    }
    if value >= t_last {
        return (last, last, 0.0);
    }
    for pair in children.windows(2) {
        if let [(a, ta), (b, tb)] = pair
            && value >= *ta
            && value < *tb
        {
            return (*a, *b, (value - ta) / (tb - ta));
        }
    }
    (last, last, 0.0)
}

fn parameter(params: &[f32], index: usize) -> f32 {
    params.get(index).copied().unwrap_or(0.0)
}

/// Normalized time of `node` (see the module docs).
pub(crate) fn normalized_time(graph: &AnimGraph, params: &[f32], states: &[NodeState], node: usize) -> f32 {
    match (graph.nodes.get(node), states.get(node)) {
        (Some(Node::Clip { .. }), Some(NodeState::Clip { normalized, .. })) => *normalized,
        (
            Some(Node::Blend1D {
                parameter: p,
                children,
            }),
            _,
        ) => {
            let (a, b, f) = blend_weights(children, parameter(params, *p));
            normalized_time(graph, params, states, if f < 0.5 { a } else { b })
        }
        (
            Some(Node::Machine {
                states: machine_states,
                ..
            }),
            Some(NodeState::Machine { current, .. }),
        ) => machine_states
            .get(*current)
            .map_or(0.0, |n| normalized_time(graph, params, states, *n)),
        _ => 0.0,
    }
}

/// Advances `node`'s subtree by `dt` seconds.
pub(crate) fn advance(graph: &AnimGraph, params: &mut [f32], states: &mut [NodeState], node: usize, dt: f32) {
    match graph.nodes.get(node) {
        Some(Node::Clip { clip, speed }) => advance_clip(graph, states, node, *clip, *speed, dt),
        Some(Node::Blend1D { children, .. }) => {
            for (child, _) in children {
                advance(graph, params, states, *child, dt);
            }
        }
        Some(Node::Machine {
            states: machine_states,
            transitions,
            ..
        }) => advance_machine(graph, params, states, node, machine_states, transitions, dt),
        None => {}
    }
}

fn advance_clip(graph: &AnimGraph, states: &mut [NodeState], node: usize, clip: usize, speed: f32, dt: f32) {
    let (
        Some(c),
        Some(NodeState::Clip {
            time,
            normalized,
            delta,
        }),
    ) = (graph.clip(clip), states.get_mut(node))
    else {
        return;
    };
    let step = dt * speed;
    let (new_time, wraps) = c.step(*time, step);
    let bind_root = graph
        .skeleton()
        .bind_pose()
        .first()
        .copied()
        .unwrap_or(Transform::IDENTITY);
    *delta = c.root_delta_wrapped(bind_root, *time, wraps, new_time);
    *time = new_time;
    let played = *normalized + step.abs() / c.duration();
    *normalized = if c.is_looping() { played } else { played.min(1.0) };
}

fn advance_machine(
    graph: &AnimGraph,
    params: &mut [f32],
    states: &mut [NodeState],
    node: usize,
    machine_states: &[usize],
    transitions: &[Transition],
    dt: f32,
) {
    let Some(&NodeState::Machine { current, next, .. }) = states.get(node) else {
        return;
    };
    if next.is_none() {
        try_transition(graph, params, states, node, machine_states, transitions, current);
    }
    let Some(&NodeState::Machine {
        current,
        next,
        fade_elapsed,
        fade_duration,
    }) = states.get(node)
    else {
        return;
    };
    if let Some(n) = machine_states.get(current) {
        advance(graph, params, states, *n, dt);
    }
    if let Some(target) = next {
        if let Some(n) = machine_states.get(target) {
            advance(graph, params, states, *n, dt);
        }
        let elapsed = fade_elapsed + dt;
        if let Some(NodeState::Machine {
            current,
            next,
            fade_elapsed,
            ..
        }) = states.get_mut(node)
        {
            if elapsed >= fade_duration {
                *current = target;
                *next = None;
                *fade_elapsed = 0.0;
            } else {
                *fade_elapsed = elapsed;
            }
        }
    }
}

fn try_transition(
    graph: &AnimGraph,
    params: &mut [f32],
    states: &mut [NodeState],
    node: usize,
    machine_states: &[usize],
    transitions: &[Transition],
    current: usize,
) {
    let current_node = machine_states.get(current).copied();
    let fired = transitions.iter().find(|t| {
        let from_ok = match t.from {
            Some(f) => f == current,
            None => t.to != current,
        };
        let exit_ok = t
            .exit_time
            .is_none_or(|e| current_node.is_some_and(|n| normalized_time(graph, params, states, n) >= e));
        from_ok
            && exit_ok
            && t.conditions
                .iter()
                .all(|c| c.op.holds(parameter(params, c.parameter), c.value))
    });
    let Some(t) = fired else {
        return;
    };
    for c in t.conditions.iter().filter(|c| c.op == CompareOp::Triggered) {
        if let Some(p) = params.get_mut(c.parameter) {
            *p = 0.0;
        }
    }
    let immediate = t.to == current || t.crossfade <= 0.0;
    if let Some(n) = machine_states.get(t.to) {
        reset(graph, states, *n);
    }
    if let Some(NodeState::Machine {
        current,
        next,
        fade_elapsed,
        fade_duration,
    }) = states.get_mut(node)
    {
        *fade_elapsed = 0.0;
        if immediate {
            *current = t.to;
            *next = None;
            *fade_duration = 0.0;
        } else {
            *next = Some(t.to);
            *fade_duration = t.crossfade;
        }
    }
}

/// Samples `node`'s subtree into `out` and returns its root-motion delta. `scratch`
/// provides temporary poses (one per blend level below this node).
pub(crate) fn sample(
    graph: &AnimGraph,
    params: &[f32],
    states: &[NodeState],
    node: usize,
    out: &mut [Transform],
    scratch: &mut [Pose],
) -> RootDelta {
    match (graph.nodes.get(node), states.get(node)) {
        (Some(Node::Clip { clip, .. }), Some(NodeState::Clip { time, delta, .. })) => {
            let Some(c) = graph.clip(*clip) else {
                return RootDelta::ZERO;
            };
            c.sample(graph.skeleton().bind_pose(), *time, out);
            if c.has_root_motion()
                && let Some(root) = out.first_mut()
            {
                strip_root_motion(root);
            }
            *delta
        }
        (
            Some(Node::Blend1D {
                parameter: p,
                children,
            }),
            _,
        ) => {
            let (a, b, f) = blend_weights(children, parameter(params, *p));
            blend_two(graph, params, states, (a, b, f), out, scratch)
        }
        (
            Some(Node::Machine {
                states: machine_states,
                ..
            }),
            Some(NodeState::Machine {
                current,
                next,
                fade_elapsed,
                fade_duration,
            }),
        ) => {
            let a = machine_states.get(*current).copied().unwrap_or(usize::MAX);
            match next.and_then(|n| machine_states.get(n).copied()) {
                Some(b) if *fade_duration > 0.0 => {
                    let f = (fade_elapsed / fade_duration).clamp(0.0, 1.0);
                    blend_two(graph, params, states, (a, b, f), out, scratch)
                }
                _ => sample(graph, params, states, a, out, scratch),
            }
        }
        _ => {
            for (o, b) in out.iter_mut().zip(graph.skeleton().bind_pose()) {
                *o = *b;
            }
            RootDelta::ZERO
        }
    }
}

fn blend_two(
    graph: &AnimGraph,
    params: &[f32],
    states: &[NodeState],
    (a, b, f): (usize, usize, f32),
    out: &mut [Transform],
    scratch: &mut [Pose],
) -> RootDelta {
    if a == b || f <= 0.0 {
        return sample(graph, params, states, a, out, scratch);
    }
    if f >= 1.0 {
        return sample(graph, params, states, b, out, scratch);
    }
    let first = sample(graph, params, states, a, out, scratch);
    let Some((tmp, rest)) = scratch.split_first_mut() else {
        return first;
    };
    let second = sample(graph, params, states, b, tmp.as_mut_slice(), rest);
    lerp_into(out, tmp.as_slice(), f);
    first.lerp(second, f)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn blend_weights_pick_neighbors() {
        let children = [(10, 0.0), (11, 1.0), (12, 3.0)];
        assert_eq!(blend_weights(&children, -1.0), (10, 10, 0.0));
        assert_eq!(blend_weights(&children, 0.0), (10, 10, 0.0));
        assert_eq!(blend_weights(&children, 0.5), (10, 11, 0.5));
        assert_eq!(blend_weights(&children, 2.0), (11, 12, 0.5));
        assert_eq!(blend_weights(&children, 3.0), (12, 12, 0.0));
        assert_eq!(blend_weights(&children, 9.0), (12, 12, 0.0));
        assert_eq!(blend_weights(&children, f32::NAN), (10, 10, 0.0));
        assert_eq!(blend_weights(&[(4, 1.0)], 5.0), (4, 4, 0.0));
        assert_eq!(blend_weights(&[], 5.0), (0, 0, 0.0));
    }
}

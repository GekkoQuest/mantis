//! Per-cell evaluation of running gameplay graph instances.

use core::fmt;

use super::{
    ActionId, ActionParams, CompiledGraph, GraphCatalog, GraphError, GraphId, GraphInstanceId, MarkerId,
    MarkerKind, MarkerSpec, NodeKey, NodeKind, Target, TimelineMarker,
};
use crate::ecs::{EntityId, Resource, Saved};
use crate::hash::{StableHasher, StateHash};
use crate::mem::BoundedVec;
use crate::rng::{Rng, Salt, Seed};
use crate::time::Tick;
use crate::wire::{DecodeError, Decoder, Encoder, Wire};

/// Repeat counters per instance.
pub const MAX_COUNTERS: usize = 4;

/// Nodes one instance may execute in one evaluation before it is stopped
/// (fail closed). Validation already rules out zero-time cycles; this bounds
/// pathological straight-line graphs.
pub const STEP_BUDGET: u32 = 256;

/// One action invocation, handed to the [`ActionHandler`].
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct ActionCall {
    /// The action.
    pub action: ActionId,
    /// Its parameters.
    pub params: ActionParams,
    /// The entity running the graph.
    pub source: EntityId,
    /// The entity the action applies to (source or the instance target).
    pub target: EntityId,
    /// The instance.
    pub instance: GraphInstanceId,
    /// The graph and node, for diagnostics.
    pub node: MarkerId,
    /// The current tick.
    pub tick: Tick,
    /// The instance's repeat counters.
    pub counters: [u16; MAX_COUNTERS],
}

/// An action failed; the instance that called it is stopped.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct ActionError(pub &'static str);

impl fmt::Display for ActionError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "action failed: {}", self.0)
    }
}

impl std::error::Error for ActionError {}

/// Applies actions: the bridge from graphs to module rules.
pub trait ActionHandler {
    /// Applies one action.
    ///
    /// # Errors
    /// [`ActionError`]; the calling instance stops (fail closed).
    fn apply(&mut self, call: &ActionCall) -> Result<(), ActionError>;
}

/// What one evaluation did.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct EvalReport {
    /// Instances that ran at least one node.
    pub evaluated: u32,
    /// Instances that reached the end of their graph.
    pub finished: u32,
    /// Instances stopped by an action error, a missing graph, or the step budget.
    pub failed: u32,
    /// Markers that did not fit in the buffer this tick.
    pub markers_dropped: u32,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
struct Instance {
    id: GraphInstanceId,
    graph: GraphId,
    source: EntityId,
    target: Option<EntityId>,
    started: Tick,
    cursor: Option<u16>,
    wake_at: Tick,
    counters: [u16; MAX_COUNTERS],
}

impl StateHash for Instance {
    fn state_hash(&self, h: &mut StableHasher) {
        self.id.state_hash(h);
        h.write_u32(self.graph.0);
        self.source.state_hash(h);
        self.target.state_hash(h);
        self.started.state_hash(h);
        self.cursor.state_hash(h);
        self.wake_at.state_hash(h);
        self.counters.state_hash(h);
    }
}

/// Running graph instances of one cell, plus this tick's marker buffer.
/// Pre-sized; never grows. A resource in the cell's world.
#[derive(Debug)]
pub struct GraphRuntime {
    instances: BoundedVec<Instance>,
    markers: BoundedVec<TimelineMarker>,
    next_id: u64,
}

impl Resource for GraphRuntime {
    const NAME: &'static str = "core.graph_runtime";

    /// The running instances and the id counter (markers are per-tick output).
    fn save(&self, e: &mut Encoder<'_>) -> Saved {
        e.u64(self.next_id);
        e.u32(u32::try_from(self.instances.len()).unwrap_or(u32::MAX));
        for i in self.instances.iter() {
            e.u64(i.id.0);
            e.u32(i.graph.0);
            i.source.encode(e);
            e.bool(i.target.is_some());
            e.u64(i.target.map_or(0, EntityId::to_bits));
            e.u64(i.started.0);
            e.bool(i.cursor.is_some());
            e.u16(i.cursor.unwrap_or(0));
            e.u64(i.wake_at.0);
            for c in i.counters {
                e.u16(c);
            }
        }
        Saved::Written
    }

    fn load(&mut self, d: &mut Decoder<'_>) -> Result<(), DecodeError> {
        self.next_id = d.u64()?;
        self.instances.clear();
        self.markers.clear();
        let n = d.u32()?;
        for _ in 0..n {
            let id = GraphInstanceId(d.u64()?);
            let graph = GraphId(d.u32()?);
            let source = EntityId::decode(d)?;
            let has_target = d.bool()?;
            let target = EntityId::from_bits(d.u64()?);
            let started = Tick(d.u64()?);
            let has_cursor = d.bool()?;
            let cursor = d.u16()?;
            let wake_at = Tick(d.u64()?);
            let mut counters = [0u16; MAX_COUNTERS];
            for c in &mut counters {
                *c = d.u16()?;
            }
            let inst = Instance {
                id,
                graph,
                source,
                target: has_target.then_some(target),
                started,
                cursor: has_cursor.then_some(cursor),
                wake_at,
                counters,
            };
            self.instances
                .push(inst)
                .map_err(|_| DecodeError::Invalid("graph instances over capacity"))?;
        }
        Ok(())
    }
}

/// Instances (with their cursors, timers, and counters) and the id counter
/// are simulation state. The marker buffer is per-tick output, rebuilt by
/// every evaluation, and is not hashed.
impl StateHash for GraphRuntime {
    fn state_hash(&self, h: &mut StableHasher) {
        h.write_u64(self.next_id);
        self.instances.state_hash(h);
    }
}

enum Step {
    Continue(Option<u16>),
    Sleep(Option<u16>, Tick),
    Fail,
}

impl GraphRuntime {
    /// A runtime holding at most `instances` running graphs and
    /// `markers_per_tick` markers per evaluation. The only allocating call.
    #[must_use]
    pub fn with_capacity(instances: usize, markers_per_tick: usize) -> Self {
        Self {
            instances: BoundedVec::with_capacity(instances),
            markers: BoundedVec::with_capacity(markers_per_tick),
            next_id: 1,
        }
    }

    /// Running instances.
    #[must_use]
    pub fn len(&self) -> usize {
        self.instances.len()
    }

    /// True when nothing is running.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.instances.is_empty()
    }

    /// Markers emitted by the last evaluation, in emission order.
    #[must_use]
    pub fn markers(&self) -> &[TimelineMarker] {
        &self.markers
    }

    /// Starts `graph` for `source`. It runs its first nodes at the next
    /// evaluation with `now >= now`.
    ///
    /// # Errors
    /// [`GraphError::UnknownGraph`], [`GraphError::MissingTarget`] for a graph
    /// that needs a target, [`GraphError::InstancesFull`].
    pub fn start(
        &mut self,
        catalog: &GraphCatalog,
        graph: GraphId,
        source: EntityId,
        target: Option<EntityId>,
        now: Tick,
    ) -> Result<GraphInstanceId, GraphError> {
        let compiled = catalog.get(graph).ok_or(GraphError::UnknownGraph(graph))?;
        if compiled.needs_target && target.is_none() {
            return Err(GraphError::MissingTarget(graph));
        }
        let id = GraphInstanceId(self.next_id);
        self.instances
            .push(Instance {
                id,
                graph,
                source,
                target,
                started: now,
                cursor: Some(compiled.entry),
                wake_at: now,
                counters: [0; MAX_COUNTERS],
            })
            .map_err(|_| GraphError::InstancesFull)?;
        self.next_id += 1;
        Ok(id)
    }

    /// Stops an instance. False if it is not running.
    pub fn cancel(&mut self, id: GraphInstanceId) -> bool {
        let before = self.instances.len();
        self.instances.retain(|i| i.id != id);
        self.instances.len() != before
    }

    /// Runs every instance that is due at `now`, in start order, until each
    /// waits or ends. Clears and refills the marker buffer. Allocation-free.
    pub fn evaluate(
        &mut self,
        catalog: &GraphCatalog,
        now: Tick,
        seed: Seed,
        handler: &mut impl ActionHandler,
    ) -> EvalReport {
        self.markers.clear();
        let mut report = EvalReport::default();
        for inst in self.instances.iter_mut() {
            if inst.wake_at > now || inst.cursor.is_none() {
                continue;
            }
            report.evaluated += 1;
            let Some(graph) = catalog.get(inst.graph) else {
                inst.cursor = None;
                report.failed += 1;
                continue;
            };
            let mut budget = STEP_BUDGET;
            loop {
                let Some(at) = inst.cursor else {
                    report.finished += 1;
                    break;
                };
                if budget == 0 {
                    inst.cursor = None;
                    report.failed += 1;
                    break;
                }
                budget -= 1;
                match step(
                    graph,
                    inst,
                    at,
                    now,
                    seed,
                    handler,
                    &mut self.markers,
                    &mut report,
                ) {
                    Step::Continue(next) => inst.cursor = next,
                    Step::Sleep(next, wake) => {
                        inst.cursor = next;
                        inst.wake_at = wake;
                        if next.is_none() {
                            report.finished += 1;
                        }
                        break;
                    }
                    Step::Fail => {
                        inst.cursor = None;
                        report.failed += 1;
                        break;
                    }
                }
            }
        }
        self.instances.retain(|i| i.cursor.is_some());
        report
    }
}

#[allow(clippy::too_many_arguments)] // one evaluation step needs all of it
fn step(
    graph: &CompiledGraph,
    inst: &mut Instance,
    at: u16,
    now: Tick,
    seed: Seed,
    handler: &mut impl ActionHandler,
    markers: &mut BoundedVec<TimelineMarker>,
    report: &mut EvalReport,
) -> Step {
    let (Some(node), Some(links)) = (graph.nodes.get(usize::from(at)), graph.links.get(usize::from(at)))
    else {
        return Step::Fail;
    };
    let id = MarkerId {
        graph: graph.id,
        node: node.key,
    };
    let [first, second] = *links;
    match node.kind {
        NodeKind::Marker { marker, offset, .. } => {
            let kind = match marker {
                MarkerSpec::CastStart => MarkerKind::CastStart,
                MarkerSpec::Impact => match inst.target {
                    Some(target) => MarkerKind::Impact { target },
                    None => return Step::Fail,
                },
                MarkerSpec::Tick { counter } => {
                    MarkerKind::TickN(inst.counters.get(usize::from(counter)).copied().unwrap_or(0))
                }
                MarkerSpec::Expire => MarkerKind::Expire,
                MarkerSpec::Package(p) => MarkerKind::Package(p),
            };
            let at_tick = Tick(now.0.saturating_add(u64::from(offset)));
            let pushed = markers.push(TimelineMarker {
                id,
                kind,
                at: at_tick,
                offset: u32::try_from(at_tick.saturating_sub(inst.started)).unwrap_or(u32::MAX),
                source: inst.source,
                target: inst.target,
                instance: inst.id,
            });
            if pushed.is_err() {
                report.markers_dropped += 1;
            }
            Step::Continue(first)
        }
        NodeKind::Delay { ticks, .. } => Step::Sleep(first, Tick(now.0.saturating_add(u64::from(ticks)))),
        NodeKind::Action {
            action,
            target,
            params,
            ..
        } => {
            let resolved = match target {
                Target::Source => inst.source,
                Target::Target => match inst.target {
                    Some(t) => t,
                    None => return Step::Fail,
                },
            };
            let call = ActionCall {
                action,
                params,
                source: inst.source,
                target: resolved,
                instance: inst.id,
                node: id,
                tick: now,
                counters: inst.counters,
            };
            match handler.apply(&call) {
                Ok(()) => Step::Continue(first),
                Err(_) => Step::Fail,
            }
        }
        NodeKind::Chance {
            numerator,
            denominator,
            ..
        } => {
            let salt = Salt::new(
                (u64::from(graph.id.0) << 32 | u64::from(node.key.0) << 16) ^ inst.id.0.rotate_left(17),
            );
            let mut rng = Rng::for_entity(seed, now, inst.source, salt);
            if rng.chance(numerator, denominator) {
                Step::Continue(first)
            } else {
                Step::Continue(second)
            }
        }
        NodeKind::Repeat { counter, times, .. } => {
            let Some(c) = inst.counters.get_mut(usize::from(counter)) else {
                return Step::Fail;
            };
            if *c < times {
                *c += 1;
                Step::Continue(first)
            } else {
                *c = 0;
                Step::Continue(second)
            }
        }
    }
}

/// Key of the node an instance will run next; for diagnostics and tests.
impl GraphRuntime {
    /// `(instance, graph, next node key, wake tick)` for every running instance.
    pub fn instances(
        &self,
        catalog: &GraphCatalog,
    ) -> impl Iterator<Item = (GraphInstanceId, GraphId, Option<NodeKey>, Tick)> {
        self.instances.iter().map(move |i| {
            let key = i.cursor.and_then(|c| {
                catalog
                    .get(i.graph)
                    .and_then(|g| g.nodes.get(usize::from(c)))
                    .map(|n| n.key)
            });
            (i.id, i.graph, key, i.wake_at)
        })
    }
}

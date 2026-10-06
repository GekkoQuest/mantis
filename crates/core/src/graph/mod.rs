//! Gameplay graphs and timeline markers (plan 6.6, decision 0006).
//!
//! Abilities, buffs, damage, healing, displacement, loot, and procs are
//! **gameplay graphs**: content made of typed nodes, evaluated by the core on
//! both hosts. A graph controls *when* things happen ([`NodeKind::Delay`],
//! [`NodeKind::Repeat`], [`NodeKind::Chance`]) and delegates *what* happens to
//! typed actions that modules register ([`GraphCatalog::register_action`],
//! [`ActionHandler`]), so game rules stay in packages.
//!
//! Graphs emit typed **timeline markers** with tick offsets ([`TimelineMarker`]):
//! `CastStart`, `Impact{target}`, `TickN`, `Expire`, and package-defined
//! kinds. Presentation graphs (VFX, sound, camera) bind to marker ids. They
//! are separate content with their own hash; **no presentation node exists
//! here**, and the server never loads presentation content.
//!
//! [`MarkerId`] is `(graph, node key)`. Both parts derive from authored
//! content (the graph's stable name and the node's authored key), never from
//! load order, so presentation bindings survive re-cooks.

mod runtime;

pub use runtime::{
    ActionCall, ActionError, ActionHandler, EvalReport, GraphRuntime, MAX_COUNTERS, STEP_BUDGET,
};

use core::fmt;
use std::borrow::Cow;
use std::collections::BTreeMap;

use crate::ecs::EntityId;
use crate::hash::{StableHasher, StateHash};
use crate::time::Tick;

/// Stable id of a gameplay graph: the 32-bit FNV-1a hash of its stable
/// content name. The catalog refuses collisions.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct GraphId(pub u32);

impl GraphId {
    /// The id for a stable content name, for example `"ability.bolt"`.
    #[must_use]
    pub const fn named(name: &str) -> Self {
        let mut bytes = name.as_bytes();
        let mut h: u32 = 0x811C_9DC5;
        while let [first, rest @ ..] = bytes {
            h ^= *first as u32;
            h = h.wrapping_mul(0x0100_0193);
            bytes = rest;
        }
        Self(h)
    }
}

/// Authored, stable key of a node within its graph. Editors never reuse a
/// deleted key, so marker bindings never silently move to another node.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct NodeKey(pub u16);

/// Id of a registered action kind.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct ActionId(pub u16);

/// A package-defined marker kind.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct PackageMarker(pub u16);

/// Identity of a marker for presentation binding: which node of which graph.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct MarkerId {
    /// The graph.
    pub graph: GraphId,
    /// The node that emitted it.
    pub node: NodeKey,
}

/// The marker a [`NodeKind::Marker`] node emits, as authored.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum MarkerSpec {
    /// The cast begins.
    CastStart,
    /// The effect lands on the instance's target.
    Impact,
    /// A periodic tick; carries the value of the given repeat counter.
    Tick {
        /// Counter slot, `< MAX_COUNTERS`.
        counter: u8,
    },
    /// The effect ends.
    Expire,
    /// A package-defined kind.
    Package(PackageMarker),
}

/// The kind of an emitted marker, with runtime data filled in.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum MarkerKind {
    /// The cast begins.
    CastStart,
    /// The effect lands on `target`.
    Impact {
        /// The entity hit.
        target: EntityId,
    },
    /// The `n`th periodic tick (1-based).
    TickN(u16),
    /// The effect ends.
    Expire,
    /// A package-defined kind.
    Package(PackageMarker),
}

/// Identity of one running graph instance in one cell. Monotonic, never reused.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct GraphInstanceId(pub u64);

/// A typed timeline marker: presentation (and legacy adapters) schedule on
/// these.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct TimelineMarker {
    /// Which node of which graph.
    pub id: MarkerId,
    /// What happened.
    pub kind: MarkerKind,
    /// The tick the marker is for (may be in the future: `emitted + offset`).
    pub at: Tick,
    /// Ticks from the instance's start to `at`.
    pub offset: u32,
    /// The entity running the graph.
    pub source: EntityId,
    /// The instance's target, if any.
    pub target: Option<EntityId>,
    /// The instance.
    pub instance: GraphInstanceId,
}

/// Whom an action applies to.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum Target {
    /// The entity running the graph.
    Source,
    /// The instance's target.
    Target,
}

/// Parameters of an action. Their meaning is the registered action's schema;
/// values come from content and are validated by the cook.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, Default)]
pub struct ActionParams(pub [i32; 4]);

/// One node, as authored.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Node {
    /// Stable key.
    pub key: NodeKey,
    /// What the node does.
    pub kind: NodeKind,
}

/// Node kinds. Control flow and typed actions only; no presentation.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum NodeKind {
    /// Emit a marker `offset` ticks from now, then continue.
    Marker {
        /// The marker.
        marker: MarkerSpec,
        /// Ticks ahead of now.
        offset: u16,
        /// Next node, or end.
        next: Option<NodeKey>,
    },
    /// Wait `ticks` (at least 1), then continue on a later tick.
    Delay {
        /// Ticks to wait.
        ticks: u32,
        /// Next node, or end.
        next: Option<NodeKey>,
    },
    /// Apply a registered action, then continue.
    Action {
        /// The action.
        action: ActionId,
        /// Whom it applies to.
        target: Target,
        /// Its parameters.
        params: ActionParams,
        /// Next node, or end.
        next: Option<NodeKey>,
    },
    /// Branch with probability `numerator / denominator` from the cell's
    /// seeded stream.
    Chance {
        /// Numerator.
        numerator: u32,
        /// Denominator, nonzero.
        denominator: u32,
        /// Taken with the given probability.
        then: Option<NodeKey>,
        /// Taken otherwise.
        otherwise: Option<NodeKey>,
    },
    /// Loop: while counter `counter` is below `times`, increment it and go to
    /// `body`; otherwise reset it and go to `done`. The body must reach a
    /// `Delay` before returning here (validated).
    Repeat {
        /// Counter slot, `< MAX_COUNTERS`.
        counter: u8,
        /// Iterations.
        times: u16,
        /// Loop body.
        body: NodeKey,
        /// After the loop, or end.
        done: Option<NodeKey>,
    },
}

/// A gameplay graph as authored content.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct GameplayGraph {
    /// Stable id ([`GraphId::named`]).
    pub id: GraphId,
    /// The first node.
    pub entry: NodeKey,
    /// The nodes, in any order.
    pub nodes: Vec<Node>,
}

/// Why a graph or an evaluation was refused.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum GraphError {
    /// Two nodes share a key.
    DuplicateNode(NodeKey),
    /// A link names a key that does not exist.
    UnknownNode(NodeKey),
    /// The graph has no nodes or more than `u16::MAX`.
    BadSize,
    /// A `Delay` of zero ticks.
    ZeroDelay(NodeKey),
    /// A `Chance` with denominator zero.
    ZeroDenominator(NodeKey),
    /// A counter slot out of range.
    BadCounter(NodeKey),
    /// An action id that was never registered.
    UnknownAction(ActionId),
    /// A cycle that can run without passing a `Delay`: it could spin forever
    /// within one tick.
    ZeroTimeCycle(NodeKey),
    /// A graph with this id is already in the catalog.
    DuplicateGraph(GraphId),
    /// An action name registered twice (the id it already has).
    DuplicateAction(ActionId),
    /// No graph with this id.
    UnknownGraph(GraphId),
    /// The graph targets an entity but the instance has no target.
    MissingTarget(GraphId),
    /// The runtime has no free instance slot.
    InstancesFull,
}

impl fmt::Display for GraphError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "gameplay graph error: {self:?}")
    }
}

impl std::error::Error for GraphError {}

/// A validated graph with links resolved to indices.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct CompiledGraph {
    pub(crate) id: GraphId,
    pub(crate) entry: u16,
    pub(crate) nodes: Vec<Node>,
    pub(crate) links: Vec<[Option<u16>; 2]>,
    pub(crate) needs_target: bool,
}

impl CompiledGraph {
    /// The graph id.
    #[must_use]
    pub fn id(&self) -> GraphId {
        self.id
    }

    /// True when the graph uses the instance target (an `Impact` marker or an
    /// action on `Target`); such graphs refuse to start without one.
    #[must_use]
    pub fn needs_target(&self) -> bool {
        self.needs_target
    }
}

fn successors(kind: &NodeKind) -> [Option<NodeKey>; 2] {
    match *kind {
        NodeKind::Marker { next, .. } | NodeKind::Delay { next, .. } | NodeKind::Action { next, .. } => {
            [next, None]
        }
        NodeKind::Chance { then, otherwise, .. } => [then, otherwise],
        NodeKind::Repeat { body, done, .. } => [Some(body), done],
    }
}

/// The registry of action kinds and the loaded gameplay graphs of a cell.
#[derive(Clone, Default, Debug)]
pub struct GraphCatalog {
    actions: Vec<Cow<'static, str>>,
    graphs: BTreeMap<GraphId, CompiledGraph>,
}

impl GraphCatalog {
    /// An empty catalog.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Registers an action kind by stable name. Ids are dense in registration
    /// order, which is deterministic for a module set.
    ///
    /// # Errors
    /// [`GraphError::DuplicateAction`]; [`GraphError::BadSize`] past `u16::MAX`.
    pub fn register_action(&mut self, name: impl Into<Cow<'static, str>>) -> Result<ActionId, GraphError> {
        let name = name.into();
        if let Some(existing) = self.action_id(&name) {
            return Err(GraphError::DuplicateAction(existing));
        }
        let id = u16::try_from(self.actions.len()).map_err(|_| GraphError::BadSize)?;
        self.actions.push(name);
        Ok(ActionId(id))
    }

    /// The name of an action.
    #[must_use]
    pub fn action_name(&self, id: ActionId) -> Option<&str> {
        self.actions.get(usize::from(id.0)).map(|n| &**n)
    }

    /// The id of the action registered as `name`.
    #[must_use]
    pub fn action_id(&self, name: &str) -> Option<ActionId> {
        self.actions
            .iter()
            .position(|a| a == name)
            .and_then(|i| u16::try_from(i).ok())
            .map(ActionId)
    }

    /// Number of registered actions.
    #[must_use]
    pub fn action_count(&self) -> usize {
        self.actions.len()
    }

    /// The compiled graph with `id`.
    #[must_use]
    pub fn get(&self, id: GraphId) -> Option<&CompiledGraph> {
        self.graphs.get(&id)
    }

    /// Number of graphs.
    #[must_use]
    pub fn len(&self) -> usize {
        self.graphs.len()
    }

    /// True when no graph is loaded.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.graphs.is_empty()
    }

    /// Validates, compiles, and adds a graph.
    ///
    /// # Errors
    /// Any structural [`GraphError`]; the catalog is unchanged on error.
    pub fn insert(&mut self, graph: GameplayGraph) -> Result<(), GraphError> {
        if self.graphs.contains_key(&graph.id) {
            return Err(GraphError::DuplicateGraph(graph.id));
        }
        let compiled = self.compile(graph)?;
        self.graphs.insert(compiled.id, compiled);
        Ok(())
    }

    fn compile(&self, graph: GameplayGraph) -> Result<CompiledGraph, GraphError> {
        if graph.nodes.is_empty() || graph.nodes.len() > usize::from(u16::MAX) {
            return Err(GraphError::BadSize);
        }
        let mut index: BTreeMap<NodeKey, u16> = BTreeMap::new();
        for (i, node) in (0u16..).zip(&graph.nodes) {
            if index.insert(node.key, i).is_some() {
                return Err(GraphError::DuplicateNode(node.key));
            }
        }
        let resolve = |k: Option<NodeKey>| -> Result<Option<u16>, GraphError> {
            k.map(|k| index.get(&k).copied().ok_or(GraphError::UnknownNode(k)))
                .transpose()
        };
        let entry = resolve(Some(graph.entry))?.ok_or(GraphError::UnknownNode(graph.entry))?;
        let mut links = Vec::with_capacity(graph.nodes.len());
        let mut needs_target = false;
        for node in &graph.nodes {
            match node.kind {
                NodeKind::Delay { ticks: 0, .. } => return Err(GraphError::ZeroDelay(node.key)),
                NodeKind::Chance { denominator: 0, .. } => return Err(GraphError::ZeroDenominator(node.key)),
                NodeKind::Repeat { counter, .. }
                | NodeKind::Marker {
                    marker: MarkerSpec::Tick { counter },
                    ..
                } if usize::from(counter) >= MAX_COUNTERS => {
                    return Err(GraphError::BadCounter(node.key));
                }
                NodeKind::Action { action, target, .. } => {
                    if self.action_name(action).is_none() {
                        return Err(GraphError::UnknownAction(action));
                    }
                    needs_target |= target == Target::Target;
                }
                NodeKind::Marker {
                    marker: MarkerSpec::Impact,
                    ..
                } => needs_target = true,
                _ => {}
            }
            let [a, b] = successors(&node.kind);
            links.push([resolve(a)?, resolve(b)?]);
        }
        Self::check_cycles(&graph.nodes, &links)?;
        Ok(CompiledGraph {
            id: graph.id,
            entry,
            nodes: graph.nodes,
            links,
            needs_target,
        })
    }

    /// Fails if a cycle exists among non-`Delay` edges: every loop must wait
    /// at least one tick. Iterative three-color DFS over the edges leaving
    /// non-`Delay` nodes.
    fn check_cycles(nodes: &[Node], links: &[[Option<u16>; 2]]) -> Result<(), GraphError> {
        const WHITE: u8 = 0;
        const GREY: u8 = 1;
        const BLACK: u8 = 2;
        let mut color = vec![WHITE; nodes.len()];
        let edges = |i: usize| -> [Option<u16>; 2] {
            match nodes.get(i).map(|n| n.kind) {
                Some(NodeKind::Delay { .. }) | None => [None, None],
                Some(_) => links.get(i).copied().unwrap_or([None, None]),
            }
        };
        for start in 0..nodes.len() {
            if color.get(start) != Some(&WHITE) {
                continue;
            }
            let mut stack: Vec<(usize, usize)> = vec![(start, 0)];
            if let Some(c) = color.get_mut(start) {
                *c = GREY;
            }
            while let Some(&(node, edge)) = stack.last() {
                let next = edges(node).get(edge).copied().flatten();
                if let Some(top) = stack.last_mut() {
                    top.1 += 1;
                }
                if edge >= 2 {
                    stack.pop();
                    if let Some(c) = color.get_mut(node) {
                        *c = BLACK;
                    }
                    continue;
                }
                let Some(next) = next.map(usize::from) else {
                    continue;
                };
                match color.get(next) {
                    Some(&GREY) => {
                        let key = nodes.get(next).map_or(NodeKey(0), |n| n.key);
                        return Err(GraphError::ZeroTimeCycle(key));
                    }
                    Some(&WHITE) => {
                        if let Some(c) = color.get_mut(next) {
                            *c = GREY;
                        }
                        stack.push((next, 0));
                    }
                    _ => {}
                }
            }
        }
        Ok(())
    }
}

impl StateHash for GraphInstanceId {
    fn state_hash(&self, h: &mut StableHasher) {
        h.write_u64(self.0);
    }
}

#[cfg(test)]
mod tests;

//! Gameplay graph v1 (MGPH): one cooked gameplay graph (decision 0021), gameplay content
//! domain. The cell builds its `mantis_core::graph::GraphCatalog` from these after its
//! modules register their actions ([`GraphAsset::to_core`]); the cook validates every
//! graph with the same catalog rules before it ships.
//!
//! # Layout (little-endian)
//!
//! | size | field |
//! |---|---|
//! | 4 | magic `"MGPH"` |
//! | 2 | version `u16` = 1 |
//! | 2 | flags `u16`, 0 |
//! | 2 + n | stable name: `u16` byte length (1 to [`MAX_NAME`]), UTF-8; the graph id is `GraphId::named(name)` |
//! | 2 | entry node key `u16` |
//! | 2 | action name count `u16` |
//! | 2 + n each | action names: `u16` byte length (1 to [`MAX_NAME`]), UTF-8; distinct, each used by an action node |
//! | 2 | node count `u16`, 1 to `u16::MAX - 1` |
//! | 28 each | nodes in strictly ascending key order: `key u16` (not [`NO_NODE`]), `kind u8`, `0 u8`, 24 field bytes |
//!
//! Node fields by kind (unused bytes zero; a link is a node key, or [`NO_NODE`] for "end"):
//!
//! | kind | fields |
//! |---|---|
//! | 0 marker | `spec u8` (0 cast start, 1 impact, 2 tick, 3 expire, 4 package), `0 u8`, `arg u16` (tick: the counter; package: the package marker; else 0), `offset u16`, `next u16` |
//! | 1 delay | `ticks u32` (at least 1), `next u16` |
//! | 2 action | `action u16` (index into the action names), `target u8` (0 source, 1 target), `0 u8`, `params [i32; 4]`, `next u16` |
//! | 3 chance | `numerator u32`, `denominator u32` (nonzero), `then u16`, `otherwise u16` |
//! | 4 repeat | `counter u8` (below `mantis_core::graph::MAX_COUNTERS`), `0 u8`, `times u16`, `body u16` (a node, never [`NO_NODE`]), `done u16` |
//!
//! Every link must name a node of the graph. The parser checks the layout and these
//! field rules; the catalog rules that span nodes (no zero-time cycles) are the cook's
//! and the cell's, through `GraphCatalog::insert`.

use mantis_core::graph::{
    ActionId, ActionParams, GameplayGraph, GraphId, MAX_COUNTERS, MarkerSpec, Node, NodeKey, NodeKind,
    PackageMarker, Target,
};

use crate::FormatError;
use crate::bytes::{Reader, Writer};

/// Magic bytes.
pub const MAGIC: [u8; 4] = *b"MGPH";
/// Format version.
pub const VERSION: u16 = 1;
/// The link value meaning "no next node".
pub const NO_NODE: u16 = u16::MAX;
/// Longest graph or action name, in bytes.
pub const MAX_NAME: usize = 255;
/// Bytes of one node record.
const NODE_BYTES: usize = 28;

/// A node's kind with its fields, as stored. Links are node keys; `None` is the end.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum GraphNodeKind {
    /// Emit a marker `offset` ticks from now, then continue.
    Marker {
        /// The marker.
        marker: MarkerSpec,
        /// Ticks ahead of now.
        offset: u16,
        /// Next node.
        next: Option<u16>,
    },
    /// Wait `ticks` (at least 1).
    Delay {
        /// Ticks.
        ticks: u32,
        /// Next node.
        next: Option<u16>,
    },
    /// Apply the action named by `action` (an index into [`GraphAsset::actions`]).
    Action {
        /// Index of the action's name.
        action: u16,
        /// Whom it applies to.
        target: Target,
        /// Parameters.
        params: [i32; 4],
        /// Next node.
        next: Option<u16>,
    },
    /// Branch with probability `numerator / denominator`.
    Chance {
        /// Numerator.
        numerator: u32,
        /// Denominator, nonzero.
        denominator: u32,
        /// Taken with the probability.
        then: Option<u16>,
        /// Taken otherwise.
        otherwise: Option<u16>,
    },
    /// Loop `times` through `body` on counter `counter`, then go to `done`.
    Repeat {
        /// Counter slot.
        counter: u8,
        /// Iterations.
        times: u16,
        /// Loop body.
        body: u16,
        /// After the loop.
        done: Option<u16>,
    },
}

/// One node.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct GraphNode {
    /// Stable authored key.
    pub key: u16,
    /// Kind and fields.
    pub kind: GraphNodeKind,
}

/// A cooked gameplay graph.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct GraphAsset {
    /// The stable name (the graph id is [`GraphId::named`] of it).
    pub name: String,
    /// The entry node's key.
    pub entry: u16,
    /// Names of the registered actions the graph's action nodes apply.
    pub actions: Vec<String>,
    /// Nodes, in strictly ascending key order.
    pub nodes: Vec<GraphNode>,
}

/// Why a graph could not become a core graph.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum GraphLoadError {
    /// An action name no module registered.
    UnknownAction(String),
    /// The asset breaks a format rule.
    Format(FormatError),
}

impl core::fmt::Display for GraphLoadError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            GraphLoadError::UnknownAction(name) => write!(f, "no module registers the action `{name}`"),
            GraphLoadError::Format(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for GraphLoadError {}

fn links(kind: &GraphNodeKind) -> [Option<u16>; 2] {
    match *kind {
        GraphNodeKind::Marker { next, .. }
        | GraphNodeKind::Delay { next, .. }
        | GraphNodeKind::Action { next, .. } => [next, None],
        GraphNodeKind::Chance { then, otherwise, .. } => [then, otherwise],
        GraphNodeKind::Repeat { body, done, .. } => [Some(body), done],
    }
}

fn name_ok(name: &str) -> bool {
    !name.is_empty() && name.len() <= MAX_NAME
}

impl GraphAsset {
    /// The graph id.
    pub fn id(&self) -> GraphId {
        GraphId::named(&self.name)
    }

    /// The node with key `key`.
    pub fn node(&self, key: u16) -> Option<&GraphNode> {
        self.nodes
            .binary_search_by_key(&key, |n| n.key)
            .ok()
            .and_then(|i| self.nodes.get(i))
    }

    /// Keys of the nodes that emit markers (what presentation binds to).
    pub fn marker_keys(&self) -> impl Iterator<Item = u16> + '_ {
        self.nodes
            .iter()
            .filter(|n| matches!(n.kind, GraphNodeKind::Marker { .. }))
            .map(|n| n.key)
    }

    /// Checks every rule of the format.
    ///
    /// # Errors
    /// [`FormatError::Validity`] for names, keys, and field values;
    /// [`FormatError::Inconsistent`] for links, action indices, and unused or repeated
    /// action names.
    pub fn validate(&self) -> Result<(), FormatError> {
        if !name_ok(&self.name) || !self.actions.iter().all(|a| name_ok(a)) {
            return Err(FormatError::Validity);
        }
        if self.nodes.is_empty() || self.nodes.len() >= usize::from(NO_NODE) {
            return Err(FormatError::Dimensions);
        }
        let ascending = self
            .nodes
            .windows(2)
            .all(|w| matches!(w, [a, b] if a.key < b.key));
        if !ascending || self.nodes.iter().any(|n| n.key == NO_NODE) {
            return Err(FormatError::Inconsistent);
        }
        if self.node(self.entry).is_none() {
            return Err(FormatError::Inconsistent);
        }
        let mut used = vec![false; self.actions.len()];
        for n in &self.nodes {
            if links(&n.kind).iter().flatten().any(|k| self.node(*k).is_none()) {
                return Err(FormatError::Inconsistent);
            }
            match n.kind {
                GraphNodeKind::Marker {
                    marker: MarkerSpec::Tick { counter },
                    ..
                }
                | GraphNodeKind::Repeat { counter, .. }
                    if usize::from(counter) >= MAX_COUNTERS =>
                {
                    return Err(FormatError::Validity);
                }
                GraphNodeKind::Delay { ticks: 0, .. } | GraphNodeKind::Chance { denominator: 0, .. } => {
                    return Err(FormatError::Validity);
                }
                GraphNodeKind::Action { action, .. } => {
                    let slot = used
                        .get_mut(usize::from(action))
                        .ok_or(FormatError::Inconsistent)?;
                    *slot = true;
                }
                _ => {}
            }
        }
        let distinct = self
            .actions
            .iter()
            .enumerate()
            .all(|(i, a)| !self.actions.iter().take(i).any(|b| b == a));
        if !distinct || used.contains(&false) {
            return Err(FormatError::Inconsistent);
        }
        Ok(())
    }

    /// The core graph, with each action name resolved by `resolve_action` (the cell's
    /// catalog after its modules registered their actions).
    ///
    /// # Errors
    /// [`GraphLoadError::UnknownAction`] for a name `resolve_action` does not know;
    /// [`GraphLoadError::Format`] when the asset breaks a format rule.
    pub fn to_core(
        &self,
        mut resolve_action: impl FnMut(&str) -> Option<ActionId>,
    ) -> Result<GameplayGraph, GraphLoadError> {
        self.validate().map_err(GraphLoadError::Format)?;
        let mut ids = Vec::with_capacity(self.actions.len());
        for name in &self.actions {
            ids.push(resolve_action(name).ok_or_else(|| GraphLoadError::UnknownAction(name.clone()))?);
        }
        let key = |k: Option<u16>| k.map(NodeKey);
        let nodes = self
            .nodes
            .iter()
            .map(|n| {
                let kind = match n.kind {
                    GraphNodeKind::Marker { marker, offset, next } => NodeKind::Marker {
                        marker,
                        offset,
                        next: key(next),
                    },
                    GraphNodeKind::Delay { ticks, next } => NodeKind::Delay {
                        ticks,
                        next: key(next),
                    },
                    GraphNodeKind::Action {
                        action,
                        target,
                        params,
                        next,
                    } => NodeKind::Action {
                        action: ids
                            .get(usize::from(action))
                            .copied()
                            .ok_or(GraphLoadError::Format(FormatError::Inconsistent))?,
                        target,
                        params: ActionParams(params),
                        next: key(next),
                    },
                    GraphNodeKind::Chance {
                        numerator,
                        denominator,
                        then,
                        otherwise,
                    } => NodeKind::Chance {
                        numerator,
                        denominator,
                        then: key(then),
                        otherwise: key(otherwise),
                    },
                    GraphNodeKind::Repeat {
                        counter,
                        times,
                        body,
                        done,
                    } => NodeKind::Repeat {
                        counter,
                        times,
                        body: NodeKey(body),
                        done: key(done),
                    },
                };
                Ok(Node {
                    key: NodeKey(n.key),
                    kind,
                })
            })
            .collect::<Result<Vec<_>, GraphLoadError>>()?;
        Ok(GameplayGraph {
            id: self.id(),
            entry: NodeKey(self.entry),
            nodes,
        })
    }

    /// Encodes the payload.
    pub fn encode(&self) -> Vec<u8> {
        let mut w = Writer::new();
        w.bytes(&MAGIC);
        w.u16(VERSION);
        w.u16(0);
        write_name(&mut w, &self.name);
        w.u16(self.entry);
        w.u16(u16::try_from(self.actions.len()).unwrap_or(u16::MAX));
        for a in &self.actions {
            write_name(&mut w, a);
        }
        w.u16(u16::try_from(self.nodes.len()).unwrap_or(u16::MAX));
        for n in &self.nodes {
            let mut fields = [0u8; 24];
            let kind = encode_fields(&n.kind, &mut fields);
            w.u16(n.key);
            w.u8(kind);
            w.u8(0);
            w.bytes(&fields);
        }
        w.into_bytes()
    }

    /// Parses and validates a payload.
    ///
    /// # Errors
    /// [`FormatError`] describing the first problem found.
    pub fn parse(bytes: &[u8]) -> Result<GraphAsset, FormatError> {
        let mut r = Reader::new(bytes);
        if r.array::<4>()? != MAGIC {
            return Err(FormatError::Magic);
        }
        let version = r.u16()?;
        if version != VERSION {
            return Err(FormatError::Version(version));
        }
        let flags = r.u16()?;
        if flags != 0 {
            return Err(FormatError::Flags(u32::from(flags)));
        }
        let name = read_name(&mut r)?;
        let entry = r.u16()?;
        let action_count = usize::from(r.u16()?);
        let mut actions = Vec::with_capacity(action_count.min(r.remaining() / 3));
        for _ in 0..action_count {
            actions.push(read_name(&mut r)?);
        }
        let node_count = usize::from(r.u16()?);
        if r.remaining() != node_count * NODE_BYTES {
            return Err(FormatError::Length {
                expected: (r.position() + node_count * NODE_BYTES) as u64,
                actual: bytes.len() as u64,
            });
        }
        let mut nodes = Vec::with_capacity(node_count);
        for _ in 0..node_count {
            let key = r.u16()?;
            let kind = r.u8()?;
            if r.u8()? != 0 {
                return Err(FormatError::Reserved);
            }
            let fields: [u8; 24] = r.array()?;
            nodes.push(GraphNode {
                key,
                kind: decode_fields(kind, &fields)?,
            });
        }
        r.finish()?;
        let asset = GraphAsset {
            name,
            entry,
            actions,
            nodes,
        };
        asset.validate()?;
        Ok(asset)
    }
}

fn write_name(w: &mut Writer, name: &str) {
    w.u16(u16::try_from(name.len()).unwrap_or(u16::MAX));
    w.bytes(name.as_bytes());
}

fn read_name(r: &mut Reader<'_>) -> Result<String, FormatError> {
    let n = usize::from(r.u16()?);
    if n == 0 || n > MAX_NAME {
        return Err(FormatError::Validity);
    }
    let bytes = r.slice(n)?;
    String::from_utf8(bytes.to_vec()).map_err(|_| FormatError::Validity)
}

fn link(k: Option<u16>) -> u16 {
    k.unwrap_or(NO_NODE)
}

fn unlink(v: u16) -> Option<u16> {
    (v != NO_NODE).then_some(v)
}

/// Writes a kind's fields; returns the kind code.
fn encode_fields(kind: &GraphNodeKind, out: &mut [u8; 24]) -> u8 {
    let mut w = Writer::new();
    let code = match *kind {
        GraphNodeKind::Marker { marker, offset, next } => {
            let (spec, arg) = match marker {
                MarkerSpec::CastStart => (0, 0),
                MarkerSpec::Impact => (1, 0),
                MarkerSpec::Tick { counter } => (2, u16::from(counter)),
                MarkerSpec::Expire => (3, 0),
                MarkerSpec::Package(PackageMarker(p)) => (4, p),
            };
            w.u8(spec);
            w.u8(0);
            w.u16(arg);
            w.u16(offset);
            w.u16(link(next));
            0
        }
        GraphNodeKind::Delay { ticks, next } => {
            w.u32(ticks);
            w.u16(link(next));
            1
        }
        GraphNodeKind::Action {
            action,
            target,
            params,
            next,
        } => {
            w.u16(action);
            w.u8(match target {
                Target::Source => 0,
                Target::Target => 1,
            });
            w.u8(0);
            for p in params {
                w.i32(p);
            }
            w.u16(link(next));
            2
        }
        GraphNodeKind::Chance {
            numerator,
            denominator,
            then,
            otherwise,
        } => {
            w.u32(numerator);
            w.u32(denominator);
            w.u16(link(then));
            w.u16(link(otherwise));
            3
        }
        GraphNodeKind::Repeat {
            counter,
            times,
            body,
            done,
        } => {
            w.u8(counter);
            w.u8(0);
            w.u16(times);
            w.u16(body);
            w.u16(link(done));
            4
        }
    };
    for (o, b) in out.iter_mut().zip(w.into_bytes()) {
        *o = b;
    }
    code
}

fn decode_fields(kind: u8, fields: &[u8; 24]) -> Result<GraphNodeKind, FormatError> {
    let mut r = Reader::new(fields);
    let zero_rest = |r: &Reader<'_>| {
        let used = r.position();
        if fields.iter().skip(used).all(|b| *b == 0) {
            Ok(())
        } else {
            Err(FormatError::Reserved)
        }
    };
    let out = match kind {
        0 => {
            let spec = r.u8()?;
            if r.u8()? != 0 {
                return Err(FormatError::Reserved);
            }
            let arg = r.u16()?;
            let marker = match (spec, arg) {
                (0, 0) => MarkerSpec::CastStart,
                (1, 0) => MarkerSpec::Impact,
                (2, c) => MarkerSpec::Tick {
                    counter: u8::try_from(c).map_err(|_| FormatError::Validity)?,
                },
                (3, 0) => MarkerSpec::Expire,
                (4, p) => MarkerSpec::Package(PackageMarker(p)),
                _ => return Err(FormatError::Validity),
            };
            let offset = r.u16()?;
            let next = unlink(r.u16()?);
            GraphNodeKind::Marker { marker, offset, next }
        }
        1 => {
            let ticks = r.u32()?;
            let next = unlink(r.u16()?);
            GraphNodeKind::Delay { ticks, next }
        }
        2 => {
            let action = r.u16()?;
            let target = match r.u8()? {
                0 => Target::Source,
                1 => Target::Target,
                _ => return Err(FormatError::Validity),
            };
            if r.u8()? != 0 {
                return Err(FormatError::Reserved);
            }
            let params = [r.i32()?, r.i32()?, r.i32()?, r.i32()?];
            let next = unlink(r.u16()?);
            GraphNodeKind::Action {
                action,
                target,
                params,
                next,
            }
        }
        3 => {
            let numerator = r.u32()?;
            let denominator = r.u32()?;
            let then = unlink(r.u16()?);
            let otherwise = unlink(r.u16()?);
            GraphNodeKind::Chance {
                numerator,
                denominator,
                then,
                otherwise,
            }
        }
        4 => {
            let counter = r.u8()?;
            if r.u8()? != 0 {
                return Err(FormatError::Reserved);
            }
            let times = r.u16()?;
            let body = r.u16()?;
            if body == NO_NODE {
                return Err(FormatError::Inconsistent);
            }
            let done = unlink(r.u16()?);
            GraphNodeKind::Repeat {
                counter,
                times,
                body,
                done,
            }
        }
        other => return Err(FormatError::Encoding(u32::from(other))),
    };
    zero_rest(&r)?;
    Ok(out)
}

#[cfg(test)]
mod tests;

//! Gameplay and presentation graphs side by side (plan 16, decision 0021): every
//! gameplay graph's nodes, with the presentation bindings attached to each marker node,
//! edited in their sources and checked by the cook's own importers.
//!
//! [`GraphBoard`] reads every `*.graph.toml` and `*.presentation.toml` under a content
//! directory (and the `*.particles.toml` effects presentation names). [`GraphBoard::check`]
//! cooks exactly those sources, so every rule (catalog rules for gameplay graphs, a
//! binding naming a real marker node with a matching filter) reports at its file and
//! line. [`GraphBoard::graphs`] lays the result out for the editor: per graph, per node,
//! the bindings that react to it, with unbound markers visible. Presentation edits
//! hot-reload into a live [`PresentationLibrary`]; gameplay graph edits reach the server
//! with the next recook (they change the handshake hash, as decision 0021 intends).

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use mantis_client::presentation::PresentationLibrary;
use mantis_cook::importer::CookError;
use mantis_cook::tree::ContentTree;
use mantis_cook::{Cook, importers};
use mantis_core::graph::{GraphId, MarkerSpec, Target};
use mantis_core::module::toml::Value;
use mantis_formats::bundle::AssetKind;
use mantis_formats::gameplay_graph::{GraphAsset, GraphNodeKind};
use mantis_formats::presentation::{ActionOp, Anchor, MarkerFilter, PresentationGraph};

use crate::source::{EditError, SourceDoc};

const GRAPH: &str = ".graph.toml";
const PRESENTATION: &str = ".presentation.toml";
const PARTICLES: &str = ".particles.toml";

/// One binding as the board shows it.
#[derive(Clone, PartialEq, Debug)]
pub struct BindingView {
    /// The presentation source.
    pub source: String,
    /// The `[binding.<name>]` name.
    pub name: String,
    /// The kind filter, as authored.
    pub filter: String,
    /// One line per action.
    pub actions: Vec<String>,
}

/// One node as the board shows it.
#[derive(Clone, PartialEq, Debug)]
pub struct NodeView {
    /// Stable key.
    pub key: u16,
    /// Authored name.
    pub name: String,
    /// What it does, in a line.
    pub kind: String,
    /// The node names it continues to (empty: the end).
    pub next: Vec<String>,
    /// Whether it emits a marker.
    pub marker: bool,
    /// Bindings reacting to its marker.
    pub bindings: Vec<BindingView>,
}

/// One gameplay graph as the board shows it.
#[derive(Clone, PartialEq, Debug)]
pub struct GraphView {
    /// Stable name.
    pub name: String,
    /// Its source.
    pub source: String,
    /// Nodes in key order.
    pub nodes: Vec<NodeView>,
}

/// Errors of graph editing.
#[derive(Debug)]
pub enum GraphEditError {
    /// A file could not be read or written.
    Io(PathBuf, std::io::Error),
    /// An edit was refused.
    Edit(String, EditError),
    /// No such source.
    NoSource(String),
    /// The sources do not cook: every error at its file and line.
    Check(Vec<CookError>),
}

impl core::fmt::Display for GraphEditError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Io(p, e) => write!(f, "{}: {e}", p.display()),
            Self::Edit(p, e) => write!(f, "{p}: {e}"),
            Self::NoSource(p) => write!(f, "no source `{p}`"),
            Self::Check(errors) => {
                let lines: Vec<String> = errors.iter().map(ToString::to_string).collect();
                write!(f, "{}", lines.join("\n"))
            }
        }
    }
}

impl std::error::Error for GraphEditError {}

fn walk(root: &Path, dir: &Path, out: &mut Vec<String>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    let mut paths: Vec<PathBuf> = entries.filter_map(Result::ok).map(|e| e.path()).collect();
    paths.sort();
    for p in paths {
        if p.is_dir() {
            walk(root, &p, out);
        } else if let Ok(rel) = p.strip_prefix(root) {
            let rel = rel.to_string_lossy().replace('\\', "/");
            if rel.ends_with(GRAPH) || rel.ends_with(PRESENTATION) || rel.ends_with(PARTICLES) {
                out.push(rel);
            }
        }
    }
}

/// The graphs of a content directory.
#[derive(Debug)]
pub struct GraphBoard {
    content: PathBuf,
    /// Graph and presentation sources (edited).
    sources: BTreeMap<String, SourceDoc>,
    /// Effects presentation names (read only, cooked alongside).
    effects: BTreeMap<String, Vec<u8>>,
    dirty: BTreeSet<String>,
    graphs: Vec<(String, GraphAsset)>,
    presentations: Vec<(String, PresentationGraph)>,
    /// What [`GraphBoard::apply`] loaded last, to unload on the next apply.
    loaded: Vec<PresentationGraph>,
}

fn marker_text(m: MarkerSpec) -> String {
    match m {
        MarkerSpec::CastStart => "marker cast_start".to_owned(),
        MarkerSpec::Impact => "marker impact".to_owned(),
        MarkerSpec::Tick { counter } => format!("marker tick (counter {counter})"),
        MarkerSpec::Expire => "marker expire".to_owned(),
        MarkerSpec::Package(p) => format!("marker package {}", p.0),
    }
}

fn filter_text(f: MarkerFilter) -> String {
    match f {
        MarkerFilter::Any => "any".to_owned(),
        MarkerFilter::CastStart => "cast_start".to_owned(),
        MarkerFilter::Impact => "impact".to_owned(),
        MarkerFilter::Tick(0) => "tick".to_owned(),
        MarkerFilter::Tick(n) => format!("tick {n}"),
        MarkerFilter::Expire => "expire".to_owned(),
        MarkerFilter::Package(k) => format!("package {k}"),
    }
}

fn action_text(a: &mantis_formats::presentation::Action) -> String {
    let what = match &a.op {
        ActionOp::SpawnEffect { effect, scale, .. } => format!("effect {effect} x{scale}"),
        ActionOp::PlaySound { sound, volume, .. } => format!("sound {sound} at {volume}"),
        ActionOp::CameraShake {
            amplitude, duration, ..
        } => {
            format!("shake {amplitude} for {duration} s")
        }
        ActionOp::AnimTrigger { parameter } => format!("anim trigger {parameter:08x}"),
    };
    let on = match a.anchor {
        Anchor::Source => "source",
        Anchor::Target => "target",
    };
    if a.delay > 0.0 {
        format!("{what} on {on} after {} s", a.delay)
    } else {
        format!("{what} on {on}")
    }
}

impl GraphBoard {
    /// Reads every graph, presentation, and effect source under `content` and checks
    /// them.
    ///
    /// # Errors
    /// [`GraphEditError::Io`], an unparseable source, or [`GraphEditError::Check`].
    pub fn open(content: &Path) -> Result<Self, GraphEditError> {
        let mut paths = Vec::new();
        walk(content, content, &mut paths);
        let mut sources = BTreeMap::new();
        let mut effects = BTreeMap::new();
        for rel in paths {
            let path = content.join(&rel);
            let bytes = std::fs::read(&path).map_err(|e| GraphEditError::Io(path.clone(), e))?;
            if rel.ends_with(PARTICLES) {
                effects.insert(rel, bytes);
            } else {
                let doc = SourceDoc::parse(&String::from_utf8_lossy(&bytes))
                    .map_err(|e| GraphEditError::Edit(rel.clone(), e))?;
                sources.insert(rel, doc);
            }
        }
        let mut board = Self {
            content: content.to_path_buf(),
            sources,
            effects,
            dirty: BTreeSet::new(),
            graphs: Vec::new(),
            presentations: Vec::new(),
            loaded: Vec::new(),
        };
        board.check()?;
        Ok(board)
    }

    /// The source texts, by path.
    pub fn source(&self, path: &str) -> Option<String> {
        self.sources.get(path).map(SourceDoc::text)
    }

    /// Sets `key` of `table` in source `path` (a node, a binding, or an action:
    /// `node.<name>`, `binding.<name>`, `action.<binding>.<n>`). Call
    /// [`GraphBoard::check`] to validate.
    ///
    /// # Errors
    /// [`GraphEditError::NoSource`] or [`GraphEditError::Edit`].
    pub fn set(&mut self, path: &str, table: &str, key: &str, value: &Value) -> Result<(), GraphEditError> {
        let doc = self
            .sources
            .get_mut(path)
            .ok_or_else(|| GraphEditError::NoSource(path.to_owned()))?;
        doc.set(table, key, value)
            .map_err(|e| GraphEditError::Edit(path.to_owned(), e))?;
        self.dirty.insert(path.to_owned());
        Ok(())
    }

    /// Cooks the current texts with the cook's importers and keeps the result for
    /// [`GraphBoard::graphs`]. On failure the last good result stays.
    ///
    /// # Errors
    /// [`GraphEditError::Check`] with every error located in its source.
    pub fn check(&mut self) -> Result<(), GraphEditError> {
        let mut tree = ContentTree::new();
        for (path, doc) in &self.sources {
            tree.insert(path, doc.text());
        }
        for (path, bytes) in &self.effects {
            tree.insert(path, bytes.clone());
        }
        let out = Cook::new(importers::builtin())
            .map_err(|e| GraphEditError::Check(vec![e]))?
            .run(&tree)
            .map_err(GraphEditError::Check)?;
        let mut graphs = Vec::new();
        let mut presentations = Vec::new();
        for a in out.assets.values() {
            let err = |what: String| GraphEditError::Check(vec![CookError::at(&a.source, 0, &what)]);
            match a.kind {
                AssetKind::GameplayGraph => graphs.push((
                    a.source.clone(),
                    GraphAsset::parse(&a.bytes).map_err(|e| err(e.to_string()))?,
                )),
                AssetKind::Presentation => presentations.push((
                    a.source.clone(),
                    PresentationGraph::parse(&a.bytes).map_err(|e| err(e.to_string()))?,
                )),
                _ => {}
            }
        }
        self.graphs = graphs;
        self.presentations = presentations;
        Ok(())
    }

    /// The name of node `key` in graph source `path`.
    fn node_name(&self, path: &str, key: u16) -> String {
        self.sources
            .get(path)
            .and_then(|doc| {
                doc.tables_under("node.").into_iter().find(|n| {
                    matches!(doc.get(&format!("node.{n}"), "key"), Some(Value::Int(k)) if *k == i64::from(key))
                })
            })
            .unwrap_or_else(|| format!("#{key}"))
    }

    /// The `[binding.<name>]` of presentation source `path` naming graph `graph`, node
    /// `node`.
    fn binding_name(&self, path: &str, graph: &str, node: u16, filter: MarkerFilter) -> String {
        let Some(doc) = self.sources.get(path) else {
            return String::new();
        };
        let wanted = filter_text(filter);
        doc.tables_under("binding.")
            .into_iter()
            .find(|name| {
                let t = format!("binding.{name}");
                let node_ok = matches!(doc.get(&t, "node"), Some(Value::Int(k)) if *k == i64::from(node));
                let graph_ok = doc.get(&t, "graph").and_then(Value::as_str) == Some(graph);
                let authored = doc.get(&t, "filter").and_then(Value::as_str).unwrap_or("any");
                node_ok && graph_ok && wanted.starts_with(authored)
            })
            .unwrap_or_default()
    }

    /// Every gameplay graph with its nodes and the bindings on each marker node, from
    /// the last good check.
    pub fn graphs(&self) -> Vec<GraphView> {
        self.graphs
            .iter()
            .map(|(path, g)| {
                let id = GraphId::named(&g.name).0;
                let nodes = g
                    .nodes
                    .iter()
                    .map(|n| {
                        let (kind, next, marker) = match n.kind {
                            GraphNodeKind::Marker { marker, offset, next } => {
                                let k = if offset > 0 {
                                    format!("{} at +{offset}", marker_text(marker))
                                } else {
                                    marker_text(marker)
                                };
                                (k, vec![next], true)
                            }
                            GraphNodeKind::Delay { ticks, next } => {
                                (format!("delay {ticks}"), vec![next], false)
                            }
                            GraphNodeKind::Action {
                                action,
                                target,
                                params,
                                next,
                            } => {
                                let name = g.actions.get(usize::from(action)).map_or("?", String::as_str);
                                let on = if target == Target::Target {
                                    "target"
                                } else {
                                    "source"
                                };
                                (format!("action {name} on {on} {params:?}"), vec![next], false)
                            }
                            GraphNodeKind::Chance {
                                numerator,
                                denominator,
                                then,
                                otherwise,
                            } => (
                                format!("chance {numerator}/{denominator}"),
                                vec![then, otherwise],
                                false,
                            ),
                            GraphNodeKind::Repeat {
                                counter,
                                times,
                                body,
                                done,
                            } => (
                                format!("repeat {times} on counter {counter}"),
                                vec![Some(body), done],
                                false,
                            ),
                        };
                        let bindings = self
                            .presentations
                            .iter()
                            .flat_map(|(source, p)| {
                                p.bindings
                                    .iter()
                                    .filter(|b| b.graph == id && b.node == n.key)
                                    .map(|b| BindingView {
                                        source: source.clone(),
                                        name: self.binding_name(source, &g.name, b.node, b.filter),
                                        filter: filter_text(b.filter),
                                        actions: b.actions.iter().map(action_text).collect(),
                                    })
                                    .collect::<Vec<_>>()
                            })
                            .collect();
                        NodeView {
                            key: n.key,
                            name: self.node_name(path, n.key),
                            kind,
                            next: next
                                .into_iter()
                                .flatten()
                                .map(|k| self.node_name(path, k))
                                .collect(),
                            marker,
                            bindings,
                        }
                    })
                    .collect();
                GraphView {
                    name: g.name.clone(),
                    source: path.clone(),
                    nodes,
                }
            })
            .collect()
    }

    /// `(graph name, node name)` of every marker node no binding reacts to.
    pub fn unbound_markers(&self) -> Vec<(String, String)> {
        self.graphs()
            .into_iter()
            .flat_map(|g| {
                g.nodes
                    .into_iter()
                    .filter(|n| n.marker && n.bindings.is_empty())
                    .map(move |n| (g.name.clone(), n.name))
            })
            .collect()
    }

    /// Hot-reloads the presentation graphs of the last good check into `library`: what
    /// the previous apply loaded is unloaded first, so other graphs (module views) stay.
    /// Returns the bindings loaded.
    pub fn apply(&mut self, library: &mut PresentationLibrary) -> usize {
        for old in self.loaded.drain(..) {
            let _ = library.unload(&old);
        }
        let mut count = 0;
        for (_, p) in &self.presentations {
            library.load(p);
            count += p.bindings.len();
            self.loaded.push(p.clone());
        }
        count
    }

    /// Writes the edited sources; returns their paths.
    ///
    /// # Errors
    /// [`GraphEditError::Io`].
    pub fn save(&mut self) -> Result<Vec<String>, GraphEditError> {
        let mut written = Vec::new();
        for path in core::mem::take(&mut self.dirty) {
            if let Some(doc) = self.sources.get(&path) {
                let full = self.content.join(&path);
                std::fs::write(&full, doc.text()).map_err(|e| GraphEditError::Io(full.clone(), e))?;
                written.push(path);
            }
        }
        Ok(written)
    }
}

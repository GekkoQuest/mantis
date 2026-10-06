//! Gameplay graphs (decision 0021): `*.graph.toml` to MGPH
//! (`mantis_formats::gameplay_graph`), gameplay domain, phase 5; and the package's
//! ability table, `tables/abilities`, phase 6 (it names cooked graphs).
//!
//! A graph names its nodes; links name other nodes; each node carries the stable `key`
//! that presentation bindings and marker ids use (never reuse a deleted key):
//!
//! ```toml
//! name = "pkg.ability.burn"    # stable name; the graph id is GraphId::named(name)
//! entry = "start"              # the first node
//!
//! [node.start]
//! key = 1
//! kind = "marker"              # marker | delay | action | chance | repeat
//! marker = "cast_start"        # cast_start | impact | tick | expire | package
//! # counter = 0                # tick: the repeat counter it reports
//! # package = 7                # package: the package marker kind
//! offset = 0                   # ticks ahead of now (default 0)
//! next = "burn"                # default: the end
//!
//! [node.burn]
//! key = 2
//! kind = "repeat"
//! counter = 0                  # below mantis_core::graph::MAX_COUNTERS
//! times = 3
//! body = "wait"
//! done = "end"
//!
//! [node.wait]
//! key = 3
//! kind = "delay"
//! ticks = 5                    # at least 1
//! next = "hit"
//!
//! [node.hit]
//! key = 4
//! kind = "action"
//! action = "pkg.damage"        # a graph action a module of the package declares
//! target = "target"            # source (default) | target
//! params = [10]                # up to 4 integers (the rest 0)
//! next = "burn"
//!
//! [node.end]
//! key = 5
//! kind = "marker"
//! marker = "expire"
//! ```
//!
//! `chance` nodes take `numerator`, `denominator` (nonzero), `then`, and `otherwise`.
//!
//! The cooked graph is checked with the core catalog's own rules
//! (`mantis_core::graph::GraphCatalog::insert`: unique keys, resolved links, nonzero
//! delays and denominators, counters in range, no zero-time cycle). When the cook knows
//! the graph actions the package's modules declare ([`Graphs::with_actions`]), every
//! action name must be one of them.
//!
//! `tables/abilities` maps ability ids to graphs, one `<ability id> <graph name>` row per
//! line with `#` comments; ids are distinct `u32`s and every name must be a graph cooked
//! in this package. It is copied into the gameplay bundle verbatim, like other tables.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use mantis_core::graph::{GraphCatalog, GraphError, MarkerSpec, NodeKey, PackageMarker, Target};
use mantis_formats::bundle::{AssetKind, Domain};
use mantis_formats::gameplay_graph::{GraphAsset, GraphNode, GraphNodeKind};

use crate::importer::{CookError, Cooked, ImportContext, Importer, Source};
use crate::source::{Doc, Fields, output_name};

const SUFFIX: &str = ".graph.toml";

/// The ability table's source path.
pub const ABILITIES: &str = "tables/abilities";

/// The gameplay graph importer.
#[derive(Clone, Debug, Default)]
pub struct Graphs {
    actions: Option<Arc<BTreeSet<String>>>,
}

impl Graphs {
    /// An importer that also checks every action name against `actions` (the union of
    /// the graph actions the package's modules declare).
    pub fn with_actions(actions: BTreeSet<String>) -> Self {
        Self {
            actions: Some(Arc::new(actions)),
        }
    }
}

/// A node as read, before keys are resolved.
struct Read<'d> {
    name: &'d str,
    key: u16,
    line: usize,
    fields: Fields<'d>,
}

fn link(f: &Fields<'_>, field: &str, keys: &BTreeMap<&str, u16>) -> Result<Option<u16>, CookError> {
    match f.opt_str(field)? {
        None => Ok(None),
        Some((name, line)) => keys.get(name).copied().map(Some).ok_or_else(|| {
            f.err(
                line,
                &format!("`{field}` = \"{name}\" names no node of this graph"),
            )
        }),
    }
}

fn marker(f: &Fields<'_>) -> Result<(MarkerSpec, Vec<&'static str>), CookError> {
    let specs = [
        ("cast_start", 0u8),
        ("impact", 1),
        ("tick", 2),
        ("expire", 3),
        ("package", 4),
    ];
    let (code, _) = f.choice("marker", &specs, None)?;
    Ok(match code {
        0 => (MarkerSpec::CastStart, vec![]),
        1 => (MarkerSpec::Impact, vec![]),
        2 => (
            MarkerSpec::Tick {
                counter: counter(f, f.int_or::<u8>("counter", 0)?)?,
            },
            vec!["counter"],
        ),
        3 => (MarkerSpec::Expire, vec![]),
        _ => (
            MarkerSpec::Package(PackageMarker(f.int::<u16>("package")?.0)),
            vec!["package"],
        ),
    })
}

impl Graphs {
    fn node(
        &self,
        r: &Read<'_>,
        keys: &BTreeMap<&str, u16>,
        actions: &mut Vec<String>,
    ) -> Result<GraphNodeKind, CookError> {
        let f = &r.fields;
        let kinds = [
            ("marker", 0u8),
            ("delay", 1),
            ("action", 2),
            ("chance", 3),
            ("repeat", 4),
        ];
        let (kind, _) = f.choice("kind", &kinds, None)?;
        let mut allowed = vec!["key", "kind"];
        let out = match kind {
            0 => {
                let (marker, extra) = marker(f)?;
                allowed.extend(["marker", "offset", "next"]);
                allowed.extend(extra);
                GraphNodeKind::Marker {
                    marker,
                    offset: f.int_or::<u16>("offset", 0)?.0,
                    next: link(f, "next", keys)?,
                }
            }
            1 => {
                allowed.extend(["ticks", "next"]);
                GraphNodeKind::Delay {
                    ticks: at_least_one(f, f.int::<u32>("ticks")?, "a delay must be at least 1 tick")?,
                    next: link(f, "next", keys)?,
                }
            }
            2 => {
                allowed.extend(["action", "target", "params", "next"]);
                let (name, line) = f.str("action")?;
                if let Some(declared) = &self.actions
                    && !declared.contains(name)
                {
                    return Err(f.err(
                        line,
                        &format!("`{name}` is not a graph action any module of this package declares"),
                    ));
                }
                let index = if let Some(i) = actions.iter().position(|a| a == name) {
                    i
                } else {
                    actions.push(name.to_owned());
                    actions.len() - 1
                };
                let targets = [("source", Target::Source), ("target", Target::Target)];
                let (target, _) = f.choice("target", &targets, Some(Target::Source))?;
                let mut params = [0i32; 4];
                if f.has("params") {
                    let (values, line) = f.ints::<i32>("params")?;
                    if values.len() > 4 {
                        return Err(f.err(line, "`params` holds at most 4 integers"));
                    }
                    for (p, v) in params.iter_mut().zip(values) {
                        *p = v;
                    }
                }
                GraphNodeKind::Action {
                    action: u16::try_from(index).map_err(|_| f.err(line, "too many actions"))?,
                    target,
                    params,
                    next: link(f, "next", keys)?,
                }
            }
            3 => {
                allowed.extend(["numerator", "denominator", "then", "otherwise"]);
                GraphNodeKind::Chance {
                    numerator: f.int::<u32>("numerator")?.0,
                    denominator: at_least_one(
                        f,
                        f.int::<u32>("denominator")?,
                        "a chance needs a nonzero denominator",
                    )?,
                    then: link(f, "then", keys)?,
                    otherwise: link(f, "otherwise", keys)?,
                }
            }
            _ => {
                allowed.extend(["counter", "times", "body", "done"]);
                let (body, line) = f.str("body")?;
                GraphNodeKind::Repeat {
                    counter: counter(f, f.int::<u8>("counter")?)?,
                    times: f.int::<u16>("times")?.0,
                    body: keys.get(body).copied().ok_or_else(|| {
                        f.err(line, &format!("`body` = \"{body}\" names no node of this graph"))
                    })?,
                    done: link(f, "done", keys)?,
                }
            }
        };
        f.only(&allowed)?;
        Ok(out)
    }
}

fn at_least_one(f: &Fields<'_>, (v, line): (u32, usize), message: &str) -> Result<u32, CookError> {
    if v == 0 { Err(f.err(line, message)) } else { Ok(v) }
}

fn counter(f: &Fields<'_>, (v, line): (u8, usize)) -> Result<u8, CookError> {
    if usize::from(v) < mantis_core::graph::MAX_COUNTERS {
        Ok(v)
    } else {
        Err(f.err(
            line,
            &format!(
                "the counter is out of range (0 to {})",
                mantis_core::graph::MAX_COUNTERS - 1
            ),
        ))
    }
}

/// The line of the node with `key`, for errors the core catalog reports by key.
fn line_of_key(read: &[Read<'_>], key: NodeKey) -> usize {
    read.iter().find(|r| r.key == key.0).map_or(0, |r| r.line)
}

fn catalog_error(doc: &Doc<'_>, read: &[Read<'_>], e: GraphError) -> CookError {
    let at = |k: NodeKey, what: &str| {
        let name = read.iter().find(|r| r.key == k.0).map_or("?", |r| r.name);
        doc.err(
            line_of_key(read, k),
            &format!("node `{name}` (key {}): {what}", k.0),
        )
    };
    match e {
        GraphError::DuplicateNode(k) => at(k, "another node has this key"),
        GraphError::UnknownNode(k) => doc.err(0, &format!("a link names key {}, which no node has", k.0)),
        GraphError::ZeroDelay(k) => at(k, "a delay must be at least 1 tick"),
        GraphError::ZeroDenominator(k) => at(k, "a chance needs a nonzero denominator"),
        GraphError::BadCounter(k) => at(k, "the counter is out of range"),
        GraphError::ZeroTimeCycle(k) => at(
            k,
            "this node is on a cycle that never passes a delay (it could spin forever in one tick)",
        ),
        other => doc.err(0, &format!("the graph is refused: {other}")),
    }
}

impl Importer for Graphs {
    fn name(&self) -> &'static str {
        "graph.toml"
    }

    fn version(&self) -> u32 {
        1
    }

    fn phase(&self) -> u32 {
        5
    }

    fn accepts(&self, path: &str) -> bool {
        path.ends_with(SUFFIX) && path.len() > SUFFIX.len() && !path.ends_with(&format!("/{SUFFIX}"))
    }

    fn import(&self, source: &Source<'_>, _ctx: &ImportContext<'_>) -> Result<Vec<Cooked>, CookError> {
        let doc = Doc::from_source(source)?;
        doc.only_tables(&[], &["node"])?;
        let root = doc.root();
        root.only(&["name", "entry"])?;
        let (name, name_line) = root.str("name")?;
        if name.is_empty() || name.len() > mantis_formats::gameplay_graph::MAX_NAME {
            return Err(doc.err(name_line, "`name` must be 1 to 255 bytes"));
        }
        let mut read = Vec::new();
        let mut keys = BTreeMap::new();
        for (node, fields) in doc.items("node") {
            let (key, line) = fields.int::<u16>("key")?;
            if key == mantis_formats::gameplay_graph::NO_NODE {
                return Err(fields.err(line, "key 65535 is reserved"));
            }
            if let Some(other) = read.iter().find(|r: &&Read<'_>| r.key == key) {
                return Err(fields.err(line, &format!("key {key} is also node `{}`'s", other.name)));
            }
            keys.insert(node, key);
            read.push(Read {
                name: node,
                key,
                line: fields.line(),
                fields,
            });
        }
        if read.is_empty() {
            return Err(doc.err(0, "a graph needs at least one `[node.<name>]`"));
        }
        let (entry, entry_line) = root.str("entry")?;
        let entry = *keys
            .get(entry)
            .ok_or_else(|| doc.err(entry_line, &format!("`entry` = \"{entry}\" names no node")))?;
        let mut actions = Vec::new();
        let mut nodes = Vec::with_capacity(read.len());
        for r in &read {
            nodes.push(GraphNode {
                key: r.key,
                kind: self.node(r, &keys, &mut actions)?,
            });
        }
        nodes.sort_by_key(|n| n.key);
        let asset = GraphAsset {
            name: name.to_owned(),
            entry,
            actions,
            nodes,
        };
        // The core catalog's rules, exactly as a cell applies them at load. Action ids
        // are registered here in the asset's name order; a cell resolves the names to
        // its own ids.
        let mut catalog = GraphCatalog::new();
        for a in &asset.actions {
            let _ = catalog
                .register_action(a.clone())
                .map_err(|e| doc.err(0, &e.to_string()))?;
        }
        let core = asset
            .to_core(|n| catalog.action_id(n))
            .map_err(|e| doc.err(0, &format!("the graph does not load: {e}")))?;
        catalog.insert(core).map_err(|e| catalog_error(&doc, &read, e))?;
        let bytes = asset.encode();
        GraphAsset::parse(&bytes)
            .map_err(|e| doc.err(0, &format!("the cooked graph fails its runtime parser: {e}")))?;
        Ok(vec![Cooked {
            name: output_name(source.path, SUFFIX, ".graph"),
            kind: AssetKind::GameplayGraph,
            domain: Domain::Gameplay,
            bytes,
        }])
    }
}

/// Every gameplay graph cooked so far, by stable name.
pub(crate) fn cooked_graphs(ctx: &ImportContext<'_>) -> BTreeMap<String, GraphAsset> {
    ctx.cooked_of(AssetKind::GameplayGraph)
        .filter_map(|(_, bytes)| GraphAsset::parse(bytes).ok())
        .map(|g| (g.name.clone(), g))
        .collect()
}

/// The ability table importer.
#[derive(Clone, Copy, Debug, Default)]
pub struct Abilities;

impl Importer for Abilities {
    fn name(&self) -> &'static str {
        "table.abilities"
    }

    fn version(&self) -> u32 {
        1
    }

    fn phase(&self) -> u32 {
        6
    }

    fn accepts(&self, path: &str) -> bool {
        path == ABILITIES
    }

    fn import(&self, source: &Source<'_>, ctx: &ImportContext<'_>) -> Result<Vec<Cooked>, CookError> {
        let graphs = cooked_graphs(ctx);
        let mut seen = BTreeSet::new();
        for (n, line) in source.text()?.lines().enumerate() {
            let row = line.split('#').next().unwrap_or("").trim();
            if row.is_empty() {
                continue;
            }
            let at = |what: &str| CookError::at(source.path, n + 1, what);
            let cols: Vec<&str> = row.split_whitespace().collect();
            let [id, graph] = cols.as_slice() else {
                return Err(at("a row is `<ability id> <graph name>`"));
            };
            let id: u32 = id
                .parse()
                .map_err(|_| at(&format!("`{id}` is not an ability id (a u32)")))?;
            if !seen.insert(id) {
                return Err(at(&format!("ability {id} is listed twice")));
            }
            if !graphs.contains_key(*graph) {
                return Err(at(&format!(
                    "`{graph}` is not a gameplay graph cooked in this package"
                )));
            }
        }
        Ok(vec![Cooked {
            name: source.path.to_owned(),
            kind: AssetKind::Table,
            domain: Domain::Gameplay,
            bytes: source.bytes.to_vec(),
        }])
    }
}

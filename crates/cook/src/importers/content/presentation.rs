//! Presentation graphs: `*.presentation.toml` to MPRS (`mantis_formats::presentation`),
//! phase 10.
//!
//! A binding names a gameplay marker, `(graph, node)`, and a kind filter; its actions
//! are `[action.<binding>.<n>]` tables, `n` counting from 0 in play order. Effects are
//! named by the **source path** of a particle effect (cooked in phase 5, resolved to its
//! content hash); sounds by their id in the package's sound bank; animation triggers by
//! parameter name (hashed as the animation graph format hashes names).
//!
//! ```toml
//! [binding.cast]
//! graph = "ability.bolt"      # gameplay graph name (GraphId::named)
//! node = 1                    # node key in that graph
//! filter = "cast_start"       # any | cast_start | impact | tick | expire | package (default any)
//! # tick = 0                  # filter "tick": tick number, 0 every tick (default 0)
//! # kind = 3                  # filter "package": the package marker kind (required)
//!
//! [action.cast.0]
//! op = "spawn_effect"
//! effect = "effects/spark.particles.toml"
//! follow = true               # default false
//! # scale = 1.0               # uniform effect scale, > 0 and at most 100 (default 1)
//! delay = 0.0                 # seconds after the marker, 0 to 60 (default 0)
//! anchor = "source"           # source | target (default source)
//! offset = [0.0, 1.0, 0.0]    # meters from the anchor, world axes (default zero)
//!
//! [action.cast.1]
//! op = "play_sound"
//! sound = 42                  # sound id in the sound bank
//! volume = 0.8                # 0 to 4 (default 1)
//! pitch = 1.0                 # 0.25 to 4 (default 1)
//!
//! [action.cast.2]
//! op = "camera_shake"
//! amplitude = 1.5             # degrees, (0, 10]
//! frequency = 12.0            # hertz, (0, 100]
//! duration = 0.4              # seconds, (0, 10]
//! radius = 20.0               # meters, > 0
//!
//! [action.cast.3]
//! op = "anim_trigger"
//! parameter = "attack"        # trigger parameter name
//! ```
//!
//! Every binding needs 1 to 16 actions; two bindings on the same marker and filter are
//! refused. Output: `<stem>.prs`, kind `Presentation`, presentation domain.

use std::collections::BTreeMap;

use mantis_core::graph::{GraphId, MarkerSpec};
use mantis_formats::bundle::{AssetKind, Domain};
use mantis_formats::gameplay_graph::{GraphAsset, GraphNodeKind};
use mantis_formats::presentation::{
    Action, ActionOp, Anchor, Binding, MAX_ACTIONS, MAX_DELAY, MarkerFilter, PresentationGraph,
};
use mantis_formats::skeleton::bone_name_hash;

use super::fields::{Doc, Fields, output_name, within};
use crate::importer::{CookError, Cooked, ImportContext, Importer, Source};

const SUFFIX: &str = ".presentation.toml";

/// The presentation graph importer.
#[derive(Clone, Copy, Debug, Default)]
pub struct Presentations;

#[derive(Clone, Copy, PartialEq, Eq)]
enum OpKind {
    Effect,
    Sound,
    Shake,
    Trigger,
}

const COMMON: &[&str] = &["op", "delay", "anchor", "offset"];

fn binding_header(f: &Fields<'_>) -> Result<(u32, u16, MarkerFilter), CookError> {
    let (graph, _) = f.str("graph")?;
    let (node, _) = f.int::<u16>("node")?;
    let filters = [
        ("any", 0u8),
        ("cast_start", 1),
        ("impact", 2),
        ("tick", 3),
        ("expire", 4),
        ("package", 5),
    ];
    let (code, _) = f.choice("filter", &filters, Some(0))?;
    let mut allowed = vec!["graph", "node", "filter"];
    let filter = match code {
        0 => MarkerFilter::Any,
        1 => MarkerFilter::CastStart,
        2 => MarkerFilter::Impact,
        3 => {
            allowed.push("tick");
            MarkerFilter::Tick(f.opt_int::<u16>("tick", 0)?.0)
        }
        4 => MarkerFilter::Expire,
        _ => {
            allowed.push("kind");
            MarkerFilter::Package(f.int::<u16>("kind")?.0)
        }
    };
    f.only(&allowed)?;
    Ok((GraphId::named(graph).0, node, filter))
}

/// A binding must name a marker node of a gameplay graph cooked in this package
/// (decision 0021), and a kind filter must match what that node emits.
fn check_marker(
    f: &Fields<'_>,
    graphs: &BTreeMap<String, GraphAsset>,
    filter: MarkerFilter,
) -> Result<(), CookError> {
    let (name, graph_line) = f.str("graph")?;
    let (node, node_line) = f.int::<u16>("node")?;
    let graph = graphs.get(name).ok_or_else(|| {
        f.err(
            graph_line,
            &format!("`{name}` is not a gameplay graph cooked in this package"),
        )
    })?;
    let Some(GraphNodeKind::Marker { marker, .. }) = graph.node(node).map(|n| n.kind) else {
        let markers: Vec<String> = graph.marker_keys().map(|k| k.to_string()).collect();
        return Err(f.err(
            node_line,
            &format!(
                "node {node} of `{name}` is not a marker node (its marker nodes: {})",
                markers.join(", ")
            ),
        ));
    };
    let matches = match (filter, marker) {
        (MarkerFilter::Any, _)
        | (MarkerFilter::CastStart, MarkerSpec::CastStart)
        | (MarkerFilter::Impact, MarkerSpec::Impact)
        | (MarkerFilter::Tick(_), MarkerSpec::Tick { .. })
        | (MarkerFilter::Expire, MarkerSpec::Expire) => true,
        (MarkerFilter::Package(k), MarkerSpec::Package(p)) => k == p.0,
        _ => false,
    };
    if matches {
        Ok(())
    } else {
        Err(f.err(
            f.line_of("filter"),
            &format!("node {node} of `{name}` emits {marker:?}, which this filter never matches"),
        ))
    }
}

/// The sort key the format orders bindings by: graph, node, filter code, argument.
fn sort_key(graph: u32, node: u16, filter: MarkerFilter) -> (u32, u16, u8, u16) {
    let (code, arg) = match filter {
        MarkerFilter::Any => (0, 0),
        MarkerFilter::CastStart => (1, 0),
        MarkerFilter::Impact => (2, 0),
        MarkerFilter::Tick(n) => (3, n),
        MarkerFilter::Expire => (4, 0),
        MarkerFilter::Package(p) => (5, p),
    };
    (graph, node, code, arg)
}

fn in_range(
    f: &Fields<'_>,
    key: &str,
    default: Option<f32>,
    lo: f32,
    hi: f32,
    open_low: bool,
) -> Result<f32, CookError> {
    let (v, line) = match default {
        Some(d) => f.opt_f32(key, d)?,
        None => f.f32(key)?,
    };
    let ok = if open_low {
        v > lo && v <= hi
    } else {
        within(v, lo, hi)
    };
    if ok {
        Ok(v)
    } else {
        let low = if open_low { "(" } else { "[" };
        Err(f.err(line, &format!("`{key}` = {v} is outside {low}{lo}, {hi}]")))
    }
}

fn action(f: &Fields<'_>, ctx: &ImportContext<'_>, from: &str) -> Result<Action, CookError> {
    let ops = [
        ("spawn_effect", OpKind::Effect),
        ("play_sound", OpKind::Sound),
        ("camera_shake", OpKind::Shake),
        ("anim_trigger", OpKind::Trigger),
    ];
    let (kind, _) = f.choice("op", &ops, None)?;
    let extra: &[&str] = match kind {
        OpKind::Effect => &["effect", "scale", "follow"],
        OpKind::Sound => &["sound", "volume", "pitch", "follow"],
        OpKind::Shake => &["amplitude", "frequency", "duration", "radius"],
        OpKind::Trigger => &["parameter"],
    };
    let mut allowed = COMMON.to_vec();
    allowed.extend_from_slice(extra);
    f.only(&allowed)?;
    let delay = in_range(f, "delay", Some(0.0), 0.0, MAX_DELAY, false)?;
    let (anchor, _) = f.choice(
        "anchor",
        &[("source", Anchor::Source), ("target", Anchor::Target)],
        Some(Anchor::Source),
    )?;
    let (offset, _) = f.opt_array::<3>("offset", [0.0; 3])?;
    let op = match kind {
        OpKind::Effect => {
            let (path, line) = f.str("effect")?;
            let effect = ctx.resolve(path, AssetKind::ParticleEffect, from, line)?;
            let (scale, sl) = f.opt_f32("scale", 1.0)?;
            if !(scale > 0.0 && scale <= mantis_formats::presentation::MAX_EFFECT_SCALE) {
                return Err(f.err(
                    sl,
                    &format!(
                        "`scale` = {scale} is outside (0, {}]",
                        mantis_formats::presentation::MAX_EFFECT_SCALE
                    ),
                ));
            }
            ActionOp::SpawnEffect {
                effect,
                scale,
                follow: f.opt_bool("follow", false)?.0,
            }
        }
        OpKind::Sound => ActionOp::PlaySound {
            sound: f.int::<u32>("sound")?.0,
            volume: in_range(f, "volume", Some(1.0), 0.0, 4.0, false)?,
            pitch: in_range(f, "pitch", Some(1.0), 0.25, 4.0, false)?,
            follow: f.opt_bool("follow", false)?.0,
        },
        OpKind::Shake => ActionOp::CameraShake {
            amplitude: in_range(f, "amplitude", None, 0.0, 10.0, true)?,
            frequency: in_range(f, "frequency", None, 0.0, 100.0, true)?,
            duration: in_range(f, "duration", None, 0.0, 10.0, true)?,
            radius: in_range(f, "radius", None, 0.0, f32::MAX, true)?,
        },
        OpKind::Trigger => {
            let (name, line) = f.str("parameter")?;
            if name.is_empty() {
                return Err(f.err(line, "`parameter` is empty"));
            }
            ActionOp::AnimTrigger {
                parameter: bone_name_hash(name),
            }
        }
    };
    Ok(Action {
        delay,
        anchor,
        offset,
        op,
    })
}

struct Pending<'d> {
    fields: Fields<'d>,
    actions: BTreeMap<u32, (usize, Action)>,
}

fn collect_actions<'d>(
    doc: &'d Doc<'_>,
    pending: &mut BTreeMap<&'d str, Pending<'d>>,
    ctx: &ImportContext<'_>,
) -> Result<(), CookError> {
    for (rest, f) in doc.prefixed("action.") {
        let (binding, n) = rest.rsplit_once('.').ok_or_else(|| {
            f.err(
                f.line,
                &format!("`[action.{rest}]`: expected `[action.<binding>.<n>]`"),
            )
        })?;
        let n = n.parse::<u32>().map_err(|_| {
            f.err(
                f.line,
                &format!("`[action.{rest}]`: `{n}` is not an action number"),
            )
        })?;
        let p = pending.get_mut(binding).ok_or_else(|| {
            f.err(
                f.line,
                &format!("`[action.{rest}]` names unknown binding `{binding}`"),
            )
        })?;
        let a = action(&f, ctx, doc.path())?;
        p.actions.insert(n, (f.line, a));
    }
    Ok(())
}

impl Importer for Presentations {
    fn name(&self) -> &'static str {
        "presentation.toml"
    }

    fn version(&self) -> u32 {
        1
    }

    fn phase(&self) -> u32 {
        10
    }

    fn accepts(&self, path: &str) -> bool {
        path.ends_with(SUFFIX)
    }

    fn import(&self, source: &Source<'_>, ctx: &ImportContext<'_>) -> Result<Vec<Cooked>, CookError> {
        let doc = Doc::parse(source)?;
        doc.only_tables(&[], &["binding.", "action."])?;
        if let Some(root) = doc.root() {
            root.only(&[])?;
        }
        let mut pending: BTreeMap<&str, Pending<'_>> = BTreeMap::new();
        for (name, f) in doc.prefixed("binding.") {
            if name.contains('.') {
                return Err(f.err(f.line, &format!("binding name `{name}` may not contain `.`")));
            }
            pending.insert(
                name,
                Pending {
                    fields: f,
                    actions: BTreeMap::new(),
                },
            );
        }
        collect_actions(&doc, &mut pending, ctx)?;
        let graphs = super::gameplay::cooked_graphs(ctx);

        // Bindings sorted by marker and filter, each with its actions in number order.
        let mut sorted: BTreeMap<(u32, u16, u8, u16), (&str, Binding)> = BTreeMap::new();
        let mut by_line: Vec<(&str, &Pending<'_>)> = pending.iter().map(|(k, v)| (*k, v)).collect();
        by_line.sort_by_key(|(_, p)| p.fields.line);
        for (name, p) in by_line {
            let f = &p.fields;
            let (graph, node, filter) = binding_header(f)?;
            check_marker(f, &graphs, filter)?;
            if p.actions.is_empty() || p.actions.len() > MAX_ACTIONS {
                return Err(f.err(
                    f.line,
                    &format!(
                        "binding `{name}` has {} actions; a binding has 1 to {MAX_ACTIONS} (`[action.{name}.0]`, ...)",
                        p.actions.len()
                    ),
                ));
            }
            let mut actions = Vec::with_capacity(p.actions.len());
            for (expected, (n, (line, a))) in (0u32..).zip(&p.actions) {
                if *n != expected {
                    return Err(f.err(
                        *line,
                        &format!(
                            "action numbers of `{name}` must run 0, 1, ... without gaps (expected {expected})"
                        ),
                    ));
                }
                actions.push(*a);
            }
            let key = sort_key(graph, node, filter);
            if let Some((other, _)) = sorted.get(&key) {
                return Err(f.err(
                    f.line,
                    &format!("binding `{name}` repeats the marker and filter of binding `{other}`"),
                ));
            }
            sorted.insert(
                key,
                (
                    name,
                    Binding {
                        graph,
                        node,
                        filter,
                        actions,
                    },
                ),
            );
        }
        let graph = PresentationGraph {
            bindings: sorted.into_values().map(|(_, b)| b).collect(),
        };
        let bytes = graph.encode();
        PresentationGraph::parse(&bytes).map_err(|e| {
            doc.err(
                0,
                &format!("cooked presentation graph fails its runtime parser: {e}"),
            )
        })?;
        Ok(vec![Cooked {
            name: output_name(source.path, SUFFIX, ".prs"),
            kind: AssetKind::Presentation,
            domain: Domain::Presentation,
            bytes,
        }])
    }
}

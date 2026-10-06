//! The animation graph importer: `*.animgraph.toml` to MAGR, bound with
//! [`mantis_anim::AnimGraph::new`] against the resolved skeleton and clips.

use std::collections::BTreeMap;
use std::sync::Arc;

use mantis_anim::{AnimError, AnimGraph, Clip, Skeleton};
use mantis_core::content::ContentHash;
use mantis_formats::anim_clip::ClipAsset;
use mantis_formats::anim_graph::{
    BlendChildDef, CompareOp, ConditionDef, GraphAsset, LayerDef, LayerMode, LookAtDef, NodeDef,
    ParameterDef, ParameterKind, StateMachineDef, TransitionDef, TwoBoneChainDef,
};
use mantis_formats::bundle::{AssetKind, Domain};
use mantis_formats::skeleton::{SkeletonAsset, bone_name_hash};

use super::skeleton::{bone_index, resolve_skeleton};
use crate::importer::{CookError, Cooked, ImportContext, Importer, Source};
use crate::source::{Doc, Fields, has_suffix, output_name};

/// `*.animgraph.toml` to an animation graph (phase 15: it resolves phase-10 clips).
#[derive(Clone, Copy, Debug, Default)]
pub struct GraphImporter;

impl Importer for GraphImporter {
    fn name(&self) -> &'static str {
        "animgraph.toml"
    }

    fn version(&self) -> u32 {
        1
    }

    fn phase(&self) -> u32 {
        15
    }

    fn accepts(&self, path: &str) -> bool {
        has_suffix(path, ".animgraph.toml")
    }

    fn import(&self, source: &Source<'_>, ctx: &ImportContext<'_>) -> Result<Vec<Cooked>, CookError> {
        let doc = Doc::from_source(source)?;
        doc.only_tables(
            &["look_at"],
            &["clip", "parameter", "node", "transition", "layer", "foot"],
        )?;
        let root = doc.root();
        root.only(&["skeleton"])?;
        let skeleton = resolve_skeleton(ctx, &root, source.path, "skeleton")?;
        let mut b = Builder {
            doc: &doc,
            skeleton: &skeleton,
            clip_names: Vec::new(),
            clip_hashes: Vec::new(),
            clips: BTreeMap::new(),
            parameters: Vec::new(),
            node_names: Vec::new(),
            referenced: Vec::new(),
        };
        b.clips(ctx)?;
        b.parameters()?;
        let nodes = b.nodes()?;
        let layers = b.layers()?;
        b.check_structure(&nodes)?;
        let foot_chains = b.foot_chains()?;
        let look_at = b.look_at()?;
        let asset = GraphAsset {
            bone_count: u32::try_from(skeleton.bones.len()).unwrap_or(u32::MAX),
            clips: b.clip_hashes.clone(),
            parameters: b.parameters.iter().map(|(_, p)| *p).collect(),
            nodes,
            layers,
            foot_chains,
            look_at,
        };
        let bytes = asset.encode();
        let parsed = GraphAsset::parse(&bytes)
            .map_err(|e| CookError::at(source.path, 0, &format!("the cooked graph does not load: {e}")))?;
        let bound = Skeleton::new(&skeleton).map_err(|e| {
            root.err(
                root.line_of("skeleton"),
                &format!("the skeleton does not bind: {e}"),
            )
        })?;
        AnimGraph::new(Arc::new(bound), &parsed, |h| b.clips.get(h).cloned()).map_err(|e| match e {
            AnimError::IkChain(i) => {
                let table = doc.items("foot").nth(i).map(|(_, f)| f);
                let line = table.map_or(0, |f| f.line());
                CookError::at(
                    source.path,
                    line,
                    "the foot chain is not an ancestor line (root above mid above tip)",
                )
            }
            other => CookError::at(source.path, 0, &format!("the graph does not bind: {other}")),
        })?;
        Ok(vec![Cooked {
            name: output_name(source.path, ".animgraph.toml", ".animgraph"),
            kind: AssetKind::AnimGraph,
            domain: Domain::Presentation,
            bytes,
        }])
    }
}

struct Builder<'a> {
    doc: &'a Doc<'a>,
    skeleton: &'a SkeletonAsset,
    /// Clip name to clip index.
    clip_names: Vec<(&'a str, u32)>,
    clip_hashes: Vec<ContentHash>,
    clips: BTreeMap<ContentHash, Arc<Clip>>,
    parameters: Vec<(&'a str, ParameterDef)>,
    /// Node names with their header lines, in pool order.
    node_names: Vec<(&'a str, usize)>,
    /// Whether each node is referenced yet.
    referenced: Vec<bool>,
}

fn u32_of(i: usize) -> u32 {
    u32::try_from(i).unwrap_or(u32::MAX)
}

impl Builder<'_> {
    fn clips(&mut self, ctx: &ImportContext<'_>) -> Result<(), CookError> {
        let bones = self.skeleton.bones.len();
        for (name, f) in self.doc.items("clip") {
            f.only(&["path"])?;
            let path = f.str("path")?.0;
            let line = f.line_of("path");
            let (hash, bytes) = ctx.resolve_bytes(path, AssetKind::AnimClip, self.doc.path(), line)?;
            let asset = ClipAsset::parse(bytes).map_err(|e| f.err(line, &format!("`{path}`: {e}")))?;
            if asset.bone_count as usize != bones {
                return Err(f.err(
                    line,
                    &format!(
                        "`{path}` targets {} bones; the graph's skeleton has {bones}",
                        asset.bone_count
                    ),
                ));
            }
            let clip = Clip::new(&asset).map_err(|e| f.err(line, &format!("`{path}`: {e}")))?;
            let index = if let Some(i) = self.clip_hashes.iter().position(|h| *h == hash) {
                u32_of(i)
            } else {
                self.clip_hashes.push(hash);
                self.clips.insert(hash, Arc::new(clip));
                u32_of(self.clip_hashes.len() - 1)
            };
            self.clip_names.push((name, index));
        }
        Ok(())
    }

    fn clip(&self, f: &Fields<'_>, key: &str) -> Result<u32, CookError> {
        let name = f.str(key)?.0;
        self.clip_names
            .iter()
            .find(|(n, _)| *n == name)
            .map(|(_, i)| *i)
            .ok_or_else(|| f.err(f.line_of(key), &format!("unknown clip `{name}`")))
    }

    fn parameters(&mut self) -> Result<(), CookError> {
        for (name, f) in self.doc.items("parameter") {
            f.only(&["kind", "default"])?;
            let kind_line = f.line_of("kind");
            let (kind, default) = match f.opt_str("kind")?.map_or("float", |(s, _)| s) {
                "float" => (ParameterKind::Float, f.f32_or("default", 0.0)?.0),
                "bool" => {
                    let v = f.bool_or("default", false)?.0;
                    (ParameterKind::Bool, if v { 1.0 } else { 0.0 })
                }
                "trigger" => {
                    if f.has("default") {
                        return Err(f.err(f.line_of("default"), "a trigger has no default"));
                    }
                    (ParameterKind::Trigger, 0.0)
                }
                other => {
                    return Err(f.err(
                        kind_line,
                        &format!("unknown parameter kind `{other}` (float, bool, or trigger)"),
                    ));
                }
            };
            let name_hash = bone_name_hash(name);
            if let Some((other, _)) = self.parameters.iter().find(|(_, p)| p.name_hash == name_hash) {
                return Err(f.err(
                    f.line(),
                    &format!("parameter `{name}` has the same name hash as `{other}`; rename one"),
                ));
            }
            self.parameters.push((
                name,
                ParameterDef {
                    name_hash,
                    kind,
                    default,
                },
            ));
        }
        Ok(())
    }

    fn parameter(&self, f: &Fields<'_>, line: usize, name: &str) -> Result<(u32, ParameterKind), CookError> {
        self.parameters
            .iter()
            .position(|(n, _)| *n == name)
            .and_then(|i| self.parameters.get(i).map(|(_, p)| (u32_of(i), p.kind)))
            .ok_or_else(|| f.err(line, &format!("unknown parameter `{name}`")))
    }

    /// Resolves a node reference and marks it used; a second use is an error.
    fn reference(&mut self, f: &Fields<'_>, line: usize, name: &str) -> Result<u32, CookError> {
        let index = self
            .node_names
            .iter()
            .position(|(n, _)| *n == name)
            .ok_or_else(|| f.err(line, &format!("unknown node `{name}`")))?;
        match self.referenced.get_mut(index) {
            Some(r) if *r => Err(f.err(
                line,
                &format!("node `{name}` is used twice (a node has exactly one parent node or layer)"),
            )),
            Some(r) => {
                *r = true;
                Ok(u32_of(index))
            }
            None => Err(f.err(line, &format!("unknown node `{name}`"))),
        }
    }

    fn nodes(&mut self) -> Result<Vec<NodeDef>, CookError> {
        self.node_names = self.doc.items("node").map(|(n, f)| (n, f.line())).collect();
        self.referenced = vec![false; self.node_names.len()];
        for (machine, f) in self.doc.items("transition") {
            let Some((machine, _)) = machine.split_once('.') else {
                return Err(f.err(f.line(), "a transition table is `[transition.<machine>.<name>]`"));
            };
            let is_machine = self.doc.items("node").any(|(n, nf)| {
                n == machine && nf.opt_str("kind").ok().flatten().map(|(k, _)| k) == Some("state_machine")
            });
            if !is_machine {
                return Err(f.err(f.line(), &format!("`{machine}` is not a state machine node")));
            }
        }
        let doc = self.doc;
        let mut nodes = Vec::new();
        for (name, f) in doc.items("node") {
            let kind = f.str("kind")?.0;
            let node = match kind {
                "clip" => {
                    f.only(&["kind", "clip", "speed"])?;
                    NodeDef::Clip {
                        clip: self.clip(&f, "clip")?,
                        speed: f.f32_or("speed", 1.0)?.0,
                    }
                }
                "blend1d" => self.blend(&f)?,
                "state_machine" => NodeDef::StateMachine(self.state_machine(name, &f)?),
                other => {
                    return Err(f.err(
                        f.line_of("kind"),
                        &format!("unknown node kind `{other}` (clip, blend1d, or state_machine)"),
                    ));
                }
            };
            nodes.push(node);
        }
        if nodes.is_empty() {
            return Err(CookError::at(self.doc.path(), 0, "no `[node.<name>]` tables"));
        }
        Ok(nodes)
    }

    fn blend(&mut self, f: &Fields<'_>) -> Result<NodeDef, CookError> {
        f.only(&["kind", "parameter", "children", "thresholds"])?;
        let line = f.line_of("parameter");
        let (parameter, kind) = self.parameter(f, line, f.str("parameter")?.0)?;
        if kind != ParameterKind::Float {
            return Err(f.err(line, "a blend parameter must be a float"));
        }
        let children = f
            .strs("children")?
            .0
            .into_iter()
            .map(str::to_owned)
            .collect::<Vec<String>>();
        let thresholds = f.f32s("thresholds")?.0;
        let thresholds_line = f.line_of("thresholds");
        if children.is_empty() || thresholds.len() != children.len() {
            return Err(f.err(
                thresholds_line,
                &format!("{} thresholds for {} children", thresholds.len(), children.len()),
            ));
        }
        if thresholds.windows(2).any(|w| matches!(w, [a, b] if a >= b)) {
            return Err(f.err(thresholds_line, "thresholds must increase strictly"));
        }
        let children_line = f.line_of("children");
        let mut defs = Vec::with_capacity(children.len());
        for (child, threshold) in children.iter().zip(thresholds) {
            defs.push(BlendChildDef {
                node: self.reference(f, children_line, child)?,
                threshold,
            });
        }
        Ok(NodeDef::Blend1D {
            parameter,
            children: defs,
        })
    }

    fn state_machine(&mut self, name: &str, f: &Fields<'_>) -> Result<StateMachineDef, CookError> {
        f.only(&["kind", "states", "entry"])?;
        let state_names = f
            .strs("states")?
            .0
            .into_iter()
            .map(str::to_owned)
            .collect::<Vec<String>>();
        let states_line = f.line_of("states");
        if state_names.is_empty() {
            return Err(f.err(states_line, "a state machine needs at least one state"));
        }
        let mut states = Vec::with_capacity(state_names.len());
        for s in &state_names {
            states.push(self.reference(f, states_line, s)?);
        }
        let state_index = |f: &Fields<'_>, key: &str, s: &str| {
            state_names
                .iter()
                .position(|n| n == s)
                .map(u32_of)
                .ok_or_else(|| f.err(f.line_of(key), &format!("`{s}` is not a state of `{name}`")))
        };
        let entry = match f.opt_str("entry")?.map(|(s, _)| s) {
            Some(e) => state_index(f, "entry", e)?,
            None => 0,
        };
        let mut transitions = Vec::new();
        for (rest, t) in self.doc.items("transition") {
            if rest.split_once('.').map(|(m, _)| m) != Some(name) {
                continue;
            }
            t.only(&["from", "to", "crossfade", "exit_time", "conditions"])?;
            let from = match t.str("from")?.0 {
                "any" => None,
                s => Some(state_index(&t, "from", s)?),
            };
            let to = state_index(&t, "to", t.str("to")?.0)?;
            let crossfade = t.f32_or("crossfade", 0.0)?.0;
            if crossfade < 0.0 {
                return Err(t.err(t.line_of("crossfade"), "`crossfade` must not be negative"));
            }
            let exit_time = t.opt_f32("exit_time")?.map(|(v, _)| v);
            if exit_time.is_some_and(|e| e < 0.0) {
                return Err(t.err(t.line_of("exit_time"), "`exit_time` must not be negative"));
            }
            let line = t.line_of("conditions");
            let conditions = t
                .strs_or_empty("conditions")?
                .0
                .into_iter()
                .map(str::to_owned)
                .collect::<Vec<String>>()
                .iter()
                .map(|c| self.condition(&t, line, c))
                .collect::<Result<Vec<_>, _>>()?;
            if exit_time.is_none() && conditions.is_empty() {
                return Err(t.err(t.line(), "a transition needs `exit_time` or `conditions`"));
            }
            transitions.push(TransitionDef {
                from,
                to,
                crossfade,
                exit_time,
                conditions,
            });
        }
        Ok(StateMachineDef {
            states,
            entry,
            transitions,
        })
    }

    /// `"<parameter> <op> <value>"` (`>`, `>=`, `<`, `<=`, `==`, `!=`) or
    /// `"<parameter> triggered"`.
    fn condition(&self, f: &Fields<'_>, line: usize, text: &str) -> Result<ConditionDef, CookError> {
        let words: Vec<&str> = text.split_whitespace().collect();
        let bad = || {
            f.err(
                line,
                &format!("bad condition `{text}` (`<parameter> <op> <value>` or `<parameter> triggered`)"),
            )
        };
        match words.as_slice() {
            [p, "triggered"] => {
                let (parameter, kind) = self.parameter(f, line, p)?;
                if kind != ParameterKind::Trigger {
                    return Err(f.err(line, &format!("`{p}` is not a trigger")));
                }
                Ok(ConditionDef {
                    parameter,
                    op: CompareOp::Triggered,
                    value: 0.0,
                })
            }
            [p, op, v] => {
                let (parameter, kind) = self.parameter(f, line, p)?;
                let op = match *op {
                    ">" => CompareOp::Greater,
                    ">=" => CompareOp::GreaterOrEqual,
                    "<" => CompareOp::Less,
                    "<=" => CompareOp::LessOrEqual,
                    "==" => CompareOp::Equal,
                    "!=" => CompareOp::NotEqual,
                    _ => return Err(bad()),
                };
                let value = match (kind, *v) {
                    (ParameterKind::Trigger, _) => {
                        return Err(f.err(line, &format!("`{p}` is a trigger: use `{p} triggered`")));
                    }
                    (ParameterKind::Bool, "true") => 1.0,
                    (ParameterKind::Bool, "false") => 0.0,
                    (ParameterKind::Bool, _) => return Err(f.err(line, &format!("`{p}` is a bool"))),
                    (ParameterKind::Float, v) => {
                        v.parse::<f32>().ok().filter(|x| x.is_finite()).ok_or_else(bad)?
                    }
                };
                Ok(ConditionDef { parameter, op, value })
            }
            _ => Err(bad()),
        }
    }

    fn layers(&mut self) -> Result<Vec<LayerDef>, CookError> {
        let doc = self.doc;
        let mut layers = Vec::new();
        for (_, f) in doc.items("layer") {
            f.only(&["node", "weight", "mode", "reference", "mask"])?;
            let node = self.reference(&f, f.line_of("node"), f.str("node")?.0)?;
            let weight = f.f32_or("weight", 1.0)?.0;
            if !(0.0..=1.0).contains(&weight) {
                return Err(f.err(f.line_of("weight"), "`weight` must be 0 to 1"));
            }
            let mode = match f.opt_str("mode")?.map_or("override", |(s, _)| s) {
                "override" => LayerMode::Override,
                "additive" => LayerMode::Additive,
                other => {
                    return Err(f.err(
                        f.line_of("mode"),
                        &format!("unknown layer mode `{other}` (override or additive)"),
                    ));
                }
            };
            if mode == LayerMode::Additive && layers.is_empty() {
                return Err(f.err(f.line_of("mode"), "the first layer must be an override layer"));
            }
            let reference_clip = if f.has("reference") {
                if mode != LayerMode::Additive {
                    return Err(f.err(f.line_of("reference"), "only an additive layer has a `reference`"));
                }
                Some(self.clip(&f, "reference")?)
            } else {
                None
            };
            let mask_line = f.line_of("mask");
            let mut mask = Vec::new();
            for bone in f
                .strs_or_empty("mask")?
                .0
                .into_iter()
                .map(str::to_owned)
                .collect::<Vec<String>>()
            {
                mask.push(
                    bone_index(self.skeleton, &bone)
                        .ok_or_else(|| f.err(mask_line, &format!("unknown bone `{bone}`")))?,
                );
            }
            mask.sort_unstable();
            if mask.windows(2).any(|w| matches!(w, [a, b] if a == b)) {
                return Err(f.err(mask_line, "a bone is listed twice"));
            }
            layers.push(LayerDef {
                node,
                weight,
                mode,
                reference_clip,
                mask,
            });
        }
        if layers.is_empty() {
            return Err(CookError::at(self.doc.path(), 0, "no `[layer.<name>]` tables"));
        }
        Ok(layers)
    }

    /// Every node is used once (checked as references resolve) and reachable from a layer.
    fn check_structure(&self, nodes: &[NodeDef]) -> Result<(), CookError> {
        let path = self.doc.path();
        if let Some((name, line)) = self
            .node_names
            .iter()
            .zip(&self.referenced)
            .find(|(_, r)| !**r)
            .map(|(n, _)| *n)
        {
            return Err(CookError::at(
                path,
                line,
                &format!("node `{name}` is not used by any layer or node"),
            ));
        }
        let mut reached = vec![false; nodes.len()];
        let mut stack: Vec<u32> = Vec::new();
        for (_, f) in self.doc.items("layer") {
            if let Some(i) = f
                .opt_str("node")
                .ok()
                .flatten()
                .map(|(n, _)| n)
                .and_then(|n| self.node_names.iter().position(|(m, _)| *m == n))
            {
                stack.push(u32_of(i));
            }
        }
        while let Some(n) = stack.pop() {
            if let Some(r) = reached.get_mut(n as usize)
                && !*r
            {
                *r = true;
                if let Some(def) = nodes.get(n as usize) {
                    def.for_each_child(|c| stack.push(c));
                }
            }
        }
        if let Some(((name, line), _)) = self.node_names.iter().zip(&reached).find(|(_, r)| !**r) {
            return Err(CookError::at(
                path,
                *line,
                &format!("node `{name}` is not reachable from a layer (the nodes form a cycle)"),
            ));
        }
        Ok(())
    }

    fn bone(&self, f: &Fields<'_>, key: &str) -> Result<u16, CookError> {
        let name = f.str(key)?.0;
        bone_index(self.skeleton, name)
            .ok_or_else(|| f.err(f.line_of(key), &format!("unknown bone `{name}`")))
    }

    fn foot_chains(&self) -> Result<Vec<TwoBoneChainDef>, CookError> {
        let mut chains = Vec::new();
        for (_, f) in self.doc.items("foot") {
            f.only(&["root", "mid", "tip", "pole"])?;
            let (root, mid, tip) = (
                self.bone(&f, "root")?,
                self.bone(&f, "mid")?,
                self.bone(&f, "tip")?,
            );
            if !(root < mid && mid < tip) {
                return Err(f.err(f.line(), "`root`, `mid`, and `tip` must go down the hierarchy"));
            }
            let pole = f
                .opt_array::<3>("pole")?
                .map(|(v, _)| v)
                .ok_or_else(|| f.err(f.line(), "missing `pole`"))?;
            if pole.iter().map(|v| v * v).sum::<f32>() <= 1e-12 {
                return Err(f.err(f.line_of("pole"), "`pole` must not be zero"));
            }
            chains.push(TwoBoneChainDef { root, mid, tip, pole });
        }
        Ok(chains)
    }

    fn look_at(&self) -> Result<Option<LookAtDef>, CookError> {
        let Some(f) = self.doc.tables().find(|t| t.name() == "look_at") else {
            return Ok(None);
        };
        f.only(&["head", "axis", "max_angle"])?;
        let head = self.bone(&f, "head")?;
        let axis = f
            .opt_array::<3>("axis")?
            .map(|(v, _)| v)
            .ok_or_else(|| f.err(f.line(), "missing `axis`"))?;
        let len = axis.iter().map(|v| v * v).sum::<f32>().sqrt();
        if len.is_nan() || len <= 1e-6 {
            return Err(f.err(f.line_of("axis"), "`axis` must not be zero"));
        }
        let max_angle = f.f32("max_angle")?.0;
        if !(max_angle > 0.0 && max_angle <= std::f32::consts::PI) {
            return Err(f.err(f.line_of("max_angle"), "`max_angle` must be in (0, pi] radians"));
        }
        Ok(Some(LookAtDef {
            head,
            axis: axis.map(|v| v / len),
            max_angle,
        }))
    }
}

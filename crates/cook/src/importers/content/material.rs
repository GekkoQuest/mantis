//! Materials: `*.material.toml` to MMAT (`mantis_formats::material`), phase 10.
//!
//! A material is a node graph with named nodes, a lighting model, optional rim light and
//! outline, and texture slots that name texture **sources** (resolved to the content
//! hashes of the phase-0 texture importer's outputs).
//!
//! ```toml
//! casts_shadows = true                 # default true
//! deformations = ["skinned", "vat"]    # default none
//! alpha_cutoff = 0.5                   # optional, 0 to 1: alpha test
//! textures = ["textures/crate.ppm"]    # slot 0, 1, ... (at most 4)
//!
//! [lighting]
//! model = "toon"        # unlit | lambert | toon | toon_ramp
//! bands = 3             # toon: 2 to 8
//! softness = 0.2        # toon: 0 to 1
//! shadow_tint = [0.55, 0.5, 0.7]   # toon
//! # ramp_slot = 1      # toon_ramp: the ramp's texture slot
//!
//! [rim]                 # optional
//! color = [1.0, 0.9, 0.8]
//! power = 3.0           # > 0
//! intensity = 0.6       # >= 0
//!
//! [outline]             # optional
//! width_px = 2.0        # > 0
//! color = [0.05, 0.03, 0.04]
//!
//! [output]
//! base_color = "rgb"    # vec3 or vec4 node
//! # alpha = "a"         # f32 node
//! # emissive = "glow"   # vec3 node
//!
//! [node.uv]
//! op = "uv0"
//!
//! [node.albedo]
//! op = "texture"
//! slot = 0
//! inputs = ["uv"]
//!
//! [node.tint]
//! op = "color_param"
//! name = "tint
//!
//! [node.tinted]
//! op = "multiply"
//! inputs = ["albedo", "tint"]
//!
//! [node.rgb]
//! op = "swizzle"
//! components = "xyz"
//! inputs = ["tinted"]
//! ```
//!
//! Node ops and their fields (`inputs` name other nodes, in this order):
//!
//! | op | fields |
//! |---|---|
//! | `uv0`, `uv1`, `world_position`, `world_normal`, `view_direction`, `time` | none |
//! | `constant` | `value`: 1 to 4 numbers (the length is the type) |
//! | `scalar_param` | `name` of a `[param.<name>]` with `kind = "scalar"` |
//! | `color_param` | `name` of a `[param.<name>]` with `kind = "color"` |
//! | `texture` | `slot` (a listed texture), `inputs = [uv]` |
//! | `add`, `subtract`, `multiply`, `divide`, `dot` | `inputs = [a, b]` |
//! | `lerp` | `inputs = [a, b, t]` |
//! | `normalize`, `saturate`, `one_minus` | `inputs = [a]` |
//! | `power` | `inputs = [a, exponent]` |
//! | `swizzle` | `components` (1 to 4 of `xyzw` or `rgba`), `inputs = [a]` |
//! | `combine` | `inputs` of 2 to 4 `f32` nodes |
//! | `fresnel` | `inputs = [power]` |
//!
//! Nodes may appear in any order; the cook orders them so every node follows its inputs
//! (file order otherwise) and rejects cycles and unknown names at the `inputs` line. The
//! graph is validated with `MaterialGraph::validate` (type errors are reported at the
//! node's header line), then **every canonical permutation** is compiled with
//! `mantis_shadergen::compile_all` and each WGSL module is parsed and validated by naga
//! with all validation flags and capabilities, so a material that would fail on the GPU
//! fails the cook.
//!
//! # Parameters
//!
//! Parameters are named in `[param.<name>]` tables (names: 1 to 31 bytes of `a-z`, `0-9`,
//! `_`) with a `kind` and a `default`, and nodes refer to them by `name`. Indices are
//! assigned per kind in file order; every declared parameter must be read by a node.
//!
//! ```toml
//! [param.tint]
//! kind = "color"                 # "color" (default: 3 or 4 numbers, alpha 1 when 3)
//! default = [0.55, 0.75, 0.5]    # or "scalar" (default: one number)
//! ```
//!
//! # Output (presentation domain)
//!
//! `<stem>.mat`, kind `Material`: MMAT version 2 (`mantis_formats::material`), whose
//! binding tables hold the content hash and sRGB / normal-map flags of every listed
//! texture (taken from the cooked MTEX, so they agree by construction; the runtime checks
//! them again at load) and every parameter's name and default. The cook refuses tables
//! that do not match the graph exactly (a listed texture or parameter nothing reads).

use std::collections::BTreeMap;

use mantis_formats::bundle::{AssetKind, Domain};
use mantis_formats::material::{
    COLOR_PARAMS, ColorDefault, Deformations, LightingModel, MAX_NODES, MAX_PARAM_NAME, MaterialAsset,
    MaterialBindings, MaterialError, MaterialGraph, Node, NodeId, Outline, RimLight, SCALAR_PARAMS,
    ScalarDefault, SurfaceOutputs, TEXTURE_SLOTS, TextureRef, ValueType,
};
use mantis_formats::texture::TextureAsset;

use super::fields::{Doc, Fields, output_name, within};
use crate::importer::{CookError, Cooked, ImportContext, Importer, Source};

const SUFFIX: &str = ".material.toml";

/// The material importer.
#[derive(Clone, Copy, Debug, Default)]
pub struct Materials;

/// Parses and validates one WGSL module with naga (all validation flags and
/// capabilities), as the GPU would see it.
///
/// # Errors
/// The naga diagnostic, rendered against `source`.
pub fn check_wgsl(source: &str) -> Result<(), String> {
    let module = naga::front::wgsl::parse_str(source).map_err(|e| e.emit_to_string(source))?;
    naga::valid::Validator::new(
        naga::valid::ValidationFlags::all(),
        naga::valid::Capabilities::all(),
    )
    .validate(&module)
    .map_err(|e| e.emit_to_string(source))?;
    Ok(())
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Op {
    Uv0,
    Uv1,
    WorldPosition,
    WorldNormal,
    ViewDirection,
    Time,
    Constant,
    ScalarParam,
    ColorParam,
    Texture,
    Add,
    Subtract,
    Multiply,
    Divide,
    Lerp,
    Dot,
    Normalize,
    Saturate,
    OneMinus,
    Power,
    Swizzle,
    Combine,
    Fresnel,
}

const OPS: &[(&str, Op)] = &[
    ("uv0", Op::Uv0),
    ("uv1", Op::Uv1),
    ("world_position", Op::WorldPosition),
    ("world_normal", Op::WorldNormal),
    ("view_direction", Op::ViewDirection),
    ("time", Op::Time),
    ("constant", Op::Constant),
    ("scalar_param", Op::ScalarParam),
    ("color_param", Op::ColorParam),
    ("texture", Op::Texture),
    ("add", Op::Add),
    ("subtract", Op::Subtract),
    ("multiply", Op::Multiply),
    ("divide", Op::Divide),
    ("lerp", Op::Lerp),
    ("dot", Op::Dot),
    ("normalize", Op::Normalize),
    ("saturate", Op::Saturate),
    ("one_minus", Op::OneMinus),
    ("power", Op::Power),
    ("swizzle", Op::Swizzle),
    ("combine", Op::Combine),
    ("fresnel", Op::Fresnel),
];

impl Op {
    /// Allowed input counts and the extra keys besides `op` and `inputs`.
    fn shape(self) -> (core::ops::RangeInclusive<usize>, &'static [&'static str]) {
        match self {
            Op::Uv0 | Op::Uv1 | Op::WorldPosition | Op::WorldNormal | Op::ViewDirection | Op::Time => {
                (0..=0, &[])
            }
            Op::Constant => (0..=0, &["value"]),
            Op::ScalarParam | Op::ColorParam => (0..=0, &["name"]),
            Op::Texture => (1..=1, &["slot"]),
            Op::Add | Op::Subtract | Op::Multiply | Op::Divide | Op::Dot | Op::Power => (2..=2, &[]),
            Op::Lerp => (3..=3, &[]),
            Op::Normalize | Op::Saturate | Op::OneMinus | Op::Fresnel => (1..=1, &[]),
            Op::Swizzle => (1..=1, &["components"]),
            Op::Combine => (2..=4, &[]),
        }
    }

    fn name(self) -> &'static str {
        OPS.iter().find(|(_, o)| *o == self).map_or("?", |(n, _)| *n)
    }
}

/// One `[node.<name>]` table, read but not yet linked.
struct Spec<'d> {
    name: &'d str,
    fields: Fields<'d>,
    op: Op,
    inputs: Vec<&'d str>,
    inputs_line: usize,
}

fn read_spec<'d>(name: &'d str, f: Fields<'d>) -> Result<Spec<'d>, CookError> {
    let (op, _) = f.choice("op", OPS, None)?;
    let (count, extra) = op.shape();
    let mut allowed = vec!["op"];
    if *count.end() > 0 {
        allowed.push("inputs");
    }
    allowed.extend_from_slice(extra);
    f.only(&allowed)?;
    let (inputs, inputs_line) = if *count.end() > 0 {
        f.strs("inputs")?
    } else {
        (Vec::new(), f.line)
    };
    if !count.contains(&inputs.len()) {
        let want = if count.start() == count.end() {
            count.start().to_string()
        } else {
            format!("{} to {}", count.start(), count.end())
        };
        return Err(f.err(
            inputs_line,
            &format!(
                "node `{name}` (`{}`) takes {want} inputs, not {}",
                op.name(),
                inputs.len()
            ),
        ));
    }
    Ok(Spec {
        name,
        fields: f,
        op,
        inputs,
        inputs_line,
    })
}

/// Orders specs so each follows its inputs (file order otherwise).
fn order(doc: &Doc<'_>, specs: &[Spec<'_>]) -> Result<Vec<usize>, CookError> {
    let index: BTreeMap<&str, usize> = specs.iter().enumerate().map(|(i, s)| (s.name, i)).collect();
    // 0 unvisited, 1 on the current path, 2 placed.
    let mut state = vec![0u8; specs.len()];
    let mut out = Vec::with_capacity(specs.len());
    for start in 0..specs.len() {
        if state.get(start).copied() != Some(0) {
            continue;
        }
        if let Some(slot) = state.get_mut(start) {
            *slot = 1;
        }
        // Iterative depth-first walk: (spec, next input to visit).
        let mut stack = vec![(start, 0usize)];
        while let Some(top) = stack.last_mut() {
            let (at, k) = *top;
            top.1 += 1;
            let s = specs.get(at).ok_or_else(|| doc.err(0, "node index"))?;
            if let Some(input) = s.inputs.get(k) {
                let j = *index.get(input).ok_or_else(|| {
                    doc.err(
                        s.inputs_line,
                        &format!("node `{}` names unknown node `{input}`", s.name),
                    )
                })?;
                match state.get(j).copied() {
                    Some(0) => {
                        if let Some(slot) = state.get_mut(j) {
                            *slot = 1;
                        }
                        stack.push((j, 0));
                    }
                    Some(1) => {
                        return Err(doc.err(
                            s.inputs_line,
                            &format!("node `{}` is part of a cycle through `{input}`", s.name),
                        ));
                    }
                    _ => {}
                }
            } else {
                if let Some(slot) = state.get_mut(at) {
                    *slot = 2;
                }
                out.push(at);
                stack.pop();
            }
        }
    }
    Ok(out)
}

const SWIZZLE: &[(char, u8)] = &[
    ('x', 0),
    ('y', 1),
    ('z', 2),
    ('w', 3),
    ('r', 0),
    ('g', 1),
    ('b', 2),
    ('a', 3),
];

fn constant(f: &Fields<'_>) -> Result<Node, CookError> {
    let (values, line) = f.f32s("value")?;
    let n = u8::try_from(values.len()).unwrap_or(u8::MAX);
    let ty = ValueType::with_components(n)
        .ok_or_else(|| f.err(line, &format!("`value` must hold 1 to 4 numbers, not {n}")))?;
    let mut v = [0.0f32; 4];
    for (d, x) in v.iter_mut().zip(values) {
        *d = x;
    }
    Ok(Node::Constant(v, ty))
}

/// A swizzle's `components`: indices and length.
fn components(f: &Fields<'_>) -> Result<([u8; 4], u8), CookError> {
    let (text, line) = f.str("components")?;
    let mut idx = [0u8; 4];
    let mut len = 0u8;
    for c in text.chars() {
        let v = SWIZZLE
            .iter()
            .find(|(k, _)| *k == c)
            .map(|(_, v)| *v)
            .ok_or_else(|| {
                f.err(
                    line,
                    &format!("`components` = \"{text}\": `{c}` is not one of xyzw or rgba"),
                )
            })?;
        let slot = idx
            .get_mut(usize::from(len))
            .ok_or_else(|| f.err(line, "`components` has more than 4 letters"))?;
        *slot = v;
        len += 1;
    }
    if len == 0 {
        return Err(f.err(line, "`components` is empty"));
    }
    Ok((idx, len))
}

/// The declared parameters: defaults per kind, and each name's kind, index, and line.
struct Params<'d> {
    scalars: Vec<ScalarDefault>,
    colors: Vec<ColorDefault>,
    by_name: BTreeMap<&'d str, (bool, u8, usize)>,
}

fn read_params<'d>(doc: &'d Doc<'_>) -> Result<Params<'d>, CookError> {
    let mut out = Params {
        scalars: Vec::new(),
        colors: Vec::new(),
        by_name: BTreeMap::new(),
    };
    for (name, f) in doc.prefixed("param.") {
        f.only(&["kind", "default"])?;
        let ok = (1..=MAX_PARAM_NAME).contains(&name.len())
            && name
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_');
        if !ok {
            return Err(f.err(
                f.line,
                &format!("parameter name `{name}` must be 1 to {MAX_PARAM_NAME} bytes of a-z, 0-9, _"),
            ));
        }
        let (color, _) = f.choice("kind", &[("scalar", false), ("color", true)], None)?;
        let (values, line) = f.f32s("default")?;
        let index = if color {
            let value = match values.as_slice() {
                [red, green, blue] => [*red, *green, *blue, 1.0],
                [red, green, blue, alpha] => [*red, *green, *blue, *alpha],
                _ => return Err(f.err(line, "a color `default` has 3 or 4 numbers")),
            };
            if out.colors.len() >= usize::from(COLOR_PARAMS) {
                return Err(f.err(f.line, &format!("more than {COLOR_PARAMS} color parameters")));
            }
            out.colors.push(ColorDefault {
                name: name.to_owned(),
                value,
            });
            out.colors.len() - 1
        } else {
            let [value] = values.as_slice() else {
                return Err(f.err(line, "a scalar `default` is one number"));
            };
            if out.scalars.len() >= usize::from(SCALAR_PARAMS) {
                return Err(f.err(f.line, &format!("more than {SCALAR_PARAMS} scalar parameters")));
            }
            out.scalars.push(ScalarDefault {
                name: name.to_owned(),
                value: *value,
            });
            out.scalars.len() - 1
        };
        let index = u8::try_from(index).map_err(|_| f.err(f.line, "too many parameters"))?;
        out.by_name.insert(name, (color, index, f.line));
    }
    Ok(out)
}

/// Builds one node from its spec, with `ids` of the specs placed so far.
fn build_node(
    s: &Spec<'_>,
    ids: &BTreeMap<&str, NodeId>,
    textures: usize,
    params: &Params<'_>,
) -> Result<Node, CookError> {
    let f = &s.fields;
    let input = |i: usize| -> Result<NodeId, CookError> {
        s.inputs.get(i).and_then(|n| ids.get(n)).copied().ok_or_else(|| {
            f.err(
                s.inputs_line,
                &format!("node `{}`: input {i} is not placed", s.name),
            )
        })
    };
    Ok(match s.op {
        Op::Uv0 => Node::Uv0,
        Op::Uv1 => Node::Uv1,
        Op::WorldPosition => Node::WorldPosition,
        Op::WorldNormal => Node::WorldNormal,
        Op::ViewDirection => Node::ViewDirection,
        Op::Time => Node::Time,
        Op::Constant => constant(f)?,
        Op::ScalarParam | Op::ColorParam => {
            let (name, line) = f.str("name")?;
            let want_color = s.op == Op::ColorParam;
            let (color, index, _) = *params
                .by_name
                .get(name)
                .ok_or_else(|| f.err(line, &format!("no `[param.{name}]` is declared")))?;
            if color != want_color {
                let kind = if color { "color" } else { "scalar" };
                return Err(f.err(
                    line,
                    &format!(
                        "parameter `{name}` is a {kind}; `{}` needs the other kind",
                        s.op.name()
                    ),
                ));
            }
            if color {
                Node::ColorParam(index)
            } else {
                Node::ScalarParam(index)
            }
        }
        Op::Texture => {
            let (slot, line) = f.int::<u8>("slot")?;
            if usize::from(slot) >= textures {
                return Err(f.err(
                    line,
                    &format!("texture slot {slot} has no texture (`textures` lists {textures})"),
                ));
            }
            Node::Texture { slot, uv: input(0)? }
        }
        Op::Add => Node::Add(input(0)?, input(1)?),
        Op::Subtract => Node::Subtract(input(0)?, input(1)?),
        Op::Multiply => Node::Multiply(input(0)?, input(1)?),
        Op::Divide => Node::Divide(input(0)?, input(1)?),
        Op::Lerp => Node::Lerp(input(0)?, input(1)?, input(2)?),
        Op::Dot => Node::Dot(input(0)?, input(1)?),
        Op::Normalize => Node::Normalize(input(0)?),
        Op::Saturate => Node::Saturate(input(0)?),
        Op::OneMinus => Node::OneMinus(input(0)?),
        Op::Power => Node::Power(input(0)?, input(1)?),
        Op::Swizzle => {
            let (idx, len) = components(f)?;
            Node::Swizzle(input(0)?, idx, len)
        }
        Op::Combine => {
            let mut parts = [None; 4];
            for (i, p) in parts.iter_mut().enumerate().take(s.inputs.len()) {
                *p = Some(input(i)?);
            }
            Node::Combine(parts)
        }
        Op::Fresnel => Node::Fresnel(input(0)?),
    })
}

/// The lighting model and the fields it was read from.
fn read_lighting<'d>(doc: &'d Doc<'_>, textures: usize) -> Result<(LightingModel, Fields<'d>), CookError> {
    let f = doc
        .table("lighting")
        .ok_or_else(|| doc.err(0, "missing `[lighting]` (with `model`)"))?;
    let models = [("unlit", 0u8), ("lambert", 1), ("toon", 2), ("toon_ramp", 3)];
    let (model, _) = f.choice("model", &models, None)?;
    let lighting = match model {
        0 | 1 => {
            f.only(&["model"])?;
            if model == 0 {
                LightingModel::Unlit
            } else {
                LightingModel::Lambert
            }
        }
        2 => {
            f.only(&["model", "bands", "softness", "shadow_tint"])?;
            let (bands, bl) = f.int::<u8>("bands")?;
            if !(2..=8).contains(&bands) {
                return Err(f.err(bl, &format!("toon `bands` = {bands} is outside 2 to 8")));
            }
            let (softness, sl) = f.f32("softness")?;
            if !within(softness, 0.0, 1.0) {
                return Err(f.err(sl, &format!("toon `softness` = {softness} is outside 0 to 1")));
            }
            let (shadow_tint, _) = f.array::<3>("shadow_tint")?;
            LightingModel::Toon {
                bands,
                softness,
                shadow_tint,
            }
        }
        _ => {
            f.only(&["model", "ramp_slot"])?;
            let (slot, line) = f.int::<u8>("ramp_slot")?;
            if usize::from(slot) >= textures {
                return Err(f.err(
                    line,
                    &format!("`ramp_slot` {slot} has no texture (`textures` lists {textures})"),
                ));
            }
            LightingModel::ToonRamp { slot }
        }
    };
    Ok((lighting, f))
}

fn read_rim(doc: &Doc<'_>) -> Result<Option<RimLight>, CookError> {
    let Some(f) = doc.table("rim") else {
        return Ok(None);
    };
    f.only(&["color", "power", "intensity"])?;
    let (color, _) = f.array::<3>("color")?;
    let (power, pl) = f.f32("power")?;
    if power <= 0.0 {
        return Err(f.err(pl, &format!("rim `power` = {power} must be > 0")));
    }
    let (intensity, il) = f.f32("intensity")?;
    if intensity < 0.0 {
        return Err(f.err(il, &format!("rim `intensity` = {intensity} must be >= 0")));
    }
    Ok(Some(RimLight {
        color,
        power,
        intensity,
    }))
}

fn read_outline(doc: &Doc<'_>) -> Result<Option<Outline>, CookError> {
    let Some(f) = doc.table("outline") else {
        return Ok(None);
    };
    f.only(&["width_px", "color"])?;
    let (width_px, wl) = f.f32("width_px")?;
    if width_px <= 0.0 {
        return Err(f.err(wl, &format!("outline `width_px` = {width_px} must be > 0")));
    }
    let (color, _) = f.array::<3>("color")?;
    Ok(Some(Outline { width_px, color }))
}

fn read_deformations(root: &Fields<'_>) -> Result<Deformations, CookError> {
    let (names, line) = root.opt_strs("deformations")?;
    let mut d = Deformations::NONE;
    for n in names {
        match n {
            "skinned" => d.skinned = true,
            "vat" => d.vat = true,
            other => {
                return Err(root.err(
                    line,
                    &format!("unknown deformation `{other}` (expected skinned, vat)"),
                ));
            }
        }
    }
    Ok(d)
}

/// Where each part of the graph was read, for locating validation errors.
struct Located<'a, 'd> {
    doc: &'a Doc<'d>,
    specs: &'a [Spec<'a>],
    /// Spec index of each node id.
    placed: &'a [usize],
    output: Fields<'a>,
    lighting: Fields<'a>,
    rim_line: usize,
    outline_line: usize,
}

impl Located<'_, '_> {
    fn node_error(&self, node: u16, message: &str) -> CookError {
        match self
            .placed
            .get(usize::from(node))
            .and_then(|i| self.specs.get(*i))
        {
            Some(s) => self.doc.err(
                s.fields.line,
                &format!("node `{}` (`{}`): {message}", s.name, s.op.name()),
            ),
            None => self.doc.err(0, message),
        }
    }

    fn map(&self, e: MaterialError) -> CookError {
        match e {
            MaterialError::NodeCount => self.doc.err(
                0,
                &format!("a material has 1 to {MAX_NODES} nodes"),
            ),
            MaterialError::ForwardReference { node, input } => {
                self.node_error(node, &format!("references node {input}, which is not earlier"))
            }
            MaterialError::TypeMismatch { node } => {
                let s = self
                    .placed
                    .get(usize::from(node))
                    .and_then(|i| self.specs.get(*i));
                let inputs = s.map(|s| s.inputs.join(", ")).unwrap_or_default();
                self.node_error(
                    node,
                    &format!("input types do not fit this op (inputs: {inputs})"),
                )
            }
            MaterialError::OutOfRange { node: Some(node) } => self.node_error(
                node,
                "a value is out of range (a swizzle component the input does not have, or a combine of fewer than 2 inputs)",
            ),
            MaterialError::OutOfRange { node: None } => self.doc.err(0, "a value is out of range"),
            MaterialError::Output(which) => {
                let expected = match which {
                    "base_color" => "a vec3 or vec4 node",
                    "alpha" => "an f32 node",
                    "emissive" => "a vec3 node",
                    _ => "within 0 to 1",
                };
                let line = if which == "alpha_cutoff" {
                    0
                } else {
                    self.output.line_of(which)
                };
                self.doc
                    .err(line, &format!("output `{which}` must be {expected}"))
            }
            MaterialError::Setting(which) => {
                let line = match which {
                    "rim" => self.rim_line,
                    "outline" => self.outline_line,
                    _ => self.lighting.line,
                };
                self.doc
                    .err(line, &format!("`{which}` settings are out of range"))
            }
        }
    }
}

fn output_node(
    output: &Fields<'_>,
    key: &str,
    ids: &BTreeMap<&str, NodeId>,
) -> Result<Option<NodeId>, CookError> {
    match output.opt_str(key)? {
        None => Ok(None),
        Some((name, line)) => ids
            .get(name)
            .copied()
            .map(Some)
            .ok_or_else(|| output.err(line, &format!("output `{key}` names unknown node `{name}`"))),
    }
}

fn read_cutoff(root: &Fields<'_>) -> Result<Option<f32>, CookError> {
    if !root.has("alpha_cutoff") {
        return Ok(None);
    }
    let (c, line) = root.f32("alpha_cutoff")?;
    if within(c, 0.0, 1.0) {
        Ok(Some(c))
    } else {
        Err(root.err(line, &format!("`alpha_cutoff` = {c} is outside 0 to 1")))
    }
}

/// Resolves the texture slots to the cooked textures' hashes and flags; also returns the
/// `textures` line.
fn read_textures(
    root: &Fields<'_>,
    ctx: &ImportContext<'_>,
    from: &str,
) -> Result<(Vec<TextureRef>, usize), CookError> {
    let (paths, line) = root.opt_strs("textures")?;
    if paths.len() > usize::from(TEXTURE_SLOTS) {
        return Err(root.err(
            line,
            &format!(
                "{} textures; a material has at most {TEXTURE_SLOTS} slots",
                paths.len()
            ),
        ));
    }
    let mut refs = Vec::with_capacity(paths.len());
    for path in &paths {
        let (hash, bytes) = ctx.resolve_bytes(path, AssetKind::Texture, from, line)?;
        let texture = TextureAsset::parse(bytes)
            .map_err(|e| root.err(line, &format!("`{path}` is not a texture: {e}")))?;
        refs.push(TextureRef {
            hash,
            flags: u32::from(texture.flags),
        });
    }
    Ok((refs, line))
}

fn read_specs<'d>(doc: &'d Doc<'_>) -> Result<Vec<Spec<'d>>, CookError> {
    let mut specs = Vec::new();
    for (name, f) in doc.prefixed("node.") {
        specs.push(read_spec(name, f)?);
    }
    if specs.is_empty() || specs.len() > MAX_NODES {
        return Err(doc.err(
            0,
            &format!("{} nodes; a material has 1 to {MAX_NODES}", specs.len()),
        ));
    }
    Ok(specs)
}

/// Builds the nodes in `placed` order; returns them and each name's id.
fn build_nodes<'d>(
    doc: &Doc<'_>,
    specs: &[Spec<'d>],
    placed: &[usize],
    textures: usize,
    params: &Params<'_>,
) -> Result<(Vec<Node>, BTreeMap<&'d str, NodeId>), CookError> {
    let mut ids: BTreeMap<&'d str, NodeId> = BTreeMap::new();
    let mut nodes = Vec::with_capacity(placed.len());
    for (id, i) in placed.iter().enumerate() {
        let s = specs.get(*i).ok_or_else(|| doc.err(0, "node index"))?;
        nodes.push(build_node(s, &ids, textures, params)?);
        let id = u16::try_from(id).map_err(|_| doc.err(0, "too many nodes"))?;
        ids.insert(s.name, NodeId(id));
    }
    Ok((nodes, ids))
}

/// Exact binding tables: a listed texture past the highest slot read, or a parameter no
/// node reads, is an error at its line.
fn check_tables(
    doc: &Doc<'_>,
    root: &Fields<'_>,
    asset: &MaterialAsset,
    params: &Params<'_>,
    textures_line: usize,
) -> Result<(), CookError> {
    let (need_textures, _, _) = asset.required_bindings();
    if asset.bindings.textures.len() != need_textures {
        return Err(root.err(
            textures_line,
            &format!(
                "{} textures listed, but nodes and lighting read slots below {need_textures} only",
                asset.bindings.textures.len()
            ),
        ));
    }
    let read: std::collections::BTreeSet<(bool, u8)> = asset
        .graph
        .graph
        .nodes
        .iter()
        .filter_map(|n| match n {
            Node::ScalarParam(i) => Some((false, *i)),
            Node::ColorParam(i) => Some((true, *i)),
            _ => None,
        })
        .collect();
    if let Some((name, (_, _, line))) = params
        .by_name
        .iter()
        .find(|(_, (color, index, _))| !read.contains(&(*color, *index)))
    {
        return Err(doc.err(*line, &format!("parameter `{name}` is not read by any node")));
    }
    if let Some(e) = asset.bindings_error() {
        return Err(doc.err(0, &e));
    }
    Ok(())
}

impl Importer for Materials {
    fn name(&self) -> &'static str {
        "material.toml"
    }

    fn version(&self) -> u32 {
        2
    }

    fn phase(&self) -> u32 {
        10
    }

    fn accepts(&self, path: &str) -> bool {
        path.ends_with(SUFFIX)
    }

    fn import(&self, source: &Source<'_>, ctx: &ImportContext<'_>) -> Result<Vec<Cooked>, CookError> {
        let doc = Doc::parse(source)?;
        doc.only_tables(&["lighting", "rim", "outline", "output"], &["node.", "param."])?;
        let root = doc.root().ok_or_else(|| doc.err(0, "empty document"))?;
        root.only(&["casts_shadows", "deformations", "alpha_cutoff", "textures"])?;
        let (casts_shadows, _) = root.opt_bool("casts_shadows", true)?;
        let deformations = read_deformations(&root)?;
        let alpha_cutoff = read_cutoff(&root)?;
        let (textures, textures_line) = read_textures(&root, ctx, source.path)?;
        let texture_count = textures.len();
        let params = read_params(&doc)?;

        let (lighting, lighting_fields) = read_lighting(&doc, texture_count)?;
        let rim = read_rim(&doc)?;
        let outline = read_outline(&doc)?;

        let specs = read_specs(&doc)?;
        let placed = order(&doc, &specs)?;
        let (nodes, ids) = build_nodes(&doc, &specs, &placed, texture_count, &params)?;

        let output = doc
            .table("output")
            .ok_or_else(|| doc.err(0, "missing `[output]` (with `base_color`)"))?;
        output.only(&["base_color", "alpha", "emissive"])?;
        let base_color = output_node(&output, "base_color", &ids)?
            .ok_or_else(|| output.err(output.line, "`[output]` is missing `base_color`"))?;
        let outputs = SurfaceOutputs {
            base_color,
            alpha: output_node(&output, "alpha", &ids)?,
            emissive: output_node(&output, "emissive", &ids)?,
            alpha_cutoff,
        };
        let graph = MaterialGraph {
            nodes,
            outputs,
            lighting,
            rim,
            outline,
        };
        let located = Located {
            doc: &doc,
            specs: &specs,
            placed: &placed,
            output,
            lighting: lighting_fields,
            rim_line: doc.table("rim").map_or(0, |f| f.line),
            outline_line: doc.table("outline").map_or(0, |f| f.line),
        };
        let typed = graph.validate().map_err(|e| located.map(e))?;
        let asset = MaterialAsset {
            graph: typed,
            casts_shadows,
            deformations,
            bindings: MaterialBindings {
                textures,
                scalars: params.scalars.clone(),
                colors: params.colors.clone(),
            },
        };
        check_tables(&doc, &root, &asset, &params, textures_line)?;

        // Every permutation the renderer may ask for must be valid WGSL.
        let permutations = mantis_shadergen::compile_all(&asset)
            .map_err(|e| doc.err(0, &format!("shader generation failed: {e}")))?;
        for p in &permutations {
            check_wgsl(&p.source)
                .map_err(|e| doc.err(0, &format!("permutation {:?} is not valid WGSL:\n{e}", p.key)))?;
        }

        let bytes = asset.encode();
        MaterialAsset::parse(&bytes)
            .map_err(|e| doc.err(0, &format!("cooked material fails its runtime parser: {e}")))?;
        let name = output_name(source.path, SUFFIX, ".mat");
        Ok(vec![Cooked {
            name,
            kind: AssetKind::Material,
            domain: Domain::Presentation,
            bytes,
        }])
    }
}

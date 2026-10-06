//! Material asset v1: a data-defined shader graph, validated and typed, plus the set of
//! shader permutations the cook compiles for it (decision 0018).
//!
//! A graph is a list of nodes in dependency order (a node references only earlier nodes),
//! surface outputs, a lighting model, and optional stylized features. Toon ramps, outlines,
//! and rim lighting are first-class (plan 8.3), not node soup.
//!
//! Types are inferred: `f32`, `vec2`, `vec3`, `vec4`. Binary operations take two values of
//! the same type, or a vector and an `f32` (broadcast). Validation rejects anything else,
//! fails closed on out-of-range parameters, and never panics.

use mantis_core::content::ContentHash;

use crate::bytes::{FormatError, Reader, Writer};

/// Most nodes per graph.
pub const MAX_NODES: usize = 256;
/// Scalar parameters per material.
pub const SCALAR_PARAMS: u8 = 16;
/// Color parameters per material.
pub const COLOR_PARAMS: u8 = 8;
/// Texture slots per material.
pub const TEXTURE_SLOTS: u8 = 4;

/// A value type.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum ValueType {
    /// `f32`.
    F32,
    /// `vec2<f32>`.
    Vec2,
    /// `vec3<f32>`.
    Vec3,
    /// `vec4<f32>`.
    Vec4,
}

impl ValueType {
    /// Components.
    pub fn components(self) -> u8 {
        match self {
            ValueType::F32 => 1,
            ValueType::Vec2 => 2,
            ValueType::Vec3 => 3,
            ValueType::Vec4 => 4,
        }
    }

    /// The type with `n` components.
    pub fn with_components(n: u8) -> Option<ValueType> {
        match n {
            1 => Some(ValueType::F32),
            2 => Some(ValueType::Vec2),
            3 => Some(ValueType::Vec3),
            4 => Some(ValueType::Vec4),
            _ => None,
        }
    }

    /// WGSL spelling.
    pub fn wgsl(self) -> &'static str {
        match self {
            ValueType::F32 => "f32",
            ValueType::Vec2 => "vec2<f32>",
            ValueType::Vec3 => "vec3<f32>",
            ValueType::Vec4 => "vec4<f32>",
        }
    }
}

/// Index of a node in its graph.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, PartialOrd, Ord)]
pub struct NodeId(pub u16);

/// A graph node.
#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Node {
    /// First UV set (`vec2`).
    Uv0,
    /// Second UV set, the lightmap UVs (`vec2`).
    Uv1,
    /// World position (`vec3`).
    WorldPosition,
    /// Unit world normal (`vec3`).
    WorldNormal,
    /// Unit direction from the surface toward the camera (`vec3`).
    ViewDirection,
    /// Seconds since the session started (`f32`).
    Time,
    /// A constant of the given type (unused components ignored).
    Constant([f32; 4], ValueType),
    /// Scalar parameter `n` from the material's parameter block (`f32`).
    ScalarParam(u8),
    /// Color parameter `n` (`vec4`).
    ColorParam(u8),
    /// Samples texture slot `slot` at `uv` (`vec4`).
    Texture {
        /// Slot, below [`TEXTURE_SLOTS`].
        slot: u8,
        /// A `vec2` node.
        uv: NodeId,
    },
    /// `a + b`.
    Add(NodeId, NodeId),
    /// `a - b`.
    Subtract(NodeId, NodeId),
    /// `a * b`.
    Multiply(NodeId, NodeId),
    /// `a / b`.
    Divide(NodeId, NodeId),
    /// `mix(a, b, t)`; `t` is `f32` or the same type as `a`.
    Lerp(NodeId, NodeId, NodeId),
    /// `dot(a, b)` of two equal vector types (`f32`).
    Dot(NodeId, NodeId),
    /// `normalize(a)` of a vector.
    Normalize(NodeId),
    /// `clamp(a, 0, 1)`.
    Saturate(NodeId),
    /// `1 - a`.
    OneMinus(NodeId),
    /// `pow(max(a, 0), e)` with `e` an `f32`.
    Power(NodeId, NodeId),
    /// Picks `len` components (indices 0 to 3, each below the input's component count).
    Swizzle(NodeId, [u8; 4], u8),
    /// Builds a vector from 2 to 4 `f32` nodes.
    Combine([Option<NodeId>; 4]),
    /// `pow(1 - saturate(dot(n, v)), power)`: view-angle falloff (`f32`).
    Fresnel(NodeId),
}

/// How a surface responds to direct light.
#[derive(Clone, Copy, PartialEq, Debug)]
pub enum LightingModel {
    /// No lighting: base color plus emissive.
    Unlit,
    /// Lambert diffuse.
    Lambert,
    /// Banded toon diffuse.
    Toon {
        /// Bands (2 to 8).
        bands: u8,
        /// Edge softness between bands, 0 (hard) to 1.
        softness: f32,
        /// Color multiplier in the darkest band.
        shadow_tint: [f32; 3],
    },
    /// Toon diffuse from a ramp texture sampled by the lit amount.
    ToonRamp {
        /// The ramp's texture slot.
        slot: u8,
    },
}

/// Rim lighting: a view-angle glow added to the lit result.
#[derive(Clone, Copy, PartialEq, Debug)]
pub struct RimLight {
    /// Linear color.
    pub color: [f32; 3],
    /// Falloff exponent (> 0).
    pub power: f32,
    /// Strength (>= 0).
    pub intensity: f32,
}

/// An inverted-hull outline drawn in its own pass.
#[derive(Clone, Copy, PartialEq, Debug)]
pub struct Outline {
    /// Width in pixels at any distance (> 0).
    pub width_px: f32,
    /// Linear color.
    pub color: [f32; 3],
}

/// The surface outputs.
#[derive(Clone, Copy, PartialEq, Debug)]
pub struct SurfaceOutputs {
    /// Base color (`vec3`, or `vec4` whose rgb is used).
    pub base_color: NodeId,
    /// Opacity (`f32`); 1 when absent.
    pub alpha: Option<NodeId>,
    /// Emitted light (`vec3`); zero when absent.
    pub emissive: Option<NodeId>,
    /// Alpha-test threshold: fragments with lower alpha are discarded.
    pub alpha_cutoff: Option<f32>,
}

/// A material graph.
#[derive(Clone, PartialEq, Debug)]
pub struct MaterialGraph {
    /// Nodes in dependency order.
    pub nodes: Vec<Node>,
    /// Outputs.
    pub outputs: SurfaceOutputs,
    /// Direct-light response.
    pub lighting: LightingModel,
    /// Optional rim light.
    pub rim: Option<RimLight>,
    /// Optional outline.
    pub outline: Option<Outline>,
}

/// Why a graph was rejected.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum MaterialError {
    /// More than [`MAX_NODES`] nodes, or no nodes.
    NodeCount,
    /// Node `node` references `input`, which is not an earlier node.
    ForwardReference {
        /// The referencing node.
        node: u16,
        /// The referenced id.
        input: u16,
    },
    /// Node `node` combines incompatible types.
    TypeMismatch {
        /// The node.
        node: u16,
    },
    /// A parameter, slot, swizzle index, or constant is out of range or not finite.
    OutOfRange {
        /// The node, if the problem is in a node.
        node: Option<u16>,
    },
    /// An output has the wrong type or names a missing node.
    Output(&'static str),
    /// A lighting, rim, or outline setting is out of range.
    Setting(&'static str),
}

impl core::fmt::Display for MaterialError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "invalid material graph: {self:?}")
    }
}

impl std::error::Error for MaterialError {}

/// A validated graph with every node's type.
#[derive(Clone, PartialEq, Debug)]
pub struct TypedGraph {
    /// The graph.
    pub graph: MaterialGraph,
    /// Type of each node.
    pub types: Vec<ValueType>,
    /// Texture slots sampled anywhere (including a ramp).
    pub slots_used: [bool; TEXTURE_SLOTS as usize],
}

fn finite(v: &[f32]) -> bool {
    v.iter().all(|x| x.is_finite())
}

impl MaterialGraph {
    /// Validates and types the graph.
    ///
    /// # Errors
    /// [`MaterialError`] describing the first problem found.
    pub fn validate(self) -> Result<TypedGraph, MaterialError> {
        if self.nodes.is_empty() || self.nodes.len() > MAX_NODES {
            return Err(MaterialError::NodeCount);
        }
        let mut types: Vec<ValueType> = Vec::with_capacity(self.nodes.len());
        let mut slots_used = [false; TEXTURE_SLOTS as usize];
        for (i, node) in self.nodes.iter().enumerate() {
            let at = u16::try_from(i).map_err(|_| MaterialError::NodeCount)?;
            let ty = node_type(node, at, &types, &mut slots_used)?;
            types.push(ty);
        }
        let ty_of = |id: NodeId, what: &'static str| {
            types
                .get(usize::from(id.0))
                .copied()
                .ok_or(MaterialError::Output(what))
        };
        match ty_of(self.outputs.base_color, "base_color")? {
            ValueType::Vec3 | ValueType::Vec4 => {}
            _ => return Err(MaterialError::Output("base_color")),
        }
        if let Some(a) = self.outputs.alpha
            && ty_of(a, "alpha")? != ValueType::F32
        {
            return Err(MaterialError::Output("alpha"));
        }
        if let Some(e) = self.outputs.emissive
            && ty_of(e, "emissive")? != ValueType::Vec3
        {
            return Err(MaterialError::Output("emissive"));
        }
        if let Some(c) = self.outputs.alpha_cutoff
            && !(c.is_finite() && (0.0..=1.0).contains(&c))
        {
            return Err(MaterialError::Output("alpha_cutoff"));
        }
        match self.lighting {
            LightingModel::Unlit | LightingModel::Lambert => {}
            LightingModel::Toon {
                bands,
                softness,
                shadow_tint,
            } => {
                if !(2..=8).contains(&bands) || !(0.0..=1.0).contains(&softness) || !finite(&shadow_tint) {
                    return Err(MaterialError::Setting("toon"));
                }
            }
            LightingModel::ToonRamp { slot } => {
                let s = slots_used
                    .get_mut(usize::from(slot))
                    .ok_or(MaterialError::Setting("toon ramp slot"))?;
                *s = true;
            }
        }
        if let Some(r) = self.rim
            && !(finite(&r.color)
                && r.power.is_finite()
                && r.power > 0.0
                && r.intensity.is_finite()
                && r.intensity >= 0.0)
        {
            return Err(MaterialError::Setting("rim"));
        }
        if let Some(o) = self.outline
            && !(finite(&o.color) && o.width_px.is_finite() && o.width_px > 0.0)
        {
            return Err(MaterialError::Setting("outline"));
        }
        Ok(TypedGraph {
            graph: self,
            types,
            slots_used,
        })
    }
}

/// The type of node `id` as seen from node `at`, which may only reference earlier nodes.
fn input_type(types: &[ValueType], at: u16, id: NodeId) -> Result<ValueType, MaterialError> {
    let err = MaterialError::ForwardReference {
        node: at,
        input: id.0,
    };
    if id.0 >= at {
        return Err(err);
    }
    types.get(usize::from(id.0)).copied().ok_or(err)
}

/// Combine: 2 to 4 leading `f32` parts, then only empty slots.
fn combine_type(
    parts: [Option<NodeId>; 4],
    at: u16,
    input: &dyn Fn(NodeId) -> Result<ValueType, MaterialError>,
) -> Result<ValueType, MaterialError> {
    let range = MaterialError::OutOfRange { node: Some(at) };
    let mut count = 0u8;
    let mut ended = false;
    for part in parts {
        match part {
            Some(id) if !ended => {
                if input(id)? != ValueType::F32 {
                    return Err(MaterialError::TypeMismatch { node: at });
                }
                count += 1;
            }
            Some(_) => return Err(range),
            None => ended = true,
        }
    }
    if count < 2 {
        return Err(range);
    }
    ValueType::with_components(count).ok_or(range)
}

fn node_type(
    node: &Node,
    at: u16,
    types: &[ValueType],
    slots: &mut [bool; TEXTURE_SLOTS as usize],
) -> Result<ValueType, MaterialError> {
    let input = |id: NodeId| input_type(types, at, id);
    let mismatch = MaterialError::TypeMismatch { node: at };
    let range = MaterialError::OutOfRange { node: Some(at) };
    let binary = |a: NodeId, b: NodeId| -> Result<ValueType, MaterialError> {
        let (ta, tb) = (input(a)?, input(b)?);
        match (ta, tb) {
            (x, y) if x == y => Ok(x),
            (x, ValueType::F32) | (ValueType::F32, x) => Ok(x),
            _ => Err(MaterialError::TypeMismatch { node: at }),
        }
    };
    Ok(match *node {
        Node::Uv0 | Node::Uv1 => ValueType::Vec2,
        Node::WorldPosition | Node::WorldNormal | Node::ViewDirection => ValueType::Vec3,
        Node::Time => ValueType::F32,
        Node::Constant(v, ty) => {
            let used = usize::from(ty.components());
            // Unused components must be zero so every graph has one canonical encoding.
            if !finite(&v) || v.iter().skip(used).any(|c| c.to_bits() != 0) {
                return Err(range);
            }
            ty
        }
        Node::ScalarParam(i) => {
            if i >= SCALAR_PARAMS {
                return Err(range);
            }
            ValueType::F32
        }
        Node::ColorParam(i) => {
            if i >= COLOR_PARAMS {
                return Err(range);
            }
            ValueType::Vec4
        }
        Node::Texture { slot, uv } => {
            if input(uv)? != ValueType::Vec2 {
                return Err(mismatch);
            }
            let s = slots.get_mut(usize::from(slot)).ok_or(range)?;
            *s = true;
            ValueType::Vec4
        }
        Node::Add(a, b) | Node::Subtract(a, b) | Node::Multiply(a, b) | Node::Divide(a, b) => binary(a, b)?,
        Node::Lerp(a, b, t) => {
            let ty = input(a)?;
            if input(b)? != ty {
                return Err(mismatch);
            }
            let tt = input(t)?;
            if tt != ValueType::F32 && tt != ty {
                return Err(mismatch);
            }
            ty
        }
        Node::Dot(a, b) => {
            let ty = input(a)?;
            if ty == ValueType::F32 || input(b)? != ty {
                return Err(mismatch);
            }
            ValueType::F32
        }
        Node::Normalize(a) => {
            let ty = input(a)?;
            if ty == ValueType::F32 {
                return Err(mismatch);
            }
            ty
        }
        Node::Saturate(a) | Node::OneMinus(a) => input(a)?,
        Node::Power(a, e) => {
            if input(e)? != ValueType::F32 {
                return Err(mismatch);
            }
            input(a)?
        }
        Node::Swizzle(a, idx, len) => {
            let n = input(a)?.components();
            let picked = idx.get(..usize::from(len)).ok_or(range)?;
            if picked.iter().any(|c| *c >= n) {
                return Err(range);
            }
            ValueType::with_components(len).ok_or(range)?
        }
        Node::Combine(parts) => combine_type(parts, at, &input)?,
        Node::Fresnel(power) => {
            if input(power)? != ValueType::F32 {
                return Err(mismatch);
            }
            ValueType::F32
        }
    })
}

// ---------------------------------------------------------------------------------------
// Permutations
// ---------------------------------------------------------------------------------------

/// A render pass a material is compiled for.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, PartialOrd, Ord)]
pub enum Pass {
    /// Lit color.
    Forward,
    /// Depth only (prepass).
    Depth,
    /// Shadow-map depth.
    Shadow,
    /// Inverted-hull outline.
    Outline,
}

impl Pass {
    const ALL: [Pass; 4] = [Pass::Forward, Pass::Depth, Pass::Shadow, Pass::Outline];

    fn bits(self) -> u32 {
        match self {
            Pass::Forward => 0,
            Pass::Depth => 1,
            Pass::Shadow => 2,
            Pass::Outline => 3,
        }
    }
}

const KEY_LIGHTMAPPED: u32 = 1 << 8;
const KEY_ALPHA_TEST: u32 = 1 << 9;
const KEY_BINDLESS: u32 = 1 << 10;
const KEY_DEFORM_SHIFT: u32 = 11;
const KEY_DEFORM: u32 = 0b11 << KEY_DEFORM_SHIFT;
const KEY_KNOWN: u32 = 0b11 | KEY_LIGHTMAPPED | KEY_ALPHA_TEST | KEY_BINDLESS | KEY_DEFORM;

/// How a permutation's vertices are deformed.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, PartialOrd, Ord)]
pub enum Deform {
    /// Rigid: the instance transform only.
    Static,
    /// Linear-blend skinning from a bone palette (the near crowd tier).
    Skinned,
    /// Positions and normals read from a baked vertex animation (mid and far tiers).
    Vat,
}

impl Deform {
    /// Every deformation, ascending.
    pub const ALL: [Deform; 3] = [Deform::Static, Deform::Skinned, Deform::Vat];

    fn bits(self) -> u32 {
        let v = match self {
            Deform::Static => 0,
            Deform::Skinned => 1,
            Deform::Vat => 2,
        };
        v << KEY_DEFORM_SHIFT
    }
}

/// Deformations a material supports besides static geometry (character materials enable
/// them; the cook then compiles the extra permutations).
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, Default)]
pub struct Deformations {
    /// Skinned meshes.
    pub skinned: bool,
    /// Vertex animation textures.
    pub vat: bool,
}

impl Deformations {
    /// Static geometry only.
    pub const NONE: Deformations = Deformations {
        skinned: false,
        vat: false,
    };
    /// Skinning and vertex animation.
    pub const ALL: Deformations = Deformations {
        skinned: true,
        vat: true,
    };

    /// Whether `d` is supported (static always is).
    pub fn supports(self, d: Deform) -> bool {
        match d {
            Deform::Static => true,
            Deform::Skinned => self.skinned,
            Deform::Vat => self.vat,
        }
    }
}

/// One shader permutation: a pass and its feature switches, packed in a `u32` (bits 0-1
/// pass, bit 8 lightmapped, bit 9 alpha test, bit 10 bindless textures, bits 11-12
/// deformation: 0 static, 1 skinned, 2 vertex animation).
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, PartialOrd, Ord)]
pub struct PermutationKey(u32);

impl PermutationKey {
    /// A key; `None` for impossible combinations (lightmapping outside the forward pass).
    pub fn new(pass: Pass, lightmapped: bool, alpha_test: bool, bindless: bool) -> Option<Self> {
        if lightmapped && pass != Pass::Forward {
            return None;
        }
        let mut bits = pass.bits();
        if lightmapped {
            bits |= KEY_LIGHTMAPPED;
        }
        if alpha_test {
            bits |= KEY_ALPHA_TEST;
        }
        if bindless {
            bits |= KEY_BINDLESS;
        }
        Some(Self(bits))
    }

    /// This key with a deformation; `None` when lightmapped (deformed geometry is never
    /// lightmapped).
    #[must_use]
    pub fn with_deform(self, deform: Deform) -> Option<Self> {
        if deform != Deform::Static && self.lightmapped() {
            return None;
        }
        Some(Self((self.0 & !KEY_DEFORM) | deform.bits()))
    }

    /// The deformation.
    pub fn deform(self) -> Deform {
        Deform::ALL
            .into_iter()
            .find(|d| d.bits() == self.0 & KEY_DEFORM)
            .unwrap_or(Deform::Static)
    }

    /// From packed bits; `None` for unknown bits or impossible combinations.
    pub fn from_bits(bits: u32) -> Option<Self> {
        if bits & !KEY_KNOWN != 0 {
            return None;
        }
        let pass = Pass::ALL.into_iter().find(|p| p.bits() == bits & 0b11)?;
        let deform = Deform::ALL.into_iter().find(|d| d.bits() == bits & KEY_DEFORM)?;
        Self::new(
            pass,
            bits & KEY_LIGHTMAPPED != 0,
            bits & KEY_ALPHA_TEST != 0,
            bits & KEY_BINDLESS != 0,
        )?
        .with_deform(deform)
    }

    /// Packed bits.
    pub fn bits(self) -> u32 {
        self.0
    }

    /// The pass.
    pub fn pass(self) -> Pass {
        Pass::ALL
            .into_iter()
            .find(|p| p.bits() == self.0 & 0b11)
            .unwrap_or(Pass::Forward)
    }

    /// Lightmapped (forward only).
    pub fn lightmapped(self) -> bool {
        self.0 & KEY_LIGHTMAPPED != 0
    }

    /// Alpha tested.
    pub fn alpha_test(self) -> bool {
        self.0 & KEY_ALPHA_TEST != 0
    }

    /// Bindless textures.
    pub fn bindless(self) -> bool {
        self.0 & KEY_BINDLESS != 0
    }
}

/// Texture flag: the texture holds sRGB color.
pub const TEXTURE_SRGB: u32 = 1;
/// Texture flag: the texture is a tangent-space normal map.
pub const TEXTURE_NORMAL_MAP: u32 = 2;
/// Longest parameter name in bytes.
pub const MAX_PARAM_NAME: usize = 31;

/// The texture of one slot: its content hash and what the material expects of it.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct TextureRef {
    /// Content hash of the MTEX payload.
    pub hash: ContentHash,
    /// [`TEXTURE_SRGB`] and [`TEXTURE_NORMAL_MAP`] (not both); must match the texture's
    /// own flags (checked by the cook and again at load).
    pub flags: u32,
}

/// A named scalar parameter and its default.
#[derive(Clone, PartialEq, Debug)]
pub struct ScalarDefault {
    /// Name: 1 to [`MAX_PARAM_NAME`] bytes of `[a-z0-9_]`, unique among scalars.
    pub name: String,
    /// Default value (finite).
    pub value: f32,
}

/// A named color parameter and its default (linear RGBA).
#[derive(Clone, PartialEq, Debug)]
pub struct ColorDefault {
    /// Name: 1 to [`MAX_PARAM_NAME`] bytes of `[a-z0-9_]`, unique among colors.
    pub name: String,
    /// Default value (finite).
    pub value: [f32; 4],
}

/// What a material binds besides its graph: the texture of each slot and every
/// parameter's name and default (version 2). Each table holds exactly one entry per index
/// from 0 to the highest the graph reads (texture nodes and the toon ramp for textures),
/// so indices never dangle and every parameter has a default.
#[derive(Clone, PartialEq, Debug, Default)]
pub struct MaterialBindings {
    /// Texture of each slot, in slot order.
    pub textures: Vec<TextureRef>,
    /// Scalar parameters, in index order.
    pub scalars: Vec<ScalarDefault>,
    /// Color parameters, in index order.
    pub colors: Vec<ColorDefault>,
}

/// A material asset: the graph plus asset-level flags and bindings.
#[derive(Clone, PartialEq, Debug)]
pub struct MaterialAsset {
    /// The validated graph.
    pub graph: TypedGraph,
    /// Whether the material is drawn into shadow maps.
    pub casts_shadows: bool,
    /// Supported deformations besides static geometry.
    pub deformations: Deformations,
    /// Textures and parameter defaults.
    pub bindings: MaterialBindings,
}

fn valid_param_name(name: &str) -> bool {
    (1..=MAX_PARAM_NAME).contains(&name.len())
        && name
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_')
}

fn unique<'a>(names: impl Iterator<Item = &'a str>) -> Option<&'a str> {
    let mut seen = std::collections::BTreeSet::new();
    names.into_iter().find(|n| !seen.insert(*n))
}

impl MaterialAsset {
    /// The table sizes the graph needs: `(textures, scalars, colors)`, each 1 + the
    /// highest index read (the toon ramp's slot counts as a texture read).
    pub fn required_bindings(&self) -> (usize, usize, usize) {
        let g = &self.graph.graph;
        let mut need = (0usize, 0usize, 0usize);
        let up = |n: &mut usize, i: u8| *n = (*n).max(usize::from(i) + 1);
        for node in &g.nodes {
            match node {
                Node::Texture { slot, .. } => up(&mut need.0, *slot),
                Node::ScalarParam(i) => up(&mut need.1, *i),
                Node::ColorParam(i) => up(&mut need.2, *i),
                _ => {}
            }
        }
        if let LightingModel::ToonRamp { slot } = g.lighting {
            up(&mut need.0, slot);
        }
        need
    }

    /// Why the bindings do not fit the graph and the format's rules, or `None`.
    pub fn bindings_error(&self) -> Option<String> {
        let b = &self.bindings;
        let (textures, scalars, colors) = self.required_bindings();
        if b.textures.len() != textures {
            return Some(format!(
                "{} textures listed; the graph reads slots 0 to {} ({textures} needed)",
                b.textures.len(),
                textures.saturating_sub(1)
            ));
        }
        if b.scalars.len() != scalars {
            return Some(format!(
                "{} scalar parameters defined; the graph reads {scalars}",
                b.scalars.len()
            ));
        }
        if b.colors.len() != colors {
            return Some(format!(
                "{} color parameters defined; the graph reads {colors}",
                b.colors.len()
            ));
        }
        if let Some(t) = b.textures.iter().find(|t| {
            t.flags & !(TEXTURE_SRGB | TEXTURE_NORMAL_MAP) != 0
                || t.flags == TEXTURE_SRGB | TEXTURE_NORMAL_MAP
        }) {
            return Some(format!("texture flags {:#x} are invalid", t.flags));
        }
        let names = b
            .scalars
            .iter()
            .map(|p| p.name.as_str())
            .chain(b.colors.iter().map(|p| p.name.as_str()));
        if let Some(bad) = names.clone().find(|n| !valid_param_name(n)) {
            return Some(format!(
                "parameter name `{bad}` must be 1 to {MAX_PARAM_NAME} bytes of a-z, 0-9, _"
            ));
        }
        if let Some(d) = unique(b.scalars.iter().map(|p| p.name.as_str()))
            .or_else(|| unique(b.colors.iter().map(|p| p.name.as_str())))
        {
            return Some(format!("parameter `{d}` is defined twice"));
        }
        let finite = b.scalars.iter().all(|p| p.value.is_finite())
            && b.colors.iter().all(|p| p.value.iter().all(|v| v.is_finite()));
        if !finite {
            return Some("parameter defaults must be finite".to_owned());
        }
        None
    }

    /// The index of the scalar parameter named `name`.
    pub fn scalar_index(&self, name: &str) -> Option<u8> {
        let i = self.bindings.scalars.iter().position(|p| p.name == name)?;
        u8::try_from(i).ok()
    }

    /// The index of the color parameter named `name`.
    pub fn color_index(&self, name: &str) -> Option<u8> {
        let i = self.bindings.colors.iter().position(|p| p.name == name)?;
        u8::try_from(i).ok()
    }

    /// The passes this material is drawn in.
    pub fn passes(&self) -> Vec<Pass> {
        let mut p = vec![Pass::Forward, Pass::Depth];
        if self.casts_shadows {
            p.push(Pass::Shadow);
        }
        if self.graph.graph.outline.is_some() {
            p.push(Pass::Outline);
        }
        p
    }

    /// The permutation a renderer asks for, or `None` when the material is not drawn in
    /// `pass`. `lightmapped` is ignored outside the forward pass.
    pub fn request(&self, pass: Pass, lightmapped: bool, bindless: bool) -> Option<PermutationKey> {
        if !self.passes().contains(&pass) {
            return None;
        }
        let alpha_test = self.graph.graph.outputs.alpha_cutoff.is_some();
        PermutationKey::new(pass, lightmapped && pass == Pass::Forward, alpha_test, bindless)
    }

    /// The permutation for deformed geometry, or `None` when the material is not drawn in
    /// `pass` or does not support `deform`. Deformed geometry is never lightmapped.
    pub fn request_deformed(&self, pass: Pass, bindless: bool, deform: Deform) -> Option<PermutationKey> {
        if !self.deformations.supports(deform) {
            return None;
        }
        self.request(pass, false, bindless)?.with_deform(deform)
    }

    /// Every permutation the cook compiles, ascending: every pass the material is drawn
    /// in, with and without bindless textures, for the forward pass with and without
    /// lightmapping, and for each supported deformation (never lightmapped).
    pub fn canonical_permutations(&self) -> Vec<PermutationKey> {
        let mut out = Vec::new();
        for pass in self.passes() {
            for lightmapped in [false, true] {
                if lightmapped && pass != Pass::Forward {
                    continue;
                }
                for bindless in [false, true] {
                    if let Some(k) = self.request(pass, lightmapped, bindless) {
                        out.push(k);
                    }
                }
            }
            for deform in [Deform::Skinned, Deform::Vat] {
                for bindless in [false, true] {
                    if let Some(k) = self.request_deformed(pass, bindless, deform) {
                        out.push(k);
                    }
                }
            }
        }
        out.sort_unstable();
        out
    }
}

// ---------------------------------------------------------------------------------------
// Binary format
// ---------------------------------------------------------------------------------------
//
// Layout (little-endian):
//
// | offset | size | field |
// |---|---|---|
// | 0 | 4 | magic `"MMAT"` |
// | 4 | 2 | version `u16` = 1 |
// | 6 | 2 | flags `u16`: bit 0 casts shadows, bit 1 supports skinning, bit 2 supports vertex animation; other bits 0 |
// | 8 | 4 | node count `u32`, 1 to 256 |
// | 12 | 1 | lighting model `u8`: 0 unlit, 1 Lambert, 2 toon, 3 toon ramp |
// | 13 | 1 | toon bands `u8` (toon only, else 0) |
// | 14 | 1 | ramp slot `u8` (toon ramp only, else 0) |
// | 15 | 1 | reserved, 0 |
// | 16 | 4 | toon softness `f32` (toon only, else 0) |
// | 20 | 12 | toon shadow tint `[f32; 3]` (toon only, else 0) |
// | 32 | 4 | rim present `u32` (0 or 1) |
// | 36 | 20 | rim color `[f32; 3]`, power `f32`, intensity `f32` (all 0 when absent) |
// | 56 | 4 | outline present `u32` (0 or 1) |
// | 60 | 16 | outline width `f32`, color `[f32; 3]` (all 0 when absent) |
// | 76 | 2 | base color node `u16` |
// | 78 | 2 | alpha node `u16` (0xFFFF: none) |
// | 80 | 2 | emissive node `u16` (0xFFFF: none) |
// | 82 | 2 | alpha cutoff present `u16` (0 or 1) |
// | 84 | 4 | alpha cutoff `f32` (0 when absent) |
// | 88 | 4 | permutation count `u32` |
// | 92 | | permutation keys `u32`, ascending; must equal the canonical set exactly |
// | | | nodes, 28 bytes each: op `u8`, aux `u8`, type `u8`, swizzle `u8`, inputs `[u16; 4]`, values `[f32; 4]` |
//
// Node ops: 0 uv0, 1 uv1, 2 world position, 3 world normal, 4 view direction, 5 time,
// 6 constant (type = component count, values), 7 scalar parameter (aux), 8 color
// parameter (aux), 9 texture (aux slot, input 0 uv), 10 add, 11 subtract, 12 multiply,
// 13 divide (inputs 0 and 1), 14 lerp (inputs 0 to 2), 15 dot, 16 normalize, 17 saturate,
// 18 one minus, 19 power (inputs 0 and 1), 20 swizzle (input 0, aux length, swizzle =
// four 2-bit indices, low first), 21 combine (inputs 0 to 3, unused 0xFFFF), 22 fresnel
// (input 0). Every field an op does not use is zero (unused inputs 0xFFFF), so each node
// has exactly one encoding: the parser re-encodes every decoded node and rejects any
// difference, and likewise re-encodes the whole asset.

/// Magic bytes.
pub const MAGIC: [u8; 4] = *b"MMAT";
/// The version this crate writes. Version 1 (no binding tables) still parses, with empty
/// bindings.
pub const VERSION: u16 = 2;
const NONE: u16 = 0xFFFF;
const NODE_BYTES: usize = 28;

struct NodeRecord {
    op: u8,
    aux: u8,
    ty: u8,
    swizzle: u8,
    inputs: [u16; 4],
    values: [f32; 4],
}

impl NodeRecord {
    fn new(op: u8) -> Self {
        Self {
            op,
            aux: 0,
            ty: 0,
            swizzle: 0,
            inputs: [NONE; 4],
            values: [0.0; 4],
        }
    }

    fn with_inputs(op: u8, inputs: &[NodeId]) -> Self {
        let mut r = Self::new(op);
        for (slot, n) in r.inputs.iter_mut().zip(inputs) {
            *slot = n.0;
        }
        r
    }
}

fn encode_node(node: &Node) -> NodeRecord {
    match *node {
        Node::Uv0 => NodeRecord::new(0),
        Node::Uv1 => NodeRecord::new(1),
        Node::WorldPosition => NodeRecord::new(2),
        Node::WorldNormal => NodeRecord::new(3),
        Node::ViewDirection => NodeRecord::new(4),
        Node::Time => NodeRecord::new(5),
        Node::Constant(values, ty) => NodeRecord {
            ty: ty.components(),
            values,
            ..NodeRecord::new(6)
        },
        Node::ScalarParam(i) => NodeRecord {
            aux: i,
            ..NodeRecord::new(7)
        },
        Node::ColorParam(i) => NodeRecord {
            aux: i,
            ..NodeRecord::new(8)
        },
        Node::Texture { slot, uv } => NodeRecord {
            aux: slot,
            ..NodeRecord::with_inputs(9, &[uv])
        },
        Node::Add(a, b) => NodeRecord::with_inputs(10, &[a, b]),
        Node::Subtract(a, b) => NodeRecord::with_inputs(11, &[a, b]),
        Node::Multiply(a, b) => NodeRecord::with_inputs(12, &[a, b]),
        Node::Divide(a, b) => NodeRecord::with_inputs(13, &[a, b]),
        Node::Lerp(a, b, t) => NodeRecord::with_inputs(14, &[a, b, t]),
        Node::Dot(a, b) => NodeRecord::with_inputs(15, &[a, b]),
        Node::Normalize(a) => NodeRecord::with_inputs(16, &[a]),
        Node::Saturate(a) => NodeRecord::with_inputs(17, &[a]),
        Node::OneMinus(a) => NodeRecord::with_inputs(18, &[a]),
        Node::Power(a, e) => NodeRecord::with_inputs(19, &[a, e]),
        Node::Swizzle(a, idx, len) => {
            let swizzle = idx
                .iter()
                .take(usize::from(len))
                .enumerate()
                .fold(0u8, |acc, (i, c)| acc | ((c & 3) << (2 * i)));
            NodeRecord {
                aux: len,
                swizzle,
                ..NodeRecord::with_inputs(20, &[a])
            }
        }
        Node::Combine(parts) => {
            let mut r = NodeRecord::new(21);
            for (slot, p) in r.inputs.iter_mut().zip(parts) {
                *slot = p.map_or(NONE, |n| n.0);
            }
            r
        }
        Node::Fresnel(p) => NodeRecord::with_inputs(22, &[p]),
    }
}

fn decode_node(r: &NodeRecord) -> Result<Node, FormatError> {
    let [in0, in1, in2, _] = r.inputs.map(NodeId);
    Ok(match r.op {
        0 => Node::Uv0,
        1 => Node::Uv1,
        2 => Node::WorldPosition,
        3 => Node::WorldNormal,
        4 => Node::ViewDirection,
        5 => Node::Time,
        6 => Node::Constant(
            r.values,
            ValueType::with_components(r.ty).ok_or(FormatError::Encoding(u32::from(r.ty)))?,
        ),
        7 => Node::ScalarParam(r.aux),
        8 => Node::ColorParam(r.aux),
        9 => Node::Texture { slot: r.aux, uv: in0 },
        10 => Node::Add(in0, in1),
        11 => Node::Subtract(in0, in1),
        12 => Node::Multiply(in0, in1),
        13 => Node::Divide(in0, in1),
        14 => Node::Lerp(in0, in1, in2),
        15 => Node::Dot(in0, in1),
        16 => Node::Normalize(in0),
        17 => Node::Saturate(in0),
        18 => Node::OneMinus(in0),
        19 => Node::Power(in0, in1),
        20 => {
            let s = r.swizzle;
            Node::Swizzle(in0, [s & 3, (s >> 2) & 3, (s >> 4) & 3, (s >> 6) & 3], r.aux)
        }
        21 => Node::Combine(r.inputs.map(|i| (i != NONE).then_some(NodeId(i)))),
        22 => Node::Fresnel(in0),
        other => return Err(FormatError::Encoding(u32::from(other))),
    })
}

fn write_record(w: &mut Writer, r: &NodeRecord) {
    w.u8(r.op);
    w.u8(r.aux);
    w.u8(r.ty);
    w.u8(r.swizzle);
    for i in r.inputs {
        w.u16(i);
    }
    for v in r.values {
        w.f32(v);
    }
}

fn read_record(raw: &[u8]) -> Result<NodeRecord, FormatError> {
    let mut rr = Reader::new(raw);
    Ok(NodeRecord {
        op: rr.u8()?,
        aux: rr.u8()?,
        ty: rr.u8()?,
        swizzle: rr.u8()?,
        inputs: [rr.u16()?, rr.u16()?, rr.u16()?, rr.u16()?],
        values: [rr.f32()?, rr.f32()?, rr.f32()?, rr.f32()?],
    })
}

fn opt_node(v: u16) -> Option<NodeId> {
    (v != NONE).then_some(NodeId(v))
}

/// The fixed header fields after the node count.
struct Header {
    casts_shadows: bool,
    deformations: Deformations,
    lighting: LightingModel,
    rim: Option<RimLight>,
    outline: Option<Outline>,
    outputs: SurfaceOutputs,
    keys: Vec<PermutationKey>,
    node_count: u32,
    version: u16,
}

fn read_header(r: &mut Reader<'_>) -> Result<Header, FormatError> {
    if r.array::<4>()? != MAGIC {
        return Err(FormatError::Magic);
    }
    let version = r.u16()?;
    if version != 1 && version != VERSION {
        return Err(FormatError::Version(version));
    }
    let flags = r.u16()?;
    if flags & !0b111 != 0 {
        return Err(FormatError::Flags(u32::from(flags)));
    }
    let node_count = r.u32()?;
    if node_count == 0 || node_count as usize > MAX_NODES {
        return Err(FormatError::Dimensions);
    }
    let (model, bands, ramp_slot, _reserved) = (r.u8()?, r.u8()?, r.u8()?, r.u8()?);
    let softness = r.f32()?;
    let shadow_tint = r.vec3()?;
    let lighting = match model {
        0 => LightingModel::Unlit,
        1 => LightingModel::Lambert,
        2 => LightingModel::Toon {
            bands,
            softness,
            shadow_tint,
        },
        3 => LightingModel::ToonRamp { slot: ramp_slot },
        other => return Err(FormatError::Encoding(u32::from(other))),
    };
    let present = |v: u32| match v {
        0 => Ok(false),
        1 => Ok(true),
        _ => Err(FormatError::Inconsistent),
    };
    let rim_present = present(r.u32()?)?;
    let rim = RimLight {
        color: r.vec3()?,
        power: r.f32()?,
        intensity: r.f32()?,
    };
    let outline_present = present(r.u32()?)?;
    let outline = Outline {
        width_px: r.f32()?,
        color: r.vec3()?,
    };
    let base_color = NodeId(r.u16()?);
    let (alpha, emissive) = (opt_node(r.u16()?), opt_node(r.u16()?));
    let cutoff_present = present(u32::from(r.u16()?))?;
    let cutoff = r.f32()?;
    let key_count = r.u32()?;
    if key_count > 64 {
        return Err(FormatError::Dimensions);
    }
    let mut keys = Vec::with_capacity(key_count as usize);
    for _ in 0..key_count {
        keys.push(PermutationKey::from_bits(r.u32()?).ok_or(FormatError::Inconsistent)?);
    }
    Ok(Header {
        casts_shadows: flags & 1 != 0,
        deformations: Deformations {
            skinned: flags & 2 != 0,
            vat: flags & 4 != 0,
        },
        lighting,
        rim: rim_present.then_some(rim),
        outline: outline_present.then_some(outline),
        outputs: SurfaceOutputs {
            base_color,
            alpha,
            emissive,
            alpha_cutoff: cutoff_present.then_some(cutoff),
        },
        keys,
        node_count,
        version,
    })
}

fn read_name(r: &mut Reader<'_>) -> Result<String, FormatError> {
    let len = usize::from(r.u8()?);
    let bytes = r.slice(len)?;
    String::from_utf8(bytes.to_vec()).map_err(|_| FormatError::Inconsistent)
}

fn read_bindings(r: &mut Reader<'_>) -> Result<MaterialBindings, FormatError> {
    let mut b = MaterialBindings::default();
    let count = |r: &mut Reader<'_>, max: u8| -> Result<u8, FormatError> {
        let n = r.u8()?;
        if r.array::<3>()? != [0; 3] {
            return Err(FormatError::Reserved);
        }
        if n > max {
            return Err(FormatError::Dimensions);
        }
        Ok(n)
    };
    for _ in 0..count(r, TEXTURE_SLOTS)? {
        b.textures.push(TextureRef {
            hash: ContentHash::from_bytes(r.array::<32>()?),
            flags: r.u32()?,
        });
    }
    for _ in 0..count(r, SCALAR_PARAMS)? {
        let value = r.f32()?;
        b.scalars.push(ScalarDefault {
            name: read_name(r)?,
            value,
        });
    }
    for _ in 0..count(r, COLOR_PARAMS)? {
        let value = [r.f32()?, r.f32()?, r.f32()?, r.f32()?];
        b.colors.push(ColorDefault {
            name: read_name(r)?,
            value,
        });
    }
    Ok(b)
}

fn write_bindings(w: &mut Writer, b: &MaterialBindings) {
    let count = |w: &mut Writer, n: usize| {
        w.u8(u8::try_from(n).unwrap_or(u8::MAX));
        w.bytes(&[0; 3]);
    };
    count(w, b.textures.len());
    for t in &b.textures {
        w.bytes(t.hash.as_bytes());
        w.u32(t.flags);
    }
    let name = |w: &mut Writer, n: &str| {
        w.u8(u8::try_from(n.len()).unwrap_or(u8::MAX));
        w.bytes(n.as_bytes());
    };
    count(w, b.scalars.len());
    for p in &b.scalars {
        w.f32(p.value);
        name(w, &p.name);
    }
    count(w, b.colors.len());
    for p in &b.colors {
        for v in p.value {
            w.f32(v);
        }
        name(w, &p.name);
    }
}

impl MaterialAsset {
    /// Parses and validates a material asset: structure, every node (canonical encoding
    /// and types), the outputs and settings, and the permutation list.
    ///
    /// # Errors
    /// [`FormatError`]; graph validation failures map to [`FormatError::Inconsistent`],
    /// non-canonical encodings to [`FormatError::Reserved`].
    pub fn parse(bytes: &[u8]) -> Result<MaterialAsset, FormatError> {
        let mut r = Reader::new(bytes);
        let header = read_header(&mut r)?;
        let mut nodes = Vec::with_capacity(header.node_count as usize);
        for _ in 0..header.node_count {
            let raw = r.slice(NODE_BYTES)?;
            let node = decode_node(&read_record(raw)?)?;
            let mut canonical = Writer::new();
            write_record(&mut canonical, &encode_node(&node));
            if canonical.into_bytes().as_slice() != raw {
                return Err(FormatError::Reserved);
            }
            nodes.push(node);
        }
        let bindings = if header.version >= 2 {
            read_bindings(&mut r)?
        } else {
            MaterialBindings::default()
        };
        r.finish()?;
        let graph = MaterialGraph {
            nodes,
            outputs: header.outputs,
            lighting: header.lighting,
            rim: header.rim,
            outline: header.outline,
        };
        let asset = MaterialAsset {
            graph: graph.validate().map_err(|_| FormatError::Inconsistent)?,
            casts_shadows: header.casts_shadows,
            deformations: header.deformations,
            bindings,
        };
        if header.keys != asset.canonical_permutations() {
            return Err(FormatError::Inconsistent);
        }
        if header.version >= 2 && asset.bindings_error().is_some() {
            return Err(FormatError::Inconsistent);
        }
        // Absent sections and unused settings must be zero: one encoding per asset.
        if asset.encode_version(header.version).as_slice() != bytes {
            return Err(FormatError::Reserved);
        }
        Ok(asset)
    }

    /// Reference encoder (version 2), including the canonical permutation list and the
    /// binding tables.
    pub fn encode(&self) -> Vec<u8> {
        self.encode_version(VERSION)
    }

    /// The encoding of `version` (1 omits the binding tables).
    fn encode_version(&self, version: u16) -> Vec<u8> {
        let g = &self.graph.graph;
        let mut w = Writer::new();
        w.bytes(&MAGIC);
        w.u16(version);
        let flags = u16::from(self.casts_shadows)
            | u16::from(self.deformations.skinned) << 1
            | u16::from(self.deformations.vat) << 2;
        w.u16(flags);
        w.count(g.nodes.len());
        let (model, bands, ramp, softness, tint) = match g.lighting {
            LightingModel::Unlit => (0, 0, 0, 0.0, [0.0; 3]),
            LightingModel::Lambert => (1, 0, 0, 0.0, [0.0; 3]),
            LightingModel::Toon {
                bands,
                softness,
                shadow_tint,
            } => (2, bands, 0, softness, shadow_tint),
            LightingModel::ToonRamp { slot } => (3, 0, slot, 0.0, [0.0; 3]),
        };
        w.u8(model);
        w.u8(bands);
        w.u8(ramp);
        w.u8(0);
        w.f32(softness);
        w.vec3(tint);
        let rim = g.rim.unwrap_or(RimLight {
            color: [0.0; 3],
            power: 0.0,
            intensity: 0.0,
        });
        w.u32(u32::from(g.rim.is_some()));
        w.vec3(rim.color);
        w.f32(rim.power);
        w.f32(rim.intensity);
        let outline = g.outline.unwrap_or(Outline {
            width_px: 0.0,
            color: [0.0; 3],
        });
        w.u32(u32::from(g.outline.is_some()));
        w.f32(outline.width_px);
        w.vec3(outline.color);
        w.u16(g.outputs.base_color.0);
        w.u16(g.outputs.alpha.map_or(NONE, |n| n.0));
        w.u16(g.outputs.emissive.map_or(NONE, |n| n.0));
        w.u16(u16::from(g.outputs.alpha_cutoff.is_some()));
        w.f32(g.outputs.alpha_cutoff.unwrap_or(0.0));
        let keys = self.canonical_permutations();
        w.count(keys.len());
        for k in keys {
            w.u32(k.bits());
        }
        for n in &g.nodes {
            write_record(&mut w, &encode_node(n));
        }
        if version >= 2 {
            write_bindings(&mut w, &self.bindings);
        }
        w.into_bytes()
    }

    /// The version 1 encoding (no binding tables), for compatibility tests.
    #[doc(hidden)]
    pub fn encode_v1(&self) -> Vec<u8> {
        self.encode_version(1)
    }
}

#[cfg(test)]
mod tests;

/// Reference materials that together use every node, lighting model, and feature. Tests
/// across crates (formats, shadergen, render, cook) share them.
///
/// # Errors
/// Never in practice; a validation failure would be a bug in this list.
pub fn reference_materials() -> Result<Vec<MaterialAsset>, MaterialError> {
    Ok(vec![
        MaterialAsset {
            graph: reference_toon().validate()?,
            casts_shadows: true,
            deformations: Deformations::ALL,
            bindings: MaterialBindings {
                textures: vec![texture(1, TEXTURE_SRGB)],
                scalars: Vec::new(),
                colors: vec![color("tint", [1.0, 0.9, 0.8, 1.0])],
            },
        },
        MaterialAsset {
            graph: reference_foliage().validate()?,
            casts_shadows: true,
            deformations: Deformations::NONE,
            bindings: MaterialBindings {
                textures: vec![texture(2, TEXTURE_SRGB), texture(3, TEXTURE_SRGB), texture(4, 0)],
                scalars: Vec::new(),
                colors: Vec::new(),
            },
        },
        MaterialAsset {
            graph: reference_lambert().validate()?,
            casts_shadows: false,
            deformations: Deformations::NONE,
            bindings: MaterialBindings {
                textures: Vec::new(),
                scalars: vec![scalar("unused", 0.0), scalar("sharpness", 2.0)],
                colors: Vec::new(),
            },
        },
        MaterialAsset {
            graph: reference_unlit().validate()?,
            casts_shadows: false,
            deformations: Deformations::NONE,
            bindings: MaterialBindings {
                textures: Vec::new(),
                scalars: Vec::new(),
                colors: vec![
                    color("c0", [0.0; 4]),
                    color("c1", [0.0; 4]),
                    color("c2", [0.0; 4]),
                    color("glow", [1.0, 0.5, 0.2, 1.0]),
                ],
            },
        },
    ])
}

fn texture(seed: u8, flags: u32) -> TextureRef {
    TextureRef {
        hash: ContentHash::from_bytes([seed; 32]),
        flags,
    }
}

fn scalar(name: &str, value: f32) -> ScalarDefault {
    ScalarDefault {
        name: name.to_owned(),
        value,
    }
}

fn color(name: &str, value: [f32; 4]) -> ColorDefault {
    ColorDefault {
        name: name.to_owned(),
        value,
    }
}

/// A toon character: albedo texture tinted by a color parameter, banded light, rim,
/// outline, shadows; skinned and vertex-animated variants.
fn reference_toon() -> MaterialGraph {
    let n = NodeId;
    MaterialGraph {
        nodes: vec![
            Node::Uv0,
            Node::Texture { slot: 0, uv: n(0) },
            Node::ColorParam(0),
            Node::Multiply(n(1), n(2)),
            Node::Swizzle(n(3), [0, 1, 2, 0], 3),
        ],
        outputs: SurfaceOutputs {
            base_color: n(4),
            alpha: None,
            emissive: None,
            alpha_cutoff: None,
        },
        lighting: LightingModel::Toon {
            bands: 3,
            softness: 0.2,
            shadow_tint: [0.55, 0.5, 0.7],
        },
        rim: Some(RimLight {
            color: [1.0, 0.9, 0.8],
            power: 3.0,
            intensity: 0.6,
        }),
        outline: Some(Outline {
            width_px: 2.0,
            color: [0.05, 0.03, 0.04],
        }),
    }
}

/// Alpha-tested foliage lit through a ramp texture.
fn reference_foliage() -> MaterialGraph {
    let n = NodeId;
    MaterialGraph {
        nodes: vec![
            Node::Uv0,
            Node::Texture { slot: 1, uv: n(0) },
            Node::Swizzle(n(1), [3, 0, 0, 0], 1),
            Node::Swizzle(n(1), [0, 1, 2, 0], 3),
        ],
        outputs: SurfaceOutputs {
            base_color: n(3),
            alpha: Some(n(2)),
            emissive: None,
            alpha_cutoff: Some(0.5),
        },
        lighting: LightingModel::ToonRamp { slot: 2 },
        rim: None,
        outline: None,
    }
}

/// Lambert with procedural terms, using every math node.
fn reference_lambert() -> MaterialGraph {
    let n = NodeId;
    MaterialGraph {
        nodes: vec![
            Node::WorldNormal,
            Node::ViewDirection,
            Node::Dot(n(0), n(1)),
            Node::Saturate(n(2)),
            Node::OneMinus(n(3)),
            Node::ScalarParam(1),
            Node::Power(n(4), n(5)),
            Node::Constant([0.2, 0.4, 0.8, 0.0], ValueType::Vec3),
            Node::Constant([1.0, 0.5, 0.25, 0.0], ValueType::Vec3),
            Node::Lerp(n(7), n(8), n(6)),
            Node::WorldPosition,
            Node::Normalize(n(10)),
            Node::Add(n(9), n(11)),
            Node::Subtract(n(12), n(11)),
            Node::Divide(n(13), n(5)),
            Node::Time,
            Node::Fresnel(n(5)),
            Node::Combine([Some(n(15)), Some(n(16)), Some(n(3)), None]),
            Node::Uv1,
            Node::Swizzle(n(18), [1, 0, 0, 0], 2),
            Node::Multiply(n(17), n(6)),
        ],
        outputs: SurfaceOutputs {
            base_color: n(14),
            alpha: None,
            emissive: Some(n(20)),
            alpha_cutoff: None,
        },
        lighting: LightingModel::Lambert,
        rim: None,
        outline: None,
    }
}

/// An unlit emissive surface.
fn reference_unlit() -> MaterialGraph {
    let n = NodeId;
    MaterialGraph {
        nodes: vec![Node::ColorParam(3), Node::Swizzle(n(0), [0, 1, 2, 0], 3)],
        outputs: SurfaceOutputs {
            base_color: n(1),
            alpha: None,
            emissive: Some(n(1)),
            alpha_cutoff: None,
        },
        lighting: LightingModel::Unlit,
        rim: None,
        outline: None,
    }
}

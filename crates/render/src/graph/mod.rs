//! The render graph (plan 8.3, decision 0012).
//!
//! Passes declare which resources they read and write; the graph orders the passes,
//! culls the ones that contribute nothing, computes every transient resource's lifetime,
//! and aliases transient resources whose lifetimes do not overlap onto the same physical
//! texture or buffer. It never inserts barriers: under `wgpu` the API owns
//! synchronization.
//!
//! # Versions
//!
//! Every write produces a new *version* of a resource, returned as a new handle. A pass
//! reads a specific version. This makes dependencies explicit:
//! - read of version `v` depends on the pass that produced `v` (read after write);
//! - a write producing `v + 1` must follow every reader of `v` (write after read) and the
//!   producer of `v` (write after write);
//! - a [`WriteMode::Load`] write also *needs* `v`'s contents (a data dependency), while a
//!   [`WriteMode::Discard`] write overwrites everything and only needs ordering.
//!
//! Only the latest version of a resource may be written (no forks). Reading an older
//! version is allowed and orders the reader before the next writer; if that is
//! impossible the graph reports a cycle.
//!
//! # Culling
//!
//! A pass is kept if it has side effects or if, through data dependencies, it contributes
//! to an output (a handle passed to [`GraphBuilder::output`]). Ordering-only edges never
//! keep a pass alive.
//!
//! # Order
//!
//! Kept passes are sorted topologically; among passes whose dependencies are satisfied,
//! the one declared first runs first. Declaration order is therefore preserved wherever
//! dependencies allow, and the result is deterministic.
//!
//! Compilation allocates; it runs when the graph's shape changes (start-up, resize,
//! settings), not per frame. The compiled graph is executed every frame.

pub mod exec;

use std::cmp::Reverse;
use std::collections::BinaryHeap;

/// Texture description of a graph resource.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct TextureDesc {
    /// Width in texels.
    pub width: u32,
    /// Height in texels.
    pub height: u32,
    /// Depth (3D) or array layers (2D).
    pub depth_or_array_layers: u32,
    /// Mip levels.
    pub mip_level_count: u32,
    /// Samples per texel.
    pub sample_count: u32,
    /// Dimension.
    pub dimension: wgpu::TextureDimension,
    /// Format.
    pub format: wgpu::TextureFormat,
}

impl TextureDesc {
    /// A single-mip, single-sample 2D texture.
    pub const fn d2(width: u32, height: u32, format: wgpu::TextureFormat) -> Self {
        Self {
            width,
            height,
            depth_or_array_layers: 1,
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format,
        }
    }

    /// Estimated bytes, including every mip.
    pub fn estimated_bytes(&self) -> u64 {
        let (bw, bh) = self.format.block_dimensions();
        let block = u64::from(self.format.block_copy_size(None).unwrap_or(match self.format {
            wgpu::TextureFormat::Depth24PlusStencil8 | wgpu::TextureFormat::Depth32FloatStencil8 => 8,
            _ => 4,
        }));
        let mut total = 0u64;
        for mip in 0..self.mip_level_count.max(1) {
            let w = (self.width >> mip).max(1).div_ceil(bw.max(1));
            let h = (self.height >> mip).max(1).div_ceil(bh.max(1));
            let d = match self.dimension {
                wgpu::TextureDimension::D3 => (self.depth_or_array_layers >> mip).max(1),
                _ => self.depth_or_array_layers.max(1),
            };
            total = total.saturating_add(u64::from(w) * u64::from(h) * u64::from(d) * block);
        }
        total.saturating_mul(u64::from(self.sample_count.max(1)))
    }
}

/// Buffer description of a graph resource.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct BufferDesc {
    /// Size in bytes.
    pub size: u64,
}

/// How a pass uses a texture.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum TextureAccess {
    /// Sampled in a shader.
    Sampled,
    /// Read as a storage texture.
    StorageRead,
    /// Written as a storage texture.
    StorageWrite,
    /// Color attachment.
    ColorTarget,
    /// Depth attachment, read only.
    DepthRead,
    /// Depth attachment, written.
    DepthWrite,
    /// Copy source.
    CopySrc,
    /// Copy destination.
    CopyDst,
}

impl TextureAccess {
    fn usage(self) -> wgpu::TextureUsages {
        match self {
            TextureAccess::Sampled => wgpu::TextureUsages::TEXTURE_BINDING,
            TextureAccess::StorageRead | TextureAccess::StorageWrite => wgpu::TextureUsages::STORAGE_BINDING,
            TextureAccess::ColorTarget | TextureAccess::DepthRead | TextureAccess::DepthWrite => {
                wgpu::TextureUsages::RENDER_ATTACHMENT
            }
            TextureAccess::CopySrc => wgpu::TextureUsages::COPY_SRC,
            TextureAccess::CopyDst => wgpu::TextureUsages::COPY_DST,
        }
    }

    fn is_write(self) -> bool {
        matches!(
            self,
            TextureAccess::StorageWrite
                | TextureAccess::ColorTarget
                | TextureAccess::DepthWrite
                | TextureAccess::CopyDst
        )
    }

    fn valid_for(self, format: wgpu::TextureFormat) -> bool {
        let depth = format.is_depth_stencil_format();
        match self {
            TextureAccess::ColorTarget | TextureAccess::StorageRead | TextureAccess::StorageWrite => !depth,
            TextureAccess::DepthRead | TextureAccess::DepthWrite => depth,
            TextureAccess::Sampled | TextureAccess::CopySrc | TextureAccess::CopyDst => true,
        }
    }
}

/// How a pass uses a buffer.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum BufferAccess {
    /// Uniform buffer.
    Uniform,
    /// Read-only storage buffer.
    StorageRead,
    /// Read-write storage buffer.
    StorageWrite,
    /// Indirect draw or dispatch arguments.
    Indirect,
    /// Vertex buffer.
    Vertex,
    /// Index buffer.
    Index,
    /// Copy source.
    CopySrc,
    /// Copy destination.
    CopyDst,
}

impl BufferAccess {
    fn usage(self) -> wgpu::BufferUsages {
        match self {
            BufferAccess::Uniform => wgpu::BufferUsages::UNIFORM,
            BufferAccess::StorageRead | BufferAccess::StorageWrite => wgpu::BufferUsages::STORAGE,
            BufferAccess::Indirect => wgpu::BufferUsages::INDIRECT,
            BufferAccess::Vertex => wgpu::BufferUsages::VERTEX,
            BufferAccess::Index => wgpu::BufferUsages::INDEX,
            BufferAccess::CopySrc => wgpu::BufferUsages::COPY_SRC,
            BufferAccess::CopyDst => wgpu::BufferUsages::COPY_DST,
        }
    }

    fn is_write(self) -> bool {
        matches!(self, BufferAccess::StorageWrite | BufferAccess::CopyDst)
    }
}

/// Whether a write needs the previous contents.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum WriteMode {
    /// The pass reads or preserves the previous contents (load op `Load`, read-modify-write).
    Load,
    /// The pass overwrites everything (load op `Clear`, full-screen write).
    Discard,
}

/// What kind of work a pass records.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum PassKind {
    /// A render pass.
    Render,
    /// A compute pass.
    Compute,
    /// Copies only.
    Copy,
}

/// Index of a resource in its graph.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, PartialOrd, Ord)]
pub struct ResourceId(u32);

impl ResourceId {
    /// Dense index.
    pub const fn index(self) -> usize {
        self.0 as usize
    }
}

/// Index of a pass in its graph, in declaration order.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, PartialOrd, Ord)]
pub struct PassId(u32);

impl PassId {
    /// Dense index (declaration order).
    pub const fn index(self) -> usize {
        self.0 as usize
    }
}

/// A specific version of a texture resource.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct TextureHandle {
    id: ResourceId,
    version: u32,
}

/// A specific version of a buffer resource.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct BufferHandle {
    id: ResourceId,
    version: u32,
}

impl TextureHandle {
    /// The resource.
    pub const fn resource(self) -> ResourceId {
        self.id
    }
    /// The version.
    pub const fn version(self) -> u32 {
        self.version
    }
}

impl BufferHandle {
    /// The resource.
    pub const fn resource(self) -> ResourceId {
        self.id
    }
    /// The version.
    pub const fn version(self) -> u32 {
        self.version
    }
}

/// Either kind of handle.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum AnyHandle {
    /// A texture version.
    Texture(TextureHandle),
    /// A buffer version.
    Buffer(BufferHandle),
}

impl From<TextureHandle> for AnyHandle {
    fn from(h: TextureHandle) -> Self {
        AnyHandle::Texture(h)
    }
}

impl From<BufferHandle> for AnyHandle {
    fn from(h: BufferHandle) -> Self {
        AnyHandle::Buffer(h)
    }
}

impl AnyHandle {
    fn parts(self) -> (ResourceId, u32) {
        match self {
            AnyHandle::Texture(h) => (h.id, h.version),
            AnyHandle::Buffer(h) => (h.id, h.version),
        }
    }
}

/// Graph construction errors, reported by [`GraphBuilder::compile`].
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum GraphError {
    /// A handle does not belong to this graph or names the wrong resource kind.
    UnknownResource {
        /// Pass that used it, if any.
        pass: Option<String>,
    },
    /// A write targeted a version that is not the latest (a fork).
    StaleWrite {
        /// The pass.
        pass: String,
        /// The resource.
        resource: String,
        /// Version written.
        version: u32,
        /// Latest version.
        latest: u32,
    },
    /// A transient resource was read (or load-written) before anything wrote it.
    ReadBeforeWrite {
        /// The pass.
        pass: String,
        /// The resource.
        resource: String,
    },
    /// An access is impossible for the resource (depth access on a color format, a write
    /// declared as a read, and so on).
    InvalidAccess {
        /// The pass.
        pass: String,
        /// The resource.
        resource: String,
    },
    /// A pass wrote the same resource twice.
    DuplicateWrite {
        /// The pass.
        pass: String,
        /// The resource.
        resource: String,
    },
    /// The dependencies cannot be satisfied by any order.
    Cycle {
        /// Passes left unordered.
        passes: Vec<String>,
    },
}

impl core::fmt::Display for GraphError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            GraphError::UnknownResource { pass } => write!(f, "unknown resource handle (pass {pass:?})"),
            GraphError::StaleWrite {
                pass,
                resource,
                version,
                latest,
            } => {
                write!(f, "pass {pass} writes {resource} v{version}, latest is v{latest}")
            }
            GraphError::ReadBeforeWrite { pass, resource } => {
                write!(f, "pass {pass} reads {resource} before any write")
            }
            GraphError::InvalidAccess { pass, resource } => {
                write!(f, "pass {pass} has an invalid access to {resource}")
            }
            GraphError::DuplicateWrite { pass, resource } => write!(f, "pass {pass} writes {resource} twice"),
            GraphError::Cycle { passes } => write!(f, "dependency cycle among passes {passes:?}"),
        }
    }
}

impl std::error::Error for GraphError {}

#[derive(Clone, Debug)]
enum ResourceKind {
    Texture(TextureDesc),
    Buffer(BufferDesc),
}

#[derive(Clone, Debug, Default)]
struct VersionInfo {
    producer: Option<PassId>,
    readers: Vec<PassId>,
}

#[derive(Clone, Debug)]
struct ResourceNode {
    name: String,
    kind: ResourceKind,
    imported: bool,
    versions: Vec<VersionInfo>,
}

impl ResourceNode {
    fn latest(&self) -> u32 {
        u32::try_from(self.versions.len().saturating_sub(1)).unwrap_or(u32::MAX)
    }
}

#[derive(Clone, Copy, Debug)]
enum Access {
    Texture(TextureAccess),
    Buffer(BufferAccess),
}

#[derive(Clone, Copy, Debug)]
struct Use {
    resource: ResourceId,
    /// Version read, or version produced by a write.
    version: u32,
    access: Access,
    write: Option<WriteMode>,
}

#[derive(Clone, Debug)]
struct PassNode {
    name: String,
    kind: PassKind,
    side_effect: bool,
    uses: Vec<Use>,
}

/// Builds a render graph.
#[derive(Clone, Debug, Default)]
pub struct GraphBuilder {
    resources: Vec<ResourceNode>,
    passes: Vec<PassNode>,
    outputs: Vec<(ResourceId, u32)>,
    errors: Vec<GraphError>,
}

/// Declares one pass's resource uses.
#[derive(Debug)]
pub struct PassBuilder<'a> {
    graph: &'a mut GraphBuilder,
    pass: PassId,
}

impl GraphBuilder {
    /// An empty graph.
    pub fn new() -> Self {
        Self::default()
    }

    fn add_resource(&mut self, name: &str, kind: ResourceKind, imported: bool) -> ResourceId {
        let id = resource_id(self.resources.len());
        self.resources.push(ResourceNode {
            name: name.to_owned(),
            kind,
            imported,
            versions: vec![VersionInfo::default()],
        });
        id
    }

    /// A transient texture: its contents are undefined until a pass writes it, and its
    /// memory may be shared with other transients whose lifetimes do not overlap.
    pub fn create_texture(&mut self, name: &str, desc: TextureDesc) -> TextureHandle {
        TextureHandle {
            id: self.add_resource(name, ResourceKind::Texture(desc), false),
            version: 0,
        }
    }

    /// An external texture (the swapchain image, a persistent history buffer): version 0
    /// holds valid contents, and it is never aliased.
    pub fn import_texture(&mut self, name: &str, desc: TextureDesc) -> TextureHandle {
        TextureHandle {
            id: self.add_resource(name, ResourceKind::Texture(desc), true),
            version: 0,
        }
    }

    /// A transient buffer.
    pub fn create_buffer(&mut self, name: &str, desc: BufferDesc) -> BufferHandle {
        BufferHandle {
            id: self.add_resource(name, ResourceKind::Buffer(desc), false),
            version: 0,
        }
    }

    /// An external buffer.
    pub fn import_buffer(&mut self, name: &str, desc: BufferDesc) -> BufferHandle {
        BufferHandle {
            id: self.add_resource(name, ResourceKind::Buffer(desc), true),
            version: 0,
        }
    }

    /// Starts declaring a pass.
    pub fn add_pass(&mut self, name: &str, kind: PassKind) -> PassBuilder<'_> {
        let pass = pass_id(self.passes.len());
        self.passes.push(PassNode {
            name: name.to_owned(),
            kind,
            side_effect: false,
            uses: Vec::new(),
        });
        PassBuilder { graph: self, pass }
    }

    /// Marks a resource version as a graph output: whatever produces it is kept.
    pub fn output(&mut self, handle: impl Into<AnyHandle>) {
        let (id, v) = handle.into().parts();
        if self.resources.get(id.index()).is_none() {
            self.errors.push(GraphError::UnknownResource { pass: None });
            return;
        }
        self.outputs.push((id, v));
    }

    /// Compiles the graph.
    ///
    /// # Errors
    /// The first declaration error, or [`GraphError::Cycle`].
    pub fn compile(&self) -> Result<CompiledGraph, GraphError> {
        if let Some(e) = self.errors.first() {
            return Err(e.clone());
        }
        let edges = self.edges();
        let keep = self.cull(&edges);
        let order = self.order(&edges, &keep)?;
        let culled: Vec<PassId> = (0..self.passes.len())
            .filter(|p| !keep.get(*p).copied().unwrap_or(false))
            .map(pass_id)
            .collect();
        let usage = self.usage(&order);
        let plan = self.alias(&usage);
        Ok(CompiledGraph {
            order,
            culled,
            pass_names: self.passes.iter().map(|p| p.name.clone()).collect(),
            pass_kinds: self.passes.iter().map(|p| p.kind).collect(),
            resource_names: self.resources.iter().map(|r| r.name.clone()).collect(),
            lifetimes: usage.lifetimes,
            bindings: plan.bindings,
            imported_usage: plan.imported_usage,
            unaliased_bytes: plan.unaliased_bytes,
            aliased_bytes: plan.physical.iter().map(PhysicalResource::estimated_bytes).sum(),
            physical: plan.physical,
        })
    }

    /// Dependency edges `(from, to, keeps_alive)` between pass indices.
    fn edges(&self) -> Vec<(usize, usize, bool)> {
        let mut edges = Vec::new();
        for (pi, pass) in self.passes.iter().enumerate() {
            for u in &pass.uses {
                let Some(res) = self.resources.get(u.resource.index()) else {
                    continue;
                };
                let version = u.version as usize;
                match u.write {
                    None => {
                        if let Some(p) = res.versions.get(version).and_then(|v| v.producer) {
                            edges.push((p.index(), pi, true));
                        }
                        // Readers of a version precede the writer of the next one.
                        if let Some(next) = res.versions.get(version + 1).and_then(|v| v.producer)
                            && next.index() != pi
                        {
                            edges.push((pi, next.index(), false));
                        }
                    }
                    Some(mode) => {
                        let Some(prev) = res.versions.get(version.saturating_sub(1)) else {
                            continue;
                        };
                        if let Some(p) = prev.producer {
                            edges.push((p.index(), pi, mode == WriteMode::Load));
                        }
                        edges.extend(
                            prev.readers
                                .iter()
                                .filter(|r| r.index() != pi)
                                .map(|r| (r.index(), pi, false)),
                        );
                    }
                }
            }
        }
        edges
    }

    /// Passes kept: side-effect passes, output producers, and everything they need
    /// through data edges.
    fn cull(&self, edges: &[(usize, usize, bool)]) -> Vec<bool> {
        let mut keep = vec![false; self.passes.len()];
        let mut stack: Vec<usize> = self
            .passes
            .iter()
            .enumerate()
            .filter(|(_, p)| p.side_effect)
            .map(|(i, _)| i)
            .collect();
        stack.extend(self.outputs.iter().filter_map(|(id, v)| {
            self.resources
                .get(id.index())
                .and_then(|r| r.versions.get(*v as usize))
                .and_then(|vi| vi.producer)
                .map(PassId::index)
        }));
        while let Some(p) = stack.pop() {
            match keep.get_mut(p) {
                Some(k) if !*k => *k = true,
                _ => continue,
            }
            stack.extend(
                edges
                    .iter()
                    .filter(|(_, to, data)| *data && *to == p)
                    .map(|(from, _, _)| *from),
            );
        }
        keep
    }

    /// Kept passes in topological order, declaration order breaking ties.
    fn order(&self, edges: &[(usize, usize, bool)], keep: &[bool]) -> Result<Vec<PassId>, GraphError> {
        let n = self.passes.len();
        let kept = |p: usize| keep.get(p).copied().unwrap_or(false);
        let mut indegree = vec![0usize; n];
        let mut succ: Vec<Vec<usize>> = vec![Vec::new(); n];
        for &(from, to, _) in edges {
            if kept(from) && kept(to) && from != to {
                if let Some(s) = succ.get_mut(from) {
                    s.push(to);
                }
                if let Some(d) = indegree.get_mut(to) {
                    *d += 1;
                }
            }
        }
        let mut ready: BinaryHeap<Reverse<usize>> = (0..n)
            .filter(|&p| kept(p) && indegree.get(p) == Some(&0))
            .map(Reverse)
            .collect();
        let mut order: Vec<PassId> = Vec::with_capacity(n);
        while let Some(Reverse(p)) = ready.pop() {
            order.push(pass_id(p));
            for &s in succ.get(p).map_or(&[][..], Vec::as_slice) {
                if let Some(d) = indegree.get_mut(s) {
                    *d -= 1;
                    if *d == 0 {
                        ready.push(Reverse(s));
                    }
                }
            }
        }
        if order.len() == keep.iter().filter(|k| **k).count() {
            return Ok(order);
        }
        let passes = (0..n)
            .filter(|p| kept(*p) && !order.contains(&pass_id(*p)))
            .filter_map(|p| self.passes.get(p).map(|x| x.name.clone()))
            .collect();
        Err(GraphError::Cycle { passes })
    }

    /// Lifetimes (execution positions) and accumulated usage per resource.
    fn usage(&self, order: &[PassId]) -> Usage {
        let count = self.resources.len();
        let mut u = Usage {
            lifetimes: vec![None; count],
            textures: vec![wgpu::TextureUsages::empty(); count],
            buffers: vec![wgpu::BufferUsages::empty(); count],
        };
        for (at, p) in order.iter().enumerate() {
            let Some(pass) = self.passes.get(p.index()) else {
                continue;
            };
            for use_ in &pass.uses {
                let r = use_.resource.index();
                if let Some(l) = u.lifetimes.get_mut(r) {
                    *l = Some(l.map_or((at, at), |(a, b)| (a.min(at), b.max(at))));
                }
                match use_.access {
                    Access::Texture(a) => {
                        if let Some(x) = u.textures.get_mut(r) {
                            *x |= a.usage();
                        }
                    }
                    Access::Buffer(a) => {
                        if let Some(x) = u.buffers.get_mut(r) {
                            *x |= a.usage();
                        }
                    }
                }
            }
        }
        u
    }

    /// Greedy interval assignment of transient resources to physical ones, by first use.
    fn alias(&self, usage: &Usage) -> AliasPlan {
        let lifetime = |r: usize| usage.lifetimes.get(r).copied().flatten();
        let mut transient: Vec<usize> = (0..self.resources.len())
            .filter(|r| self.resources.get(*r).is_some_and(|x| !x.imported) && lifetime(*r).is_some())
            .collect();
        transient.sort_by_key(|r| (lifetime(*r).map_or(usize::MAX, |l| l.0), *r));

        let mut plan = AliasPlan {
            bindings: self
                .resources
                .iter()
                .map(|n| {
                    if n.imported {
                        ResourceBinding::Imported
                    } else {
                        ResourceBinding::Unused
                    }
                })
                .collect(),
            physical: Vec::new(),
            imported_usage: self.imported_usage(usage),
            unaliased_bytes: 0,
        };
        let mut free_after: Vec<usize> = Vec::new();
        for r in transient {
            let (Some(node), Some((first, last))) = (self.resources.get(r), lifetime(r)) else {
                continue;
            };
            let slot = match node.kind {
                ResourceKind::Texture(desc) => {
                    plan.unaliased_bytes = plan.unaliased_bytes.saturating_add(desc.estimated_bytes());
                    let want = usage
                        .textures
                        .get(r)
                        .copied()
                        .unwrap_or(wgpu::TextureUsages::empty());
                    let found = plan.physical.iter().zip(&free_after).position(|(p, fa)| {
                        matches!(p, PhysicalResource::Texture { desc: d, .. } if *d == desc) && *fa < first
                    });
                    if let Some(i) = found {
                        if let Some(PhysicalResource::Texture { usage: u, .. }) = plan.physical.get_mut(i) {
                            *u |= want;
                        }
                        i
                    } else {
                        plan.physical
                            .push(PhysicalResource::Texture { desc, usage: want });
                        free_after.push(0);
                        plan.physical.len() - 1
                    }
                }
                ResourceKind::Buffer(desc) => {
                    plan.unaliased_bytes = plan.unaliased_bytes.saturating_add(desc.size);
                    let want = usage
                        .buffers
                        .get(r)
                        .copied()
                        .unwrap_or(wgpu::BufferUsages::empty());
                    // Best fit: the smallest free buffer large enough, else the largest
                    // free one (grown), else a new one.
                    let free: Vec<(usize, u64)> = plan
                        .physical
                        .iter()
                        .zip(&free_after)
                        .enumerate()
                        .filter_map(|(i, (p, fa))| match p {
                            PhysicalResource::Buffer { size, .. } if *fa < first => Some((i, *size)),
                            _ => None,
                        })
                        .collect();
                    let pick = free
                        .iter()
                        .filter(|(_, s)| *s >= desc.size)
                        .min_by_key(|(i, s)| (*s, *i))
                        .or_else(|| free.iter().max_by_key(|(i, s)| (*s, Reverse(*i))))
                        .map(|(i, _)| *i);
                    if let Some(i) = pick {
                        if let Some(PhysicalResource::Buffer { size, usage: u }) = plan.physical.get_mut(i) {
                            *size = (*size).max(desc.size);
                            *u |= want;
                        }
                        i
                    } else {
                        plan.physical.push(PhysicalResource::Buffer {
                            size: desc.size,
                            usage: want,
                        });
                        free_after.push(0);
                        plan.physical.len() - 1
                    }
                }
            };
            if let Some(fa) = free_after.get_mut(slot) {
                *fa = last;
            }
            if let Some(b) = plan.bindings.get_mut(r) {
                *b = ResourceBinding::Physical(slot);
            }
        }
        plan
    }

    fn imported_usage(&self, usage: &Usage) -> Vec<(ResourceId, RequiredUsage)> {
        self.resources
            .iter()
            .enumerate()
            .filter(|(_, n)| n.imported)
            .map(|(r, n)| {
                let required = match n.kind {
                    ResourceKind::Texture(_) => RequiredUsage::Texture(
                        usage
                            .textures
                            .get(r)
                            .copied()
                            .unwrap_or(wgpu::TextureUsages::empty()),
                    ),
                    ResourceKind::Buffer(_) => RequiredUsage::Buffer(
                        usage
                            .buffers
                            .get(r)
                            .copied()
                            .unwrap_or(wgpu::BufferUsages::empty()),
                    ),
                };
                (resource_id(r), required)
            })
            .collect()
    }
}

/// Per-resource lifetimes and usage over the ordered passes.
struct Usage {
    lifetimes: Vec<Option<(usize, usize)>>,
    textures: Vec<wgpu::TextureUsages>,
    buffers: Vec<wgpu::BufferUsages>,
}

/// The physical resource plan.
struct AliasPlan {
    bindings: Vec<ResourceBinding>,
    physical: Vec<PhysicalResource>,
    imported_usage: Vec<(ResourceId, RequiredUsage)>,
    unaliased_bytes: u64,
}

fn pass_id(i: usize) -> PassId {
    PassId(u32::try_from(i).unwrap_or(u32::MAX))
}

fn resource_id(i: usize) -> ResourceId {
    ResourceId(u32::try_from(i).unwrap_or(u32::MAX))
}

impl PassBuilder<'_> {
    fn pass_name(&self) -> String {
        self.graph
            .passes
            .get(self.pass.index())
            .map(|p| p.name.clone())
            .unwrap_or_default()
    }

    fn fail(&mut self, e: GraphError) {
        self.graph.errors.push(e);
    }

    fn push_use(&mut self, u: Use) {
        if let Some(p) = self.graph.passes.get_mut(self.pass.index()) {
            p.uses.push(u);
        }
    }

    fn texture_desc(&self, id: ResourceId) -> Option<(TextureDesc, bool, u32, String)> {
        let node = self.graph.resources.get(id.index())?;
        match node.kind {
            ResourceKind::Texture(d) => Some((d, node.imported, node.latest(), node.name.clone())),
            ResourceKind::Buffer(_) => None,
        }
    }

    fn buffer_node(&self, id: ResourceId) -> Option<(bool, u32, String)> {
        let node = self.graph.resources.get(id.index())?;
        match node.kind {
            ResourceKind::Buffer(_) => Some((node.imported, node.latest(), node.name.clone())),
            ResourceKind::Texture(_) => None,
        }
    }

    fn record_read(&mut self, id: ResourceId, version: u32) {
        let pass = self.pass;
        if let Some(v) = self
            .graph
            .resources
            .get_mut(id.index())
            .and_then(|r| r.versions.get_mut(version as usize))
            && !v.readers.contains(&pass)
        {
            v.readers.push(pass);
        }
    }

    fn record_write(&mut self, id: ResourceId) -> u32 {
        let pass = self.pass;
        match self.graph.resources.get_mut(id.index()) {
            Some(r) => {
                r.versions.push(VersionInfo {
                    producer: Some(pass),
                    readers: Vec::new(),
                });
                r.latest()
            }
            None => 0,
        }
    }

    fn already_writes(&self, id: ResourceId) -> bool {
        self.graph
            .passes
            .get(self.pass.index())
            .is_some_and(|p| p.uses.iter().any(|u| u.resource == id && u.write.is_some()))
    }

    /// Declares a read of texture version `h`.
    pub fn read_texture(&mut self, h: TextureHandle, access: TextureAccess) -> &mut Self {
        let pass = self.pass_name();
        let Some((desc, imported, latest, name)) = self.texture_desc(h.id) else {
            self.fail(GraphError::UnknownResource { pass: Some(pass) });
            return self;
        };
        if h.version > latest {
            self.fail(GraphError::UnknownResource { pass: Some(pass) });
        } else if access.is_write() || !access.valid_for(desc.format) {
            self.fail(GraphError::InvalidAccess { pass, resource: name });
        } else if h.version == 0 && !imported {
            self.fail(GraphError::ReadBeforeWrite { pass, resource: name });
        } else {
            self.record_read(h.id, h.version);
            self.push_use(Use {
                resource: h.id,
                version: h.version,
                access: Access::Texture(access),
                write: None,
            });
        }
        self
    }

    /// Declares a write of texture version `h` (which must be the latest), returning the
    /// new version.
    pub fn write_texture(
        &mut self,
        h: TextureHandle,
        access: TextureAccess,
        mode: WriteMode,
    ) -> TextureHandle {
        let pass = self.pass_name();
        let Some((desc, imported, latest, name)) = self.texture_desc(h.id) else {
            self.fail(GraphError::UnknownResource { pass: Some(pass) });
            return h;
        };
        if h.version != latest {
            self.fail(GraphError::StaleWrite {
                pass,
                resource: name,
                version: h.version,
                latest,
            });
            return h;
        }
        if !access.is_write() || !access.valid_for(desc.format) {
            self.fail(GraphError::InvalidAccess { pass, resource: name });
            return h;
        }
        if mode == WriteMode::Load && h.version == 0 && !imported {
            self.fail(GraphError::ReadBeforeWrite { pass, resource: name });
            return h;
        }
        if self.already_writes(h.id) {
            self.fail(GraphError::DuplicateWrite { pass, resource: name });
            return h;
        }
        let version = self.record_write(h.id);
        self.push_use(Use {
            resource: h.id,
            version,
            access: Access::Texture(access),
            write: Some(mode),
        });
        TextureHandle { id: h.id, version }
    }

    /// Declares a read of buffer version `h`.
    pub fn read_buffer(&mut self, h: BufferHandle, access: BufferAccess) -> &mut Self {
        let pass = self.pass_name();
        let Some((imported, latest, name)) = self.buffer_node(h.id) else {
            self.fail(GraphError::UnknownResource { pass: Some(pass) });
            return self;
        };
        if h.version > latest {
            self.fail(GraphError::UnknownResource { pass: Some(pass) });
        } else if access.is_write() {
            self.fail(GraphError::InvalidAccess { pass, resource: name });
        } else if h.version == 0 && !imported {
            self.fail(GraphError::ReadBeforeWrite { pass, resource: name });
        } else {
            self.record_read(h.id, h.version);
            self.push_use(Use {
                resource: h.id,
                version: h.version,
                access: Access::Buffer(access),
                write: None,
            });
        }
        self
    }

    /// Declares a write of buffer version `h` (which must be the latest), returning the new
    /// version.
    pub fn write_buffer(&mut self, h: BufferHandle, access: BufferAccess, mode: WriteMode) -> BufferHandle {
        let pass = self.pass_name();
        let Some((imported, latest, name)) = self.buffer_node(h.id) else {
            self.fail(GraphError::UnknownResource { pass: Some(pass) });
            return h;
        };
        if h.version != latest {
            self.fail(GraphError::StaleWrite {
                pass,
                resource: name,
                version: h.version,
                latest,
            });
            return h;
        }
        if !access.is_write() {
            self.fail(GraphError::InvalidAccess { pass, resource: name });
            return h;
        }
        if mode == WriteMode::Load && h.version == 0 && !imported {
            self.fail(GraphError::ReadBeforeWrite { pass, resource: name });
            return h;
        }
        if self.already_writes(h.id) {
            self.fail(GraphError::DuplicateWrite { pass, resource: name });
            return h;
        }
        let version = self.record_write(h.id);
        self.push_use(Use {
            resource: h.id,
            version,
            access: Access::Buffer(access),
            write: Some(mode),
        });
        BufferHandle { id: h.id, version }
    }

    /// Marks the pass as having effects outside the graph (readback, debug output), so it
    /// is never culled.
    pub fn side_effect(&mut self) -> &mut Self {
        if let Some(p) = self.graph.passes.get_mut(self.pass.index()) {
            p.side_effect = true;
        }
        self
    }

    /// The pass being declared.
    pub fn id(&self) -> PassId {
        self.pass
    }
}

/// A physical resource backing one or more transient graph resources.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum PhysicalResource {
    /// A texture.
    Texture {
        /// Description shared by every resource aliased onto it.
        desc: TextureDesc,
        /// Union of all their usages.
        usage: wgpu::TextureUsages,
    },
    /// A buffer.
    Buffer {
        /// Largest size among the resources aliased onto it.
        size: u64,
        /// Union of all their usages.
        usage: wgpu::BufferUsages,
    },
}

impl PhysicalResource {
    fn estimated_bytes(&self) -> u64 {
        match self {
            PhysicalResource::Texture { desc, .. } => desc.estimated_bytes(),
            PhysicalResource::Buffer { size, .. } => *size,
        }
    }
}

/// How a graph resource is backed.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ResourceBinding {
    /// Not used by any kept pass.
    Unused,
    /// Supplied by the caller each frame.
    Imported,
    /// Backed by physical resource `n`.
    Physical(usize),
}

/// Usage an imported resource must have been created with.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum RequiredUsage {
    /// A texture.
    Texture(wgpu::TextureUsages),
    /// A buffer.
    Buffer(wgpu::BufferUsages),
}

/// A compiled graph: pass order, culling, lifetimes, and the physical resource plan.
#[derive(Clone, Debug)]
pub struct CompiledGraph {
    order: Vec<PassId>,
    culled: Vec<PassId>,
    pass_names: Vec<String>,
    pass_kinds: Vec<PassKind>,
    resource_names: Vec<String>,
    lifetimes: Vec<Option<(usize, usize)>>,
    bindings: Vec<ResourceBinding>,
    physical: Vec<PhysicalResource>,
    imported_usage: Vec<(ResourceId, RequiredUsage)>,
    unaliased_bytes: u64,
    aliased_bytes: u64,
}

impl CompiledGraph {
    /// Kept passes in execution order.
    pub fn order(&self) -> &[PassId] {
        &self.order
    }

    /// Culled passes, in declaration order.
    pub fn culled(&self) -> &[PassId] {
        &self.culled
    }

    /// Names of the kept passes in execution order.
    pub fn ordered_names(&self) -> Vec<&str> {
        self.order
            .iter()
            .filter_map(|p| self.pass_names.get(p.index()).map(String::as_str))
            .collect()
    }

    /// A pass's name.
    pub fn pass_name(&self, p: PassId) -> &str {
        self.pass_names.get(p.index()).map_or("", String::as_str)
    }

    /// A pass's kind.
    pub fn pass_kind(&self, p: PassId) -> Option<PassKind> {
        self.pass_kinds.get(p.index()).copied()
    }

    /// A resource's name.
    pub fn resource_name(&self, r: ResourceId) -> &str {
        self.resource_names.get(r.index()).map_or("", String::as_str)
    }

    /// First and last execution positions using a resource, if any kept pass uses it.
    pub fn lifetime(&self, r: ResourceId) -> Option<(usize, usize)> {
        self.lifetimes.get(r.index()).copied().flatten()
    }

    /// How a resource is backed.
    pub fn binding(&self, r: ResourceId) -> ResourceBinding {
        self.bindings
            .get(r.index())
            .copied()
            .unwrap_or(ResourceBinding::Unused)
    }

    /// The physical resources to create.
    pub fn physical(&self) -> &[PhysicalResource] {
        &self.physical
    }

    /// Usage each imported resource must support.
    pub fn imported_usage(&self) -> &[(ResourceId, RequiredUsage)] {
        &self.imported_usage
    }

    /// Estimated transient bytes without aliasing.
    pub fn unaliased_bytes(&self) -> u64 {
        self.unaliased_bytes
    }

    /// Estimated transient bytes with aliasing.
    pub fn aliased_bytes(&self) -> u64 {
        self.aliased_bytes
    }
}

#[cfg(test)]
mod tests;

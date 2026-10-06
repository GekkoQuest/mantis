//! Executing a compiled render graph on `wgpu`.
//!
//! [`GraphResources`] creates the physical textures and buffers of a compiled graph once;
//! aliased graph resources share them. [`RenderGraph::execute`] validates the imports
//! supplied for the frame, then runs every kept pass in order. `wgpu` inserts all
//! synchronization (decision 0012).
//!
//! The executor's own bookkeeping does not allocate per frame: imports are caller-provided
//! slices, passes are stored once, and the physical resource table is fixed at creation.

use super::{
    BufferHandle, CompiledGraph, PassId, PhysicalResource, RequiredUsage, ResourceBinding, ResourceId,
    TextureDesc, TextureHandle,
};

/// One pass's GPU work. Implementations are created once with the graph and reused every
/// frame; per-frame data arrives through `frame`.
pub trait GraphPass<F: ?Sized>: Send {
    /// Records the pass.
    fn execute(&mut self, ctx: &mut PassContext<'_>, frame: &F);
}

/// An imported texture for one frame.
#[derive(Clone, Copy, Debug)]
pub struct ImportedTexture<'a> {
    /// The graph resource it backs.
    pub resource: ResourceId,
    /// The texture.
    pub texture: &'a wgpu::Texture,
    /// A view of it.
    pub view: &'a wgpu::TextureView,
}

/// An imported buffer for one frame.
#[derive(Clone, Copy, Debug)]
pub struct ImportedBuffer<'a> {
    /// The graph resource it backs.
    pub resource: ResourceId,
    /// The buffer.
    pub buffer: &'a wgpu::Buffer,
}

/// Every import for one frame.
#[derive(Clone, Copy, Debug, Default)]
pub struct Imports<'a> {
    /// Imported textures.
    pub textures: &'a [ImportedTexture<'a>],
    /// Imported buffers.
    pub buffers: &'a [ImportedBuffer<'a>],
}

/// Physical resources of a compiled graph.
#[derive(Debug)]
pub struct GraphResources {
    textures: Vec<Option<(wgpu::Texture, wgpu::TextureView)>>,
    buffers: Vec<Option<wgpu::Buffer>>,
}

impl GraphResources {
    /// Creates every physical resource the graph needs.
    pub fn create(device: &wgpu::Device, compiled: &CompiledGraph) -> Self {
        let mut textures = Vec::with_capacity(compiled.physical().len());
        let mut buffers = Vec::with_capacity(compiled.physical().len());
        for (i, p) in compiled.physical().iter().enumerate() {
            match *p {
                PhysicalResource::Texture { desc, usage } => {
                    let label = format!("graph.texture.{i}");
                    let texture = device.create_texture(&texture_descriptor(&label, &desc, usage));
                    let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
                    textures.push(Some((texture, view)));
                    buffers.push(None);
                }
                PhysicalResource::Buffer { size, usage } => {
                    let label = format!("graph.buffer.{i}");
                    let buffer = device.create_buffer(&wgpu::BufferDescriptor {
                        label: Some(&label),
                        size: size.max(4).next_multiple_of(wgpu::COPY_BUFFER_ALIGNMENT),
                        usage,
                        mapped_at_creation: false,
                    });
                    textures.push(None);
                    buffers.push(Some(buffer));
                }
            }
        }
        Self { textures, buffers }
    }
}

/// The `wgpu` descriptor for a graph texture.
pub fn texture_descriptor<'a>(
    label: &'a str,
    desc: &TextureDesc,
    usage: wgpu::TextureUsages,
) -> wgpu::TextureDescriptor<'a> {
    wgpu::TextureDescriptor {
        label: Some(label),
        size: wgpu::Extent3d {
            width: desc.width,
            height: desc.height,
            depth_or_array_layers: desc.depth_or_array_layers,
        },
        mip_level_count: desc.mip_level_count,
        sample_count: desc.sample_count,
        dimension: desc.dimension,
        format: desc.format,
        usage,
        view_formats: &[],
    }
}

/// What a pass may touch while recording.
#[derive(Debug)]
pub struct PassContext<'a> {
    /// The frame's command encoder.
    pub encoder: &'a mut wgpu::CommandEncoder,
    /// The device.
    pub device: &'a wgpu::Device,
    /// The queue (for buffer writes ahead of the submit).
    pub queue: &'a wgpu::Queue,
    compiled: &'a CompiledGraph,
    resources: &'a GraphResources,
    imports: &'a Imports<'a>,
}

impl PassContext<'_> {
    fn physical_texture(&self, r: ResourceId) -> Option<(&wgpu::Texture, &wgpu::TextureView)> {
        match self.compiled.binding(r) {
            ResourceBinding::Physical(i) => self
                .resources
                .textures
                .get(i)
                .and_then(Option::as_ref)
                .map(|(t, v)| (t, v)),
            ResourceBinding::Imported => self
                .imports
                .textures
                .iter()
                .find(|t| t.resource == r)
                .map(|t| (t.texture, t.view)),
            ResourceBinding::Unused => None,
        }
    }

    /// The texture behind a handle (any version).
    pub fn texture(&self, h: TextureHandle) -> Option<&wgpu::Texture> {
        self.physical_texture(h.resource()).map(|(t, _)| t)
    }

    /// The full view of the texture behind a handle.
    pub fn texture_view(&self, h: TextureHandle) -> Option<&wgpu::TextureView> {
        self.physical_texture(h.resource()).map(|(_, v)| v)
    }

    /// The buffer behind a handle.
    pub fn buffer(&self, h: BufferHandle) -> Option<&wgpu::Buffer> {
        let r = h.resource();
        match self.compiled.binding(r) {
            ResourceBinding::Physical(i) => self.resources.buffers.get(i).and_then(Option::as_ref),
            ResourceBinding::Imported => self
                .imports
                .buffers
                .iter()
                .find(|b| b.resource == r)
                .map(|b| b.buffer),
            ResourceBinding::Unused => None,
        }
    }
}

/// Execution errors. Every one stops the frame before any pass records.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum ExecError {
    /// A kept pass has no implementation.
    MissingPass(String),
    /// An import the graph uses was not supplied.
    MissingImport(String),
    /// A supplied import lacks a usage the graph needs.
    ImportUsage(String),
}

impl core::fmt::Display for ExecError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            ExecError::MissingPass(p) => write!(f, "pass {p} has no implementation"),
            ExecError::MissingImport(r) => write!(f, "import {r} not supplied"),
            ExecError::ImportUsage(r) => write!(f, "import {r} lacks a required usage"),
        }
    }
}

impl std::error::Error for ExecError {}

/// A compiled graph with its passes and physical resources, ready to run every frame.
pub struct RenderGraph<F: ?Sized> {
    compiled: CompiledGraph,
    passes: Vec<Option<Box<dyn GraphPass<F>>>>,
    resources: GraphResources,
}

impl<F: ?Sized> core::fmt::Debug for RenderGraph<F> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("RenderGraph")
            .field("order", &self.compiled.ordered_names())
            .finish_non_exhaustive()
    }
}

impl<F: ?Sized> RenderGraph<F> {
    /// Binds pass implementations to a compiled graph and creates its resources.
    ///
    /// # Errors
    /// [`ExecError::MissingPass`] if a kept pass has no implementation.
    pub fn new(
        device: &wgpu::Device,
        compiled: CompiledGraph,
        implementations: Vec<(PassId, Box<dyn GraphPass<F>>)>,
    ) -> Result<Self, ExecError> {
        let mut passes: Vec<Option<Box<dyn GraphPass<F>>>> = Vec::new();
        passes.resize_with(compiled.pass_names.len(), || None);
        for (id, imp) in implementations {
            if let Some(slot) = passes.get_mut(id.index()) {
                *slot = Some(imp);
            }
        }
        if let Some(missing) = compiled
            .order()
            .iter()
            .find(|p| passes.get(p.index()).is_none_or(Option::is_none))
        {
            return Err(ExecError::MissingPass(compiled.pass_name(*missing).to_owned()));
        }
        let resources = GraphResources::create(device, &compiled);
        Ok(Self {
            compiled,
            passes,
            resources,
        })
    }

    /// The compiled graph.
    pub fn compiled(&self) -> &CompiledGraph {
        &self.compiled
    }

    /// The physical texture and full view behind a transient handle (stable for the
    /// graph's lifetime, so bind groups may be built from it once). `None` for imported or
    /// unused resources.
    pub fn transient_texture(&self, h: TextureHandle) -> Option<(&wgpu::Texture, &wgpu::TextureView)> {
        match self.compiled.binding(h.resource()) {
            ResourceBinding::Physical(i) => self
                .resources
                .textures
                .get(i)
                .and_then(Option::as_ref)
                .map(|(t, v)| (t, v)),
            ResourceBinding::Imported | ResourceBinding::Unused => None,
        }
    }

    /// Validates `imports` and records every kept pass, in order, into `encoder`.
    ///
    /// # Errors
    /// [`ExecError::MissingImport`] or [`ExecError::ImportUsage`]; nothing is recorded.
    pub fn execute(
        &mut self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        encoder: &mut wgpu::CommandEncoder,
        imports: &Imports<'_>,
        frame: &F,
    ) -> Result<u32, ExecError> {
        for (r, required) in self.compiled.imported_usage() {
            if self.compiled.lifetime(*r).is_none() {
                continue; // not used by any kept pass
            }
            let ok = match required {
                RequiredUsage::Texture(u) => {
                    let Some(t) = imports.textures.iter().find(|t| t.resource == *r) else {
                        return Err(ExecError::MissingImport(
                            self.compiled.resource_name(*r).to_owned(),
                        ));
                    };
                    t.texture.usage().contains(*u)
                }
                RequiredUsage::Buffer(u) => {
                    let Some(b) = imports.buffers.iter().find(|b| b.resource == *r) else {
                        return Err(ExecError::MissingImport(
                            self.compiled.resource_name(*r).to_owned(),
                        ));
                    };
                    b.buffer.usage().contains(*u)
                }
            };
            if !ok {
                return Err(ExecError::ImportUsage(self.compiled.resource_name(*r).to_owned()));
            }
        }
        let mut ran = 0u32;
        for p in &self.compiled.order {
            let Some(Some(pass)) = self.passes.get_mut(p.index()) else {
                continue;
            };
            let mut ctx = PassContext {
                encoder,
                device,
                queue,
                compiled: &self.compiled,
                resources: &self.resources,
                imports,
            };
            pass.execute(&mut ctx, frame);
            ran = ran.saturating_add(1);
        }
        Ok(ran)
    }
}

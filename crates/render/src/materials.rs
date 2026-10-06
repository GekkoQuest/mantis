//! Material pipelines: every canonical permutation of a material compiled (through
//! `mantis_shadergen`) into a render pipeline per pass, plus the material's parameter
//! buffer and bind group.
//!
//! In production the cook ships precompiled permutations; compiling here at load is the
//! development path (hot reload) and produces the same text.

use mantis_formats::material::{Deform, MaterialAsset, Pass, PermutationKey};

use crate::gpu_types::{GpuMaterialParams, skin_layout, vertex_layout};
use crate::layouts::Layouts;

/// A loaded material.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, PartialOrd, Ord)]
pub struct MaterialHandle(u32);

impl MaterialHandle {
    /// Dense index (also the scene's material id).
    pub fn index(self) -> u32 {
        self.0
    }

    /// The handle with dense index `i`.
    pub(crate) fn from_index(i: u32) -> Self {
        Self(i)
    }
}

/// Render target formats the pipelines are built for.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct TargetFormats {
    /// Lit color.
    pub hdr: wgpu::TextureFormat,
    /// Camera depth (reverse Z).
    pub depth: wgpu::TextureFormat,
    /// Shadow-map depth (conventional Z).
    pub shadow: wgpu::TextureFormat,
    /// The velocity target the depth prepass writes.
    pub velocity: wgpu::TextureFormat,
}

/// Errors loading a material.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum MaterialLoadError {
    /// Shader generation failed.
    Shader(String),
    /// The device rejected a shader or pipeline (validation message).
    Device(String),
    /// The bindless texture table is full.
    TableFull,
    /// No such material, or no parameter of that name and kind.
    UnknownParameter,
    /// No such material.
    UnknownMaterial,
    /// A replacement drops a deformation the material it replaces supports (instances
    /// using it would have no pipeline).
    Incompatible,
}

impl core::fmt::Display for MaterialLoadError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            MaterialLoadError::Shader(e) => write!(f, "shader generation: {e}"),
            MaterialLoadError::Device(e) => write!(f, "device: {e}"),
            MaterialLoadError::TableFull => f.write_str("bindless texture table full"),
            MaterialLoadError::UnknownParameter => f.write_str("no such material parameter"),
            MaterialLoadError::UnknownMaterial => f.write_str("no such material"),
            MaterialLoadError::Incompatible => {
                f.write_str("the replacement drops a deformation the material supports")
            }
        }
    }
}

impl std::error::Error for MaterialLoadError {}

#[derive(Debug)]
struct Entry {
    asset: MaterialAsset,
    /// Bindless table slots this material registered.
    table_slots: Vec<u32>,
    params: GpuMaterialParams,
    params_buffer: wgpu::Buffer,
    bind_group: wgpu::BindGroup,
    pipelines: Vec<(PermutationKey, wgpu::RenderPipeline)>,
}

/// Loaded materials, and in bindless mode the global texture table.
#[derive(Debug)]
pub struct MaterialCache {
    entries: Vec<Option<Entry>>,
    free_entries: Vec<u32>,
    /// Bindless table slots released by removed materials.
    free_table: Vec<u32>,
    sampler: wgpu::Sampler,
    fallback: wgpu::TextureView,
    bindless: bool,
    /// Bindless: every registered texture; entry 0 is the white fallback.
    table: Vec<wgpu::TextureView>,
    table_bind: Option<wgpu::BindGroup>,
    formats: TargetFormats,
}

impl MaterialCache {
    /// An empty cache. `bindless` selects the bindless permutations and layouts;
    /// `formats` are the targets every pipeline renders to.
    pub fn new(device: &wgpu::Device, queue: &wgpu::Queue, bindless: bool, formats: TargetFormats) -> Self {
        let sampler = device.create_sampler(&wgpu::SamplerDescriptor {
            label: Some("material.sampler"),
            address_mode_u: wgpu::AddressMode::Repeat,
            address_mode_v: wgpu::AddressMode::Repeat,
            mag_filter: wgpu::FilterMode::Linear,
            min_filter: wgpu::FilterMode::Linear,
            mipmap_filter: wgpu::MipmapFilterMode::Linear,
            ..Default::default()
        });
        let fallback = crate::textures::solid_2d(device, queue, "material.fallback", [255, 255, 255, 255]);
        let table = vec![fallback.clone()];
        Self {
            entries: Vec::new(),
            free_entries: Vec::new(),
            free_table: Vec::new(),
            sampler,
            fallback,
            bindless,
            table,
            table_bind: None,
            formats,
        }
    }

    /// Whether bindless permutations are in use.
    pub fn bindless(&self) -> bool {
        self.bindless
    }

    /// Loads a material: compiles its permutations for the active texture mode and builds
    /// every pipeline. Parameters start at the asset's defaults
    /// ([`MaterialAsset::bindings`]); `textures` fill the four slots (a white texture
    /// where `None`).
    ///
    /// # Errors
    /// [`MaterialLoadError`] if generation or any pipeline fails; nothing is added.
    pub fn add(
        &mut self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        layouts: &Layouts,
        asset: MaterialAsset,
        textures: [Option<&wgpu::TextureView>; 4],
    ) -> Result<MaterialHandle, MaterialLoadError> {
        let entry = self.build_entry(device, queue, layouts, asset, textures)?;
        let handle = if let Some(index) = self.free_entries.pop()
            && let Some(slot) = self.entries.get_mut(index as usize)
        {
            *slot = Some(entry);
            MaterialHandle(index)
        } else {
            self.entries.push(Some(entry));
            MaterialHandle(u32::try_from(self.entries.len() - 1).unwrap_or(u32::MAX))
        };
        Ok(handle)
    }

    /// Replaces a loaded material in place (hot reload): the new asset's permutations
    /// compile first, and only when every pipeline builds does the handle switch to
    /// them, so instances drawn with the handle keep drawing, now with the new material,
    /// and a broken edit leaves the old one live. Parameters start at the new asset's
    /// defaults. The replacement must keep every deformation the old material supports.
    ///
    /// # Errors
    /// [`MaterialLoadError::UnknownMaterial`], [`MaterialLoadError::Incompatible`], or
    /// any load error of the new asset (the old material stays).
    pub fn replace(
        &mut self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        layouts: &Layouts,
        material: MaterialHandle,
        asset: MaterialAsset,
        textures: [Option<&wgpu::TextureView>; 4],
    ) -> Result<(), MaterialLoadError> {
        let old = self
            .entries
            .get(material.0 as usize)
            .and_then(Option::as_ref)
            .ok_or(MaterialLoadError::UnknownMaterial)?;
        let (was, now) = (old.asset.deformations, asset.deformations);
        if (was.skinned && !now.skinned) || (was.vat && !now.vat) {
            return Err(MaterialLoadError::Incompatible);
        }
        let entry = self.build_entry(device, queue, layouts, asset, textures)?;
        let old = self
            .entries
            .get_mut(material.0 as usize)
            .and_then(|slot| slot.replace(entry))
            .ok_or(MaterialLoadError::UnknownMaterial)?;
        if !old.table_slots.is_empty() {
            for slot in &old.table_slots {
                if let Some(view) = self.table.get_mut(*slot as usize) {
                    *view = self.fallback.clone();
                }
            }
            self.free_table.extend(old.table_slots);
            self.rebuild_table(device, layouts)?;
        }
        Ok(())
    }

    fn build_entry(
        &mut self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        layouts: &Layouts,
        asset: MaterialAsset,
        textures: [Option<&wgpu::TextureView>; 4],
    ) -> Result<Entry, MaterialLoadError> {
        let mut params = GpuMaterialParams::from_defaults(&asset.bindings);
        let mut table_slots = Vec::new();
        if self.bindless {
            for (slot, t) in textures.iter().enumerate() {
                let index = match t {
                    Some(view) => {
                        let i = self.register(view)?;
                        table_slots.push(i);
                        i
                    }
                    None => 0,
                };
                if let Some(i) = params.texture_index.get_mut(slot) {
                    *i = index;
                }
            }
            self.rebuild_table(device, layouts)?;
        }
        let params_buffer = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("material.params"),
            size: core::mem::size_of::<GpuMaterialParams>() as u64,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        queue.write_buffer(&params_buffer, 0, bytemuck::bytes_of(&params));
        let views: Vec<&wgpu::TextureView> = textures.iter().map(|t| t.unwrap_or(&self.fallback)).collect();
        let bind_group = self.bind_group(device, layouts, &params_buffer, &views);
        let mut pipelines = Vec::new();
        let scope = device.push_error_scope(wgpu::ErrorFilter::Validation);
        for key in asset
            .canonical_permutations()
            .into_iter()
            .filter(|k| k.bindless() == self.bindless)
        {
            let perm = mantis_shadergen::compile(&asset, key)
                .map_err(|e| MaterialLoadError::Shader(e.to_string()))?;
            let module = device.create_shader_module(wgpu::ShaderModuleDescriptor {
                label: Some("material"),
                source: wgpu::ShaderSource::Wgsl(perm.source.clone().into()),
            });
            pipelines.push((
                key,
                build_pipeline(device, layouts, self.formats, &module, key, perm.fragment_entry),
            ));
        }
        if let Some(e) = pollster::block_on(scope.pop()) {
            return Err(MaterialLoadError::Device(e.to_string()));
        }
        Ok(Entry {
            asset,
            table_slots,
            params,
            params_buffer,
            bind_group,
            pipelines,
        })
    }

    /// Adds a texture to the bindless table, returning its index (existing views are not
    /// deduplicated; callers register each texture once).
    fn register(&mut self, view: &wgpu::TextureView) -> Result<u32, MaterialLoadError> {
        if let Some(index) = self.free_table.pop()
            && let Some(slot) = self.table.get_mut(index as usize)
        {
            *slot = view.clone();
            return Ok(index);
        }
        let index = u32::try_from(self.table.len()).map_err(|_| MaterialLoadError::TableFull)?;
        if index >= mantis_shadergen::layout::BINDLESS_TEXTURES {
            return Err(MaterialLoadError::TableFull);
        }
        self.table.push(view.clone());
        Ok(index)
    }

    fn rebuild_table(&mut self, device: &wgpu::Device, layouts: &Layouts) -> Result<(), MaterialLoadError> {
        let layout = layouts
            .texture_table
            .as_ref()
            .ok_or(MaterialLoadError::Device("bindless unsupported".to_owned()))?;
        let mut views: Vec<&wgpu::TextureView> = self.table.iter().collect();
        views.resize(
            mantis_shadergen::layout::BINDLESS_TEXTURES as usize,
            &self.fallback,
        );
        self.table_bind = Some(device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("texture_table"),
            layout,
            entries: &[wgpu::BindGroupEntry {
                binding: 0,
                resource: wgpu::BindingResource::TextureViewArray(&views),
            }],
        }));
        Ok(())
    }

    fn bind_group(
        &self,
        device: &wgpu::Device,
        layouts: &Layouts,
        params: &wgpu::Buffer,
        views: &[&wgpu::TextureView],
    ) -> wgpu::BindGroup {
        if self.bindless {
            return device.create_bind_group(&wgpu::BindGroupDescriptor {
                label: Some("material.params"),
                layout: &layouts.material_params,
                entries: &[
                    wgpu::BindGroupEntry {
                        binding: 0,
                        resource: params.as_entire_binding(),
                    },
                    wgpu::BindGroupEntry {
                        binding: 1,
                        resource: wgpu::BindingResource::Sampler(&self.sampler),
                    },
                ],
            });
        }
        let first = mantis_shadergen::layout::MATERIAL_TEXTURE_BINDING;
        let view =
            |i: usize| wgpu::BindingResource::TextureView(views.get(i).copied().unwrap_or(&self.fallback));
        device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("material.fixed"),
            layout: &layouts.material_fixed,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: params.as_entire_binding(),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: wgpu::BindingResource::Sampler(&self.sampler),
                },
                wgpu::BindGroupEntry {
                    binding: first,
                    resource: view(0),
                },
                wgpu::BindGroupEntry {
                    binding: first + 1,
                    resource: view(1),
                },
                wgpu::BindGroupEntry {
                    binding: first + 2,
                    resource: view(2),
                },
                wgpu::BindGroupEntry {
                    binding: first + 3,
                    resource: view(3),
                },
            ],
        })
    }

    /// Unloads a material: its pipelines, bind group, and parameters are dropped, its
    /// bindless table slots return to the free list (pointing at the white fallback until
    /// reused), and the handle may be reused by a later [`MaterialCache::add`]. The scene
    /// must hold no instance of it (see `Scene::release_material`).
    ///
    /// # Errors
    /// [`MaterialLoadError::UnknownMaterial`], or a bindless table rebuild failure.
    pub fn remove(
        &mut self,
        device: &wgpu::Device,
        layouts: &Layouts,
        material: MaterialHandle,
    ) -> Result<(), MaterialLoadError> {
        let entry = self
            .entries
            .get_mut(material.0 as usize)
            .and_then(Option::take)
            .ok_or(MaterialLoadError::UnknownMaterial)?;
        self.free_entries.push(material.0);
        if !entry.table_slots.is_empty() {
            for slot in &entry.table_slots {
                if let Some(view) = self.table.get_mut(*slot as usize) {
                    *view = self.fallback.clone();
                }
            }
            self.free_table.extend(entry.table_slots);
            self.rebuild_table(device, layouts)?;
        }
        Ok(())
    }

    /// The global texture table's bind group (bindless mode, once a material is loaded).
    pub fn texture_table(&self) -> Option<&wgpu::BindGroup> {
        self.table_bind.as_ref()
    }

    /// The pipeline for a material's permutation.
    pub fn pipeline(&self, material: MaterialHandle, key: PermutationKey) -> Option<&wgpu::RenderPipeline> {
        self.entries
            .get(material.0 as usize)?
            .as_ref()?
            .pipelines
            .iter()
            .find(|(k, _)| *k == key)
            .map(|(_, p)| p)
    }

    /// The pipeline a renderer asks for (see `MaterialAsset::request`).
    pub fn request(
        &self,
        material: MaterialHandle,
        pass: Pass,
        lightmapped: bool,
    ) -> Option<&wgpu::RenderPipeline> {
        let key = self.asset(material)?.request(pass, lightmapped, self.bindless)?;
        self.pipeline(material, key)
    }

    /// The pipeline for deformed geometry (`Deform::Static` is [`MaterialCache::request`]
    /// without lightmapping); `None` when the material does not support `deform`.
    pub fn request_deformed(
        &self,
        material: MaterialHandle,
        pass: Pass,
        deform: Deform,
    ) -> Option<&wgpu::RenderPipeline> {
        let key = self
            .asset(material)?
            .request_deformed(pass, self.bindless, deform)?;
        self.pipeline(material, key)
    }

    /// Sets a parameter by name: a scalar from `value[0]`, or a color from all four. Only
    /// the material's uniform is written; no pipeline or bind group is rebuilt.
    ///
    /// # Errors
    /// [`MaterialLoadError::UnknownParameter`] for an unknown material or name.
    pub fn set_param(
        &mut self,
        queue: &wgpu::Queue,
        material: MaterialHandle,
        name: &str,
        value: [f32; 4],
    ) -> Result<(), MaterialLoadError> {
        let entry = self
            .entries
            .get_mut(material.0 as usize)
            .and_then(Option::as_mut)
            .ok_or(MaterialLoadError::UnknownParameter)?;
        if let Some(i) = entry.asset.scalar_index(name) {
            let i = usize::from(i);
            let [v, ..] = value;
            if let Some(slot) = entry
                .params
                .scalars
                .get_mut(i / 4)
                .and_then(|row| row.get_mut(i % 4))
            {
                *slot = v;
            }
        } else if let Some(i) = entry.asset.color_index(name) {
            if let Some(slot) = entry.params.colors.get_mut(usize::from(i)) {
                *slot = value;
            }
        } else {
            return Err(MaterialLoadError::UnknownParameter);
        }
        queue.write_buffer(&entry.params_buffer, 0, bytemuck::bytes_of(&entry.params));
        Ok(())
    }

    /// The material's bind group.
    pub fn bind_group_of(&self, material: MaterialHandle) -> Option<&wgpu::BindGroup> {
        self.entries
            .get(material.0 as usize)?
            .as_ref()
            .map(|e| &e.bind_group)
    }

    /// The material's asset.
    pub fn asset(&self, material: MaterialHandle) -> Option<&MaterialAsset> {
        self.entries.get(material.0 as usize)?.as_ref().map(|e| &e.asset)
    }

    /// Loaded materials.
    pub fn len(&self) -> usize {
        self.entries.iter().flatten().count()
    }

    /// True when none are loaded.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

fn build_pipeline(
    device: &wgpu::Device,
    layouts: &Layouts,
    formats: TargetFormats,
    module: &wgpu::ShaderModule,
    key: PermutationKey,
    fragment_entry: Option<&'static str>,
) -> wgpu::RenderPipeline {
    let pass = key.pass();
    let layout_index = usize::from(key.bindless());
    let layout = match pass {
        Pass::Forward => layouts.forward.get(layout_index),
        Pass::Depth | Pass::Shadow | Pass::Outline => layouts.basic.get(layout_index),
    }
    .and_then(Option::as_ref);
    let color_target = [Some(wgpu::ColorTargetState {
        format: if pass == Pass::Depth {
            formats.velocity
        } else {
            formats.hdr
        },
        blend: None,
        write_mask: wgpu::ColorWrites::ALL,
    })];
    let (depth_format, compare, write, bias, cull) = match pass {
        // Reverse Z: nearer is greater. The forward pass tests against the prepass depth.
        Pass::Forward => (
            formats.depth,
            wgpu::CompareFunction::GreaterEqual,
            false,
            wgpu::DepthBiasState::default(),
            Some(wgpu::Face::Back),
        ),
        Pass::Depth => (
            formats.depth,
            wgpu::CompareFunction::GreaterEqual,
            true,
            wgpu::DepthBiasState::default(),
            Some(wgpu::Face::Back),
        ),
        Pass::Outline => (
            formats.depth,
            wgpu::CompareFunction::GreaterEqual,
            true,
            wgpu::DepthBiasState::default(),
            Some(wgpu::Face::Front),
        ),
        // Shadow maps use conventional depth with slope-scaled bias.
        Pass::Shadow => (
            formats.shadow,
            wgpu::CompareFunction::LessEqual,
            true,
            wgpu::DepthBiasState {
                constant: 2,
                slope_scale: 2.0,
                clamp: 0.0,
            },
            Some(wgpu::Face::Back),
        ),
    };
    let has_color = matches!(pass, Pass::Forward | Pass::Outline | Pass::Depth);
    let skinned = [Some(vertex_layout()), Some(skin_layout())];
    let rigid = [Some(vertex_layout())];
    let vertex_buffers: &[Option<wgpu::VertexBufferLayout<'_>>] = if key.deform() == Deform::Skinned {
        &skinned
    } else {
        &rigid
    };
    device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
        label: Some("material"),
        layout,
        vertex: wgpu::VertexState {
            module,
            entry_point: Some("vs_main"),
            compilation_options: wgpu::PipelineCompilationOptions::default(),
            buffers: vertex_buffers,
        },
        primitive: wgpu::PrimitiveState {
            cull_mode: cull,
            front_face: crate::gpu_types::FRONT_FACE,
            ..Default::default()
        },
        depth_stencil: Some(wgpu::DepthStencilState {
            format: depth_format,
            depth_write_enabled: Some(write),
            depth_compare: Some(compare),
            stencil: wgpu::StencilState::default(),
            bias,
        }),
        multisample: wgpu::MultisampleState::default(),
        fragment: fragment_entry.map(|entry| wgpu::FragmentState {
            module,
            entry_point: Some(entry),
            compilation_options: wgpu::PipelineCompilationOptions::default(),
            targets: if has_color { &color_target } else { &[] },
        }),
        multiview_mask: None,
        cache: None,
    })
}

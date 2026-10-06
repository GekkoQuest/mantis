//! The UI pass (plan 8.6): one instanced draw of every `mantis_ui` quad (rounded
//! rectangles and MSDF glyphs) over the composited frame, premultiplied-alpha blended in
//! painter's order. The glyph atlas is mirrored into a texture: dirty rectangles upload
//! as they appear, and the texture is recreated when the atlas grows.

use mantis_ui::atlas::GlyphAtlas;
use mantis_ui::draw::UiQuad;

/// Quad instance attributes (the `QuadIn` of `shaders/ui.wgsl`).
const QUAD_ATTRIBUTES: [wgpu::VertexAttribute; 6] = wgpu::vertex_attr_array![
    0 => Float32x4,
    1 => Float32x4,
    2 => Float32x4,
    3 => Float32x4,
    4 => Float32x4,
    5 => Float32x4,
];

/// Counters.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct UiStats {
    /// Quads drawn last frame.
    pub quads: u32,
    /// Quads dropped for lack of instance capacity (last frame).
    pub dropped: u32,
    /// Atlas texels uploaded last frame.
    pub atlas_texels: u64,
    /// Times the atlas texture was recreated.
    pub atlas_rebuilds: u64,
}

#[derive(Debug)]
struct AtlasTexture {
    texture: wgpu::Texture,
    generation: u64,
    size: [u32; 2],
    bind: wgpu::BindGroup,
}

/// GPU state of the UI pass.
#[derive(Debug)]
pub struct UiRenderer {
    pipeline: wgpu::RenderPipeline,
    layout: wgpu::BindGroupLayout,
    view: wgpu::Buffer,
    sampler: wgpu::Sampler,
    instances: wgpu::Buffer,
    capacity: u32,
    count: u32,
    atlas: Option<AtlasTexture>,
    stats: UiStats,
}

impl UiRenderer {
    /// A pass drawing up to `capacity` quads per frame into `format` targets.
    #[expect(clippy::too_many_lines)] // One-time pipeline construction, declarative descriptors.
    pub fn new(device: &wgpu::Device, format: wgpu::TextureFormat, capacity: u32) -> Self {
        let layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("ui"),
            entries: &[
                crate::layouts::entry(
                    0,
                    wgpu::ShaderStages::VERTEX_FRAGMENT,
                    wgpu::BindingType::Buffer {
                        ty: wgpu::BufferBindingType::Uniform,
                        has_dynamic_offset: false,
                        min_binding_size: None,
                    },
                ),
                crate::layouts::entry(
                    1,
                    wgpu::ShaderStages::FRAGMENT,
                    wgpu::BindingType::Texture {
                        sample_type: wgpu::TextureSampleType::Float { filterable: true },
                        view_dimension: wgpu::TextureViewDimension::D2,
                        multisampled: false,
                    },
                ),
                crate::layouts::entry(
                    2,
                    wgpu::ShaderStages::FRAGMENT,
                    wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
                ),
            ],
        });
        let module = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("ui"),
            source: wgpu::ShaderSource::Wgsl(include_str!("shaders/ui.wgsl").into()),
        });
        let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("ui"),
            bind_group_layouts: &[Some(&layout)],
            immediate_size: 0,
        });
        let premultiplied = wgpu::BlendState {
            color: wgpu::BlendComponent {
                src_factor: wgpu::BlendFactor::One,
                dst_factor: wgpu::BlendFactor::OneMinusSrcAlpha,
                operation: wgpu::BlendOperation::Add,
            },
            alpha: wgpu::BlendComponent {
                src_factor: wgpu::BlendFactor::One,
                dst_factor: wgpu::BlendFactor::OneMinusSrcAlpha,
                operation: wgpu::BlendOperation::Add,
            },
        };
        let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("ui"),
            layout: Some(&pipeline_layout),
            vertex: wgpu::VertexState {
                module: &module,
                entry_point: Some("vs_ui"),
                compilation_options: wgpu::PipelineCompilationOptions::default(),
                buffers: &[Some(wgpu::VertexBufferLayout {
                    array_stride: core::mem::size_of::<UiQuad>() as u64,
                    step_mode: wgpu::VertexStepMode::Instance,
                    attributes: &QUAD_ATTRIBUTES,
                })],
            },
            primitive: wgpu::PrimitiveState {
                front_face: crate::gpu_types::FRONT_FACE,
                ..Default::default()
            },
            depth_stencil: None,
            multisample: wgpu::MultisampleState::default(),
            fragment: Some(wgpu::FragmentState {
                module: &module,
                entry_point: Some("fs_ui"),
                compilation_options: wgpu::PipelineCompilationOptions::default(),
                targets: &[Some(wgpu::ColorTargetState {
                    format,
                    blend: Some(premultiplied),
                    write_mask: wgpu::ColorWrites::ALL,
                })],
            }),
            multiview_mask: None,
            cache: None,
        });
        let capacity = capacity.max(1);
        Self {
            pipeline,
            layout,
            view: device.create_buffer(&wgpu::BufferDescriptor {
                label: Some("ui.view"),
                size: 16,
                usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
                mapped_at_creation: false,
            }),
            sampler: device.create_sampler(&wgpu::SamplerDescriptor {
                label: Some("ui.atlas"),
                mag_filter: wgpu::FilterMode::Linear,
                min_filter: wgpu::FilterMode::Linear,
                ..Default::default()
            }),
            instances: device.create_buffer(&wgpu::BufferDescriptor {
                label: Some("ui.quads"),
                size: u64::from(capacity) * core::mem::size_of::<UiQuad>() as u64,
                usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
                mapped_at_creation: false,
            }),
            capacity,
            count: 0,
            atlas: None,
            stats: UiStats::default(),
        }
    }

    /// Counters.
    pub fn stats(&self) -> UiStats {
        self.stats
    }

    fn rebuild_atlas(&mut self, device: &wgpu::Device, atlas: &GlyphAtlas) {
        let [width, height] = atlas.size();
        let texture = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("ui.atlas"),
            size: wgpu::Extent3d {
                width: width.max(1),
                height: height.max(1),
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::Rgba8Unorm,
            usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
            view_formats: &[],
        });
        let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
        let bind = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("ui"),
            layout: &self.layout,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: self.view.as_entire_binding(),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: wgpu::BindingResource::TextureView(&view),
                },
                wgpu::BindGroupEntry {
                    binding: 2,
                    resource: wgpu::BindingResource::Sampler(&self.sampler),
                },
            ],
        });
        self.atlas = Some(AtlasTexture {
            texture,
            generation: atlas.generation(),
            size: atlas.size(),
            bind,
        });
        self.stats.atlas_rebuilds += 1;
    }

    /// Uploads this frame's quads (beyond capacity they are dropped and counted) and the
    /// atlas changes, for a `target` of `[width, height]` pixels.
    #[expect(clippy::cast_precision_loss)] // Pixel sizes are far below 2^24.
    pub fn prepare(
        &mut self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        quads: &[UiQuad],
        atlas: &mut GlyphAtlas,
        target: [u32; 2],
    ) {
        let stale = self
            .atlas
            .as_ref()
            .is_none_or(|a| a.generation != atlas.generation() || a.size != atlas.size());
        let [aw, ah] = atlas.size();
        let dirty = if stale {
            self.rebuild_atlas(device, atlas);
            let _ = atlas.take_dirty();
            Some(mantis_ui::atlas::AtlasRect {
                x: 0,
                y: 0,
                w: aw,
                h: ah,
            })
        } else {
            atlas.take_dirty()
        };
        self.stats.atlas_texels = 0;
        if let (Some(r), Some(t)) = (dirty, &self.atlas) {
            let (w, h) = (r.w.min(aw.saturating_sub(r.x)), r.h.min(ah.saturating_sub(r.y)));
            if w > 0 && h > 0 {
                let offset = (u64::from(r.y) * u64::from(aw) + u64::from(r.x)) * 4;
                queue.write_texture(
                    wgpu::TexelCopyTextureInfo {
                        texture: &t.texture,
                        mip_level: 0,
                        origin: wgpu::Origin3d { x: r.x, y: r.y, z: 0 },
                        aspect: wgpu::TextureAspect::All,
                    },
                    atlas.pixels(),
                    wgpu::TexelCopyBufferLayout {
                        offset,
                        bytes_per_row: Some(aw * 4),
                        rows_per_image: Some(ah),
                    },
                    wgpu::Extent3d {
                        width: w,
                        height: h,
                        depth_or_array_layers: 1,
                    },
                );
                self.stats.atlas_texels = u64::from(w) * u64::from(h);
            }
        }
        let (tw, th) = (target[0].max(1) as f32, target[1].max(1) as f32);
        queue.write_buffer(&self.view, 0, bytemuck::cast_slice(&[tw, th, 1.0 / tw, 1.0 / th]));
        let shown = quads
            .get(..quads.len().min(self.capacity as usize))
            .unwrap_or(&[]);
        self.count = u32::try_from(shown.len()).unwrap_or(0);
        self.stats.quads = self.count;
        self.stats.dropped = u32::try_from(quads.len() - shown.len()).unwrap_or(u32::MAX);
        if !shown.is_empty() {
            queue.write_buffer(&self.instances, 0, bytemuck::cast_slice(shown));
        }
    }

    /// Draws the prepared quads into the caller's pass (whose color target is the output).
    pub fn record(&self, pass: &mut wgpu::RenderPass<'_>) {
        let Some(atlas) = &self.atlas else { return };
        if self.count == 0 {
            return;
        }
        pass.set_pipeline(&self.pipeline);
        pass.set_bind_group(0, &atlas.bind, &[]);
        pass.set_vertex_buffer(0, self.instances.slice(..));
        pass.draw(0..6, 0..self.count);
    }
}

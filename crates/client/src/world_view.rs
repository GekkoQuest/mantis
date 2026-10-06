//! Draws the render world with `mantis-render` on the render thread: a third-person
//! camera behind the local avatar, a proxy body for every entity in the frame's poses,
//! and the ground: a flat placeholder plane, replaced by the cooked world when one is
//! attached ([`WorldView::attach_world`]), streamed around the camera each frame.
//!
//! Positions are world space as `mantis_core` kinematics defines it (decision 0019: left
//! handed, +X right, +Y up, +Z forward, yaw 0 along +Z). The renderer's view transform is
//! the only conversion, so poses go to the renderer unchanged.

use std::sync::Arc;
use std::time::Duration;

use glam::{Mat4, Quat, Vec3};
use mantis_core::ecs::EntityId;
use mantis_render::materials::MaterialLoadError;
use mantis_render::math::Camera;
use mantis_render::mesh::{cube, plane};
use mantis_render::post::PostSettings;
use mantis_render::renderer::{FrameInputs, FrameStats, Renderer, RendererConfig, RendererError};
use mantis_render::scene::{InstanceHandle, MaterialId, MeshId};

use crate::render_world::PoseSource;
use crate::threads::render_thread::FrameContext;
use crate::time::HostClock;
use crate::world_stream::{Gpu, WorldError, WorldStats, WorldStreamer};

/// A cooked world streamed into the view.
pub struct StreamedWorld {
    /// The streamer.
    pub streamer: WorldStreamer,
    /// The clock the hand-off budget is measured on.
    pub clock: Arc<dyn HostClock>,
    /// Render-thread hand-off budget per frame.
    pub budget: Duration,
    /// The last frame's streaming outcome.
    pub last: WorldStats,
}

impl core::fmt::Debug for StreamedWorld {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("StreamedWorld")
            .field("streamer", &self.streamer)
            .field("budget", &self.budget)
            .field("last", &self.last)
            .finish_non_exhaustive()
    }
}

fn to_glam(p: crate::core_api::Vec3) -> Vec3 {
    Vec3::new(p.x, p.y, p.z)
}

/// The third-person camera: behind and above `avatar`, looking along the camera rig's
/// orientation (turns, as the rig and the simulation hold them).
pub fn third_person_camera(
    avatar: crate::core_api::Vec3,
    yaw_turns: f32,
    pitch_turns: f32,
    config: &WorldViewConfig,
) -> Camera {
    let mut camera = Camera {
        position: Vec3::ZERO,
        yaw: yaw_turns * core::f32::consts::TAU,
        pitch: pitch_turns * core::f32::consts::TAU,
        fov_y: config.fov_y,
        #[expect(clippy::cast_precision_loss)] // Pixel sizes are far below 2^24.
        aspect: config.renderer.width as f32 / config.renderer.height.max(1) as f32,
        near: 0.1,
    };
    let back = camera.forward() * config.camera_distance;
    camera.position = to_glam(avatar) + Vec3::Y * config.camera_height - back;
    camera
}

/// View settings.
#[derive(Clone, Copy, Debug)]
pub struct WorldViewConfig {
    /// The renderer's configuration (output size and format, capacities).
    pub renderer: RendererConfig,
    /// Camera distance behind the avatar, meters.
    pub camera_distance: f32,
    /// Camera height above the avatar's feet, meters.
    pub camera_height: f32,
    /// Vertical field of view, radians.
    pub fov_y: f32,
    /// Post-processing.
    pub post: PostSettings,
}

impl WorldViewConfig {
    /// Defaults for a `width x height` output of `format`.
    pub fn new(width: u32, height: u32, format: wgpu::TextureFormat) -> Self {
        Self {
            renderer: RendererConfig::new(width, height, format),
            camera_distance: 6.0,
            camera_height: 2.5,
            fov_y: 1.0,
            post: PostSettings::default(),
        }
    }
}

/// Avatar proxy dimensions (meters): width and height.
const BODY: [f32; 2] = [0.6, 1.8];

/// The world view.
#[derive(Debug)]
pub struct WorldView {
    renderer: Renderer,
    config: WorldViewConfig,
    body: MeshId,
    local_material: MaterialId,
    remote_material: MaterialId,
    /// Sorted by entity; the flag marks entities seen this frame.
    entities: Vec<(EntityId, InstanceHandle, bool)>,
    capacity: usize,
    time: f32,
    /// The placeholder ground, until a world is attached.
    ground: Option<InstanceHandle>,
    world: Option<StreamedWorld>,
}

fn flat_material(color: [f32; 3]) -> Result<mantis_formats::material::MaterialAsset, RendererError> {
    use mantis_formats::material::{
        ColorDefault, Deformations, LightingModel, MaterialAsset, MaterialBindings, MaterialGraph, Node,
        NodeId, SurfaceOutputs,
    };
    let graph = MaterialGraph {
        nodes: vec![Node::ColorParam(0), Node::Swizzle(NodeId(0), [0, 1, 2, 0], 3)],
        outputs: SurfaceOutputs {
            base_color: NodeId(1),
            alpha: None,
            emissive: None,
            alpha_cutoff: None,
        },
        lighting: LightingModel::Toon {
            bands: 3,
            softness: 0.1,
            shadow_tint: [0.5, 0.55, 0.7],
        },
        rim: None,
        outline: None,
    };
    let graph = graph
        .validate()
        .map_err(|e| RendererError::Material(MaterialLoadError::Shader(format!("{e:?}"))))?;
    Ok(MaterialAsset {
        graph,
        casts_shadows: true,
        deformations: Deformations::NONE,
        bindings: MaterialBindings {
            textures: Vec::new(),
            scalars: Vec::new(),
            colors: vec![ColorDefault {
                name: "color".to_owned(),
                value: [color[0], color[1], color[2], 1.0],
            }],
        },
    })
}

impl WorldView {
    /// Creates the renderer, the proxy meshes and materials, and the ground.
    ///
    /// # Errors
    /// [`RendererError`].
    pub fn new(
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        bindless: bool,
        config: WorldViewConfig,
    ) -> Result<Self, RendererError> {
        let mut renderer = Renderer::new(device, queue, bindless, config.renderer)?;
        let (mut body, indices) = cube();
        // An upright box with its feet at the origin (baked into the mesh: instance
        // transforms are similarities).
        for v in &mut body {
            let [x, y, z] = v.position;
            v.position = [x * BODY[0], (y + 0.5) * BODY[1], z * BODY[0]];
        }
        let body = renderer.add_mesh(queue, &body, &indices)?;
        let (gv, gi) = plane(400.0);
        let ground_mesh = renderer.add_mesh(queue, &gv, &gi)?;
        let local_asset = flat_material([0.95, 0.55, 0.2])?;
        let local_material = renderer.add_material(device, queue, local_asset, [None; 4])?;
        let remote_asset = flat_material([0.3, 0.55, 0.9])?;
        let remote_material = renderer.add_material(device, queue, remote_asset, [None; 4])?;
        let ground_asset = flat_material([0.35, 0.45, 0.3])?;
        let ground_material = renderer.add_material(device, queue, ground_asset, [None; 4])?;
        let ground = renderer
            .scene_mut()
            .spawn(ground_mesh, ground_material, Mat4::IDENTITY)
            .map_err(RendererError::Scene)?;
        let capacity = config.renderer.max_instances as usize;
        Ok(Self {
            renderer,
            config,
            body,
            local_material,
            remote_material,
            entities: Vec::with_capacity(capacity),
            capacity,
            time: 0.0,
            ground: Some(ground),
            world: None,
        })
    }

    /// Streams `world` from now on, replacing the placeholder ground. The world's
    /// materials are compiled first ([`WorldStreamer::preload_materials`]), so streaming
    /// hand-offs never compile pipelines.
    ///
    /// # Errors
    /// [`WorldError`] when a material fails to load; the world is attached regardless and
    /// sectors naming that material fail when they stream in.
    pub fn attach_world(&mut self, gpu: &Gpu<'_>, mut world: StreamedWorld) -> Result<usize, WorldError> {
        if let Some(g) = self.ground.take() {
            let _ = self.renderer.despawn(g);
        }
        let compiled = world.streamer.preload_materials(&mut self.renderer, gpu);
        self.world = Some(world);
        compiled
    }

    /// Detaches the streamed world (to move it to a rebuilt view; call
    /// [`WorldStreamer::restart`] before attaching it to another renderer).
    pub fn take_world(&mut self) -> Option<StreamedWorld> {
        self.world.take()
    }

    /// The last frame's streaming outcome, when a world is attached.
    pub fn world_stats(&self) -> Option<WorldStats> {
        self.world.as_ref().map(|w| w.last)
    }

    /// Streams the attached world around this frame's camera (call before
    /// [`WorldView::update`]): loads and unloads by distance, and hands finished sectors
    /// to the renderer within the budget.
    pub fn stream(&mut self, gpu: &Gpu<'_>, frame: &FrameContext<'_>) {
        let camera = self.camera(frame);
        let Some(world) = self.world.as_mut() else {
            return;
        };
        world.last = world.streamer.update(
            &mut self.renderer,
            gpu,
            world.clock.as_ref(),
            world.budget,
            (camera.position, Vec3::ZERO, camera.forward()),
        );
    }

    /// The renderer (effects, materials, UI).
    pub fn renderer_mut(&mut self) -> &mut Renderer {
        &mut self.renderer
    }

    /// The renderer (read).
    pub fn renderer(&self) -> &Renderer {
        &self.renderer
    }

    /// The attached world and the renderer together (an editor reloads the world into
    /// the live renderer).
    pub fn world_and_renderer_mut(&mut self) -> (Option<&mut StreamedWorld>, &mut Renderer) {
        (self.world.as_mut(), &mut self.renderer)
    }

    /// The attached world.
    pub fn world(&self) -> Option<&StreamedWorld> {
        self.world.as_ref()
    }

    /// Entities drawn.
    pub fn entity_count(&self) -> usize {
        self.entities.len()
    }

    /// The camera for a frame: behind and above the local avatar, looking along the
    /// camera rig's orientation.
    pub fn camera(&self, frame: &FrameContext<'_>) -> Camera {
        let local = frame
            .poses
            .as_slice()
            .iter()
            .find(|p| p.source == PoseSource::LocalPredicted)
            .map_or(crate::core_api::Vec3::ZERO, |p| p.position);
        third_person_camera(
            local,
            frame.camera.yaw_turns(),
            frame.camera.pitch_turns(),
            &self.config,
        )
    }

    /// Syncs proxies with the frame's poses and prepares the frame. Allocation-free once
    /// every entity has been seen (spawns and despawns reuse scene slots).
    pub fn update(&mut self, frame: &FrameContext<'_>) -> FrameStats {
        self.time += frame.frame_dt.max(0.0);
        for e in &mut self.entities {
            e.2 = false;
        }
        for pose in frame.poses.as_slice() {
            // Yaw turns +Z toward +X: a rotation about +Y (decision 0019).
            let model = Mat4::from_rotation_translation(
                Quat::from_rotation_y(pose.yaw_turns * core::f32::consts::TAU),
                to_glam(pose.position),
            );
            match self.entities.binary_search_by_key(&pose.id, |e| e.0) {
                Ok(i) => {
                    if let Some(e) = self.entities.get_mut(i) {
                        let _ = self.renderer.scene_mut().set_transform(e.1, model);
                        e.2 = true;
                    }
                }
                Err(i) => {
                    if self.entities.len() >= self.capacity {
                        continue;
                    }
                    let material = if pose.source == PoseSource::LocalPredicted {
                        self.local_material
                    } else {
                        self.remote_material
                    };
                    if let Ok(h) = self.renderer.scene_mut().spawn(self.body, material, model) {
                        self.entities.insert(i, (pose.id, h, true));
                    }
                }
            }
        }
        let renderer = &mut self.renderer;
        self.entities.retain(|e| {
            if !e.2 {
                let _ = renderer.despawn(e.1);
            }
            e.2
        });
        let camera = self.camera(frame);
        self.renderer.prepare(&FrameInputs {
            camera,
            time: self.time,
            sun_direction: Vec3::new(0.4, -1.0, -0.3),
            sun_color: Vec3::splat(2.5),
            shadows: true,
            sky: mantis_formats::sh::ShL1::ZERO,
            ambient_intensity: 1.0,
            exposure: 1.0,
            clear_color: [0.45, 0.6, 0.8, 1.0],
            ssao_strength: 0.5,
            post: self.config.post,
        })
    }

    /// Records the prepared frame into `encoder`, rendering into `output`.
    ///
    /// # Errors
    /// [`RendererError`].
    pub fn encode(
        &mut self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        encoder: &mut wgpu::CommandEncoder,
        output: (&wgpu::Texture, &wgpu::TextureView),
    ) -> Result<(), RendererError> {
        self.renderer.encode(device, queue, encoder, output)
    }
}

/// Render-thread time of the last frame's stages (all zero until a clock is set,
/// [`SurfaceSink::set_clock`]).
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct FrameTimings {
    /// Streaming hand-off.
    pub stream: Duration,
    /// Overlay update and frame preparation.
    pub update: Duration,
    /// UI layout and its draw list.
    pub ui: Duration,
    /// Recording the render graph and submitting.
    pub encode: Duration,
    /// The whole frame.
    pub total: Duration,
}

/// A tool drawn over the world view (the editor module): it sees platform events first,
/// edits the view before each frame is prepared, and draws its own UI after the module
/// UI (the renderer's UI pass shows the last list set).
pub trait SinkOverlay: Send {
    /// Sees every platform event before the module UI; true consumes it.
    fn ui_event(&mut self, event: &crate::threads::render_thread::PlatformEvent) -> bool;
    /// Runs after streaming and before the frame is prepared. `last` is the previous
    /// frame's stage timings.
    fn update(&mut self, view: &mut WorldView, gpu: &Gpu<'_>, frame: &FrameContext<'_>, last: &FrameTimings);
    /// Draws its UI for a `viewport` of physical pixels; true when it set the UI pass.
    fn draw(
        &mut self,
        renderer: &mut Renderer,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        viewport: [f32; 2],
    ) -> bool;
}

/// A frame sink presenting the world view (and an optional UI) to a window surface.
pub struct SurfaceSink {
    target: crate::platform::GpuTarget,
    view: WorldView,
    ui: Option<crate::ui_layer::UiLayer>,
    template: WorldViewConfig,
    overlay: Option<Box<dyn SinkOverlay>>,
    clock: Option<Arc<dyn HostClock>>,
    timings: FrameTimings,
    /// Frames that failed to record (device errors); the frame is skipped.
    pub failed_frames: u64,
}

impl core::fmt::Debug for SurfaceSink {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("SurfaceSink")
            .field("failed_frames", &self.failed_frames)
            .finish_non_exhaustive()
    }
}

impl SurfaceSink {
    /// A sink drawing into `target`'s surface with `template` settings (its size and format
    /// are taken from the surface).
    ///
    /// # Errors
    /// [`RendererError`].
    pub fn new(
        target: crate::platform::GpuTarget,
        mut template: WorldViewConfig,
        ui: Option<crate::ui_layer::UiLayer>,
    ) -> Result<Self, RendererError> {
        template.renderer.width = target.config.width.max(1);
        template.renderer.height = target.config.height.max(1);
        template.renderer.output_format = target.config.format;
        let view = WorldView::new(&target.device, &target.queue, false, template)?;
        Ok(Self {
            target,
            view,
            ui,
            template,
            overlay: None,
            clock: None,
            timings: FrameTimings::default(),
            failed_frames: 0,
        })
    }

    /// Attaches or removes the UI.
    pub fn set_ui(&mut self, ui: Option<crate::ui_layer::UiLayer>) {
        self.ui = ui;
    }

    /// Attaches or removes an overlay tool (the editor module).
    pub fn set_overlay(&mut self, overlay: Option<Box<dyn SinkOverlay>>) {
        self.overlay = overlay;
    }

    /// Measures frame stages on `clock` from now on ([`SurfaceSink::timings`]).
    pub fn set_clock(&mut self, clock: Arc<dyn HostClock>) {
        self.clock = Some(clock);
    }

    /// The last frame's stage timings.
    pub fn timings(&self) -> FrameTimings {
        self.timings
    }

    /// Streams a cooked world into the view (compiling its materials now).
    ///
    /// # Errors
    /// As [`WorldView::attach_world`].
    pub fn attach_world(&mut self, world: StreamedWorld) -> Result<usize, WorldError> {
        let gpu = Gpu {
            device: &self.target.device,
            queue: &self.target.queue,
            capabilities: mantis_render::gpu::Capabilities::of(self.target.device.features()),
        };
        self.view.attach_world(&gpu, world)
    }
}

impl crate::threads::render_thread::FrameSink for SurfaceSink {
    fn resize(&mut self, width: u32, height: u32) {
        if width == 0 || height == 0 {
            return;
        }
        self.target.resize(width, height);
        let mut config = self.template;
        config.renderer.width = width;
        config.renderer.height = height;
        // Graph resources are sized to the output: rebuild the view at the new size.
        match WorldView::new(&self.target.device, &self.target.queue, false, config) {
            Ok(mut view) => {
                // The streamed world moves to the new renderer and streams in again.
                if let Some(mut world) = self.view.take_world() {
                    world.streamer.restart();
                    let gpu = Gpu {
                        device: &self.target.device,
                        queue: &self.target.queue,
                        capabilities: mantis_render::gpu::Capabilities::of(self.target.device.features()),
                    };
                    if view.attach_world(&gpu, world).is_err() {
                        self.failed_frames += 1;
                    }
                }
                self.view = view;
                self.template = config;
            }
            Err(_) => self.failed_frames += 1,
        }
    }

    fn ui_event(&mut self, event: &crate::threads::render_thread::PlatformEvent) -> bool {
        if self.overlay.as_mut().is_some_and(|o| o.ui_event(event)) {
            return true;
        }
        self.ui.as_mut().is_some_and(|ui| ui.handle(event))
    }

    fn submit(&mut self, frame: &FrameContext<'_>) {
        let gpu = Gpu {
            device: &self.target.device,
            queue: &self.target.queue,
            capabilities: mantis_render::gpu::Capabilities::of(self.target.device.features()),
        };
        let clock = self.clock.clone();
        let now = || clock.as_deref().map(HostClock::now);
        let since =
            |from: Option<crate::time::HostInstant>, to: Option<crate::time::HostInstant>| match (from, to) {
                (Some(a), Some(b)) => b.saturating_since(a),
                _ => Duration::ZERO,
            };
        let t0 = now();
        self.view.stream(&gpu, frame);
        let t1 = now();
        if let Some(o) = self.overlay.as_mut() {
            o.update(&mut self.view, &gpu, frame, &self.timings);
        }
        let _ = self.view.update(frame);
        let t2 = now();
        #[expect(clippy::cast_precision_loss)] // Pixel sizes are far below 2^24.
        let viewport = [
            self.template.renderer.width as f32,
            self.template.renderer.height as f32,
        ];
        if let Some(ui) = self.ui.as_mut() {
            ui.on_actions(frame.actions);
            // Module intents are routed inside; the host has no intents of its own yet.
            ui.drain_intents(|_| {});
            ui.draw(
                self.view.renderer_mut(),
                &self.target.device,
                &self.target.queue,
                viewport,
                1.0,
            );
        }
        if let Some(o) = self.overlay.as_mut() {
            let _ = o.draw(
                self.view.renderer_mut(),
                &self.target.device,
                &self.target.queue,
                viewport,
            );
        }
        let t3 = now();
        let texture = match self.target.surface.get_current_texture() {
            wgpu::CurrentSurfaceTexture::Success(t) | wgpu::CurrentSurfaceTexture::Suboptimal(t) => t,
            wgpu::CurrentSurfaceTexture::Outdated | wgpu::CurrentSurfaceTexture::Lost => {
                let (w, h) = (self.target.config.width, self.target.config.height);
                self.target.resize(w, h);
                return;
            }
            _ => return,
        };
        let view = texture
            .texture
            .create_view(&wgpu::TextureViewDescriptor::default());
        let mut encoder = self
            .target
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor { label: Some("frame") });
        if self
            .view
            .encode(
                &self.target.device,
                &self.target.queue,
                &mut encoder,
                (&texture.texture, &view),
            )
            .is_err()
        {
            self.failed_frames += 1;
            return;
        }
        let _ = self.target.queue.submit(Some(encoder.finish()));
        self.target.queue.present(texture);
        let t4 = now();
        self.timings = FrameTimings {
            stream: since(t0, t1),
            update: since(t1, t2),
            ui: since(t2, t3),
            encode: since(t3, t4),
            total: since(t0, t4),
        };
    }
}

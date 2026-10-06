//! The `town-300` reference scene (plan 17): the client frame-time budget rows are
//! measured on it.
//!
//! - **World.** The cooked toy world, streamed around the camera as the client streams
//!   it ([`crate::world`]).
//! - **Avatars.** 300 animated avatars ([`AVATARS`]) of three kinds (one mesh set, three
//!   tints) from the cooked `avatars/` content: a skeleton, an idle and a walk clip, an
//!   animation graph, near, mid, and far meshes, and the walk baked as vertex animations
//!   for the mid and far tiers. They stand on a fan in front of the camera
//!   ([`town_300`]) and each walks a small circle, so every transform changes every
//!   frame. The crowd selector's reference budgets ([`CrowdConfig::default`]) place
//!   exactly 50 near, 100 mid, and 150 far: every avatar is in view and within range,
//!   and the budgets, not the ranges, decide the tiers.
//! - **Particles.** Twelve braziers (`effects/brazier`) burn among the crowd.
//! - **UI.** The `ui/town/hud` screen: a status bar whose frame counter changes every
//!   frame (text reshaped every frame), a party roster, and a chat log that scrolls.
//!
//! [`TownScene::record`] is one render-thread frame: streaming, characters (tiers,
//! animation on the worker pool, palettes and vertex animation), the UI, renderer
//! preparation, and recording the render graph into the caller's encoder. The caller
//! submits, so a harness can bracket the frame with GPU timestamps.
//!
//! Time is synthetic ([`FRAME_DT`] per frame), so a run is reproducible; only the
//! streaming hand-off reads the host clock (for its budget).

use std::path::Path;
use std::sync::Arc;

use glam::{Mat4, Quat, Vec3};
use mantis_anim::{AnimGraph, Clip, Skeleton};
use mantis_client::characters::{CharacterKind, CharacterStats, Characters};
use mantis_client::content_store::ContentStore;
use mantis_client::platform::MonotonicClock;
use mantis_client::time::HostClock;
use mantis_client::world_stream::{Gpu, WorldStats};
use mantis_client::world_view::StreamedWorld;
use mantis_core::content::ContentHash;
use mantis_core::ecs::EntityId;
use mantis_formats::anim_clip::ClipAsset;
use mantis_formats::anim_graph::GraphAsset;
use mantis_formats::bundle::{AssetKind, Bundle, Domain};
use mantis_formats::material::MaterialAsset;
use mantis_formats::mesh::MeshAsset;
use mantis_formats::particle_effect::ParticleEffect;
use mantis_formats::sh::ShL1;
use mantis_formats::skeleton::SkeletonAsset;
use mantis_formats::vat::VatAsset;
use mantis_render::crowd::CrowdConfig;
use mantis_render::deform::{VatId, VatUpload};
use mantis_render::math::{Camera, Frustum, Sphere};
use mantis_render::particles::EmitterTransform;
use mantis_render::post::PostSettings;
use mantis_render::renderer::{FrameInputs, Renderer, RendererConfig};
use mantis_render::scene::{MaterialId, MeshId};
use mantis_ui::{FontLibrary, ListItem, PropertyId, Ui, Value};

/// Avatars in the scene.
pub const AVATARS: usize = 300;

/// Braziers in the scene.
pub const BRAZIERS: usize = 12;

/// Synthetic seconds per frame.
pub const FRAME_DT: f32 = 1.0 / 60.0;

/// Avatar kinds (tints of one mesh set).
const TINTS: [[f32; 4]; 3] = [
    [0.75, 0.42, 0.28, 1.0],
    [0.3, 0.5, 0.78, 1.0],
    [0.45, 0.68, 0.36, 1.0],
];

/// Radius of the circle each avatar walks (meters).
const WALK_RADIUS: f32 = 0.6;

/// Angular speed of the walk around that circle (radians per second).
const WALK_TURN: f32 = 0.8;

/// One avatar of the layout.
#[derive(Clone, Copy, PartialEq, Debug)]
pub struct AvatarSpot {
    /// Center of the circle it walks.
    pub center: Vec3,
    /// Where on the circle it starts (radians).
    pub phase: f32,
    /// Its kind (tint), `0..3`.
    pub kind: usize,
}

/// Where everything stands.
#[derive(Clone, PartialEq, Debug)]
pub struct TownLayout {
    /// Camera position.
    pub eye: Vec3,
    /// Camera yaw (radians; 0 looks along +Z, decision 0019).
    pub yaw: f32,
    /// Camera pitch (radians, positive up).
    pub pitch: f32,
    /// Vertical field of view (radians).
    pub fov_y: f32,
    /// The avatars.
    pub avatars: Vec<AvatarSpot>,
    /// The braziers.
    pub braziers: Vec<Vec3>,
}

/// The `town-300` layout: the camera at eye height near the south edge of the toy world
/// looking north (all four sectors within the load radius), the avatars on arcs 2 m apart from 4 m out, about 1.5 m apart along
/// each arc within 30 degrees of the view axis (so every avatar is inside the toy world
/// and in view), and the braziers on four arcs among them.
#[allow(
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss
)] // Counts and arc lengths here are small and positive.
pub fn town_300() -> TownLayout {
    let eye = Vec3::new(0.0, 1.7, -28.0);
    let half = 30f32.to_radians();
    let mut avatars = Vec::with_capacity(AVATARS);
    let mut radius = 4.0f32;
    while avatars.len() < AVATARS {
        let arc = radius * 2.0 * half;
        let count = ((arc / 1.5).floor() as usize).max(1);
        for k in 0..count {
            if avatars.len() == AVATARS {
                break;
            }
            let t = if count == 1 {
                0.5
            } else {
                k as f32 / (count - 1) as f32
            };
            let angle = -half + 2.0 * half * t;
            let i = avatars.len();
            avatars.push(AvatarSpot {
                center: eye.with_y(0.0) + radius * Vec3::new(angle.sin(), 0.0, angle.cos()),
                phase: i as f32 * 2.399_963,
                kind: i % TINTS.len(),
            });
        }
        radius += 2.0;
    }
    let braziers = (0..BRAZIERS)
        .map(|i| {
            let ring = (i / 3) as f32;
            let angle = (((i % 3) as f32) - 1.0) * 20f32.to_radians() + 0.08;
            eye.with_y(0.0) + (9.0 + ring * 9.0) * Vec3::new(angle.sin(), 0.0, angle.cos())
        })
        .collect();
    TownLayout {
        eye,
        yaw: 0.0,
        pitch: -0.08,
        fov_y: 1.0,
        avatars,
        braziers,
    }
}

impl AvatarSpot {
    /// The avatar's model matrix `time` seconds in: on its circle, facing along it.
    pub fn model(&self, time: f32) -> Mat4 {
        let a = self.phase + WALK_TURN * time;
        let position = self.center + WALK_RADIUS * Vec3::new(a.cos(), 0.0, a.sin());
        Mat4::from_rotation_translation(Quat::from_rotation_y(-a), position)
    }
}

/// Scene settings.
#[derive(Clone, Copy, Debug)]
pub struct TownConfig {
    /// Output width in pixels.
    pub width: u32,
    /// Output height in pixels.
    pub height: u32,
    /// Output format.
    pub format: wgpu::TextureFormat,
    /// Animation and streaming worker threads (the calling thread included).
    pub workers: usize,
    /// Crowd tiers (the reference budgets by default).
    pub crowd: CrowdConfig,
}

impl TownConfig {
    /// The reference settings at `width x height`.
    pub fn new(width: u32, height: u32, format: wgpu::TextureFormat, workers: usize) -> Self {
        Self {
            width,
            height,
            format,
            workers,
            crowd: CrowdConfig::default(),
        }
    }
}

/// One frame's outcome.
#[derive(Clone, Copy, PartialEq, Debug, Default)]
pub struct TownFrame {
    /// Characters: tiers, changes, refusals.
    pub characters: CharacterStats,
    /// Streaming.
    pub world: WorldStats,
    /// Live particle emitters.
    pub emitters: u32,
    /// UI quads drawn.
    pub ui_quads: usize,
    /// Instances submitted to culling.
    pub instances: u32,
    /// Draw batches.
    pub batches: u32,
}

struct UiProps {
    frame: PropertyId,
    chat: PropertyId,
}

/// The scene.
pub struct TownScene {
    renderer: Renderer,
    world: StreamedWorld,
    characters: Characters,
    ui: Ui,
    props: UiProps,
    chat: Vec<ListItem>,
    layout: TownLayout,
    config: TownConfig,
    frame: u64,
    time: f32,
}

impl core::fmt::Debug for TownScene {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("TownScene")
            .field("frame", &self.frame)
            .field("avatars", &self.characters.len())
            .finish_non_exhaustive()
    }
}

fn entry<'a>(bundle: &'a Bundle, name: &str) -> Result<&'a ContentHash, String> {
    bundle
        .get(name)
        .map(|e| &e.hash)
        .ok_or_else(|| format!("the presentation bundle has no `{name}` (cook packages/toy)"))
}

fn read(store: &ContentStore, bundle: &Bundle, name: &str) -> Result<Vec<u8>, String> {
    store
        .get(entry(bundle, name)?)
        .map_err(|e| format!("{name}: {e}"))
}

fn text(store: &ContentStore, bundle: &Bundle, name: &str) -> Result<String, String> {
    String::from_utf8(read(store, bundle, name)?).map_err(|e| format!("{name}: {e}"))
}

fn vat(renderer: &mut Renderer, queue: &wgpu::Queue, asset: &VatAsset) -> Result<VatId, String> {
    renderer
        .add_vat(
            queue,
            &VatUpload {
                vertex_count: asset.vertex_count,
                frame_count: asset.frame_count,
                seconds_per_frame: asset.seconds_per_frame,
                looping: asset.looping,
                positions: &asset.positions,
                normals: &asset.normals,
                bounds_min: asset.bounds_min,
                bounds_max: asset.bounds_max,
            },
        )
        .map_err(|e| e.to_string())
}

fn plain_mesh(renderer: &mut Renderer, queue: &wgpu::Queue, asset: &MeshAsset) -> Result<MeshId, String> {
    let vertices = mantis_render::assets::mesh_vertices(asset);
    renderer
        .add_mesh(queue, &vertices, &asset.indices)
        .map_err(|e| e.to_string())
}

/// Loads the avatar kinds from the cooked `avatars/` content.
fn avatar_kinds(
    store: &ContentStore,
    bundle: &Bundle,
    renderer: &mut Renderer,
    gpu: &Gpu<'_>,
) -> Result<Vec<Arc<CharacterKind>>, String> {
    let skeleton = SkeletonAsset::parse(&read(store, bundle, "avatars/avatar.skeleton")?)
        .map_err(|e| format!("avatars/avatar.skeleton: {e}"))?;
    let skeleton = Arc::new(Skeleton::new(&skeleton).map_err(|e| e.to_string())?);
    let mut clips = std::collections::BTreeMap::new();
    for e in bundle.entries.iter().filter(|e| e.kind == AssetKind::AnimClip) {
        let bytes = store.get(&e.hash).map_err(|err| format!("{}: {err}", e.name))?;
        let asset = ClipAsset::parse(&bytes).map_err(|err| format!("{}: {err}", e.name))?;
        clips.insert(
            e.hash,
            Arc::new(Clip::new(&asset).map_err(|err| err.to_string())?),
        );
    }
    let graph = GraphAsset::parse(&read(store, bundle, "avatars/avatar.animgraph")?)
        .map_err(|e| format!("avatars/avatar.animgraph: {e}"))?;
    let graph =
        Arc::new(AnimGraph::new(skeleton, &graph, |h| clips.get(h).cloned()).map_err(|e| e.to_string())?);
    let mesh = |name: &str| MeshAsset::parse(&read(store, bundle, name)?).map_err(|e| format!("{name}: {e}"));
    let near = mesh("avatars/avatar_near.skinmesh")?;
    let mid = mesh("avatars/avatar_mid.skinmesh")?;
    let far = mesh("avatars/avatar_far.skinmesh")?;
    let near_mesh = renderer
        .add_mesh_asset(gpu.queue, &near)
        .map_err(|e| e.to_string())?;
    let mid_mesh = plain_mesh(renderer, gpu.queue, &mid)?;
    let far_mesh = plain_mesh(renderer, gpu.queue, &far)?;
    let vat_asset =
        |name: &str| VatAsset::parse(&read(store, bundle, name)?).map_err(|e| format!("{name}: {e}"));
    let mid_vat = vat(renderer, gpu.queue, &vat_asset("avatars/avatar_mid.vat")?)?;
    let far_vat = vat(renderer, gpu.queue, &vat_asset("avatars/avatar_far.vat")?)?;
    let material = MaterialAsset::parse(&read(store, bundle, "avatars/avatar.mat")?)
        .map_err(|e| format!("avatars/avatar.mat: {e}"))?;
    let mut kinds = Vec::with_capacity(TINTS.len());
    for tint in TINTS {
        let id: MaterialId = renderer
            .add_material(gpu.device, gpu.queue, material.clone(), [None; 4])
            .map_err(|e| e.to_string())?;
        renderer
            .set_material_param(gpu.queue, id, "tint", tint)
            .map_err(|e| e.to_string())?;
        kinds.push(Arc::new(CharacterKind {
            graph: Arc::clone(&graph),
            material: id,
            near_mesh,
            mid_mesh,
            far_mesh,
            mid_vat,
            far_vat,
            // Feet at the origin, 1.86 m tall; the swing keeps every vertex within 1.1 m
            // of mid-height.
            bounds: Sphere {
                center: Vec3::new(0.0, 0.93, 0.0),
                radius: 1.1,
            },
            far_frame_rate: 10.0,
        }));
    }
    Ok(kinds)
}

fn party() -> Value {
    let members = [
        ("Tank", 820, 900),
        ("Healer", 510, 560),
        ("Scout", 430, 610),
        ("Mage", 390, 480),
        ("You", 700, 700),
    ];
    Value::List(
        members
            .iter()
            .map(|(name, health, max)| {
                ListItem::new()
                    .with("name", Value::Text((*name).to_owned()))
                    .with("health", Value::Int(*health))
                    .with("health_max", Value::Int(*max))
            })
            .collect(),
    )
}

/// Fonts for the scene's UI when no font file is given: the `ui` stack over the
/// generated printable-ASCII fixture font of `mantis_ui::test_font` (the repository ships
/// no font files). Shaping, the atlas, and the draw list do the same work as with a real
/// font of the same coverage.
///
/// # Errors
/// When the fixture does not load (a `mantis_ui` defect).
pub fn fixture_fonts() -> Result<FontLibrary, String> {
    let mut fonts = FontLibrary::new();
    let id = fonts
        .add_font(mantis_ui::test_font::latin())
        .map_err(|e| format!("{e:?}"))?;
    fonts.define_stack("ui", &[id]).map_err(|e| format!("{e:?}"))?;
    Ok(fonts)
}

/// The entity of avatar `i` (synthetic; the scene has no simulation).
fn avatar_entity(i: usize) -> EntityId {
    EntityId::new(u32::try_from(i).unwrap_or(u32::MAX), 0)
}

/// Lines of chat kept on screen.
const CHAT_LINES: usize = 10;

/// A new chat line every this many frames.
const CHAT_EVERY: u64 = 20;

impl TownScene {
    /// Builds the scene from the cooked store at `store_dir` (verified against
    /// `public_key`, the store's development key when `None`), drawing UI text with
    /// `fonts` (which must define the `ui` stack). Every material is compiled here.
    ///
    /// # Errors
    /// A message naming the missing asset, refused bundle, or renderer refusal.
    pub fn new(
        gpu: &Gpu<'_>,
        store_dir: &Path,
        public_key: Option<[u8; 32]>,
        fonts: FontLibrary,
        config: TownConfig,
    ) -> Result<Self, String> {
        let key = crate::world::public_key(store_dir, public_key)?;
        let store = ContentStore::open(store_dir);
        let bundle = store
            .bundle(Domain::Presentation, &key)
            .map_err(|e| e.to_string())?;
        let mut renderer = Renderer::new(
            gpu.device,
            gpu.queue,
            gpu.capabilities.bindless_textures,
            RendererConfig::new(config.width, config.height, config.format),
        )
        .map_err(|e| e.to_string())?;
        let kinds = avatar_kinds(&store, &bundle, &mut renderer, gpu)?;
        let layout = town_300();
        let mut characters =
            Characters::new(config.crowd, AVATARS, config.workers).map_err(|e| e.to_string())?;
        for (i, spot) in layout.avatars.iter().enumerate() {
            let kind = kinds.get(spot.kind).ok_or("avatar kind")?;
            let entity = avatar_entity(i);
            characters
                .add(entity, Arc::clone(kind), spot.model(0.0))
                .map_err(|e| e.to_string())?;
        }
        let effect = ParticleEffect::parse(&read(&store, &bundle, "effects/brazier.pfx")?)
            .map_err(|e| format!("effects/brazier.pfx: {e}"))?;
        let effect = renderer
            .particles_mut()
            .register(gpu.queue, &effect)
            .map_err(|e| format!("{e:?}"))?;
        for (seed, at) in (1u32..).zip(&layout.braziers) {
            let _ = renderer
                .particles_mut()
                .spawn(effect, EmitterTransform::at(*at + Vec3::Y * 0.9), seed)
                .map_err(|e| format!("{e:?}"))?;
        }
        let mut ui = Ui::new(
            fonts,
            &text(&store, &bundle, "ui/town/hud.layout")?,
            Some(&text(&store, &bundle, "ui/town/hud.theme")?),
        )
        .map_err(|e| e.to_string())?;
        let props = {
            let p = ui.properties_mut();
            let zone = p.intern("town.zone");
            p.set(zone, Value::Text("Town square".to_owned()));
            let avatars = p.intern("town.avatars");
            p.set(avatars, Value::Int(i64::try_from(AVATARS).unwrap_or(0)));
            let roster = p.intern("town.party");
            p.set(roster, party());
            UiProps {
                frame: p.intern("town.frame"),
                chat: p.intern("town.chat"),
            }
        };
        let opened = crate::world::open(store_dir, Some(key), config.workers)?;
        let clock: Arc<dyn HostClock> = Arc::new(MonotonicClock::new());
        let mut world = crate::world::streamed(opened.streamer, clock);
        world
            .streamer
            .preload_materials(&mut renderer, gpu)
            .map_err(|e| e.to_string())?;
        Ok(Self {
            renderer,
            world,
            characters,
            ui,
            props,
            chat: Vec::with_capacity(CHAT_LINES + 1),
            layout,
            config,
            frame: 0,
            time: 0.0,
        })
    }

    /// The layout.
    pub fn layout(&self) -> &TownLayout {
        &self.layout
    }

    /// The renderer.
    pub fn renderer(&self) -> &Renderer {
        &self.renderer
    }

    /// The camera.
    #[allow(clippy::cast_precision_loss)] // Pixel sizes are far below 2^24.
    pub fn camera(&self) -> Camera {
        Camera {
            position: self.layout.eye,
            yaw: self.layout.yaw,
            pitch: self.layout.pitch,
            fov_y: self.layout.fov_y,
            aspect: self.config.width as f32 / self.config.height.max(1) as f32,
            near: 0.1,
        }
    }

    /// Streams until every sector around the camera is resident (at most `frames`
    /// updates, sleeping briefly between them while workers load).
    ///
    /// # Errors
    /// When streaming has not settled after `frames` updates.
    pub fn settle(&mut self, gpu: &Gpu<'_>, frames: u32) -> Result<WorldStats, String> {
        let camera = self.camera();
        let mut last = WorldStats::default();
        for _ in 0..frames {
            last = self.world.streamer.update(
                &mut self.renderer,
                gpu,
                self.world.clock.as_ref(),
                self.world.budget,
                (camera.position, Vec3::ZERO, camera.forward()),
            );
            if last.stream.in_flight == 0 && !last.backlog && last.stream.requested == 0 && last.resident > 0
            {
                return Ok(last);
            }
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
        Err(format!("streaming did not settle: {last:?}"))
    }

    fn update_ui(&mut self) {
        let frame = i64::try_from(self.frame).unwrap_or(i64::MAX);
        let props = self.ui.properties_mut();
        props.set(self.props.frame, Value::Int(frame));
        if self.frame.is_multiple_of(CHAT_EVERY) {
            let n = self.frame / CHAT_EVERY;
            let speakers = ["Tank", "Healer", "Scout", "Mage", "Trader"];
            let speaker = speakers
                .get(usize::try_from(n).unwrap_or(0) % speakers.len())
                .copied()
                .unwrap_or("Trader");
            self.chat.push(
                ListItem::new()
                    .with("speaker", Value::Text(speaker.to_owned()))
                    .with("line", Value::Text(format!("meet at the fountain, call {n}"))),
            );
            if self.chat.len() > CHAT_LINES {
                self.chat.remove(0);
            }
            props.set(self.props.chat, Value::List(self.chat.clone()));
        }
    }

    /// One render-thread frame, recorded into `encoder` and drawn into `target` (the
    /// caller submits).
    ///
    /// # Errors
    /// A renderer refusal, as text.
    #[allow(clippy::cast_precision_loss)] // Pixel sizes are far below 2^24.
    pub fn record(
        &mut self,
        gpu: &Gpu<'_>,
        encoder: &mut wgpu::CommandEncoder,
        target: (&wgpu::Texture, &wgpu::TextureView),
    ) -> Result<TownFrame, String> {
        self.frame += 1;
        self.time += FRAME_DT;
        let camera = self.camera();
        let world = self.world.streamer.update(
            &mut self.renderer,
            gpu,
            self.world.clock.as_ref(),
            self.world.budget,
            (camera.position, Vec3::ZERO, camera.forward()),
        );
        self.world.last = world;
        for (i, spot) in self.layout.avatars.iter().enumerate() {
            let _ = self
                .characters
                .set_transform(avatar_entity(i), spot.model(self.time));
        }
        let frustum = Frustum::from_view_projection(&camera.view_projection());
        let characters = self
            .characters
            .update(&mut self.renderer, camera.position, &frustum, FRAME_DT)
            .map_err(|e| e.to_string())?;
        self.update_ui();
        let viewport = [self.config.width as f32, self.config.height as f32];
        let _ = self.ui.frame(viewport, 1.0);
        let (list, atlas) = self.ui.draw_parts();
        let ui_quads = list.quads.len();
        self.renderer.set_ui(gpu.device, gpu.queue, &list.quads, atlas);
        let stats = self.renderer.prepare(&FrameInputs {
            camera,
            time: self.time,
            sun_direction: Vec3::new(0.4, -1.0, 0.3),
            sun_color: Vec3::splat(2.5),
            shadows: true,
            sky: ShL1::constant([0.35, 0.4, 0.5]),
            ambient_intensity: 1.0,
            exposure: 1.0,
            clear_color: [0.45, 0.6, 0.8, 1.0],
            ssao_strength: 0.5,
            post: PostSettings::default(),
        });
        self.renderer
            .encode(gpu.device, gpu.queue, encoder, target)
            .map_err(|e| e.to_string())?;
        Ok(TownFrame {
            characters,
            world,
            emitters: self.renderer.particles().live_emitters(),
            ui_quads,
            instances: stats.instances,
            batches: stats.batches,
        })
    }
}

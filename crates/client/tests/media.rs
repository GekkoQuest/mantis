//! Render-thread media glue: the mixer behind the audio thread, presentation actions
//! reaching sounds, effects, camera shakes, and animation triggers, and characters moving
//! between crowd tiers on a headless (no-op) renderer.

#![expect(clippy::cast_precision_loss)] // Small test counts.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use glam::{Mat4, Quat, Vec3};
use mantis_anim::skeleton::inverse_bind_matrices;
use mantis_anim::{AnimGraph, Clip, Skeleton};
use mantis_audio::{AudioEvent, HandleAllocator, Mixer, MixerConfig};
use mantis_client::characters::{CharacterKind, Characters};
use mantis_client::media::{EffectSpawner, EntityPositions, MediaSink, MediaState};
use mantis_client::presentation::{PresentationConfig, PresentationLibrary, Presenter};
use mantis_client::threads::audio::{AudioBackend, AudioOutput, AudioThread};
use mantis_client::time::HostInstant;
use mantis_core::content::ContentHash;
use mantis_core::ecs::EntityId;
use mantis_core::graph::{GraphId, GraphInstanceId, MarkerId, MarkerKind, NodeKey, TimelineMarker};
use mantis_core::time::Tick;
use mantis_formats::anim_clip::{Channel, ClipAsset, Interpolation, TrackDef};
use mantis_formats::anim_graph::{GraphAsset, LayerDef, LayerMode, NodeDef, ParameterDef, ParameterKind};
use mantis_formats::material::{
    Deformations, LightingModel, MaterialAsset, MaterialGraph, Node, NodeId, SurfaceOutputs,
};
use mantis_formats::mixer_graph::{Bus, MixerGraph};
use mantis_formats::presentation::{Action, ActionOp, Anchor, Binding, MarkerFilter, PresentationGraph};
use mantis_formats::skeleton::{BoneDef, SkeletonAsset, bone_name_hash};
use mantis_formats::sound_bank::{
    Attenuation, AttenuationModel, Clip as SoundClip, ClipSamples, ClipSelection, Sound, SoundBank,
    StealPolicy,
};
use mantis_render::crowd::{CrowdConfig, CrowdTier};
use mantis_render::deform::VatUpload;
use mantis_render::gpu_test::noop;
use mantis_render::gpu_types::SkinVertex;
use mantis_render::math::{Camera, Frustum, Sphere};
use mantis_render::mesh::cube;
use mantis_render::post::PostSettings;
use mantis_render::renderer::{FrameInputs, Renderer, RendererConfig};

#[global_allocator]
static ALLOC: mantis_testkit::alloc::CountingAllocator = mantis_testkit::alloc::CountingAllocator;

type TestResult = Result<(), Box<dyn std::error::Error>>;

// ---------------------------------------------------------------------------------------
// Audio
// ---------------------------------------------------------------------------------------

fn bank() -> SoundBank {
    SoundBank {
        sample_rate: 48_000,
        clips: vec![SoundClip {
            channels: 1,
            samples: ClipSamples::F32(vec![0.5; 4800]),
        }],
        sounds: vec![Sound {
            id: 7,
            clips: vec![0],
            selection: ClipSelection::RoundRobin,
            volume: 1.0,
            pitch_min: 1.0,
            pitch_max: 1.0,
            looping: false,
            spatial: false,
            bus: 1,
            priority: 100,
            max_instances: 8,
            steal: StealPolicy::Oldest,
            attenuation: Attenuation {
                model: AttenuationModel::Linear,
                min_distance: 1.0,
                max_distance: 50.0,
                rolloff: 1.0,
            },
        }],
    }
}

fn mixer_graph() -> MixerGraph {
    MixerGraph {
        buses: vec![
            Bus {
                id: 0,
                parent: None,
                gain: 1.0,
                effects: Vec::new(),
            },
            Bus {
                id: 1,
                parent: Some(0),
                gain: 1.0,
                effects: Vec::new(),
            },
        ],
    }
}

/// An output that records the loudest sample it was given.
struct PeakOutput {
    peak: Arc<Mutex<f32>>,
}

impl AudioOutput for PeakOutput {
    fn block_len(&self) -> usize {
        256
    }
    fn write(&mut self, block: &[f32]) -> Duration {
        let loudest = block.iter().fold(0.0f32, |m, s| m.max(s.abs()));
        if let Ok(mut p) = self.peak.lock() {
            *p = p.max(loudest);
        }
        Duration::from_micros(500)
    }
}

#[test]
fn the_mixer_plays_behind_the_audio_thread() -> TestResult {
    let mixer = Mixer::new(
        &bank(),
        &mixer_graph(),
        MixerConfig {
            sample_rate: 48_000,
            ..MixerConfig::default()
        },
    )?;
    let peak = Arc::new(Mutex::new(0.0f32));
    let (thread, tx) = AudioThread::spawn(
        mixer,
        PeakOutput {
            peak: Arc::clone(&peak),
        },
        64,
    )?;
    let voices = HandleAllocator::new();
    tx.send(AudioEvent::play(mantis_audio::SoundId(7), voices.allocate()))?;
    let mut heard = 0.0;
    for _ in 0..2000 {
        heard = peak.lock().map_or(0.0, |p| *p);
        if heard > 0.1 {
            break;
        }
        std::thread::sleep(Duration::from_millis(1));
    }
    let (mixer, _) = thread.stop()?;
    assert!(heard > 0.1, "peak {heard}");
    assert_eq!(mixer.stats().ignored, 0);
    Ok(())
}

// ---------------------------------------------------------------------------------------
// Presentation through the media sink
// ---------------------------------------------------------------------------------------

struct Positions(Vec<(EntityId, Vec3)>);

impl EntityPositions for Positions {
    fn position(&self, entity: EntityId) -> Option<Vec3> {
        self.0.iter().find(|(e, _)| *e == entity).map(|(_, p)| *p)
    }
}

#[derive(Default)]
struct Effects(Vec<(ContentHash, Vec3, Option<EntityId>)>);

impl EffectSpawner for Effects {
    fn spawn_effect(
        &mut self,
        effect: ContentHash,
        position: Vec3,
        _scale: f32,
        follow: Option<EntityId>,
    ) -> bool {
        self.0.push((effect, position, follow));
        true
    }
}

/// Records every event the audio thread receives.
struct EventLog(Arc<Mutex<Vec<AudioEvent>>>);

impl AudioBackend for EventLog {
    type Event = AudioEvent;
    fn handle(&mut self, event: AudioEvent) {
        if let Ok(mut l) = self.0.lock() {
            l.push(event);
        }
    }
    fn render(&mut self, out: &mut [f32]) {
        out.fill(0.0);
    }
}

struct NullOut;

impl AudioOutput for NullOut {
    fn block_len(&self) -> usize {
        64
    }
    fn write(&mut self, _: &[f32]) -> Duration {
        Duration::from_micros(200)
    }
}

const CASTER: EntityId = EntityId::new(1, 0);
const TARGET: EntityId = EntityId::new(2, 0);

fn presentation() -> PresentationGraph {
    let act = |anchor, op| Action {
        delay: 0.0,
        anchor,
        offset: [0.0, 1.0, 0.0],
        op,
    };
    PresentationGraph {
        bindings: vec![Binding {
            graph: 3,
            node: 1,
            filter: MarkerFilter::Impact,
            actions: vec![
                act(
                    Anchor::Target,
                    ActionOp::SpawnEffect {
                        effect: ContentHash::from_bytes([9; 32]),
                        scale: 1.0,
                        follow: true,
                    },
                ),
                act(
                    Anchor::Source,
                    ActionOp::PlaySound {
                        sound: 7,
                        volume: 1.0,
                        pitch: 1.0,
                        follow: true,
                    },
                ),
                act(
                    Anchor::Target,
                    ActionOp::CameraShake {
                        amplitude: 2.0,
                        frequency: 10.0,
                        duration: 0.5,
                        radius: 20.0,
                    },
                ),
                act(
                    Anchor::Target,
                    ActionOp::AnimTrigger {
                        parameter: bone_name_hash("flinch"),
                    },
                ),
            ],
        }],
    }
}

#[test]
fn presentation_actions_reach_every_media_system() -> TestResult {
    let mut library = PresentationLibrary::new();
    library.load(&presentation());
    let mut presenter = Presenter::new(library, PresentationConfig::default());
    let log = Arc::new(Mutex::new(Vec::new()));
    let (audio, tx) = AudioThread::spawn(EventLog(Arc::clone(&log)), NullOut, 64)?;
    let voices = HandleAllocator::new();
    let positions = Positions(vec![
        (CASTER, Vec3::new(0.0, 0.0, -5.0)),
        (TARGET, Vec3::new(2.0, 0.0, -6.0)),
    ]);
    let mut effects = Effects::default();
    let mut state = MediaState::new(4, 8);
    // A character for the target, so the animation trigger has somewhere to go.
    let ctx = noop()?;
    let (mut renderer, kind) = character_rig(&ctx)?;
    let mut characters = Characters::new(CrowdConfig::default(), 8, 1)?;
    characters.add(
        TARGET,
        Arc::clone(&kind),
        Mat4::from_translation(Vec3::new(2.0, 0.0, -6.0)),
    )?;

    let now = HostInstant::from_nanos(1_000_000_000);
    let marker = TimelineMarker {
        id: MarkerId {
            graph: GraphId(3),
            node: NodeKey(1),
        },
        kind: MarkerKind::Impact { target: TARGET },
        at: Tick(10),
        offset: 0,
        source: CASTER,
        target: Some(TARGET),
        instance: GraphInstanceId(1),
    };
    presenter.on_marker(&marker, now, now);
    let fired = presenter.update(
        now,
        &mut MediaSink {
            positions: &positions,
            effects: &mut effects,
            audio: Some(&tx),
            voices: &voices,
            characters: Some(&mut characters),
            state: &mut state,
            camera: Vec3::ZERO,
            now,
        },
    );
    assert_eq!(fired, 4);
    let s = state.stats;
    assert_eq!(
        (
            s.effects,
            s.sounds,
            s.shakes,
            s.triggers,
            s.refused,
            s.no_position
        ),
        (1, 1, 1, 1, 0, 0)
    );
    assert_eq!(
        effects.0,
        [(
            ContentHash::from_bytes([9; 32]),
            Vec3::new(2.0, 1.0, -6.0),
            Some(TARGET)
        )]
    );
    assert_eq!(state.shakes.len(), 1);
    // The following sound's emitter tracks its entity.
    state.follow(now, &positions, &tx);
    let _ = renderer.prepare(&inputs());
    let _ = audio.stop()?;
    let events = log.lock().map(|l| l.clone()).unwrap_or_default();
    assert!(
        matches!(
            events.first(),
            Some(AudioEvent::Play {
                emitter: Some(_),
                position: Some([0.0, 1.0, -5.0]),
                ..
            })
        ),
        "{events:?}"
    );
    assert!(
        matches!(
            events.get(1),
            Some(AudioEvent::SetEmitter {
                position: [0.0, 0.0, -5.0],
                ..
            })
        ),
        "{events:?}"
    );
    Ok(())
}

// ---------------------------------------------------------------------------------------
// Characters and crowd tiers
// ---------------------------------------------------------------------------------------

/// A two-bone skeleton with a looping clip that bends the second bone, and a trigger.
fn anim_graph() -> Result<Arc<AnimGraph>, Box<dyn std::error::Error>> {
    let mut bones = vec![
        BoneDef {
            parent: None,
            name_hash: bone_name_hash("root"),
            translation: [0.0; 3],
            rotation: [0.0, 0.0, 0.0, 1.0],
            scale: [1.0; 3],
            inverse_bind: [0.0; 16],
        },
        BoneDef {
            parent: Some(0),
            name_hash: bone_name_hash("tip"),
            translation: [0.0, 0.5, 0.0],
            rotation: [0.0, 0.0, 0.0, 1.0],
            scale: [1.0; 3],
            inverse_bind: [0.0; 16],
        },
    ];
    let inverse = inverse_bind_matrices(&bones);
    for (b, m) in bones.iter_mut().zip(inverse) {
        b.inverse_bind = m;
    }
    let skeleton = Arc::new(Skeleton::new(&SkeletonAsset { bones })?);
    let values = [0.0f32, 0.8, 0.0]
        .iter()
        .flat_map(|a| Quat::from_rotation_z(*a).to_array())
        .collect();
    let clip_asset = ClipAsset {
        duration: 1.0,
        sample_rate: 30.0,
        looping: true,
        root_motion: false,
        bone_count: 2,
        tracks: vec![TrackDef {
            bone: 1,
            channel: Channel::Rotation,
            interpolation: Interpolation::Linear,
            times: vec![0.0, 0.5, 1.0],
            values,
        }],
    };
    let hash = ContentHash::of(&clip_asset.encode());
    let clip = Arc::new(Clip::new(&clip_asset)?);
    let graph = GraphAsset {
        bone_count: 2,
        clips: vec![hash],
        parameters: vec![ParameterDef {
            name_hash: bone_name_hash("flinch"),
            kind: ParameterKind::Trigger,
            default: 0.0,
        }],
        nodes: vec![NodeDef::Clip { clip: 0, speed: 1.0 }],
        layers: vec![LayerDef {
            node: 0,
            weight: 1.0,
            mode: LayerMode::Override,
            reference_clip: None,
            mask: vec![],
        }],
        foot_chains: vec![],
        look_at: None,
    };
    Ok(Arc::new(AnimGraph::new(skeleton, &graph, |h| {
        (*h == hash).then(|| Arc::clone(&clip))
    })?))
}

fn inputs() -> FrameInputs {
    FrameInputs {
        camera: camera(),
        time: 0.0,
        sun_direction: Vec3::NEG_Y,
        sun_color: Vec3::ONE,
        shadows: false,
        sky: mantis_formats::sh::ShL1::ZERO,
        ambient_intensity: 1.0,
        exposure: 1.0,
        clear_color: [0.0; 4],
        ssao_strength: 0.0,
        post: PostSettings::default(),
    }
}

fn camera() -> Camera {
    Camera {
        position: Vec3::ZERO,
        yaw: 0.0,
        pitch: 0.0,
        fov_y: 1.0,
        aspect: 1.0,
        near: 0.1,
    }
}

fn character_rig(
    ctx: &mantis_render::gpu::GpuContext,
) -> Result<(Renderer, Arc<CharacterKind>), Box<dyn std::error::Error>> {
    let mut config = RendererConfig::new(32, 32, wgpu::TextureFormat::Rgba8Unorm);
    config.max_instances = 64;
    config.max_batches = 16;
    config.max_vertices = 1024;
    config.max_indices = 4096;
    config.shadow_resolution = 64;
    config.palette_slots = 3 * 2 * 16;
    config.vat_slots = 4096;
    let mut renderer = Renderer::new(&ctx.device, &ctx.queue, false, config)?;
    let graph = MaterialGraph {
        nodes: vec![Node::ColorParam(0), Node::Swizzle(NodeId(0), [0, 1, 2, 0], 3)],
        outputs: SurfaceOutputs {
            base_color: NodeId(1),
            alpha: None,
            emissive: None,
            alpha_cutoff: None,
        },
        lighting: LightingModel::Lambert,
        rim: None,
        outline: None,
    };
    let material = renderer.add_material(
        &ctx.device,
        &ctx.queue,
        MaterialAsset {
            graph: graph.validate()?,
            casts_shadows: true,
            deformations: Deformations::ALL,
            bindings: mantis_formats::material::MaterialBindings::default(),
        },
        [None; 4],
    )?;
    let (v, i) = cube();
    let skin: Vec<SkinVertex> = v
        .iter()
        .map(|vert| SkinVertex {
            joints: [u16::from(vert.position[1] > 0.0), 0, 0, 0],
            weights: [255, 0, 0, 0],
        })
        .collect();
    let near_mesh = renderer.add_skinned_mesh(&ctx.queue, &v, &skin, &i)?;
    let mid_mesh = renderer.add_mesh(&ctx.queue, &v, &i)?;
    let far_mesh = renderer.add_mesh(&ctx.queue, &v, &i)?;
    let frames = 4usize;
    let positions: Vec<[f32; 4]> = (0..frames)
        .flat_map(|f| {
            v.iter().map(move |vert| {
                [
                    vert.position[0],
                    vert.position[1] + f as f32 * 0.1,
                    vert.position[2],
                    1.0,
                ]
            })
        })
        .collect();
    let normals: Vec<[f32; 4]> = (0..frames)
        .flat_map(|_| {
            v.iter()
                .map(|vert| [vert.normal[0], vert.normal[1], vert.normal[2], 0.0])
        })
        .collect();
    let upload = VatUpload {
        vertex_count: u32::try_from(v.len())?,
        frame_count: u32::try_from(frames)?,
        seconds_per_frame: 0.25,
        looping: true,
        positions: &positions,
        normals: &normals,
        bounds_min: [-0.5, -0.5, -0.5],
        bounds_max: [0.5, 0.9, 0.5],
    };
    let mid_vat = renderer.add_vat(&ctx.queue, &upload)?;
    let far_vat = renderer.add_vat(&ctx.queue, &upload)?;
    let kind = Arc::new(CharacterKind {
        graph: anim_graph()?,
        material,
        near_mesh,
        mid_mesh,
        far_mesh,
        mid_vat,
        far_vat,
        bounds: Sphere {
            center: Vec3::ZERO,
            radius: 1.5,
        },
        far_frame_rate: 4.0,
    });
    Ok((renderer, kind))
}

#[test]
fn characters_move_between_crowd_tiers() -> TestResult {
    let ctx = noop()?;
    let (mut renderer, kind) = character_rig(&ctx)?;
    let mut characters = Characters::new(CrowdConfig::default(), 16, 3)?;
    assert_eq!(characters.workers(), 3);
    let place = |z: f32| Mat4::from_translation(Vec3::new(0.0, 0.0, z));
    let near = EntityId::new(1, 0);
    let mid = EntityId::new(2, 0);
    let far = EntityId::new(3, 0);
    let behind = EntityId::new(4, 0);
    for (e, z) in [(near, 5.0), (mid, 40.0), (far, 150.0), (behind, -10.0)] {
        characters.add(e, Arc::clone(&kind), place(z))?;
    }
    assert!(
        characters.add(near, Arc::clone(&kind), place(0.0)).is_err(),
        "duplicate"
    );
    let cam = camera();
    let frustum = Frustum::from_view_projection(&cam.view_projection());
    let stats = characters.update(&mut renderer, cam.position, &frustum, 1.0 / 60.0)?;
    assert_eq!(
        (stats.crowd.near, stats.crowd.mid, stats.crowd.far, stats.refused),
        (1, 1, 1, 0)
    );
    let tiers: Vec<_> = [near, mid, far, behind]
        .iter()
        .map(|e| characters.tier(*e))
        .collect();
    assert_eq!(
        tiers,
        [
            Some(CrowdTier::Near),
            Some(CrowdTier::Mid),
            Some(CrowdTier::Far),
            Some(CrowdTier::Culled)
        ]
    );
    assert_eq!(renderer.prepare(&inputs()).instances, 3);
    let _ = characters.update(&mut renderer, cam.position, &frustum, 1.0 / 60.0)?;
    // Walking the near character out to the mid band swaps its instance.
    characters.set_transform(near, place(40.0))?;
    let stats = characters.update(&mut renderer, cam.position, &frustum, 1.0 / 60.0)?;
    assert_eq!((stats.tier_changes, stats.crowd.near, stats.crowd.mid), (1, 0, 2));
    assert_eq!(renderer.prepare(&inputs()).instances, 3);
    // Triggers by name hash reach the graph; unknown names do not.
    assert!(characters.trigger(mid, bone_name_hash("flinch")));
    assert!(!characters.trigger(mid, bone_name_hash("unknown")));
    // Removal releases the instance.
    characters.remove(&mut renderer, mid)?;
    let _ = characters.update(&mut renderer, cam.position, &frustum, 1.0 / 60.0)?;
    assert_eq!(renderer.prepare(&inputs()).instances, 2);
    assert_eq!(characters.len(), 3);
    Ok(())
}

#[test]
fn steady_state_media_allocates_nothing() -> TestResult {
    steady_state_media(1)
}

#[test]
fn steady_state_media_allocates_nothing_with_animation_workers() -> TestResult {
    steady_state_media(4)
}

/// Heap operations the animation workers performed inside counting scopes.
static WORKER_OPS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
/// Worker shares run (so the test knows the workers did run).
static WORKER_RUNS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

fn count_worker(f: &mut dyn FnMut()) {
    let ((), stats) = mantis_testkit::alloc::count_allocs(f);
    WORKER_OPS.fetch_add(stats.total_ops(), std::sync::atomic::Ordering::Relaxed);
    WORKER_RUNS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
}

fn steady_state_media(workers: usize) -> TestResult {
    let ctx = noop()?;
    let (mut renderer, kind) = character_rig(&ctx)?;
    let mut characters = Characters::with_worker_wrapper(CrowdConfig::default(), 16, workers, count_worker)?;
    for (i, z) in [5.0f32, 8.0, 40.0, 150.0].into_iter().enumerate() {
        characters.add(
            EntityId::new(u32::try_from(i)? + 1, 0),
            Arc::clone(&kind),
            Mat4::from_translation(Vec3::Z * z),
        )?;
    }
    let mut library = PresentationLibrary::new();
    library.load(&presentation());
    let mut presenter = Presenter::new(library, PresentationConfig::default());
    let log = Arc::new(Mutex::new(Vec::with_capacity(4096)));
    let (audio, tx) = AudioThread::spawn(EventLog(Arc::clone(&log)), NullOut, 1024)?;
    let voices = HandleAllocator::new();
    let positions = Positions(vec![(CASTER, Vec3::NEG_Z), (TARGET, Vec3::NEG_Z * 2.0)]);
    let mut effects = Effects(Vec::with_capacity(4096));
    let mut state = MediaState::new(4, 8);
    let cam = camera();
    let frustum = Frustum::from_view_projection(&cam.view_projection());
    let mut frame = |n: u64, characters: &mut Characters, renderer: &mut Renderer| -> TestResult {
        let now = HostInstant::from_nanos(1_000_000_000 + n * 16_000_000);
        let _ = characters.update(renderer, cam.position, &frustum, 1.0 / 60.0)?;
        let marker = TimelineMarker {
            id: MarkerId {
                graph: GraphId(3),
                node: NodeKey(1),
            },
            kind: MarkerKind::Impact { target: TARGET },
            at: Tick(n),
            offset: 0,
            source: CASTER,
            target: Some(TARGET),
            instance: GraphInstanceId(n),
        };
        presenter.on_marker(&marker, now, now);
        let _ = presenter.update(
            now,
            &mut MediaSink {
                positions: &positions,
                effects: &mut effects,
                audio: Some(&tx),
                voices: &voices,
                characters: Some(characters),
                state: &mut state,
                camera: Vec3::ZERO,
                now,
            },
        );
        state.follow(now, &positions, &tx);
        let _ = state.shakes.sample(now);
        Ok(())
    };
    for n in 0..8 {
        frame(n, &mut characters, &mut renderer)?;
    }
    let ops_before = WORKER_OPS.load(std::sync::atomic::Ordering::Relaxed);
    let runs_before = WORKER_RUNS.load(std::sync::atomic::Ordering::Relaxed);
    mantis_testkit::alloc::assert_no_alloc("characters, presentation, media", || {
        for n in 8..64 {
            frame(n, &mut characters, &mut renderer)?;
        }
        Ok::<(), Box<dyn std::error::Error>>(())
    })?;
    if workers > 1 {
        assert!(
            WORKER_RUNS.load(std::sync::atomic::Ordering::Relaxed) >= runs_before + 56,
            "the workers ran every frame"
        );
        assert_eq!(
            WORKER_OPS.load(std::sync::atomic::Ordering::Relaxed),
            ops_before,
            "the animation workers allocate nothing in steady state"
        );
    }
    let _ = audio.stop()?;
    Ok(())
}

#[test]
fn effects_spawn_into_the_particle_system_and_follow() -> TestResult {
    use mantis_client::media::{EffectLibrary, ParticleEffects};
    use mantis_formats::particle_effect::{
        BlendMode, Burst, ColorKey, EmitterDef, EmitterShape, ParticleEffect, SimulationSpace, SizeKey,
    };
    let ctx = noop()?;
    let (mut renderer, _) = character_rig(&ctx)?;
    let effect = ParticleEffect {
        emitters: vec![EmitterDef {
            capacity: 8,
            duration: 1.0,
            looping: true,
            rate: 8.0,
            bursts: vec![Burst { time: 0.0, count: 2 }],
            lifetime_min: 0.5,
            lifetime_max: 0.5,
            shape: EmitterShape::Point,
            speed_min: 0.0,
            speed_max: 1.0,
            acceleration: [0.0; 3],
            drag: 0.0,
            color: vec![ColorKey {
                t: 0.0,
                value: [1.0, 0.5, 0.2, 1.0],
            }],
            size: vec![SizeKey { t: 0.0, value: [0.2] }],
            blend: BlendMode::Additive,
            space: SimulationSpace::World,
        }],
    };
    let hash = ContentHash::of(&effect.encode());
    let mut library = EffectLibrary::default();
    let _ = library.register(&mut renderer, &ctx.queue, hash, &effect)?;
    let mut effects = ParticleEffects::new(library, 4);
    let mut lib = PresentationLibrary::new();
    lib.load(&PresentationGraph {
        bindings: vec![Binding {
            graph: 3,
            node: 1,
            filter: MarkerFilter::Any,
            actions: vec![Action {
                delay: 0.0,
                anchor: Anchor::Target,
                offset: [0.0; 3],
                op: ActionOp::SpawnEffect {
                    effect: hash,
                    scale: 1.0,
                    follow: true,
                },
            }],
        }],
    });
    let mut presenter = Presenter::new(lib, PresentationConfig::default());
    let mut positions = Positions(vec![(TARGET, Vec3::new(0.0, 0.0, -4.0))]);
    let mut state = MediaState::new(2, 2);
    let voices = HandleAllocator::new();
    let now = HostInstant::from_nanos(1_000_000_000);
    let marker = TimelineMarker {
        id: MarkerId {
            graph: GraphId(3),
            node: NodeKey(1),
        },
        kind: MarkerKind::CastStart,
        at: Tick(1),
        offset: 0,
        source: CASTER,
        target: Some(TARGET),
        instance: GraphInstanceId(1),
    };
    presenter.on_marker(&marker, now, now);
    let _ = presenter.update(
        now,
        &mut MediaSink {
            positions: &positions,
            effects: &mut effects.spawner(&mut renderer),
            audio: None,
            voices: &voices,
            characters: None,
            state: &mut state,
            camera: Vec3::ZERO,
            now,
        },
    );
    assert_eq!(state.stats.effects, 1);
    assert_eq!(renderer.particles().live_emitters(), 1);
    // Unknown effects are refused, not errors.
    let mut spawner = effects.spawner(&mut renderer);
    assert!(!spawner.spawn_effect(ContentHash::from_bytes([1; 32]), Vec3::ZERO, 1.0, None));
    // Following moves the instance with its entity each frame.
    positions.0 = vec![(TARGET, Vec3::new(1.0, 0.0, -4.0))];
    effects.follow(&mut renderer, &positions);
    let frame_stats = renderer.prepare(&inputs());
    assert_eq!(frame_stats.particle_emitters, 1);
    Ok(())
}

#[test]
fn the_worker_pool_animates_exactly_like_the_calling_thread() -> TestResult {
    use mantis_client::anim_pool::AnimPool;
    let ctx = noop()?;
    let (_renderer, kind) = character_rig(&ctx)?;
    let mut one = AnimPool::new(1, 12)?;
    let mut four = AnimPool::new(4, 12)?;
    let mut slots = Vec::new();
    for i in 0..10u32 {
        let entity = EntityId::new(i + 1, 0);
        let a = one.add(entity, mantis_anim::AnimInstance::new(Arc::clone(&kind.graph)));
        let b = four.add(entity, mantis_anim::AnimInstance::new(Arc::clone(&kind.graph)));
        // Every other character is near; the rest stay still.
        one.set_near(a, i % 2 == 0);
        four.set_near(b, i % 2 == 0);
        slots.push((a, b));
    }
    // Every instance has its own slot.
    assert_eq!(
        slots
            .iter()
            .map(|(_, b)| b)
            .collect::<std::collections::BTreeSet<_>>()
            .len(),
        10
    );
    for _ in 0..3 {
        one.run(1.0 / 60.0);
        four.run(1.0 / 60.0);
    }
    mantis_testkit::alloc::assert_no_alloc("animation worker pool", || {
        for _ in 0..30 {
            one.run(1.0 / 60.0);
            four.run(1.0 / 60.0);
        }
    });
    for (a, b) in &slots {
        let pa = one.with(*a, |x| x.palette().to_vec()).ok_or("one")?;
        let pb = four.with(*b, |x| x.palette().to_vec()).ok_or("four")?;
        assert_eq!(pa, pb, "same graph, same time, same pose on any worker");
    }
    // Removal moves the shard's last instance into the hole.
    let (_, first) = *slots.first().ok_or("slot")?;
    let _ = four.remove(first);
    assert!(
        four.with(first, |_| ()).is_some(),
        "the shard's last instance moved into the hole"
    );
    Ok(())
}

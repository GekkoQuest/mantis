//! World editing end to end, on a copy of the toy package's content: placements are
//! moved (within and across sectors), turned, added, and removed; only the edited lines
//! of the sources change; the recook changes the presentation bundle but never the
//! handshake hash; and the live renderer streams the edited world after a reload.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use glam::Vec3;
use mantis_client::content_store::ContentStore;
use mantis_client::time::ManualClock;
use mantis_client::world_stream::{Gpu, WorldStats};
use mantis_client::world_view::StreamedWorld;
use mantis_editor::world::{PlacementRef, WorldEditor, open_world, recook, reload};
use mantis_formats::bundle::Domain;
use mantis_formats::sector::Sector;
use mantis_render::gpu_test::noop;
use mantis_render::renderer::{Renderer, RendererConfig};
use mantis_render::streaming::StreamingConfig;

type TestResult = Result<(), Box<dyn std::error::Error>>;
type Error = Box<dyn std::error::Error>;

struct TempDir(PathBuf);

impl TempDir {
    fn new(name: &str) -> std::io::Result<Self> {
        let p = std::env::temp_dir().join(format!("mantis-editor-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&p);
        std::fs::create_dir_all(&p)?;
        Ok(Self(p))
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn copy_dir(from: &Path, to: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(to)?;
    for e in std::fs::read_dir(from)? {
        let e = e?;
        let target = to.join(e.file_name());
        if e.file_type()?.is_dir() {
            copy_dir(&e.path(), &target)?;
        } else {
            std::fs::copy(e.path(), target)?;
        }
    }
    Ok(())
}

fn toy_content() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../packages/toy/content")
}

/// Every placement translation of the store's visual sector `(x, z)`.
fn placed(store: &Path, x: i32, z: i32) -> Result<Vec<[f32; 3]>, Error> {
    let key = std::fs::read(store.join("keys/dev.pub"))?;
    let content = ContentStore::open(store);
    let bundle = content.bundle(Domain::Presentation, &key)?;
    let entry = bundle
        .get(&format!("sectors/{x}_{z}.visual"))
        .ok_or("visual sector")?;
    let sector = Sector::parse(&content.get(&entry.hash)?)?;
    Ok(sector
        .placements
        .iter()
        .flatten()
        .map(|p| [p.transform[9], p.transform[10], p.transform[11]])
        .collect())
}

fn near(a: [f32; 3], b: [f32; 3]) -> bool {
    a.iter().zip(b).all(|(x, y)| (x - y).abs() < 1e-4)
}

fn settle(world: &mut StreamedWorld, r: &mut Renderer, gpu: &Gpu<'_>) -> Result<WorldStats, Error> {
    let clock = ManualClock::new();
    let mut last = WorldStats::default();
    for _ in 0..4000 {
        last = world.streamer.update(
            r,
            gpu,
            &clock,
            Duration::from_millis(5),
            (Vec3::ZERO, Vec3::ZERO, Vec3::Z),
        );
        if last.stream.in_flight == 0 && !last.backlog && last.stream.requested == 0 && last.resident == 4 {
            return Ok(last);
        }
        std::thread::sleep(Duration::from_millis(1));
    }
    Err(format!("streaming did not settle: {last:?}").into())
}

#[test]
fn placement_edits_write_back_recook_and_stream_again() -> TestResult {
    let dir = TempDir::new("world")?;
    let content = dir.0.join("content");
    copy_dir(&toy_content(), &content)?;
    let store = dir.0.join("cooked");
    let before = recook(&content, &store)?;
    let source_00 = std::fs::read_to_string(content.join("sectors/0_0.sector.toml"))?;

    let mut ed = WorldEditor::open(&content)?;
    let all = ed.placements();
    assert_eq!(all.len(), 10, "ten crates over four sectors");
    let crate_01 = PlacementRef {
        sector: (0, 0),
        name: "crate_01".to_owned(),
    };
    assert_eq!(ed.placement(&crate_01).map(|p| p.position), Some([8.0, 0.0, 6.0]));
    // Within the sector: one line changes.
    assert_eq!(ed.move_to(&crate_01, [10.0, 0.0, 9.0])?, crate_01);
    ed.set_yaw(&crate_01, 90.0)?;
    // Across sectors: the table moves to the source of (-1, 0).
    let crate_02 = PlacementRef {
        sector: (0, 0),
        name: "crate_02".to_owned(),
    };
    let moved = ed.move_to(&crate_02, [-10.0, 0.0, 18.0])?;
    assert_eq!(moved.sector, (-1, 0));
    assert_ne!(moved.name, "crate_02", "(-1, 0) already has a crate_02");
    // Added and removed.
    let added = ed.add(
        "barrel",
        "meshes/crate.obj",
        "materials/crate.material.toml",
        [-20.0, 0.0, -20.0],
    )?;
    assert_eq!(added.sector, (-1, -1));
    ed.remove(&PlacementRef {
        sector: (0, 0),
        name: "crate_03".to_owned(),
    })?;
    assert!(
        ed.move_to(&crate_01, [500.0, 0.0, 0.0]).is_err(),
        "no sector out there"
    );
    assert_eq!(ed.placements().len(), 10, "one added, one removed");
    let written = ed.save()?;
    assert_eq!(written.len(), 3, "three sources changed: {written:?}");
    assert!(ed.dirty().is_empty());

    // The edited source keeps everything it did not touch.
    let edited = std::fs::read_to_string(content.join("sectors/0_0.sector.toml"))?;
    assert!(edited.starts_with("# Generated toy sector (0, 0): flat ground and a few crates."));
    assert!(edited.contains("[bake.dusk]"));
    assert!(edited.contains("position = [10.0, 0.0, 9.0]"));
    assert!(edited.contains("yaw = 90.0"));
    assert!(!edited.contains("[placement.crate_02]") && !edited.contains("[placement.crate_03]"));
    let untouched: Vec<&str> = source_00
        .lines()
        .take_while(|l| !l.starts_with("[placement"))
        .collect();
    assert!(
        edited.starts_with(&untouched.join("\n")),
        "everything above the placements is unchanged"
    );

    // A reopened editor reads the same world.
    let again = WorldEditor::open(&content)?;
    assert_eq!(
        again.placement(&crate_01).map(|p| (p.position, p.yaw)),
        Some(([10.0, 0.0, 9.0], 90.0))
    );

    // The recook: new placements, same handshake hash.
    let after = recook(&content, &store)?;
    assert_eq!(
        after.gameplay, before.gameplay,
        "placements are presentation content"
    );
    assert_ne!(after.presentation, before.presentation);
    let in_00 = placed(&store, 0, 0)?;
    assert!(in_00.iter().any(|p| near(*p, [10.0, 0.0, 9.0])), "{in_00:?}");
    assert!(
        !in_00.iter().any(|p| near(*p, [20.0, 0.0, 18.0])),
        "crate_02 left (0, 0)"
    );
    assert!(
        !in_00.iter().any(|p| near(*p, [12.0, 0.0, 26.0])),
        "crate_03 is gone"
    );
    assert!(
        placed(&store, -1, 0)?
            .iter()
            .any(|p| near(*p, [-10.0, 0.0, 18.0]))
    );
    assert!(
        placed(&store, -1, -1)?
            .iter()
            .any(|p| near(*p, [-20.0, 0.0, -20.0]))
    );
    Ok(())
}

#[test]
fn a_reload_streams_the_recooked_world_into_the_live_renderer() -> TestResult {
    let dir = TempDir::new("reload")?;
    let content = dir.0.join("content");
    copy_dir(&toy_content(), &content)?;
    let store = dir.0.join("cooked");
    recook(&content, &store)?;
    let ctx = noop()?;
    let gpu = Gpu {
        device: &ctx.device,
        queue: &ctx.queue,
        capabilities: ctx.capabilities,
    };
    let mut config = RendererConfig::new(64, 64, wgpu::TextureFormat::Rgba8Unorm);
    config.shadow_resolution = 256;
    let mut r = Renderer::new(&ctx.device, &ctx.queue, false, config)?;
    let empty = r.mesh_space();
    let streaming = StreamingConfig {
        load_radius: 80.0,
        unload_radius: 100.0,
        lookahead: 0.0,
        ..StreamingConfig::default()
    };
    let mut world = open_world(&store, streaming, 1, Arc::new(ManualClock::new()))?;
    world.streamer.preload_materials(&mut r, &gpu)?;
    let first = settle(&mut world, &mut r, &gpu)?;

    let mut ed = WorldEditor::open(&content)?;
    ed.add(
        "barrel",
        "meshes/crate.obj",
        "materials/crate.material.toml",
        [4.0, 0.0, 4.0],
    )?;
    ed.save()?;
    recook(&content, &store)?;
    let compiled = reload(&mut world, &mut r, &gpu, &store, streaming, 1)?;
    assert_eq!(compiled, 2, "the ground and crate materials");
    let second = settle(&mut world, &mut r, &gpu)?;
    assert_eq!(second.instances, first.instances + 1, "the added barrel is drawn");
    assert_eq!(world.streamer.take_errors(), Vec::new());
    // Releasing the world returns every range the reloads used.
    let _ = world.streamer.release_all(&mut r, &gpu);
    assert_eq!(r.mesh_space(), empty);
    Ok(())
}

#[test]
fn a_view_ray_picks_the_nearest_placement_it_hits() -> TestResult {
    let ed = WorldEditor::open(&toy_content())?;
    // From above and behind crate_01 (8, 0, 6; scale 2) looking at it.
    let eye = Vec3::new(8.0, 4.0, -4.0);
    let at = Vec3::new(8.0, 1.0, 6.0);
    let hit = ed.pick(eye, at - eye, 0.5).ok_or("nothing picked")?;
    assert_eq!(
        hit,
        PlacementRef {
            sector: (0, 0),
            name: "crate_01".to_owned()
        }
    );
    // Looking up at the sky picks nothing.
    assert_eq!(ed.pick(eye, Vec3::Y, 0.5), None);
    // A ray through two crates picks the nearer one.
    let far = ed.placement(&PlacementRef {
        sector: (0, 0),
        name: "crate_02".to_owned(),
    });
    let far = Vec3::from_array(far.ok_or("crate_02")?.position) + Vec3::Y;
    let from = Vec3::new(8.0, 1.0, 6.0) - (far - Vec3::new(8.0, 1.0, 6.0)).normalize() * 5.0;
    assert_eq!(
        ed.pick(from, far - from, 0.5).map(|p| p.name),
        Some("crate_01".to_owned())
    );
    Ok(())
}

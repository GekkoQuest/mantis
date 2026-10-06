//! Material editing on a copy of the toy package's content: a parameter edit compiles as
//! the cook compiles it and replaces the live material in place; a broken edit reports
//! its line and leaves the live material alone; saving keeps the rest of the source.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use glam::Vec3;
use mantis_client::content_store::ContentStore;
use mantis_client::time::ManualClock;
use mantis_client::world_stream::{Gpu, WorldStats};
use mantis_client::world_view::StreamedWorld;
use mantis_editor::materials::{MaterialEditError, MaterialEditor};
use mantis_editor::world::{open_world, recook};
use mantis_formats::bundle::Domain;
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
fn a_material_edit_recompiles_and_replaces_the_live_material() -> TestResult {
    let dir = TempDir::new("material")?;
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
    let streaming = StreamingConfig {
        load_radius: 80.0,
        unload_radius: 100.0,
        lookahead: 0.0,
        ..StreamingConfig::default()
    };
    let mut world = open_world(&store, streaming, 1, Arc::new(ManualClock::new()))?;
    world.streamer.preload_materials(&mut r, &gpu)?;
    let before = settle(&mut world, &mut r, &gpu)?;
    // The live ground material, found by its cooked name.
    let key = std::fs::read(store.join("keys/dev.pub"))?;
    let bundle = ContentStore::open(&store).bundle(Domain::Presentation, &key)?;
    let hash = bundle.get("materials/ground.mat").ok_or("ground material")?.hash;
    let ground = world
        .streamer
        .material(&hash)
        .ok_or("ground material is loaded")?;
    let loaded = r.materials().len();

    let mut ed = MaterialEditor::open(&content, "materials/ground.material.toml")?;
    ed.set_color("tint", [0.9, 0.2, 0.1, 1.0])?;
    ed.apply(&mut r, &gpu, ground)?;
    assert_eq!(r.materials().len(), loaded, "replaced in place, nothing added");
    let after = settle(&mut world, &mut r, &gpu)?;
    assert_eq!(
        after.instances, before.instances,
        "every ground instance still draws"
    );

    // A broken edit: the error names the source line; the live material stays.
    let mut broken = ed.clone();
    broken.set(
        "node.tinted",
        "op",
        &mantis_core::module::toml::Value::Str("blend".to_owned()),
    )?;
    let line = broken
        .text()
        .lines()
        .position(|l| l.contains("op = \"blend\""))
        .map(|i| i + 1)
        .ok_or("edited line")?;
    match broken.apply(&mut r, &gpu, ground) {
        Err(MaterialEditError::Compile(errors)) => {
            assert!(
                errors
                    .iter()
                    .any(|e| e.file == "materials/ground.material.toml" && e.line == line),
                "{errors:?}"
            );
        }
        other => return Err(format!("expected a compile error, got {other:?}").into()),
    }
    assert_eq!(r.materials().len(), loaded);

    // Saving writes the edit and keeps the comment.
    ed.save()?;
    let saved = std::fs::read_to_string(content.join("materials/ground.material.toml"))?;
    assert!(saved.starts_with("# The ground: a gray checker"));
    assert!(saved.contains("default = [0.9, 0.2, 0.1, 1.0]"));
    // The recook gives the material a new hash; the handshake hash is unchanged.
    let first = recook(&content, &store)?;
    let bundle = ContentStore::open(&store)
        .bundle(Domain::Presentation, &std::fs::read(store.join("keys/dev.pub"))?)?;
    assert_ne!(bundle.get("materials/ground.mat").map(|e| e.hash), Some(hash));
    assert_eq!(recook(&content, &store)?.gameplay, first.gameplay);
    Ok(())
}

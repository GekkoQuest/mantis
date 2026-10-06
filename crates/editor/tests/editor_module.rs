//! The editor module, headless: a world view on the no-op backend streams a recooked
//! copy of the toy content, and the editor's tabs drive every tool through the same
//! intents its buttons emit: the inspector shows the simulation and render world, world
//! edits recook and stream back in, a material tint replaces the live material, graphs
//! list their markers, and a layout edit hot-reloads into the preview.

#![allow(clippy::too_many_lines)]

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use glam::Vec3;
use mantis_client::inspect::{InspectSlot, SimInspect};
use mantis_client::render_world::{PresentationConfig, render_world_channel};
use mantis_client::time::ManualClock;
use mantis_client::world_stream::{Gpu, WorldStats};
use mantis_client::world_view::{FrameTimings, WorldView, WorldViewConfig};
use mantis_editor::module::{Editor, EditorConfig, Tab};
use mantis_editor::world::{open_world, recook};
use mantis_render::gpu_test::noop;
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

fn settle(view: &mut WorldView, gpu: &Gpu<'_>) -> Result<WorldStats, Error> {
    let clock = ManualClock::new();
    let mut last = WorldStats::default();
    for _ in 0..4000 {
        let (world, renderer) = view.world_and_renderer_mut();
        let world = world.ok_or("no world")?;
        last = world.streamer.update(
            renderer,
            gpu,
            &clock,
            Duration::from_millis(5),
            (Vec3::ZERO, Vec3::ZERO, Vec3::Z),
        );
        world.last = last;
        if last.stream.in_flight == 0 && !last.backlog && last.stream.requested == 0 && last.resident == 4 {
            return Ok(last);
        }
        std::thread::sleep(Duration::from_millis(1));
    }
    Err(format!("streaming did not settle: {last:?}").into())
}

fn fonts() -> Result<mantis_ui::FontLibrary, Error> {
    let mut fonts = mantis_ui::FontLibrary::new();
    let id = fonts
        .add_font(mantis_ui::test_font::latin())
        .map_err(|e| format!("{e:?}"))?;
    fonts.define_stack("ui", &[id]).map_err(|e| format!("{e:?}"))?;
    Ok(fonts)
}

#[test]
fn every_tab_drives_its_tool_through_intents() -> TestResult {
    let dir = TempDir::new("module")?;
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
    let mut config = WorldViewConfig::new(320, 180, wgpu::TextureFormat::Rgba8Unorm);
    config.renderer.shadow_resolution = 256;
    let mut view = WorldView::new(&ctx.device, &ctx.queue, false, config)?;
    let streaming = StreamingConfig {
        load_radius: 80.0,
        unload_radius: 100.0,
        lookahead: 0.0,
        ..StreamingConfig::default()
    };
    let world = open_world(&store, streaming, 1, Arc::new(ManualClock::new()))?;
    view.attach_world(&gpu, world)?;
    let before = settle(&mut view, &gpu)?;

    let sim = InspectSlot::new();
    sim.publish(SimInspect {
        tick: 42,
        ..SimInspect::default()
    });
    let mut editor = Editor::new(EditorConfig {
        content: content.clone(),
        store: store.clone(),
        streaming,
        workers: 1,
        fonts: fonts()?,
        sim: Some(sim),
        ops: None,
        material: Some("materials/ground.material.toml".to_owned()),
        layout: Some((
            "ui/town/hud.layout".to_owned(),
            Some("ui/town/hud.theme".to_owned()),
        )),
    })?;
    let (_publisher, reader) = render_world_channel(16, PresentationConfig::default());
    let timings = FrameTimings::default();
    let step =
        |editor: &mut Editor, view: &mut WorldView| editor.step(view, &gpu, reader.current(), &timings);

    // Inspect.
    step(&mut editor, &mut view);
    assert_eq!(editor.tab(), Tab::Inspect);
    let lines = editor.lines();
    assert!(lines.iter().any(|l| l.starts_with("sim tick 42")), "{lines:?}");
    assert!(
        lines.iter().any(|l| l.starts_with("world: 4 sectors")),
        "{lines:?}"
    );

    // World: select, nudge, add, then save and recook into the live renderer.
    editor.send("editor.tab", Some("world"));
    editor.send("editor.world.select", Some("1"));
    step(&mut editor, &mut view);
    assert_eq!(editor.tab(), Tab::World);
    let picked = editor.lines().get(1).cloned().unwrap_or_default();
    assert!(picked.starts_with("selected: "), "{picked}");
    editor.send("editor.world.nudge", Some("0.5,0"));
    editor.send("editor.world.add", None);
    step(&mut editor, &mut view);
    assert!(editor.status().starts_with("added "), "{}", editor.status());
    editor.send("editor.world.commit", None);
    step(&mut editor, &mut view);
    assert!(
        editor.status().starts_with("saved 1 sources, cooked"),
        "{}",
        editor.status()
    );
    let after = settle(&mut view, &gpu)?;
    assert_eq!(after.instances, before.instances + 1, "the copy streams in");
    assert!(editor.world().dirty().is_empty());

    // Material: a tint replaces the live ground material.
    editor.send("editor.tab", Some("material"));
    editor.send("editor.material.tint", Some("0.3,0.5,0.8"));
    step(&mut editor, &mut view);
    assert_eq!(editor.status(), "material replaced live");
    assert!(editor.lines().first().is_some_and(|l| l.ends_with("(edited)")));

    // Graphs: the toy's pulse with both markers unbound.
    editor.send("editor.tab", Some("graphs"));
    editor.send("editor.graphs.check", None);
    step(&mut editor, &mut view);
    let lines = editor.lines();
    assert!(
        lines.iter().any(|l| l.starts_with("toy.ability.pulse")),
        "{lines:?}"
    );
    assert_eq!(
        lines.iter().filter(|l| l.contains("(no presentation)")).count(),
        2,
        "{lines:?}"
    );

    // UI: an edit hot-reloads into the preview.
    editor.send("editor.tab", Some("ui"));
    step(&mut editor, &mut view);
    editor.set_layout_text("town_party_title", "Squad")?;
    editor.send("editor.ui.apply", None);
    step(&mut editor, &mut view);
    assert!(editor.status().starts_with("reloaded: "), "{}", editor.status());
    let preview = editor.preview().ok_or("a preview")?;
    assert_eq!(preview.text_of("town_party_title"), Some("Squad"));

    // Unknown intents report, never panic.
    editor.send("editor.bogus", None);
    step(&mut editor, &mut view);
    assert!(editor.status().contains("unknown intent"));
    Ok(())
}

#[test]
fn f10_toggles_the_overlay_and_it_draws_its_screen() -> TestResult {
    use mantis_client::input::device::{ButtonSource, KeyCode, RawInput};
    use mantis_client::threads::render_thread::PlatformEvent;
    use mantis_client::world_view::SinkOverlay;
    let dir = TempDir::new("overlay")?;
    let content = dir.0.join("content");
    copy_dir(&toy_content(), &content)?;
    let mut editor = Editor::new(EditorConfig {
        content,
        store: dir.0.join("cooked"),
        streaming: StreamingConfig::default(),
        workers: 1,
        fonts: fonts()?,
        sim: None,
        ops: None,
        material: None,
        layout: None,
    })?;
    let ctx = noop()?;
    let mut config = WorldViewConfig::new(320, 180, wgpu::TextureFormat::Rgba8Unorm);
    config.renderer.shadow_resolution = 256;
    let mut view = WorldView::new(&ctx.device, &ctx.queue, false, config)?;
    let f10 = PlatformEvent::Input(RawInput::Button {
        source: ButtonSource::Key(KeyCode::F10),
        pressed: true,
    });
    assert!(editor.visible());
    let mut drew = false;
    let errors = mantis_render::gpu_test::validation_errors(&ctx.device, || {
        drew = editor.draw(view.renderer_mut(), &ctx.device, &ctx.queue, [320.0, 180.0]);
    });
    assert_eq!(errors, None);
    assert!(drew, "a shown editor sets the UI pass");
    assert!(editor.ui_event(&f10), "F10 is the editor's");
    assert!(!editor.visible());
    assert!(!editor.draw(view.renderer_mut(), &ctx.device, &ctx.queue, [320.0, 180.0]));
    // Hidden, it takes nothing else.
    let w = PlatformEvent::Input(RawInput::Button {
        source: ButtonSource::Key(KeyCode::W),
        pressed: true,
    });
    assert!(!editor.ui_event(&w));
    Ok(())
}

/// A replay that counts the ticks it was asked for.
struct Counting(u32);

impl mantis_editor::module::ReplayTab for Counting {
    fn step_ticks(&mut self, ticks: u32) -> Result<String, String> {
        if self.0 + ticks > 40 {
            return Err(format!("state hash diverged at tick {}", self.0 + 1));
        }
        self.0 += ticks;
        Ok(format!("replayed {ticks} ticks"))
    }
    fn restart(&mut self) {
        self.0 = 0;
    }
    fn lines(&self) -> Vec<String> {
        vec![format!("tick {}", self.0)]
    }
}

#[test]
fn the_replay_tab_steps_and_reports_divergence() -> TestResult {
    let dir = TempDir::new("replay-tab")?;
    let content = dir.0.join("content");
    copy_dir(&toy_content(), &content)?;
    let mut editor = Editor::new(EditorConfig {
        content,
        store: dir.0.join("cooked"),
        streaming: StreamingConfig::default(),
        workers: 1,
        fonts: fonts()?,
        sim: None,
        ops: None,
        material: None,
        layout: None,
    })?;
    let ctx = noop()?;
    let gpu = Gpu {
        device: &ctx.device,
        queue: &ctx.queue,
        capabilities: ctx.capabilities,
    };
    let mut view = WorldView::new(
        &ctx.device,
        &ctx.queue,
        false,
        WorldViewConfig::new(64, 64, wgpu::TextureFormat::Rgba8Unorm),
    )?;
    let (_publisher, reader) = render_world_channel(4, PresentationConfig::default());
    let timings = FrameTimings::default();
    editor.send("editor.tab", Some("replay"));
    editor.send("editor.replay.step", Some("1"));
    editor.step(&mut view, &gpu, reader.current(), &timings);
    assert_eq!(editor.tab(), Tab::Replay);
    assert!(editor.status().contains("no recording loaded"));
    editor.set_replay(Some(Box::new(Counting(0))));
    editor.send("editor.replay.step", Some("30"));
    editor.step(&mut view, &gpu, reader.current(), &timings);
    assert_eq!(editor.lines(), ["tick 30"]);
    editor.send("editor.replay.step", Some("30"));
    editor.step(&mut view, &gpu, reader.current(), &timings);
    assert!(
        editor.status().contains("diverged at tick 31"),
        "{}",
        editor.status()
    );
    editor.send("editor.replay.restart", None);
    editor.step(&mut view, &gpu, reader.current(), &timings);
    assert_eq!(editor.lines(), ["tick 0"]);
    Ok(())
}

#[test]
fn a_replay_viewer_over_the_core_motion_is_a_replay_tab() {
    use mantis_client::core_api::{CoreMotion, FlatGround, MotionState};
    use mantis_client::sim::ClientSimParts;
    use mantis_editor::replay::{ReplayCapture, ReplayViewer, ReplayWiring};
    type Factory = fn(ReplayWiring<MotionState>) -> ClientSimParts<CoreMotion<FlatGround>, ReplayCapture>;
    fn is_tab<T: mantis_editor::module::ReplayTab>() {}
    is_tab::<ReplayViewer<CoreMotion<FlatGround>, Factory>>();
}

//! The editor module: every tool of this crate behind one screen, drawn over the client's
//! world view ([`mantis_client::world_view::SinkOverlay`]) when the client is started
//! with the editor (`toy-client --editor`). `F10` shows and hides it; while shown it
//! takes pointer and keyboard input first.
//!
//! Tabs:
//! - **Inspect**: the simulation (from the [`InspectSlot`] the simulation thread fills),
//!   streaming and frame-stage timings, the render world's entities, and, when an Ops
//!   dashboard is configured, the server cell's summary and system timings (read-only,
//!   polled off the render thread).
//! - **World**: the selected placement; step through placements, nudge, turn, add,
//!   remove; "Save and recook" writes the sources, cooks, and streams the new world into
//!   the live renderer.
//! - **Graphs**: every gameplay graph with the bindings on each marker; check and save.
//! - **Material**: the selected material's tint; apply compiles and replaces it live.
//! - **UI**: the selected content layout; apply hot-reloads it into a preview shown in
//!   place of the editor (`F11` toggles the preview).
//! - **Replay**: a client recording (MCRC) stepped tick by tick by a [`ReplayViewer`],
//!   every tick verified against the recording, with the replayed simulation inspected.
//!
//! [`Editor::step`] does one frame's work on explicit parts, so every tool is driven
//! headless in tests through the same intents the screen's buttons emit.

use std::path::PathBuf;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use mantis_client::content_store::ContentStore;
use mantis_client::core_api::{AvatarKinematics, MotionStep};
use mantis_client::input::device::{ButtonSource, KeyCode, RawInput};
use mantis_client::inspect::{InspectSlot, render_world_rows};
use mantis_client::recording::RecordState;
use mantis_client::render_world::RenderWorld;
use mantis_client::sim::ClientSimParts;
use mantis_client::threads::render_thread::{FrameContext, PlatformEvent};
use mantis_client::ui_layer::UiLayer;
use mantis_client::world_stream::Gpu;
use mantis_client::world_view::{FrameTimings, SinkOverlay, WorldView};
use mantis_formats::bundle::Domain;
use mantis_render::renderer::Renderer;
use mantis_render::streaming::StreamingConfig;
use mantis_ui::{FontLibrary, ListItem, Ui, Value};

use crate::graphs::GraphBoard;
use crate::materials::MaterialEditor;
use crate::ops::OpsInspector;
use crate::replay::{ReplayCapture, ReplayViewer, ReplayWiring};
use crate::ui_edit::{Attr, LayoutEditor};
use crate::world::{WorldEditor, recook, reload};

/// The editor screen.
pub const LAYOUT: &str = r#"// The editor module's screen.
panel id=editor_root style=editor_panel {
  row id=editor_tabs gap=4 {
    button id=editor_tab_inspect text="Inspect" intent="editor.tab" payload="inspect"
    button id=editor_tab_world text="World" intent="editor.tab" payload="world"
    button id=editor_tab_graphs text="Graphs" intent="editor.tab" payload="graphs"
    button id=editor_tab_material text="Material" intent="editor.tab" payload="material"
    button id=editor_tab_ui text="UI" intent="editor.tab" payload="ui"
    button id=editor_tab_replay text="Replay" intent="editor.tab" payload="replay"
  }
  text id=editor_status bind="editor.status"
  list id=editor_lines bind="editor.lines" height=grow {
    text bind="item.line"
  }
  row id=editor_world visible="editor.on_world" gap=4 {
    button id=editor_prev text="<" intent="editor.world.select" payload="-1"
    button id=editor_next text=">" intent="editor.world.select" payload="1"
    button id=editor_xm text="x-" intent="editor.world.nudge" payload="-1,0"
    button id=editor_xp text="x+" intent="editor.world.nudge" payload="1,0"
    button id=editor_zm text="z-" intent="editor.world.nudge" payload="0,-1"
    button id=editor_zp text="z+" intent="editor.world.nudge" payload="0,1"
    button id=editor_turn text="Turn" intent="editor.world.turn" payload="15"
    button id=editor_add text="Add" intent="editor.world.add"
    button id=editor_remove text="Remove" intent="editor.world.remove"
    button id=editor_commit text="Save and recook" intent="editor.world.commit"
  }
  row id=editor_graph_buttons visible="editor.on_graphs" gap=4 {
    button id=editor_check text="Check" intent="editor.graphs.check"
    button id=editor_graph_save text="Save" intent="editor.graphs.save"
  }
  row id=editor_material_buttons visible="editor.on_material" gap=4 {
    button id=editor_warm text="Warm" intent="editor.material.tint" payload="0.8,0.45,0.3"
    button id=editor_cool text="Cool" intent="editor.material.tint" payload="0.3,0.5,0.8"
    button id=editor_green text="Green" intent="editor.material.tint" payload="0.45,0.75,0.4"
    button id=editor_material_save text="Save" intent="editor.material.save"
  }
  row id=editor_replay_buttons visible="editor.on_replay" gap=4 {
    button id=editor_replay_step text="Step" intent="editor.replay.step" payload="1"
    button id=editor_replay_play text="Step 30" intent="editor.replay.step" payload="30"
    button id=editor_replay_restart text="Restart" intent="editor.replay.restart"
  }
  row id=editor_ui_buttons visible="editor.on_ui" gap=4 {
    button id=editor_ui_apply text="Apply" intent="editor.ui.apply"
    button id=editor_ui_preview text="Preview" intent="editor.ui.preview"
    button id=editor_ui_save text="Save" intent="editor.ui.save"
  }
}
"#;

/// The editor theme.
pub const THEME: &str = "theme {
  style editor_panel { background = #11151ce8 padding = 8 gap = 6 width = 520 height = grow }
  style text { font = \"ui\" size = 14 color = #e6e6e6 }
  style button { font = \"ui\" size = 14 background = #2a3140 padding_x = 8 padding_y = 4 radius = 3 }
  style button:hover { background = #3a4458 }
}
";

/// Inspector lines listing render-world entities.
const ENTITY_LINES: usize = 8;

/// Where the editor works.
pub struct EditorConfig {
    /// The package's `content/` directory.
    pub content: PathBuf,
    /// Its cooked store (the client's world).
    pub store: PathBuf,
    /// Streaming radii of the client's world.
    pub streaming: StreamingConfig,
    /// Streaming workers.
    pub workers: usize,
    /// Fonts for the editor's screen (a `ui` stack).
    pub fonts: FontLibrary,
    /// The simulation's inspect slot, when the client publishes one.
    pub sim: Option<InspectSlot>,
    /// The Ops dashboard and the cell to inspect, when configured.
    pub ops: Option<(OpsInspector, u64)>,
    /// The material the Material tab edits (a source path).
    pub material: Option<String>,
    /// The layout (and theme) the UI tab edits (source paths).
    pub layout: Option<(String, Option<String>)>,
}

/// Which tab is shown.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Tab {
    /// The live inspector.
    Inspect,
    /// Placements.
    World,
    /// Gameplay and presentation graphs.
    Graphs,
    /// The selected material.
    Material,
    /// The selected layout.
    Ui,
    /// A client recording, replayed tick by tick with the replayed state inspected.
    Replay,
}

/// A replay the Replay tab steps (any [`ReplayViewer`]).
pub trait ReplayTab: Send {
    /// Replays up to `ticks` more ticks, verifying each; a summary of what happened.
    ///
    /// # Errors
    /// The first divergence, as text (the viewer shows the diverging tick's state).
    fn step_ticks(&mut self, ticks: u32) -> Result<String, String>;
    /// Starts over from the first record.
    fn restart(&mut self);
    /// The replayed state, as lines.
    fn lines(&self) -> Vec<String>;
}

impl<M, F> ReplayTab for ReplayViewer<M, F>
where
    M: MotionStep,
    M::State: RecordState + AvatarKinematics,
    F: FnMut(ReplayWiring<M::State>) -> ClientSimParts<M, ReplayCapture> + Send,
    ReplayViewer<M, F>: Send,
{
    fn step_ticks(&mut self, ticks: u32) -> Result<String, String> {
        let mut done = 0;
        for _ in 0..ticks {
            match self.step().map_err(|e| e.to_string())? {
                Some(_) => done += 1,
                None => break,
            }
        }
        Ok(if self.at_end() {
            format!("replayed {done} ticks; at the end, every tick matched")
        } else {
            format!("replayed {done} ticks, every one matched")
        })
    }

    fn restart(&mut self) {
        ReplayViewer::restart(self);
    }

    fn lines(&self) -> Vec<String> {
        self.inspect().to_string().lines().map(str::to_owned).collect()
    }
}

/// Lines the Ops poller left for the render thread.
type OpsLines = Arc<Mutex<Vec<String>>>;

/// The editor.
pub struct Editor {
    content: PathBuf,
    store: PathBuf,
    streaming: StreamingConfig,
    workers: usize,
    ui: UiLayer,
    visible: bool,
    tab: Tab,
    world: WorldEditor,
    selected: usize,
    graphs: Option<GraphBoard>,
    material: Option<MaterialEditor>,
    layout: Option<LayoutEditor>,
    preview: Option<Ui>,
    show_preview: bool,
    sim: Option<InspectSlot>,
    ops_lines: Option<OpsLines>,
    replay: Option<Box<dyn ReplayTab>>,
    status: String,
    intents: Vec<(String, Option<String>)>,
    frame: u64,
}

impl core::fmt::Debug for Editor {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Editor")
            .field("tab", &self.tab)
            .field("visible", &self.visible)
            .finish_non_exhaustive()
    }
}

fn floats(payload: Option<&str>) -> Vec<f32> {
    payload
        .unwrap_or("")
        .split(',')
        .filter_map(|p| p.trim().parse().ok())
        .collect()
}

/// Starts the Ops poller thread: one read of the cell and its systems every second.
fn poll_ops(ops: OpsInspector, cell: u64) -> OpsLines {
    let lines: OpsLines = Arc::new(Mutex::new(vec!["server: waiting for the dashboard".to_owned()]));
    let out = Arc::downgrade(&lines);
    let _ = std::thread::Builder::new()
        .name("mantis-editor-ops".into())
        .spawn(move || {
            while let Some(out) = out.upgrade() {
                let mut text = Vec::new();
                match ops.cell(cell) {
                    Ok(c) => text.push(format!(
                        "server cell {}: tick {}, {} sessions, {} entities, hash {:016x}",
                        c.cell, c.tick, c.sessions, c.entities, c.state_hash
                    )),
                    Err(e) => text.push(format!("server: {e}")),
                }
                if let Ok(s) = ops.systems(cell) {
                    text.push(format!(
                        "inbox {}, encode p99 {} us, log lag {} ticks",
                        s.inbox_depth, s.encode_micros_p99, s.log_lag_ticks
                    ));
                    let mut slow = s.systems.clone();
                    slow.sort_by_key(|t| core::cmp::Reverse(t.micros_p99));
                    for t in slow.iter().take(5) {
                        text.push(format!("  {} ({}): p99 {} us", t.name, t.phase, t.micros_p99));
                    }
                }
                *out.lock().unwrap_or_else(PoisonError::into_inner) = text;
                drop(out);
                std::thread::sleep(Duration::from_secs(1));
            }
        });
    lines
}

fn ms(d: Duration) -> f64 {
    d.as_secs_f64() * 1000.0
}

impl Editor {
    /// Opens the editor on `config`'s content and store.
    ///
    /// # Errors
    /// A message for a source that does not open or parse.
    pub fn new(config: EditorConfig) -> Result<Self, String> {
        let world = WorldEditor::open(&config.content).map_err(|e| e.to_string())?;
        let graphs = match GraphBoard::open(&config.content) {
            Ok(b) => Some(b),
            Err(e) => {
                // A package whose graphs do not check still opens; the tab shows why.
                return Err(format!("graphs: {e}"));
            }
        };
        let material = config
            .material
            .as_deref()
            .map(|p| MaterialEditor::open(&config.content, p))
            .transpose()
            .map_err(|e| e.to_string())?;
        let layout = config
            .layout
            .as_ref()
            .map(|(l, t)| {
                LayoutEditor::open(
                    &config.content.join(l),
                    t.as_ref().map(|t| config.content.join(t)).as_deref(),
                )
            })
            .transpose()
            .map_err(|e| e.to_string())?;
        let ui = Ui::new(config.fonts, LAYOUT, Some(THEME)).map_err(|e| e.to_string())?;
        let mut editor = Self {
            content: config.content,
            store: config.store,
            streaming: config.streaming,
            workers: config.workers,
            ui: UiLayer::new(ui),
            visible: true,
            tab: Tab::Inspect,
            world,
            selected: 0,
            graphs,
            material,
            layout,
            preview: None,
            show_preview: false,
            sim: config.sim,
            ops_lines: config.ops.map(|(ops, cell)| poll_ops(ops, cell)),
            replay: None,
            status: "ready".to_owned(),
            intents: Vec::new(),
            frame: 0,
        };
        editor.publish_tab();
        Ok(editor)
    }

    /// The tab shown.
    pub fn tab(&self) -> Tab {
        self.tab
    }

    /// Whether the editor is shown.
    pub fn visible(&self) -> bool {
        self.visible
    }

    /// The status line (the outcome of the last action).
    pub fn status(&self) -> &str {
        &self.status
    }

    /// The lines the current tab shows.
    pub fn lines(&self) -> Vec<String> {
        let props = self.ui.ui().properties();
        let Some(id) = props.id("editor.lines") else {
            return Vec::new();
        };
        match props.get(id) {
            Some(Value::List(items)) => items
                .iter()
                .filter_map(|i| match i.field("line") {
                    Some(Value::Text(t)) => Some(t.clone()),
                    _ => None,
                })
                .collect(),
            _ => Vec::new(),
        }
    }

    /// The world editor (for tools and tests).
    pub fn world(&self) -> &WorldEditor {
        &self.world
    }

    /// The editor's own UI.
    pub fn ui(&self) -> &Ui {
        self.ui.ui()
    }

    /// Queues an intent as a button would (tests and scripted tools).
    pub fn send(&mut self, intent: &str, payload: Option<&str>) {
        self.intents.push((intent.to_owned(), payload.map(str::to_owned)));
    }

    fn set(&mut self, name: &str, value: Value) {
        let props = self.ui.ui_mut().properties_mut();
        let id = props.intern(name);
        props.set(id, value);
    }

    fn publish_tab(&mut self) {
        for (name, tab) in [
            ("editor.on_world", Tab::World),
            ("editor.on_graphs", Tab::Graphs),
            ("editor.on_material", Tab::Material),
            ("editor.on_ui", Tab::Ui),
            ("editor.on_replay", Tab::Replay),
        ] {
            self.set(name, Value::Bool(self.tab == tab));
        }
        let status = self.status.clone();
        self.set("editor.status", Value::Text(status));
    }

    fn set_lines(&mut self, lines: Vec<String>) {
        let items = lines
            .into_iter()
            .map(|l| ListItem::new().with("line", Value::Text(l)))
            .collect();
        self.set("editor.lines", Value::List(items));
    }

    fn inspect_lines(&self, view: &WorldView, world: &RenderWorld, timings: &FrameTimings) -> Vec<String> {
        let mut out = Vec::new();
        if let Some(sim) = &self.sim {
            let s = sim.read();
            out.push(format!(
                "sim tick {}: {} snapshots, {} moves, {} resets",
                s.tick, s.stats.snapshots, s.stats.moves_sent, s.stats.resets
            ));
            out.push(format!(
                "local {:?} at ({:.2}, {:.2}, {:.2}), {} remotes",
                s.local.map(|e| (e.index(), e.generation())),
                s.position.x,
                s.position.y,
                s.position.z,
                s.remotes
            ));
            out.push(match s.correction_p99 {
                Some(p) => format!(
                    "corrections: {} recorded, p99 {p:.3} m, max {:.3} m",
                    s.corrections, s.correction_max
                ),
                None => "corrections: none yet".to_owned(),
            });
        } else {
            out.push("sim: not published by this client".to_owned());
        }
        if let Some(w) = view.world_stats() {
            out.push(format!(
                "world: {} sectors, {} instances, hand-off {:.3} ms",
                w.resident,
                w.instances,
                ms(w.handoff)
            ));
        }
        out.push(format!(
            "frame {:.2} ms: stream {:.2}, update {:.2}, ui {:.2}, encode {:.2}",
            ms(timings.total),
            ms(timings.stream),
            ms(timings.update),
            ms(timings.ui),
            ms(timings.encode)
        ));
        let stats = view.renderer().stats();
        out.push(format!(
            "renderer: {} instances in {} batches",
            stats.instances, stats.batches
        ));
        let rows = render_world_rows(world);
        out.push(format!(
            "render world tick {}: {} entities",
            world.tick().0,
            rows.len()
        ));
        for r in rows.iter().take(ENTITY_LINES) {
            out.push(format!(
                "  {}:{}{} at ({:.1}, {:.1}, {:.1})",
                r.id.index(),
                r.id.generation(),
                if r.local { " (local)" } else { "" },
                r.position.x,
                r.position.y,
                r.position.z
            ));
        }
        if let Some(lines) = &self.ops_lines {
            out.extend(
                lines
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .iter()
                    .cloned(),
            );
        }
        out
    }

    fn world_lines(&self) -> Vec<String> {
        let all = self.world.placements();
        let mut out = vec![format!("{} placements", all.len())];
        if let Some(p) = all.get(self.selected) {
            out.push(format!("selected: {} in sector {:?}", p.at.name, p.at.sector));
            out.push(format!(
                "  at ({:.2}, {:.2}, {:.2}), yaw {:.1}, scale {:.2}",
                p.position[0], p.position[1], p.position[2], p.yaw, p.scale
            ));
            out.push(format!("  mesh {}, material {}", p.mesh, p.material));
        }
        let dirty = self.world.dirty();
        if !dirty.is_empty() {
            out.push(format!("{} sources edited, not saved", dirty.len()));
        }
        out
    }

    fn graph_lines(&self) -> Vec<String> {
        let Some(board) = &self.graphs else {
            return vec!["no graphs".to_owned()];
        };
        let mut out = Vec::new();
        for g in board.graphs() {
            out.push(format!("{} ({})", g.name, g.source));
            for n in &g.nodes {
                let next = if n.next.is_empty() {
                    "end".to_owned()
                } else {
                    n.next.join(", ")
                };
                out.push(format!("  [{}] {}: {} -> {next}", n.key, n.name, n.kind));
                for b in &n.bindings {
                    out.push(format!(
                        "      {} ({}): {}",
                        b.name,
                        b.filter,
                        b.actions.join("; ")
                    ));
                }
                if n.marker && n.bindings.is_empty() {
                    out.push("      (no presentation)".to_owned());
                }
            }
        }
        out
    }

    fn material_lines(&self) -> Vec<String> {
        match &self.material {
            Some(m) => {
                let mut out = vec![format!(
                    "{}{}",
                    m.path(),
                    if m.is_dirty() { " (edited)" } else { "" }
                )];
                out.extend(m.text().lines().take(24).map(str::to_owned));
                out
            }
            None => vec!["no material selected".to_owned()],
        }
    }

    fn ui_lines(&self) -> Vec<String> {
        match &self.layout {
            Some(l) => {
                let mut out = vec![if l.is_dirty() {
                    "layout (edited)".to_owned()
                } else {
                    "layout".to_owned()
                }];
                out.extend(l.layout().lines().take(24).map(str::to_owned));
                out
            }
            None => vec!["no layout selected".to_owned()],
        }
    }

    /// The live renderer material of the Material tab's source, by its cooked name.
    fn live_material(&self, view: &WorldView) -> Option<mantis_render::scene::MaterialId> {
        let m = self.material.as_ref()?;
        let name = format!("{}.mat", m.path().strip_suffix(".material.toml")?);
        let key = mantis_cook::package::dev_public_key(&self.store).ok()?;
        let bundle = ContentStore::open(&self.store)
            .bundle(Domain::Presentation, &key)
            .ok()?;
        let hash = bundle.get(&name)?.hash;
        view.world()?.streamer.material(&hash)
    }

    fn run_world(
        &mut self,
        intent: &str,
        payload: Option<&str>,
        view: &mut WorldView,
        gpu: &Gpu<'_>,
    ) -> Result<String, String> {
        let selected =
            || -> Result<crate::world::PlacementRef, String> { Err("no placement selected".to_owned()) };
        match intent {
            "editor.world.select" => {
                let n = self.world.placements().len().max(1);
                let step: i64 = payload.and_then(|p| p.parse().ok()).unwrap_or(1);
                let at = i64::try_from(self.selected).unwrap_or(0) + step;
                self.selected = usize::try_from(at.rem_euclid(i64::try_from(n).unwrap_or(1))).unwrap_or(0);
                Ok("selected".to_owned())
            }
            "editor.world.nudge" | "editor.world.turn" | "editor.world.remove" => {
                let Some(p) = self.world.placements().get(self.selected).cloned() else {
                    return selected().map(|_| String::new());
                };
                match intent {
                    "editor.world.nudge" => {
                        let d = floats(payload);
                        let to = [
                            p.position[0] + d.first().copied().unwrap_or(0.0),
                            p.position[1],
                            p.position[2] + d.get(1).copied().unwrap_or(0.0),
                        ];
                        let at = self.world.move_to(&p.at, to).map_err(|e| e.to_string())?;
                        self.selected = self
                            .world
                            .placements()
                            .iter()
                            .position(|q| q.at == at)
                            .unwrap_or(0);
                        Ok(format!("moved {} to {to:?}", at.name))
                    }
                    "editor.world.turn" => {
                        let by = floats(payload).first().copied().unwrap_or(15.0);
                        self.world
                            .set_yaw(&p.at, (p.yaw + by).rem_euclid(360.0))
                            .map_err(|e| e.to_string())?;
                        Ok(format!("turned {}", p.at.name))
                    }
                    _ => {
                        self.world.remove(&p.at).map_err(|e| e.to_string())?;
                        self.selected = 0;
                        Ok(format!("removed {}", p.at.name))
                    }
                }
            }
            "editor.world.add" => {
                let template = self.world.placements().get(self.selected).cloned();
                let Some(t) = template else {
                    return Err("select a placement to copy".to_owned());
                };
                let to = [t.position[0] + 2.0, t.position[1], t.position[2]];
                let at = self.world.duplicate(&t.at, to).map_err(|e| e.to_string())?;
                self.selected = self
                    .world
                    .placements()
                    .iter()
                    .position(|q| q.at == at)
                    .unwrap_or(0);
                Ok(format!("added {}", at.name))
            }
            "editor.world.commit" => {
                let written = self.world.save().map_err(|e| e.to_string())?;
                let cooked = recook(&self.content, &self.store)?;
                let (world, renderer) = view.world_and_renderer_mut();
                let world = world.ok_or("no world is streamed")?;
                let compiled = reload(world, renderer, gpu, &self.store, self.streaming, self.workers)?;
                Ok(format!(
                    "saved {} sources, cooked {} assets, {compiled} materials compiled",
                    written.len(),
                    cooked.assets
                ))
            }
            other => Err(format!("unknown intent `{other}`")),
        }
    }

    fn run(
        &mut self,
        intent: &str,
        payload: Option<&str>,
        view: &mut WorldView,
        gpu: &Gpu<'_>,
    ) -> Result<String, String> {
        match intent {
            "editor.tab" => {
                self.tab = match payload {
                    Some("world") => Tab::World,
                    Some("graphs") => Tab::Graphs,
                    Some("material") => Tab::Material,
                    Some("ui") => Tab::Ui,
                    Some("replay") => Tab::Replay,
                    _ => Tab::Inspect,
                };
                Ok(format!("{:?}", self.tab))
            }
            w if w.starts_with("editor.world.") => self.run_world(w, payload, view, gpu),
            "editor.graphs.check" => {
                let board = self.graphs.as_mut().ok_or("no graphs")?;
                board.check().map_err(|e| e.to_string())?;
                Ok(format!("{} graphs check", board.graphs().len()))
            }
            "editor.graphs.save" => {
                let board = self.graphs.as_mut().ok_or("no graphs")?;
                let written = board.save().map_err(|e| e.to_string())?;
                Ok(format!("saved {} sources", written.len()))
            }
            "editor.material.tint" => {
                let rgb = floats(payload);
                let [r, g, b] = rgb.as_slice() else {
                    return Err("a tint is `r,g,b`".to_owned());
                };
                let id = self.live_material(view);
                let m = self.material.as_mut().ok_or("no material selected")?;
                m.set_color("tint", [*r, *g, *b, 1.0])
                    .map_err(|e| e.to_string())?;
                if let Some(id) = id {
                    m.apply(view.renderer_mut(), gpu, id).map_err(|e| e.to_string())?;
                    Ok("material replaced live".to_owned())
                } else {
                    m.compile().map_err(|e| e.to_string())?;
                    Ok("compiled (not loaded in this view)".to_owned())
                }
            }
            "editor.material.save" => {
                self.material
                    .as_mut()
                    .ok_or("no material selected")?
                    .save()
                    .map_err(|e| e.to_string())?;
                Ok("material saved".to_owned())
            }
            "editor.ui.apply" | "editor.ui.preview" => {
                let layout = self.layout.as_ref().ok_or("no layout selected")?;
                if self.preview.is_none() {
                    let mut fonts = FontLibrary::new();
                    let id = fonts
                        .add_font(mantis_ui::test_font::latin())
                        .map_err(|e| format!("{e:?}"))?;
                    fonts.define_stack("ui", &[id]).map_err(|e| format!("{e:?}"))?;
                    self.preview =
                        Some(Ui::new(fonts, layout.layout(), layout.theme()).map_err(|e| e.to_string())?);
                }
                let preview = self.preview.as_mut().ok_or("no preview")?;
                let report = layout.apply(preview).map_err(|e| e.to_string())?;
                if intent == "editor.ui.preview" {
                    self.show_preview = !self.show_preview;
                }
                Ok(format!(
                    "reloaded: {} kept, {} added, {} removed",
                    report.kept.len(),
                    report.added.len(),
                    report.removed.len()
                ))
            }
            "editor.ui.save" => {
                self.layout
                    .as_mut()
                    .ok_or("no layout selected")?
                    .save()
                    .map_err(|e| e.to_string())?;
                Ok("layout saved".to_owned())
            }
            "editor.replay.step" => {
                let n = payload.and_then(|p| p.parse().ok()).unwrap_or(1);
                self.replay.as_mut().ok_or("no recording loaded")?.step_ticks(n)
            }
            "editor.replay.restart" => {
                self.replay.as_mut().ok_or("no recording loaded")?.restart();
                Ok("replay restarted".to_owned())
            }
            other => Err(format!("unknown intent `{other}`")),
        }
    }

    /// Loads a recording into the Replay tab (a [`ReplayViewer`] built with the client's
    /// motion model and tuning).
    pub fn set_replay(&mut self, replay: Option<Box<dyn ReplayTab>>) {
        self.replay = replay;
    }

    /// Edits the selected layout's element attribute (the UI tab's text edits).
    ///
    /// # Errors
    /// The layout edit's refusal, as text.
    pub fn set_layout_text(&mut self, id: &str, text: &str) -> Result<(), String> {
        self.layout
            .as_mut()
            .ok_or("no layout selected")?
            .set_attr(id, "text", &Attr::Text(text.to_owned()))
            .map_err(|e| e.to_string())
    }

    /// The UI preview, after an apply.
    pub fn preview(&self) -> Option<&Ui> {
        self.preview.as_ref()
    }

    /// One frame: runs queued intents (the screen's buttons and [`Editor::send`]) against
    /// the view, then refreshes the shown tab.
    pub fn step(&mut self, view: &mut WorldView, gpu: &Gpu<'_>, world: &RenderWorld, timings: &FrameTimings) {
        self.frame += 1;
        let mut collected = Vec::new();
        self.ui.drain_intents(|i| collected.push(i.clone()));
        for i in collected {
            if let Some(name) = self.ui.ui().intent_name(i.intent) {
                self.intents.push((name.to_owned(), i.payload));
            }
        }
        for (intent, payload) in core::mem::take(&mut self.intents) {
            self.status = match self.run(&intent, payload.as_deref(), view, gpu) {
                Ok(s) => s,
                Err(e) => format!("{intent}: {e}"),
            };
        }
        let lines = match self.tab {
            Tab::Inspect => self.inspect_lines(view, world, timings),
            Tab::World => self.world_lines(),
            Tab::Graphs => self.graph_lines(),
            Tab::Material => self.material_lines(),
            Tab::Ui => self.ui_lines(),
            Tab::Replay => self
                .replay
                .as_ref()
                .map_or_else(|| vec!["no recording loaded".to_owned()], |r| r.lines()),
        };
        self.set_lines(lines);
        self.publish_tab();
    }
}

impl SinkOverlay for Editor {
    fn ui_event(&mut self, event: &PlatformEvent) -> bool {
        if let PlatformEvent::Input(RawInput::Button {
            source: ButtonSource::Key(key),
            pressed: true,
        }) = event
        {
            match key {
                KeyCode::F10 => {
                    self.visible = !self.visible;
                    return true;
                }
                KeyCode::F11 => {
                    self.show_preview = !self.show_preview;
                    return true;
                }
                _ => {}
            }
        }
        self.visible && self.ui.handle(event)
    }

    fn update(&mut self, view: &mut WorldView, gpu: &Gpu<'_>, frame: &FrameContext<'_>, last: &FrameTimings) {
        self.step(view, gpu, frame.world, last);
    }

    fn draw(
        &mut self,
        renderer: &mut Renderer,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        viewport: [f32; 2],
    ) -> bool {
        if !self.visible {
            return false;
        }
        if self.show_preview
            && let Some(preview) = self.preview.as_mut()
        {
            let _ = preview.frame(viewport, 1.0);
            let (list, atlas) = preview.draw_parts();
            renderer.set_ui(device, queue, &list.quads, atlas);
            return true;
        }
        self.ui.draw(renderer, device, queue, viewport, 1.0);
        true
    }
}

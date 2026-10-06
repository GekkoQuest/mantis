//! The toy client.
//!
//! ```text
//! toy-client [--server ADDR] [--cert FILE] [--font FILE] [--token TEXT] [--world DIR] [--no-world]
//!            [--mods DIR]
//!            [--editor [--content DIR] [--ops ADDR --ops-cert FILE --ops-token-file FILE --ops-cell N]]
//! ```
//!
//! Connects to a toy server over QUIC (pinning the development certificate the server
//! wrote with `toy-server serve --cert-out FILE`), opens a window, and plays: W, A, S, D
//! move, Space jumps, Left Shift walks, the mouse looks. With `--font`, the package's
//! module screens (party and the other std modules) are drawn with that font; the
//! repository ships no font files. The cooked toy world (`--world`, default
//! `packages/toy/cooked`, written by `cargo run -p mantis-cook -- packages/toy`) streams
//! in around the camera, and its gameplay bundle hash is the content hash announced at
//! the handshake, the one a cooked server checks. `--no-world` draws the flat
//! placeholder ground and announces the uncooked manifest hash. `--editor` loads the
//! editor module over the world view (see `toy_client::editor`; `F10` shows and hides
//! it). With `--font`, client mods load from `--mods DIR` (one folder per mod; default:
//! the package's own `packages/toy/mods`) and run at the tier each cell permits; refused
//! or stopped mods are listed on screen and printed at start.

#![forbid(unsafe_code)]

use std::process::ExitCode;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::JoinHandle;
use std::time::Duration;

use mantis_client::platform::{MonotonicClock, PlatformConfig, PlatformError, RunningSession, run};
use mantis_client::threads::render_thread::RenderThread;
use mantis_client::threads::sim_thread::SimThread;
use mantis_client::time::HostClock;
use mantis_client::ui_layer::UiLayer;
use mantis_client::world_view::{SurfaceSink, WorldViewConfig};
use mantis_net::NetRuntime;
use mantis_net::quic::QuicClient;
use toy_client::ToyClient;

struct Args(Vec<String>);

impl Args {
    fn value(&self, flag: &str) -> Option<&str> {
        let at = self.0.iter().position(|a| a == flag)?;
        self.0.get(at + 1).map(String::as_str)
    }
}

/// Everything the window keeps alive: the render and sim threads and the network thread.
struct Running {
    render: RenderThread<SurfaceSink>,
    _sim: SimThread<mantis_client::sim::ClientSim<toy_client::ToyMotion, mantis_client::net::MoveOutbox>>,
    stop: Arc<AtomicBool>,
    net: Option<JoinHandle<()>>,
}

impl RunningSession for Running {
    fn is_finished(&self) -> bool {
        self.render.is_finished()
    }
}

impl Drop for Running {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(net) = self.net.take() {
            let _ = net.join();
        }
    }
}

fn fonts(font: Option<&str>) -> Result<Option<mantis_ui::FontLibrary>, String> {
    let Some(path) = font else { return Ok(None) };
    let bytes = std::fs::read(path).map_err(|e| format!("{path}: {e}"))?;
    let mut fonts = mantis_ui::FontLibrary::new();
    let id = fonts.add_font(bytes).map_err(|e| format!("{path}: {e:?}"))?;
    fonts.define_stack("ui", &[id]).map_err(|e| format!("{e:?}"))?;
    Ok(Some(fonts))
}

/// `--editor` and its options: content directory, store, and the Ops dashboard.
type EditorArgs = (
    std::path::PathBuf,
    std::path::PathBuf,
    Option<toy_client::editor::OpsTarget>,
);

fn editor_args(args: &Args, store: &str) -> Result<Option<EditorArgs>, String> {
    if !args.0.iter().any(|a| a == "--editor") {
        return Ok(None);
    }
    let content = args.value("--content").unwrap_or(toy_client::editor::CONTENT_DIR);
    let ops = match args.value("--ops") {
        None => None,
        Some(addr) => Some(toy_client::editor::OpsTarget {
            addr: addr
                .parse()
                .map_err(|_| format!("--ops: not an address: {addr}"))?,
            cert: args.value("--ops-cert").unwrap_or("ops-dev-cert.der").into(),
            token_file: args.value("--ops-token-file").unwrap_or("ops-token.txt").into(),
            cell: args
                .value("--ops-cell")
                .map_or(Ok(1), str::parse)
                .map_err(|_| "--ops-cell: not a number".to_owned())?,
        }),
    };
    Ok(Some((content.into(), store.into(), ops)))
}

/// A second font library for the editor's own screen (the module UI keeps the first).
fn fonts_again(font: Option<&str>, wanted: bool) -> Result<Option<mantis_ui::FontLibrary>, String> {
    if wanted { fonts(font) } else { Ok(None) }
}

/// Loads the editor module over the client's world view and publishes the simulation
/// to its inspector.
fn attach_editor<T: mantis_adapter_contract::Transport>(
    client: &mut ToyClient<T, SurfaceSink>,
    editor: &EditorArgs,
    fonts: Option<mantis_ui::FontLibrary>,
    clock: &Arc<dyn HostClock>,
) -> Result<(), PlatformError> {
    let slot = mantis_client::inspect::InspectSlot::new();
    client
        .sim
        .set_observer(Some(mantis_client::inspect::client_sim_observer(slot.clone())));
    let fonts = match fonts {
        Some(f) => f,
        None => toy_client::town::fixture_fonts().map_err(PlatformError::Start)?,
    };
    let config = toy_client::editor::config(&editor.0, &editor.1, fonts, Some(slot), editor.2.as_ref())
        .map_err(PlatformError::Start)?;
    let overlay = mantis_editor::module::Editor::new(config).map_err(PlatformError::Start)?;
    let sink = client.render.sink_mut();
    sink.set_clock(Arc::clone(clock));
    sink.set_overlay(Some(Box::new(overlay)));
    Ok(())
}

/// Draws the module screens with `fonts`, and loads and runs the client mods in
/// `mods_dir` beside them (mods draw UI, so they run only with a module UI).
fn attach_ui<T: mantis_adapter_contract::Transport>(
    client: &mut ToyClient<T, SurfaceSink>,
    fonts: mantis_ui::FontLibrary,
    mods_dir: &std::path::Path,
) -> Result<(), PlatformError> {
    for n in client
        .load_mods(mods_dir)
        .map_err(|e| PlatformError::Start(e.to_string()))?
    {
        println!("toy-client: mod {}: {}", n.module, n.text);
    }
    let Some((registry, link)) = client.take_modules() else {
        return Ok(());
    };
    let open: Vec<&str> = registry.screens().iter().map(|s| s.key).collect();
    let actions = core::mem::take(&mut client.module_actions);
    let mut layer = UiLayer::with_modules(fonts, registry, link, actions, &open)
        .map_err(|e| PlatformError::Start(e.to_string()))?;
    if let Some(mods) = client.take_mods() {
        layer
            .set_mods(mods)
            .map_err(|e| PlatformError::Start(format!("mod screens: {e}")))?;
    }
    client.render.sink_mut().set_ui(Some(layer));
    Ok(())
}

fn start(args: &Args) -> Result<(), String> {
    let server = args.value("--server").unwrap_or("127.0.0.1:7777");
    let addr = server
        .parse()
        .map_err(|_| format!("--server: not an address: {server}"))?;
    let cert_path = args.value("--cert").unwrap_or("toy-dev-cert.der");
    let cert = std::fs::read(cert_path).map_err(|e| format!("--cert {cert_path}: {e}"))?;
    let token = args.value("--token").unwrap_or("player").as_bytes().to_vec();
    let fonts = fonts(args.value("--font"))?;
    let no_world = args.0.iter().any(|a| a == "--no-world");
    let world = match (no_world, args.value("--world").unwrap_or("packages/toy/cooked")) {
        (true, _) => None,
        (false, dir) => {
            let opened = toy_client::world::open(std::path::Path::new(dir), None, 2).map_err(|e| {
                format!(
                    "--world {dir}: {e} (run `cargo run -p mantis-cook -- packages/toy`, or pass --no-world)"
                )
            })?;
            println!(
                "toy-client: world {dir}: {} sectors, content {}",
                opened.sectors, opened.content_hash
            );
            Some((opened.streamer, opened.content_hash))
        }
    };
    let content = world
        .as_ref()
        .map_or_else(toy_client::package::content, |(_, hash)| *hash);
    let mut editor = editor_args(args, args.value("--world").unwrap_or("packages/toy/cooked"))?;
    let mut editor_fonts = fonts_again(args.value("--font"), editor.is_some())?;
    let mods_dir = args
        .value("--mods")
        .map_or_else(toy_client::package::package_mods_dir, std::path::PathBuf::from);
    let runtime = NetRuntime::new(2).map_err(|e| format!("{e:?}"))?;
    let config = PlatformConfig {
        title: "toy".to_owned(),
        ..PlatformConfig::default()
    };
    run(config, move |target, events| {
        let clock: Arc<dyn HostClock> = Arc::new(MonotonicClock::new());
        let transport =
            QuicClient::connect(&runtime, addr, &cert).map_err(|e| PlatformError::Start(format!("{e:?}")))?;
        // The window's sink starts without a UI; the module screens join it once the
        // client has built its module registry.
        let sink = SurfaceSink::new(
            target,
            WorldViewConfig::new(1, 1, wgpu::TextureFormat::Rgba8Unorm),
            None,
        )
        .map_err(|e| PlatformError::Start(e.to_string()))?;
        let mut sink = sink;
        if let Some((streamer, _)) = world {
            sink.attach_world(toy_client::world::streamed(streamer, Arc::clone(&clock)))
                .map_err(|e| PlatformError::Start(format!("world materials: {e}")))?;
        }
        let mut client = ToyClient::new_for_content(transport, Arc::clone(&clock), sink, events, content)
            .map_err(|e| PlatformError::Start(e.to_string()))?;
        if let Some(editor) = editor.take() {
            attach_editor(&mut client, &editor, editor_fonts.take(), &clock)?;
        }
        if let Some(fonts) = fonts {
            attach_ui(&mut client, fonts, &mods_dir)?;
        }
        let hello_mods = core::mem::take(&mut client.hello_mods);
        let ToyClient {
            sim,
            render,
            mut net,
            mut module_net,
            ..
        } = client;
        net.start_with_modules(&token, &hello_mods);
        let stop = Arc::new(AtomicBool::new(false));
        let stop_net = Arc::clone(&stop);
        let net_thread = std::thread::Builder::new()
            .name("toy-net".into())
            .spawn(move || {
                // The runtime lives as long as the connection.
                let _runtime = runtime;
                while !stop_net.load(Ordering::Acquire) {
                    net.step();
                    module_net.pump(&mut net);
                    std::thread::sleep(Duration::from_millis(2));
                }
            })
            .map_err(|e| PlatformError::Start(e.to_string()))?;
        let sim = SimThread::spawn(sim, Arc::clone(&clock), Duration::from_millis(4))
            .map_err(|e| PlatformError::Start(e.to_string()))?;
        let render = RenderThread::spawn(render, None).map_err(|e| PlatformError::Start(e.to_string()))?;
        Ok(Running {
            render,
            _sim: sim,
            stop,
            net: Some(net_thread),
        })
    })
    .map_err(|e| format!("{e:?}"))
}

fn main() -> ExitCode {
    let args = Args(std::env::args().skip(1).collect());
    match start(&args) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("toy-client: {e}");
            ExitCode::FAILURE
        }
    }
}

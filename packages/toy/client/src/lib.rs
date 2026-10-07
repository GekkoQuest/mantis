//! toy-client: the toy package's native client (plan 18 step 6 proving package).
//!
//! It wires the engine's client host to the toy package: the package manifest (content
//! hash and movement tunables, parsed from the same `package.toml` the server reads, so
//! prediction runs the server's exact parameters), the control scheme, and a
//! [`ToyClient`] that drives a world session and a native protocol session over any
//! `Transport`. Production connects over QUIC to the gateway
//! ([`ToyClient::connect_trusted`]) and reconnects through it after a drop
//! ([`ToyClient::enable_reconnect`], [`gateway`]); tests drive the same code over the
//! simulated network.

#![forbid(unsafe_code)]

pub mod controls;
pub mod editor;
pub mod gateway;
pub mod modules;
pub mod package;
pub mod town;
pub mod trust;
pub mod world;

use std::sync::Arc;
use std::sync::mpsc::SyncSender;

use mantis_adapter_contract::Transport;
use mantis_client::core_api::{Angle16, CoreMotion, FlatGround, Motion, MotionState, Vec3};
use mantis_client::host::{
    WorldSessionConfig, WorldSessionParts, build_world_session, platform_event_channel,
};
use mantis_client::net::{MoveOutbox, NativeSession, NetConfig, move_channel};
use mantis_client::sim::ClientSim;
use mantis_client::snapshot::MarkerReceiver;
use mantis_client::threads::render_thread::RenderLoop;
use mantis_client::threads::render_thread::{FrameContext, FrameSink, PlatformEvent};
use mantis_client::threads::sim_thread::SimDriver;
use mantis_client::time::HostClock;

/// The client's motion model: core kinematics over the toy zone's flat ground.
pub type ToyMotion = CoreMotion<FlatGround>;

/// A frame sink that draws nothing (headless runs and tests).
#[derive(Clone, Copy, Debug, Default)]
pub struct HeadlessSink;

impl FrameSink for HeadlessSink {
    fn resize(&mut self, _width: u32, _height: u32) {}
    fn submit(&mut self, _frame: &FrameContext<'_>) {}
}

/// Overrides of the package's presentation timing.
#[derive(Clone, Copy, Debug, Default)]
pub struct ClientOptions {
    /// The interpolation delay policy (the package's `[tunables.client]` when `None`).
    pub delay: Option<mantis_client::jitter::DelayConfig>,
    /// Whether the server timeline absorbs a sustained latency shift at once (the engine
    /// default, on, when `None`).
    pub timeline_adaptive: Option<bool>,
}

/// Errors building a toy client.
#[derive(Debug)]
pub enum ToyClientError {
    /// The package manifest is invalid.
    Package(package::PackageError),
    /// The motion parameters are invalid.
    Motion(&'static str),
    /// The module graph or a client module refused to start.
    Modules(String),
    /// The control scheme failed to build.
    Controls(mantis_client::input::InputError),
    /// The network failed.
    Net(mantis_net::NetError),
}

impl core::fmt::Display for ToyClientError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Package(e) => write!(f, "package: {e}"),
            Self::Motion(e) => write!(f, "motion parameter `{e}` is invalid"),
            Self::Modules(e) => write!(f, "modules: {e}"),
            Self::Controls(e) => write!(f, "controls: {e:?}"),
            Self::Net(e) => write!(f, "network: {e:?}"),
        }
    }
}

impl std::error::Error for ToyClientError {}

/// A toy client: the world session (simulation and render loop bodies) and the native
/// protocol session, driven step by step on the caller's thread.
pub struct ToyClient<T: Transport, S: FrameSink> {
    /// The simulation loop body.
    pub sim: SimDriver<ClientSim<ToyMotion, MoveOutbox>>,
    /// The render loop body.
    pub render: RenderLoop<S>,
    /// Timeline markers for presentation.
    pub markers: MarkerReceiver,
    /// The client modules and their UI end of the link, until a UI takes them
    /// ([`ToyClient::take_modules`]); a headless client pumps them itself.
    pub modules: Option<(
        mantis_client::modules::ClientModules,
        mantis_client::modules::ModuleUiLink,
    )>,
    /// The module actions as defined in the input tables.
    pub module_actions: Vec<(&'static str, mantis_client::input::action::ActionId)>,
    /// Properties the modules publish while no UI owns them.
    pub module_props: mantis_ui::Properties,
    /// The network end of the module link (pumped by [`ToyClient::step`], or by the
    /// network thread in a windowed client).
    pub module_net: mantis_client::modules::ModuleNetLink,
    /// The client mods ([`ToyClient::load_mods`]), until a UI takes them
    /// ([`ToyClient::take_mods`]); a headless client runs them itself.
    pub mods: Option<mantis_client::mods::ModHost>,
    /// The mods announced in `Hello` (those that passed the package's checks).
    pub hello_mods: Vec<mantis_adapter_contract::ModuleEntry>,
    /// The native protocol session.
    pub net: NativeSession<T>,
    /// Platform events into the render loop (keys, mouse, resize), when this client
    /// created the channel ([`ToyClient::new`]); a windowed client gets them from the
    /// platform instead.
    pub events: Option<SyncSender<PlatformEvent>>,
    /// The shared clock.
    pub clock: Arc<dyn HostClock>,
    /// Reconnecting through the gateway after a drop ([`ToyClient::enable_reconnect`]);
    /// [`ToyClient::step`] drives it.
    pub redial: Option<gateway::Redial<T>>,
}

impl<T: Transport, S: FrameSink> core::fmt::Debug for ToyClient<T, S> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("ToyClient")
            .field("net", &self.net)
            .finish_non_exhaustive()
    }
}

impl<T: Transport, S: FrameSink> ToyClient<T, S> {
    /// A client over `transport`, drawing into `sink`, on `clock`.
    ///
    /// # Errors
    /// [`ToyClientError`] for an invalid manifest or control scheme.
    pub fn new(transport: T, clock: Arc<dyn HostClock>, sink: S) -> Result<Self, ToyClientError> {
        let (events, rx) = platform_event_channel();
        let mut client = Self::new_with_events(transport, clock, sink, rx)?;
        client.events = Some(events);
        Ok(client)
    }

    /// A client whose render loop reads platform events from `events` (the window's).
    ///
    /// # Errors
    /// [`ToyClientError`] for an invalid manifest or control scheme.
    pub fn new_with_events(
        transport: T,
        clock: Arc<dyn HostClock>,
        sink: S,
        events: std::sync::mpsc::Receiver<PlatformEvent>,
    ) -> Result<Self, ToyClientError> {
        Self::new_for_content(transport, clock, sink, events, package::content())
    }

    /// As [`ToyClient::new_with_events`], announcing `content` at the handshake: the
    /// cooked gameplay bundle hash ([`world::OpenedWorld::content_hash`]) a cooked server
    /// checks.
    ///
    /// # Errors
    /// [`ToyClientError`] for an invalid manifest or control scheme.
    pub fn new_for_content(
        transport: T,
        clock: Arc<dyn HostClock>,
        sink: S,
        events: std::sync::mpsc::Receiver<PlatformEvent>,
        content: mantis_core::content::ContentHash,
    ) -> Result<Self, ToyClientError> {
        Self::build(transport, clock, sink, events, content, ClientOptions::default())
    }

    /// As [`ToyClient::new`], with `options` overriding the package's presentation timing
    /// (tests compare the adaptive interpolation delay with a fixed one).
    ///
    /// # Errors
    /// [`ToyClientError`] for an invalid manifest or control scheme.
    pub fn new_with_options(
        transport: T,
        clock: Arc<dyn HostClock>,
        sink: S,
        options: ClientOptions,
    ) -> Result<Self, ToyClientError> {
        let (events, rx) = platform_event_channel();
        let mut client = Self::build(transport, clock, sink, rx, package::content(), options)?;
        client.events = Some(events);
        Ok(client)
    }

    fn build(
        transport: T,
        clock: Arc<dyn HostClock>,
        sink: S,
        events: std::sync::mpsc::Receiver<PlatformEvent>,
        content: mantis_core::content::ContentHash,
        options: ClientOptions,
    ) -> Result<Self, ToyClientError> {
        let tunables = package::Tunables::parse(package::PACKAGE_TOML).map_err(ToyClientError::Package)?;
        let motion = Motion::new(tunables.motion).map_err(ToyClientError::Motion)?;
        let graph = package::module_graph().map_err(ToyClientError::Modules)?;
        let registry = mantis_client::modules::ClientModules::new(&graph, &modules::linked())
            .map_err(|e| ToyClientError::Modules(e.to_string()))?;
        let (controls, module_actions) =
            controls::Controls::build_with(Some(&registry)).map_err(ToyClientError::Controls)?;
        let (module_net, module_ui) = mantis_client::modules::module_link(256);
        let mut module_props = mantis_ui::Properties::new();
        let mut registry = registry;
        registry.start(&mut module_props);
        let mut config = WorldSessionConfig::new(tunables.tick_rate);
        config.sim.delay = options.delay.unwrap_or(tunables.delay);
        if let Some(on) = options.timeline_adaptive {
            config.sim.timeline_adaptive = on;
        }
        let (outbox, moves) = move_channel(256);
        let build = build_world_session(
            &config,
            WorldSessionParts {
                motion: CoreMotion::new(motion),
                ground: Arc::new(FlatGround(0.0)),
                initial_state: MotionState::at_rest(Vec3::ZERO, Angle16(0)),
                router: controls.router,
                intents: controls.intents,
                outbox,
            },
            Arc::clone(&clock),
            events,
            sink,
        );
        let net = NativeSession::new(
            transport,
            Arc::clone(&clock),
            NetConfig::new(content),
            build.snapshots,
            moves,
        );
        Ok(Self {
            sim: build.sim,
            render: build.render,
            markers: build.markers,
            modules: Some((registry, module_ui)),
            module_actions,
            module_props,
            module_net,
            mods: None,
            hello_mods: Vec::new(),
            net,
            events: None,
            clock,
            redial: None,
        })
    }

    /// Loads the client mods in `dir` (one folder per mod; see `mantis_client::mods`),
    /// checked against this package: its permitted keys and its modules. Call before
    /// [`ToyClient::start`], which announces the mods that passed in `Hello`. Returns the
    /// notices (folders that did not load, mods refused), which the UI also shows.
    ///
    /// # Errors
    /// [`ToyClientError::Modules`] when the package manifest cannot be read.
    pub fn load_mods(
        &mut self,
        dir: &std::path::Path,
    ) -> Result<Vec<mantis_client::mods::ModNotice>, ToyClientError> {
        let permitted = package::client_mods().map_err(ToyClientError::Modules)?;
        let graph = package::module_graph().map_err(ToyClientError::Modules)?;
        let (mods, folder_notices) = mantis_client::mods::scan(dir);
        let host = mantis_client::mods::ModHost::new(
            mods,
            folder_notices,
            &mantis_client::mods::ModEnvironment {
                permitted: &permitted,
                graph: &graph,
            },
        );
        self.hello_mods = host.hello_modules();
        let notices = host.notices();
        self.mods = Some(host);
        Ok(notices)
    }

    /// Hands the mods to a UI ([`mantis_client::ui_layer::UiLayer::set_mods`]), which then
    /// runs them on the render thread beside the modules.
    pub fn take_mods(&mut self) -> Option<mantis_client::mods::ModHost> {
        self.mods.take()
    }

    /// Hands the client modules and their link to a UI (which then pumps them on the
    /// render thread); the network end stays with this client.
    pub fn take_modules(
        &mut self,
    ) -> Option<(
        mantis_client::modules::ClientModules,
        mantis_client::modules::ModuleUiLink,
    )> {
        self.modules.take()
    }

    /// Pumps the network end of the module link (for a client whose network session
    /// runs on its own thread).
    pub fn pump_modules(net: &mut NativeSession<T>, link: &mut mantis_client::modules::ModuleNetLink) {
        link.pump(net);
    }

    /// Opens the session with `token`, announcing the loaded mods ([`ToyClient::load_mods`]).
    pub fn start(&mut self, token: &[u8]) {
        self.net.start_with_modules(token, &self.hello_mods);
    }

    /// Reconnects after a dropped connection: `dial` opens a new connection to the
    /// gateway, and the session resumes with its ticket (or `launcher_token` once the
    /// ticket expired). The status is [`gateway::Redial::status`]; a module UI shows it
    /// once given it ([`mantis_client::ui_layer::UiLayer::set_connection`]).
    pub fn enable_reconnect(&mut self, launcher_token: &[u8], dial: gateway::Dial<T>) {
        self.redial = Some(gateway::Redial::new(launcher_token, dial));
    }

    /// One step on the caller's thread: receive (and reconnect after a drop), apply the server's permitted list to the
    /// mods and run them (while no UI took them), run every due simulation tick, send the
    /// moves it produced, and render `frames` frames.
    pub fn step(&mut self, frames: u32) {
        self.net.step();
        if let Some(r) = self.redial.as_mut() {
            r.step(self.clock.now(), &mut self.net);
        }
        self.module_net.pump(&mut self.net);
        if let Some((registry, link)) = self.modules.as_mut() {
            link.pump(registry, &mut self.module_props);
            let permitted = link.take_permitted();
            if let Some(mods) = self.mods.as_mut() {
                if let Some(p) = permitted {
                    mods.apply_permitted(&p);
                }
                mods.step(Some(registry), &mut self.module_props);
                // What the mods routed to the modules leaves on this step.
                link.pump(registry, &mut self.module_props);
                self.module_net.pump(&mut self.net);
            }
        }
        let _ = self.sim.run_due(self.clock.now());
        self.net.send_moves();
        for _ in 0..frames {
            let _ = self.render.frame();
        }
    }
}

impl<S: FrameSink> ToyClient<mantis_net::quic::QuicClient, S> {
    /// Connects to a toy server over QUIC, verifying it with `trust` (a CA bundle and the
    /// server name, or a pinned development leaf).
    ///
    /// # Errors
    /// [`ToyClientError::Net`] ([`mantis_net::NetError::Untrusted`] for a refused
    /// certificate; [`trust::notice`] words it) or a build error.
    pub fn connect_trusted(
        runtime: &mantis_net::NetRuntime,
        addr: std::net::SocketAddr,
        trust: &mantis_net::quic::ServerTrust,
        clock: Arc<dyn HostClock>,
        sink: S,
    ) -> Result<Self, ToyClientError> {
        let transport = mantis_net::quic::QuicClient::connect_trusted(runtime, addr, trust)
            .map_err(ToyClientError::Net)?;
        Self::new(transport, clock, sink)
    }

    /// Connects to a toy server over QUIC, trusting exactly `server_cert_der`.
    ///
    /// # Errors
    /// [`ToyClientError::Net`] or a build error.
    pub fn connect_quic(
        runtime: &mantis_net::NetRuntime,
        addr: std::net::SocketAddr,
        server_cert_der: &[u8],
        clock: Arc<dyn HostClock>,
        sink: S,
    ) -> Result<Self, ToyClientError> {
        let transport = mantis_net::quic::QuicClient::connect(runtime, addr, server_cert_der)
            .map_err(ToyClientError::Net)?;
        Self::new(transport, clock, sink)
    }
}

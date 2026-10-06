//! The platform layer: the window, the OS event loop, and GPU device and surface creation.
//!
//! This is the only module that touches the operating system: `winit`, `wgpu` surface
//! creation, and the OS monotonic clock ([`MonotonicClock`]). Everything
//! else in the crate runs headless and is tested without a window; nothing here is called
//! from tests.
//!
//! The OS event loop runs on the main thread (a platform requirement). It translates OS
//! events into [`PlatformEvent`]s and forwards them to the render thread, which owns the
//! device, the surface, and presentation.

use std::sync::Arc;
use std::sync::mpsc::{Receiver, SyncSender};

use winit::application::ApplicationHandler;
use winit::event::{DeviceEvent, DeviceId, ElementState, MouseScrollDelta, WindowEvent};
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop};
use winit::keyboard::PhysicalKey;
use winit::window::{Window, WindowId};

use crate::input::device::{ButtonSource, KeyCode, MouseButton, RawInput, WheelDirection};
use crate::threads::render_thread::{FrameContext, FrameSink, PlatformEvent};

pub use os_clock::MonotonicClock;

/// The operating system's monotonic clock behind the injected
/// [`HostClock`](crate::time::HostClock): the one place host time enters the client. The
/// simulation never sees it; it sees ticks. This module is the crate's only exception to
/// the determinism lint's wall-clock ban, and any such allow elsewhere is a bug.
#[allow(clippy::disallowed_types, clippy::disallowed_methods)] // The single OS-clock allow site.
mod os_clock {
    use std::time::Instant;

    use crate::time::{HostClock, HostInstant};

    /// Production [`HostClock`] backed by the OS monotonic clock.
    #[derive(Debug)]
    pub struct MonotonicClock {
        origin: Instant,
    }

    impl MonotonicClock {
        /// A clock whose origin is now.
        pub fn new() -> Self {
            Self {
                origin: Instant::now(),
            }
        }
    }

    impl Default for MonotonicClock {
        fn default() -> Self {
            Self::new()
        }
    }

    impl HostClock for MonotonicClock {
        #[allow(clippy::cast_possible_truncation)] // Saturated explicitly.
        fn now(&self) -> HostInstant {
            let n = self.origin.elapsed().as_nanos();
            HostInstant::from_nanos(if n > u128::from(u64::MAX) {
                u64::MAX
            } else {
                n as u64
            })
        }
    }
}

/// Window and presentation settings.
#[derive(Clone, Debug)]
pub struct PlatformConfig {
    /// Window title.
    pub title: String,
    /// Initial inner width in physical pixels.
    pub width: u32,
    /// Initial inner height in physical pixels.
    pub height: u32,
    /// Wait for vertical blank when presenting.
    pub vsync: bool,
}

impl Default for PlatformConfig {
    fn default() -> Self {
        Self {
            title: "Mantis".to_owned(),
            width: 1280,
            height: 720,
            vsync: true,
        }
    }
}

/// Platform failures. Every one is reported, none is a panic.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum PlatformError {
    /// The OS event loop could not be created or run.
    EventLoop(String),
    /// The window could not be created.
    Window(String),
    /// The GPU surface could not be created or is unsupported by the adapter.
    Surface(String),
    /// No suitable GPU adapter.
    Adapter(String),
    /// The GPU device could not be created.
    Device(String),
    /// The session start callback failed.
    Start(String),
}

impl core::fmt::Display for PlatformError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            PlatformError::EventLoop(e) => write!(f, "event loop: {e}"),
            PlatformError::Window(e) => write!(f, "window: {e}"),
            PlatformError::Surface(e) => write!(f, "surface: {e}"),
            PlatformError::Adapter(e) => write!(f, "adapter: {e}"),
            PlatformError::Device(e) => write!(f, "device: {e}"),
            PlatformError::Start(e) => write!(f, "start: {e}"),
        }
    }
}

impl std::error::Error for PlatformError {}

/// The GPU objects bound to the window, handed to the render thread.
#[derive(Debug)]
pub struct GpuTarget {
    /// The instance.
    pub instance: wgpu::Instance,
    /// The adapter.
    pub adapter: wgpu::Adapter,
    /// The device.
    pub device: wgpu::Device,
    /// The queue.
    pub queue: wgpu::Queue,
    /// The window surface.
    pub surface: wgpu::Surface<'static>,
    /// The surface configuration in effect.
    pub config: wgpu::SurfaceConfiguration,
}

impl GpuTarget {
    /// Reconfigures the surface for a new size. Zero sizes (minimized) are ignored.
    pub fn resize(&mut self, width: u32, height: u32) {
        if width == 0 || height == 0 {
            return;
        }
        self.config.width = width;
        self.config.height = height;
        self.surface.configure(&self.device, &self.config);
    }
}

/// Creates the instance, surface, adapter, and device for `window`.
///
/// # Errors
/// [`PlatformError::Surface`], [`PlatformError::Adapter`], or [`PlatformError::Device`].
pub fn create_gpu_target(
    event_loop: &ActiveEventLoop,
    window: Arc<Window>,
    vsync: bool,
) -> Result<GpuTarget, PlatformError> {
    let display = event_loop.owned_display_handle();
    let instance = wgpu::Instance::new(wgpu::InstanceDescriptor::new_with_display_handle_from_env(
        Box::new(display),
    ));
    let size = window.inner_size();
    let surface = instance
        .create_surface(window)
        .map_err(|e| PlatformError::Surface(e.to_string()))?;
    let adapter = pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
        power_preference: wgpu::PowerPreference::HighPerformance,
        compatible_surface: Some(&surface),
        ..Default::default()
    }))
    .map_err(|e| PlatformError::Adapter(e.to_string()))?;
    // Every optional capability the renderer can use (bindless, indirect first instance,
    // block-compressed textures) is requested when the adapter offers it.
    let features = mantis_render::gpu::Capabilities::of(adapter.features()).features();
    let (device, queue) = pollster::block_on(adapter.request_device(&wgpu::DeviceDescriptor {
        label: Some("mantis"),
        required_features: features,
        ..Default::default()
    }))
    .map_err(|e| PlatformError::Device(e.to_string()))?;
    let mut config = surface
        .get_default_config(&adapter, size.width.max(1), size.height.max(1))
        .ok_or_else(|| PlatformError::Surface("surface unsupported by adapter".to_owned()))?;
    config.present_mode = if vsync {
        wgpu::PresentMode::AutoVsync
    } else {
        wgpu::PresentMode::AutoNoVsync
    };
    surface.configure(&device, &config);
    Ok(GpuTarget {
        instance,
        adapter,
        device,
        queue,
        surface,
        config,
    })
}

/// A session started on a window: anything the platform keeps alive until exit (the render
/// thread handle, the scope stack). Dropping it shuts the session down.
pub trait RunningSession {
    /// True once the session ended on its own (its render loop exited).
    fn is_finished(&self) -> bool;
}

struct App<F, R> {
    config: PlatformConfig,
    start: Option<F>,
    window: Option<Arc<Window>>,
    events: Option<SyncSender<PlatformEvent>>,
    session: Option<R>,
    error: Option<PlatformError>,
}

/// Runs the OS event loop on the calling (main) thread.
///
/// When the window first exists, `start` receives the GPU target and the receiving end of
/// the platform event channel, and returns the running session (normally a render thread
/// owning both). The loop forwards events until the window closes or the session ends,
/// then drops the session, which joins its threads.
///
/// # Errors
/// Any [`PlatformError`] from window, device, or session creation, or from the loop.
pub fn run<F, R>(config: PlatformConfig, start: F) -> Result<(), PlatformError>
where
    F: FnOnce(GpuTarget, Receiver<PlatformEvent>) -> Result<R, PlatformError>,
    R: RunningSession,
{
    let event_loop = EventLoop::new().map_err(|e| PlatformError::EventLoop(e.to_string()))?;
    event_loop.set_control_flow(ControlFlow::Poll);
    let mut app = App {
        config,
        start: Some(start),
        window: None,
        events: None,
        session: None,
        error: None,
    };
    event_loop
        .run_app(&mut app)
        .map_err(|e| PlatformError::EventLoop(e.to_string()))?;
    drop(app.session.take());
    app.error.map_or(Ok(()), Err)
}

impl<F, R> App<F, R>
where
    F: FnOnce(GpuTarget, Receiver<PlatformEvent>) -> Result<R, PlatformError>,
    R: RunningSession,
{
    fn fail(&mut self, event_loop: &ActiveEventLoop, e: PlatformError) {
        self.error = Some(e);
        event_loop.exit();
    }

    fn forward(&mut self, event_loop: &ActiveEventLoop, ev: PlatformEvent) {
        if let Some(tx) = &self.events
            && tx.send(ev).is_err()
        {
            // The render thread is gone: the session is over.
            event_loop.exit();
        }
    }
}

impl<F, R> ApplicationHandler for App<F, R>
where
    F: FnOnce(GpuTarget, Receiver<PlatformEvent>) -> Result<R, PlatformError>,
    R: RunningSession,
{
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if self.window.is_some() {
            return;
        }
        let attrs = Window::default_attributes()
            .with_title(self.config.title.clone())
            .with_inner_size(winit::dpi::PhysicalSize::new(
                self.config.width,
                self.config.height,
            ));
        let window = match event_loop.create_window(attrs) {
            Ok(w) => Arc::new(w),
            Err(e) => return self.fail(event_loop, PlatformError::Window(e.to_string())),
        };
        // IME composition is handled on the render thread by the UI (plan 8.6).
        window.set_ime_allowed(true);
        let target = match create_gpu_target(event_loop, Arc::clone(&window), self.config.vsync) {
            Ok(t) => t,
            Err(e) => return self.fail(event_loop, e),
        };
        let Some(start) = self.start.take() else { return };
        let (tx, rx) = crate::host::platform_event_channel();
        match start(target, rx) {
            Ok(session) => {
                self.session = Some(session);
                self.events = Some(tx);
                self.window = Some(window);
            }
            Err(e) => self.fail(event_loop, e),
        }
    }

    fn window_event(&mut self, event_loop: &ActiveEventLoop, _id: WindowId, event: WindowEvent) {
        match event {
            WindowEvent::CloseRequested => {
                self.forward(event_loop, PlatformEvent::CloseRequested);
                event_loop.exit();
            }
            WindowEvent::Resized(size) => self.forward(
                event_loop,
                PlatformEvent::Resized {
                    width: size.width,
                    height: size.height,
                },
            ),
            WindowEvent::Focused(false) => {
                self.forward(event_loop, PlatformEvent::Input(RawInput::FocusLost));
            }
            WindowEvent::CursorMoved { position, .. } => {
                #[allow(clippy::cast_possible_truncation)] // Pixel coordinates fit f32.
                let (x, y) = (position.x as f32, position.y as f32);
                self.forward(event_loop, PlatformEvent::CursorMoved { x, y });
            }
            WindowEvent::Ime(ime) => match ime {
                winit::event::Ime::Preedit(text, cursor) => {
                    self.forward(event_loop, PlatformEvent::ImePreedit { text, cursor });
                }
                winit::event::Ime::Commit(text) => self.forward(event_loop, PlatformEvent::ImeCommit(text)),
                winit::event::Ime::Enabled | winit::event::Ime::Disabled => {}
            },
            WindowEvent::KeyboardInput { event, .. } => {
                if event.state == ElementState::Pressed
                    && let Some(text) = event.text.as_ref().filter(|t| t.chars().any(|c| !c.is_control()))
                {
                    self.forward(event_loop, PlatformEvent::Text(text.to_string()));
                }
                if let PhysicalKey::Code(code) = event.physical_key
                    && let Some(key) = map_key(code)
                {
                    let pressed = event.state == ElementState::Pressed;
                    self.forward(
                        event_loop,
                        PlatformEvent::Input(RawInput::Button {
                            source: ButtonSource::Key(key),
                            pressed,
                        }),
                    );
                }
            }
            WindowEvent::MouseInput { state, button, .. } => {
                let pressed = state == ElementState::Pressed;
                let source = ButtonSource::Mouse(map_mouse(button));
                self.forward(
                    event_loop,
                    PlatformEvent::Input(RawInput::Button { source, pressed }),
                );
            }
            WindowEvent::MouseWheel { delta, .. } => {
                let dy = match delta {
                    MouseScrollDelta::LineDelta(_, y) => f64::from(y),
                    MouseScrollDelta::PixelDelta(p) => p.y,
                };
                if dy != 0.0 {
                    let dir = if dy > 0.0 {
                        WheelDirection::Up
                    } else {
                        WheelDirection::Down
                    };
                    let source = ButtonSource::Wheel(dir);
                    self.forward(
                        event_loop,
                        PlatformEvent::Input(RawInput::Button {
                            source,
                            pressed: true,
                        }),
                    );
                    self.forward(
                        event_loop,
                        PlatformEvent::Input(RawInput::Button {
                            source,
                            pressed: false,
                        }),
                    );
                }
            }
            _ => {}
        }
    }

    #[allow(clippy::cast_possible_truncation)] // Mouse counts per event fit f32.
    fn device_event(&mut self, event_loop: &ActiveEventLoop, _id: DeviceId, event: DeviceEvent) {
        if let DeviceEvent::MouseMotion { delta: (dx, dy) } = event {
            self.forward(
                event_loop,
                PlatformEvent::Input(RawInput::MouseMotion {
                    dx: dx as f32,
                    dy: dy as f32,
                }),
            );
        }
    }

    fn about_to_wait(&mut self, event_loop: &ActiveEventLoop) {
        if self.session.as_ref().is_some_and(RunningSession::is_finished) {
            event_loop.exit();
        }
    }
}

fn map_mouse(b: winit::event::MouseButton) -> MouseButton {
    use winit::event::MouseButton as W;
    match b {
        W::Left => MouseButton::Left,
        W::Right => MouseButton::Right,
        W::Middle => MouseButton::Middle,
        W::Back => MouseButton::Back,
        W::Forward => MouseButton::Forward,
        W::Other(n) => MouseButton::Other(n),
    }
}

#[allow(clippy::too_many_lines)] // One arm per key is the clearest form.
fn map_key(k: winit::keyboard::KeyCode) -> Option<KeyCode> {
    use winit::keyboard::KeyCode as W;
    Some(match k {
        W::KeyA => KeyCode::A,
        W::KeyB => KeyCode::B,
        W::KeyC => KeyCode::C,
        W::KeyD => KeyCode::D,
        W::KeyE => KeyCode::E,
        W::KeyF => KeyCode::F,
        W::KeyG => KeyCode::G,
        W::KeyH => KeyCode::H,
        W::KeyI => KeyCode::I,
        W::KeyJ => KeyCode::J,
        W::KeyK => KeyCode::K,
        W::KeyL => KeyCode::L,
        W::KeyM => KeyCode::M,
        W::KeyN => KeyCode::N,
        W::KeyO => KeyCode::O,
        W::KeyP => KeyCode::P,
        W::KeyQ => KeyCode::Q,
        W::KeyR => KeyCode::R,
        W::KeyS => KeyCode::S,
        W::KeyT => KeyCode::T,
        W::KeyU => KeyCode::U,
        W::KeyV => KeyCode::V,
        W::KeyW => KeyCode::W,
        W::KeyX => KeyCode::X,
        W::KeyY => KeyCode::Y,
        W::KeyZ => KeyCode::Z,
        W::Digit0 => KeyCode::Digit0,
        W::Digit1 => KeyCode::Digit1,
        W::Digit2 => KeyCode::Digit2,
        W::Digit3 => KeyCode::Digit3,
        W::Digit4 => KeyCode::Digit4,
        W::Digit5 => KeyCode::Digit5,
        W::Digit6 => KeyCode::Digit6,
        W::Digit7 => KeyCode::Digit7,
        W::Digit8 => KeyCode::Digit8,
        W::Digit9 => KeyCode::Digit9,
        W::F1 => KeyCode::F1,
        W::F2 => KeyCode::F2,
        W::F3 => KeyCode::F3,
        W::F4 => KeyCode::F4,
        W::F5 => KeyCode::F5,
        W::F6 => KeyCode::F6,
        W::F7 => KeyCode::F7,
        W::F8 => KeyCode::F8,
        W::F9 => KeyCode::F9,
        W::F10 => KeyCode::F10,
        W::F11 => KeyCode::F11,
        W::F12 => KeyCode::F12,
        W::Escape => KeyCode::Escape,
        W::Enter => KeyCode::Enter,
        W::Space => KeyCode::Space,
        W::Tab => KeyCode::Tab,
        W::Backspace => KeyCode::Backspace,
        W::CapsLock => KeyCode::CapsLock,
        W::ShiftLeft => KeyCode::ShiftLeft,
        W::ShiftRight => KeyCode::ShiftRight,
        W::ControlLeft => KeyCode::ControlLeft,
        W::ControlRight => KeyCode::ControlRight,
        W::AltLeft => KeyCode::AltLeft,
        W::AltRight => KeyCode::AltRight,
        W::SuperLeft => KeyCode::SuperLeft,
        W::SuperRight => KeyCode::SuperRight,
        W::ArrowUp => KeyCode::ArrowUp,
        W::ArrowDown => KeyCode::ArrowDown,
        W::ArrowLeft => KeyCode::ArrowLeft,
        W::ArrowRight => KeyCode::ArrowRight,
        W::Insert => KeyCode::Insert,
        W::Delete => KeyCode::Delete,
        W::Home => KeyCode::Home,
        W::End => KeyCode::End,
        W::PageUp => KeyCode::PageUp,
        W::PageDown => KeyCode::PageDown,
        W::Minus => KeyCode::Minus,
        W::Equal => KeyCode::Equal,
        W::BracketLeft => KeyCode::BracketLeft,
        W::BracketRight => KeyCode::BracketRight,
        W::Backslash => KeyCode::Backslash,
        W::Semicolon => KeyCode::Semicolon,
        W::Quote => KeyCode::Quote,
        W::Backquote => KeyCode::Backquote,
        W::Comma => KeyCode::Comma,
        W::Period => KeyCode::Period,
        W::Slash => KeyCode::Slash,
        W::Numpad0 => KeyCode::Numpad0,
        W::Numpad1 => KeyCode::Numpad1,
        W::Numpad2 => KeyCode::Numpad2,
        W::Numpad3 => KeyCode::Numpad3,
        W::Numpad4 => KeyCode::Numpad4,
        W::Numpad5 => KeyCode::Numpad5,
        W::Numpad6 => KeyCode::Numpad6,
        W::Numpad7 => KeyCode::Numpad7,
        W::Numpad8 => KeyCode::Numpad8,
        W::Numpad9 => KeyCode::Numpad9,
        W::NumpadAdd => KeyCode::NumpadAdd,
        W::NumpadSubtract => KeyCode::NumpadSubtract,
        W::NumpadMultiply => KeyCode::NumpadMultiply,
        W::NumpadDivide => KeyCode::NumpadDivide,
        W::NumpadEnter => KeyCode::NumpadEnter,
        W::NumpadDecimal => KeyCode::NumpadDecimal,
        _ => return None,
    })
}

/// A minimal frame sink that clears the window to a color and presents. It proves the
/// device, surface, and present path until the renderer replaces it.
pub struct ClearSink {
    target: GpuTarget,
    color: fn(&FrameContext<'_>) -> [f64; 3],
}

impl core::fmt::Debug for ClearSink {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("ClearSink").finish_non_exhaustive()
    }
}

impl ClearSink {
    /// A sink clearing to `color(frame)` each frame.
    pub fn new(target: GpuTarget, color: fn(&FrameContext<'_>) -> [f64; 3]) -> Self {
        Self { target, color }
    }
}

impl FrameSink for ClearSink {
    fn resize(&mut self, width: u32, height: u32) {
        self.target.resize(width, height);
    }

    fn submit(&mut self, frame: &FrameContext<'_>) {
        let texture = match self.target.surface.get_current_texture() {
            wgpu::CurrentSurfaceTexture::Success(t) | wgpu::CurrentSurfaceTexture::Suboptimal(t) => t,
            wgpu::CurrentSurfaceTexture::Outdated | wgpu::CurrentSurfaceTexture::Lost => {
                let (w, h) = (self.target.config.width, self.target.config.height);
                self.target.resize(w, h);
                return;
            }
            _ => return, // timeout, occluded, validation: skip this frame
        };
        let view = texture
            .texture
            .create_view(&wgpu::TextureViewDescriptor::default());
        let [r, g, b] = (self.color)(frame);
        let mut encoder = self
            .target
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor { label: Some("clear") });
        {
            let _pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("clear"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &view,
                    depth_slice: None,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(wgpu::Color { r, g, b, a: 1.0 }),
                        store: wgpu::StoreOp::Store,
                    },
                })],
                ..Default::default()
            });
        }
        let _ = self.target.queue.submit(Some(encoder.finish()));
        self.target.queue.present(texture);
    }
}

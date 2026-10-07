//! The UI on the render thread (plan 8.6): platform events go to the UI first, and only
//! what it does not consume reaches game actions; each frame the UI lays out, and its
//! draw list and glyph atlas go to the renderer's UI pass. Widgets emit intents, which the
//! client drains and turns into game requests.
//!
//! Key and button releases always reach game actions as well, so a key held before a text
//! field took focus is never stuck down.
//!
//! Client mods ([`crate::mods`]) run here too, beside the modules: every frame the
//! server's latest permitted list is applied before any mod runs, the mods tick, and their
//! screens join the module layout (recomposed whenever the set of running mods changes).
//! An intent from a mod's widget goes to [`crate::mods::ModHost::on_widget_intent`] and
//! never to the modules or the host directly.
//!
//! The connection status ([`UiLayer::set_connection`]) shows as a notice at the bottom
//! of the module layout while the session is reconnecting or was refused.

use mantis_render::renderer::Renderer;
use mantis_ui::{Handled, Modifiers, PointerButton, Ui, UiEvent, UiIntent, UiKey};

use crate::input::device::{ButtonSource, KeyCode, MouseButton, RawInput, WheelDirection};
use crate::mods::{ModHost, ModRoute};
use crate::reconnect::{ReconnectStatus, SharedStatus};
use crate::threads::render_thread::PlatformEvent;

/// Client modules wired into the UI.
#[derive(Debug)]
struct Modules {
    registry: crate::modules::ClientModules,
    link: crate::modules::ModuleUiLink,
    actions: Vec<(&'static str, crate::input::action::ActionId)>,
    /// The module screens shown, by key (kept to recompose with the mods' screens).
    open: Vec<String>,
}

/// The UI and its input state.
#[derive(Debug)]
pub struct UiLayer {
    ui: Ui,
    modules: Option<Modules>,
    mods: Option<ModHost>,
    /// The mods' layout version the UI was last composed with.
    composed: u64,
    modifiers: Modifiers,
    intents: Vec<UiIntent>,
    /// The connection status shown, and the version last published.
    connection: Option<(std::sync::Arc<SharedStatus>, u64)>,
}

/// Publishes `client.connection.has_notice` and `client.connection.notice` (the panel
/// [`crate::modules::ClientModules::compose`] adds).
fn publish_connection(status: &ReconnectStatus, props: &mut mantis_ui::Properties) {
    let notice = status.notice();
    let id = props.intern("client.connection.has_notice");
    let _ = props.set_bool(id, notice.is_some());
    let id = props.intern("client.connection.notice");
    let _ = props.set_text(id, notice.as_deref().unwrap_or(""));
}

fn ui_key(key: KeyCode) -> UiKey {
    match key {
        KeyCode::Tab => UiKey::Tab,
        KeyCode::Enter => UiKey::Enter,
        KeyCode::Escape => UiKey::Escape,
        KeyCode::Backspace => UiKey::Backspace,
        KeyCode::Delete => UiKey::Delete,
        KeyCode::ArrowLeft => UiKey::Left,
        KeyCode::ArrowRight => UiKey::Right,
        KeyCode::ArrowUp => UiKey::Up,
        KeyCode::ArrowDown => UiKey::Down,
        KeyCode::Home => UiKey::Home,
        KeyCode::End => UiKey::End,
        KeyCode::PageUp => UiKey::PageUp,
        KeyCode::PageDown => UiKey::PageDown,
        KeyCode::Space => UiKey::Space,
        other => UiKey::Other(other as u32),
    }
}

fn pointer_button(b: MouseButton) -> PointerButton {
    match b {
        MouseButton::Left => PointerButton::Left,
        MouseButton::Right => PointerButton::Right,
        MouseButton::Middle => PointerButton::Middle,
        MouseButton::Back => PointerButton::Other(3),
        MouseButton::Forward => PointerButton::Other(4),
        MouseButton::Other(n) => PointerButton::Other(n),
    }
}

impl UiLayer {
    /// A layer over `ui`.
    pub fn new(ui: Ui) -> Self {
        Self {
            ui,
            modules: None,
            mods: None,
            composed: 0,
            modifiers: Modifiers::default(),
            intents: Vec::with_capacity(32),
            connection: None,
        }
    }

    /// A layer whose UI shows the client modules' `open` screens (composed into one
    /// layout), whose properties the modules publish, and whose intents go to the
    /// modules first. `actions` are the module actions as defined in the input tables
    /// ([`crate::modules::ClientModules::define_actions`]).
    ///
    /// # Errors
    /// [`mantis_ui::MarkupError`] when the composed layout does not parse.
    pub fn with_modules(
        fonts: mantis_ui::FontLibrary,
        registry: crate::modules::ClientModules,
        link: crate::modules::ModuleUiLink,
        actions: Vec<(&'static str, crate::input::action::ActionId)>,
        open: &[&str],
    ) -> Result<Self, mantis_ui::MarkupError> {
        let mut registry = registry;
        let (layout, theme) = registry.compose(open);
        let mut ui = Ui::new(fonts, &layout, Some(&theme))?;
        registry.start(ui.properties_mut());
        // An unset flag reads as visible: the connection panel starts hidden.
        publish_connection(&ReconnectStatus::Live, ui.properties_mut());
        let mut layer = Self::new(ui);
        layer.modules = Some(Modules {
            registry,
            link,
            actions,
            open: open.iter().map(|s| (*s).to_owned()).collect(),
        });
        Ok(layer)
    }

    /// Runs `mods` in this layer: their screens join the module layout once they run, and
    /// the notices panel shows why any mod was refused or stopped. Mods need a module
    /// layer ([`UiLayer::with_modules`]); on a plain layer they are kept but never drawn.
    ///
    /// # Errors
    /// [`mantis_ui::MarkupError`] when the recomposed layout does not parse (the old
    /// layout stays live).
    pub fn set_mods(&mut self, mods: ModHost) -> Result<(), mantis_ui::MarkupError> {
        let mut mods = mods;
        mods.publish(self.ui.properties_mut());
        self.mods = Some(mods);
        self.recompose()
    }

    /// Shows `status` (set by the thread that drives the session): a notice while it
    /// is reconnecting or was refused.
    pub fn set_connection(&mut self, status: std::sync::Arc<SharedStatus>) {
        let version = status.version();
        publish_connection(&status.get(), self.ui.properties_mut());
        self.connection = Some((status, version));
    }

    /// The client mods, if any.
    pub fn mods(&self) -> Option<&ModHost> {
        self.mods.as_ref()
    }

    /// The client mods, if any (to set sounds or audio).
    pub fn mods_mut(&mut self) -> Option<&mut ModHost> {
        self.mods.as_mut()
    }

    /// Recomposes the module layout with the running mods' screens when they changed.
    fn recompose(&mut self) -> Result<(), mantis_ui::MarkupError> {
        let (Some(m), Some(mods)) = (self.modules.as_ref(), self.mods.as_ref()) else {
            return Ok(());
        };
        if self.composed == mods.layout_version() {
            return Ok(());
        }
        let (extra_layout, extra_theme) = mods.compose();
        let open: Vec<&str> = m.open.iter().map(String::as_str).collect();
        let (layout, theme) = m.registry.compose_with(&open, &extra_layout, &extra_theme);
        self.composed = mods.layout_version();
        self.ui.reload(&layout, Some(&theme)).map(|_| ())
    }

    /// Everything but drawing, once per frame: applies what the network delivered to the
    /// modules, applies the server's latest permitted list to the mods (before any mod
    /// runs), ticks the mods and routes the intents automation mods emitted, and
    /// recomposes the layout when the running mods changed. [`UiLayer::draw`] calls it;
    /// headless callers call it directly.
    pub fn update(&mut self) {
        let props = self.ui.properties_mut();
        if let Some((status, shown)) = self.connection.as_mut() {
            let version = status.version();
            if version != *shown {
                *shown = version;
                publish_connection(&status.get(), props);
            }
        }
        let mut permitted = None;
        if let Some(m) = self.modules.as_mut() {
            m.link.pump(&mut m.registry, props);
            permitted = m.link.take_permitted();
        }
        if let Some(mods) = self.mods.as_mut() {
            if let Some(p) = permitted {
                mods.apply_permitted(&p);
            }
            mods.step(self.modules.as_mut().map(|m| &mut m.registry), props);
        }
        if let Err(e) = self.recompose() {
            // Mod layouts were checked at load, so this is a client bug; the old layout
            // stays live and the reason is visible.
            self.composition_failed(&e);
        }
    }

    fn composition_failed(&mut self, e: &mantis_ui::MarkupError) {
        let props = self.ui.properties_mut();
        let id = props.intern("client.mods.compose_error");
        let _ = props.set_text(id, &format!("mod screens could not be composed: {e}"));
    }

    /// Runs the module actions pressed this frame.
    pub fn on_actions(&mut self, frame: &crate::input::router::ActionFrame) {
        let Some(m) = self.modules.as_mut() else { return };
        let actions = &m.actions;
        m.registry.on_actions(
            |name| {
                actions
                    .iter()
                    .any(|(n, id)| *n == name && frame.pressed.contains(*id))
            },
            self.ui.properties_mut(),
        );
    }

    /// The client modules, if any.
    pub fn modules(&self) -> Option<&crate::modules::ClientModules> {
        self.modules.as_ref().map(|m| &m.registry)
    }

    /// The UI (properties, reload, queries).
    pub fn ui_mut(&mut self) -> &mut Ui {
        &mut self.ui
    }

    /// The UI.
    pub fn ui(&self) -> &Ui {
        &self.ui
    }

    fn track_modifiers(&mut self, key: KeyCode, pressed: bool) {
        match key {
            KeyCode::ShiftLeft | KeyCode::ShiftRight => self.modifiers.shift = pressed,
            KeyCode::ControlLeft | KeyCode::ControlRight => self.modifiers.ctrl = pressed,
            KeyCode::AltLeft | KeyCode::AltRight => self.modifiers.alt = pressed,
            KeyCode::SuperLeft | KeyCode::SuperRight => self.modifiers.logo = pressed,
            _ => {}
        }
    }

    /// Offers a platform event to the UI. Returns true when it was consumed (game actions
    /// must not see it); releases are never reported consumed.
    pub fn handle(&mut self, event: &PlatformEvent) -> bool {
        let (ui_event, release) = match event {
            PlatformEvent::Input(RawInput::Button { source, pressed }) => match *source {
                ButtonSource::Key(key) => {
                    self.track_modifiers(key, *pressed);
                    (
                        UiEvent::Key {
                            key: ui_key(key),
                            pressed: *pressed,
                            modifiers: self.modifiers,
                        },
                        !*pressed,
                    )
                }
                ButtonSource::Mouse(button) => (
                    UiEvent::PointerButton {
                        button: pointer_button(button),
                        pressed: *pressed,
                    },
                    !*pressed,
                ),
                ButtonSource::Wheel(direction) => {
                    if !*pressed {
                        return false;
                    }
                    let dy = if direction == WheelDirection::Up {
                        1.0
                    } else {
                        -1.0
                    };
                    (UiEvent::Wheel { dx: 0.0, dy }, false)
                }
                ButtonSource::Gamepad(_) => return false,
            },
            PlatformEvent::Input(RawInput::FocusLost) => {
                self.modifiers = Modifiers::default();
                (UiEvent::FocusLost, true)
            }
            PlatformEvent::Input(RawInput::Axis { .. } | RawInput::MouseMotion { .. })
            | PlatformEvent::Resized { .. }
            | PlatformEvent::CloseRequested => return false,
            PlatformEvent::CursorMoved { x, y } => (UiEvent::PointerMove { x: *x, y: *y }, false),
            PlatformEvent::Text(text) => (UiEvent::Text(text.clone()), false),
            PlatformEvent::ImePreedit { text, cursor } => (
                UiEvent::ImePreedit {
                    text: text.clone(),
                    cursor: *cursor,
                },
                false,
            ),
            PlatformEvent::ImeCommit(text) => (UiEvent::ImeCommit(text.clone()), false),
        };
        let consumed = self.ui.handle(&ui_event) == Handled::Consumed;
        consumed && !release
    }

    /// Lays out the UI for a `viewport` of physical pixels at `scale` and hands the frame
    /// to the renderer's UI pass.
    pub fn draw(
        &mut self,
        renderer: &mut Renderer,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        viewport: [f32; 2],
        scale: f32,
    ) {
        self.update();
        let _ = self.ui.frame(viewport, scale);
        let (list, atlas) = self.ui.draw_parts();
        renderer.set_ui(device, queue, &list.quads, atlas);
    }

    /// Hands every intent widgets emitted since the last call to `f`, after the client
    /// mods and the client modules took theirs. An intent from a mod's widget is the
    /// mods' alone ([`ModHost::on_widget_intent`]): it reaches a module only when that
    /// mod runs at the automation tier, and never reaches `f`.
    pub fn drain_intents(&mut self, mut f: impl FnMut(&UiIntent)) {
        self.ui.drain_intents(&mut self.intents);
        for intent in self.intents.drain(..) {
            if let Some(mods) = self.mods.as_mut() {
                // Intents are user actions; short owned names are fine here.
                let widget = self.ui.widget_name(intent.widget).map(str::to_owned);
                let name = self.ui.intent_name(intent.intent).map(str::to_owned);
                if let (Some(widget), Some(name)) = (widget, name) {
                    let route = mods.on_widget_intent(
                        &widget,
                        &name,
                        intent.payload.as_deref(),
                        self.modules.as_mut().map(|m| &mut m.registry),
                        self.ui.properties_mut(),
                    );
                    if route != ModRoute::NotMod {
                        continue;
                    }
                }
            }
            if let Some(m) = self.modules.as_mut()
                // Intents are user actions; a short owned name is fine here.
                && let Some(name) = self.ui.intent_name(intent.intent).map(str::to_owned)
                && m.registry
                    .on_intent(&name, intent.payload.as_deref(), self.ui.properties_mut())
                    != crate::modules::IntentRoute::NotModule
            {
                continue;
            }
            f(&intent);
        }
    }
}

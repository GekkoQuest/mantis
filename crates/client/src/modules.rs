//! The client's module registry surface (plan 13, decision 0010).
//!
//! A module's client crate (`packages/<p>/modules/<f>/client`) exports `pub struct Module`
//! implementing [`ClientModule`]. The package's client crate links them through the
//! generated `modules.rs` (`linked()` and `MANIFESTS`, written by `mantis-modsync`), the
//! same mechanism the server uses, so one set of manifests drives both hosts:
//! [`ClientModules::new`] takes the package's resolved [`ModuleGraph`] (the one the server
//! resolves from the same manifests) and installs every module in it that has a client
//! half, in graph order.
//!
//! A module registers through one [`ClientRegistrar`]:
//!
//! - **extension kinds** it owns ([`ClientRegistrar::kinds`]): the server-to-client
//!   messages it decodes with its contract crate's codec ([`ClientRegistrar::on_message`])
//!   and the client-to-server intents it may send ([`ModuleContext::send`]). Kinds are
//!   disjoint across modules;
//! - **view-model state** ([`ClientRegistrar::state`]) that its handlers update from
//!   change sets: module messages and entity changes ([`ClientRegistrar::on_entities`]),
//!   publishing bindable UI properties named `<contract>.<name>`;
//! - **UI intents** its widgets emit ([`ClientRegistrar::on_intent`]), named
//!   `<contract>.<name>`;
//! - **screens** by key ([`ClientRegistrar::screen`]), named `<contract>.<name>`: layout
//!   markup whose element ids all start with the implementing module's id prefix
//!   (`std.party` → `std_party_`), plus theme styles;
//!
//! Names live in the namespace of the **contract** the module implements, not its own
//! key: a package module that overrides `std.party` (`contract = "std.party"`) registers
//! `std.party.roster`, binds `std.party.enabled`, and answers `std.party.*` intents, so
//! everything that refers to the contract's names keeps working, while its element ids
//! carry its own prefix and never collide with the module it replaced.
//! - **presentation views** ([`ClientRegistrar::presentation`]): presentation graphs
//!   loaded while the module is enabled;
//! - **input actions** ([`ClientRegistrar::action`]) with default key bindings in the
//!   module's own input context, and what pressing them does.
//!
//! **Feature gating.** Every module publishes `<key>.enabled` and one
//! `<key>.flag.<name>` per manifest flag, from the resolved graph at start and updated at
//! run time: when the server refuses one of the module's kinds with `FeatureDisabled`, the
//! module is disabled on the client too, its presentation views stop, its messages are
//! dropped and counted, its intents are refused, and its screens show it as unavailable
//! (layouts bind `enabled="<key>.enabled"`; never a dead control). Other refusals set
//! `<key>.refusal` to `not allowed` or `invalid` for the screen to show.
//!
//! Nothing here is an error at run time: unknown kinds, malformed payloads, and messages
//! for disabled modules are dropped and counted ([`ModuleStats`]).

use core::any::Any;
use std::collections::BTreeMap;
use std::sync::Arc;

pub use mantis_adapter_contract::{ExtensionKind, ExtensionRefusal};
use mantis_core::ecs::EntityId;
use mantis_core::module::ModuleGraph;
use mantis_core::wire::Message;
use mantis_formats::presentation::PresentationGraph;
use mantis_ui::Properties;

use crate::input::device::KeyCode;

/// Largest extension payload the contract carries.
pub const PAYLOAD: usize = 512;

/// A module's client half.
pub trait ClientModule: Send + Sync {
    /// The module key, as in its manifest (`std.party`).
    fn key(&self) -> &'static str;

    /// Registers everything the module provides.
    ///
    /// # Errors
    /// [`ClientRegistryError`]; the client refuses to start.
    fn register(&self, r: &mut ClientRegistrar<'_>) -> Result<(), ClientRegistryError>;
}

/// Registration was refused.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum ClientRegistryError {
    /// An extension kind is claimed twice.
    DuplicateKind(u16),
    /// A handler names a kind outside the module's declared kinds.
    UndeclaredKind(u16),
    /// An intent, property, or action name does not start with `<contract>.`.
    Name(String),
    /// A screen key is taken, or its layout does not parse, or an element id lacks the
    /// module's id prefix.
    Screen(String),
    /// A presentation graph does not parse.
    Presentation(String),
    /// Two linked client halves claim the same module key.
    DuplicateModule(String),
    /// A module failed its own setup.
    Module(&'static str),
}

impl core::fmt::Display for ClientRegistryError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::DuplicateKind(k) => write!(f, "extension kind {k} is claimed twice"),
            Self::UndeclaredKind(k) => write!(f, "extension kind {k} is not one of the module's kinds"),
            Self::Name(n) => write!(f, "`{n}` is not named after its module"),
            Self::Screen(why) => write!(f, "screen: {why}"),
            Self::Presentation(why) => write!(f, "presentation graph: {why}"),
            Self::DuplicateModule(k) => write!(f, "module {k} has two client halves"),
            Self::Module(why) => write!(f, "module setup: {why}"),
        }
    }
}

impl std::error::Error for ClientRegistryError {}

/// A module handler refused an intent or could not use a message.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ModuleError {
    /// The payload did not decode with the contract codec.
    Malformed,
    /// The request is not valid now (for example, nothing selected).
    Invalid,
    /// The outbound queue is full.
    Full,
}

impl core::fmt::Display for ModuleError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str(match self {
            Self::Malformed => "the payload does not decode with the contract codec",
            Self::Invalid => "the request is not valid now",
            Self::Full => "the outbound queue is full",
        })
    }
}

impl std::error::Error for ModuleError {}

/// An entity entering or leaving interest (from snapshots).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum EntityChange {
    /// Entered interest with this appearance.
    Entered {
        /// The entity.
        entity: EntityId,
        /// Its appearance id.
        appearance: u32,
    },
    /// Left interest or despawned.
    Removed(EntityId),
}

/// Handles one server-to-client message.
pub type MessageFn = fn(&mut ModuleContext<'_>, kind: u16, payload: &[u8]) -> Result<(), ModuleError>;
/// Handles one UI intent (payload as the widget sent it).
pub type IntentFn = fn(&mut ModuleContext<'_>, payload: Option<&str>) -> Result<(), ModuleError>;
/// Observes one entity change.
pub type EntityFn = fn(&mut ModuleContext<'_>, &EntityChange);
/// Runs when an action is pressed.
pub type ActionFn = fn(&mut ModuleContext<'_>);
/// Runs when the module is enabled or disabled (to reset view-model state).
pub type EnabledFn = fn(&mut ModuleContext<'_>, bool);
/// Sees a server refusal of one of the module's sends before the default handling:
/// `kind`, the envelope `request` the client stamped on that send (0 when untracked), and
/// the reason. Returning true means the module answered it (for example, a command whose
/// own result message is authoritative), so `<namespace>.refusal` is left alone.
/// `FeatureDisabled` always disables the module regardless.
pub type RefusedFn = fn(&mut ModuleContext<'_>, kind: u16, request: u32, reason: ExtensionRefusal) -> bool;

/// A screen a module provides.
#[derive(Clone, Debug)]
pub struct Screen {
    /// The module.
    pub module: &'static str,
    /// The screen key (`<contract>.<name>`).
    pub key: &'static str,
    /// Layout markup (one root element, no theme blocks).
    pub layout: &'static str,
    /// Theme blocks for its styles, if any.
    pub theme: Option<&'static str>,
}

/// An input action a module provides.
#[derive(Clone, Copy, Debug)]
pub struct ModuleAction {
    /// The module.
    pub module: &'static str,
    /// Action name (`<contract>.<name>`).
    pub name: &'static str,
    /// Default key, rebindable.
    pub default_key: Option<KeyCode>,
    /// What pressing it does.
    pub run: ActionFn,
}

struct Installed {
    key: &'static str,
    /// The contract the module implements: the namespace of its screens, intents,
    /// actions, and properties (an override answers to the contract it replaces).
    namespace: String,
    id_prefix: String,
    enabled: bool,
    flags: BTreeMap<String, bool>,
    kinds: Vec<core::ops::RangeInclusive<u16>>,
    state: Box<dyn Any + Send>,
    on_entities: Option<EntityFn>,
    on_enabled: Option<EnabledFn>,
    on_refused: Option<RefusedFn>,
    presentation: Vec<PresentationGraph>,
}

/// Registers one module.
pub struct ClientRegistrar<'a> {
    module: &'a mut Installed,
    handlers: &'a mut BTreeMap<u16, (usize, MessageFn)>,
    intents: &'a mut BTreeMap<&'static str, (usize, IntentFn)>,
    screens: &'a mut Vec<Screen>,
    actions: &'a mut Vec<(usize, ModuleAction)>,
    index: usize,
}

impl ClientRegistrar<'_> {
    /// The module key.
    pub fn key(&self) -> &'static str {
        self.module.key
    }

    /// The contract the module implements: screens, intents, actions, and properties
    /// are named under it (`<contract>.<name>`), so an override keeps the names the
    /// contract defines.
    pub fn namespace(&self) -> &str {
        &self.module.namespace
    }

    /// The module's resolved flags (manifest defaults with the package's overrides).
    pub fn flags(&self) -> &BTreeMap<String, bool> {
        &self.module.flags
    }

    fn named(&self, name: &'static str) -> Result<&'static str, ClientRegistryError> {
        let ok = name
            .strip_prefix(self.module.namespace.as_str())
            .and_then(|rest| rest.strip_prefix('.'))
            .is_some_and(|rest| !rest.is_empty());
        if ok {
            Ok(name)
        } else {
            Err(ClientRegistryError::Name(name.to_owned()))
        }
    }

    /// Declares the extension kinds the module owns (its contract's message ids).
    ///
    /// # Errors
    /// [`ClientRegistryError::DuplicateKind`] when a kind is declared twice (overlaps with
    /// other modules are refused by [`ClientModules::new`]).
    pub fn kinds(&mut self, kinds: core::ops::RangeInclusive<u16>) -> Result<(), ClientRegistryError> {
        if let Some(k) = kinds
            .clone()
            .find(|k| self.handlers.contains_key(k) || self.owns(*k))
        {
            return Err(ClientRegistryError::DuplicateKind(k));
        }
        self.module.kinds.push(kinds);
        Ok(())
    }

    fn owns(&self, kind: u16) -> bool {
        self.module.kinds.iter().any(|r| r.contains(&kind))
    }

    /// Sets the module's view-model state (replacing any earlier state).
    pub fn state<T: Any + Send>(&mut self, state: T) {
        self.module.state = Box::new(state);
    }

    /// Handles server-to-client message `kind` (one of the module's kinds).
    ///
    /// # Errors
    /// [`ClientRegistryError::UndeclaredKind`] or [`ClientRegistryError::DuplicateKind`].
    pub fn on_message(&mut self, kind: u16, handler: MessageFn) -> Result<(), ClientRegistryError> {
        if !self.owns(kind) {
            return Err(ClientRegistryError::UndeclaredKind(kind));
        }
        if self.handlers.insert(kind, (self.index, handler)).is_some() {
            return Err(ClientRegistryError::DuplicateKind(kind));
        }
        Ok(())
    }

    /// Handles the UI intent `name` (`<contract>.<name>`).
    ///
    /// # Errors
    /// [`ClientRegistryError::Name`] for a name outside the module, or one taken.
    pub fn on_intent(&mut self, name: &'static str, handler: IntentFn) -> Result<(), ClientRegistryError> {
        let name = self.named(name)?;
        if self.intents.insert(name, (self.index, handler)).is_some() {
            return Err(ClientRegistryError::Name(name.to_owned()));
        }
        Ok(())
    }

    /// Observes entities entering and leaving interest.
    pub fn on_entities(&mut self, handler: EntityFn) {
        self.module.on_entities = Some(handler);
    }

    /// Runs at start ([`ClientModules::start`], with the starting state) and whenever the
    /// module is enabled or disabled at run time: publish initial or reset properties here.
    pub fn on_enabled(&mut self, handler: EnabledFn) {
        self.module.on_enabled = Some(handler);
    }

    /// Sees refusals of the module's own kinds first (see [`RefusedFn`]).
    pub fn on_refused(&mut self, handler: RefusedFn) {
        self.module.on_refused = Some(handler);
    }

    /// Adds a screen. Its layout must parse, and every element id must start with the
    /// module's id prefix (`std.party` → `std_party_`), so screens never collide.
    ///
    /// # Errors
    /// [`ClientRegistryError::Screen`].
    pub fn screen(
        &mut self,
        key: &'static str,
        layout: &'static str,
        theme: Option<&'static str>,
    ) -> Result<(), ClientRegistryError> {
        let key = self
            .named(key)
            .map_err(|e| ClientRegistryError::Screen(e.to_string()))?;
        if self.screens.iter().any(|s| s.key == key) {
            return Err(ClientRegistryError::Screen(format!("{key} is registered twice")));
        }
        let doc = mantis_ui::markup::parse_layout(layout)
            .map_err(|e| ClientRegistryError::Screen(format!("{key}: {e}")))?;
        if !doc.theme.styles.is_empty() {
            return Err(ClientRegistryError::Screen(format!(
                "{key}: styles belong in the theme source"
            )));
        }
        if let Some(t) = theme {
            let _ = mantis_ui::markup::parse_theme(t)
                .map_err(|e| ClientRegistryError::Screen(format!("{key} theme: {e}")))?;
        }
        let mut stack = vec![&doc.root];
        while let Some(el) = stack.pop() {
            match &el.id {
                Some(id) if id.starts_with(self.module.id_prefix.as_str()) => {}
                Some(id) => {
                    return Err(ClientRegistryError::Screen(format!(
                        "{key}: element id `{id}` must start with `{}`",
                        self.module.id_prefix
                    )));
                }
                None => {}
            }
            stack.extend(el.children.iter());
        }
        self.screens.push(Screen {
            module: self.module.key,
            key,
            layout,
            theme,
        });
        Ok(())
    }

    /// Adds a presentation graph (encoded MPRS), active while the module is enabled.
    ///
    /// # Errors
    /// [`ClientRegistryError::Presentation`].
    pub fn presentation(&mut self, bytes: &[u8]) -> Result<(), ClientRegistryError> {
        let g =
            PresentationGraph::parse(bytes).map_err(|e| ClientRegistryError::Presentation(e.to_string()))?;
        self.module.presentation.push(g);
        Ok(())
    }

    /// Adds an input action in the module's context.
    ///
    /// # Errors
    /// [`ClientRegistryError::Name`].
    pub fn action(
        &mut self,
        name: &'static str,
        default_key: Option<KeyCode>,
        run: ActionFn,
    ) -> Result<(), ClientRegistryError> {
        let name = self.named(name)?;
        if self.actions.iter().any(|(_, a)| a.name == name) {
            return Err(ClientRegistryError::Name(name.to_owned()));
        }
        self.actions.push((
            self.index,
            ModuleAction {
                module: self.module.key,
                name,
                default_key,
                run,
            },
        ));
        Ok(())
    }
}

/// What a handler may touch.
pub struct ModuleContext<'a> {
    key: &'static str,
    namespace: &'a str,
    kinds: &'a [core::ops::RangeInclusive<u16>],
    flags: &'a BTreeMap<String, bool>,
    state: &'a mut (dyn Any + Send),
    props: &'a mut Properties,
    outbound: &'a mut Vec<(ExtensionKind, ModulePayload)>,
    outbound_capacity: usize,
}

/// An outbound payload by value (fixed size, so the queue never allocates).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct ModulePayload {
    len: u16,
    bytes: [u8; PAYLOAD],
}

impl ModulePayload {
    /// The bytes.
    pub fn as_slice(&self) -> &[u8] {
        self.bytes.get(..usize::from(self.len)).unwrap_or(&[])
    }

    fn from_slice(b: &[u8]) -> Option<Self> {
        let mut p = Self {
            len: 0,
            bytes: [0; PAYLOAD],
        };
        p.bytes.get_mut(..b.len())?.copy_from_slice(b);
        p.len = u16::try_from(b.len()).ok()?;
        Some(p)
    }
}

impl ModuleContext<'_> {
    /// The module key.
    pub fn key(&self) -> &'static str {
        self.key
    }

    /// The module's view-model state, if it is a `T`.
    pub fn state<T: Any>(&mut self) -> Option<&mut T> {
        self.state.downcast_mut::<T>()
    }

    /// A resolved manifest flag (false when unknown).
    pub fn flag(&self, name: &str) -> bool {
        self.flags.get(name).copied().unwrap_or(false)
    }

    /// The UI properties, for publishing view-model values.
    pub fn props(&mut self) -> &mut Properties {
        self.props
    }

    /// Sets `<contract>.<name>` to `text`.
    pub fn set_text(&mut self, name: &str, text: &str) {
        let id = self.props.intern(&self.full(name));
        let _ = self.props.set_text(id, text);
    }

    /// Sets `<contract>.<name>` to an integer.
    pub fn set_int(&mut self, name: &str, v: i64) {
        let id = self.props.intern(&self.full(name));
        let _ = self.props.set_int(id, v);
    }

    /// Sets `<contract>.<name>` to a flag.
    pub fn set_bool(&mut self, name: &str, v: bool) {
        let id = self.props.intern(&self.full(name));
        let _ = self.props.set_bool(id, v);
    }

    /// Sets `<contract>.<name>` to any value (lists for rosters and listings).
    pub fn set(&mut self, name: &str, v: mantis_ui::Value) {
        let id = self.props.intern(&self.full(name));
        let _ = self.props.set(id, v);
    }

    fn full(&self, name: &str) -> String {
        let mut s = String::with_capacity(self.namespace.len() + 1 + name.len());
        s.push_str(self.namespace);
        s.push('.');
        s.push_str(name);
        s
    }

    /// Queues a client-to-server message of one of the module's kinds, encoded with its
    /// contract codec.
    ///
    /// # Errors
    /// [`ModuleError::Invalid`] for a kind the module does not own or a payload over
    /// [`PAYLOAD`] bytes, [`ModuleError::Full`] when the queue is full.
    pub fn send<M: Message>(&mut self, message: &M) -> Result<(), ModuleError> {
        let kind = M::ID.0;
        if !self.kinds.iter().any(|r| r.contains(&kind)) {
            return Err(ModuleError::Invalid);
        }
        // Sends are user actions, not per-frame work: encoding through a short-lived
        // buffer is fine here.
        let mut v = Vec::new();
        mantis_core::wire::encode_into(message, &mut v);
        let payload = ModulePayload::from_slice(&v).ok_or(ModuleError::Invalid)?;
        if self.outbound.len() >= self.outbound_capacity {
            return Err(ModuleError::Full);
        }
        self.outbound.push((ExtensionKind(kind), payload));
        Ok(())
    }
}

/// Decodes a contract message for a handler.
///
/// # Errors
/// [`ModuleError::Malformed`].
pub fn decode<M: Message>(payload: &[u8]) -> Result<M, ModuleError> {
    mantis_core::wire::decode_message::<M>(payload).map_err(|_| ModuleError::Malformed)
}

/// Counters. Nothing here is an error at run time.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct ModuleStats {
    /// Messages handled.
    pub messages: u64,
    /// Messages of a kind no module owns.
    pub unknown_kind: u64,
    /// Messages a handler could not decode.
    pub malformed: u64,
    /// Messages or intents for a disabled module (dropped).
    pub disabled: u64,
    /// UI intents handled.
    pub intents: u64,
    /// UI intents refused by their handler.
    pub intents_refused: u64,
    /// Refusals received from the server.
    pub refusals: u64,
    /// Refusals a module's [`RefusedFn`] answered.
    pub refusals_answered: u64,
}

/// What the registry did with a UI intent.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum IntentRoute {
    /// A module handled it.
    Handled,
    /// A module owns it but refused it (or is disabled).
    Refused,
    /// No module owns it (the host handles it).
    NotModule,
}

/// The installed client modules.
pub struct ClientModules {
    modules: Vec<Installed>,
    handlers: BTreeMap<u16, (usize, MessageFn)>,
    intents: BTreeMap<&'static str, (usize, IntentFn)>,
    screens: Vec<Screen>,
    actions: Vec<(usize, ModuleAction)>,
    outbound: Vec<(ExtensionKind, ModulePayload)>,
    stats: ModuleStats,
    character: Option<u64>,
}

impl core::fmt::Debug for ClientModules {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("ClientModules")
            .field("modules", &self.modules.iter().map(|m| m.key).collect::<Vec<_>>())
            .field("stats", &self.stats)
            .finish_non_exhaustive()
    }
}

/// Outbound messages kept at most (the network thread drains them every step).
const OUTBOUND: usize = 64;

impl ClientModules {
    /// Installs the client half of every module in `graph` (graph order). Modules
    /// without a client half (server-only features) are skipped; linked halves outside
    /// the graph (overridden or not used by the package) are left out.
    ///
    /// # Errors
    /// [`ClientRegistryError`]; the client refuses to start.
    pub fn new(graph: &ModuleGraph, linked: &[Arc<dyn ClientModule>]) -> Result<Self, ClientRegistryError> {
        let mut keys: Vec<&str> = linked.iter().map(|l| l.key()).collect();
        keys.sort_unstable();
        if let Some(w) = keys.windows(2).find(|w| matches!(w, [a, b] if a == b)) {
            return Err(ClientRegistryError::DuplicateModule(
                w.first().copied().unwrap_or("").to_owned(),
            ));
        }
        let mut out = Self {
            modules: Vec::new(),
            handlers: BTreeMap::new(),
            intents: BTreeMap::new(),
            screens: Vec::new(),
            actions: Vec::new(),
            outbound: Vec::with_capacity(OUTBOUND),
            stats: ModuleStats::default(),
            character: None,
        };
        for resolved in &graph.modules {
            let Some(half) = linked.iter().find(|l| l.key() == resolved.key) else {
                continue;
            };
            let index = out.modules.len();
            let mut installed = Installed {
                key: half.key(),
                namespace: resolved.contract.clone(),
                id_prefix: format!("{}_", half.key().replace('.', "_")),
                enabled: resolved.enabled,
                flags: resolved.flags.clone(),
                kinds: Vec::new(),
                state: Box::new(()),
                on_entities: None,
                on_enabled: None,
                on_refused: None,
                presentation: Vec::new(),
            };
            let taken: Vec<core::ops::RangeInclusive<u16>> = out
                .modules
                .iter()
                .flat_map(|m: &Installed| m.kinds.iter().cloned())
                .collect();
            {
                let mut r = ClientRegistrar {
                    module: &mut installed,
                    handlers: &mut out.handlers,
                    intents: &mut out.intents,
                    screens: &mut out.screens,
                    actions: &mut out.actions,
                    index,
                };
                half.register(&mut r)?;
            }
            for range in &installed.kinds {
                if let Some(k) = range.clone().find(|k| taken.iter().any(|t| t.contains(k))) {
                    return Err(ClientRegistryError::DuplicateKind(k));
                }
            }
            out.modules.push(installed);
        }
        Ok(out)
    }

    /// Installed module keys, in order.
    pub fn keys(&self) -> impl Iterator<Item = &'static str> + '_ {
        self.modules.iter().map(|m| m.key)
    }

    /// Every installed module's key, contract, and element id prefix (`std.party` →
    /// `std_party_`): names client mods may not take ([`crate::mods`]).
    pub fn names(&self) -> impl Iterator<Item = (&'static str, &str, &str)> + '_ {
        self.modules
            .iter()
            .map(|m| (m.key, m.namespace.as_str(), m.id_prefix.as_str()))
    }

    /// Counters.
    pub fn stats(&self) -> ModuleStats {
        self.stats
    }

    /// Whether `key` is installed and enabled.
    pub fn is_enabled(&self, key: &str) -> bool {
        self.modules
            .iter()
            .any(|m| (m.key == key || m.namespace == key) && m.enabled)
    }

    /// Publishes every module's `<contract>.enabled`, `<contract>.unavailable` (its
    /// negation, for "unavailable" notices), and `<contract>.flag.<name>` properties.
    pub fn publish_flags(&self, props: &mut Properties) {
        for m in &self.modules {
            let id = props.intern(&format!("{}.enabled", m.namespace));
            let _ = props.set_bool(id, m.enabled);
            let id = props.intern(&format!("{}.unavailable", m.namespace));
            let _ = props.set_bool(id, !m.enabled);
            for (name, on) in &m.flags {
                let id = props.intern(&format!("{}.flag.{name}", m.namespace));
                let _ = props.set_bool(id, *on && m.enabled);
            }
        }
    }

    /// Records the player's character identity (from the session's Welcome) and publishes
    /// it as the `client.character` property (text), for screens that mark "you".
    pub fn set_character(&mut self, character: u64, props: &mut Properties) {
        self.character = Some(character);
        let id = props.intern("client.character");
        let _ = props.set_text(id, &character.to_string());
    }

    /// The player's character identity, once the session was accepted.
    pub fn character(&self) -> Option<u64> {
        self.character
    }

    /// Starts every module: publishes the flags, then runs each module's `on_enabled`
    /// with its starting state, so view models publish their initial properties before
    /// the first frame (an unset flag would otherwise read as true in layouts).
    pub fn start(&mut self, props: &mut Properties) {
        self.publish_flags(props);
        for m in &mut self.modules {
            if let Some(f) = m.on_enabled {
                let enabled = m.enabled;
                f(&mut Self::context(m, props, &mut self.outbound), enabled);
            }
        }
    }

    fn context<'a>(
        m: &'a mut Installed,
        props: &'a mut Properties,
        outbound: &'a mut Vec<(ExtensionKind, ModulePayload)>,
    ) -> ModuleContext<'a> {
        ModuleContext {
            key: m.key,
            namespace: &m.namespace,
            kinds: &m.kinds,
            flags: &m.flags,
            state: m.state.as_mut(),
            props,
            outbound,
            outbound_capacity: OUTBOUND,
        }
    }

    /// Enables or disables a module at run time (and republishes its properties).
    pub fn set_enabled(&mut self, key: &str, enabled: bool, props: &mut Properties) {
        let Some(m) = self
            .modules
            .iter_mut()
            .find(|m| m.key == key || m.namespace == key)
        else {
            return;
        };
        if m.enabled == enabled {
            return;
        }
        m.enabled = enabled;
        if let Some(f) = m.on_enabled {
            f(&mut Self::context(m, props, &mut self.outbound), enabled);
        }
        self.publish_flags(props);
    }

    fn owner(&self, kind: u16) -> Option<usize> {
        self.modules
            .iter()
            .position(|m| m.kinds.iter().any(|r| r.contains(&kind)))
    }

    /// Routes a server-to-client module message to the module that owns its kind.
    pub fn on_message(&mut self, kind: ExtensionKind, payload: &[u8], props: &mut Properties) {
        let Some(&(index, handler)) = self.handlers.get(&kind.0) else {
            self.stats.unknown_kind += 1;
            return;
        };
        let Some(m) = self.modules.get_mut(index) else {
            return;
        };
        if !m.enabled {
            self.stats.disabled += 1;
            return;
        }
        match handler(&mut Self::context(m, props, &mut self.outbound), kind.0, payload) {
            Ok(()) => self.stats.messages += 1,
            Err(_) => self.stats.malformed += 1,
        }
    }

    /// As [`ClientModules::on_refused_tracked`] for an untracked send (request 0).
    pub fn on_refused(&mut self, kind: ExtensionKind, reason: ExtensionRefusal, props: &mut Properties) {
        self.on_refused_tracked(kind, 0, reason, props);
    }

    /// Applies a server refusal of one of a module's kinds: `FeatureDisabled` disables
    /// the module; for other reasons the module's [`RefusedFn`] runs first, and unless it
    /// answers, `<key>.refusal` is set for its screen to show. `request` is the envelope
    /// request the refused send carried (0 when untracked).
    pub fn on_refused_tracked(
        &mut self,
        kind: ExtensionKind,
        request: u32,
        reason: ExtensionRefusal,
        props: &mut Properties,
    ) {
        self.stats.refusals += 1;
        let Some(index) = self.owner(kind.0) else {
            self.stats.unknown_kind += 1;
            return;
        };
        let Some(key) = self.modules.get(index).map(|m| m.namespace.clone()) else {
            return;
        };
        if reason != ExtensionRefusal::FeatureDisabled
            && let Some(m) = self.modules.get_mut(index)
            && let Some(hook) = m.on_refused
            && hook(
                &mut Self::context(m, props, &mut self.outbound),
                kind.0,
                request,
                reason,
            )
        {
            self.stats.refusals_answered += 1;
            return;
        }
        if reason == ExtensionRefusal::FeatureDisabled {
            self.set_enabled(&key, false, props);
        } else {
            // A reason this build does not know (the contract enum is non-exhaustive) reads
            // as a refusal all the same.
            let text = match reason {
                ExtensionRefusal::NotAllowed => "not allowed",
                ExtensionRefusal::Invalid => "invalid",
                _ => "refused",
            };
            let id = props.intern(&format!("{key}.refusal"));
            let _ = props.set_text(id, text);
        }
    }

    /// Routes a UI intent by name.
    pub fn on_intent(&mut self, name: &str, payload: Option<&str>, props: &mut Properties) -> IntentRoute {
        let Some(&(index, handler)) = self.intents.get(name) else {
            return IntentRoute::NotModule;
        };
        let Some(m) = self.modules.get_mut(index) else {
            return IntentRoute::NotModule;
        };
        if !m.enabled {
            self.stats.disabled += 1;
            return IntentRoute::Refused;
        }
        if handler(&mut Self::context(m, props, &mut self.outbound), payload).is_ok() {
            self.stats.intents += 1;
            IntentRoute::Handled
        } else {
            self.stats.intents_refused += 1;
            IntentRoute::Refused
        }
    }

    /// Hands an entity change to every enabled module observing entities.
    pub fn on_entities(&mut self, change: &EntityChange, props: &mut Properties) {
        for m in &mut self.modules {
            if let (true, Some(f)) = (m.enabled, m.on_entities) {
                f(&mut Self::context(m, props, &mut self.outbound), change);
            }
        }
    }

    /// Runs the actions pressed this frame (`pressed(name)` says whether the action
    /// named `name` was pressed), for enabled modules.
    pub fn on_actions(&mut self, mut pressed: impl FnMut(&str) -> bool, props: &mut Properties) {
        for (index, action) in &self.actions {
            if !pressed(action.name) {
                continue;
            }
            if let Some(m) = self.modules.get_mut(*index).filter(|m| m.enabled) {
                (action.run)(&mut Self::context(m, props, &mut self.outbound));
            }
        }
    }

    /// Defines every module action as a button action bound to its default key in `ctx`
    /// (rebindable afterwards like any action). Returns the action ids by name, for
    /// [`ClientModules::on_actions`].
    ///
    /// # Errors
    /// [`crate::input::InputError`] when a name or binding conflicts with the host's.
    pub fn define_actions(
        &self,
        actions: &mut crate::input::action::ActionTable,
        contexts: &mut crate::input::binding::ContextTable,
        ctx: crate::input::binding::ContextId,
    ) -> Result<Vec<(&'static str, crate::input::action::ActionId)>, crate::input::InputError> {
        let mut ids = Vec::with_capacity(self.actions.len());
        for (_, a) in &self.actions {
            let id = actions.define(a.name, crate::input::action::ActionKind::Button)?;
            if let Some(key) = a.default_key {
                contexts.bind(
                    actions,
                    ctx,
                    id,
                    crate::input::binding::Binding::Button(crate::input::device::ButtonSource::Key(key)),
                )?;
            }
            ids.push((a.name, id));
        }
        Ok(ids)
    }

    /// Every module action (to define in the input tables at start-up).
    pub fn actions(&self) -> impl Iterator<Item = &ModuleAction> {
        self.actions.iter().map(|(_, a)| a)
    }

    /// Every screen.
    pub fn screens(&self) -> &[Screen] {
        &self.screens
    }

    /// The presentation graphs of every enabled module.
    pub fn presentation(&self) -> impl Iterator<Item = &PresentationGraph> {
        self.modules
            .iter()
            .filter(|m| m.enabled)
            .flat_map(|m| m.presentation.iter())
    }

    /// Hands every queued client-to-server message to `f` (the network session sends
    /// them as `Extension` intents).
    pub fn drain_outbound(&mut self, mut f: impl FnMut(ExtensionKind, &[u8])) {
        for (kind, payload) in self.outbound.drain(..) {
            f(kind, payload.as_slice());
        }
    }

    /// One layout holding the `open` screens (by key, in order) under a full-size root,
    /// and the theme source of every screen, for `Ui::reload`. Unknown keys are skipped.
    /// The connection notice panel closes the layout (shown while
    /// `client.connection.has_notice`; [`crate::ui_layer::UiLayer::set_connection`]).
    pub fn compose(&self, open: &[&str]) -> (String, String) {
        self.compose_with(open, "", "")
    }

    /// [`ClientModules::compose`] with `extra_layout` (more screens, already checked)
    /// appended under the same root after the module screens, and `extra_theme` before
    /// the module themes (so a module style always wins a name clash): client mods'
    /// screens ([`crate::mods`]).
    pub fn compose_with(&self, open: &[&str], extra_layout: &str, extra_theme: &str) -> (String, String) {
        let mut layout =
            String::from("panel id=modules_root width=grow height=grow direction=column gap=8 padding=8 {\n");
        let mut theme = String::from(extra_theme);
        for key in open {
            if let Some(s) = self.screens.iter().find(|s| s.key == *key) {
                layout.push_str(s.layout);
                layout.push('\n');
            }
        }
        layout.push_str(extra_layout);
        layout.push_str(CONNECTION_PANEL);
        layout.push_str("}\n");
        for s in &self.screens {
            if let Some(t) = s.theme {
                theme.push_str(t);
                theme.push('\n');
            }
        }
        (layout, theme)
    }
}

/// The connection notice: reconnecting, or why the server refused.
const CONNECTION_PANEL: &str = r#"panel id=client_connection_notice direction=column visible="client.connection.has_notice" {
  text id=client_connection_text bind="client.connection.notice"
}
"#;

/// A module event crossing from the network thread to the render thread.
#[derive(Clone, Copy, Debug)]
pub enum ModuleInbound {
    /// A server-to-client module message.
    Message(mantis_adapter_contract::ExtensionMessage),
    /// The server refused one of the client's module messages.
    Refused(ExtensionKind, u32, ExtensionRefusal),
    /// An entity entered or left interest.
    Entity(EntityChange),
    /// The session was accepted; this is the player's character identity.
    Character(u64),
    /// The server announced whether a module is enabled (key, at most 64 bytes).
    Feature {
        /// Key bytes.
        key: [u8; 64],
        /// Key length.
        len: u8,
        /// Enabled.
        enabled: bool,
    },
    /// The cell's permitted client mods and their tier (sent on joining a cell, on every
    /// transfer, and on every tier change); client mod loading applies it at once.
    Permitted(mantis_adapter_contract::PermittedModules),
}

/// Network-thread end of the module link.
#[derive(Debug)]
pub struct ModuleNetLink {
    character: Option<u64>,
    /// A permitted list the render thread has not taken yet because the channel was
    /// full: offered again first on every pump and never dropped, because a lowered
    /// tier must always arrive.
    permitted: Option<mantis_adapter_contract::PermittedModules>,
    to_ui: std::sync::mpsc::SyncSender<ModuleInbound>,
    from_ui: std::sync::mpsc::Receiver<(ExtensionKind, ModulePayload)>,
    dropped: u64,
}

/// Render-thread end of the module link.
#[derive(Debug)]
pub struct ModuleUiLink {
    from_net: std::sync::mpsc::Receiver<ModuleInbound>,
    to_net: std::sync::mpsc::SyncSender<(ExtensionKind, ModulePayload)>,
    dropped: u64,
    permitted: Option<mantis_adapter_contract::PermittedModules>,
}

/// A bounded two-way link between the network session and the client modules. Neither
/// end blocks or allocates per item; a full queue drops and counts.
pub fn module_link(capacity: usize) -> (ModuleNetLink, ModuleUiLink) {
    let (to_ui, from_net) = std::sync::mpsc::sync_channel(capacity.max(1));
    let (to_net, from_ui) = std::sync::mpsc::sync_channel(capacity.max(1));
    (
        ModuleNetLink {
            character: None,
            permitted: None,
            to_ui,
            from_ui,
            dropped: 0,
        },
        ModuleUiLink {
            from_net,
            to_net,
            dropped: 0,
            permitted: None,
        },
    )
}

impl ModuleNetLink {
    /// Forwards the session's module traffic to the render thread and sends what the
    /// modules queued.
    pub fn pump<T: mantis_adapter_contract::Transport>(
        &mut self,
        session: &mut crate::net::NativeSession<T>,
    ) {
        // The character first: module messages may refer to it.
        if let crate::net::SessionState::Welcomed { character, .. } = session.state()
            && self.character != Some(character)
        {
            self.character = Some(character);
            if self.to_ui.try_send(ModuleInbound::Character(character)).is_err() {
                self.dropped += 1;
            }
        }
        // The permitted list next, so mods are re-tiered before anything else is applied.
        // Only the latest list matters; one the channel had no room for is kept and
        // offered again, never dropped.
        if let Some(p) = session.take_permitted() {
            self.permitted = Some(*p);
        }
        if let Some(p) = self.permitted.take()
            && self.to_ui.try_send(ModuleInbound::Permitted(p)).is_err()
        {
            self.permitted = Some(p);
        }
        let (tx, dropped) = (&self.to_ui, &mut self.dropped);
        let mut forward = |m: ModuleInbound| {
            if tx.try_send(m).is_err() {
                *dropped += 1;
            }
        };
        session.drain_extensions(|m| forward(ModuleInbound::Message(*m)));
        session.drain_tracked_refusals(|kind, request, reason| {
            forward(ModuleInbound::Refused(kind, request, reason));
        });
        session.drain_entity_changes(|c| forward(ModuleInbound::Entity(*c)));
        session.drain_feature_states(|key, enabled| {
            let mut bytes = [0u8; 64];
            let len = key.len().min(64);
            if let (Some(dst), Some(src)) = (bytes.get_mut(..len), key.as_bytes().get(..len)) {
                dst.copy_from_slice(src);
            }
            forward(ModuleInbound::Feature {
                key: bytes,
                len: u8::try_from(len).unwrap_or(0),
                enabled,
            });
        });
        while let Ok((kind, payload)) = self.from_ui.try_recv() {
            let _ = session.send_extension(kind, payload.as_slice());
        }
    }

    /// Items dropped because the render thread fell behind.
    pub fn dropped(&self) -> u64 {
        self.dropped
    }
}

impl ModuleUiLink {
    /// Applies everything the network delivered, then hands the modules' outbound
    /// messages to the network thread.
    pub fn pump(&mut self, modules: &mut ClientModules, props: &mut Properties) {
        while let Ok(m) = self.from_net.try_recv() {
            match m {
                ModuleInbound::Message(msg) => {
                    // The contract's bounded array holds options; copy into a flat buffer.
                    let mut buf = [0u8; PAYLOAD];
                    let len = msg.payload.len().min(PAYLOAD);
                    for (dst, src) in buf.iter_mut().zip(msg.payload.iter()) {
                        *dst = *src;
                    }
                    modules.on_message(msg.kind, buf.get(..len).unwrap_or(&[]), props);
                }
                ModuleInbound::Refused(kind, request, reason) => {
                    modules.on_refused_tracked(kind, request, reason, props);
                }
                ModuleInbound::Entity(change) => modules.on_entities(&change, props),
                ModuleInbound::Character(c) => modules.set_character(c, props),
                ModuleInbound::Feature { key, len, enabled } => {
                    if let Some(k) = key
                        .get(..usize::from(len))
                        .and_then(|b| core::str::from_utf8(b).ok())
                    {
                        modules.set_enabled(k, enabled, props);
                    }
                }
                ModuleInbound::Permitted(p) => self.permitted = Some(p),
            }
        }
        let (tx, dropped) = (&self.to_net, &mut self.dropped);
        modules.drain_outbound(|kind, payload| {
            if let Some(p) = ModulePayload::from_slice(payload)
                && tx.try_send((kind, p)).is_ok()
            {
                return;
            }
            *dropped += 1;
        });
    }

    /// Outbound messages dropped because the network thread fell behind.
    pub fn dropped(&self) -> u64 {
        self.dropped
    }

    /// The permitted-mods list the last [`ModuleUiLink::pump`] received, once (the latest
    /// when several arrived). Client mod loading applies it before running any mod.
    pub fn take_permitted(&mut self) -> Option<mantis_adapter_contract::PermittedModules> {
        self.permitted.take()
    }
}

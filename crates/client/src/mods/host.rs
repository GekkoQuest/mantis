//! Running mods: one Luau VM per mod at the tier the server grants ([`ModHost`]).

use std::cell::RefCell;

use mantis_adapter_contract::{ModTier, ModuleEntry, PermittedModules};
use mantis_audio::{AudioEvent, SoundId, VoiceHandle};
use mantis_core::ecs::EntityId;
use mantis_core::hash::StableHasher;
use mantis_core::module::{ModuleGraph, is_valid_key};
use mantis_core::rng::Seed;
use mantis_core::time::Tick;
use mantis_core::wire::{Decoder, Encoder, WireString};
use mantis_script::{Api, Limits, ScriptValue, ScriptVm, ScriptWrapper, Tier, Tiers};
use mantis_ui::{ListItem, Properties, Value};

use super::ModNotice;
use super::package::{ModPackage, own_intent, prefix_of};
use crate::modules::{ClientModules, IntentRoute};
use crate::threads::audio::AudioSender;

/// Most mods one client runs (the handshake's list bound).
pub const MAX_MODS: usize = 32;
/// Intents one automation mod may emit per tick; more are refused and counted.
pub const INTENTS_PER_TICK: usize = 8;
/// Widget events queued for one mod between ticks; more are dropped and counted.
pub const EVENTS_PER_TICK: usize = 32;
/// Sounds one mod may start per tick.
pub const SOUNDS_PER_TICK: usize = 4;

/// Default limits for a mod VM: a smaller memory cap than a server cell's scripts.
pub const MOD_LIMITS: Limits = Limits {
    budget: 20_000,
    memory: 8 << 20,
    gc_steps: 2,
};

/// What a package offers mods: the keys it permits (`client_mods`) and its resolved
/// module graph (the names mods may not take, and the contracts they may depend on).
#[derive(Clone, Copy, Debug)]
pub struct ModEnvironment<'a> {
    /// Mod keys the package permits.
    pub permitted: &'a [String],
    /// The package's resolved modules.
    pub graph: &'a ModuleGraph,
}

/// Where a mod is.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ModState {
    /// Refused at load; never announced or run.
    Refused,
    /// Loaded and announced; waiting for the server's permitted list.
    Waiting,
    /// Running at this tier.
    Running(ModTier),
    /// Stopped by the server's list (not permitted here, or over-tier).
    Stopped,
}

/// Counters.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct ModStats {
    /// Mod ticks run (one per mod per [`ModHost::step`]).
    pub ticks: u64,
    /// Script calls that failed (errors, budget exhaustion).
    pub script_errors: u64,
    /// Widget intents delivered to their own mod's VM.
    pub widget_events: u64,
    /// Widget events dropped because a mod's queue was full.
    pub widget_events_dropped: u64,
    /// Intents automation mods emitted (by `host.intent` or a widget) that a module
    /// handled.
    pub intents_handled: u64,
    /// Intents from mods that were refused: at the presentation tier, from a mod not
    /// running, over the per-tick limit, refused by the module, or owned by no module.
    pub intents_refused: u64,
    /// Of those, intents refused because the mod runs at the presentation tier.
    pub presentation_refused: u64,
    /// Sounds started.
    pub sounds: u64,
    /// Permitted lists applied.
    pub permitted: u64,
    /// Mods restarted at a lower tier.
    pub demotions: u64,
    /// Mods restarted at a higher tier.
    pub promotions: u64,
    /// Mods stopped by a permitted list.
    pub stops: u64,
}

/// What [`ModHost::on_widget_intent`] did with a widget's intent.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ModRoute {
    /// Not a mod's widget: the client routes it as usual.
    NotMod,
    /// The mod's own intent, delivered to its VM.
    Delivered,
    /// An automation mod's intent, routed to the modules with this result.
    Forwarded(IntentRoute),
    /// Refused (presentation tier, or the mod is not running).
    Refused,
}

struct Slot {
    package: ModPackage,
    prefix: String,
    state: ModState,
    vm: Option<ScriptVm>,
    events: Vec<(String, ScriptValue)>,
    /// Why the mod was refused or stopped, or how it runs; shown as a notice.
    notice: Option<String>,
    /// The latest script error.
    error: Option<String>,
}

/// One tick's outputs of a mod VM, collected by its host functions.
#[derive(Default)]
struct Outputs {
    intents: Vec<(String, Option<String>)>,
    sounds: Vec<SoundId>,
    over_limit: u64,
}

/// The client's mods.
pub struct ModHost {
    slots: Vec<Slot>,
    folder_notices: Vec<ModNotice>,
    tier: Option<ModTier>,
    tick: u64,
    limits: Limits,
    wrapper: Option<ScriptWrapper>,
    pending: Vec<(usize, String, Option<String>)>,
    sounds: Vec<(String, SoundId)>,
    audio: Option<AudioSender<AudioEvent>>,
    layout_version: u64,
    dirty: bool,
    stats: ModStats,
}

impl core::fmt::Debug for ModHost {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("ModHost")
            .field(
                "mods",
                &self
                    .slots
                    .iter()
                    .map(|s| (s.package.key.as_str(), s.state))
                    .collect::<Vec<_>>(),
            )
            .field("stats", &self.stats)
            .finish_non_exhaustive()
    }
}

fn script_tier(t: ModTier) -> Tier {
    match t {
        ModTier::Presentation => Tier::Presentation,
        ModTier::Automation => Tier::Automation,
    }
}

fn tier_name(t: ModTier) -> &'static str {
    match t {
        ModTier::Presentation => "presentation",
        ModTier::Automation => "automation",
    }
}

/// Why a mod cannot run in this package, if it cannot.
fn environment_refusal(m: &ModPackage, env: &ModEnvironment<'_>, earlier: &[Slot]) -> Option<String> {
    if !env.permitted.contains(&m.key) {
        return Some("not permitted by this package (`client_mods` in package.toml)".to_owned());
    }
    let prefix = prefix_of(&m.key);
    if earlier
        .iter()
        .any(|s| s.package.key == m.key && s.state != ModState::Refused)
    {
        return Some(format!("another mod already has the key `{}`", m.key));
    }
    for r in &env.graph.modules {
        if r.key == m.key || r.contract == m.key {
            return Some(format!("module `{}` of this package has this key", r.key));
        }
        let theirs = prefix_of(&r.key);
        if theirs.starts_with(&prefix) || prefix.starts_with(&theirs) {
            return Some(format!(
                "its element ids (`{prefix}`) would overlap module `{}`'s (`{theirs}`)",
                r.key
            ));
        }
    }
    for s in earlier.iter().filter(|s| s.state != ModState::Refused) {
        if s.prefix.starts_with(&prefix) || prefix.starts_with(&s.prefix) {
            return Some(format!(
                "its element ids (`{prefix}`) would overlap mod `{}`'s (`{}`)",
                s.package.key, s.prefix
            ));
        }
    }
    for d in &m.dependencies {
        let met = env
            .graph
            .modules
            .iter()
            .any(|r| r.contract == d.contract && r.enabled && r.version.satisfies(d.version));
        if !met {
            return Some(format!(
                "needs `{}` {}, which this package does not provide",
                d.contract, d.version
            ));
        }
    }
    None
}

/// A UI value as a script value: text, number, flag, or a list's length.
fn script_value(v: Option<&Value>) -> ScriptValue {
    match v {
        None => ScriptValue::Nil,
        Some(Value::Text(s)) => ScriptValue::Str(s.clone()),
        #[allow(clippy::cast_precision_loss)] // Script numbers are doubles.
        Some(Value::Int(i)) => ScriptValue::Number(*i as f64),
        Some(Value::Fixed { value, decimals }) => {
            let scale = 10i64.checked_pow(u32::from(*decimals)).unwrap_or(i64::MAX);
            #[allow(clippy::cast_precision_loss)] // Script numbers are doubles.
            ScriptValue::Number(*value as f64 / scale as f64)
        }
        Some(Value::Bool(b)) => ScriptValue::Bool(*b),
        #[allow(clippy::cast_precision_loss)] // Lists are far shorter than 2^53.
        Some(Value::List(items)) => ScriptValue::Number(items.len() as f64),
    }
}

fn arg_str(args: &[ScriptValue], i: usize, what: &str) -> Result<String, String> {
    args.get(i)
        .and_then(ScriptValue::as_str)
        .map(str::to_owned)
        .ok_or_else(|| format!("{what}: expected a string"))
}

/// True for a property name a mod may write under its key.
fn is_property_name(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 64
        && s.split('.').all(|seg| {
            !seg.is_empty()
                && seg
                    .chars()
                    .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
        })
}

/// Sets property `id` from a script value.
fn set_property(props: &mut Properties, id: mantis_ui::PropertyId, v: &ScriptValue) {
    match v {
        ScriptValue::Nil => props.clear(id),
        ScriptValue::Bool(b) => {
            let _ = props.set_bool(id, *b);
        }
        ScriptValue::Str(s) => {
            let _ = props.set_text(id, s);
        }
        ScriptValue::Entity(e) => {
            let _ = props.set_text(id, &e.to_bits().to_string());
        }
        ScriptValue::Number(x) => {
            const EXACT: f64 = 9_007_199_254_740_992.0; // 2^53
            if x.fract() == 0.0 && x.abs() < EXACT {
                #[allow(clippy::cast_possible_truncation)] // Whole and below 2^53: exact.
                let _ = props.set_int(id, *x as i64);
            } else if x.is_finite() && x.abs() < EXACT / 1000.0 {
                #[allow(clippy::cast_possible_truncation)] // Bounded just above.
                let _ = props.set_fixed(id, (x * 1000.0).round() as i64, 3);
            } else {
                let _ = props.set_text(id, "-");
            }
        }
    }
}

impl ModHost {
    /// Mods `packages` (from [`super::scan`]), checked against the package in `env`, with
    /// `folder_notices` (folders that did not load) kept as notices. Nothing runs until
    /// the first permitted list ([`ModHost::apply_permitted`]).
    pub fn new(packages: Vec<ModPackage>, folder_notices: Vec<ModNotice>, env: &ModEnvironment<'_>) -> Self {
        let mut slots: Vec<Slot> = Vec::with_capacity(packages.len());
        for (i, package) in packages.into_iter().enumerate() {
            let refusal = if i >= MAX_MODS {
                Some(format!("only {MAX_MODS} mods may run"))
            } else {
                environment_refusal(&package, env, &slots)
            };
            slots.push(Slot {
                prefix: package.id_prefix(),
                state: if refusal.is_some() {
                    ModState::Refused
                } else {
                    ModState::Waiting
                },
                package,
                vm: None,
                events: Vec::new(),
                notice: refusal.map(|r| format!("not loaded: {r}")),
                error: None,
            });
        }
        Self {
            slots,
            folder_notices,
            tier: None,
            tick: 0,
            limits: MOD_LIMITS,
            wrapper: None,
            pending: Vec::new(),
            sounds: Vec::new(),
            audio: None,
            layout_version: 1,
            dirty: true,
            stats: ModStats::default(),
        }
    }

    /// Replaces the VM limits for mods started from now on.
    pub fn set_limits(&mut self, limits: Limits) {
        self.limits = limits;
    }

    /// Wraps every mod script call (the allocation harness's exempt scope in tests).
    pub fn set_wrapper(&mut self, wrapper: ScriptWrapper) {
        self.wrapper = Some(wrapper);
        for vm in self.slots.iter_mut().filter_map(|s| s.vm.as_mut()) {
            vm.set_wrapper(wrapper);
        }
    }

    /// The UI sounds `host.play` may start, by name.
    pub fn set_sounds(&mut self, sounds: &[(&str, SoundId)]) {
        self.sounds = sounds.iter().map(|(n, s)| ((*n).to_owned(), *s)).collect();
    }

    /// Where `host.play` sends its sounds.
    pub fn set_audio(&mut self, audio: Option<AudioSender<AudioEvent>>) {
        self.audio = audio;
    }

    /// Counters.
    pub fn stats(&self) -> ModStats {
        self.stats
    }

    /// The tier of the last permitted list (`None` before one arrived).
    pub fn granted(&self) -> Option<ModTier> {
        self.tier
    }

    /// Every mod key with its state, in load order.
    pub fn states(&self) -> impl Iterator<Item = (&str, ModState)> {
        self.slots.iter().map(|s| (s.package.key.as_str(), s.state))
    }

    /// The state of mod `key`.
    pub fn state(&self, key: &str) -> Option<ModState> {
        self.slots.iter().find(|s| s.package.key == key).map(|s| s.state)
    }

    /// The mods announced in `Hello`: every mod that loaded and passed the package's
    /// checks, with its content hash.
    pub fn hello_modules(&self) -> Vec<ModuleEntry> {
        self.slots
            .iter()
            .filter(|s| s.state != ModState::Refused)
            .filter_map(|s| {
                Some(ModuleEntry {
                    name: WireString::new(&s.package.key)?,
                    hash: s.package.hash,
                })
            })
            .collect()
    }

    /// Every notice, folders first, then mods in load order.
    pub fn notices(&self) -> Vec<ModNotice> {
        let mut out = self.folder_notices.clone();
        for s in &self.slots {
            for text in [&s.notice, &s.error].into_iter().flatten() {
                out.push(ModNotice {
                    module: s.package.key.clone(),
                    text: text.clone(),
                });
            }
        }
        out
    }

    /// Bumped whenever the set of drawn screens changes (the UI recomposes).
    pub fn layout_version(&self) -> u64 {
        self.layout_version
    }

    /// The layout of every running mod's screens plus the mod notices panel, and the
    /// theme of the running mods, to compose under the module root
    /// ([`ClientModules::compose_with`]).
    pub fn compose(&self) -> (String, String) {
        let mut layout = String::new();
        let mut theme = String::new();
        if self.slots.is_empty() && self.folder_notices.is_empty() {
            return (layout, theme);
        }
        layout.push_str(NOTICES_PANEL);
        for s in self
            .slots
            .iter()
            .filter(|s| matches!(s.state, ModState::Running(_)))
        {
            for screen in &s.package.screens {
                layout.push_str(&screen.layout);
                layout.push('\n');
            }
            if let Some(t) = &s.package.theme {
                theme.push_str(t);
                theme.push('\n');
            }
        }
        (layout, theme)
    }

    fn change(&mut self) {
        self.layout_version += 1;
        self.dirty = true;
    }

    /// Applies the cell's permitted list at once: stops mods it does not name, demotes or
    /// stops automation mods under a presentation-only list, promotes demoted mods when
    /// automation returns, starts waiting mods, and drops every queued intent of a mod
    /// that no longer runs at the automation tier.
    pub fn apply_permitted(&mut self, p: &PermittedModules) {
        self.stats.permitted += 1;
        self.tier = Some(p.tier);
        let mut changed = false;
        for index in 0..self.slots.len() {
            changed |= self.retier(index, p);
        }
        let slots = &self.slots;
        self.pending.retain(|(i, _, _)| {
            slots
                .get(*i)
                .is_some_and(|s| s.state == ModState::Running(ModTier::Automation))
        });
        if changed {
            self.change();
        }
        self.dirty = true;
    }

    /// Re-evaluates one mod against `p`. Returns whether its state changed.
    fn retier(&mut self, index: usize, p: &PermittedModules) -> bool {
        let Some(slot) = self.slots.get(index) else {
            return false;
        };
        if slot.state == ModState::Refused {
            return false;
        }
        let named = p.modules.iter().any(|k| k.as_str() == slot.package.key);
        let target = match (named, slot.package.tier, p.tier) {
            (false, _, _) => Err("stopped: not permitted in this instance".to_owned()),
            (true, ModTier::Presentation, _) => Ok((ModTier::Presentation, None)),
            (true, ModTier::Automation, ModTier::Automation) => Ok((ModTier::Automation, None)),
            (true, ModTier::Automation, ModTier::Presentation) if slot.package.demote => Ok((
                ModTier::Presentation,
                Some("running at the presentation tier: this instance does not permit automation"),
            )),
            (true, ModTier::Automation, ModTier::Presentation) => {
                Err("stopped: needs the automation tier; this instance permits presentation only".to_owned())
            }
        };
        let before = slot.state;
        match target {
            Err(why) => {
                self.stop(index, why);
            }
            Ok((tier, note)) => {
                if before != ModState::Running(tier) {
                    self.start(index, tier);
                }
                if let Some(s) = self.slots.get_mut(index)
                    && matches!(s.state, ModState::Running(_))
                {
                    s.notice = note.map(str::to_owned);
                }
            }
        }
        let after = self.slots.get(index).map(|s| s.state);
        if let (ModState::Running(a), Some(ModState::Running(b))) = (before, after) {
            if a == ModTier::Automation && b == ModTier::Presentation {
                self.stats.demotions += 1;
            } else if a == ModTier::Presentation && b == ModTier::Automation {
                self.stats.promotions += 1;
            }
        }
        after != Some(before)
    }

    fn stop(&mut self, index: usize, why: String) {
        let Some(s) = self.slots.get_mut(index) else {
            return;
        };
        if matches!(s.state, ModState::Running(_)) {
            self.stats.stops += 1;
        }
        s.state = ModState::Stopped;
        s.vm = None;
        s.events.clear();
        s.notice = Some(why);
    }

    /// (Re)starts a mod's VM at `tier`, carrying its scripts' state over from a running
    /// VM. A script that does not load stops the mod with a notice.
    fn start(&mut self, index: usize, tier: ModTier) {
        let (limits, wrapper) = (self.limits, self.wrapper);
        let Some(s) = self.slots.get_mut(index) else {
            return;
        };
        let mut saved = Vec::new();
        if let Some(old) = s.vm.take()
            && old.save(&mut Encoder::new(&mut saved)).is_err()
        {
            saved.clear();
        }
        let mut seed = StableHasher::new();
        seed.write(s.package.key.as_bytes());
        let built = ScriptVm::new(script_tier(tier), limits, Seed(seed.finish()))
            .map_err(|e| e.to_string())
            .and_then(|mut vm| {
                if let Some(w) = wrapper {
                    vm.set_wrapper(w);
                }
                for (name, source) in &s.package.scripts {
                    vm.load(name, source, EntityId::new(0, 0))
                        .map_err(|e| format!("script `{name}`: {e}"))?;
                }
                if !saved.is_empty() {
                    let sources: Vec<(mantis_core::content::ContentHash, String)> = s
                        .package
                        .scripts
                        .iter()
                        .map(|(_, src)| (mantis_core::content::ContentHash::of(src.as_bytes()), src.clone()))
                        .collect();
                    vm.restore(&mut Decoder::new(&saved), &|h| {
                        sources.iter().find(|(k, _)| k == h).map(|(_, v)| v.clone())
                    })?;
                }
                Ok(vm)
            });
        match built {
            Ok(vm) => {
                s.vm = Some(vm);
                s.state = ModState::Running(tier);
                s.events.clear();
                s.error = None;
            }
            Err(e) => {
                s.vm = None;
                s.state = ModState::Stopped;
                s.notice = Some(format!("stopped: {e}"));
            }
        }
    }

    /// A widget emitted `intent` (with `payload`); `widget` is its widget name. A mod's
    /// widget is attributed by its element id prefix: its own intents go to its VM, other
    /// intents are routed to `modules` only at the automation tier, and are refused at
    /// the presentation tier or when the mod is not running.
    pub fn on_widget_intent(
        &mut self,
        widget: &str,
        intent: &str,
        payload: Option<&str>,
        modules: Option<&mut ClientModules>,
        props: &mut Properties,
    ) -> ModRoute {
        // A list item's widget is `list[i].id`; the element is the last segment.
        let element = widget.rsplit("].").next().unwrap_or(widget);
        let Some(index) = self.slots.iter().position(|s| element.starts_with(&s.prefix)) else {
            return ModRoute::NotMod;
        };
        let Some(slot) = self.slots.get_mut(index) else {
            return ModRoute::NotMod;
        };
        let ModState::Running(tier) = slot.state else {
            self.stats.intents_refused += 1;
            return ModRoute::Refused;
        };
        if own_intent(&slot.package.key, intent) {
            if slot.events.len() < EVENTS_PER_TICK {
                let p = payload.map_or(ScriptValue::Nil, |p| ScriptValue::Str(p.to_owned()));
                slot.events.push((intent.to_owned(), p));
                self.stats.widget_events += 1;
            } else {
                self.stats.widget_events_dropped += 1;
            }
            return ModRoute::Delivered;
        }
        if tier != ModTier::Automation {
            self.stats.intents_refused += 1;
            self.stats.presentation_refused += 1;
            return ModRoute::Refused;
        }
        ModRoute::Forwarded(self.forward(intent, payload, modules, props))
    }

    fn forward(
        &mut self,
        intent: &str,
        payload: Option<&str>,
        modules: Option<&mut ClientModules>,
        props: &mut Properties,
    ) -> IntentRoute {
        let route = modules.map_or(IntentRoute::NotModule, |m| m.on_intent(intent, payload, props));
        if route == IntentRoute::Handled {
            self.stats.intents_handled += 1;
        } else {
            self.stats.intents_refused += 1;
        }
        route
    }

    /// One mod tick: runs every running mod's VM (timers, its widget events, `on_tick`),
    /// then routes the intents automation mods emitted to `modules`, then publishes the
    /// mod properties if anything changed.
    pub fn step(&mut self, modules: Option<&mut ClientModules>, props: &mut Properties) {
        self.tick += 1;
        let tick = Tick(self.tick);
        for index in 0..self.slots.len() {
            self.run(index, tick, props);
        }
        let mut modules = modules;
        for (index, intent, payload) in core::mem::take(&mut self.pending) {
            let automation = self
                .slots
                .get(index)
                .is_some_and(|s| s.state == ModState::Running(ModTier::Automation));
            if automation {
                let _ = self.forward(&intent, payload.as_deref(), modules.as_deref_mut(), props);
            } else {
                self.stats.intents_refused += 1;
            }
        }
        if self.dirty {
            self.publish(props);
        }
    }

    fn run(&mut self, index: usize, tick: Tick, props: &mut Properties) {
        let Some(slot) = self.slots.get_mut(index) else {
            return;
        };
        let ModState::Running(tier) = slot.state else {
            return;
        };
        let Some(vm) = slot.vm.as_mut() else {
            return;
        };
        let events = core::mem::take(&mut slot.events);
        let key = slot.package.key.clone();
        let outputs = RefCell::new(Outputs::default());
        let report = {
            let props = RefCell::new(&mut *props);
            let mut api = Api::new();
            build_api(&mut api, &key, &props, &outputs, &self.sounds);
            vm.tick(tick, &mut api, &events)
        };
        self.stats.ticks += 1;
        let outputs = outputs.into_inner();
        if let Some((script, e)) = report.errors.first() {
            self.stats.script_errors += report.errors.len() as u64;
            slot.error = Some(format!("script error in `{script}`: {e}"));
            self.dirty = true;
        }
        self.stats.intents_refused += outputs.over_limit;
        if tier == ModTier::Automation {
            for (name, payload) in outputs.intents {
                self.pending.push((index, name, payload));
            }
        }
        for sound in outputs.sounds {
            self.stats.sounds += 1;
            if let Some(audio) = &self.audio {
                let _ = audio.send(AudioEvent::Play {
                    sound,
                    emitter: None,
                    position: None,
                    volume: 1.0,
                    pitch: 1.0,
                    handle: VoiceHandle::NONE,
                });
            }
        }
    }

    /// Publishes `client.mods.notices` (list: `module`, `text`), `client.mods.has_notices`,
    /// and per mod `<key>.tier` (`automation`, `presentation`, or `off`) and
    /// `<key>.automation` (flag, for layouts to disable automation-only controls).
    pub fn publish(&mut self, props: &mut Properties) {
        self.dirty = false;
        let notices = self.notices();
        let id = props.intern("client.mods.has_notices");
        let _ = props.set_bool(id, !notices.is_empty());
        let items = notices
            .iter()
            .map(|n| {
                ListItem::new()
                    .with("module", Value::text(&n.module))
                    .with("text", Value::text(&n.text))
            })
            .collect();
        let id = props.intern("client.mods.notices");
        let _ = props.set(id, Value::List(items));
        for s in &self.slots {
            let tier = match s.state {
                ModState::Running(t) => tier_name(t),
                _ => "off",
            };
            let id = props.intern(&format!("{}.tier", s.package.key));
            let _ = props.set_text(id, tier);
            let id = props.intern(&format!("{}.automation", s.package.key));
            let _ = props.set_bool(id, s.state == ModState::Running(ModTier::Automation));
        }
    }
}

/// The mod notices panel, drawn whenever the client has mods.
const NOTICES_PANEL: &str = r#"panel id=client_mods_notices direction=column gap=2 visible="client.mods.has_notices" {
  list id=client_mods_notice_list bind="client.mods.notices" height=fit {
    text template="{item.module}: {item.text}"
  }
}
"#;

/// The `host.*` functions for one mod tick. Every function is offered with its tier set;
/// the VM installs only those its own tier allows, so a presentation VM has no
/// `host.intent`.
fn build_api<'h>(
    api: &mut Api<'h>,
    key: &'h str,
    props: &'h RefCell<&mut Properties>,
    outputs: &'h RefCell<Outputs>,
    sounds: &'h [(String, SoundId)],
) {
    api.function("get", Tiers::PRESENTATION, move |args| {
        let name = arg_str(args, 0, "host.get")?;
        let p = props.borrow();
        Ok(vec![script_value(p.id(&name).and_then(|id| p.get(id)))])
    });
    api.function("count", Tiers::PRESENTATION, move |args| {
        let name = arg_str(args, 0, "host.count")?;
        let p = props.borrow();
        let n = p
            .id(&name)
            .and_then(|id| p.get(id))
            .and_then(Value::as_list)
            .map_or(0, <[ListItem]>::len);
        #[allow(clippy::cast_precision_loss)] // Lists are far shorter than 2^53.
        Ok(vec![ScriptValue::Number(n as f64)])
    });
    api.function("item", Tiers::PRESENTATION, move |args| {
        let name = arg_str(args, 0, "host.item")?;
        let field = arg_str(args, 2, "host.item")?;
        let index = args
            .get(1)
            .and_then(ScriptValue::as_number)
            .filter(|i| i.fract() == 0.0 && *i >= 0.0)
            .ok_or("host.item: the index must be a whole number from 0")?;
        let p = props.borrow();
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)] // Whole and non-negative.
        let item = p
            .id(&name)
            .and_then(|id| p.get(id))
            .and_then(Value::as_list)
            .and_then(|items| items.get(index as usize));
        Ok(vec![script_value(item.and_then(|i| i.field(&field)))])
    });
    api.function("set", Tiers::PRESENTATION, move |args| {
        let name = arg_str(args, 0, "host.set")?;
        if !is_property_name(&name) {
            return Err(format!("host.set: `{name}` is not a property name"));
        }
        let mut p = props.borrow_mut();
        let id = p.intern(&format!("{key}.{name}"));
        set_property(&mut p, id, args.get(1).unwrap_or(&ScriptValue::Nil));
        Ok(Vec::new())
    });
    api.function("play", Tiers::PRESENTATION, move |args| {
        let name = arg_str(args, 0, "host.play")?;
        let mut out = outputs.borrow_mut();
        let found = sounds.iter().find(|(n, _)| *n == name).map(|(_, s)| *s);
        let played = match found {
            Some(s) if out.sounds.len() < SOUNDS_PER_TICK => {
                out.sounds.push(s);
                true
            }
            _ => false,
        };
        Ok(vec![ScriptValue::Bool(played)])
    });
    api.function("intent", Tiers::AUTOMATION, move |args| {
        let name = arg_str(args, 0, "host.intent")?;
        if !is_valid_key(&name) {
            return Err(format!("host.intent: `{name}` is not an intent name"));
        }
        let payload = match args.get(1) {
            None | Some(ScriptValue::Nil) => None,
            Some(ScriptValue::Str(s)) => Some(s.clone()),
            Some(_) => return Err("host.intent: the payload must be a string".to_owned()),
        };
        let mut out = outputs.borrow_mut();
        if out.intents.len() >= INTENTS_PER_TICK {
            out.over_limit += 1;
            return Ok(vec![ScriptValue::Bool(false)]);
        }
        out.intents.push((name, payload));
        Ok(vec![ScriptValue::Bool(true)])
    });
}

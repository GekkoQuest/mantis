//! Client mods (plan 12, decision 0001): client packages under the module contract,
//! loaded from a user directory, each running in its own Luau VM at the tier the server
//! grants.
//!
//! **A mod** is a folder laid out like a first-party module ([`package`]):
//!
//! ```text
//! <mods>/<folder>/
//!   manifest.toml      the module manifest: key, version, dependencies by contract, flags
//!   client/mod.toml    [mod] tier, demote, screens, theme, scripts
//!   client/<s>.layout  one per screen; client/<t>.theme for its styles
//!   scripts/<n>.luau   one per script, loaded in the listed order
//! ```
//!
//! Mods never run on the server: a mod manifest declaring schemas, tables, or gameplay
//! graph actions is refused, and so is one whose contract is not its own key.
//!
//! **Tiers.** A mod asks for [`ModTier::Presentation`] (read view models, draw UI, play
//! audio) or [`ModTier::Automation`] (all of that, and emit intents). The server's
//! permitted-modules list ([`PermittedModules`]) names the keys a cell allows and the
//! tier they run at; competitive instances permit presentation only. [`ModHost`] applies
//! every list the moment it arrives:
//!
//! - a key the list does not name is stopped, with a visible notice;
//! - an automation mod under a presentation-only list is **demoted** (restarted at the
//!   presentation tier with its script state carried over) when its `client/mod.toml`
//!   says `demote = true`, and stopped with a notice otherwise; its queued intents are
//!   dropped at once;
//! - when automation is permitted again, a demoted mod is promoted the same way.
//!
//! Before the first list arrives no mod runs (fail closed).
//!
//! **Refusals at load** ([`ModHost::new`]) are visible notices too: a malformed folder,
//! a key the package does not permit (`client_mods` in `package.toml`), a key or element
//! id prefix that a module of the package (or another mod) already uses, and an unmet
//! dependency. Only mods that pass are announced in `Hello`
//! ([`ModHost::hello_modules`]) with their content hashes, which are for compatibility
//! and support, never trust: the security boundary against mods is the server's intent
//! validation, movement envelopes, and rate limits (plan 12).
//!
//! **The host API** a mod's scripts see is `host.*`, gated by tier in the VM itself
//! ([`mantis_script::Api`]): a presentation VM has no `host.intent` at all.
//!
//! | function | tiers | does |
//! |---|---|---|
//! | `host.get(name)` | both | reads any UI property (view models): text, number, flag, or a list's length |
//! | `host.count(name)` | both | a list property's length (0 when absent) |
//! | `host.item(name, i, field)` | both | field `field` of item `i` (0-based) of a list property |
//! | `host.set(name, value)` | both | sets `<key>.<name>` (a mod writes only its own properties); `nil` clears |
//! | `host.play(sound, volume)` | both | plays a named UI sound at the listener ([`ModHost::set_sounds`]) |
//! | `host.intent(name, payload)` | automation | emits an intent: routed to the module that owns it, exactly as a first-party widget's |
//!
//! Scripts receive their own widgets' intents as events: `mantis.on("<key>.<name>",
//! "handler")` subscribes, and the handler gets the payload (or `nil`).
//!
//! **Widgets.** Every element id in a mod's layouts carries the mod's id prefix (`toy.hud`
//! → `toy_hud_`), and every button and input has one, so each intent a widget emits is
//! attributed to the mod that drew it ([`ModHost::on_widget_intent`]). A mod's own
//! intents (`<key>.<name>`) go to its VM and nowhere else. Any other intent from a mod's
//! widget is routed like `host.intent` when the mod runs at the automation tier and
//! **refused** at the presentation tier, so a presentation mod cannot reach an intent
//! sink even through the UI it draws. A presentation mod's layouts may not name another
//! intent at all; mod themes may style only names with the mod's prefix.

pub mod host;
pub mod package;

pub use host::{ModEnvironment, ModHost, ModRoute, ModState, ModStats};
pub use package::{ModError, ModPackage, ModScreen, scan};

pub use mantis_adapter_contract::{ModTier, ModuleEntry, PermittedModules};

/// A notice shown to the player about a mod: why it was refused or stopped, how it runs,
/// or a script error. Published as the `client.mods.notices` list property (`module` and
/// `text` fields) and drawn by the client's mod notices panel.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct ModNotice {
    /// The mod key, or the folder name when the folder did not load.
    pub module: String,
    /// What happened, in words.
    pub text: String,
}

//! The host API a script sees, gated by tier.
//!
//! A host (the server cell, the client) builds an [`Api`] of named
//! functions, each tagged with the tiers allowed to call it. A VM of a given
//! [`Tier`] gets only the functions for its tier; the rest are simply absent
//! (`nil`), so a script cannot reach them.
//!
//! - [`Tier::Server`]: server scripts (quests, AI, world events): world
//!   queries, gameplay graph triggers, events.
//! - [`Tier::Presentation`]: client mods that read view models, draw UI, and
//!   play audio. They cannot act in the world. Competitive instances permit
//!   only this tier (plan 13).
//! - [`Tier::Automation`]: client mods that may also emit intents.
//!
//! Every host function takes and returns [`ScriptValue`]s; errors become
//! script errors.

use crate::value::ScriptValue;

/// Who a VM runs for.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub enum Tier {
    /// Server scripts.
    Server,
    /// Client mods: read and present only.
    Presentation,
    /// Client mods that may emit intents.
    Automation,
}

/// A set of tiers.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct Tiers(u8);

impl Tiers {
    /// Server scripts only.
    pub const SERVER: Self = Self(1);
    /// Presentation mods (and, by inclusion, automation mods: automation is
    /// a superset of presentation).
    pub const PRESENTATION: Self = Self(2 | 4);
    /// Automation mods only.
    pub const AUTOMATION: Self = Self(4);
    /// Every tier.
    pub const ALL: Self = Self(1 | 2 | 4);

    /// True when `tier` may call a function tagged with `self`.
    #[must_use]
    pub const fn allows(self, tier: Tier) -> bool {
        let bit = match tier {
            Tier::Server => 1,
            Tier::Presentation => 2,
            Tier::Automation => 4,
        };
        self.0 & bit != 0
    }

    /// Both sets.
    #[must_use]
    pub const fn union(self, other: Self) -> Self {
        Self(self.0 | other.0)
    }
}

/// What a host function returns.
pub type HostResult = Result<Vec<ScriptValue>, String>;

/// A host function.
pub type HostFn<'h> = Box<dyn FnMut(&[ScriptValue]) -> HostResult + 'h>;

/// The functions a host offers, by name.
#[derive(Default)]
pub struct Api<'h> {
    pub(crate) entries: Vec<(&'static str, Tiers, HostFn<'h>)>,
}

impl<'h> Api<'h> {
    /// No functions.
    #[must_use]
    pub fn new() -> Self {
        Self { entries: Vec::new() }
    }

    /// Offers `f` as `host.<name>` to the tiers in `tiers`.
    pub fn function(
        &mut self,
        name: &'static str,
        tiers: Tiers,
        f: impl FnMut(&[ScriptValue]) -> HostResult + 'h,
    ) -> &mut Self {
        self.entries.push((name, tiers, Box::new(f)));
        self
    }

    /// The names a VM of `tier` sees.
    pub fn names_for(&self, tier: Tier) -> impl Iterator<Item = &'static str> + '_ {
        self.entries
            .iter()
            .filter(move |(_, t, _)| t.allows(tier))
            .map(|(n, _, _)| *n)
    }
}

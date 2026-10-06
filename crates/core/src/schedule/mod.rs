//! The tick scheduler (plan 6.2, decision 0008).
//!
//! A tick runs ten named phases in fixed order:
//! `Inbound → Timers → Movement → Combat → Effects → AI → Scripts → Interest →
//! Outbound → Persist`. Modules register systems into a phase with a priority.
//! Within a phase, systems run in ascending priority, ties broken by system
//! name, so the resolved order never depends on module load order. The
//! resolved order is printed by [`Schedule::render`] and checked against a
//! golden file in tests.
//!
//! Systems run strictly sequentially on the cell thread. Read/write
//! declarations ([`Access`]) are **not** a scheduling input. They feed:
//! - the conflict lint ([`Lint`]), computed at registration for systems that
//!   share a phase;
//! - the refusal of writes in [`Phase::Interest`] and [`Phase::Outbound`],
//!   whose work the server runs as parallel per-client read-only jobs;
//! - inspector output.
//!
//! In this crate every phase runs its systems sequentially. The server host
//! (plan 7, decision 0008) runs `Interest` and `Outbound` as per-client jobs
//! on a worker set and calls [`Schedule::run_phase`] for the rest.

use core::fmt::{self, Write as _};

use crate::ecs::{Access, ComponentSet, EcsError, ResourceSet, World};
use crate::rng::Seed;
use crate::time::{Tick, TickRate};

mod timing;
pub use timing::{Stopwatch, TIMING_WINDOW, Timing};

/// A tick phase. Declaration order is execution order.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub enum Phase {
    /// Drain the inbox and apply decoded intents.
    Inbound,
    /// Advance tick-based timers.
    Timers,
    /// Integrate movement.
    Movement,
    /// Resolve combat.
    Combat,
    /// Apply effects (gameplay graphs).
    Effects,
    /// Run AI.
    Ai,
    /// Run scripts.
    Scripts,
    /// Interest management (per-client read-only jobs on the server).
    Interest,
    /// Snapshot encoding (per-client read-only jobs on the server).
    Outbound,
    /// Flush the log segment, hand off outcomes, checkpoint.
    Persist,
}

impl Phase {
    /// Every phase in execution order.
    pub const ALL: [Self; 10] = [
        Self::Inbound,
        Self::Timers,
        Self::Movement,
        Self::Combat,
        Self::Effects,
        Self::Ai,
        Self::Scripts,
        Self::Interest,
        Self::Outbound,
        Self::Persist,
    ];

    /// Position in [`Phase::ALL`].
    #[must_use]
    pub const fn index(self) -> usize {
        self as usize
    }

    /// Display name.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::Inbound => "Inbound",
            Self::Timers => "Timers",
            Self::Movement => "Movement",
            Self::Combat => "Combat",
            Self::Effects => "Effects",
            Self::Ai => "AI",
            Self::Scripts => "Scripts",
            Self::Interest => "Interest",
            Self::Outbound => "Outbound",
            Self::Persist => "Persist",
        }
    }

    /// True for the phases whose work runs as parallel per-client read-only
    /// jobs on the server; systems there must not write.
    #[must_use]
    pub const fn is_job_phase(self) -> bool {
        matches!(self, Self::Interest | Self::Outbound)
    }

    /// True for the zero-allocation window, `Movement` through `Outbound`.
    #[must_use]
    pub const fn is_hot(self) -> bool {
        matches!(
            self,
            Self::Movement
                | Self::Combat
                | Self::Effects
                | Self::Ai
                | Self::Scripts
                | Self::Interest
                | Self::Outbound
        )
    }
}

impl fmt::Display for Phase {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

/// Per-tick inputs every system may read.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct TickContext {
    /// The tick being simulated.
    pub tick: Tick,
    /// The cell's tick rate.
    pub rate: TickRate,
    /// The cell's random seed (for [`crate::rng::Rng`] streams).
    pub seed: Seed,
}

/// Why a system failed.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum SystemError {
    /// An ECS operation failed.
    Ecs(EcsError),
    /// A system-specific invariant failed.
    Invariant(&'static str),
}

impl From<EcsError> for SystemError {
    fn from(e: EcsError) -> Self {
        Self::Ecs(e)
    }
}

impl fmt::Display for SystemError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Ecs(e) => write!(f, "{e}"),
            Self::Invariant(what) => write!(f, "invariant failed: {what}"),
        }
    }
}

impl std::error::Error for SystemError {}

/// A unit of simulation logic. Closures `FnMut(&mut World, &TickContext) ->
/// Result<(), SystemError> + Send` implement it.
pub trait System: Send {
    /// Runs once per tick in the system's phase.
    ///
    /// # Errors
    /// Any [`SystemError`]; the scheduler stops the phase and reports it.
    fn run(&mut self, world: &mut World, ctx: &TickContext) -> Result<(), SystemError>;
}

impl<F> System for F
where
    F: FnMut(&mut World, &TickContext) -> Result<(), SystemError> + Send,
{
    fn run(&mut self, world: &mut World, ctx: &TickContext) -> Result<(), SystemError> {
        self(world, ctx)
    }
}

/// Registration record of a system.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct SystemDesc {
    /// Unique, stable, dotted name, for example `"movement.integrate"`.
    pub name: &'static str,
    /// The phase it runs in.
    pub phase: Phase,
    /// Ascending order within the phase.
    pub priority: i32,
    /// Declared reads and writes.
    pub access: Access,
}

/// Registration refused.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ScheduleError {
    /// The name is empty.
    EmptyName,
    /// Another system already has this name.
    DuplicateName(&'static str),
    /// The system declares writes in a per-client job phase.
    WriteInJobPhase {
        /// The system.
        system: &'static str,
        /// Its phase.
        phase: Phase,
    },
}

impl fmt::Display for ScheduleError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::EmptyName => f.write_str("system name is empty"),
            Self::DuplicateName(n) => write!(f, "system `{n}` registered twice"),
            Self::WriteInJobPhase { system, phase } => {
                write!(
                    f,
                    "system `{system}` declares writes in read-only job phase {phase}"
                )
            }
        }
    }
}

impl std::error::Error for ScheduleError {}

/// A finding of the access lint. Informational: execution is sequential, so a
/// conflict is never a data race, but it marks an ordering dependency that the
/// priorities must express.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Lint {
    /// Two systems in one phase touch the same data and at least one writes.
    /// `first` runs before `second`.
    Conflict {
        /// The phase.
        phase: Phase,
        /// The system that runs first.
        first: &'static str,
        /// The system that runs second.
        second: &'static str,
        /// Conflicting components.
        components: ComponentSet,
        /// Conflicting resources.
        resources: ResourceSet,
    },
    /// A conflict between systems of equal priority: their relative order is
    /// decided only by the name tie-break. Give them distinct priorities.
    AmbiguousOrder {
        /// The phase.
        phase: Phase,
        /// The system that runs first (by name).
        first: &'static str,
        /// The system that runs second.
        second: &'static str,
    },
}

/// A system failed during a run.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct RunError {
    /// The failing system.
    pub system: &'static str,
    /// Its phase.
    pub phase: Phase,
    /// What went wrong.
    pub error: SystemError,
}

impl fmt::Display for RunError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "system `{}` in {} failed: {}",
            self.system, self.phase, self.error
        )
    }
}

impl std::error::Error for RunError {}

struct Entry {
    desc: SystemDesc,
    system: Box<dyn System>,
    enabled: bool,
    timing: Timing,
}

/// The resolved schedule of one world.
#[derive(Default)]
pub struct Schedule {
    entries: Vec<Entry>,
    order: [Vec<usize>; 10],
    lints: Vec<Lint>,
    stopwatch: Option<std::sync::Arc<dyn Stopwatch>>,
}

impl fmt::Debug for Schedule {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Schedule")
            .field(
                "systems",
                &self.entries.iter().map(|e| e.desc.name).collect::<Vec<_>>(),
            )
            .field("lints", &self.lints)
            .finish_non_exhaustive()
    }
}

impl Schedule {
    /// An empty schedule.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Registers a system. Re-resolves its phase's order and recomputes the
    /// phase's lints.
    ///
    /// # Errors
    /// [`ScheduleError`] for an empty or duplicate name, or writes declared in
    /// a job phase.
    pub fn add(&mut self, desc: SystemDesc, system: impl System + 'static) -> Result<(), ScheduleError> {
        if desc.name.is_empty() {
            return Err(ScheduleError::EmptyName);
        }
        if self.entries.iter().any(|e| e.desc.name == desc.name) {
            return Err(ScheduleError::DuplicateName(desc.name));
        }
        if desc.phase.is_job_phase() && desc.access.writes_anything() {
            return Err(ScheduleError::WriteInJobPhase {
                system: desc.name,
                phase: desc.phase,
            });
        }
        let index = self.entries.len();
        self.entries.push(Entry {
            desc,
            system: Box::new(system),
            enabled: true,
            timing: Timing::default(),
        });
        let entries = &self.entries;
        if let Some(order) = self.order.get_mut(desc.phase.index()) {
            order.push(index);
            order.sort_by(|a, b| {
                let (da, db) = (entries.get(*a).map(|e| e.desc), entries.get(*b).map(|e| e.desc));
                match (da, db) {
                    (Some(da), Some(db)) => (da.priority, da.name).cmp(&(db.priority, db.name)),
                    _ => a.cmp(b),
                }
            });
        }
        self.recompute_lints();
        Ok(())
    }

    fn recompute_lints(&mut self) {
        self.lints.clear();
        for phase in Phase::ALL {
            let Some(order) = self.order.get(phase.index()) else {
                continue;
            };
            for (i, a) in order.iter().enumerate() {
                for b in order.iter().skip(i + 1) {
                    let (Some(ea), Some(eb)) = (self.entries.get(*a), self.entries.get(*b)) else {
                        continue;
                    };
                    let (components, resources) = ea.desc.access.conflicts(&eb.desc.access);
                    if components.is_empty() && resources.is_empty() {
                        continue;
                    }
                    self.lints.push(Lint::Conflict {
                        phase,
                        first: ea.desc.name,
                        second: eb.desc.name,
                        components,
                        resources,
                    });
                    if ea.desc.priority == eb.desc.priority {
                        self.lints.push(Lint::AmbiguousOrder {
                            phase,
                            first: ea.desc.name,
                            second: eb.desc.name,
                        });
                    }
                }
            }
        }
    }

    /// Enables or disables every system whose name starts with `prefix`
    /// followed by a dot (a module's systems are named `<module key>.<name>`).
    /// Disabled systems keep their place in the order and are skipped.
    /// Returns how many systems changed state.
    pub fn set_enabled_under(&mut self, prefix: &str, enabled: bool) -> usize {
        let mut changed = 0;
        for e in &mut self.entries {
            let under = e
                .desc
                .name
                .strip_prefix(prefix)
                .is_some_and(|rest| rest.starts_with('.'));
            if under && e.enabled != enabled {
                e.enabled = enabled;
                changed += 1;
            }
        }
        changed
    }

    /// True when the system called `name` exists and is enabled.
    #[must_use]
    pub fn is_enabled(&self, name: &str) -> bool {
        self.entries.iter().any(|e| e.desc.name == name && e.enabled)
    }

    /// Current lint findings, in phase order, then resolved order.
    #[must_use]
    pub fn lints(&self) -> &[Lint] {
        &self.lints
    }

    /// The resolved order of one phase.
    pub fn phase_order(&self, phase: Phase) -> impl Iterator<Item = &SystemDesc> {
        self.order
            .get(phase.index())
            .into_iter()
            .flatten()
            .filter_map(|i| self.entries.get(*i).map(|e| &e.desc))
    }

    /// Runs every system of `phase` in resolved order. Stops at the first
    /// failure (fail closed) and reports which system failed.
    ///
    /// # Errors
    /// The first [`RunError`].
    pub fn run_phase(&mut self, phase: Phase, world: &mut World, ctx: &TickContext) -> Result<(), RunError> {
        let Some(order) = self.order.get(phase.index()) else {
            return Ok(());
        };
        let stopwatch = self.stopwatch.as_deref();
        for &i in order {
            let Some(entry) = self.entries.get_mut(i).filter(|e| e.enabled) else {
                continue;
            };
            let start = stopwatch.map(Stopwatch::now_nanos);
            entry.system.run(world, ctx).map_err(|error| RunError {
                system: entry.desc.name,
                phase,
                error,
            })?;
            if let (Some(sw), Some(start)) = (stopwatch, start) {
                entry.timing.record(sw.now_nanos().saturating_sub(start));
            }
        }
        Ok(())
    }

    /// Times every system run from now on with `stopwatch` (diagnostics for
    /// the Ops inspector; two clock reads per run, no allocation).
    pub fn set_stopwatch(&mut self, stopwatch: std::sync::Arc<dyn Stopwatch>) {
        self.stopwatch = Some(stopwatch);
    }

    /// Each system's name, phase, and recent run times, in registration
    /// order.
    pub fn timings(&self) -> impl Iterator<Item = (&'static str, Phase, &Timing)> + '_ {
        self.entries
            .iter()
            .map(|e| (e.desc.name, e.desc.phase, &e.timing))
    }

    /// Runs one whole tick: stamps `ctx.tick` as the world's change tick,
    /// then runs every phase in order.
    ///
    /// # Errors
    /// The first [`RunError`]; later phases do not run.
    pub fn run_tick(&mut self, world: &mut World, ctx: &TickContext) -> Result<(), RunError> {
        world.components.set_change_tick(ctx.tick);
        for phase in Phase::ALL {
            self.run_phase(phase, world, ctx)?;
        }
        Ok(())
    }

    /// The resolved order and lints as text (format version 1), for the
    /// startup log and the golden-file test. Component and resource names
    /// come from `world`'s registries.
    #[must_use]
    pub fn render(&self, world: &World) -> String {
        let comp_names = |set: &ComponentSet| -> String {
            let names: Vec<&str> = set
                .iter()
                .map(|id| world.components.component_name(id).unwrap_or("?"))
                .collect();
            names.join(",")
        };
        let res_names = |set: &ResourceSet| -> String {
            let names: Vec<&str> = set
                .iter()
                .map(|id| world.resources.name(id).unwrap_or("?"))
                .collect();
            names.join(",")
        };
        let mut out = String::from("mantis schedule v1\n");
        for phase in Phase::ALL {
            let _ = writeln!(out, "phase {phase}");
            for d in self.phase_order(phase) {
                let a = &d.access;
                let _ = writeln!(
                    out,
                    "  {:>6} {}  reads[{}] writes[{}] res_reads[{}] res_writes[{}]",
                    d.priority,
                    d.name,
                    comp_names(&a.reads),
                    comp_names(&a.writes),
                    res_names(&a.resource_reads),
                    res_names(&a.resource_writes),
                );
            }
        }
        out.push_str("lints\n");
        for lint in &self.lints {
            match lint {
                Lint::Conflict {
                    phase,
                    first,
                    second,
                    components,
                    resources,
                } => {
                    let _ = writeln!(
                        out,
                        "  conflict {phase}: {first} -> {second} on components[{}] resources[{}]",
                        comp_names(components),
                        res_names(resources)
                    );
                }
                Lint::AmbiguousOrder { phase, first, second } => {
                    let _ = writeln!(
                        out,
                        "  ambiguous-order {phase}: {first} = {second} (equal priority)"
                    );
                }
            }
        }
        out
    }
}

#[cfg(test)]
mod tests;

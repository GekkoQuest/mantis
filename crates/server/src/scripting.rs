//! Server scripts as a module (plan 16, decision 0001).
//!
//! A [`ScriptModule`] owns one server-tier [`ScriptVm`] per cell and runs it
//! in the `Scripts` phase, inside its [`ScriptWrapper`] (identity in
//! production, the allocation harness's exempt scope in tests: scripts are
//! the simulation's only allocating code and are budgeted separately).
//!
//! The host API scripts get (`host.*`):
//! - `position(entity)` → `x, y, z` of an entity in this cell (or nothing);
//! - `near(entity, radius)` → up to 16 entities within `radius` metres;
//! - `trigger(graph, source [, target])` → starts a gameplay graph by its
//!   stable name, so its timeline markers reach clients;
//! - `set(key, number)` / `get(key)` → script variables other modules can
//!   read (`ScriptVars`), part of the world state;
//! - `emit(event, value)` → a script event delivered to subscribers next tick.
//!
//! Script state, timers, subscriptions, pending events, and variables are
//! all hashed into the world state, so replay checks them.

use std::cell::RefCell;
use std::collections::BTreeMap;

use mantis_core::ecs::{Resource, World};
use mantis_core::graph::{GraphId, GraphRuntime};
use mantis_core::hash::{StableHasher, StateHash};
use mantis_core::math::Vec3;
use mantis_core::schedule::{Phase, SystemDesc, SystemError, TickContext};
use mantis_script::{Api, Limits, ScriptValue, ScriptVm, ScriptWrapper, Tier, Tiers};

use crate::cell::{Catalog, ReplMap};
use crate::components::{Body, ReplicationId};
use crate::modules::{Registrar, RegistryError, ServerModule};

/// Variables scripts publish, by name. Simulation state.
#[derive(Debug, Default, Clone, PartialEq)]
pub struct ScriptVars(pub BTreeMap<String, f64>);

impl StateHash for ScriptVars {
    fn state_hash(&self, h: &mut StableHasher) {
        h.write_u64(self.0.len() as u64);
        for (k, v) in &self.0 {
            h.write(k.as_bytes());
            h.write_u8(0);
            h.write_f64(*v);
        }
    }
}

impl Resource for ScriptVars {
    const NAME: &'static str = "server.script_vars";

    fn save(&self, e: &mut mantis_adapter_contract::core_types::Encoder<'_>) -> mantis_core::ecs::Saved {
        e.u32(u32::try_from(self.0.len()).unwrap_or(u32::MAX));
        for (k, v) in &self.0 {
            e.u16(u16::try_from(k.len()).unwrap_or(u16::MAX));
            e.bytes(k.as_bytes());
            e.u64(v.to_bits());
        }
        mantis_core::ecs::Saved::Written
    }

    fn load(
        &mut self,
        d: &mut mantis_adapter_contract::core_types::Decoder<'_>,
    ) -> Result<(), mantis_adapter_contract::core_types::DecodeError> {
        self.0.clear();
        for _ in 0..d.u32()? {
            let n = usize::from(d.u16()?);
            let k = core::str::from_utf8(d.take(n)?)
                .map_err(|_| mantis_adapter_contract::core_types::DecodeError::Invalid("utf-8"))?
                .to_owned();
            self.0.insert(k, f64::from_bits(d.u64()?));
        }
        Ok(())
    }
}

/// Counters of the cell's script runs (Ops).
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct ScriptStats {
    /// Script calls made.
    pub calls: u64,
    /// Calls that failed.
    pub errors: u64,
    /// Ticks whose instruction budget ran out.
    pub budget_exhausted: u64,
    /// Scripts unloaded for non-primitive state keys.
    pub key_violations: u64,
}

/// The cell's VM and what it carries between ticks.
pub struct Scripts {
    vm: ScriptVm,
    events: Vec<(String, ScriptValue)>,
    /// Every source this cell has run, by hash (a snapshot names sources by
    /// hash; a restore loads them from here).
    known: std::collections::BTreeMap<mantis_adapter_contract::core_types::ContentHash, String>,
    /// Counters.
    pub stats: ScriptStats,
    /// Errors of the last tick: (script, error).
    pub last_errors: Vec<(String, String)>,
}

impl Scripts {
    /// The VM (for Ops tooling: names, memory).
    #[must_use]
    pub fn vm(&self) -> &ScriptVm {
        &self.vm
    }

    /// Schedules a hot reload of `name` at the next tick boundary.
    pub fn reload(&mut self, name: &str, source: &str) {
        self.know(source);
        self.vm.reload(name, source);
    }

    /// Makes `source` available to a restore that names it by hash.
    pub fn know(&mut self, source: &str) {
        self.known.insert(
            mantis_adapter_contract::core_types::ContentHash::of(source.as_bytes()),
            source.to_owned(),
        );
    }
}

impl StateHash for Scripts {
    fn state_hash(&self, h: &mut StableHasher) {
        self.vm.state_hash(h);
        h.write_u64(self.events.len() as u64);
        for (name, v) in &self.events {
            h.write(name.as_bytes());
            h.write_u8(0);
            mantis_script::value::hash_value(v, h);
        }
    }
}

impl Resource for Scripts {
    const NAME: &'static str = "server.scripts";

    /// Every script's `state`, timers, subscriptions, and source hash (the
    /// environments are frozen, so that is all a script keeps), then the
    /// events emitted for next tick.
    fn save(&self, e: &mut mantis_adapter_contract::core_types::Encoder<'_>) -> mantis_core::ecs::Saved {
        if self.vm.save(e).is_err() {
            return mantis_core::ecs::Saved::Unsupported;
        }
        e.u32(u32::try_from(self.events.len()).unwrap_or(u32::MAX));
        for (name, v) in &self.events {
            e.u32(u32::try_from(name.len()).unwrap_or(u32::MAX));
            e.bytes(name.as_bytes());
            mantis_script::vm::save_value(v, e);
        }
        mantis_core::ecs::Saved::Written
    }

    fn load(
        &mut self,
        d: &mut mantis_adapter_contract::core_types::Decoder<'_>,
    ) -> Result<(), mantis_adapter_contract::core_types::DecodeError> {
        use mantis_adapter_contract::core_types::DecodeError;
        let known = self.known.clone();
        self.vm
            .restore(d, &|h| known.get(h).cloned())
            .map_err(|_| DecodeError::Invalid("script state"))?;
        self.events.clear();
        for _ in 0..d.u32()? {
            let n = d.u32()? as usize;
            let name = core::str::from_utf8(d.take(n)?)
                .map_err(|_| DecodeError::Invalid("utf-8"))?
                .to_owned();
            self.events.push((name, mantis_script::vm::load_value(d)?));
        }
        Ok(())
    }
}

/// One script to load: its name (its identity across reloads), its source,
/// and the entity whose random stream it draws from.
#[derive(Clone, Debug)]
pub struct ScriptSource {
    /// Name.
    pub name: String,
    /// Luau source.
    pub source: String,
    /// Owner entity (replication identity).
    pub owner: mantis_core::ecs::EntityId,
}

/// The module.
pub struct ScriptModule {
    key: &'static str,
    scripts: Vec<ScriptSource>,
    limits: Limits,
    wrapper: ScriptWrapper,
}

fn identity(f: &mut dyn FnMut()) {
    f();
}

impl ScriptModule {
    /// A module `key` running `scripts` under `limits`.
    #[must_use]
    pub fn new(key: &'static str, scripts: Vec<ScriptSource>, limits: Limits) -> Self {
        Self {
            key,
            scripts,
            limits,
            wrapper: identity,
        }
    }

    /// Wraps the whole script phase (tests: the harness's exempt scope).
    #[must_use]
    pub fn with_wrapper(mut self, wrapper: ScriptWrapper) -> Self {
        self.wrapper = wrapper;
        self
    }
}

fn number(v: Option<&ScriptValue>) -> Result<f64, String> {
    v.and_then(ScriptValue::as_number)
        .ok_or_else(|| "expected a number".to_owned())
}

fn entity(v: Option<&ScriptValue>) -> Result<mantis_core::ecs::EntityId, String> {
    v.and_then(ScriptValue::as_entity)
        .ok_or_else(|| "expected an entity".to_owned())
}

fn position(world: &World, repl: mantis_core::ecs::EntityId) -> Option<Vec3> {
    let local = world
        .resource::<ReplMap>()?
        .0
        .get(&ReplicationId(repl))
        .copied()?;
    world.get::<Body>(local).map(|b| b.0.position)
}

/// The server-tier host API over `world`.
fn api<'w>(
    world: &'w RefCell<&mut World>,
    emitted: &'w RefCell<Vec<(String, ScriptValue)>>,
    ctx: &TickContext,
) -> Api<'w> {
    let tick = ctx.tick;
    let mut api = Api::new();
    api.function("position", Tiers::SERVER, move |args| {
        let e = entity(args.first())?;
        Ok(position(&world.borrow(), e)
            .map(|p| {
                vec![
                    ScriptValue::Number(p.x.into()),
                    ScriptValue::Number(p.y.into()),
                    ScriptValue::Number(p.z.into()),
                ]
            })
            .unwrap_or_default())
    });
    api.function("near", Tiers::SERVER, move |args| {
        let e = entity(args.first())?;
        let radius = number(args.get(1))?;
        let w = world.borrow();
        let Some(at) = position(&w, e) else {
            return Ok(vec![]);
        };
        let map = w.resource::<ReplMap>().ok_or("no replication map")?;
        let mut out = Vec::new();
        for (repl, local) in &map.0 {
            if repl.0 == e || out.len() >= 16 {
                continue;
            }
            if let Some(b) = w.get::<Body>(*local)
                && f64::from((b.0.position - at).horizontal().length()) <= radius
            {
                out.push(ScriptValue::Entity(repl.0));
            }
        }
        Ok(out)
    });
    api.function("trigger", Tiers::SERVER, move |args| {
        let name = args
            .first()
            .and_then(ScriptValue::as_str)
            .ok_or("expected a graph name")?;
        let source = entity(args.get(1))?;
        let target = args.get(2).and_then(ScriptValue::as_entity);
        let mut w = world.borrow_mut();
        let map = w.resource::<ReplMap>().ok_or("no replication map")?;
        let src = map
            .0
            .get(&ReplicationId(source))
            .copied()
            .ok_or("unknown source")?;
        let tgt = match target {
            Some(t) => Some(map.0.get(&ReplicationId(t)).copied().ok_or("unknown target")?),
            None => None,
        };
        let catalog = w
            .resource::<Catalog>()
            .map(|c| std::sync::Arc::clone(&c.0))
            .ok_or("no catalog")?;
        let rt = w.resource_mut::<GraphRuntime>().ok_or("no graph runtime")?;
        rt.start(&catalog, GraphId::named(name), src, tgt, tick)
            .map_err(|e| format!("{e:?}"))?;
        Ok(vec![])
    });
    api.function("set", Tiers::SERVER, move |args| {
        let key = args
            .first()
            .and_then(ScriptValue::as_str)
            .ok_or("expected a name")?
            .to_owned();
        let value = number(args.get(1))?;
        let mut w = world.borrow_mut();
        w.resource_mut::<ScriptVars>()
            .ok_or("no script vars")?
            .0
            .insert(key, value);
        Ok(vec![])
    });
    api.function("get", Tiers::SERVER, move |args| {
        let key = args
            .first()
            .and_then(ScriptValue::as_str)
            .ok_or("expected a name")?;
        let w = world.borrow();
        Ok(w.resource::<ScriptVars>()
            .and_then(|v| v.0.get(key))
            .map(|v| vec![ScriptValue::Number(*v)])
            .unwrap_or_default())
    });
    api.function("emit", Tiers::SERVER, move |args| {
        let name = args
            .first()
            .and_then(ScriptValue::as_str)
            .ok_or("expected an event name")?
            .to_owned();
        let value = args.get(1).cloned().unwrap_or(ScriptValue::Nil);
        emitted.borrow_mut().push((name, value));
        Ok(vec![])
    });
    api
}

/// The `Scripts`-phase system body.
fn run(world: &mut World, ctx: &TickContext) -> Result<(), SystemError> {
    let mut scripts = world
        .resources
        .remove::<Scripts>()
        .ok_or(SystemError::Invariant("scripts resource"))?;
    let events = std::mem::take(&mut scripts.events);
    let emitted = RefCell::new(Vec::new());
    let report = {
        let cell = RefCell::new(&mut *world);
        let mut api = api(&cell, &emitted, ctx);
        scripts.vm.tick(ctx.tick, &mut api, &events)
    };
    scripts.events = emitted.into_inner();
    scripts.stats.calls += u64::from(report.calls);
    scripts.stats.errors += report.errors.len() as u64;
    scripts.stats.budget_exhausted += u64::from(report.budget_exhausted);
    scripts.stats.key_violations += report.key_findings.len() as u64;
    scripts.last_errors = report.errors;
    world.insert_resource(scripts).map_err(SystemError::Ecs)?;
    Ok(())
}

impl ServerModule for ScriptModule {
    fn key(&self) -> &'static str {
        self.key
    }

    fn register(&self, r: &mut Registrar<'_>) -> Result<(), RegistryError> {
        let mut vm = ScriptVm::new(Tier::Server, self.limits, r.seed())
            .map_err(|e| RegistryError::Script(e.to_string()))?;
        vm.set_wrapper(self.wrapper);
        for s in &self.scripts {
            vm.load(&s.name, &s.source, s.owner)
                .map_err(|e| RegistryError::Script(format!("{}: {e}", s.name)))?;
        }
        let known = self
            .scripts
            .iter()
            .map(|s| {
                (
                    mantis_adapter_contract::core_types::ContentHash::of(s.source.as_bytes()),
                    s.source.clone(),
                )
            })
            .collect();
        r.resource(Scripts {
            vm,
            events: Vec::new(),
            known,
            stats: ScriptStats::default(),
            last_errors: Vec::new(),
        })?;
        r.resource(ScriptVars::default())?;
        let access = r
            .access()
            .write_resource::<Scripts>()
            .write_resource::<ScriptVars>()
            .write_resource::<GraphRuntime>()
            .read_resource::<ReplMap>()
            .read::<Body>()
            .build()?;
        let name: &'static str = Box::leak(format!("{}.run", self.key).into_boxed_str());
        let wrapper = self.wrapper;
        r.system(
            SystemDesc {
                name,
                phase: Phase::Scripts,
                priority: 0,
                access,
            },
            move |w: &mut World, ctx: &TickContext| -> Result<(), SystemError> {
                let mut result = Ok(());
                wrapper(&mut || result = run(w, ctx));
                result
            },
        )?;
        Ok(())
    }
}

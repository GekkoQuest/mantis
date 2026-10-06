//! One Luau VM: sandboxed, budgeted, deterministic within a build, and
//! reloadable at tick boundaries.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};

use mantis_core::content::ContentHash;
use mantis_core::ecs::EntityId;
use mantis_core::hash::StableHasher;
use mantis_core::rng::{Rng, Salt, Seed};
use mantis_core::time::Tick;
use mantis_core::wire::{DecodeError, Decoder, Encoder, Wire};
use mlua::chunk::Compiler;
use mlua::{Function, IntoLua, Lua, LuaOptions, MultiValue, StdLib, Table, Value};

use crate::api::{Api, Tier};
use crate::lint::check_server_source;
use crate::value::{ScriptValue, entity_userdata, hash_value};

/// How much a VM may use.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Limits {
    /// Interrupt checks per tick across all of the VM's calls. Luau checks
    /// at loop back-edges and calls, so this bounds the work of any loop.
    pub budget: u32,
    /// Hard memory limit in bytes.
    pub memory: usize,
    /// Incremental GC steps at the end of each tick.
    pub gc_steps: u32,
}

impl Limits {
    /// Defaults for a cell's server scripts.
    pub const DEFAULT: Self = Self {
        budget: 20_000,
        memory: 16 << 20,
        gc_steps: 4,
    };
}

/// Runs one script call; the allocation harness wraps it in its exempt
/// scope in tests (scripts are the only allocating code in the simulation
/// window, budgeted separately; decision 0001). Production: identity.
pub type ScriptWrapper = fn(&mut dyn FnMut());

fn identity(f: &mut dyn FnMut()) {
    f();
}

/// A script failed to load or run.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum ScriptError {
    /// The static lint refused the source (server scripts).
    Lint(Vec<String>),
    /// Compiling or running the chunk failed.
    Load(String),
    /// The VM could not be set up.
    Setup(String),
    /// No script by that name.
    Unknown(String),
}

impl std::fmt::Display for ScriptError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Lint(v) => write!(f, "refused by lint: {}", v.join("; ")),
            Self::Load(e) => write!(f, "load failed: {e}"),
            Self::Setup(e) => write!(f, "VM setup failed: {e}"),
            Self::Unknown(n) => write!(f, "no script named {n}"),
        }
    }
}

impl std::error::Error for ScriptError {}

/// What one tick of a VM did.
#[derive(Clone, PartialEq, Debug, Default)]
pub struct TickReport {
    /// Calls made (timers, events, `on_tick`).
    pub calls: u32,
    /// Calls that failed: (script, error).
    pub errors: Vec<(String, String)>,
    /// True when the instruction budget ran out (later calls were skipped).
    pub budget_exhausted: bool,
    /// Non-primitive keys found in script state: (script, path).
    pub key_findings: Vec<(String, String)>,
    /// Scripts reloaded at this tick's start.
    pub reloaded: Vec<String>,
    /// Lines scripts printed.
    pub log: Vec<String>,
}

#[derive(Clone, PartialEq, Debug)]
struct Timer {
    script: String,
    func: String,
    arg: ScriptValue,
}

#[derive(Debug)]
struct Shared {
    tick: Tick,
    seed: Seed,
    current: String,
    rngs: BTreeMap<String, Rng>,
    owners: BTreeMap<String, EntityId>,
    timers: BTreeMap<(u64, u64), Timer>,
    seq: u64,
    subs: BTreeMap<String, Vec<(String, String)>>,
    log: Vec<String>,
}

impl Shared {
    fn rng(&mut self) -> &mut Rng {
        let (seed, tick) = (self.seed, self.tick);
        let owner = self
            .owners
            .get(&self.current)
            .copied()
            .unwrap_or(EntityId::new(0, 0));
        self.rngs
            .entry(self.current.clone())
            .or_insert_with(|| Rng::for_entity(seed, tick, owner, Salt::named("script.random")))
    }
}

fn lock(m: &Mutex<Shared>) -> MutexGuard<'_, Shared> {
    m.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

struct Loaded {
    env: Table,
    state: Table,
    source: ContentHash,
}

/// Math functions compiled as calls, never as native builtins, because the
/// VM replaces them with deterministic versions (or removes them).
const MATH_OVERRIDES: &[&str] = &[
    "sin",
    "cos",
    "tan",
    "atan",
    "atan2",
    "exp",
    "log",
    "pow",
    "sqrt",
    "asin",
    "acos",
    "sinh",
    "cosh",
    "tanh",
    "log10",
    "noise",
    "random",
    "randomseed",
];

/// Base functions a script environment gets.
const BASE: &[&str] = &[
    "assert",
    "error",
    "ipairs",
    "pairs",
    "next",
    "select",
    "tonumber",
    "type",
    "typeof",
    "unpack",
    "rawequal",
    "rawget",
    "rawset",
    "rawlen",
    "setmetatable",
    "getmetatable",
    "pcall",
    "xpcall",
];

/// Libraries a script environment gets (made read-only).
const LIBS: &[&str] = &["string", "table", "bit32", "utf8", "buffer", "vector"];

/// One VM.
pub struct ScriptVm {
    lua: Lua,
    tier: Tier,
    limits: Limits,
    counter: Arc<AtomicU32>,
    shared: Arc<Mutex<Shared>>,
    scripts: BTreeMap<String, Loaded>,
    pending: Vec<(String, String)>,
    host: Table,
    mantis: Table,
    math: Table,
    tostring: Function,
    print: Function,
    wrapper: ScriptWrapper,
}

fn setup<T>(r: mlua::Result<T>) -> Result<T, ScriptError> {
    r.map_err(|e| ScriptError::Setup(e.to_string()))
}

fn f32_of(x: f64) -> f32 {
    // Script numbers are doubles; the deterministic kernels are f32.
    #[expect(clippy::cast_possible_truncation)] // documented precision: f32
    let y = x as f32;
    y
}

impl ScriptVm {
    /// A VM for `tier`, seeded with `seed` (math.random draws from
    /// `Rng::for_entity(seed, tick, owner, "script.random")`).
    ///
    /// # Errors
    /// [`ScriptError::Setup`].
    pub fn new(tier: Tier, limits: Limits, seed: Seed) -> Result<Self, ScriptError> {
        let libs = StdLib::TABLE
            | StdLib::STRING
            | StdLib::BIT
            | StdLib::MATH
            | StdLib::BUFFER
            | StdLib::VECTOR
            | StdLib::UTF8;
        let lua = setup(Lua::new_with(libs, LuaOptions::default()))?;
        setup(lua.set_memory_limit(limits.memory))?;
        lua.gc_stop();
        lua.set_compiler(
            Compiler::new()
                .set_optimization_level(1)
                .set_disabled_builtins(MATH_OVERRIDES.iter().map(|f| format!("math.{f}"))),
        );
        let counter = Arc::new(AtomicU32::new(0));
        let c = Arc::clone(&counter);
        let budget = limits.budget;
        lua.set_interrupt(move |_| {
            if c.fetch_add(1, Ordering::Relaxed) >= budget {
                Err(mlua::Error::runtime("instruction budget exceeded"))
            } else {
                Ok(mlua::VmState::Continue)
            }
        });
        let shared = Arc::new(Mutex::new(Shared {
            tick: Tick::ZERO,
            seed,
            current: String::new(),
            rngs: BTreeMap::new(),
            owners: BTreeMap::new(),
            timers: BTreeMap::new(),
            seq: 0,
            subs: BTreeMap::new(),
            log: Vec::new(),
        }));
        let globals = lua.globals();
        for name in LIBS {
            if let Ok(t) = globals.get::<Table>(*name) {
                t.set_readonly(true);
            }
        }
        let math = setup(Self::math(&lua, &globals, &shared))?;
        let mantis = setup(Self::mantis(&lua, &shared))?;
        let tostring = setup(lua.create_function(|lua, v: Value| -> mlua::Result<Value> {
            // Never an address: tables and functions print as their type.
            let text = match &v {
                Value::Nil => "nil".to_owned(),
                Value::Boolean(b) => b.to_string(),
                Value::Number(_) | Value::Integer(_) | Value::String(_) => lua
                    .coerce_string(v.clone())?
                    .map(|s| s.to_string_lossy())
                    .unwrap_or_default(),
                Value::LightUserData(u) => format!("entity:{}", crate::value::entity_of(*u).to_bits()),
                other => other.type_name().to_owned(),
            };
            Ok(Value::String(lua.create_string(text)?))
        }))?;
        let s2 = Arc::clone(&shared);
        let print = setup(lua.create_function(move |lua, args: MultiValue| {
            let parts: Vec<String> = args
                .iter()
                .map(|v| {
                    lua.coerce_string(v.clone())
                        .ok()
                        .flatten()
                        .map_or_else(|| v.type_name().to_owned(), |s| s.to_string_lossy())
                })
                .collect();
            lock(&s2).log.push(parts.join("\t"));
            Ok(())
        }))?;
        let host = setup(lua.create_table())?;
        Ok(Self {
            lua,
            tier,
            limits,
            counter,
            shared,
            scripts: BTreeMap::new(),
            pending: Vec::new(),
            host,
            mantis,
            math,
            tostring,
            print,
            wrapper: identity,
        })
    }

    /// The deterministic `math` table.
    fn math(lua: &Lua, globals: &Table, shared: &Arc<Mutex<Shared>>) -> mlua::Result<Table> {
        let native: Table = globals.get("math")?;
        let math = lua.create_table()?;
        native.for_each(|k: Value, v: Value| {
            let banned = k
                .as_string()
                .is_some_and(|s| MATH_OVERRIDES.iter().any(|b| s == *b));
            if !banned {
                math.raw_set(k, v)?;
            }
            Ok(())
        })?;
        let unary = |f: fn(f32) -> f32| move |_: &Lua, x: f64| Ok(f64::from(f(f32_of(x))));
        math.raw_set("sin", lua.create_function(unary(mantis_core::math::sin))?)?;
        math.raw_set("cos", lua.create_function(unary(mantis_core::math::cos))?)?;
        math.raw_set("tan", lua.create_function(unary(mantis_core::math::tan))?)?;
        math.raw_set("exp", lua.create_function(unary(mantis_core::math::exp))?)?;
        math.raw_set("log", lua.create_function(unary(mantis_core::math::ln))?)?;
        math.raw_set("sqrt", lua.create_function(unary(mantis_core::math::sqrt))?)?;
        math.raw_set(
            "atan",
            lua.create_function(|_, (y, x): (f64, Option<f64>)| {
                Ok(f64::from(match x {
                    Some(x) => mantis_core::math::atan2(f32_of(y), f32_of(x)),
                    None => mantis_core::math::atan(f32_of(y)),
                }))
            })?,
        )?;
        math.raw_set(
            "atan2",
            lua.create_function(|_, (y, x): (f64, f64)| {
                Ok(f64::from(mantis_core::math::atan2(f32_of(y), f32_of(x))))
            })?,
        )?;
        math.raw_set(
            "pow",
            lua.create_function(|_, (x, y): (f64, f64)| {
                Ok(f64::from(mantis_core::math::pow(f32_of(x), f32_of(y))))
            })?,
        )?;
        let s = Arc::clone(shared);
        math.raw_set(
            "random",
            lua.create_function(move |_, (m, n): (Option<f64>, Option<f64>)| {
                let mut shared = lock(&s);
                let rng = shared.rng();
                let bounds = |lo: f64, hi: f64| -> mlua::Result<(u32, u32)> {
                    let ok = lo.fract() == 0.0
                        && hi.fract() == 0.0
                        && lo >= 0.0
                        && hi >= lo
                        && hi < 4_294_967_295.0;
                    if !ok {
                        return Err(mlua::Error::runtime(
                            "math.random: bounds must be whole and in range",
                        ));
                    }
                    #[expect(clippy::cast_possible_truncation, clippy::cast_sign_loss)] // checked just above
                    Ok((lo as u32, hi as u32))
                };
                Ok(match (m, n) {
                    (None, _) => rng.next_f64(),
                    (Some(n), None) => {
                        let (_, hi) = bounds(1.0, n)?;
                        f64::from(rng.below(hi) + 1)
                    }
                    (Some(m), Some(n)) => {
                        let (lo, hi) = bounds(m, n)?;
                        f64::from(lo + rng.below(hi - lo + 1))
                    }
                })
            })?,
        )?;
        math.set_readonly(true);
        Ok(math)
    }

    /// The `mantis` table: timers, events, tick, available in every tier.
    fn mantis(lua: &Lua, shared: &Arc<Mutex<Shared>>) -> mlua::Result<Table> {
        let t = lua.create_table()?;
        let s = Arc::clone(shared);
        t.raw_set(
            "after",
            lua.create_function(move |_, (ticks, func, arg): (u32, String, Value)| {
                let arg = ScriptValue::from_lua(&arg).ok_or_else(|| {
                    mlua::Error::runtime("mantis.after: the argument must be a primitive or an entity")
                })?;
                let mut shared = lock(&s);
                shared.seq += 1;
                let key = (shared.tick.0 + u64::from(ticks.max(1)), shared.seq);
                let script = shared.current.clone();
                shared.timers.insert(key, Timer { script, func, arg });
                Ok(())
            })?,
        )?;
        let s = Arc::clone(shared);
        t.raw_set(
            "on",
            lua.create_function(move |_, (event, func): (String, String)| {
                let mut shared = lock(&s);
                let script = shared.current.clone();
                let subs = shared.subs.entry(event).or_default();
                if !subs.iter().any(|(sc, f)| *sc == script && *f == func) {
                    subs.push((script, func));
                }
                Ok(())
            })?,
        )?;
        let s = Arc::clone(shared);
        t.raw_set(
            "tick",
            lua.create_function(move |_, ()| {
                Ok(f64::from(u32::try_from(lock(&s).tick.0).unwrap_or(u32::MAX)))
            })?,
        )?;
        let s = Arc::clone(shared);
        t.raw_set(
            "owner",
            lua.create_function(move |_, ()| {
                let shared = lock(&s);
                let owner = shared
                    .owners
                    .get(&shared.current)
                    .copied()
                    .unwrap_or(EntityId::new(0, 0));
                Ok(Value::LightUserData(entity_userdata(owner)))
            })?,
        )?;
        t.set_readonly(true);
        Ok(t)
    }

    /// Wraps every script call (see [`ScriptWrapper`]).
    pub fn set_wrapper(&mut self, wrapper: ScriptWrapper) {
        self.wrapper = wrapper;
    }

    /// The VM's tier.
    #[must_use]
    pub fn tier(&self) -> Tier {
        self.tier
    }

    /// Memory in use, in bytes.
    #[must_use]
    pub fn memory(&self) -> usize {
        self.lua.used_memory()
    }

    /// Names of loaded scripts.
    pub fn scripts(&self) -> impl Iterator<Item = &str> {
        self.scripts.keys().map(String::as_str)
    }

    fn environment(&self, state: &Table) -> mlua::Result<Table> {
        let globals = self.lua.globals();
        let env = self.lua.create_table()?;
        for name in BASE {
            env.raw_set(*name, globals.raw_get::<Value>(*name)?)?;
        }
        for name in LIBS {
            env.raw_set(*name, globals.raw_get::<Value>(*name)?)?;
        }
        env.raw_set("math", self.math.clone())?;
        env.raw_set("tostring", self.tostring.clone())?;
        env.raw_set("print", self.print.clone())?;
        env.raw_set("mantis", self.mantis.clone())?;
        env.raw_set("host", self.host.clone())?;
        env.raw_set("state", state.clone())?;
        Ok(env)
    }

    /// Loads (or immediately replaces) script `name`, owned by `owner` (the
    /// entity its random stream belongs to). Server scripts are linted
    /// first. The script's `state` table survives reloads.
    ///
    /// # Errors
    /// [`ScriptError`]: the lint, or the chunk failing to compile or run.
    pub fn load(&mut self, name: &str, source: &str, owner: EntityId) -> Result<(), ScriptError> {
        if self.tier == Tier::Server {
            let findings = check_server_source(source);
            if !findings.is_empty() {
                return Err(ScriptError::Lint(
                    findings.iter().map(ToString::to_string).collect(),
                ));
            }
        }
        let state = match self.scripts.get(name) {
            Some(l) => l.state.clone(),
            None => self
                .lua
                .create_table()
                .map_err(|e| ScriptError::Load(e.to_string()))?,
        };
        let env = self
            .environment(&state)
            .map_err(|e| ScriptError::Load(e.to_string()))?;
        {
            let mut shared = lock(&self.shared);
            name.clone_into(&mut shared.current);
            shared.owners.insert(name.to_owned(), owner);
        }
        self.counter.store(0, Ordering::Relaxed);
        let mut result = Ok(());
        let wrapper = self.wrapper;
        wrapper(&mut || {
            result = self
                .lua
                .load(source)
                .set_name(name)
                .set_environment(env.clone())
                .exec();
        });
        result.map_err(|e| ScriptError::Load(e.to_string()))?;
        // Frozen once loaded: `state` is the only table a script can write,
        // so the state hash (and a snapshot) covers everything it keeps.
        env.set_readonly(true);
        self.scripts.insert(
            name.to_owned(),
            Loaded {
                env,
                state,
                source: ContentHash::of(source.as_bytes()),
            },
        );
        Ok(())
    }

    /// The hash of every loaded script's source, by name.
    pub fn sources(&self) -> impl Iterator<Item = (&str, ContentHash)> {
        self.scripts.iter().map(|(n, l)| (n.as_str(), l.source))
    }

    /// Writes everything scripts keep between ticks (a cell snapshot): each
    /// script's name, source hash, owner, and `state`, then the timers, the
    /// subscriptions, and the timer sequence.
    ///
    /// # Errors
    /// A `state` holding a value that cannot be saved (the value lint
    /// unloads such scripts at the end of every tick, so this means a
    /// snapshot taken mid-tick).
    pub fn save(&self, e: &mut Encoder<'_>) -> Result<(), String> {
        let mut out = Ok(());
        let wrapper = self.wrapper;
        wrapper(&mut || out = self.save_inner(e));
        out
    }

    fn save_inner(&self, e: &mut Encoder<'_>) -> Result<(), String> {
        let shared = lock(&self.shared);
        e.u32(u32::try_from(self.scripts.len()).unwrap_or(u32::MAX));
        for (name, loaded) in &self.scripts {
            put_str(e, name);
            loaded.source.encode(e);
            e.u64(
                shared
                    .owners
                    .get(name)
                    .copied()
                    .unwrap_or(EntityId::new(0, 0))
                    .to_bits(),
            );
            save_table(&loaded.state, e, 0)?;
        }
        e.u32(u32::try_from(shared.timers.len()).unwrap_or(u32::MAX));
        for ((due, seq), t) in &shared.timers {
            e.u64(*due);
            e.u64(*seq);
            put_str(e, &t.script);
            put_str(e, &t.func);
            save_value(&t.arg, e);
        }
        e.u32(u32::try_from(shared.subs.len()).unwrap_or(u32::MAX));
        for (event, subs) in &shared.subs {
            put_str(e, event);
            e.u32(u32::try_from(subs.len()).unwrap_or(u32::MAX));
            for (s, f) in subs {
                put_str(e, s);
                put_str(e, f);
            }
        }
        e.u64(shared.seq);
        Ok(())
    }

    /// Restores what [`ScriptVm::save`] wrote. Scripts whose source differs
    /// from the saved hash are loaded again from `source_of(hash)`; scripts
    /// the snapshot does not hold are unloaded; every `state` is replaced.
    ///
    /// # Errors
    /// A source the caller cannot supply, or bytes that are not a save.
    pub fn restore(
        &mut self,
        d: &mut Decoder<'_>,
        source_of: &dyn Fn(&ContentHash) -> Option<String>,
    ) -> Result<(), String> {
        let mut out = Ok(());
        let wrapper = self.wrapper;
        wrapper(&mut || out = self.restore_inner(d, source_of));
        out
    }

    fn restore_inner(
        &mut self,
        d: &mut Decoder<'_>,
        source_of: &dyn Fn(&ContentHash) -> Option<String>,
    ) -> Result<(), String> {
        let text = |e: DecodeError| e.to_string();
        let n = d.u32().map_err(text)?;
        let mut kept = Vec::new();
        for _ in 0..n {
            let name = get_str(d).map_err(text)?;
            let source = ContentHash::decode(d).map_err(text)?;
            let owner = EntityId::from_bits(d.u64().map_err(text)?);
            if self.scripts.get(&name).is_none_or(|l| l.source != source) {
                let code = source_of(&source).ok_or_else(|| format!("no source for script `{name}`"))?;
                self.load(&name, &code, owner).map_err(|e| e.to_string())?;
            }
            lock(&self.shared).owners.insert(name.clone(), owner);
            let state = self
                .scripts
                .get(&name)
                .map(|l| l.state.clone())
                .ok_or_else(|| format!("script `{name}` did not load"))?;
            state.clear().map_err(|e| e.to_string())?;
            load_table(&self.lua, &state, d, 0)?;
            kept.push(name);
        }
        let gone: Vec<String> = self
            .scripts
            .keys()
            .filter(|n| !kept.contains(n))
            .cloned()
            .collect();
        for name in gone {
            self.unload(&name);
        }
        let mut shared = lock(&self.shared);
        shared.timers.clear();
        for _ in 0..d.u32().map_err(text)? {
            let due = d.u64().map_err(text)?;
            let seq = d.u64().map_err(text)?;
            let script = get_str(d).map_err(text)?;
            let func = get_str(d).map_err(text)?;
            let arg = load_value(d).map_err(text)?;
            shared.timers.insert((due, seq), Timer { script, func, arg });
        }
        shared.subs.clear();
        for _ in 0..d.u32().map_err(text)? {
            let event = get_str(d).map_err(text)?;
            let mut list = Vec::new();
            for _ in 0..d.u32().map_err(text)? {
                let s = get_str(d).map_err(text)?;
                let f = get_str(d).map_err(text)?;
                list.push((s, f));
            }
            shared.subs.insert(event, list);
        }
        shared.seq = d.u64().map_err(text)?;
        Ok(())
    }

    /// Schedules a reload of `name` at the start of the next tick (a tick
    /// boundary): the new code runs against the same `state`.
    pub fn reload(&mut self, name: &str, source: &str) {
        self.pending.push((name.to_owned(), source.to_owned()));
    }

    /// Removes a script and its timers and subscriptions.
    pub fn unload(&mut self, name: &str) {
        self.scripts.remove(name);
        let mut shared = lock(&self.shared);
        shared.timers.retain(|_, t| t.script != name);
        for subs in shared.subs.values_mut() {
            subs.retain(|(s, _)| s != name);
        }
    }

    fn call_one(&self, script: &str, func: &str, args: &[ScriptValue], report: &mut TickReport) {
        let Some(loaded) = self.scripts.get(script) else {
            return;
        };
        let Ok(Value::Function(f)) = loaded.env.raw_get::<Value>(func) else {
            return;
        };
        script.clone_into(&mut lock(&self.shared).current);
        report.calls += 1;
        let result = args
            .iter()
            .map(|a| a.clone().into_lua(&self.lua))
            .collect::<mlua::Result<Vec<Value>>>()
            .and_then(|a| f.call::<MultiValue>(MultiValue::from_iter(a)).map(|_| ()));
        if let Err(e) = result {
            let text = e.to_string();
            if text.contains("instruction budget exceeded") {
                report.budget_exhausted = true;
            }
            report.errors.push((script.to_owned(), text));
        }
    }

    /// Runs one tick, entirely inside the [`ScriptWrapper`]: applies pending
    /// reloads, then (with the host API of
    /// this VM's tier installed as `host`) fires due timers, delivers
    /// `events` to subscribers, and calls each script's `on_tick(tick)`, all
    /// within one instruction budget; then checks state keys and steps the
    /// GC.
    pub fn tick(&mut self, tick: Tick, api: &mut Api<'_>, events: &[(String, ScriptValue)]) -> TickReport {
        let mut report = TickReport::default();
        let wrapper = self.wrapper;
        wrapper(&mut || report = self.tick_inner(tick, api, events));
        report
    }

    fn tick_inner(&mut self, tick: Tick, api: &mut Api<'_>, events: &[(String, ScriptValue)]) -> TickReport {
        let mut report = TickReport::default();
        for (name, source) in std::mem::take(&mut self.pending) {
            let owner = lock(&self.shared)
                .owners
                .get(&name)
                .copied()
                .unwrap_or(EntityId::new(0, 0));
            match self.load(&name, &source, owner) {
                Ok(()) => report.reloaded.push(name),
                Err(e) => report.errors.push((name, e.to_string())),
            }
        }
        {
            let mut shared = lock(&self.shared);
            shared.tick = tick;
            shared.rngs.clear();
        }
        self.counter.store(0, Ordering::Relaxed);
        let tier = self.tier;
        let host = self.host.clone();
        let result = self.lua.scope(|scope| {
            host.set_readonly(false);
            for (name, tiers, f) in &mut api.entries {
                if !tiers.allows(tier) {
                    continue;
                }
                let func = scope.create_function_mut(move |lua, args: MultiValue| {
                    let args: Vec<ScriptValue> = args
                        .iter()
                        .map(|v| {
                            ScriptValue::from_lua(v).ok_or_else(|| {
                                mlua::Error::runtime("host functions take primitives and entities")
                            })
                        })
                        .collect::<mlua::Result<_>>()?;
                    let out = f(&args).map_err(mlua::Error::runtime)?;
                    out.into_iter()
                        .map(|v| v.into_lua(lua))
                        .collect::<mlua::Result<MultiValue>>()
                })?;
                host.raw_set(*name, func)?;
            }
            host.set_readonly(true);
            self.run_calls(tick, events, &mut report);
            host.set_readonly(false);
            host.clear()?;
            Ok(())
        });
        if let Err(e) = result {
            report.errors.push((String::new(), e.to_string()));
        }
        self.check_keys(&mut report);
        for _ in 0..self.limits.gc_steps {
            if self.lua.gc_step().unwrap_or(true) {
                break;
            }
        }
        report.log = std::mem::take(&mut lock(&self.shared).log);
        report
    }

    fn run_calls(&self, tick: Tick, events: &[(String, ScriptValue)], report: &mut TickReport) {
        // Timers due this tick, in (due, creation) order.
        let due: Vec<Timer> = {
            let mut shared = lock(&self.shared);
            let later = shared.timers.split_off(&(tick.0 + 1, 0));
            std::mem::replace(&mut shared.timers, later)
                .into_values()
                .collect()
        };
        for t in due {
            if report.budget_exhausted {
                break;
            }
            self.call_one(&t.script, &t.func, std::slice::from_ref(&t.arg), report);
        }
        for (event, payload) in events {
            let subs = lock(&self.shared).subs.get(event).cloned().unwrap_or_default();
            for (script, func) in subs {
                if report.budget_exhausted {
                    break;
                }
                self.call_one(&script, &func, std::slice::from_ref(payload), report);
            }
        }
        let names: Vec<String> = self.scripts.keys().cloned().collect();
        let t = f64::from(u32::try_from(tick.0).unwrap_or(u32::MAX));
        for name in names {
            if report.budget_exhausted {
                break;
            }
            self.call_one(&name, "on_tick", &[ScriptValue::Number(t)], report);
        }
    }

    /// Walks every script's `state` for keys that are not primitives
    /// (decision 0001), values that are not primitives, strings, entities,
    /// or tables of the same, and metatables. A script with such state is
    /// unloaded: its iteration order would differ between runs, or a
    /// snapshot could not hold it.
    fn check_keys(&mut self, report: &mut TickReport) {
        let mut bad = Vec::new();
        for (name, loaded) in &self.scripts {
            let mut seen = Vec::new();
            walk_keys(&loaded.state, "state", &mut seen, &mut |path| {
                bad.push((name.clone(), path));
            });
        }
        for (name, path) in bad {
            self.unload(&name);
            report.key_findings.push((name, path));
        }
    }

    /// Hashes every script's `state` in a canonical order, so script state
    /// is part of the world state hash and replay checks it.
    /// Runs inside the [`ScriptWrapper`]: walking Luau tables allocates.
    pub fn state_hash(&self, h: &mut StableHasher) {
        let wrapper = self.wrapper;
        wrapper(&mut || self.state_hash_inner(h));
    }

    fn state_hash_inner(&self, h: &mut StableHasher) {
        h.write_u64(self.scripts.len() as u64);
        for (name, loaded) in &self.scripts {
            h.write(name.as_bytes());
            h.write_u8(0);
            hash_table(&loaded.state, h, 0);
        }
        let shared = lock(&self.shared);
        h.write_u64(shared.timers.len() as u64);
        for ((due, seq), t) in &shared.timers {
            h.write_u64(*due);
            h.write_u64(*seq);
            h.write(t.script.as_bytes());
            h.write_u8(0);
            h.write(t.func.as_bytes());
            h.write_u8(0);
            hash_value(&t.arg, h);
        }
        h.write_u64(shared.subs.len() as u64);
        for (event, subs) in &shared.subs {
            h.write(event.as_bytes());
            h.write_u8(0);
            for (s, f) in subs {
                h.write(s.as_bytes());
                h.write_u8(0);
                h.write(f.as_bytes());
                h.write_u8(0);
            }
        }
    }
}

fn put_str(e: &mut Encoder<'_>, s: &str) {
    e.u32(u32::try_from(s.len()).unwrap_or(u32::MAX));
    e.bytes(s.as_bytes());
}

fn get_str(d: &mut Decoder<'_>) -> Result<String, DecodeError> {
    let n = d.u32()? as usize;
    core::str::from_utf8(d.take(n)?)
        .map(str::to_owned)
        .map_err(|_| DecodeError::Invalid("utf-8"))
}

/// Writes one primitive exactly (timer arguments, pending events).
pub fn save_value(v: &ScriptValue, e: &mut Encoder<'_>) {
    match v {
        ScriptValue::Nil => e.u8(0),
        ScriptValue::Bool(b) => {
            e.u8(1);
            e.bool(*b);
        }
        ScriptValue::Number(n) => {
            e.u8(2);
            e.u64(n.to_bits());
        }
        ScriptValue::Str(s) => {
            e.u8(4);
            put_str(e, s);
        }
        ScriptValue::Entity(x) => {
            e.u8(5);
            e.u64(x.to_bits());
        }
    }
}

/// Reads a primitive written by [`save_value`].
///
/// # Errors
/// Not a saved value.
pub fn load_value(d: &mut Decoder<'_>) -> Result<ScriptValue, DecodeError> {
    Ok(match d.u8()? {
        0 => ScriptValue::Nil,
        1 => ScriptValue::Bool(d.bool()?),
        2 => ScriptValue::Number(f64::from_bits(d.u64()?)),
        4 => ScriptValue::Str(get_str(d)?),
        5 => ScriptValue::Entity(EntityId::from_bits(d.u64()?)),
        _ => return Err(DecodeError::Invalid("script value tag")),
    })
}

/// One table value or key, exactly: tag, then the value.
fn save_lua(v: &Value, e: &mut Encoder<'_>, depth: u32) -> Result<(), String> {
    match v {
        Value::Nil => e.u8(0),
        Value::Boolean(b) => {
            e.u8(1);
            e.bool(*b);
        }
        Value::Number(n) => {
            e.u8(2);
            e.u64(n.to_bits());
        }
        Value::Integer(i) => {
            e.u8(3);
            e.u64(i.cast_unsigned());
        }
        Value::String(s) => {
            e.u8(4);
            let b = s.as_bytes();
            e.u32(u32::try_from(b.len()).unwrap_or(u32::MAX));
            e.bytes(&b);
        }
        Value::LightUserData(u) => {
            e.u8(5);
            e.u64(crate::value::entity_of(*u).to_bits());
        }
        Value::Table(t) => {
            e.u8(6);
            save_table(t, e, depth + 1)?;
        }
        other => return Err(format!("state holds a {}", other.type_name())),
    }
    Ok(())
}

/// A table in canonical (sorted key) order.
fn save_table(t: &Table, e: &mut Encoder<'_>, depth: u32) -> Result<(), String> {
    if depth > 16 {
        return Err("state nests too deep".to_owned());
    }
    let mut entries: Vec<((u8, String), Value, Value)> = Vec::new();
    t.for_each(|k: Value, v: Value| {
        if let Some(o) = key_order(&k) {
            entries.push((o, k, v));
        }
        Ok(())
    })
    .map_err(|e| e.to_string())?;
    entries.sort_by(|a, b| a.0.cmp(&b.0));
    e.u32(u32::try_from(entries.len()).unwrap_or(u32::MAX));
    for (_, k, v) in &entries {
        save_lua(k, e, depth)?;
        save_lua(v, e, depth)?;
    }
    Ok(())
}

fn load_lua(lua: &Lua, d: &mut Decoder<'_>, depth: u32) -> Result<Value, String> {
    let text = |e: DecodeError| e.to_string();
    Ok(match d.u8().map_err(text)? {
        0 => Value::Nil,
        1 => Value::Boolean(d.bool().map_err(text)?),
        2 => Value::Number(f64::from_bits(d.u64().map_err(text)?)),
        3 => Value::Integer(d.u64().map_err(text)?.cast_signed()),
        4 => {
            let n = d.u32().map_err(text)? as usize;
            Value::String(
                lua.create_string(d.take(n).map_err(text)?)
                    .map_err(|e| e.to_string())?,
            )
        }
        5 => Value::LightUserData(entity_userdata(EntityId::from_bits(d.u64().map_err(text)?))),
        6 => {
            let t = lua.create_table().map_err(|e| e.to_string())?;
            load_table(lua, &t, d, depth + 1)?;
            Value::Table(t)
        }
        _ => return Err("state value tag".to_owned()),
    })
}

fn load_table(lua: &Lua, table: &Table, d: &mut Decoder<'_>, depth: u32) -> Result<(), String> {
    if depth > 16 {
        return Err("state nests too deep".to_owned());
    }
    let count = d.u32().map_err(|err| err.to_string())?;
    for _ in 0..count {
        let key = load_lua(lua, d, depth)?;
        let value = load_lua(lua, d, depth)?;
        table.raw_set(key, value).map_err(|err| err.to_string())?;
    }
    Ok(())
}

/// A sortable form of a primitive key.
fn key_order(v: &Value) -> Option<(u8, String)> {
    Some(match v {
        Value::Boolean(b) => (0, u8::from(*b).to_string()),
        Value::Integer(i) => (1, format!("{i:+032}")),
        Value::Number(n) => (1, format!("{:032x}", n.to_bits())),
        Value::String(s) => (2, s.to_string_lossy()),
        Value::LightUserData(u) => (3, format!("{:020}", u.0.addr())),
        _ => return None,
    })
}

fn hash_table(t: &Table, h: &mut StableHasher, depth: u32) {
    if depth > 16 {
        return;
    }
    let mut entries: Vec<((u8, String), Value)> = Vec::new();
    let _ = t.for_each(|k: Value, v: Value| {
        if let Some(o) = key_order(&k) {
            entries.push((o, v));
        }
        Ok(())
    });
    entries.sort_by(|a, b| a.0.cmp(&b.0));
    h.write_u64(entries.len() as u64);
    for ((kind, key), v) in entries {
        h.write_u8(kind);
        h.write(key.as_bytes());
        h.write_u8(0);
        match v {
            Value::Table(inner) => {
                h.write_u8(5);
                hash_table(&inner, h, depth + 1);
            }
            other => hash_value(&ScriptValue::from_lua(&other).unwrap_or(ScriptValue::Nil), h),
        }
    }
}

fn walk_keys(t: &Table, path: &str, seen: &mut Vec<Table>, bad: &mut dyn FnMut(String)) {
    if seen.iter().any(|s| s == t) || seen.len() > 4096 {
        return;
    }
    seen.push(t.clone());
    if t.metatable().is_some() {
        bad(format!("{path} has a metatable"));
    }
    let mut children = Vec::new();
    let _ = t.for_each(|k: Value, v: Value| {
        if key_order(&k).is_none() {
            bad(format!("{path}[<{}>]", k.type_name()));
        }
        // Values are primitives, strings, entities, or tables of the same,
        // so `state` is saved canonically (a snapshot holds all of it).
        let saveable = matches!(
            v,
            Value::Nil
                | Value::Boolean(_)
                | Value::Number(_)
                | Value::Integer(_)
                | Value::String(_)
                | Value::LightUserData(_)
                | Value::Table(_)
        );
        if !saveable {
            bad(format!("{path} holds a {}", v.type_name()));
        }
        if let Value::Table(inner) = v {
            let label = match &k {
                Value::String(s) => format!("{path}.{}", s.to_string_lossy()),
                other => format!(
                    "{path}[{}]",
                    key_order(other).map_or_else(|| other.type_name().to_owned(), |o| o.1)
                ),
            };
            children.push((label, inner));
        }
        Ok(())
    });
    for (label, inner) in children {
        walk_keys(&inner, &label, seen, bad);
    }
}

/// The light userdata a host passes to scripts for `e`.
#[must_use]
pub fn entity_value(e: EntityId) -> Value {
    Value::LightUserData(entity_userdata(e))
}

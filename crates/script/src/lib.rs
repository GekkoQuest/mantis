//! mantis-script: the Luau script runtime (plan 16, decision 0001).
//!
//! - [`ScriptVm`]: one sandboxed VM. Each script gets its own environment
//!   (base functions, read-only `string`, `table`, `bit32`, `utf8`, `buffer`,
//!   `vector`, a deterministic `math`, `mantis`, `host`, and its persistent
//!   `state` table). `os`, `debug`, `require`, `loadstring`, `getfenv`,
//!   `setfenv`, `collectgarbage`, and `newproxy` are absent.
//! - **Budget.** One instruction budget per tick across a VM's calls, enforced
//!   by Luau's interrupt; a script that loops forever is stopped and the
//!   tick goes on.
//! - **Memory.** A hard per-VM memory limit; the GC is stopped and stepped a
//!   fixed number of times at the end of each tick. Every call runs inside a
//!   [`ScriptWrapper`], which the allocation-harness tests set to the
//!   exempt scope (scripts are the only allocating code in the simulation
//!   window; decision 0001, option A).
//! - **Determinism.** `math.random` draws from
//!   `Rng::for_entity(seed, tick, owner, "script.random")`; the
//!   transcendental `math` functions are the engine's deterministic kernels
//!   (f32 precision) and compile as calls, never native builtins; `tostring`
//!   never prints an address. Server scripts pass a static [`lint`] (no `^`,
//!   no table or function keys) and a runtime key check over their state;
//!   script state is hashed in canonical key order ([`ScriptVm::state_hash`]).
//! - **Hot reload** at tick boundaries keeps each script's `state` by name.
//! - `mantis.after(ticks, fn, arg)`, `mantis.on(event, fn)`, `mantis.tick()`,
//!   and `mantis.owner()` are available to every tier; `host.*` is the
//!   host's tiered API.
//! - **Tiers** ([`api`]): server, presentation, and automation VMs see only
//!   the host functions their tier allows.

#![forbid(unsafe_code)]

pub mod api;
pub mod lint;
pub mod value;
pub mod vm;

pub use api::{Api, HostResult, Tier, Tiers};
pub use value::ScriptValue;
pub use vm::{Limits, ScriptError, ScriptVm, ScriptWrapper, TickReport, entity_value};

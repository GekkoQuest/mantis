//! mantis-core: the simulation core shared by both hosts (plan section 6).
//!
//! Depends on nothing of ours. Deterministic by construction: time is an
//! injected tick counter ([`time`]), randomness is a seeded stream per
//! `(tick, entity, purpose)` ([`rng`]), floating-point transcendentals are
//! implemented in-crate ([`math`]), and state hashes are stable ([`hash`]).
//! Systems run sequentially in named phases ([`schedule`]); hot-path memory is
//! pre-sized and fails closed instead of growing ([`mem`]).
//!
//! Simulation building blocks: [`ecs`], shared [`kinematics`], gameplay
//! [`graph`]s with timeline markers, pure [`rules`], content addressing
//! ([`content`]), the unified per-cell [`log`], and [`replay`].

#![forbid(unsafe_code)]

pub mod content;
pub mod ecs;
pub mod graph;
pub mod hash;
pub mod kinematics;
pub mod ledger;
pub mod log;
pub mod math;
pub mod mem;
pub mod module;
pub mod replay;
pub mod rng;
pub mod rules;
pub mod schedule;
pub mod social;
pub mod time;
pub mod wire;

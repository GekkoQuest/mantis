//! toy-server: the proving package's server side (plan 18 step 2).
//!
//! One zone of two cells, one class with one ability, and two adapters: the
//! native adapter (QUIC, Predictive) and a deliberately legacy-shaped adapter
//! (TCP, Validated). Default tunables come from `packages/toy/package.toml`.
//! [`sim`] runs the whole server in-process against headless bots over a
//! simulated network, for budget tests and the soak run.

#![forbid(unsafe_code)]

pub mod cluster;
pub mod modules;
pub mod recovery;
pub mod scenario;
pub mod sim;
pub mod tunables;
pub mod wire;
pub mod world;

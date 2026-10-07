//! toy-server: the proving package's server side (plan 18 step 2).
//!
//! One zone of two cells, one class with one ability, and two adapters: the
//! native adapter (QUIC, Predictive) and a deliberately legacy-shaped adapter
//! (TCP, Validated). Default tunables come from `packages/toy/package.toml`.
//! [`sim`] runs the whole server in-process against headless bots over a
//! simulated network, for budget tests and the soak run.

#![forbid(unsafe_code)]

pub mod cluster;
pub mod content;
pub mod front;
pub mod login;
pub mod modules;
pub mod node;
pub mod recovery;
pub mod scenario;
pub mod sim;
pub mod tunables;
pub mod wire;
pub mod world;

/// The build identity written into logs and snapshots: the package, its
/// version, and the log schema version, so the same source built for any
/// architecture replays the same logs, and a binary with other record
/// encodings is refused by the log header rather than misreading it.
#[must_use]
pub fn build_id() -> mantis_core::log::BuildId {
    let text = format!(
        "toy-server {} log schema {}",
        env!("CARGO_PKG_VERSION"),
        mantis_server::intent::LOG_SCHEMA_VERSION
    );
    let id = mantis_core::content::ContentHash::of(text.as_bytes());
    mantis_core::log::BuildId(*id.as_bytes())
}

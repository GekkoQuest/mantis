//! mantis-deploy: the service roles one process per role (plan 10).
//!
//! - [`config`]: a node's configuration, one strict TOML file per process.
//! - [`registry`]: the static registry of every instance's addresses,
//!   signed with Ed25519 so a tampered registry is refused.
//! - [`matrix`]: the deployable roles and the caller matrix applied per
//!   process (a node dials only the roles its role may call).
//! - [`health`]: `/live`, `/ready`, `/metrics` on a listener of its own.
//! - [`ready`]: a node serves only once the roles it depends on are ready.
//! - [`drain`]: SIGTERM, Ctrl-C, or the end of standard input start a drain.
//! - [`node`]: what every node does at start, whatever its role.
//! - [`roles`]: the six service roles as processes.
//! - [`cell`]: the cell-host role, adopted by a package's server binary.
//! - [`local`]: every service role in one process, for development.
//! - [`keys`]: key and secret files (never environment variables).
//! - [`pki`]: the cluster CA and per-node certificates for mutual TLS.
//! - [`tls`]: a node's own mutual-TLS material, checked at start.
//! - [`cli`]: the command line of `mantisd` and of a package's `node`
//!   subcommand.

#![forbid(unsafe_code)]

pub mod cell;
pub mod cli;
pub mod config;
pub mod drain;
pub mod fields;
pub mod health;
pub mod keys;
pub mod local;
pub mod matrix;
pub mod node;
pub mod pki;
pub mod ready;
pub mod registry;
pub mod roles;
pub mod tls;

//! The module contract shared by both hosts (plan 13, decision 0010).
//!
//! Every feature is a module under `packages/<owner>/modules/<feature>/`
//! with a `manifest.toml`, a `contract/` crate holding its public events and
//! queries, and `server/` and `client/` implementation crates. This module
//! holds what both hosts need to agree on:
//!
//! - [`manifest`]: the manifest format, and the module section of
//!   `package.toml` (`uses`, `overrides`, `[flags]`);
//! - [`resolve()`]: the package's module graph, which modules are in and
//!   enabled, the order they register, and why a host refuses to start;
//! - [`bus`]: queued [`bus::Event`]s and synchronous [`bus::Query`]s, the
//!   only way modules talk to each other;
//! - [`toml`]: the strict TOML subset the manifests are written in.
//!
//! **Flags.** A flag's value is its manifest default, then the package's
//! `[flags]`, then live (Ops) overrides. A package default naming a module
//! the package does not include is skipped, so removing a module folder
//! never breaks the package; a flag of an included module must exist, and
//! every live flag must name a real flag (both refuse start otherwise).
//!
//! The server's registry surface (handlers, systems, commands) lives in
//! `mantis-server`; the client's (screens, views) in `mantis-client`.

pub mod bus;
pub mod manifest;
pub mod resolve;
pub mod toml;

pub use bus::{Event, Events, Queries, Query, QueryError, ask};
pub use manifest::{
    Dependency, Manifest, ManifestError, PackageModules, Version, is_valid_key, parse_manifest, parse_package,
};
pub use resolve::{Discovered, ModuleGraph, OptionalProvider, ResolveError, Resolved, resolve};

#[cfg(test)]
mod tests;

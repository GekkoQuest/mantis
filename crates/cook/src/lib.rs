//! mantis-cook: the content pipeline (plan 11).
//!
//! A package's source content lives under `packages/<p>/content/`. The cook reads that
//! tree ([`tree::ContentTree`]), runs every [`importer::Importer`] whose extensions match
//! a source file, in phase order (so a material can name a texture and a sector can name
//! a mesh by source path and receive its content hash), validates everything with the
//! runtime parsers, and produces cooked payloads in three **domains**
//! ([`mantis_formats::bundle::Domain`]): `gameplay` (shared by client and server; its
//! bundle hash is what the handshake compares), `server`, and `presentation`.
//!
//! - [`tree`]: source discovery, deterministic order.
//! - [`importer`]: the importer plugin API (built-in importers and package plugins alike;
//!   no importer is specific to any game).
//! - [`pipeline`]: running importers, cross references, errors with file and line.
//! - [`store`]: the content-addressed payload store.
//! - [`sign`]: Ed25519 bundle signing and verification keys.
//! - [`source`]: typed reading of structured (TOML subset) sources with located errors.
//! - [`importers`]: the built-in importers.
//!
//! Cooking is deterministic: the same sources and importer versions produce the same
//! bytes and hashes on every machine. Every importer has a version; changing an
//! importer's output means bumping it, so the change is a visible content-hash change.

#![forbid(unsafe_code)]

pub mod importer;
pub mod importers;
pub mod package;
pub mod pipeline;
pub mod sign;
pub mod source;
pub mod store;
pub mod tree;

pub use importer::{CookError, Cooked, ImportContext, Importer, Source};
pub use pipeline::{Cook, CookOutput};

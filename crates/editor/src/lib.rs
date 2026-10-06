//! mantis-editor: the editor module, the live inspector, and the replay viewer (plan 16).
//!
//! The editor is the client with this module loaded (`toy-client --editor`): every tool
//! works on the same content sources, cooked store, render world, and renderer the client
//! uses, and every tool is headless-testable.
//!
//! - [`module`]: the editor screen over the client's world view (every tool behind
//!   tabs; `F10` shows and hides it).
//! - [`ops`] (with [`json`]): the server half of the live inspector, read-only GETs
//!   against the Ops dashboard with a pinned certificate and a bearer token.
//! - [`source`]: format-preserving edits of the structured sources the cook reads.
//! - [`graphs`]: gameplay and presentation graphs side by side, marker bindings visible,
//!   checked by the cook's importers and hot-reloaded into the presentation library.
//! - [`materials`]: material graph editing, compiled as the cook compiles it and
//!   replaced in place in the live renderer.
//! - [`ui_edit`]: UI layout and theme editing, hot-reloaded into the live UI.
//! - [`world`]: placement editing, written back to sector sources, recooked, and streamed
//!   into the live renderer again.
//! - [`replay`]: the replay viewer, stepping a fresh client simulation through an MCRC
//!   client recording tick by tick, divergence reported as an error.

#![forbid(unsafe_code)]

pub mod graphs;
pub mod json;
pub mod materials;
pub mod module;
pub mod ops;
pub mod replay;
pub mod source;
pub mod ui_edit;
pub mod world;

//! mantis-client: the native client host.
//!
//! Thread model (plan 8.1, decision 0009):
//! - **Simulation thread** ([`sim`], [`threads::sim_thread`]): fixed tick, owns all game
//!   state, predicts the local avatar ([`predict`]), applies snapshots, reconciles, and
//!   publishes one [`render_world::RenderWorld`] per tick.
//! - **Render thread** ([`threads::render_thread`]): reads the double-buffered render
//!   world each frame, owns camera orientation ([`camera`]), samples mouse look, routes
//!   input ([`input`]) and hands action state to the sim, which turns it into intents.
//! - **Streaming pool** ([`threads::streaming`]): loads assets off-thread and hands
//!   finished work to the render thread under a per-frame millisecond budget.
//! - **Audio thread** ([`threads::audio`]): mixes from a bounded event queue.
//!
//! Presentation graphs ([`presentation`]) turn gameplay timeline markers into effects,
//! sounds, camera shakes, and animation triggers.
//!
//! Frame rate and tick rate are fully decoupled ([`time::FixedStepper`]). Scopes
//! ([`scope`]) own per-session resources and dispose them in reverse order. Window and GPU
//! device creation are confined to [`platform`]; everything else runs headless.
//!
//! The client's view of `mantis-core` (ids, time, math, kinematics) and its motion seam
//! are in [`core_api`]. The network thread ([`net`]) decodes the native protocol with
//! `mantis-adapter-contract` and hands the simulation its own frame view ([`snapshot`]).
//!
//! Client mods ([`mods`]) run in Luau VMs at the tier the server permits, beside the
//! module UI ([`ui_layer`]).
//!
//! [`recording`] is the MCRC session recording the editor's replay viewer reads; dev
//! builds capture it from the simulation.

#![forbid(unsafe_code)]

pub mod anim_pool;
pub mod camera;
pub mod characters;
pub mod content_store;
pub mod core_api;
pub mod host;
pub mod input;
pub mod inspect;
pub mod jitter;
pub mod media;
pub mod mods;
pub mod modules;
pub mod net;
pub mod platform;
pub mod predict;
pub mod presentation;
pub mod recording;
pub mod render_world;
pub mod scope;
pub mod sim;
pub mod snapshot;
pub mod threads;
pub mod time;
pub mod ui_layer;
pub mod world_stream;
pub mod world_view;

#[cfg(test)]
pub(crate) mod testing;

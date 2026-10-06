//! mantis-anim: data-defined animation graphs, blending, IK, root motion, skinning
//! palettes, and VAT baking helpers (plan 8.4).
//!
//! Assets come from `mantis-formats` ([`mantis_formats::skeleton`],
//! [`mantis_formats::anim_clip`], [`mantis_formats::anim_graph`]); this crate binds them
//! and plays them. The pieces:
//!
//! - [`transform`]: TRS transforms and shortest-path quaternion interpolation.
//! - [`skeleton`]: the bone hierarchy, poses, the model-space pass, and the 3x4 skinning
//!   palette uploaded by the near crowd tier.
//! - [`clip`]: keyframe sampling (step or linear), looping versus clamped playback.
//! - [`blend`]: weighted N-way blends, masked override layers, additive layers.
//! - [`root_motion`]: per-update extraction of the root's horizontal travel and yaw.
//! - [`ik`]: analytic two-bone IK for feet and a clamped look-at.
//! - [`graph`] and [`instance`]: a bound graph (shared, immutable) and a playing
//!   instance (parameters, state machines with crossfades, exit times, triggers; all
//!   buffers preallocated, so [`instance::AnimInstance::update`] allocates nothing).
//! - [`batch`]: evaluating many instances on worker threads.
//! - [`vat`]: baking vertex animation textures for the mid and far crowd tiers.
//!
//! This is presentation code: it uses `glam` and `std` float math, never the
//! deterministic simulation math, and nothing here feeds back into the simulation.

#![forbid(unsafe_code)]

pub mod batch;
pub mod blend;
pub mod clip;
pub mod error;
mod eval;
pub mod graph;
pub mod ik;
pub mod instance;
pub mod root_motion;
pub mod skeleton;
pub mod transform;
pub mod vat;

pub use batch::evaluate_batch;
pub use clip::Clip;
pub use error::AnimError;
pub use graph::{AnimGraph, LookAt, ParamId, TwoBoneChain};
pub use instance::{AnimInstance, IkTarget};
pub use root_motion::{RootDelta, RootMotion};
pub use skeleton::{PaletteEntry, Pose, Skeleton};
pub use transform::Transform;
pub use vat::{SkinnedMesh, VatData, bake_vat};

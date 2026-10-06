//! mantis-formats: runtime asset formats shared by the cook (producer) and the hosts
//! (consumers), per decision 0016.
//!
//! Pure byte-level parsers and reference encoders; no GPU, windowing, or network
//! dependency. Every format carries a magic, a version, and flags with unused bits
//! required to be zero, and a parser rejects the whole asset on any malformed field.
//!
//! **Versioning rule.** Every change to a record or chunk layout is a version bump, and the
//! parser keeps accepting the previous version, reading it into the current in-memory type
//! with the new fields at neutral values; records are never edited in place. The material
//! format is the model: version 2 added the bindings table and version 1 still parses with
//! empty bindings. Encoders write the current version only.
//!
//! - [`probe_volume`]: baked L1 SH probe grids per time-of-day keyframe.
//! - [`lightmap`]: baked lightmap atlases per keyframe.
//! - [`sector`]: the chunked sector container (ground, collision, triggers, placements,
//!   baked-lighting references, streaming hints).
//! - [`sh`] and [`time_of_day`]: the shared SH convention and keyframe rules.
//! - [`bundle`]: signed, versioned manifests of cooked content, per domain.
//! - [`texture`]: cooked textures (BC1, BC4, BC5, BC7, RGBA8) with their encoder version.
//! - [`mesh`]: cooked geometry with meshlets and an optional skinning stream.
//! - [`material`]: material graphs and their shader permutation keys.
//! - [`color_grading`]: grading parameters baked into the renderer's lookup table.
//! - [`skeleton`], [`anim_clip`], [`anim_graph`]: skeletons, compressed clips, and
//!   animation state graphs.
//! - [`sound_bank`] and [`mixer_graph`]: sound banks and the audio mixer graph.
//! - [`particle_effect`]: data-defined emitters for the GPU particle system.
//! - [`gameplay_graph`]: cooked gameplay graphs (decision 0021), gameplay domain.
//! - [`presentation`]: presentation graphs binding effects, sounds, camera shakes, and
//!   animation triggers to gameplay timeline markers.
//! - [`bytes`] and [`half`]: the shared reader, writer, error type, and half floats.

#![forbid(unsafe_code)]

pub mod anim_clip;
pub mod anim_graph;
pub mod bundle;
pub mod bytes;
pub mod color_grading;
pub mod gameplay_graph;
pub mod half;
pub mod lightmap;
pub mod material;
pub mod mesh;
pub mod mixer_graph;
pub mod particle_effect;
pub mod presentation;
pub mod probe_volume;
pub mod sector;
pub mod sh;
pub mod skeleton;
pub mod sound_bank;
pub mod texture;
pub mod time_of_day;
pub mod vat;

pub use bytes::FormatError;

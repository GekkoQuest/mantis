//! Built-in importers: generic source formats every package may use. None is specific to
//! any game.
//!
//! | module | sources | outputs | phase |
//! |---|---|---|---|
//! | [`tables`] | `tables/*` text tables | tables (gameplay) | 0 |
//! | [`textures`] | `*.ppm`, `*.pgm`, `*.pam` images with optional `*.texture.toml` settings | MTEX (presentation) | 0 |
//! | [`geometry`] | `*.obj` meshes, skinned meshes, skeletons, clips, animation graphs, VAT bakes | MMSH, skeletons, clips (presentation) at 0 and 10; graphs, MVAT at 15 | 0, 10, 15 |
//! | [`content`] | color gradings, UI layouts and themes, mixer graphs (0); particle effects (5); materials, presentation graphs, sound banks (10) | MGRD, UI text, mixer graphs, MPFX, MMAT, MPRS, MSBK (presentation) | 0, 5, 10 |
//! | [`bake`] | the `[bake]` tables of `sectors/*.sector.toml` | MPRB, MLMP, lightmap layouts (presentation) | 20 |
//! | [`world`] | `sectors/*.sector.toml` (plus `terrain/*.pgm` inputs) | sector server, gameplay, and visual copies (server, gameplay, presentation; decision 0020), ground meshes (presentation) | 30 |
//!
//! Structured sources use the strict TOML subset of `mantis_core::module::toml`, with one
//! named table per item (`[placement.tree_01]`).

use std::sync::Arc;

use crate::importer::Importer;

pub mod bake;
pub mod content;
pub mod geometry;
pub mod tables;
pub mod textures;
pub mod world;

/// Every built-in importer.
pub fn builtin() -> Vec<Arc<dyn Importer>> {
    let mut all: Vec<Arc<dyn Importer>> = vec![Arc::new(tables::Tables)];
    all.extend(textures::importers());
    all.extend(geometry::importers());
    all.extend(content::importers());
    all.extend(world::importers());
    all.extend(bake::importers());
    all
}

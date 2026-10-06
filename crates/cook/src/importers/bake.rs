//! World bakes (plan 8.3, decision 0003): cooked indirect light per sector, as a probe
//! volume for dynamic objects and an optional lightmap atlas for static placements, each
//! baked at a few time-of-day keyframes that the renderer blends.
//!
//! The baker reads the same `sectors/<x>_<z>.sector.toml` sources as the sector importer
//! ([`super::world`], whose module doc has the full sector syntax) at **phase 20**, after
//! meshes (phase 0) and before the sector itself (phase 30), which picks up its outputs
//! from the same source path. A sector source without a `[bake]` table produces nothing
//! here. Everything runs on the CPU, single-threaded, with fixed iteration orders and no
//! randomness: the same sources give the same bytes on every machine.
//!
//! # Outputs (presentation domain)
//!
//! | name | kind | contents |
//! |---|---|---|
//! | `sectors/<x>_<z>.probes` | `ProbeVolume` (MPRB) | L1 SH probes per keyframe, validity mask when a probe is inside geometry |
//! | `sectors/<x>_<z>.lightmap` | `Lightmap` (MLMP) | when a placement has `lightmapped = true`: the atlas, one layer per keyframe |
//! | `sectors/<x>_<z>.lightmap_layout` | [`world::LIGHTMAP_LAYOUT_KIND`] | with the atlas: one line per lightmapped placement, `<name> <scale u> <scale v> <offset u> <offset v>` (`atlas_uv = uv1 * scale + offset`) |
//!
//! # Source syntax
//!
//! ```toml
//! [bake]
//! probe_spacing = 4.0              # meters between probes on every axis (default 4)
//! probe_height = 8.0               # the volume reaches this far above the highest ground sample (default 8)
//! probe_lift = 0.5                 # lowest probe layer above the lowest ground sample (default 0.5)
//! lightmap_texels_per_meter = 4.0  # lightmap density (default 4)
//! lightmap_max_size = 1024         # largest atlas edge, 16 to 8192 (default 1024)
//! lightmap_padding = 2             # texels around every rectangle, filled by dilation, 1 to 8 (default 2)
//! lightmap_sun = true              # lightmaps include direct sunlight (default true)
//! samples = 256                    # rays per probe and per texel, 16 to 16384 (default 256)
//! bounces = 1                      # 0, or 1 for the sunlit bounce (default 1)
//! albedo = [0.5, 0.5, 0.5]         # constant diffuse albedo of every surface for the bounce
//! sky_color = [1.0, 1.0, 1.0]      # sky radiance at or above the horizon (keyframe default)
//! ground_color = [0.0, 0.0, 0.0]   # radiance below the horizon where nothing is hit (keyframe default)
//!
//! [bake.noon]                      # one table per keyframe, 1 to 8, distinct times
//! time = 0.5                       # day fraction, 0 (midnight) up to 1
//! sun_direction = [0.3, -1.0, 0.2] # the direction sunlight travels (renderer convention)
//! sun_color = [3.0, 2.9, 2.7]      # sunlight as the radiance of a white surface facing it
//! sky_color = [0.4, 0.5, 0.7]      # optional, overrides [bake]
//! ground_color = [0.1, 0.1, 0.1]   # optional, overrides [bake]
//!
//! [bake.night]
//! time = 0.0
//! sun_direction = [0.0, -1.0, 0.0]
//! sun_color = [0.0, 0.0, 0.0]
//! sky_color = [0.02, 0.03, 0.06]
//! ```
//!
//! # Model
//!
//! - **Geometry** ([`scene`]): the ground heightfield, the hulls that block sight, and the
//!   meshes of placements that cast shadows, in one deterministic [`bvh`].
//! - **Light** ([`light`]): sky and ground colors where rays escape, the sun with shadow
//!   rays, and an optional bounce of sunlight off every surface with one constant albedo
//!   (per-material albedo is not read yet). Values are irradiance divided by pi, the
//!   quantity both runtime formats store.
//! - **Probes** ([`probes`]): sky and bounce only (the renderer lights dynamic objects
//!   with the dynamic sun), projected with the `mantis_formats::sh` convention.
//! - **Lightmaps** ([`lightmap`]): sun (with `lightmap_sun`), sky with occlusion, and
//!   bounce per texel. A lightmapped placement's mesh must keep `uv1` inside 0..1; the
//!   cook fails at the placement otherwise.

use std::fmt::Write as _;
use std::sync::Arc;

use mantis_formats::bundle::{AssetKind, Domain};
use mantis_formats::lightmap::Lightmap;
use mantis_formats::probe_volume::ProbeVolume;

use crate::importer::{CookError, Cooked, ImportContext, Importer, Source};
use crate::importers::world;

pub mod bvh;
pub mod light;
pub mod lightmap;
pub mod math;
pub mod probes;
pub mod scene;
pub mod settings;

/// The importer version.
pub const VERSION: u32 = 1;

/// This module's importers.
pub fn importers() -> Vec<Arc<dyn Importer>> {
    vec![Arc::new(Bake)]
}

/// The sector bake importer.
#[derive(Clone, Copy, Debug, Default)]
pub struct Bake;

/// The output name stem of a sector source (`sectors/0_0.sector.toml` -> `sectors/0_0`).
pub fn stem(path: &str) -> &str {
    path.strip_suffix(".sector.toml").unwrap_or(path)
}

/// Renders the lightmap layout payload.
pub fn layout_text(layout: &[(String, [f32; 4])]) -> String {
    let mut out = String::new();
    for (name, [su, sv, ou, ov]) in layout {
        let _ = writeln!(out, "{name} {su} {sv} {ou} {ov}");
    }
    out
}

impl Importer for Bake {
    fn name(&self) -> &'static str {
        "world.bake"
    }

    fn version(&self) -> u32 {
        VERSION
    }

    fn phase(&self) -> u32 {
        20
    }

    fn accepts(&self, path: &str) -> bool {
        world::is_sector_source(path)
    }

    fn import(&self, source: &Source<'_>, ctx: &ImportContext<'_>) -> Result<Vec<Cooked>, CookError> {
        let path = source.path;
        let Some(settings) = settings::parse_settings(path, source.text()?)? else {
            return Ok(Vec::new());
        };
        // Sector syntax errors are reported once, by the sector importer.
        let Ok(sector) = world::parse_sector(source, Some(ctx)) else {
            return Ok(Vec::new());
        };
        let scene = scene::build(&sector, ctx, path)?;
        let volume = probes::bake(&sector.info, &scene, &settings)
            .map_err(|m| CookError::at(path, settings.probe_spacing_line, &m))?;
        let probe_bytes = volume.encode();
        ProbeVolume::parse(&probe_bytes)
            .map_err(|e| CookError::at(path, 0, &format!("baked probe volume does not parse: {e}")))?;
        let stem = stem(path);
        let mut out = vec![Cooked {
            name: format!("{stem}.probes"),
            kind: AssetKind::ProbeVolume,
            domain: Domain::Presentation,
            bytes: probe_bytes,
        }];
        if let Some(baked) = lightmap::bake(&scene, &settings, path)? {
            let bytes = baked.lightmap.encode();
            Lightmap::parse(&bytes)
                .map_err(|e| CookError::at(path, 0, &format!("baked lightmap does not parse: {e}")))?;
            out.push(Cooked {
                name: format!("{stem}.lightmap"),
                kind: AssetKind::Lightmap,
                domain: Domain::Presentation,
                bytes,
            });
            out.push(Cooked {
                name: format!("{stem}.lightmap_layout"),
                kind: world::LIGHTMAP_LAYOUT_KIND,
                domain: Domain::Presentation,
                bytes: layout_text(&baked.layout).into_bytes(),
            });
        }
        Ok(out)
    }
}

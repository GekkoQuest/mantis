//! World editing: placements in `sectors/<x>_<z>.sector.toml` sources, written back to
//! the files and recooked, then streamed into the live renderer again.
//!
//! - [`WorldEditor`] opens a package's `content/` directory and edits placements by
//!   [`PlacementRef`] (sector and table name): move, turn, scale, add, duplicate, remove.
//!   Positions are world space, as in the sources. A placement moved out of its sector
//!   moves to the source of the sector that now contains it (keeping its fields), so
//!   streaming and baked light stay with the right sector. Edits go through
//!   [`crate::source::SourceDoc`], so comments and untouched lines survive.
//! - [`WorldEditor::save`] writes the changed sources; [`recook`] cooks the package into
//!   its store; [`reload`] releases the old world from the renderer and streams the new
//!   one. Placements are presentation content (decision 0020), so a placement edit never
//!   changes the handshake hash and a running server is unaffected.
//! - [`WorldEditor::pick`] finds the placement a view ray hits first (by a sphere around
//!   each placement), for selection by pointer.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use glam::Vec3;
use mantis_client::content_store::ContentStore;
use mantis_client::world_stream::{Gpu, WorldIndex, WorldStreamer};
use mantis_client::world_view::StreamedWorld;
use mantis_cook::package::{Signing, cook};
use mantis_core::content::ContentHash;
use mantis_core::module::toml::Value;
use mantis_formats::bundle::Domain;
use mantis_render::renderer::Renderer;
use mantis_render::streaming::StreamingConfig;

use crate::source::{EditError, SourceDoc, float, floats, text};

/// Errors of world editing.
#[derive(Debug)]
pub enum WorldEditError {
    /// A source file could not be read or written.
    Io(PathBuf, std::io::Error),
    /// A source does not parse or an edit was refused.
    Edit(PathBuf, EditError),
    /// A sector source lacks its `[sector]` header fields.
    Sector(PathBuf),
    /// No such placement.
    NoPlacement(PlacementRef),
    /// No sector contains this position.
    Outside([f32; 3]),
}

impl core::fmt::Display for WorldEditError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Io(p, e) => write!(f, "{}: {e}", p.display()),
            Self::Edit(p, e) => write!(f, "{}: {e}", p.display()),
            Self::Sector(p) => write!(f, "{}: `[sector]` needs x, z, and size", p.display()),
            Self::NoPlacement(r) => write!(f, "no placement `{}` in sector {:?}", r.name, r.sector),
            Self::Outside(p) => write!(f, "no sector contains {p:?}"),
        }
    }
}

impl std::error::Error for WorldEditError {}

/// A placement: its sector's grid coordinates and its table name in that source.
#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct PlacementRef {
    /// Sector grid coordinates.
    pub sector: (i32, i32),
    /// The `[placement.<name>]` name.
    pub name: String,
}

/// A placement as the editor shows it.
#[derive(Clone, PartialEq, Debug)]
pub struct PlacementInfo {
    /// Which.
    pub at: PlacementRef,
    /// Mesh source path.
    pub mesh: String,
    /// Material source path.
    pub material: String,
    /// World position.
    pub position: [f32; 3],
    /// Yaw, degrees.
    pub yaw: f32,
    /// Uniform scale.
    pub scale: f32,
}

struct SectorSource {
    path: PathBuf,
    doc: SourceDoc,
    size: f32,
    dirty: bool,
}

fn number(v: Option<&Value>) -> Option<f32> {
    match v? {
        Value::Float(s) => s.parse().ok(),
        #[allow(clippy::cast_precision_loss)] // Source integers here are small.
        Value::Int(i) => Some(*i as f32),
        _ => None,
    }
}

fn vec3(v: Option<&Value>) -> Option<[f32; 3]> {
    let Value::Array(items) = v? else { return None };
    let [x, y, z] = items.as_slice() else { return None };
    Some([number(Some(x))?, number(Some(y))?, number(Some(z))?])
}

fn integer(v: Option<&Value>) -> Option<i32> {
    match v? {
        Value::Int(i) => i32::try_from(*i).ok(),
        _ => None,
    }
}

/// The table name of a placement.
fn table(name: &str) -> String {
    format!("placement.{name}")
}

/// The world's sector sources under a package's `content/` directory.
pub struct WorldEditor {
    content: PathBuf,
    sectors: BTreeMap<(i32, i32), SectorSource>,
}

impl core::fmt::Debug for WorldEditor {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("WorldEditor")
            .field("content", &self.content)
            .field("sectors", &self.sectors.len())
            .finish_non_exhaustive()
    }
}

impl WorldEditor {
    /// Opens every `sectors/*.sector.toml` under `content`.
    ///
    /// # Errors
    /// [`WorldEditError`] for an unreadable or malformed source.
    pub fn open(content: &Path) -> Result<Self, WorldEditError> {
        let dir = content.join("sectors");
        let mut sectors = BTreeMap::new();
        let entries = std::fs::read_dir(&dir).map_err(|e| WorldEditError::Io(dir.clone(), e))?;
        let mut paths: Vec<PathBuf> = entries
            .filter_map(Result::ok)
            .map(|e| e.path())
            .filter(|p| p.to_string_lossy().ends_with(".sector.toml"))
            .collect();
        paths.sort();
        for path in paths {
            let text = std::fs::read_to_string(&path).map_err(|e| WorldEditError::Io(path.clone(), e))?;
            let doc = SourceDoc::parse(&text).map_err(|e| WorldEditError::Edit(path.clone(), e))?;
            let x = integer(doc.get("sector", "x"));
            let z = integer(doc.get("sector", "z"));
            let size = number(doc.get("sector", "size"));
            let (Some(x), Some(z), Some(size)) = (x, z, size) else {
                return Err(WorldEditError::Sector(path));
            };
            sectors.insert(
                (x, z),
                SectorSource {
                    path,
                    doc,
                    size,
                    dirty: false,
                },
            );
        }
        Ok(Self {
            content: content.to_path_buf(),
            sectors,
        })
    }

    /// The content directory.
    pub fn content(&self) -> &Path {
        &self.content
    }

    /// The sector containing world position `p` (on x and z).
    #[allow(clippy::cast_possible_truncation)] // Grid coordinates are far inside i32.
    pub fn sector_of(&self, p: [f32; 3]) -> Option<(i32, i32)> {
        self.sectors.iter().find_map(|(coord, s)| {
            let x = (p[0] / s.size).floor() as i32;
            let z = (p[2] / s.size).floor() as i32;
            ((x, z) == *coord).then_some(*coord)
        })
    }

    fn info(&self, coord: (i32, i32), name: &str) -> Option<PlacementInfo> {
        let s = self.sectors.get(&coord)?;
        let t = table(name);
        let get_str = |k: &str| s.doc.get(&t, k).and_then(Value::as_str).map(str::to_owned);
        Some(PlacementInfo {
            at: PlacementRef {
                sector: coord,
                name: name.to_owned(),
            },
            mesh: get_str("mesh")?,
            material: get_str("material").unwrap_or_default(),
            position: vec3(s.doc.get(&t, "position"))?,
            yaw: number(s.doc.get(&t, "yaw")).unwrap_or(0.0),
            scale: number(s.doc.get(&t, "scale")).unwrap_or(1.0),
        })
    }

    /// Every placement, sector by sector in grid order, then in file order.
    pub fn placements(&self) -> Vec<PlacementInfo> {
        self.sectors
            .iter()
            .flat_map(|(coord, s)| {
                s.doc
                    .tables_under("placement.")
                    .into_iter()
                    .filter_map(|name| self.info(*coord, &name))
                    .collect::<Vec<_>>()
            })
            .collect()
    }

    /// One placement.
    pub fn placement(&self, at: &PlacementRef) -> Option<PlacementInfo> {
        self.info(at.sector, &at.name)
    }

    fn source_mut(&mut self, at: &PlacementRef) -> Result<&mut SectorSource, WorldEditError> {
        let exists = self
            .sectors
            .get(&at.sector)
            .is_some_and(|s| s.doc.document().table(&table(&at.name)).is_some());
        if !exists {
            return Err(WorldEditError::NoPlacement(at.clone()));
        }
        self.sectors
            .get_mut(&at.sector)
            .ok_or_else(|| WorldEditError::NoPlacement(at.clone()))
    }

    fn set(&mut self, at: &PlacementRef, key: &str, value: &Value) -> Result<(), WorldEditError> {
        let s = self.source_mut(at)?;
        s.doc
            .set(&table(&at.name), key, value)
            .map_err(|e| WorldEditError::Edit(s.path.clone(), e))?;
        s.dirty = true;
        Ok(())
    }

    /// A placement name not yet used in sector `coord`, based on `base`.
    fn free_name(&self, coord: (i32, i32), base: &str) -> String {
        let taken = |n: &str| {
            self.sectors
                .get(&coord)
                .is_some_and(|s| s.doc.document().table(&table(n)).is_some())
        };
        if !taken(base) {
            return base.to_owned();
        }
        (2u32..=u32::MAX)
            .map(|i| format!("{base}_{i}"))
            .find(|n| !taken(n))
            .unwrap_or_else(|| base.to_owned())
    }

    /// Moves a placement to world position `to`. A position in another sector moves the
    /// placement's table to that sector's source (renamed if the name is taken there);
    /// the returned reference names it from now on.
    ///
    /// # Errors
    /// [`WorldEditError::NoPlacement`], [`WorldEditError::Outside`], or an edit refusal.
    pub fn move_to(&mut self, at: &PlacementRef, to: [f32; 3]) -> Result<PlacementRef, WorldEditError> {
        let target = self.sector_of(to).ok_or(WorldEditError::Outside(to))?;
        if target == at.sector {
            self.set(at, "position", &floats(&to))?;
            return Ok(at.clone());
        }
        let entries: Vec<(String, Value)> = {
            let s = self.source_mut(at)?;
            s.doc
                .document()
                .table(&table(&at.name))
                .map(|t| {
                    t.entries
                        .iter()
                        .map(|e| (e.key.clone(), e.value.clone()))
                        .collect()
                })
                .unwrap_or_default()
        };
        let name = self.free_name(target, &at.name);
        let moved: Vec<(&str, Value)> = entries
            .iter()
            .map(|(k, v)| (k.as_str(), if k == "position" { floats(&to) } else { v.clone() }))
            .collect();
        let dest = self.sectors.get_mut(&target).ok_or(WorldEditError::Outside(to))?;
        dest.doc
            .add_table(&table(&name), &moved)
            .map_err(|e| WorldEditError::Edit(dest.path.clone(), e))?;
        dest.dirty = true;
        self.remove(at)?;
        Ok(PlacementRef { sector: target, name })
    }

    /// Sets the yaw (degrees).
    ///
    /// # Errors
    /// [`WorldEditError::NoPlacement`] or an edit refusal.
    pub fn set_yaw(&mut self, at: &PlacementRef, degrees: f32) -> Result<(), WorldEditError> {
        self.set(at, "yaw", &float(degrees))
    }

    /// Adds a placement of `mesh` drawn with `material` at world position `position`
    /// (unlit by baked light; a 200 m single level of detail), named after `name` in the
    /// sector that contains it.
    ///
    /// # Errors
    /// [`WorldEditError::Outside`] or an edit refusal.
    pub fn add(
        &mut self,
        name: &str,
        mesh: &str,
        material: &str,
        position: [f32; 3],
    ) -> Result<PlacementRef, WorldEditError> {
        let coord = self
            .sector_of(position)
            .ok_or(WorldEditError::Outside(position))?;
        let name = self.free_name(coord, name);
        let s = self
            .sectors
            .get_mut(&coord)
            .ok_or(WorldEditError::Outside(position))?;
        s.doc
            .add_table(
                &table(&name),
                &[
                    ("mesh", text(mesh)),
                    ("material", text(material)),
                    ("position", floats(&position)),
                    ("yaw", float(0.0)),
                    ("scale", float(1.0)),
                    ("lods", floats(&[200.0])),
                    ("casts_shadows", Value::Bool(true)),
                ],
            )
            .map_err(|e| WorldEditError::Edit(s.path.clone(), e))?;
        s.dirty = true;
        Ok(PlacementRef { sector: coord, name })
    }

    /// Copies a placement to world position `to`.
    ///
    /// # Errors
    /// As [`WorldEditor::move_to`].
    pub fn duplicate(&mut self, at: &PlacementRef, to: [f32; 3]) -> Result<PlacementRef, WorldEditError> {
        let entries: Vec<(String, Value)> = self
            .sectors
            .get(&at.sector)
            .and_then(|s| s.doc.document().table(&table(&at.name)))
            .map(|t| {
                t.entries
                    .iter()
                    .map(|e| (e.key.clone(), e.value.clone()))
                    .collect()
            })
            .ok_or_else(|| WorldEditError::NoPlacement(at.clone()))?;
        let coord = self.sector_of(to).ok_or(WorldEditError::Outside(to))?;
        let name = self.free_name(coord, &at.name);
        let copy: Vec<(&str, Value)> = entries
            .iter()
            .map(|(k, v)| (k.as_str(), if k == "position" { floats(&to) } else { v.clone() }))
            .collect();
        let s = self.sectors.get_mut(&coord).ok_or(WorldEditError::Outside(to))?;
        s.doc
            .add_table(&table(&name), &copy)
            .map_err(|e| WorldEditError::Edit(s.path.clone(), e))?;
        s.dirty = true;
        Ok(PlacementRef { sector: coord, name })
    }

    /// Removes a placement.
    ///
    /// # Errors
    /// [`WorldEditError::NoPlacement`] or an edit refusal.
    pub fn remove(&mut self, at: &PlacementRef) -> Result<(), WorldEditError> {
        let s = self.source_mut(at)?;
        s.doc
            .remove_table(&table(&at.name))
            .map_err(|e| WorldEditError::Edit(s.path.clone(), e))?;
        s.dirty = true;
        Ok(())
    }

    /// The source files changed since the last save.
    pub fn dirty(&self) -> Vec<&Path> {
        self.sectors
            .values()
            .filter(|s| s.dirty)
            .map(|s| s.path.as_path())
            .collect()
    }

    /// Writes every changed source; returns the files written.
    ///
    /// # Errors
    /// [`WorldEditError::Io`].
    pub fn save(&mut self) -> Result<Vec<PathBuf>, WorldEditError> {
        let mut written = Vec::new();
        for s in self.sectors.values_mut().filter(|s| s.dirty) {
            std::fs::write(&s.path, s.doc.text()).map_err(|e| WorldEditError::Io(s.path.clone(), e))?;
            s.dirty = false;
            written.push(s.path.clone());
        }
        Ok(written)
    }

    /// The placement a ray from `origin` along `direction` hits first, treating each
    /// placement as a sphere of `radius` meters times its scale around its position.
    pub fn pick(&self, origin: Vec3, direction: Vec3, radius: f32) -> Option<PlacementRef> {
        let dir = direction.try_normalize()?;
        self.placements()
            .into_iter()
            .filter_map(|p| {
                let center = Vec3::from_array(p.position) + Vec3::Y * radius * p.scale;
                let r = radius * p.scale;
                let to = center - origin;
                let along = to.dot(dir);
                let miss2 = to.length_squared() - along * along;
                (along > 0.0 && miss2 <= r * r).then(|| (along - (r * r - miss2).sqrt(), p.at))
            })
            .min_by(|a, b| a.0.total_cmp(&b.0))
            .map(|(_, at)| at)
    }
}

/// The outcome of a recook.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Recooked {
    /// The gameplay bundle hash (the handshake hash).
    pub gameplay: ContentHash,
    /// The presentation bundle hash.
    pub presentation: ContentHash,
    /// Assets cooked.
    pub assets: usize,
}

/// Cooks the package at `content` into the store at `store` with a development key.
///
/// # Errors
/// Every cook error as `file:line: message` lines.
pub fn recook(content: &Path, store: &Path) -> Result<Recooked, String> {
    let c = cook(content, store, 1, &Signing::Development).map_err(|errors| {
        errors
            .iter()
            .map(|e| format!("{}:{}: {}", e.file, e.line, e.message))
            .collect::<Vec<_>>()
            .join("\n")
    })?;
    let [gameplay, _, presentation] = c.hashes;
    Ok(Recooked {
        gameplay,
        presentation,
        assets: c.assets,
    })
}

/// Replaces the world `world` streams into `renderer` with the store at `store`
/// (verified against its development key): the old world's sectors and assets leave the
/// renderer first, then the new world's materials compile, and the world streams in again
/// from the next update. Returns the materials compiled.
///
/// # Errors
/// A refused bundle or malformed sector, as text.
pub fn reload(
    world: &mut StreamedWorld,
    renderer: &mut Renderer,
    gpu: &Gpu<'_>,
    store: &Path,
    config: StreamingConfig,
    workers: usize,
) -> Result<usize, String> {
    let key = mantis_cook::package::dev_public_key(store).map_err(|e| e.to_string())?;
    let content = ContentStore::open(store);
    let gameplay = content
        .bundle(Domain::Gameplay, &key)
        .map_err(|e| e.to_string())?;
    let presentation = content
        .bundle(Domain::Presentation, &key)
        .map_err(|e| e.to_string())?;
    let index = WorldIndex::build(&content, &gameplay, &presentation).map_err(|e| e.to_string())?;
    let streamer = WorldStreamer::new(content, index, config, workers).map_err(|e| e.to_string())?;
    let _ = world.streamer.release_all(renderer, gpu);
    world.streamer = streamer;
    world
        .streamer
        .preload_materials(renderer, gpu)
        .map_err(|e| e.to_string())
}

/// A streamed world over the store at `store` (its development key), for a renderer the
/// editor drives.
///
/// # Errors
/// A refused bundle or malformed sector, as text.
pub fn open_world(
    store: &Path,
    config: StreamingConfig,
    workers: usize,
    clock: Arc<dyn mantis_client::time::HostClock>,
) -> Result<StreamedWorld, String> {
    let key = mantis_cook::package::dev_public_key(store).map_err(|e| e.to_string())?;
    let content = ContentStore::open(store);
    let gameplay = content
        .bundle(Domain::Gameplay, &key)
        .map_err(|e| e.to_string())?;
    let presentation = content
        .bundle(Domain::Presentation, &key)
        .map_err(|e| e.to_string())?;
    let index = WorldIndex::build(&content, &gameplay, &presentation).map_err(|e| e.to_string())?;
    Ok(StreamedWorld {
        streamer: WorldStreamer::new(content, index, config, workers).map_err(|e| e.to_string())?,
        clock,
        budget: mantis_client::world_stream::HANDOFF_BUDGET,
        last: mantis_client::world_stream::WorldStats::default(),
    })
}

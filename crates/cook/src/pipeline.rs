//! Running importers over a content tree.

use std::collections::BTreeMap;
use std::sync::Arc;

use mantis_core::content::ContentHash;
use mantis_formats::bundle::{AssetKind, Bundle, Domain, Entry};

use crate::importer::{CookError, ImportContext, Importer, Produced};
use crate::tree::ContentTree;

/// One cooked payload in the output.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct CookedAsset {
    /// What it is.
    pub kind: AssetKind,
    /// Its bundle.
    pub domain: Domain,
    /// The source it came from.
    pub source: String,
    /// BLAKE3 of the bytes.
    pub hash: ContentHash,
    /// The payload.
    pub bytes: Vec<u8>,
}

/// Everything one cook produced.
#[derive(Clone, Debug, Default)]
pub struct CookOutput {
    /// Payloads by output name.
    pub assets: BTreeMap<String, CookedAsset>,
    /// Importer name to version, for every importer that ran.
    pub importers: BTreeMap<&'static str, u32>,
}

impl CookOutput {
    /// The bundle manifest of `domain`.
    pub fn bundle(&self, domain: Domain, content_version: u32) -> Bundle {
        Bundle {
            domain,
            content_version,
            entries: self
                .assets
                .iter()
                .filter(|(_, a)| a.domain == domain)
                .map(|(name, a)| Entry {
                    name: name.clone(),
                    kind: a.kind,
                    size: a.bytes.len() as u64,
                    hash: a.hash,
                })
                .collect(),
        }
    }

    /// The asset named `name`.
    pub fn get(&self, name: &str) -> Option<&CookedAsset> {
        self.assets.get(name)
    }
}

/// A configured cook.
#[derive(Clone)]
pub struct Cook {
    importers: Vec<Arc<dyn Importer>>,
}

impl core::fmt::Debug for Cook {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Cook")
            .field(
                "importers",
                &self
                    .importers
                    .iter()
                    .map(|i| (i.name(), i.version()))
                    .collect::<Vec<_>>(),
            )
            .finish()
    }
}

/// Documentation and notes, which the cook passes over without an importer.
pub fn is_note(path: &str) -> bool {
    std::path::Path::new(path)
        .extension()
        .is_some_and(|e| e.eq_ignore_ascii_case("md") || e.eq_ignore_ascii_case("txt"))
}

impl Cook {
    /// A cook with `importers` (built-ins and package plugins).
    ///
    /// # Errors
    /// [`CookError`] when two importers share a name.
    pub fn new(mut importers: Vec<Arc<dyn Importer>>) -> Result<Cook, CookError> {
        importers.sort_by_key(|i| (i.phase(), i.name()));
        if let Some(w) = importers
            .windows(2)
            .find(|w| matches!(w, [a, b] if a.name() == b.name()))
        {
            let name = w.first().map_or("", |i| i.name());
            return Err(CookError::at(
                "<importers>",
                0,
                &format!("importer `{name}` is registered twice"),
            ));
        }
        Ok(Cook { importers })
    }

    /// Cooks every source of `tree`.
    ///
    /// # Errors
    /// Every error found (the cook keeps going after a failed source so one run reports
    /// them all): sources no importer or more than one importer accepts, importer errors,
    /// and duplicate output names.
    pub fn run(&self, tree: &ContentTree) -> Result<CookOutput, Vec<CookError>> {
        let mut errors = Vec::new();
        let mut out = CookOutput::default();
        let mut produced: BTreeMap<String, Vec<Produced>> = BTreeMap::new();
        for source in tree.sources() {
            let n = self.importers.iter().filter(|i| i.accepts(source.path)).count();
            let input = self.importers.iter().any(|i| i.inputs(source.path));
            if n == 0 && !input && !is_note(source.path) {
                errors.push(CookError::at(source.path, 0, "no importer accepts this file"));
            }
            let mut phases: Vec<u32> = self
                .importers
                .iter()
                .filter(|i| !input && i.accepts(source.path))
                .map(|i| i.phase())
                .collect();
            phases.sort_unstable();
            if phases.windows(2).any(|w| matches!(w, [a, b] if a == b)) {
                errors.push(CookError::at(
                    source.path,
                    0,
                    "more than one importer in one phase accepts this file",
                ));
            }
        }
        let mut phases: Vec<u32> = self.importers.iter().map(|i| i.phase()).collect();
        phases.dedup();
        for phase in phases {
            let mut this_phase: Vec<(String, Produced)> = Vec::new();
            for importer in self.importers.iter().filter(|i| i.phase() == phase) {
                out.importers.insert(importer.name(), importer.version());
                let mut results = Vec::new();
                {
                    let ctx = ImportContext {
                        tree,
                        assets: &out.assets,
                        produced: &produced,
                        phase,
                        importer: importer.name(),
                    };
                    // A file some importer reads as its input (a sidecar, a heightmap) is
                    // never cooked on its own, even when another importer accepts its
                    // extension.
                    for source in tree.sources().filter(|s| {
                        importer.accepts(s.path) && !self.importers.iter().any(|i| i.inputs(s.path))
                    }) {
                        results.push((source.path.to_owned(), importer.import(&source, &ctx)));
                    }
                }
                for (path, result) in results {
                    match result {
                        Ok(cooked) => {
                            for c in cooked {
                                let hash = ContentHash::of(&c.bytes);
                                if out.assets.contains_key(&c.name) {
                                    errors.push(CookError::at(
                                        &path,
                                        0,
                                        &format!("output `{}` is produced twice", c.name),
                                    ));
                                    continue;
                                }
                                this_phase.push((
                                    path.clone(),
                                    Produced {
                                        name: c.name.clone(),
                                        kind: c.kind,
                                        hash,
                                    },
                                ));
                                out.assets.insert(
                                    c.name,
                                    CookedAsset {
                                        kind: c.kind,
                                        domain: c.domain,
                                        source: path.clone(),
                                        hash,
                                        bytes: c.bytes,
                                    },
                                );
                            }
                        }
                        Err(e) => errors.push(e),
                    }
                }
            }
            // Outputs become visible to later phases only.
            for (path, p) in this_phase {
                produced.entry(path).or_default().push(p);
            }
        }
        if errors.is_empty() {
            check_sector_domains(&out, &mut errors);
        }
        if errors.is_empty() { Ok(out) } else { Err(errors) }
    }
}

/// Decision 0020, enforced by chunk id for every importer: sector containers in the
/// gameplay and server bundles hold only gameplay chunks, those in the presentation bundle
/// only visual chunks, and every visual container has a gameplay sector at the same
/// coordinates.
pub fn check_sector_domains(out: &CookOutput, errors: &mut Vec<CookError>) {
    use mantis_formats::sector::{Sector, chunk};
    let mut gameplay = std::collections::BTreeSet::new();
    let mut visuals = Vec::new();
    for (name, asset) in &out.assets {
        if asset.kind != AssetKind::Sector {
            continue;
        }
        let Ok(sector) = Sector::parse(&asset.bytes) else {
            errors.push(CookError::at(
                &asset.source,
                0,
                &format!("`{name}` is not a sector container"),
            ));
            continue;
        };
        let allowed: &[[u8; 4]] = match asset.domain {
            Domain::Gameplay | Domain::Server => &chunk::GAMEPLAY,
            Domain::Presentation => &chunk::VISUAL,
        };
        for id in sector.chunk_ids() {
            if !allowed.contains(&id) {
                errors.push(CookError::at(
                    &asset.source,
                    0,
                    &format!(
                        "`{name}` holds chunk `{}`, which does not belong in the {:?} domain (decision 0020)",
                        String::from_utf8_lossy(&id),
                        asset.domain
                    ),
                ));
            }
        }
        let at = (sector.info.sector_x, sector.info.sector_z);
        match asset.domain {
            Domain::Gameplay => {
                gameplay.insert(at);
            }
            Domain::Presentation => visuals.push((name, &asset.source, at)),
            Domain::Server => {}
        }
    }
    for (name, source, at) in visuals {
        if !gameplay.contains(&at) {
            errors.push(CookError::at(
                source,
                0,
                &format!("`{name}` is the visual of sector {at:?}, which has no gameplay sector"),
            ));
        }
    }
}

//! A cell's gameplay content (decision 0021): the graph catalog and the
//! ability table, loaded from the package's verified gameplay bundle.
//!
//! The catalog registers the module set's declared graph actions first, in
//! [`ModuleGraph::graph_actions`] order, so action ids are the same on every
//! machine; then every cooked `GameplayGraph` entry is parsed, its action
//! names resolved against those ids, and inserted. `tables/abilities` maps
//! ability ids to graph names, one `<ability id> <graph name>` row per line,
//! `#` starting a comment.
//!
//! [`ModuleGraph::graph_actions`]: mantis_core::module::ModuleGraph::graph_actions

use core::fmt;
use std::collections::BTreeMap;
use std::path::Path;
use std::sync::Arc;

use mantis_adapter_contract::AbilityId;
use mantis_core::content::ContentHash;
use mantis_core::graph::{GraphCatalog, GraphError, GraphId};
use mantis_formats::bundle::{AssetKind, Bundle, Domain, SignedBundle};
use mantis_formats::gameplay_graph::{GraphAsset, GraphLoadError};

/// Where the ability table lives in a gameplay bundle.
pub const ABILITIES: &str = "tables/abilities";

/// A cell's gameplay content.
#[derive(Clone, Debug)]
pub struct Gameplay {
    /// The graph catalog (actions, then graphs).
    pub catalog: Arc<GraphCatalog>,
    /// Ability id to the graph it runs.
    pub abilities: BTreeMap<AbilityId, GraphId>,
    /// The gameplay bundle's hash (the handshake's content hash).
    pub hash: ContentHash,
}

/// Why gameplay content was refused. The server must not start.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum GameplayError {
    /// A file could not be read.
    Io(String),
    /// The bundle is malformed, wrongly signed, or not a gameplay bundle.
    Bundle(String),
    /// A payload does not match its listed hash.
    Corrupt(String),
    /// A graph asset could not become a core graph.
    Graph(String, GraphLoadError),
    /// The catalog refused a graph or an action.
    Catalog(String, GraphError),
    /// A row of the ability table is malformed or names no cooked graph.
    Ability {
        /// The 1-based line.
        line: usize,
        /// Why.
        why: &'static str,
    },
}

impl fmt::Display for GameplayError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(e) => write!(f, "{e}"),
            Self::Bundle(e) => write!(f, "gameplay bundle: {e}"),
            Self::Corrupt(name) => write!(f, "gameplay bundle: {name} does not match its hash"),
            Self::Graph(name, e) => write!(f, "{name}: {e}"),
            Self::Catalog(name, e) => write!(f, "{name}: {e}"),
            Self::Ability { line, why } => write!(f, "{ABILITIES}:{line}: {why}"),
        }
    }
}

impl std::error::Error for GameplayError {}

impl Gameplay {
    /// Loads a cooked package directory: `bundles/gameplay.bundle`, verified
    /// with `public_key`, its payloads from `objects/` (each checked
    /// against its listed hash). `actions` are the module set's declared
    /// graph actions, in order.
    ///
    /// # Errors
    /// [`GameplayError`].
    pub fn load(cooked: &Path, public_key: &[u8], actions: &[String]) -> Result<Self, GameplayError> {
        let path = cooked.join("bundles").join("gameplay.bundle");
        let bytes =
            std::fs::read(&path).map_err(|e| GameplayError::Io(format!("{}: {e}", path.display())))?;
        let signed = SignedBundle::parse(&bytes).map_err(|e| GameplayError::Bundle(e.to_string()))?;
        let bundle = signed
            .verify(public_key)
            .map_err(|e| GameplayError::Bundle(e.to_string()))?;
        Self::from_bundle(bundle, actions, |hash| {
            let hex = hash.to_string();
            let shard = hex.get(..2).unwrap_or("00");
            let p = cooked.join("objects").join(shard).join(&hex);
            std::fs::read(&p).map_err(|e| GameplayError::Io(format!("{}: {e}", p.display())))
        })
    }

    /// Builds from a verified bundle, reading each payload through `read`
    /// (by hash; the bytes are checked against it here).
    ///
    /// # Errors
    /// [`GameplayError`].
    pub fn from_bundle(
        bundle: &Bundle,
        actions: &[String],
        mut read: impl FnMut(&ContentHash) -> Result<Vec<u8>, GameplayError>,
    ) -> Result<Self, GameplayError> {
        if bundle.domain != Domain::Gameplay {
            return Err(GameplayError::Bundle("not a gameplay bundle".to_owned()));
        }
        let mut payload = |name: &str, hash: &ContentHash| {
            let bytes = read(hash)?;
            if ContentHash::of(&bytes) == *hash {
                Ok(bytes)
            } else {
                Err(GameplayError::Corrupt(name.to_owned()))
            }
        };
        let mut catalog = GraphCatalog::new();
        for a in actions {
            catalog
                .register_action(a.clone())
                .map_err(|e| GameplayError::Catalog(a.clone(), e))?;
        }
        for entry in bundle
            .entries
            .iter()
            .filter(|e| e.kind == AssetKind::GameplayGraph)
        {
            let bytes = payload(&entry.name, &entry.hash)?;
            let asset = GraphAsset::parse(&bytes)
                .map_err(|e| GameplayError::Graph(entry.name.clone(), GraphLoadError::Format(e)))?;
            let graph = asset
                .to_core(|name| catalog.action_id(name))
                .map_err(|e| GameplayError::Graph(entry.name.clone(), e))?;
            catalog
                .insert(graph)
                .map_err(|e| GameplayError::Catalog(entry.name.clone(), e))?;
        }
        let abilities = match bundle.get(ABILITIES) {
            Some(entry) => parse_abilities(&payload(&entry.name, &entry.hash)?, &catalog)?,
            None => BTreeMap::new(),
        };
        Ok(Self {
            catalog: Arc::new(catalog),
            abilities,
            hash: bundle.hash(),
        })
    }
}

/// Parses the ability table against the loaded graphs.
///
/// # Errors
/// [`GameplayError::Ability`].
pub fn parse_abilities(
    bytes: &[u8],
    catalog: &GraphCatalog,
) -> Result<BTreeMap<AbilityId, GraphId>, GameplayError> {
    let text = std::str::from_utf8(bytes).map_err(|_| GameplayError::Ability {
        line: 0,
        why: "not UTF-8",
    })?;
    let mut out = BTreeMap::new();
    for (n, raw) in text.lines().enumerate() {
        let line = n + 1;
        let row = raw.split_once('#').map_or(raw, |(r, _)| r).trim();
        if row.is_empty() {
            continue;
        }
        let mut parts = row.split_whitespace();
        let (Some(id), Some(name), None) = (parts.next(), parts.next(), parts.next()) else {
            return Err(GameplayError::Ability {
                line,
                why: "expected `<ability id> <graph name>`",
            });
        };
        let id = id.parse::<u32>().map_err(|_| GameplayError::Ability {
            line,
            why: "the ability id is not a u32",
        })?;
        let graph = GraphId::named(name);
        if catalog.get(graph).is_none() {
            return Err(GameplayError::Ability {
                line,
                why: "names no cooked graph",
            });
        }
        if out.insert(AbilityId(id), graph).is_some() {
            return Err(GameplayError::Ability {
                line,
                why: "the ability id is listed twice",
            });
        }
    }
    Ok(out)
}

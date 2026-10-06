//! The toy world: one zone of two cells, one class, two adapters.

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::{Arc, OnceLock};

use mantis_adapter_contract::core_types::ContentHash;
use mantis_adapter_contract::{AbilityId, AppearanceId, Transport, WireAdapter};
use mantis_core::graph::GraphId;
use mantis_core::kinematics::{FlatGround, GroundQuery};
use mantis_core::log::{CellId, LogWriter, SessionId};
use mantis_core::math::Vec3;
use mantis_core::module::{Discovered, parse_manifest, parse_package, resolve};
use mantis_core::rng::Seed;
use mantis_formats::bundle::{Domain, SignedBundle};
use mantis_net::handshake::ServerPolicy;
use mantis_server::cell::{BoxedSink, Cell, CellConfig, CellError};
use mantis_server::gameplay::Gameplay;
use mantis_server::host::{Host, HostConfig, Listener};
use mantis_server::intent::CellLogSchema;
use mantis_server::modules::ModuleSet;
use mantis_server::zone::Zone;
use toy_adapter_legacy::{LegacyAdapter, LegacyConfig};

use crate::tunables::{PACKAGE_TOML, Tunables};

/// The legacy client build this package serves.
pub const LEGACY_BUILD: u32 = 0x0001_0007;

/// Half the zone's width, in metres: cell 0 owns `[-HALF_WIDTH, 0)` and
/// cell 1 owns `[0, HALF_WIDTH)`.
pub const HALF_WIDTH: f32 = 1000.0;

/// Width of the strip along the border whose entities are ghosted to the
/// other cell, in metres.
pub const GHOST_MARGIN: f32 = 30.0;

/// The class's appearance.
pub const CLASS_LOOK: AppearanceId = AppearanceId(1);

/// The class's one ability.
pub const PULSE: AbilityId = AbilityId(1);

/// The ability's gameplay graph.
pub const PULSE_GRAPH: GraphId = GraphId::named("toy.ability.pulse");

/// Sessions the zone accepts.
pub const CAPACITY: usize = 256;

/// The manifest hash: the content hash of tools and tests that do not cook
/// (the default of [`Tunables::content`]).
#[must_use]
pub fn uncooked_content() -> ContentHash {
    ContentHash::of(PACKAGE_TOML.as_bytes())
}

/// Where the cook writes the package (`cargo run -p mantis-cook -- packages/toy`).
pub const COOKED_DIR: &str = "packages/toy/cooked";

/// The package's content hash, as clients must match it: the hash of the
/// cooked gameplay bundle in `cooked`, verified against `public_key` (the
/// development key the same cook wrote, `cooked/keys/dev.pub`, when
/// `None`; production passes the release key from Ops).
///
/// # Errors
/// Why the bundle cannot be trusted: missing, malformed, wrongly signed,
/// or not a gameplay bundle. The server must not start.
pub fn cooked_content(cooked: &Path, public_key: Option<&Path>) -> Result<ContentHash, String> {
    let read = |p: &Path| std::fs::read(p).map_err(|e| format!("{}: {e}", p.display()));
    let key_path = public_key.map_or_else(|| cooked.join("keys").join("dev.pub"), Path::to_path_buf);
    let key = read(&key_path)?;
    let bundle_path = cooked.join("bundles").join("gameplay.bundle");
    let bytes = read(&bundle_path)?;
    let signed = SignedBundle::parse(&bytes).map_err(|e| format!("{}: {e}", bundle_path.display()))?;
    let bundle = signed
        .verify(&key)
        .map_err(|e| format!("{}: {e}", bundle_path.display()))?;
    if bundle.domain != Domain::Gameplay {
        return Err(format!("{}: not a gameplay bundle", bundle_path.display()));
    }
    Ok(bundle.hash())
}

/// The content tables the package's modules read.
pub const TABLES: &[(&str, &[u8])] = &[
    (
        "std.vendor.stock",
        include_bytes!("../../content/tables/std.vendor.stock"),
    ),
    (
        "std.auction.rules",
        include_bytes!("../../content/tables/std.auction.rules"),
    ),
    (
        "std.titles.list",
        include_bytes!("../../content/tables/std.titles.list"),
    ),
];

/// Resolves the package's module graph from the linked manifests, the
/// package's flags, and `live` flags (Ops), and pairs it with the linked
/// modules. This is the start-up refusal point: a missing or disabled
/// dependency stops the server here, with the reason.
///
/// # Errors
/// Why the modules cannot start.
pub fn module_set(live: &BTreeMap<String, bool>) -> Result<ModuleSet, String> {
    module_set_with(live, &[], Vec::new())
}

/// [`module_set`] plus extra modules of the package itself (manifest text
/// and implementation), for tests and tools that add, say, a script module.
///
/// # Errors
/// Why the modules cannot start.
pub fn module_set_with(
    live: &BTreeMap<String, bool>,
    extra_manifests: &[&str],
    extra: Vec<Arc<dyn mantis_server::modules::ServerModule>>,
) -> Result<ModuleSet, String> {
    let package = parse_package(PACKAGE_TOML).map_err(|e| format!("package.toml: {e}"))?;
    let package_name = package.name.clone();
    let extra_found = extra_manifests.iter().map(|text| {
        parse_manifest(text).map(|manifest| Discovered {
            origin: package_name.clone(),
            manifest,
        })
    });
    let found = crate::modules::MANIFESTS
        .iter()
        .map(|(origin, text)| {
            parse_manifest(text).map(|manifest| Discovered {
                origin: (*origin).to_owned(),
                manifest,
            })
        })
        .chain(extra_found)
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| format!("module manifest: {e}"))?;
    let graph = resolve(&package, &found, live).map_err(|e| e.to_string())?;
    let mut linked = crate::modules::linked();
    linked.extend(extra);
    let mut set = ModuleSet::new(graph, &linked).map_err(|e| e.to_string())?;
    for (name, bytes) in TABLES {
        set = set.with_table(name, Arc::from(*bytes));
    }
    Ok(set)
}

/// The zone's ground.
#[must_use]
pub fn ground() -> Arc<dyn GroundQuery + Send + Sync> {
    Arc::new(FlatGround(0.0))
}

/// Where instance cells start, in metres along x: far from the world, so
/// no world entity ever crosses into one.
pub const INSTANCE_X: f32 = 100_000.0;

/// The toy's competitive queue: its instances permit presentation-only
/// client modules (plan 12). Other queues permit automation too.
pub const COMPETITIVE_QUEUE: u16 = 2;

/// The client module tier of an instance formed by `queue`.
#[must_use]
pub fn queue_tier(queue: u16) -> mantis_adapter_contract::ModTier {
    if queue == COMPETITIVE_QUEUE {
        mantis_adapter_contract::ModTier::Presentation
    } else {
        mantis_adapter_contract::ModTier::Automation
    }
}

/// Width of one instance cell's range, in metres.
pub const INSTANCE_WIDTH: f32 = 2_000.0;

/// The x range of instance `k`.
#[must_use]
pub fn instance_region(k: usize) -> (f32, f32) {
    let lo = INSTANCE_X + INSTANCE_WIDTH * u16::try_from(k).map_or(0.0, f32::from);
    (lo, lo + INSTANCE_WIDTH)
}

/// Where the `n`th member of a group enters instance `k`.
#[must_use]
pub fn instance_spawn(k: usize, n: u64) -> Vec3 {
    let (lo, _) = instance_region(k);
    let n = u16::try_from(n % 16).map_or(0.0, f32::from);
    Vec3::new(lo + 20.0 + 2.0 * n, 0.0, 0.0)
}

/// The x range of cell `index`: the world cells first, then instance cells.
#[must_use]
pub fn region_of(index: usize) -> (f32, f32) {
    let world = regions();
    world
        .get(index)
        .copied()
        .unwrap_or_else(|| instance_region(index - world.len()))
}

/// The two cells' x ranges.
#[must_use]
pub fn regions() -> Vec<(f32, f32)> {
    vec![(-HALF_WIDTH, 0.0), (0.0, HALF_WIDTH)]
}

/// The legacy adapter, configured for this package.
#[must_use]
pub fn legacy_adapter(content: ContentHash) -> LegacyAdapter {
    LegacyAdapter::new(LegacyConfig {
        build: LEGACY_BUILD,
        content,
    })
}

/// The cooked package the toy loads by default (the checked-in cook).
pub const COOKED_PATH: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../cooked");

static GAMEPLAY: OnceLock<Result<Arc<Gameplay>, String>> = OnceLock::new();

/// Selects the cooked package cells load their gameplay content from
/// (`--cooked` and `--key`; the development key that cook wrote when
/// `public_key` is `None`). Call once, before the first cell is built.
///
/// # Errors
/// Why the content cannot be trusted, or that cells already loaded content.
pub fn use_gameplay(cooked: &Path, public_key: Option<&Path>) -> Result<Arc<Gameplay>, String> {
    let loaded = load_gameplay(cooked, public_key);
    GAMEPLAY
        .set(loaded.clone())
        .map_err(|_| "gameplay content is already loaded".to_owned())?;
    loaded
}

/// The cells' gameplay content: the graph catalog and ability table from
/// the verified gameplay bundle (decision 0021), loaded once per process
/// (from [`COOKED_PATH`] unless [`use_gameplay`] chose another cook).
///
/// # Errors
/// Why the content cannot be trusted; cells refuse to start.
pub fn gameplay() -> Result<Arc<Gameplay>, String> {
    GAMEPLAY
        .get_or_init(|| load_gameplay(Path::new(COOKED_PATH), None))
        .clone()
}

fn load_gameplay(cooked: &Path, public_key: Option<&Path>) -> Result<Arc<Gameplay>, String> {
    let key_path = public_key.map_or_else(|| cooked.join("keys").join("dev.pub"), Path::to_path_buf);
    let key = std::fs::read(&key_path).map_err(|e| format!("{}: {e}", key_path.display()))?;
    let actions = module_set(&BTreeMap::new())?.graph().graph_actions();
    Gameplay::load(cooked, &key, &actions)
        .map(Arc::new)
        .map_err(|e| e.to_string())
}

/// The configuration of cell `index`.
#[must_use]
pub fn cell_config(t: &Tunables, index: usize, seed: u64) -> CellConfig {
    let (lo, hi) = region_of(index);
    let id = CellId(index as u64 + 1);
    let mut cfg = CellConfig::new(id, Seed(seed ^ id.0));
    cfg.rate = t.tick_rate;
    cfg.motion = t.motion;
    cfg.envelope = t.envelope;
    cfg.tiers = t.interest;
    cfg.region = Some((lo, hi));
    cfg.ghost_margin = GHOST_MARGIN;
    cfg.max_clients = CAPACITY;
    cfg.max_entities = CAPACITY * 4;
    cfg.client_mods = client_mods();
    cfg
}

/// The client modules the package permits (`[package] client_mods`).
#[must_use]
pub fn client_mods() -> Vec<String> {
    parse_package(PACKAGE_TOML)
        .map(|p| p.client_mods)
        .unwrap_or_default()
}

/// Builds one cell (for the zone, or alone to replay its log).
///
/// # Errors
/// [`CellError`].
pub fn cell(
    t: &Tunables,
    index: usize,
    seed: u64,
    adapters: Vec<Arc<dyn WireAdapter>>,
    log: Option<LogWriter<CellLogSchema, BoxedSink>>,
) -> Result<Cell, CellError> {
    let content = gameplay().map_err(|_| CellError::Config("gameplay content"))?;
    let mut cell = Cell::new(
        cell_config(t, index, seed),
        ground(),
        adapters,
        log,
        Arc::clone(&content.catalog),
        content.abilities.clone(),
    )?;
    let set = module_set(&BTreeMap::new()).map_err(|_| CellError::Config("modules cannot start"))?;
    cell.install_modules(&set)
        .map_err(|_| CellError::Config("modules cannot start"))?;
    Ok(cell)
}

/// The adapters in listener order: native first, legacy second.
#[must_use]
pub fn adapters(content: ContentHash) -> Vec<Arc<dyn WireAdapter>> {
    vec![
        Arc::new(toy_adapter_native::adapter()),
        Arc::new(legacy_adapter(content)),
    ]
}

/// Builds the zone; `log(i)` supplies cell `i`'s log writer.
///
/// # Errors
/// [`CellError`].
pub fn zone(
    t: &Tunables,
    seed: u64,
    mut log: impl FnMut(usize) -> Option<LogWriter<CellLogSchema, BoxedSink>>,
) -> Result<Zone, CellError> {
    let cells = (0..regions().len())
        .map(|i| cell(t, i, seed, adapters(t.content), log(i)))
        .collect::<Result<Vec<_>, _>>()?;
    Zone::new(cells, regions())
}

/// The zone plus `instances` instance cells (after the world cells, at
/// [`instance_region`]); `log(i)` supplies cell `i`'s log writer.
///
/// # Errors
/// [`CellError`].
pub fn zone_with_instances(
    t: &Tunables,
    seed: u64,
    instances: usize,
    mut log: impl FnMut(usize) -> Option<LogWriter<CellLogSchema, BoxedSink>>,
) -> Result<Zone, CellError> {
    let all = regions().len() + instances;
    let cells = (0..all)
        .map(|i| cell(t, i, seed, adapters(t.content), log(i)))
        .collect::<Result<Vec<_>, _>>()?;
    let mut zone = Zone::new(cells, (0..all).map(region_of).collect())?;
    let instance_cells: Vec<usize> = (regions().len()..all).collect();
    let grace = u64::from(t.instance_release_grace) * u64::from(t.tick_rate.hz());
    zone.set_instances(&instance_cells, grace);
    Ok(zone)
}

/// Where a session's avatar enters: a grid of 2 m spacing straddling the
/// border, so play crosses it.
#[must_use]
pub fn spawn(s: SessionId) -> Vec3 {
    let col = u16::try_from(s.0 % 20).map_or(0.0, f32::from);
    let row = u16::try_from((s.0 / 20) % 20).map_or(0.0, f32::from);
    Vec3::new(col * 2.0 - 19.0, 0.0, row * 2.0)
}

/// The host, given the native (QUIC) and legacy (TCP) transports.
#[must_use]
pub fn host(t: &Tunables, native: Box<dyn Transport>, legacy: Box<dyn Transport>) -> Host {
    let adapters = adapters(t.content);
    let mut listeners = Vec::with_capacity(2);
    for (adapter, transport) in adapters.into_iter().zip([native, legacy]) {
        listeners.push(Listener { adapter, transport });
    }
    let mut policy = ServerPolicy::new(t.content, 0);
    policy.permitted_modules = client_mods().into_iter().collect();
    let mut host = Host::new(
        HostConfig {
            policy,
            look: CLASS_LOOK,
            tick_rate: u16::try_from(t.tick_rate.hz()).unwrap_or(u16::MAX),
            capacity: CAPACITY,
            token_ok: |token| !token.is_empty(),
            spawn,
        },
        listeners,
    );
    host.set_limits(t.limits);
    host
}

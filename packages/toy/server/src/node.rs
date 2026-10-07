//! The toy's cell host as a deployed node (`toy-server node cell-host
//! --config FILE`), run through `mantis_deploy`'s cell-host role.
//!
//! The node's `[package]` table (read strictly: an unknown key is refused):
//!
//! ```text
//! quic = "ip:port"            # native clients (QUIC)
//! tcp = "ip:port"             # legacy clients (TCP)
//! cert_out = "path"           # the development certificate native clients pin
//! cooked = "path"             # the cooked package
//! key = "path"                # optional: the cook's public key (default: the cooked dev key)
//! seed = 1                    # the zone seed
//! grant_every_ticks = 0       # optional test load: std.containers grants 5 gold to every
//!                             # character in the world every N ticks (0: off)
//! first_cell = 1              # optional: the id of this host's first cell (default 1)
//! world = 0                   # optional: the world this host's cells belong to (default 0)
//! ```
//!
//! Cells are the world regions (ids `first_cell` to `first_cell + n - 1`) and one
//! instance cell (`first_cell + n`), as `toy-server cluster` runs them (from 1).
//! Several hosts serve one cluster with distinct first cells, each a world
//! of its own. In the node's state directory each cell
//! keeps `cell-<id>.snapshot` (written beside, then renamed) and one log
//! per run, `cell-<id>-<start tick>.log`. At start each cell comes back
//! from its snapshot and the log that covers the ticks after it (decision
//! 0007), every outcome of every log is pushed to the writer again (it
//! keeps each batch once), and a snapshot is written at once, so the next
//! run's log always covers what follows the newest snapshot. One line per
//! cell reports it:
//!
//! ```text
//! MANTIS-RECOVERY cell=<id> snapshot_tick=<t> replayed=<n> tick=<t> discarded=<bytes>
//! ```
//!
//! On a drain the zone stops ticking, every cell is snapshotted, and the
//! link is flushed before the node exits.
//!
//! Every session presents an entry token (the realm's, from the login flow),
//! redeemed with the realm exactly once before the session enters: a token
//! presented again is refused `BadToken`, and so is every session while the
//! realm cannot be reached (fail closed).

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use mantis_core::log::{LogHeader, LogWriter, SessionId};
use mantis_core::time::Tick;
use mantis_core::wire::Encoder;
use mantis_deploy::cell::{CellHost, CellNode};
use mantis_net::NetRuntime;
use mantis_net::quic::{QuicServer, ServerCertificate};
use mantis_net::tcp::TcpServer;
use mantis_server::cell::{BoxedSink, Cell};
use mantis_server::jobs::WorkerSet;
use mantis_server::modules::{ExtensionKind, ModuleCommand, Payload, character_of, sessions};
use mantis_server::zone::Zone;

use crate::recovery::{Recovered, logged_outcomes, recover_cell, write_snapshots};
use crate::tunables::Tunables;
use crate::{build_id, cluster, world};

/// std.containers' service-only grant (the test load).
const GRANT: u16 = 1043;
/// Gold per test-load grant.
const GRANT_GOLD: u64 = 5;

/// The toy's cells, hosted by a deployed node.
pub struct ToyCells;

/// The node's `[package]` settings.
struct Settings {
    quic: std::net::SocketAddr,
    tcp: std::net::SocketAddr,
    cert_out: PathBuf,
    cooked: PathBuf,
    key: Option<PathBuf>,
    seed: u64,
    grant_every_ticks: u64,
    first_cell: u64,
    world: u32,
}

impl CellHost for ToyCells {
    fn command(&self) -> &'static str {
        "toy-server node"
    }

    fn run(&self, node: CellNode) -> Result<(), String> {
        let s = node.settings().read(|f, base| {
            Ok(Settings {
                quic: f.addr("quic")?,
                tcp: f.addr("tcp")?,
                cert_out: f.path("cert_out", base)?,
                cooked: f.path("cooked", base)?,
                key: f.opt_path("key", base)?,
                seed: f.uint("seed", 0..=u64::MAX)?,
                grant_every_ticks: u64::try_from(f.int_or("grant_every_ticks", 0..=1_000_000, 0)?)
                    .unwrap_or(0),
                first_cell: u64::try_from(f.int_or("first_cell", 1..=i64::from(u32::MAX), 1)?).unwrap_or(1),
                world: u32::try_from(f.int_or("world", 0..=i64::from(u32::MAX), 0)?).unwrap_or(0),
            })
        })?;
        let t = crate::content::load(Some(&s.cooked), s.key.as_deref())?.tunables;
        // Cells numbered from this host's first: several hosts of the
        // package serve one cluster without colliding.
        world::set_first_cell(s.first_cell);
        let world_cells: Vec<(u64, (f32, f32))> = (s.first_cell..).zip(world::regions()).collect();
        let instance = s.first_cell + world_cells.len() as u64;
        let state = node.state_dir().to_path_buf();
        std::fs::create_dir_all(&state).map_err(|e| format!("{}: {e}", state.display()))?;

        // Every cell back from its snapshot and log, or fresh.
        let all = world_cells.len() + 1;
        let mut cells = Vec::with_capacity(all);
        let mut backlog = Vec::new();
        for index in 0..all {
            let (cell, outcomes, _) = start_cell(&t, index, s.seed, &state)?;
            backlog.push((cell.id().0, outcomes));
            cells.push(cell);
        }
        let mut zone = world::zone_of(&t, cells, 1).map_err(|e| format!("{e:?}"))?;
        cluster::time_cells(&mut zone);
        write_snapshots(&state, &zone, &t)?;

        let runtime = NetRuntime::new(2).map_err(|e| format!("{e:?}"))?;
        // The deployment's chain when its files are named (reloaded on
        // rotation below), else a development certificate.
        let cert = match node.game_tls()? {
            Some(g) => ServerCertificate::from_der(g.chain, g.key).map_err(|e| e.to_string())?,
            None => ServerCertificate::localhost().map_err(|e| format!("{e:?}"))?,
        };
        std::fs::write(&s.cert_out, &cert.cert_der).map_err(|e| format!("{}: {e}", s.cert_out.display()))?;
        let quic = QuicServer::bind(&runtime, s.quic, &cert).map_err(|e| format!("{e:?}"))?;
        let reloader = quic.reloader();
        let tcp = TcpServer::bind(&runtime, s.tcp).map_err(|e| format!("{e:?}"))?;
        let mut host = world::host(&t, Box::new(quic), Box::new(tcp));

        // This host's world: registered with its cells and reported with
        // every placement.
        let mut link_config = node.link_config(&world_cells, &[instance])?;
        link_config.world = s.world;
        redeem_tokens(&mut host, &node, &link_config)?;
        let link = mantis_services::cluster::CellLink::start(&node.handle(), &link_config)?;
        // What the logs hold goes to the writer again: it keeps each batch
        // once, so nothing acknowledged is doubled and nothing is lost.
        for (cell, outcomes) in backlog {
            if !outcomes.is_empty() {
                link.push(cell, outcomes);
            }
        }
        node.ready();

        let threads = std::thread::available_parallelism().map_or(1, |n| n.get().saturating_sub(2).max(1));
        let workers = WorkerSet::new(threads, 256, None);
        let hz = t.tick_rate.hz();
        let period = Duration::from_secs(1) / hz;
        let every = node.snapshot_every_ticks().max(1);
        let mut ops = cluster::OpsApplier::default();
        let mut next = Instant::now();
        let mut ticks = 0u64;
        while node.drain_requested().is_none() {
            host.poll(&mut zone);
            let reports = zone
                .step(&mut host, Some(&workers))
                .map_err(|e| format!("{e:?}"))?;
            cluster::after_tick(&mut zone, &link, &reports);
            ops.apply(&mut host, &mut zone, &link, hz);
            ticks += 1;
            if s.grant_every_ticks > 0 && ticks.is_multiple_of(s.grant_every_ticks) {
                grant_everyone(&mut zone);
            }
            let tick = zone.cells().first().map_or(0, |c| c.tick_now().0);
            node.observe(tick, &link);
            if ticks.is_multiple_of(every) {
                write_snapshots(&state, &zone, &t)?;
            }
            // Overran: re-anchor, never burst (docs/SERVER.md section 2).
            // A rotated chain: open sessions keep theirs, new handshakes
            // get it.
            if let Some(g) = node.game_tls_changed() {
                let rotated = ServerCertificate::from_der(g.chain, g.key).and_then(|c| reloader.reload(&c));
                match rotated {
                    Ok(()) => println!("toy-server node: the game certificate was reloaded"),
                    Err(e) => eprintln!(
                        "toy-server node: the rotated game certificate does not load ({e}); the old chain stays"
                    ),
                }
            }
            cluster::pace(&mut next, period, &mut host, &mut zone);
        }
        // A clean shutdown always ends in a snapshot (decision 0007).
        write_snapshots(&state, &zone, &t)?;
        drop(zone);
        node.finish(link)
    }
}

/// Every entry token is redeemed with the realm exactly once, by this
/// host's cells, through the link's realm endpoint (every instance) and
/// identity (behind the gateway, routing only checks a token). Fail closed:
/// a host that cannot reach the realm admits nobody.
fn redeem_tokens(
    host: &mut mantis_server::host::Host,
    node: &CellNode,
    link: &mantis_services::cluster::CellLinkConfig,
) -> Result<(), String> {
    let cells: Vec<u64> = link
        .cells
        .iter()
        .map(|c| c.0)
        .chain(link.instances.iter().map(|i| i.0))
        .collect();
    let verifier = mantis_services::cluster::TokenVerifier::with_endpoint(
        &node.handle(),
        link.realm.clone(),
        link.key.clone(),
        link.tls.clone(),
        &cells,
    )
    .map_err(|e| format!("the token verifier: {e}"))?;
    host.set_admission(
        Box::new(cluster::RealmAdmission(verifier)),
        mantis_server::host::AdmissionLimits::DEFAULT,
    );
    Ok(())
}

/// Cell `index`: recovered from its snapshot and the log after it, or
/// fresh. Opens this run's log, prints the recovery line, and returns every
/// outcome of every log of the cell (for the writer), and how it came back
/// (`None` when fresh).
///
/// # Errors
/// Why the cell cannot come back.
pub fn start_cell(
    t: &Tunables,
    index: usize,
    seed: u64,
    state: &Path,
) -> Result<
    (
        Cell,
        Vec<mantis_services::cluster::CellOutcome>,
        Option<Recovered>,
    ),
    String,
> {
    let cfg = world::cell_config(t, index, seed);
    let id = cfg.id.0;
    let logs = logs_of(state, id)?;
    let mut outcomes = Vec::new();
    for (_, path) in &logs {
        let bytes = std::fs::read(path).map_err(|e| format!("{}: {e}", path.display()))?;
        // A log of another build or content is refused by its header: its
        // outcomes went out with that run.
        if let Ok(mut o) = logged_outcomes(&bytes, build_id(), t.content) {
            outcomes.append(&mut o);
        }
    }
    let snapshot_path = state.join(format!("cell-{id}.snapshot"));
    let open_log = |start: Tick| {
        let path = state.join(format!("cell-{id}-{}.log", start.0));
        let header = LogHeader {
            build: build_id(),
            content: t.content,
            cell: cfg.id,
            seed: cfg.seed,
            start_tick: start,
        };
        std::fs::File::create(&path).ok().and_then(|f| {
            let sink: BoxedSink = Box::new(f);
            LogWriter::create(sink, &header, 1 << 20).ok()
        })
    };
    let (cell, line, recovered) = match std::fs::read(&snapshot_path) {
        Ok(snapshot) => {
            // The newest log that starts at or before the tick after the
            // snapshot covers what follows it (a run snapshots at start).
            let tick = mantis_server::snapshot::SnapshotHeader::read(&mut mantis_core::wire::Decoder::new(
                &snapshot,
            ))
            .map_err(|e| format!("{}: {e}", snapshot_path.display()))?
            .tick;
            let log = logs
                .iter()
                .rev()
                .find(|(start, _)| *start <= tick.0 + 1)
                .map(|(_, p)| std::fs::read(p).map_err(|e| format!("{}: {e}", p.display())))
                .transpose()?
                .unwrap_or_default();
            let mut opened = None;
            let (cell, r) = recover_cell(t, index, seed, &snapshot, &log, build_id(), |start| {
                let w = open_log(start);
                opened = Some(w.is_some());
                w
            })?;
            if opened == Some(false) {
                return Err(format!("cell {id}: cannot open its log in {}", state.display()));
            }
            let line = format!(
                "MANTIS-RECOVERY cell={id} snapshot_tick={} replayed={} tick={} discarded={}",
                r.snapshot_tick.0, r.replayed, r.tick.0, r.discarded_bytes
            );
            (cell, line, Some(r))
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            let log = open_log(Tick(1)).ok_or_else(|| format!("cell {id}: cannot open its log"))?;
            let cell = world::cell(t, index, seed, world::adapters(t.content), Some(log))
                .map_err(|e| format!("{e:?}"))?;
            (
                cell,
                format!("MANTIS-RECOVERY cell={id} snapshot_tick=0 replayed=0 tick=0 discarded=0"),
                None,
            )
        }
        Err(e) => return Err(format!("{}: {e}", snapshot_path.display())),
    };
    println!("{line}");
    Ok((cell, outcomes, recovered))
}

/// Cell `id`'s logs in `state`, by start tick.
fn logs_of(state: &Path, id: u64) -> Result<Vec<(u64, PathBuf)>, String> {
    let prefix = format!("cell-{id}-");
    let mut out = Vec::new();
    for entry in std::fs::read_dir(state).map_err(|e| format!("{}: {e}", state.display()))? {
        let path = entry.map_err(|e| e.to_string())?.path();
        let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
        if let Some(start) = name
            .strip_prefix(&prefix)
            .and_then(|r| r.strip_suffix(".log"))
            .and_then(|r| r.parse::<u64>().ok())
        {
            out.push((start, path));
        }
    }
    out.sort();
    Ok(out)
}

/// The test load: a grant of [`GRANT_GOLD`] to every character in the
/// world, through the cell hosting it.
fn grant_everyone(zone: &mut Zone) {
    for i in 0..zone.cells().len() {
        let Some(cell) = zone.cell_mut(i) else {
            continue;
        };
        let characters: Vec<u64> = sessions(cell.world())
            .collect::<Vec<SessionId>>()
            .into_iter()
            .filter_map(|s| character_of(cell.world(), s))
            .collect();
        for character in characters {
            let mut bytes = Vec::new();
            let mut e = Encoder::new(&mut bytes);
            e.u64(character);
            e.u32(0);
            e.u32(0);
            e.u64(GRANT_GOLD);
            if let Some(payload) = Payload::from_slice(&bytes) {
                cell.commands().push(ModuleCommand {
                    kind: ExtensionKind(GRANT),
                    session: None,
                    request: 0,
                    payload,
                });
            }
        }
    }
}

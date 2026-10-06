//! The toy server.
//!
//! ```text
//! toy-server serve   [--quic ADDR] [--tcp ADDR] [--cert-out FILE] [--seed N] [--ticks N]
//!                    [--cooked DIR] [--key FILE] [--snapshots DIR]
//! toy-server cluster (serve's options) [--ops ADDR] [--ops-token-file FILE]
//!                    [--ops-cert-out FILE] [--postgres CONN] [--verify-tokens]
//! toy-server soak   --out DIR [--ticks N] [--bots N] [--seed N]
//! toy-server replay LOG...
//! toy-server bots   [--quic ADDR --cert FILE | --tcp ADDR] [--profile P] [--count N]
//!                   [--seconds N] [--seed N] [--cooked DIR] [--key FILE]
//! ```
//!
//! - `serve` listens for native clients over QUIC and legacy clients over
//!   TCP and runs the zone at the package tick rate on the wall clock. The
//!   development certificate native clients pin is written to `--cert-out`.
//!   With `--snapshots DIR`, every cell is snapshotted every 150 ticks and
//!   at a clean shutdown (decision 0007).
//! - `cluster` is `serve` with every service role in the same process
//!   (account, realm, social, matchmaking, the persistence writer, Ops and
//!   its HTTPS dashboard on `--ops`, loopback by default), printing the
//!   resolved service graph at start-up. Cells push their outcomes to the
//!   writer and apply signed live changes from Ops. The store is in memory
//!   unless `--postgres` (or `MANTIS_POSTGRES`) names a database. The
//!   operator token is written to `--ops-token-file`. With
//!   `--verify-tokens`, game handshakes present realm entry tokens, verified
//!   in the background (async admission).
//! - `soak` runs the zone in-process against honest bots on both adapters
//!   over a simulated 100 ms RTT, 2% loss network, writing each cell's log
//!   and a per-tick state-hash trace (`hashes.txt`) to `--out`.
//! - `replay` replays cell logs, verifying every tick's state hash; it is how
//!   logs from one architecture are checked on another.
//! - `bots` connects headless bots to a running `serve` or `cluster`: native
//!   bots over QUIC (pinning the certificate `serve --cert-out` wrote) or
//!   legacy bots over TCP. Profiles: `honest`, `idle`, `speedhack` (1.5x,
//!   legacy only; the server corrects it). Prints what the bots measured
//!   every 10 seconds and at the end.

#![forbid(unsafe_code)]

use std::fmt::Write as _;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::{Duration, Instant};

use mantis_core::content::ContentHash;
use mantis_core::log::{BuildId, LogHeader, LogReader, LogWriter};
use mantis_core::replay::replay;
use mantis_core::time::Tick;
use mantis_net::NetRuntime;
use mantis_net::quic::{DevCertificate, QuicServer};
use mantis_net::tcp::TcpServer;
use mantis_server::bots::Profile;
use mantis_server::cell::BoxedSink;
use mantis_server::host::AdmissionLimits;
use mantis_server::intent::CellLogSchema;
use mantis_server::jobs::WorkerSet;
use mantis_server::simnet::LinkConfig;
use mantis_services::cluster::{
    CellLink, CellLinkConfig, ClusterConfig, LocalCluster, StoreChoice, TokenVerifier, operator_token,
};
use mantis_services::host::Role;
use toy_server::sim::{Side, Sim};
use toy_server::tunables::Tunables;
use toy_server::world;

/// The build identity written into logs: the package and its version, so
/// the same source built for any architecture replays the same logs.
fn build_id() -> BuildId {
    let id = ContentHash::of(concat!("toy-server ", env!("CARGO_PKG_VERSION")).as_bytes());
    BuildId(*id.as_bytes())
}

struct Args(Vec<String>);

impl Args {
    fn value(&self, flag: &str) -> Option<&str> {
        let at = self.0.iter().position(|a| a == flag)?;
        self.0.get(at + 1).map(String::as_str)
    }

    fn number(&self, flag: &str, default: u64) -> Result<u64, String> {
        self.value(flag).map_or(Ok(default), |v| {
            v.parse().map_err(|_| format!("{flag}: not a number: {v}"))
        })
    }

    fn has_flag(&self, flag: &str) -> bool {
        self.0.iter().any(|a| a == flag)
    }

    fn positional(&self) -> impl Iterator<Item = &str> {
        let mut skip = false;
        self.0.iter().filter_map(move |a| {
            if skip {
                skip = false;
                return None;
            }
            if a.starts_with("--") {
                skip = true;
                return None;
            }
            Some(a.as_str())
        })
    }
}

fn main() -> ExitCode {
    let all: Vec<String> = std::env::args().skip(1).collect();
    let (command, rest) = all.split_first().map_or(("", &[][..]), |(c, r)| (c.as_str(), r));
    let args = Args(rest.to_vec());
    let result = match command {
        "serve" => serve(&args, false),
        "cluster" => serve(&args, true),
        "soak" => soak(&args),
        "replay" => replay_logs(&args),
        "bots" => bots(&args),
        _ => Err("usage: toy-server serve|cluster|soak|replay|bots (see the crate docs)".to_owned()),
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("toy-server: {e}");
            ExitCode::FAILURE
        }
    }
}

/// The tunables, with the content hash of the cooked package: `--cooked
/// DIR` (default [`world::COOKED_DIR`]) and `--key FILE` (default the
/// development key that cook wrote). The server refuses to start without a
/// trustworthy cook.
fn cooked_tunables(args: &Args) -> Result<Tunables, String> {
    let mut t = Tunables::defaults().map_err(|e| e.to_string())?;
    let dir = PathBuf::from(args.value("--cooked").unwrap_or(world::COOKED_DIR));
    let key = args.value("--key").map(PathBuf::from);
    t.content = world::cooked_content(&dir, key.as_deref())
        .map_err(|e| format!("{e} (cook the package first: cargo run -p mantis-cook -- packages/toy)"))?;
    // Cells run the graphs of the same verified bundle.
    world::use_gameplay(&dir, key.as_deref())?;
    Ok(t)
}

/// Starts every service role in this process (`cluster`).
fn start_cluster(args: &Args, game_ports: Vec<u16>) -> Result<LocalCluster, String> {
    let mut config = ClusterConfig::local();
    config.dashboard.listen = args
        .value("--ops")
        .unwrap_or("127.0.0.1:7480")
        .parse()
        .map_err(|_| "--ops: not an address")?;
    config.dashboard.game_ports = game_ports;
    let token = operator_token()?;
    let token_file = PathBuf::from(args.value("--ops-token-file").unwrap_or("ops-token.txt"));
    std::fs::write(&token_file, &token).map_err(|e| format!("{}: {e}", token_file.display()))?;
    config.dashboard.operators.insert(token, "operator".to_owned());
    if let Some(conn) = args
        .value("--postgres")
        .map(str::to_owned)
        .or_else(|| std::env::var("MANTIS_POSTGRES").ok())
    {
        config.store = StoreChoice::Postgres(conn);
    }
    let cluster = LocalCluster::start(&config)?;
    if let Some(path) = args.value("--ops-cert-out") {
        std::fs::write(path, &cluster.dashboard_cert).map_err(|e| format!("{path}: {e}"))?;
    }
    print!("{}", cluster.graph());
    println!(
        "ops: dashboard operator token written to {}",
        token_file.display()
    );
    Ok(cluster)
}

fn link_cells(cluster: &LocalCluster, game: SocketAddr) -> Result<CellLink, String> {
    let need = |r: Role| cluster.addr(r).ok_or(format!("no {} role", r.name()));
    let cells = world::regions()
        .into_iter()
        .enumerate()
        .map(|(i, range)| (i as u64 + 1, game.to_string(), range))
        .collect();
    let link = CellLink::start(
        &cluster.handle(),
        &CellLinkConfig {
            key: cluster.key.clone(),
            persist: need(Role::Persist)?,
            ops: need(Role::Ops)?,
            social: need(Role::Social)?,
            matchmaking: need(Role::Matchmaking)?,
            realm: need(Role::Realm)?,
            live_key: cluster.ops.public_key(),
            cells,
            poll: Duration::from_millis(500),
            // One instance cell, after the world cells.
            instances: vec![(world::regions().len() as u64 + 1, game.to_string())],
            inspector: "127.0.0.1:0".parse().map_err(|_| "inspector address")?,
        },
    )?;
    if let Some(inspector) = link.inspector() {
        for i in 0..world::regions().len() {
            cluster.add_cell(i as u64 + 1, inspector);
        }
    }
    Ok(link)
}

fn serve(args: &Args, with_cluster: bool) -> Result<(), String> {
    let t = cooked_tunables(args)?;
    let quic_addr: SocketAddr = args
        .value("--quic")
        .unwrap_or("127.0.0.1:7400")
        .parse()
        .map_err(|_| "--quic: not an address")?;
    let tcp_addr: SocketAddr = args
        .value("--tcp")
        .unwrap_or("127.0.0.1:7401")
        .parse()
        .map_err(|_| "--tcp: not an address")?;
    let seed = args.number("--seed", 1)?;
    let max_ticks = args.number("--ticks", u64::MAX)?;
    let runtime = NetRuntime::new(2).map_err(|e| format!("{e:?}"))?;
    let cert = DevCertificate::localhost().map_err(|e| format!("{e:?}"))?;
    if let Some(path) = args.value("--cert-out") {
        std::fs::write(path, &cert.cert_der).map_err(|e| format!("{path}: {e}"))?;
    }
    let quic = QuicServer::bind(&runtime, quic_addr, &cert).map_err(|e| format!("{e:?}"))?;
    let tcp = TcpServer::bind(&runtime, tcp_addr).map_err(|e| format!("{e:?}"))?;
    println!(
        "toy-server: native (QUIC) on {}, legacy (TCP) on {}",
        quic.local_addr(),
        tcp.local_addr()
    );
    // The module graph, as the cells will install it (refuses to start on a
    // missing or disabled dependency).
    let set = world::module_set(&std::collections::BTreeMap::new())?;
    print!("{}", set.graph().render());
    let (game, legacy) = (quic.local_addr(), tcp.local_addr());
    let cluster = if with_cluster {
        Some(start_cluster(args, vec![game.port(), legacy.port()])?)
    } else {
        None
    };
    let link = cluster.as_ref().map(|c| link_cells(c, game)).transpose()?;
    let mut host = world::host(&t, Box::new(quic), Box::new(tcp));
    if let (Some(cluster), true) = (cluster.as_ref(), args.has_flag("--verify-tokens")) {
        let realm = cluster.addr(Role::Realm).ok_or("no realm role")?;
        let cells: Vec<u64> = (1..=world::regions().len() as u64).collect();
        let verifier = TokenVerifier::new(&cluster.handle(), realm, cluster.key.clone(), &cells);
        host.set_admission(
            Box::new(toy_server::cluster::RealmAdmission(verifier)),
            AdmissionLimits::DEFAULT,
        );
        println!("toy-server: game tokens are verified with the realm");
    }
    // The cluster runs one instance cell for matchmaking.
    let instances = usize::from(with_cluster);
    let mut zone = world::zone_with_instances(&t, seed, instances, |_| None).map_err(|e| format!("{e:?}"))?;
    if link.is_some() {
        toy_server::cluster::time_cells(&mut zone);
    }
    let threads = std::thread::available_parallelism().map_or(1, |n| n.get().saturating_sub(2).max(1));
    let workers = WorkerSet::new(threads, 256, None);
    let period = Duration::from_secs(1) / t.tick_rate.hz();
    let mut next = Instant::now();
    let mut ticks = 0u64;
    let mut ops = toy_server::cluster::OpsApplier::default();
    while ticks < max_ticks {
        host.poll(&mut zone);
        let reports = zone
            .step(&mut host, Some(&workers))
            .map_err(|e| format!("{e:?}"))?;
        match &link {
            Some(link) => {
                toy_server::cluster::after_tick(&mut zone, link, &reports);
                ops.apply(&mut host, &mut zone, link, t.tick_rate.hz());
            }
            None => {
                for i in 0..zone.cells().len() {
                    if let Some(cell) = zone.cell_mut(i) {
                        cell.drain_outcomes(|_| {});
                    }
                }
            }
        }
        ticks += 1;
        if ticks.is_multiple_of(u64::from(t.tick_rate.hz()) * 10) {
            let refused: u64 = zone
                .cells()
                .iter()
                .map(mantis_server::cell::Cell::encode_refusals)
                .sum();
            println!(
                "tick {ticks}: {} in world, {:?}, cell encode refusals {refused}",
                host.sessions_in_world(),
                host.stats
            );
        }
        next += period;
        let now = Instant::now();
        if next > now {
            std::thread::sleep(next - now);
        } else {
            // Overran: do not try to catch up in a burst.
            next = now;
        }
        if ticks.is_multiple_of(SNAPSHOT_EVERY) {
            write_snapshots(args, &zone, &t)?;
        }
    }
    // A clean shutdown always ends in a snapshot (decision 0007).
    write_snapshots(args, &zone, &t)
}

/// Ticks between snapshots of a serving zone.
const SNAPSHOT_EVERY: u64 = 150;

/// Writes `cell-<id>.snapshot` for every cell into `--snapshots DIR`.
fn write_snapshots(args: &Args, zone: &mantis_server::zone::Zone, t: &Tunables) -> Result<(), String> {
    let Some(dir) = args.value("--snapshots") else {
        return Ok(());
    };
    std::fs::create_dir_all(dir).map_err(|e| format!("{dir}: {e}"))?;
    for cell in zone.cells() {
        let bytes = cell.snapshot(build_id(), t.content).map_err(|e| e.to_string())?;
        let path = Path::new(dir).join(format!("cell-{}.snapshot", cell.id().0));
        // Written beside, then renamed: a crash never leaves half a snapshot.
        let partial = path.with_extension("snapshot.partial");
        std::fs::write(&partial, &bytes).map_err(|e| format!("{}: {e}", partial.display()))?;
        std::fs::rename(&partial, &path).map_err(|e| format!("{}: {e}", path.display()))?;
    }
    Ok(())
}

fn soak(args: &Args) -> Result<(), String> {
    let t = Tunables::defaults().map_err(|e| e.to_string())?;
    let out = PathBuf::from(args.value("--out").ok_or("--out DIR is required")?);
    let ticks = args.number("--ticks", 18_000)?;
    let bots = args.number("--bots", 64)?;
    let seed = args.number("--seed", 1)?;
    std::fs::create_dir_all(&out).map_err(|e| format!("{}: {e}", out.display()))?;
    let mut open_error = None;
    let mut sim = Sim::new(t, seed, |i| {
        let cfg = world::cell_config(&t, i, seed);
        let path = out.join(format!("cell-{}.log", cfg.id.0));
        let header = LogHeader {
            build: build_id(),
            content: t.content,
            cell: cfg.id,
            seed: cfg.seed,
            start_tick: Tick(1),
        };
        let opened = std::fs::File::create(&path)
            .map_err(|e| format!("{}: {e}", path.display()))
            .and_then(|f| {
                let sink: BoxedSink = Box::new(f);
                LogWriter::create(sink, &header, 1 << 20).map_err(|e| format!("{e:?}"))
            });
        match opened {
            Ok(w) => Some(w),
            Err(e) => {
                open_error = Some(e);
                None
            }
        }
    })
    .map_err(|e| format!("{e:?}"))?;
    if let Some(e) = open_error {
        return Err(e);
    }
    for k in 0..bots {
        let side = if k % 2 == 0 { Side::Native } else { Side::Legacy };
        sim.add_bot(side, Profile::Honest, LinkConfig::RTT100_LOSS2)?;
    }
    let mut trace = String::new();
    for _ in 0..ticks {
        let reports = sim.step().map_err(|e| format!("{e:?}"))?;
        for (i, r) in reports.iter().enumerate() {
            let _ = writeln!(trace, "{} {} {:016x}", r.tick.0, i + 1, r.state_hash);
        }
    }
    let path = out.join("hashes.txt");
    std::fs::write(&path, trace).map_err(|e| format!("{}: {e}", path.display()))?;
    println!(
        "soak: {ticks} ticks, {bots} bots, {} joined; logs and hashes.txt in {}",
        sim.host.stats.joined,
        out.display()
    );
    Ok(())
}

fn replay_logs(args: &Args) -> Result<(), String> {
    // A log records the content hash it ran with; replay checks it, so a
    // log made by `serve` replays against the same cook (`--uncooked` for
    // logs from `soak`, which does not cook: `--content manifest`).
    let t = if args.value("--content") == Some("manifest") {
        Tunables::defaults().map_err(|e| e.to_string())?
    } else {
        cooked_tunables(args)?
    };
    let mut any = false;
    for path in args.positional() {
        any = true;
        replay_one(&t, Path::new(path))?;
    }
    if any {
        Ok(())
    } else {
        Err("replay: no log given".to_owned())
    }
}

fn replay_one(t: &Tunables, path: &Path) -> Result<(), String> {
    let bytes = std::fs::read(path).map_err(|e| format!("{}: {e}", path.display()))?;
    let mut reader = LogReader::<CellLogSchema>::open(&bytes, build_id(), t.content)
        .map_err(|e| format!("{}: {e:?}", path.display()))?;
    let header = *reader.header();
    let index = usize::try_from(header.cell.0.saturating_sub(1)).map_err(|_| "cell id")?;
    // Cell seeds are the zone seed mixed with the cell id (world::cell_config).
    let zone_seed = header.seed.0 ^ header.cell.0;
    let mut cell =
        world::cell(t, index, zone_seed, world::adapters(t.content), None).map_err(|e| format!("{e:?}"))?;
    let report =
        replay(&mut cell, &mut reader).map_err(|e| format!("{}: diverged: {e:?}", path.display()))?;
    println!(
        "{}: cell {} replayed {} ticks with no divergence (last tick {:?}, final state {:016x}{})",
        path.display(),
        header.cell.0,
        report.ticks,
        report.last_tick.map(|t| t.0),
        cell.world().state_hash(),
        if report.truncated_tail {
            ", truncated tail ignored"
        } else {
            ""
        }
    );
    Ok(())
}

/// Headless bots against a running server (`bots`).
fn bots(args: &Args) -> Result<(), String> {
    let t = cooked_tunables(args)?;
    let profile = match args.value("--profile").unwrap_or("honest") {
        "honest" => Profile::Honest,
        "idle" => Profile::Idle,
        "speedhack" => Profile::SpeedHack(1.5),
        other => return Err(format!("--profile: honest, idle, or speedhack, not {other}")),
    };
    let count = args.number("--count", 8)?;
    let seconds = args.number("--seconds", 60)?;
    let seed = args.number("--seed", 1)?;
    let runtime = NetRuntime::new(2).map_err(|e| format!("{e:?}"))?;
    let legacy = args
        .value("--tcp")
        .map(str::parse::<SocketAddr>)
        .transpose()
        .map_err(|_| "--tcp: not an address")?;
    let cert = if legacy.is_none() {
        let path = args
            .value("--cert")
            .ok_or("--cert FILE (written by serve --cert-out) is required for QUIC")?;
        std::fs::read(path).map_err(|e| format!("{path}: {e}"))?
    } else {
        Vec::new()
    };
    let quic: SocketAddr = args
        .value("--quic")
        .unwrap_or("127.0.0.1:7400")
        .parse()
        .map_err(|_| "--quic: not an address")?;
    let mut bots = Vec::new();
    for n in 0..count {
        let (wire, transport): (
            Box<dyn mantis_server::bots::BotWire>,
            Box<dyn mantis_adapter_contract::Transport>,
        ) = match legacy {
            Some(addr) => (
                Box::new(toy_server::wire::LegacyWire::new(world::LEGACY_BUILD)),
                Box::new(mantis_net::tcp::TcpClient::connect(&runtime, addr).map_err(|e| format!("{e:?}"))?),
            ),
            None => (
                Box::new(mantis_server::bots::NativeWire::default()),
                Box::new(
                    mantis_net::quic::QuicClient::connect(&runtime, quic, &cert)
                        .map_err(|e| format!("{e:?}"))?,
                ),
            ),
        };
        let mut bot = mantis_server::bots::Bot::new(
            wire,
            transport,
            world::ground(),
            toy_server::sim::bot_config(&t, seed, n, profile),
            mantis_core::math::Vec3::ZERO,
        )
        .map_err(str::to_owned)?;
        bot.start();
        bots.push(bot);
    }
    println!(
        "toy-server bots: {count} {profile:?} bot(s) to {} for {seconds} s",
        legacy.map_or_else(|| format!("{quic} (QUIC)"), |a| format!("{a} (TCP)"))
    );
    let hz = u64::from(t.tick_rate.hz());
    let period = Duration::from_secs(1) / t.tick_rate.hz();
    let mut next = Instant::now();
    for tick in 1..=seconds.saturating_mul(hz) {
        for b in &mut bots {
            b.step();
        }
        if tick.is_multiple_of(hz * 10) || tick == seconds.saturating_mul(hz) {
            let welcomed = bots.iter().filter(|b| b.welcomed()).count();
            let refused = bots.iter().filter(|b| b.stats.refused.is_some()).count();
            let snapshots: u64 = bots.iter().map(|b| b.stats.snapshots).sum();
            let corrections: u64 = bots.iter().map(|b| b.stats.corrections).sum();
            println!(
                "after {} s: {welcomed} in world, {refused} refused, {snapshots} snapshots, {corrections} corrections",
                tick / hz
            );
        }
        next += period;
        let now = Instant::now();
        if next > now {
            std::thread::sleep(next - now);
        } else {
            next = now;
        }
    }
    Ok(())
}

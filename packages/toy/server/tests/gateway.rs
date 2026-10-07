//! The gateway in front of the toy server, over the simulated network
//! (deterministic: the same seed counts the same ticks).
//!
//! - Bots reach the host only through the gateway, and play.
//! - A cell's sessions move to another host whose ticks start over
//!   (`Sim::swap_cell_host`): every bot gets `Transferred` before any frame
//!   of the new host, drops the old host's late snapshots by their epoch
//!   stamp, and plays on with the same connection (a `budget:` line in
//!   ticks).
//! - A bot whose connection drops resumes with its ticket and plays on; the
//!   ticket it replaced is refused as stale; a session whose client never
//!   comes back ends when the ticket expires.

#![expect(clippy::unwrap_used, clippy::indexing_slicing, clippy::too_many_lines)]

use mantis_adapter_contract::RefuseReason;
use mantis_core::log::SessionId;
use mantis_server::bots::{Bot, NativeWire, Profile};
use mantis_server::simnet::LinkConfig;
use toy_server::sim::{Sim, bot_config};
use toy_server::tunables::Tunables;
use toy_server::world;

const SEED: u64 = 4242;
/// Ticks from a cell's swap until every bot plays on the new host: the
/// budget.
const HAND_OFF_LIMIT: u64 = 8;

fn sim_with_gateway(bots: usize) -> Sim {
    let mut sim = Sim::new(Tunables::defaults().unwrap(), SEED, |_| None).unwrap();
    sim.enable_gateway();
    for _ in 0..bots {
        sim.add_bot_via_gateway(Profile::Honest, LinkConfig::RTT100_LOSS2)
            .unwrap();
    }
    sim
}

fn until(sim: &mut Sim, what: &str, limit: u64, done: &dyn Fn(&Sim) -> bool) -> u64 {
    let mut ticks = 0;
    while !done(sim) {
        assert!(ticks < limit, "{what}: not within {limit} ticks");
        sim.step().unwrap();
        ticks += 1;
    }
    ticks
}

fn playing(sim: &Sim) -> bool {
    sim.bots
        .iter()
        .all(|b| b.bot.welcomed() && b.bot.synced() && b.bot.stats.snapshots > 30)
}

/// The cell (index) serving the most sessions of the first host.
fn busiest_cell(sim: &Sim) -> usize {
    let mut counts = vec![0usize; sim.zone.cells().len()];
    for s in sim.host.sessions() {
        if let Some(r) = sim.zone.route(s) {
            counts[r] += 1;
        }
    }
    (0..counts.len()).max_by_key(|i| counts[*i]).unwrap()
}

/// Runs the swap; returns what a second run must reproduce.
fn swap_run() -> (u64, Vec<(u64, u32, u64)>, u64) {
    let mut sim = sim_with_gateway(3);
    until(&mut sim, "everyone playing through the gateway", 300, &playing);
    assert_eq!(sim.host.sessions_in_world(), 3, "every bot is on the host");
    for b in &sim.bots {
        assert!(b.bot.stats.ticket.is_some(), "a resume ticket after Welcome");
        assert!(b.bot.stats.refused.is_none());
    }
    let cell = busiest_cell(&sim);
    let on_cell = sim
        .host
        .sessions()
        .into_iter()
        .filter(|s| sim.zone.route(*s) == Some(cell))
        .count();
    let before: Vec<u64> = sim.bots.iter().map(|b| b.bot.stats.snapshots).collect();
    let tick_before = sim.zone.cells()[cell].tick_now().0;
    assert_eq!(sim.swap_cell_host(cell).unwrap(), on_cell);

    // The budget: until every moved bot is placed by the new host.
    let ticks = until(&mut sim, "every moved bot placed on the new host", 120, &|s| {
        let moved = s
            .bots
            .iter()
            .filter(|b| !b.bot.stats.transfers.is_empty())
            .count();
        moved == on_cell
            && s.bots
                .iter()
                .all(|b| b.bot.stats.transfers.is_empty() || b.bot.synced())
    });
    until(&mut sim, "every moved bot playing on the new host", 120, &|s| {
        s.bots
            .iter()
            .zip(&before)
            .all(|(b, n)| b.bot.stats.transfers.is_empty() || b.bot.stats.snapshots > n + 10)
    });
    let second = sim.second.as_ref().unwrap();
    assert_eq!(second.host.sessions_in_world(), on_cell, "the sessions moved");
    assert_eq!(
        sim.host.sessions_in_world(),
        3 - on_cell,
        "and ended on the first host"
    );
    let mut transfers = Vec::new();
    for b in sim.bots.iter().filter(|b| !b.bot.stats.transfers.is_empty()) {
        let (to, epoch, tick) = b.bot.stats.transfers[0];
        assert_eq!(epoch, 1);
        assert!(
            tick.0 < tick_before,
            "the new host counts its own ticks: {} < {tick_before}",
            tick.0
        );
        assert!(b.bot.stats.refused.is_none());
        assert!(b.bot.stats.ticket.is_some(), "a new ticket after Transferred");
        transfers.push((to, epoch, tick.0));
    }
    let stats = sim.gateway.as_ref().unwrap().gateway.stats;
    assert_eq!(usize::try_from(stats.handed_off).unwrap(), on_cell);
    assert_eq!(stats.hand_offs_failed, 0);
    assert!(stats.stamped > 0, "the new host's snapshots carry the epoch");
    // Play goes on: the moved avatars answer inputs on the new host.
    let snapshots: Vec<u64> = sim.bots.iter().map(|b| b.bot.stats.snapshots).collect();
    for _ in 0..60 {
        sim.step().unwrap();
    }
    for (b, n) in sim.bots.iter().zip(snapshots) {
        assert!(b.bot.stats.snapshots >= n + 50, "snapshots keep coming");
        assert_eq!(
            b.bot.stats.undecodable, 0,
            "every frame decodes across the hand-off"
        );
    }
    (ticks, transfers, stats.stamped)
}

#[test]
fn a_cell_moves_to_another_host_and_its_bots_play_on_through_the_gateway() {
    let first = swap_run();
    assert_eq!(swap_run(), first, "a second run counts the same ticks");
    let ticks = first.0;
    println!(
        "budget: hand-off through the gateway: every bot of the swapped cell was placed by the new host {ticks} ticks after the swap (limit {HAND_OFF_LIMIT}: the hand-off message, the new host's Welcome, at 100 ms RTT and 2% loss to the clients)"
    );
    assert!(ticks <= HAND_OFF_LIMIT);
}

#[test]
fn a_dropped_client_resumes_with_its_ticket_and_a_stale_ticket_is_refused() {
    let mut sim = sim_with_gateway(2);
    until(&mut sim, "playing", 300, &playing);
    let old_ticket = sim.bots[0].bot.stats.ticket.unwrap();
    let sessions = sim.host.sessions_in_world();

    // The connection drops; the session waits on the host.
    sim.bots[0].bot.disconnect();
    for _ in 0..30 {
        sim.step().unwrap();
    }
    assert_eq!(sim.host.sessions_in_world(), sessions, "the session waits");
    assert_eq!(sim.gateway.as_ref().unwrap().gateway.sessions_away(), 1);

    // It comes back with its ticket over a new connection and plays on.
    let net = sim.gateway_net().unwrap().clone();
    sim.bots[0].bot.stats.ticket = Some(old_ticket);
    let client = net.connect(LinkConfig::RTT100_LOSS2);
    assert!(sim.bots[0].bot.resume(Box::new(client)));
    let before = sim.bots[0].bot.stats.snapshots;
    let ticks = until(&mut sim, "resumed and playing", 60, &|s| {
        let b = &s.bots[0].bot;
        b.welcomed() && b.synced() && b.stats.snapshots > before + 10
    });
    println!("resumed: playing {ticks} ticks after reconnecting");
    assert_eq!(
        sim.host.sessions_in_world(),
        sessions,
        "the same session, not a new one"
    );
    let gw = &sim.gateway.as_ref().unwrap().gateway;
    assert_eq!((gw.stats.resumed, gw.sessions_away()), (1, 0));
    assert!(sim.bots[0].bot.stats.refused.is_none());
    // No snapshot built for the old connection reaches the new one.
    assert_eq!(sim.bots[0].bot.stats.undecodable, 0);
    let new_ticket = sim.bots[0].bot.stats.ticket.unwrap();
    assert_ne!(new_ticket, old_ticket, "a new ticket after the resume");

    // The used ticket is stale now.
    let t = Tunables::defaults().unwrap();
    let mut stale = Bot::new(
        Box::new(NativeWire::default()),
        Box::new(net.connect(LinkConfig::PERFECT)),
        world::ground(),
        bot_config(&t, SEED, 99, Profile::Idle),
        mantis_core::math::Vec3::ZERO,
    )
    .unwrap()
    .with_token(&old_ticket);
    stale.start();
    for _ in 0..10 {
        sim.step().unwrap();
        stale.step();
    }
    assert_eq!(stale.stats.refused, Some(RefuseReason::StaleEpoch));

    // A client that never comes back: its session ends when the ticket
    // expires (a minute), and its character leaves the host.
    sim.bots[1].bot.disconnect();
    let rate = u64::from(t.tick_rate.hz());
    for _ in 0..(rate * 61) {
        sim.step().unwrap();
    }
    let gw = &sim.gateway.as_ref().unwrap().gateway;
    assert_eq!((gw.stats.expired, gw.sessions()), (1, 1));
    for _ in 0..5 {
        sim.step().unwrap();
    }
    assert_eq!(sim.host.sessions_in_world(), sessions - 1);
    let _ = SessionId(0);
}

// ---- the real binaries ---------------------------------------------------------

/// A child this test spawned, killed when dropped.
struct Running(std::process::Child);

impl Drop for Running {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn package(path: &str) -> std::path::PathBuf {
    std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join(path)
}

fn toy() -> std::process::Command {
    std::process::Command::new(env!("CARGO_BIN_EXE_toy-server"))
}

/// `toy-server bots` dialling only the gateway; with `login`, each logs in
/// through the account and realm roles first.
fn bots_via_gateway(dir: &std::path::Path, gateway: &str, login: bool) -> String {
    let mut cmd = toy();
    cmd.args([
        "bots",
        "--profile",
        "honest",
        "--count",
        "3",
        "--seconds",
        "4",
        "--seed",
        "9",
    ])
    .args(["--gateway", gateway])
    .arg("--cert")
    .arg(dir.join("gateway.der"))
    .arg("--cooked")
    .arg(package("cooked"))
    .arg("--key")
    .arg(package("cooked/keys/dev.pub"));
    if login {
        cmd.arg("--login").arg(dir.join("login.txt"));
    }
    let out = cmd.output().unwrap();
    let text = String::from_utf8_lossy(&out.stdout).into_owned();
    assert!(
        out.status.success(),
        "bots failed: {text}{}",
        String::from_utf8_lossy(&out.stderr)
    );
    text
}

#[test]
fn the_toy_cluster_runs_behind_its_gateway_and_bots_reach_it_only_there() {
    let dir = std::env::temp_dir().join(format!("mantis-gateway-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let out = std::fs::File::create(dir.join("server.out")).unwrap();
    let _server = Running(
        toy()
            .args([
                "cluster",
                "--quic",
                "127.0.0.1:0",
                "--tcp",
                "127.0.0.1:0",
                "--ops",
                "127.0.0.1:0",
            ])
            .args(["--gateway", "127.0.0.1:0"])
            .arg("--gateway-cert-out")
            .arg(dir.join("gateway.der"))
            .arg("--cert-out")
            .arg(dir.join("cert.der"))
            .arg("--ops-token-file")
            .arg(dir.join("ops-token.txt"))
            .arg("--ops-cert-out")
            .arg(dir.join("ops-cert.der"))
            .arg("--login-out")
            .arg(dir.join("login.txt"))
            .arg("--cooked")
            .arg(package("cooked"))
            .arg("--key")
            .arg(package("cooked/keys/dev.pub"))
            .args(["--verify-tokens", "--ticks", "3000"])
            .stdout(std::process::Stdio::from(out))
            .stderr(std::process::Stdio::null())
            .spawn()
            .unwrap(),
    );
    let start = std::time::Instant::now();
    let gateway = loop {
        let text = std::fs::read_to_string(dir.join("server.out")).unwrap_or_default();
        let addr = text
            .lines()
            .find_map(|l| l.strip_prefix("toy-server: gateway on "))
            .map(str::to_owned);
        if let Some(addr) = addr.filter(|_| dir.join("login.txt").exists()) {
            break addr;
        }
        assert!(
            start.elapsed() < std::time::Duration::from_secs(60),
            "the cluster never started its gateway: {text}"
        );
        std::thread::sleep(std::time::Duration::from_millis(50));
    };
    std::thread::sleep(std::time::Duration::from_millis(200));

    // Logged in, every bot is routed by its entry token to the host, which
    // redeems it: all three play, through the one gateway address.
    let logged_in = bots_via_gateway(&dir, &gateway, true);
    assert!(
        logged_in.contains("after 4 s: 3 in world, 0 refused"),
        "{logged_in}"
    );
    // Without an entry token the gateway refuses them itself.
    let anonymous = bots_via_gateway(&dir, &gateway, false);
    assert!(
        anonymous.contains("after 4 s: 0 in world, 3 refused"),
        "{anonymous}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

// ---- a hand-off over real sockets ------------------------------------------------

/// Hand-off tokens the second host admits: (character, where it stands).
#[derive(Clone, Default)]
struct Tokens(std::sync::Arc<std::sync::Mutex<TokenDesk>>);

#[derive(Default)]
struct TokenDesk {
    grants: std::collections::BTreeMap<[u8; 32], (u64, mantis_core::math::Vec3)>,
    ready: Vec<(u64, mantis_server::host::Verdict)>,
}

impl mantis_server::host::Admission for Tokens {
    fn begin(&mut self, ticket: u64, token: &[u8]) {
        let mut d = self.0.lock().unwrap();
        let grant = <[u8; 32]>::try_from(token).ok().and_then(|t| d.grants.remove(&t));
        let verdict = grant.map_or(mantis_server::host::Verdict::Refuse, |(character, at)| {
            mantis_server::host::Verdict::Admit {
                character,
                spawn: Some(at),
            }
        });
        d.ready.push((ticket, verdict));
    }

    fn ready(&mut self, out: &mut Vec<(u64, mantis_server::host::Verdict)>) {
        out.append(&mut self.0.lock().unwrap().ready);
    }
}

/// Every token routes to host A.
struct ToA(String, Vec<(u64, mantis_net::gateway::Route)>);

impl mantis_net::gateway::Routes for ToA {
    fn begin(&mut self, ticket: u64, _token: &[u8]) {
        self.1.push((
            ticket,
            mantis_net::gateway::Route::Host {
                cell: 1,
                address: self.0.clone(),
            },
        ));
    }

    fn ready(&mut self, out: &mut Vec<(u64, mantis_net::gateway::Route)>) {
        out.append(&mut self.1);
    }
}

#[test]
fn a_hand_off_over_quic_keeps_the_client_connection_and_every_frame_decodes() {
    use mantis_net::NetRuntime;
    use mantis_net::gateway::{Gateway, GatewayConfig};
    use mantis_net::quic::{DevCa, QuicClient, QuicDialer, QuicServer, ServerTrust};
    use mantis_net::tcp::TcpServer;

    let tun = Tunables::defaults().unwrap();
    let runtime = NetRuntime::new(2).unwrap();
    let ca = DevCa::new().unwrap();
    let host_cert = ca.issue(&["cell-host", "127.0.0.1"]).unwrap();
    let local = "127.0.0.1:0".parse().unwrap();
    // Two cell hosts serving the same world, the second built afresh.
    let quic_a = QuicServer::bind(&runtime, local, &host_cert).unwrap();
    let quic_b = QuicServer::bind(&runtime, local, &host_cert).unwrap();
    let (addr_a, addr_b) = (quic_a.local_addr(), quic_b.local_addr());
    let host_a = world::host(
        &tun,
        Box::new(quic_a),
        Box::new(TcpServer::bind(&runtime, local).unwrap()),
    );
    let mut host_b = world::host(
        &tun,
        Box::new(quic_b),
        Box::new(TcpServer::bind(&runtime, local).unwrap()),
    );
    let tokens = Tokens::default();
    host_b.set_admission(
        Box::new(tokens.clone()),
        mantis_server::host::AdmissionLimits::DEFAULT,
    );
    let zone_a = world::zone_with_instances(&tun, 31, 0, |_| None).unwrap();
    let zone_b = world::zone_with_instances(&tun, 32, 0, |_| None).unwrap();

    // The gateway: its own certificate for clients; the cluster CA for hosts.
    let front_cert = mantis_net::quic::ServerCertificate::localhost().unwrap();
    let clients = QuicServer::bind(&runtime, local, &front_cert).unwrap();
    let front = clients.local_addr();
    let hosts = QuicDialer::new(
        &runtime,
        &ServerTrust::Roots {
            roots: vec![ca.cert_der().to_vec()],
            server_name: "cell-host".to_owned(),
        },
    )
    .unwrap();
    let mut count = 0u8;
    let gateway = Gateway::new(
        Box::new(clients),
        Box::new(hosts),
        Box::new(ToA(addr_a.to_string(), Vec::new())),
        Box::new(move |ticket: &mut [u8; 32]| {
            count = count.wrapping_add(1);
            *ticket = [count; 32];
        }),
        GatewayConfig::DEFAULT,
    );
    let client = QuicClient::connect(&runtime, front, &front_cert.cert_der).unwrap();
    let mut bot = Bot::new(
        Box::new(NativeWire::default()),
        Box::new(client),
        world::ground(),
        bot_config(&tun, 7, 0, Profile::Honest),
        mantis_core::math::Vec3::ZERO,
    )
    .unwrap();
    bot.start();

    let mut w = TwoHosts {
        gateway,
        host_a,
        zone_a,
        host_b,
        zone_b,
        bot,
        period: std::time::Duration::from_secs(1) / tun.tick_rate.hz(),
        started: std::time::Instant::now(),
    };
    w.wait("playing on host A through the gateway", &|w| {
        w.bot.synced() && w.bot.stats.snapshots > 30
    });
    let before = w.bot.stats.snapshots;

    // Host A hands the session to host B: a token for where it stands.
    let session = w.host_a.sessions()[0];
    let cell = w.zone_a.route(session).unwrap();
    let (to, grant) = {
        let c = &w.zone_a.cells()[cell];
        let s = c.session(session).unwrap();
        let at = c
            .world()
            .get::<mantis_server::components::Body>(s.avatar.unwrap())
            .unwrap()
            .0
            .position;
        let token = [9u8; 32];
        let to = mantis_adapter_contract::HandOff {
            cell: c.id().0,
            address: mantis_adapter_contract::core_types::WireString::new(&addr_b.to_string()).unwrap(),
            token: mantis_adapter_contract::core_types::BoundedArray::from_slice(&token).unwrap(),
        };
        (to, (token, (s.character, at)))
    };
    tokens.0.lock().unwrap().grants.insert(grant.0, grant.1);
    assert!(w.host_a.hand_off(session, &to, &mut w.zone_a));
    w.wait("playing on host B, same connection", &|w| {
        !w.bot.stats.transfers.is_empty()
            && w.bot.synced()
            && w.bot.stats.snapshots > before + 30
            && w.host_a.sessions_in_world() == 0
            && w.host_b.sessions_in_world() == 1
    });
    assert_eq!(w.bot.stats.transfers.len(), 1);
    assert_eq!(w.bot.stats.transfers[0].1, 1, "epoch 1");
    assert_eq!(
        w.bot.stats.undecodable, 0,
        "every frame decodes across the hand-off"
    );
    assert!(w.bot.stats.refused.is_none());
    assert_eq!(w.gateway.stats.handed_off, 1);
    assert!(w.gateway.stats.stamped > 0);
}

/// Two cell hosts, a gateway and a bot, stepped on the wall clock.
struct TwoHosts {
    gateway: mantis_net::gateway::Gateway,
    host_a: mantis_server::host::Host,
    zone_a: mantis_server::zone::Zone,
    host_b: mantis_server::host::Host,
    zone_b: mantis_server::zone::Zone,
    bot: Bot,
    period: std::time::Duration,
    started: std::time::Instant,
}

impl TwoHosts {
    fn step(&mut self) {
        self.gateway
            .poll(u64::try_from(self.started.elapsed().as_millis()).unwrap());
        self.host_a.poll(&mut self.zone_a);
        self.zone_a.step(&mut self.host_a, None).unwrap();
        self.host_b.poll(&mut self.zone_b);
        self.zone_b.step(&mut self.host_b, None).unwrap();
        self.bot.step();
        std::thread::sleep(self.period);
    }

    fn wait(&mut self, what: &str, done: &dyn Fn(&Self) -> bool) {
        for _ in 0..600 {
            if done(self) {
                return;
            }
            self.step();
        }
        assert!(done(self), "{what}: not within 600 ticks");
    }
}

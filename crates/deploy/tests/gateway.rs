//! The gateway role through a process-per-role cluster: every service role
//! and the gateway a `mantisd` process, the cell host the toy package's
//! own binary (`toy-server node cell-host`), and headless bots that log in
//! through the account and realm roles (the gateway's login flow, with a
//! gateway certificate) and reach the game only through the gateway's
//! address.
//!
//! - Bots play through the gateway while its client certificate is
//!   rotated in place: no session is lost, new handshakes get the new
//!   chain.
//! - The cell host is swapped behind the gateway: the host serving cells
//!   1-3 drains, a newer registry moves the cells to another cell-host
//!   instance (a new process at new addresses, recovering the cells from
//!   the state the first one left), and the bots log in again and play
//!   through the same gateway address, never told where the host went.
//! - An entry token is single use: presented once it routes through the
//!   gateway to the host; once the host has redeemed it, the same token on
//!   another connection is refused `BadToken` at the gateway.
//!
//! The toy-server executable is not a dependency of this crate (engine
//! crates never depend on packages): it is run from the target directory,
//! next to `mantisd`. The gates build every target first; run alone, build
//! it with `cargo build -p toy-server` (same profile and target directory).

#![expect(clippy::unwrap_used, clippy::too_many_lines, clippy::indexing_slicing)]

mod support;

use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::time::{Duration, Instant};

use mantis_adapter_contract::RefuseReason;
use mantis_core::content::ContentHash;
use mantis_core::kinematics::{FlatGround, MotionParams};
use mantis_core::time::TickRate;
use mantis_deploy::keys;
use mantis_net::NetRuntime;
use mantis_net::quic::{QuicClient, ServerTrust};
use mantis_server::bots::{Bot, BotConfig, NativeWire, Profile};
use mantis_services::generated::services as m;
use mantis_services::host::{RPC_TIMEOUT, Role};
use mantis_services::methods;
use support::{CellSpec, Cluster, GATEWAY, TestStore, service, wait_for};

const HOST_A: &str = "cells-a";
const HOST_B: &str = "cells-b";
/// The cells the swap moves.
const CELLS: [u64; 3] = [1, 2, 3];

fn workspace() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("..").join("..")
}

/// The toy-server executable beside `mantisd`, or a failure naming the
/// build command.
fn toy_server() -> PathBuf {
    let mantisd = Path::new(env!("CARGO_BIN_EXE_mantisd"));
    let name = format!("toy-server{}", std::env::consts::EXE_SUFFIX);
    let path = mantisd.with_file_name(&name);
    assert!(
        path.is_file(),
        "{} is missing: this test runs the toy package's cell host. Build it first with \
         `cargo build -p toy-server` (the same profile and CARGO_TARGET_DIR as this test), \
         as the gates' build step does.",
        path.display()
    );
    path
}

fn cooked() -> PathBuf {
    let dir = workspace().join("packages").join("toy").join("cooked");
    assert!(
        dir.join("keys").join("dev.pub").is_file(),
        "{} is not cooked: cargo run -p mantis-cook -- packages/toy",
        dir.display()
    );
    dir
}

/// Native bots that log in (account, realm) with a gateway certificate and
/// dial only the gateway, trusting the cluster CA.
fn bots(cluster: &Cluster, gateway: SocketAddr, count: u32, seconds: u32, seed: u32) -> Child {
    Command::new(toy_server())
        .arg("bots")
        .args(["--count", &count.to_string(), "--seconds", &seconds.to_string()])
        .args(["--seed", &seed.to_string()])
        .arg("--cooked")
        .arg(cooked())
        .args(["--gateway", &gateway.to_string()])
        .arg("--ca")
        .arg(cluster.dir.join("keys").join(keys::files::CA))
        .args(cluster.bot_login_args())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap()
}

/// Waits for a bots process and returns what it printed.
fn bots_output(child: Child) -> String {
    let out = child.wait_with_output().unwrap();
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(out.status.success(), "bots failed:\n{text}");
    text
}

/// The bots' last progress line, which must show all `count` in the world
/// and none refused.
fn all_played(text: &str, count: u32, what: &str) -> String {
    let last = text
        .lines()
        .rev()
        .find(|l| l.contains(" in world"))
        .unwrap_or("")
        .to_owned();
    assert!(
        last.contains(&format!(": {count} in world")) && last.contains(" 0 refused"),
        "{what}:\n{text}"
    );
    last
}

/// `MANTIS-RECOVERY` lines of `output`'s first start: cell -> snapshot tick.
fn recovered_from(output: &str) -> BTreeMap<u64, u64> {
    output
        .lines()
        .filter_map(|l| l.strip_prefix("MANTIS-RECOVERY "))
        .map(|l| {
            let v: BTreeMap<&str, u64> = l
                .split(' ')
                .filter_map(|kv| kv.split_once('='))
                .map(|(k, v)| (k, v.parse().unwrap()))
                .collect();
            (v["cell"], v["snapshot_tick"])
        })
        .collect()
}

/// One raw native client presenting `token` through the gateway: its
/// handshake answer (a refusal, or `None` when welcomed). Its content hash
/// is none of the package's, so a host it reaches refuses it
/// `ContentMismatch`, before any admission: the token is not redeemed.
fn present(net: &NetRuntime, gateway: SocketAddr, ca: &[u8], token: &[u8]) -> Option<RefuseReason> {
    let trust = ServerTrust::from_pem_bundle(ca, &gateway.ip().to_string()).unwrap();
    let client = QuicClient::connect_trusted(net, gateway, &trust).unwrap();
    let mut bot = Bot::new(
        Box::new(NativeWire::default()),
        Box::new(client),
        Arc::new(FlatGround(0.0)),
        BotConfig {
            profile: Profile::Idle,
            motion: MotionParams::DEFAULT,
            rate: TickRate::HZ_30,
            seed: 1,
            content: ContentHash::of(b"a raw client"),
            clock_offset_ms: 0,
        },
        mantis_core::math::Vec3::ZERO,
    )
    .unwrap()
    .with_token(token);
    bot.start();
    let start = Instant::now();
    while !bot.welcomed() && bot.stats.refused.is_none() {
        assert!(start.elapsed() < Duration::from_secs(15), "no handshake answer");
        bot.step();
        std::thread::sleep(Duration::from_millis(10));
    }
    bot.stats.refused
}

fn wire<const N: usize>(s: &str) -> mantis_core::wire::WireString<N> {
    mantis_core::wire::WireString::new(s).unwrap()
}

fn metric(cluster: &Cluster, name: &str, key: &str) -> i64 {
    cluster.metrics(name).get(key).copied().unwrap_or(0)
}

/// Copies every file of `from` into `to` (a cell host's state directory).
fn copy_state(from: &Path, to: &Path) {
    std::fs::create_dir_all(to).unwrap();
    for entry in std::fs::read_dir(from).unwrap() {
        let path = entry.unwrap().path();
        if path.is_file() {
            std::fs::copy(&path, to.join(path.file_name().unwrap())).unwrap();
        }
    }
}

#[test]
fn bots_log_in_and_play_through_the_gateway_across_a_certificate_rotation_and_a_cell_host_swap() {
    let cooked = cooked();
    let toy = toy_server();
    // Both hosts run the same world (cells 1-3); the registry lists the
    // second one elsewhere until the swap moves the cells to it.
    let package = format!(
        "quic = \"{{quic}}\"\ntcp = \"{{tcp}}\"\ncert_out = \"toy-cert.der\"\ncooked = \"{}\"\n\
         seed = 7\nfirst_cell = 1\nworld = 0\n",
        cooked.display().to_string().replace('\\', "/"),
    );
    let mut cluster = Cluster::with_gateway(
        "gateway",
        &TestStore::Memory,
        &[
            CellSpec {
                name: HOST_A,
                cells: CELLS.to_vec(),
                package: package.clone(),
                game_tls: true,
            },
            CellSpec {
                name: HOST_B,
                cells: vec![4, 5, 6],
                package,
                game_tls: true,
            },
        ],
    );
    for host in [HOST_A, HOST_B] {
        let (quic, tcp) = cluster.game[host];
        let config = cluster.config(host);
        let text = std::fs::read_to_string(&config)
            .unwrap()
            .replace("{quic}", &quic.to_string())
            .replace("{tcp}", &tcp.to_string());
        std::fs::write(&config, text).unwrap();
    }
    let gateway = cluster.gateway.unwrap();

    for role in [
        Role::Persist,
        Role::Account,
        Role::Realm,
        Role::Social,
        Role::Matchmaking,
        Role::Ops,
    ] {
        cluster.start(&service(role));
    }
    cluster.start(GATEWAY);
    cluster.start_with(HOST_A, &toy, &["node"]);
    cluster.wait_ready(GATEWAY, Duration::from_secs(60));
    cluster.wait_ready(HOST_A, Duration::from_secs(120));
    assert!(
        cluster
            .output(GATEWAY)
            .contains(&format!("game {gateway} (QUIC)")),
        "{}",
        cluster.output(GATEWAY)
    );

    // Bots log in and play through the gateway; meanwhile its client
    // certificate is rotated in place (new files from the cluster CA).
    let first = bots(&cluster, gateway, 4, 8, 1);
    std::thread::sleep(Duration::from_secs(3));
    let keys_dir = cluster.dir.join("keys");
    let ca = keys::read_ca(&keys_dir).unwrap();
    let renewed = mantis_deploy::pki::issue_server(
        &ca,
        "gateway",
        &[gateway.ip().to_string()],
        mantis_deploy::pki::Validity::starting_now(std::time::SystemTime::now(), 7),
    )
    .unwrap();
    std::fs::write(keys_dir.join("gateway-front.key"), &renewed.key_pem).unwrap();
    std::fs::write(keys_dir.join("gateway-front.crt"), &renewed.cert_pem).unwrap();
    wait_for(
        "the gateway's client certificate rotated",
        Duration::from_secs(10),
        || metric(&cluster, GATEWAY, "gateway_tls_rotations") == 1,
    );
    let first = bots_output(first);
    assert!(
        first.contains(&format!("the gateway at {gateway}")) && first.contains("4 logged in"),
        "{first}"
    );
    let line = all_played(
        &first,
        4,
        "bots through the gateway across its certificate rotation",
    );
    println!("bots through the gateway across a client certificate rotation: {line}");
    assert!(metric(&cluster, GATEWAY, "gateway_joined") >= 4);
    assert_eq!(metric(&cluster, GATEWAY, "gateway_refused"), 0);
    assert_eq!(metric(&cluster, GATEWAY, "gateway_tls_refused"), 0);

    // The swap: the first host drains (a final snapshot of every cell), its
    // state goes with the cells, and a newer registry moves the cells to
    // the second instance, which starts at its own addresses.
    let swap = Instant::now();
    assert_eq!(cluster.drain(HOST_A, Duration::from_secs(60)).code(), Some(0));
    copy_state(
        &cluster.dir.join("state").join(HOST_A),
        &cluster.dir.join("state").join(HOST_B),
    );
    let mut next = cluster.registry.clone();
    next.serial += 1;
    next.instances.retain(|i| i.name != HOST_A);
    for i in &mut next.instances {
        if i.name == HOST_B {
            i.cells = CELLS.to_vec();
        }
    }
    let serial = i64::try_from(next.serial).unwrap();
    cluster.publish(next);
    for node in [GATEWAY.to_owned(), service(Role::Realm), service(Role::Ops)] {
        wait_for("the moved registry applied", Duration::from_secs(15), || {
            metric(&cluster, &node, "registry_serial") == serial
        });
    }
    cluster.start_with(HOST_B, &toy, &["node"]);
    cluster.wait_ready(HOST_B, Duration::from_secs(120));
    let swap_ms = swap.elapsed().as_millis();
    let recovered = recovered_from(&cluster.output(HOST_B));
    assert_eq!(
        recovered.keys().copied().collect::<Vec<_>>(),
        CELLS.to_vec(),
        "{recovered:?}"
    );
    assert!(
        recovered.values().all(|tick| *tick > 0),
        "the cells came back from the first host's snapshots: {recovered:?}"
    );
    println!("MANTIS-METRIC deploy_gateway_cell_host_swap_ms={swap_ms}");

    // The same players log in again and play through the same gateway
    // address, on the host that now serves their cells.
    let joined = metric(&cluster, GATEWAY, "gateway_joined");
    let again = bots_output(bots(&cluster, gateway, 4, 8, 1));
    let line = all_played(&again, 4, "bots through the gateway after the cell-host swap");
    println!("bots through the gateway after a cell-host swap: {line}");
    assert!(metric(&cluster, GATEWAY, "gateway_joined") >= joined + 4);
    assert_eq!(metric(&cluster, GATEWAY, "gateway_refused"), 0);
    assert!(metric(&cluster, HOST_B, "cell_tick") > 0);

    // An entry token is single use. One login, one select (as the gateway's
    // login flow), one token: through the gateway it routes to the host
    // (whose answer to this raw client is its own refusal). The host
    // redeems it as it does at admission; on another connection the same
    // token is then refused `BadToken` at the gateway, and a second
    // redemption is refused too.
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let account = cluster.client(GATEWAY, &service(Role::Account));
    let realm = cluster.client(GATEWAY, &service(Role::Realm));
    let host_realm = cluster.client(HOST_B, &service(Role::Realm));
    rt.block_on(account.call::<methods::RegisterAccount>(
        &m::Register {
            name: wire("replay-1"),
            password: wire("replay password 1"),
        },
        RPC_TIMEOUT,
    ))
    .unwrap();
    let session = rt
        .block_on(account.call::<methods::LoginAccount>(
            &m::Login {
                name: wire("replay-1"),
                password: wire("replay password 1"),
            },
            RPC_TIMEOUT,
        ))
        .unwrap();
    let character = rt
        .block_on(realm.call::<methods::NewCharacter>(
            &m::CreateCharacter {
                account: session.account,
                name: wire("replayer"),
                kind: 1,
            },
            RPC_TIMEOUT,
        ))
        .unwrap()
        .character;
    let entry = rt
        .block_on(realm.call::<methods::Select>(
            &m::SelectCharacter {
                account: session.account,
                character,
            },
            RPC_TIMEOUT,
        ))
        .unwrap();
    let token: Vec<u8> = entry.token.iter().copied().collect();
    let clients = NetRuntime::new(1).unwrap();
    let ca = std::fs::read(cluster.dir.join("keys").join(keys::files::CA)).unwrap();
    let refused_before = metric(&cluster, GATEWAY, "gateway_refused");
    assert_eq!(
        present(&clients, gateway, &ca, &token),
        Some(RefuseReason::ContentMismatch),
        "an unredeemed token routes through the gateway to the host"
    );
    let cells = m::RedeemOnHost {
        token: entry.token,
        cells: mantis_core::wire::BoundedArray::from_slice(&CELLS.map(m::CellNo)).unwrap(),
    };
    rt.block_on(host_realm.call::<methods::RedeemForHost>(&cells, RPC_TIMEOUT))
        .unwrap();
    assert_eq!(
        present(&clients, gateway, &ca, &token),
        Some(RefuseReason::BadToken),
        "a redeemed token on another connection"
    );
    assert!(
        rt.block_on(host_realm.call::<methods::RedeemForHost>(&cells, RPC_TIMEOUT))
            .is_err(),
        "redeemed once only"
    );
    wait_for(
        "the replay counted at the gateway",
        Duration::from_secs(5),
        || metric(&cluster, GATEWAY, "gateway_refused") == refused_before + 1,
    );
    println!("an entry token: routed once, redeemed once, refused BadToken on another connection");

    // Everything drains cleanly, the gateway first.
    assert_eq!(cluster.drain(GATEWAY, Duration::from_secs(30)).code(), Some(0));
    assert!(
        cluster
            .output(GATEWAY)
            .contains("stopped: the game listener closed")
    );
    assert_eq!(cluster.drain(HOST_B, Duration::from_secs(60)).code(), Some(0));
    for role in [
        Role::Ops,
        Role::Matchmaking,
        Role::Social,
        Role::Realm,
        Role::Account,
        Role::Persist,
    ] {
        assert_eq!(
            cluster.drain(&service(role), Duration::from_secs(30)).code(),
            Some(0)
        );
    }
}

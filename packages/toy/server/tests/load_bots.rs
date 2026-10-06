//! Logged-in load bots: the gateway login flow (account, realm, entry
//! token) admits bots into a cluster that verifies game tokens, in process
//! (plaintext and over mutual TLS) and with the real binaries
//! (`toy-server cluster --verify-tokens --login-out`, then
//! `toy-server bots --login`).

#![expect(clippy::unwrap_used, clippy::indexing_slicing)]

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use mantis_server::bots::Profile;
use mantis_server::host::AdmissionLimits;
use mantis_server::simnet::LinkConfig;
use mantis_services::cluster::{CellLink, CellLinkConfig, ClusterConfig, LocalCluster, TokenVerifier};
use mantis_services::host::Role;
use mantis_services::tls::dev::DevCa;
use toy_server::cluster::RealmAdmission;
use toy_server::login::{Gateway, LoginTargets};
use toy_server::sim::{Side, Sim};
use toy_server::tunables::Tunables;

fn targets(cluster: &LocalCluster) -> LoginTargets {
    LoginTargets {
        account: cluster.addr(Role::Account).unwrap(),
        realm: cluster.addr(Role::Realm).unwrap(),
        key: cluster.key.clone(),
    }
}

/// Logs three bots in through `cluster`'s roles and admits them into a
/// zone that verifies their tokens with the realm (as a cell host with
/// `cell_tls`).
fn three_bots_log_in_and_are_admitted(
    cluster: &LocalCluster,
    gateway: &Gateway,
    cell_tls: Option<std::sync::Arc<mantis_services::tls::TlsIdentity>>,
) {
    let handle = cluster.handle();
    // A cell host registers its cells with the realm, which places the bots.
    let _link = CellLink::start(
        &handle,
        &CellLinkConfig {
            key: cluster.key.clone(),
            persist: cluster.addr(Role::Persist).unwrap(),
            ops: cluster.addr(Role::Ops).unwrap(),
            social: cluster.addr(Role::Social).unwrap(),
            matchmaking: cluster.addr(Role::Matchmaking).unwrap(),
            realm: cluster.addr(Role::Realm).unwrap(),
            live_key: cluster.ops.public_key(),
            cells: toy_server::world::regions()
                .into_iter()
                .zip(1u64..)
                .map(|(r, id)| (id, "127.0.0.1:7400".to_owned(), r))
                .collect(),
            poll: Duration::from_millis(20),
            instances: Vec::new(),
            inspector: "127.0.0.1:0".parse().unwrap(),
            tls: cell_tls.clone(),
        },
    )
    .unwrap();
    let entries: Vec<_> = (0..3)
        .map(|n| {
            handle
                .block_on(gateway.enter(&format!("bot-7-{n}"), "bot password 7"))
                .unwrap()
        })
        .collect();
    // A second login of the same bot reuses its account and character,
    // with a fresh single-use token.
    let again = handle
        .block_on(gateway.enter("bot-7-0", "bot password 7"))
        .unwrap();
    assert_eq!(
        (again.account, again.character),
        (entries[0].account, entries[0].character)
    );
    assert_ne!(again.token, entries[0].token);
    // A wrong password for an existing name is refused.
    assert!(
        handle
            .block_on(gateway.enter("bot-7-1", "another password"))
            .is_err()
    );

    let t = Tunables::defaults().unwrap();
    let mut sim = Sim::new(t, 8, |_| None).unwrap();
    let realm = cluster.addr(Role::Realm).unwrap();
    let cells: Vec<u64> = (1..=toy_server::world::regions().len() as u64).collect();
    let verifier = TokenVerifier::with_tls(&handle, realm, cluster.key.clone(), cell_tls, &cells).unwrap();
    sim.host
        .set_admission(Box::new(RealmAdmission(verifier)), AdmissionLimits::DEFAULT);
    // Every token is single use and still unused here (entries[0]'s
    // account logged in again: `again` carries its newer token).
    for e in &entries[1..] {
        sim.add_bot_with_token(Side::Native, Profile::Idle, LinkConfig::PERFECT, Some(&e.token))
            .unwrap();
    }
    sim.add_bot_with_token(
        Side::Legacy,
        Profile::Idle,
        LinkConfig::PERFECT,
        Some(&again.token),
    )
    .unwrap();
    // A bot without a token is refused.
    sim.add_bot(Side::Native, Profile::Idle, LinkConfig::PERFECT)
        .unwrap();
    let start = Instant::now();
    while sim
        .bots
        .iter()
        .filter(|b| b.bot.welcomed() || b.bot.stats.refused.is_some())
        .count()
        < 4
    {
        assert!(
            start.elapsed() < Duration::from_secs(10),
            "verdicts never arrived"
        );
        sim.step().unwrap();
        std::thread::sleep(Duration::from_millis(2));
    }
    assert_eq!(sim.host.stats.joined, 3, "the three logged-in bots joined");
    assert!(
        sim.bots[3].bot.stats.refused.is_some(),
        "the bot without a token was refused"
    );
    let mut characters: Vec<u64> = sim
        .zone
        .cells()
        .iter()
        .flat_map(|c| {
            c.world()
                .resource::<mantis_server::session::Sessions>()
                .map(|s| s.map.values().map(|x| x.character).collect::<Vec<_>>())
                .unwrap_or_default()
        })
        .collect();
    characters.sort_unstable();
    let mut expected: Vec<u64> = entries.iter().map(|e| e.character).collect();
    expected.sort_unstable();
    assert_eq!(characters, expected, "each joined as its own character");
}

#[test]
fn logged_in_bots_are_admitted_by_a_cluster_verifying_tokens() {
    let cluster = LocalCluster::start(&ClusterConfig::local()).unwrap();
    let gateway = Gateway::new(&targets(&cluster), None).unwrap();
    three_bots_log_in_and_are_admitted(&cluster, &gateway, None);
}

#[test]
fn logged_in_bots_are_admitted_over_mutual_tls() {
    let ca = DevCa::new("bots").unwrap();
    let ids = ca.every_role(&["127.0.0.1".parse().unwrap()]).unwrap();
    let mut config = ClusterConfig::local();
    config.tls = Some(ids.clone());
    let cluster = LocalCluster::start(&config).unwrap();
    // Without a certificate the gateway cannot reach the roles.
    let plain = Gateway::new(&targets(&cluster), None).unwrap();
    assert!(
        cluster
            .handle()
            .block_on(plain.enter("bot-7-0", "bot password 7"))
            .is_err()
    );
    let gateway = Gateway::new(&targets(&cluster), Some(&ids[&Role::Gateway])).unwrap();
    three_bots_log_in_and_are_admitted(&cluster, &gateway, Some(ids[&Role::Cell].clone()));
}

// ---- the binaries ------------------------------------------------------------

/// Kills the server when the test ends, however it ends.
struct Running(Child);

impl Drop for Running {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn toy() -> Command {
    Command::new(env!("CARGO_BIN_EXE_toy-server"))
}

fn package(path: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("..").join(path)
}

fn bots(dir: &Path, login: bool) -> String {
    let mut cmd = toy();
    cmd.args([
        "bots",
        "--profile",
        "idle",
        "--count",
        "3",
        "--seconds",
        "4",
        "--seed",
        "5",
    ])
    .arg("--cert")
    .arg(dir.join("cert.der"))
    .arg("--cooked")
    .arg(package("cooked"))
    .arg("--key")
    .arg(package("cooked/keys/dev.pub"));
    if login {
        cmd.arg("--login").arg(dir.join("login.txt"));
    } else {
        // Without logging in, the bots dial the cluster's game address.
        let text = std::fs::read_to_string(dir.join("server.out")).unwrap_or_default();
        let quic = text
            .lines()
            .find_map(|l| l.strip_prefix("toy-server: native (QUIC) on "))
            .and_then(|l| l.split(',').next())
            .unwrap()
            .to_owned();
        cmd.args(["--quic", &quic]);
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
fn toy_server_bots_log_in_and_play_on_a_cluster_verifying_tokens() {
    let dir = std::env::temp_dir().join(format!("mantis-load-bots-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let out = std::fs::File::create(dir.join("server.out")).unwrap();
    let server = Running(
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
            .stdout(Stdio::from(out))
            .stderr(Stdio::null())
            .spawn()
            .unwrap(),
    );
    let start = Instant::now();
    while !dir.join("login.txt").exists() || !dir.join("cert.der").exists() {
        assert!(
            start.elapsed() < Duration::from_secs(60),
            "the cluster never wrote its login file"
        );
        std::thread::sleep(Duration::from_millis(50));
    }
    std::thread::sleep(Duration::from_millis(200));

    let logged_in = bots(&dir, true);
    assert!(
        logged_in.contains("3 logged in through the account and realm roles"),
        "{logged_in}"
    );
    assert!(
        logged_in.contains("after 4 s: 3 in world, 0 refused"),
        "{logged_in}"
    );
    // The same bots without logging in present no entry token: none gets
    // into the world.
    let anonymous = bots(&dir, false);
    assert!(anonymous.contains("after 4 s: 0 in world"), "{anonymous}");
    drop(server);
    let _ = std::fs::remove_dir_all(&dir);
}

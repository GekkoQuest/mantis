//! Where a node's registry comes from, and following newer ones without a
//! restart: a file, a directory a deploy tool writes into, and HTTPS from
//! `mantisd registry serve` with a pinned CA. Only a verified registry
//! with a higher serial, the same cluster and this node's own entry is
//! applied; anything else is refused and the running one stays.

#![expect(clippy::unwrap_used)]

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use mantis_deploy::keys;
use mantis_deploy::pki;
use mantis_deploy::publish::RegistryServer;
use mantis_deploy::registry::{Instance, Registry, sign};
use mantis_deploy::source::{Follow, Source, follow};
use mantis_deploy::target::Target;
use mantis_services::host::Role;
use ring::signature::Ed25519KeyPair;

fn dir(name: &str) -> PathBuf {
    let d =
        Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!("registry-source-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

fn registry(serial: u64, cluster: &str, persist: &str) -> Registry {
    Registry {
        serial,
        cluster: cluster.to_owned(),
        live_key: [1; 32],
        cas: vec![vec![0x30, 0x82, 0, 0]],
        instances: vec![
            Instance {
                name: "persist-1".to_owned(),
                role: Role::Persist,
                rpc: Target::parse(persist).unwrap(),
                health: Target::parse("persist.services.internal:7605").unwrap(),
                cells: Vec::new(),
                lease_owner: None,
            },
            Instance {
                name: "social-1".to_owned(),
                role: Role::Social,
                rpc: Target::parse("social.services.internal:7503").unwrap(),
                health: Target::parse("social.services.internal:7603").unwrap(),
                cells: Vec::new(),
                lease_owner: None,
            },
        ],
    }
}

struct Keys {
    deploy: Ed25519KeyPair,
    public: [u8; 32],
}

fn keys_in(d: &Path) -> Keys {
    keys::new_keys(&d.join("keys"), "dev", 30).unwrap();
    Keys {
        deploy: keys::read_key_pair(&d.join("keys").join(keys::files::DEPLOY)).unwrap(),
        public: keys::read_public_key(&d.join("keys").join(keys::files::DEPLOY_PUBLIC)).unwrap(),
    }
}

fn runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap()
}

#[test]
fn a_directory_offers_its_highest_verified_serial() {
    let d = dir("dir");
    let k = keys_in(&d);
    let regs = d.join("registries");
    std::fs::create_dir_all(&regs).unwrap();
    std::fs::write(
        regs.join("a.toml"),
        sign(
            &registry(4, "dev", "persist.services.internal:7505").render(),
            &k.deploy,
        ),
    )
    .unwrap();
    std::fs::write(
        regs.join("b.toml"),
        sign(
            &registry(9, "dev", "persist.services.internal:7505").render(),
            &k.deploy,
        ),
    )
    .unwrap();
    // Higher, but not signed by the deploy key: never wins.
    let (other, _) = keys::new_key_pair().unwrap();
    let other = Ed25519KeyPair::from_pkcs8(&other).unwrap();
    std::fs::write(
        regs.join("c.toml"),
        sign(&registry(50, "dev", "evil.example:1").render(), &other),
    )
    .unwrap();
    std::fs::write(regs.join("notes.txt"), "not a registry").unwrap();
    let rt = runtime();
    let source = Source::parse("dir:registries", &d, None).unwrap();
    assert_eq!(rt.block_on(source.load(&k.public)).unwrap().serial, 9);
    std::fs::remove_file(regs.join("a.toml")).unwrap();
    std::fs::remove_file(regs.join("b.toml")).unwrap();
    let e = rt.block_on(source.load(&k.public)).unwrap_err();
    assert!(
        e.contains("no registry verifies") && e.contains("does not verify"),
        "{e}"
    );
}

#[test]
fn https_with_a_pinned_ca_serves_registries_and_another_ca_is_refused() {
    let d = dir("https");
    let k = keys_in(&d);
    let kd = d.join("keys");
    let ca = keys::read_ca(&kd).unwrap();
    let ok = pki::Validity::starting_now(std::time::SystemTime::now(), 7);
    let leaf = pki::issue_server(
        &ca,
        "deploy",
        &["localhost".to_owned(), "127.0.0.1".to_owned()],
        ok,
    )
    .unwrap();
    std::fs::write(kd.join("deploy.crt"), &leaf.cert_pem).unwrap();
    std::fs::write(kd.join("deploy.key"), &leaf.key_pem).unwrap();
    let published = d.join("served");
    std::fs::create_dir_all(&published).unwrap();
    std::fs::write(
        published.join("registry.toml"),
        sign(
            &registry(3, "dev", "persist.services.internal:7505").render(),
            &k.deploy,
        ),
    )
    .unwrap();
    let rt = runtime();
    let server = rt
        .block_on(RegistryServer::start(
            "127.0.0.1:0".parse().unwrap(),
            published.clone(),
            &kd.join("deploy.crt"),
            &kd.join("deploy.key"),
        ))
        .unwrap();
    let port = server.addr().port();
    let pinned = |spec: &str, ca_file: &Path| Source::parse(spec, &d, Some(ca_file.to_path_buf())).unwrap();

    // By name (the certificate names localhost) and by address.
    for host in ["localhost", "127.0.0.1"] {
        let s = pinned(
            &format!("https://{host}:{port}/registry.toml"),
            &kd.join(keys::files::CA),
        );
        assert_eq!(rt.block_on(s.load(&k.public)).unwrap().serial, 3, "{host}");
    }
    // A missing file, a path outside the directory: not served.
    for path in ["/nothing.toml", "/../keys/ca.key", "/keys%2fca.key"] {
        let s = pinned(
            &format!("https://localhost:{port}{path}"),
            &kd.join(keys::files::CA),
        );
        let e = rt.block_on(s.load(&k.public)).unwrap_err();
        assert!(e.contains("answered 404"), "{path}: {e}");
    }
    // Pinned to another CA: the server is not trusted.
    let other = pki::new_ca("dev", ok).unwrap();
    std::fs::write(d.join("other-ca.crt"), &other.cert_pem).unwrap();
    let s = pinned(
        &format!("https://localhost:{port}/registry.toml"),
        &d.join("other-ca.crt"),
    );
    let e = rt.block_on(s.load(&k.public)).unwrap_err();
    assert!(e.to_lowercase().contains("certificate"), "{e}");
    // A tampered registry over a trusted transport: still refused.
    let good = std::fs::read_to_string(published.join("registry.toml")).unwrap();
    std::fs::write(published.join("registry.toml"), good.replace("7505", "7506")).unwrap();
    let s = pinned(
        &format!("https://localhost:{port}/registry.toml"),
        &kd.join(keys::files::CA),
    );
    let e = rt.block_on(s.load(&k.public)).unwrap_err();
    assert!(e.contains("does not verify"), "{e}");
    let _guard = rt.enter();
    drop(server);
}

#[test]
fn a_node_follows_only_newer_registries_of_its_cluster_that_keep_it() {
    let d = dir("follow");
    let k = keys_in(&d);
    let path = d.join("registry.toml");
    let write = |r: &Registry| std::fs::write(&path, sign(&r.render(), &k.deploy)).unwrap();
    let first = registry(5, "dev", "persist.services.internal:7505");
    write(&first);
    let rt = runtime();
    let reports: Arc<Mutex<Vec<String>>> = Arc::default();
    let log = Arc::clone(&reports);
    let live = follow(
        rt.handle(),
        first,
        Follow {
            source: Source::parse("registry.toml", &d, None).unwrap(),
            deploy_key: k.public,
            instance: "social-1".to_owned(),
            every: Duration::from_millis(20),
            report: Arc::new(move |r| {
                log.lock().unwrap().push(match r {
                    Ok(next) => format!("applied {}", next.serial),
                    Err(e) => e,
                });
            }),
        },
    );
    let mut seen = live.subscribe();
    let wait = |what: &str, ok: &dyn Fn() -> bool| {
        let start = std::time::Instant::now();
        while !ok() {
            assert!(
                start.elapsed() < Duration::from_secs(10),
                "{what}: {:?}",
                reports.lock().unwrap()
            );
            std::thread::sleep(Duration::from_millis(10));
        }
    };

    // Persist moves to another host and port: a higher serial is applied.
    write(&registry(6, "dev", "persist-b.services.internal:7605"));
    wait("serial 6", &|| live.current().serial == 6);
    assert!(rt.block_on(seen.changed()).is_ok());
    assert_eq!(
        live.current().one(Role::Persist).unwrap().rpc.to_string(),
        "persist-b.services.internal:7605"
    );
    // An older serial (a rollback), a tampered file, another cluster, and a
    // registry without this node: refused; the running one stays.
    write(&registry(4, "dev", "rollback.example:1"));
    std::thread::sleep(Duration::from_millis(100));
    let tampered = sign(
        &registry(7, "dev", "persist.services.internal:7505").render(),
        &k.deploy,
    )
    .replace("7505", "7507");
    std::fs::write(&path, tampered).unwrap();
    wait("the tampered registry refused", &|| {
        reports
            .lock()
            .unwrap()
            .iter()
            .any(|r| r.contains("does not verify"))
    });
    write(&registry(8, "other", "persist.services.internal:7505"));
    wait("another cluster refused", &|| {
        reports.lock().unwrap().iter().any(|r| r.contains("OtherCluster"))
    });
    let mut without = registry(9, "dev", "persist.services.internal:7505");
    without.instances.retain(|i| i.name != "social-1");
    write(&without);
    wait("a registry without this node refused", &|| {
        reports.lock().unwrap().iter().any(|r| r.contains("NotListed"))
    });
    assert_eq!(live.current().serial, 6, "{:?}", reports.lock().unwrap());
    // And the next good one is applied.
    write(&registry(10, "dev", "persist-c.services.internal:7505"));
    wait("serial 10", &|| live.current().serial == 10);
    let applied: Vec<String> = reports
        .lock()
        .unwrap()
        .iter()
        .filter(|r| r.starts_with("applied"))
        .cloned()
        .collect();
    assert_eq!(applied, ["applied 6", "applied 10"]);
}

//! `mantisd local`: every service role in one process, writing the key and
//! a signed registry a process-per-role cell host joins exactly as it joins
//! separate processes.

mod support;

use std::process::{Command, Stdio};
use std::sync::atomic::Ordering;
use std::time::Duration;

use mantis_core::ledger::{GOLD, Ledger, LedgerRow};
use mantis_core::wire::encode_into;
use mantis_deploy::cell::CellNode;
use mantis_deploy::config::NodeConfig;
use mantis_deploy::keys;
use mantis_deploy::node::Node;
use mantis_deploy::registry::Registry;
use mantis_services::cluster::CellOutcome;
use support::{free_ports, wait_for};

#[test]
fn local_mode_runs_every_role_and_a_cell_host_joins_it_through_the_registry() {
    let dir = std::path::Path::new(env!("CARGO_TARGET_TMPDIR"))
        .join(format!("deploy-local-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    keys::new_keys(&dir.join("keys"), "local", 30).unwrap();
    let p = free_ports(4);
    let (dashboard, rpc, health, game) = (p[0], p[1], p[2], p[3]);
    std::fs::write(
        dir.join("local.toml"),
        format!(
            "[local]\nbind = \"127.0.0.1\"\ndashboard = \"127.0.0.1:{dashboard}\"\n\
             operator_token_out = \"state/operator.token\"\ndashboard_cert_out = \"state/ops-cert.der\"\n\
             store = \"memory\"\ndeploy_key = \"keys/deploy.pk8\"\nregistry_out = \"state/registry.toml\"\n\
             cluster_key_out = \"state/cluster.key\"\ncell_host_rpc = \"127.0.0.1:{rpc}\"\n\
             cell_host_health = \"127.0.0.1:{health}\"\ncells = [1, 2]\n\
             cell_host_tls_out = \"state\"\n"
        ),
    )
    .unwrap();
    let mut local = Command::new(env!("CARGO_BIN_EXE_mantisd"))
        .args(["local", "--config"])
        .arg(dir.join("local.toml"))
        .arg("--drain-on-stdin-eof")
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let deploy = keys::read_public_key(&dir.join("keys").join(keys::files::DEPLOY_PUBLIC)).unwrap();
    let registry = dir.join("state").join("registry.toml");
    wait_for("the local registry", Duration::from_secs(60), || {
        std::fs::read_to_string(&registry).is_ok_and(|t| Registry::verify(&t, &deploy).is_ok())
    });
    let r = Registry::verify(&std::fs::read_to_string(&registry).unwrap(), &deploy).unwrap();
    assert_eq!(r.instances.len(), 7, "{}", r.summary());

    std::fs::write(
        dir.join("cells.toml"),
        format!(
            "[node]\nrole = \"cell-host\"\ninstance = \"cell-host-local\"\nregistry = \"state/registry.toml\"\n\
             deploy_key = \"keys/deploy.pub\"\ncluster_key = \"state/cluster.key\"\n\
             tls_cert = \"state/cell-host-local.crt\"\ntls_key = \"state/cell-host-local.key\"\n\
             listen_rpc = \"127.0.0.1:{rpc}\"\nlisten_health = \"127.0.0.1:{health}\"\n\
             [cell_host]\nadvertise = \"127.0.0.1:{game}\"\nstate = \"state/cells\"\nsnapshot_every_ticks = 150\n"
        ),
    )
    .unwrap();
    let node = Node::start(NodeConfig::load(&dir.join("cells.toml")).unwrap(), false).unwrap();
    node.wait_for_dependencies().unwrap();
    let node = CellNode::new(node).unwrap();
    let link = node.link(&[(1, (-100.0, 0.0)), (2, (0.0, 100.0))], &[]).unwrap();
    node.ready();
    let mut l = Ledger::default();
    l.push(LedgerRow {
        character: 5,
        item: GOLD,
        delta: 9,
    })
    .unwrap();
    let mut bytes = Vec::new();
    encode_into(&l, &mut bytes);
    let mut payload = [0u8; 512];
    payload[..bytes.len()].copy_from_slice(&bytes);
    link.push(
        1,
        vec![CellOutcome {
            tick: 3,
            kind: 1043,
            session: 0,
            ok: true,
            payload,
            len: bytes.len(),
        }],
    );
    wait_for("the outcome durable", Duration::from_secs(30), || {
        link.stats.durable_batches.load(Ordering::Relaxed) == 1
    });
    node.finish(link).unwrap();

    drop(local.stdin.take());
    let status = local.wait().unwrap();
    assert_eq!(status.code(), Some(0));
    let _ = std::fs::remove_dir_all(&dir);
}

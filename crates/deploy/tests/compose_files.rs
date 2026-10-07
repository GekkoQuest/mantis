//! The container deployments' files stay valid: every node configuration
//! under `deploy/compose/config` and `deploy/multihost/config` reads
//! strictly, each registry template (filled in as the scripts fill it)
//! parses, and each configuration's instance, role and ports agree with
//! its registry.
//!
//! Native clients reach the game only through the gateway: in each
//! deployment the gateway is the only container publishing a UDP (QUIC)
//! port, and every cell host advertises an address the gateway can dial
//! (an IP), with a game certificate checked against the cluster CA.

#![expect(clippy::unwrap_used, clippy::panic)]

use std::path::Path;

use mantis_deploy::config::NodeConfig;
use mantis_deploy::matrix;
use mantis_deploy::registry::Registry;

fn deployment(name: &str) -> std::path::PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../deploy")
        .join(name)
}

#[test]
fn every_compose_node_configuration_matches_the_registry_template() {
    check(&deployment("compose"));
    only_the_gateway_publishes_the_game(&deployment("compose").join("compose.yaml"));
}

#[test]
fn every_multihost_node_configuration_matches_its_registry_template() {
    check(&deployment("multihost"));
    for file in ["services.yaml", "host.yaml"] {
        only_the_gateway_publishes_the_game(&deployment("multihost").join(file));
    }
}

/// Every service of a compose file that publishes a UDP port is the
/// gateway (the legacy adapter's TCP port is published by the cell host).
fn only_the_gateway_publishes_the_game(file: &Path) {
    let text = std::fs::read_to_string(file).unwrap();
    let mut service = "";
    let mut in_services = false;
    for line in text.lines() {
        if !line.starts_with(' ') && !line.is_empty() {
            in_services = line.starts_with("services:");
        } else if in_services
            && line.starts_with("  ")
            && !line.starts_with("   ")
            && line.trim_end().ends_with(':')
            && !line.trim_start().starts_with('#')
        {
            service = line.trim().trim_end_matches(':');
        }
        if in_services && line.contains("/udp") && !line.trim_start().starts_with('#') {
            assert_eq!(
                service,
                "gateway",
                "{}: {service} publishes a UDP port: native clients reach the game only through the gateway",
                file.display()
            );
            assert!(line.contains("\"127.0.0.1:"), "{}: {line}", file.display());
        }
    }
}

fn check(dir: &Path) {
    let compose = || dir.to_path_buf();
    let body = std::fs::read_to_string(compose().join("registry.body.toml"))
        .unwrap()
        .replace("@SERIAL@", "1")
        .replace("@LIVE_KEY@", &"ab".repeat(32))
        .replace("@CA@", "3082");
    let registry = Registry::parse_body(&body).unwrap();
    let mut seen = Vec::new();
    for entry in std::fs::read_dir(compose().join("config")).unwrap() {
        let path = entry.unwrap().path();
        let config = NodeConfig::load(&path).unwrap_or_else(|e| panic!("{e}"));
        let me = registry
            .instance(&config.instance)
            .unwrap_or_else(|| panic!("{}: {} is not in the registry", path.display(), config.instance));
        assert_eq!(me.role, config.role, "{}", path.display());
        assert_eq!(me.rpc.port(), config.listen_rpc.port(), "{}", path.display());
        assert_eq!(
            me.health.port(),
            config.listen_health.port(),
            "{}",
            path.display()
        );
        if let Some(host) = &config.cell_host {
            assert!(
                host.advertise.parse::<std::net::SocketAddr>().is_ok(),
                "{}: the gateway dials the advertised address: an IP and port",
                path.display()
            );
            assert!(
                host.game_tls.as_ref().is_some_and(|g| g.ca.is_some()),
                "{}: the game certificate is from the cluster CA, checked at start",
                path.display()
            );
        }
        if let Some(gateway) = &config.gateway {
            assert!(gateway.tls.ca.is_some(), "{}", path.display());
            assert!(gateway.hosts_name.ends_with(".mantis"), "{}", path.display());
        }
        if let Some(ops) = &config.ops {
            assert!(
                !ops.dashboard.ip().is_unspecified(),
                "the dashboard binds one network's address"
            );
            assert!(
                registry
                    .instances
                    .iter()
                    .all(|i| i.rpc.ip() != Some(ops.dashboard.ip())),
                "the dashboard is not on the services network"
            );
        }
        seen.push(config.role);
    }
    for role in matrix::DEPLOYED {
        assert!(
            seen.contains(&role),
            "no configuration for {}",
            matrix::name(role)
        );
    }
    assert_eq!(seen.len(), registry.instances.len());
}

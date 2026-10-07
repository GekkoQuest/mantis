//! The container deployments' files stay valid: every node configuration
//! under `deploy/compose/config` and `deploy/multihost/config` reads
//! strictly, each registry template (filled in as the scripts fill it)
//! parses, and each configuration's instance, role and ports agree with
//! its registry.

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
}

#[test]
fn every_multihost_node_configuration_matches_its_registry_template() {
    check(&deployment("multihost"));
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

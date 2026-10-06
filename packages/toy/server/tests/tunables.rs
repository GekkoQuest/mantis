//! The package manifest's tunables: every movement parameter is there with
//! a documented unit, and malformed manifests are refused.

use mantis_core::kinematics::MotionParams;
use mantis_server::movement::EnvelopeConfig;
use toy_server::tunables::{PACKAGE_TOML, TunableError, Tunables};

#[test]
fn every_motion_parameter_is_a_tunable_with_a_unit() {
    let t = Tunables::defaults().unwrap();
    // Exhaustive destructuring: adding a field to MotionParams fails to
    // compile here until it is added to the manifest and this list.
    let MotionParams {
        run_speed: _,
        walk_speed: _,
        backward_scale: _,
        ground_accel: _,
        air_accel: _,
        jump_speed: _,
        gravity: _,
        max_fall_speed: _,
        max_step_up: _,
        max_slope: _,
        ground_snap: _,
    } = t.motion;
    let names = [
        "run_speed",
        "walk_speed",
        "backward_scale",
        "ground_accel",
        "air_accel",
        "jump_speed",
        "gravity",
        "max_fall_speed",
        "max_step_up",
        "max_slope",
        "ground_snap",
    ];
    let motion_keys: Vec<&str> = Tunables::keys()
        .filter(|(t, _)| *t == "motion")
        .map(|(_, k)| k)
        .collect();
    assert_eq!(motion_keys, names);
    for name in names {
        let line = PACKAGE_TOML
            .lines()
            .find(|l| l.trim_start().starts_with(&format!("{name} ")))
            .unwrap_or_else(|| panic!("{name} is in the manifest"));
        let unit = line.split_once('#').map(|(_, u)| u.trim()).unwrap_or_default();
        assert!(!unit.is_empty(), "{name} documents its unit");
    }
    let EnvelopeConfig {
        speed_tolerance: _,
        distance_slack: _,
        jitter_allowance_ms: _,
        vertical_tolerance: _,
        resync_window_ms: _,
    } = t.envelope;
    // The shipped values are the engine's documented defaults.
    assert_eq!(t.motion, MotionParams::DEFAULT);
    assert_eq!(t.envelope, EnvelopeConfig::DEFAULT);
    assert_eq!(t.tick_rate.hz(), 30);
    // The package trades remote updates for bandwidth (budget row
    // `cell-500-100` snapshot bytes).
    assert_eq!(t.interest.budget, 32);
    assert_eq!(t.interest, mantis_server::interest::TierConfig::DEFAULT);
    // Per-remote baselines ship off (the encode row's headroom).
    assert!(!t.interest.snapshot_own_bases);
    assert_eq!(t.interest.own_base_max_age, 64);
    let on = PACKAGE_TOML.replace("snapshot_own_bases = 0 ", "snapshot_own_bases = 1 ");
    assert!(Tunables::parse(&on).unwrap().interest.snapshot_own_bases);
    let neither = PACKAGE_TOML.replace("snapshot_own_bases = 0 ", "snapshot_own_bases = 2 ");
    assert_eq!(
        Tunables::parse(&neither),
        Err(TunableError::BadValue("interest.snapshot_own_bases".into()))
    );
    let too_old = PACKAGE_TOML.replace("own_base_max_age = 64 ", "own_base_max_age = 65 ");
    assert_eq!(
        Tunables::parse(&too_old),
        Err(TunableError::BadValue("interest.own_base_max_age".into()))
    );
}

#[test]
fn malformed_manifests_are_refused() {
    let without_unit = PACKAGE_TOML.replace("run_speed = 7.0              # m/s", "run_speed = 7.0");
    assert_eq!(
        Tunables::parse(&without_unit),
        Err(TunableError::MissingUnit("motion.run_speed".into()))
    );
    let missing = PACKAGE_TOML.replace("gravity = 20.0               # m/s^2, downward", "");
    assert_eq!(Tunables::parse(&missing), Err(TunableError::Missing("gravity")));
    let unknown = format!("{PACKAGE_TOML}\n[tunables.motion]\nswim_speed = 1.0 # m/s\n");
    assert_eq!(
        Tunables::parse(&unknown),
        Err(TunableError::Unknown("motion.swim_speed".into()))
    );
    let twice = format!("{PACKAGE_TOML}\n[tunables.motion]\ngravity = 1.0 # m/s^2\n");
    assert_eq!(
        Tunables::parse(&twice),
        Err(TunableError::Duplicate("motion.gravity".into()))
    );
    let bad = PACKAGE_TOML.replace("ground_snap = 0.3 ", "ground_snap = fast ");
    assert_eq!(
        Tunables::parse(&bad),
        Err(TunableError::BadValue("motion.ground_snap".into()))
    );
    let invalid = PACKAGE_TOML.replace("walk_speed = 2.5 ", "walk_speed = -2.5 ");
    assert!(matches!(Tunables::parse(&invalid), Err(TunableError::Invalid(_))));
    let zero_rate = PACKAGE_TOML.replace("tick_rate = 30 ", "tick_rate = 0 ");
    assert_eq!(
        Tunables::parse(&zero_rate),
        Err(TunableError::BadValue("server.tick_rate".into()))
    );
}

#[test]
fn the_client_tunables_table_is_the_clients_and_skipped_whole() {
    let with_client =
        format!("{PACKAGE_TOML}\n[tunables.client]\ninterp_delay_floor = 100   # ms\nanything_new = fast\n");
    assert_eq!(
        Tunables::parse(&with_client).map(|t| t.motion),
        Tunables::parse(PACKAGE_TOML).map(|t| t.motion)
    );
    // A server table after it is read again.
    let after = format!("{with_client}\n[tunables.motion]\nswim_speed = 1.0 # m/s\n");
    assert_eq!(
        Tunables::parse(&after),
        Err(TunableError::Unknown("motion.swim_speed".into()))
    );
}

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

use super::*;
use crate::hash::StateHash;
use crate::rng::{Rng, Salt, Seed};
use crate::time::{Tick, TickRate};

fn motion() -> Motion {
    Motion::new(MotionParams::DEFAULT).unwrap()
}

fn input(buttons: MoveButtons, yaw: u16, seq: u32) -> MoveInput {
    MoveInput {
        seq: InputSeq(seq),
        tick: Tick(u64::from(seq)),
        buttons,
        yaw: Angle16(yaw),
        aim: AimAngles::default(),
    }
}

fn dt() -> f32 {
    TickRate::HZ_30.dt_seconds()
}

/// A 64 x 64 heightfield with a gentle slope, a bump, and a 2-unit cliff.
fn terrain() -> Heightfield {
    let (w, d) = (64u32, 64u32);
    let mut heights = Vec::with_capacity((w * d) as usize);
    for j in 0..d {
        for i in 0..w {
            let (fi, fj) = (
                f32::from(u16::try_from(i).unwrap()),
                f32::from(u16::try_from(j).unwrap()),
            );
            let mut h = fi * 0.05 + fj * 0.02;
            if (30..34).contains(&i) && (30..34).contains(&j) {
                h += 0.3;
            }
            if i >= 50 {
                h += 2.0;
            }
            heights.push(h);
        }
    }
    Heightfield::new(-32.0, -32.0, 1.0, w, d, heights).unwrap()
}

#[test]
fn params_are_validated() {
    assert!(Motion::new(MotionParams::DEFAULT).is_ok());
    let mut bad = MotionParams::DEFAULT;
    bad.gravity = f32::NAN;
    assert_eq!(Motion::new(bad), Err("gravity"));
    let mut bad = MotionParams::DEFAULT;
    bad.ground_accel = 0.0;
    assert_eq!(Motion::new(bad), Err("ground_accel"));
    let mut bad = MotionParams::DEFAULT;
    bad.run_speed = -1.0;
    assert_eq!(Motion::new(bad), Err("run_speed"));
}

#[test]
fn step_is_pure() {
    let m = motion();
    let g = terrain();
    let start = MotionState::at_rest(Vec3::new(1.0, g.height_at(1.0, 2.0).unwrap(), 2.0), Angle16(0));
    let cmd = input(MoveButtons::FORWARD.with(MoveButtons::STRAFE_LEFT), 12_345, 1);
    let first = m.step(&g, &start, &cmd, &MotionModifiers::NONE, dt());
    let second = m.step(&g, &start, &cmd, &MotionModifiers::NONE, dt());
    assert!(first.bits_eq(&second));
    assert!(!first.bits_eq(&start));
}

#[test]
fn idle_on_flat_ground_does_not_move() {
    let m = motion();
    let g = FlatGround(1.5);
    let mut s = MotionState::at_rest(Vec3::new(3.0, 1.5, -4.0), Angle16(100));
    for n in 0..100 {
        s = m.step(
            &g,
            &s,
            &input(MoveButtons::NONE, 100, n),
            &MotionModifiers::NONE,
            dt(),
        );
    }
    assert!(s.bits_eq(&MotionState::at_rest(Vec3::new(3.0, 1.5, -4.0), Angle16(100))));
}

#[test]
fn running_faces_yaw_and_reaches_run_speed() {
    let m = motion();
    let g = FlatGround(0.0);
    // yaw 0 faces +z; a quarter turn faces +x.
    for (yaw, axis) in [
        (0u16, Vec3::Z),
        (16_384, Vec3::X),
        (32_768, -Vec3::Z),
        (49_152, -Vec3::X),
    ] {
        let mut s = MotionState::at_rest(Vec3::ZERO, Angle16(yaw));
        for n in 0..30 {
            s = m.step(
                &g,
                &s,
                &input(MoveButtons::FORWARD, yaw, n),
                &MotionModifiers::NONE,
                dt(),
            );
        }
        let speed = s.velocity.length();
        assert!((speed - 7.0).abs() < 1e-4, "speed {speed}");
        let dir = s.position.normalize_or_zero();
        assert!(dir.dot(axis) > 0.9999, "yaw {yaw}: dir {dir:?}");
        assert_eq!(s.position.y, 0.0);
        assert!(s.grounded);
    }
}

#[test]
fn modifiers_walk_and_backward_scale_speed() {
    let m = motion();
    let g = FlatGround(0.0);
    let run = |buttons: MoveButtons, mods: MotionModifiers| {
        let mut s = MotionState::at_rest(Vec3::ZERO, Angle16(0));
        for n in 0..60 {
            s = m.step(&g, &s, &input(buttons, 0, n), &mods, dt());
        }
        s.velocity.length()
    };
    let half = MotionModifiers {
        speed_scale: 0.5,
        ..MotionModifiers::NONE
    };
    assert!((run(MoveButtons::FORWARD, half) - 3.5).abs() < 1e-4);
    assert!(
        (run(
            MoveButtons::FORWARD.with(MoveButtons::WALK),
            MotionModifiers::NONE
        ) - 2.5)
            .abs()
            < 1e-4
    );
    assert!((run(MoveButtons::BACKWARD, MotionModifiers::NONE) - 4.2).abs() < 1e-4);
    // Diagonals are normalized, not faster.
    assert!(
        (run(
            MoveButtons::FORWARD.with(MoveButtons::STRAFE_RIGHT),
            MotionModifiers::NONE
        ) - 7.0)
            .abs()
            < 1e-4
    );
    // Opposing buttons cancel.
    assert_eq!(
        run(
            MoveButtons::FORWARD.with(MoveButtons::BACKWARD),
            MotionModifiers::NONE
        ),
        0.0
    );
}

#[test]
fn jump_arc_lands_back_on_the_ground() {
    let m = motion();
    let g = FlatGround(0.0);
    let mut s = MotionState::at_rest(Vec3::ZERO, Angle16(0));
    s = m.step(
        &g,
        &s,
        &input(MoveButtons::JUMP, 0, 0),
        &MotionModifiers::NONE,
        dt(),
    );
    assert!(!s.grounded);
    assert!(s.position.y > 0.0);
    let mut ticks = 1;
    let mut apex = s.position.y;
    while !s.grounded {
        s = m.step(
            &g,
            &s,
            &input(MoveButtons::NONE, 0, ticks),
            &MotionModifiers::NONE,
            dt(),
        );
        apex = apex.max(s.position.y);
        ticks += 1;
        assert!(ticks < 200, "never landed");
    }
    // v^2 / 2g = 64 / 40 = 1.6 (discrete integration lands a little under).
    assert!((1.4..1.7).contains(&apex), "apex {apex}");
    // 2v/g = 0.8 s = 24 ticks.
    assert!((23..=26).contains(&ticks), "airtime {ticks}");
    assert_eq!(s.position.y, 0.0);
    assert_eq!(s.velocity.y, 0.0);
}

#[test]
fn ledges_block_and_small_steps_climb() {
    let m = motion();
    let g = terrain();
    // Walk +x toward the cliff at i = 50 (x = 18).
    let start_x = 15.0;
    let mut s = MotionState::at_rest(
        Vec3::new(start_x, g.height_at(start_x, 0.0).unwrap(), 0.0),
        Angle16(16_384),
    );
    for n in 0..120 {
        s = m.step(
            &g,
            &s,
            &input(MoveButtons::FORWARD, 16_384, n),
            &MotionModifiers::NONE,
            dt(),
        );
    }
    assert!(
        s.position.x < 18.0,
        "the 2-unit cliff blocks: x = {}",
        s.position.x
    );
    assert!(s.grounded);
    // The 0.3 bump at (30..34) is climbed.
    let mut s = MotionState::at_rest(
        Vec3::new(-5.0, g.height_at(-5.0, 0.0).unwrap(), 0.0),
        Angle16(16_384),
    );
    for n in 0..60 {
        s = m.step(
            &g,
            &s,
            &input(MoveButtons::FORWARD, 16_384, n),
            &MotionModifiers::NONE,
            dt(),
        );
    }
    assert!(s.position.x > 2.0);
    assert!(s.grounded);
    let h = g.height_at(s.position.x, s.position.z).unwrap();
    assert_eq!(s.position.y, h, "snapped to the ground");
}

#[test]
fn walking_off_a_ledge_falls_and_lands() {
    let m = motion();
    let g = terrain();
    // Start on top of the cliff, walk -x off it.
    let x = 19.5;
    let mut s = MotionState::at_rest(Vec3::new(x, g.height_at(x, 0.0).unwrap(), 0.0), Angle16(49_152));
    let mut airborne = false;
    for n in 0..60 {
        s = m.step(
            &g,
            &s,
            &input(MoveButtons::FORWARD, 49_152, n),
            &MotionModifiers::NONE,
            dt(),
        );
        airborne |= !s.grounded;
    }
    assert!(airborne, "left the ground at the cliff edge");
    assert!(s.grounded, "landed");
    assert!(s.position.x < 18.0);
}

#[test]
fn leaving_the_ground_model_is_refused() {
    let m = motion();
    let g = terrain(); // covers [-32, 31] on both axes
    let mut s = MotionState::at_rest(Vec3::new(0.0, g.height_at(0.0, 30.5).unwrap(), 30.5), Angle16(0));
    for n in 0..60 {
        s = m.step(
            &g,
            &s,
            &input(MoveButtons::FORWARD, 0, n),
            &MotionModifiers::NONE,
            dt(),
        );
    }
    assert!(s.position.z <= 31.0, "z = {}", s.position.z);
    assert!(g.height_at(s.position.x, s.position.z).is_some());
}

#[test]
fn bad_dt_changes_only_yaw() {
    let m = motion();
    let s = MotionState::at_rest(Vec3::new(1.0, 0.0, 1.0), Angle16(0));
    for bad in [0.0, -1.0, f32::NAN, f32::INFINITY] {
        let n = m.step(
            &FlatGround(0.0),
            &s,
            &input(MoveButtons::FORWARD, 77, 0),
            &MotionModifiers::NONE,
            bad,
        );
        assert!(n.position.bits_eq(s.position));
        assert_eq!(n.yaw, Angle16(77));
    }
}

/// The Validated-mode envelope rests on this: no input sequence, on any
/// terrain, moves a mover horizontally faster than `max_horizontal_speed`.
#[test]
fn horizontal_speed_never_exceeds_the_envelope() {
    let m = motion();
    let g = terrain();
    let mods = MotionModifiers {
        speed_scale: 1.3,
        ..MotionModifiers::NONE
    };
    let bound = m.max_horizontal_speed(&mods) * dt();
    let mut rng = Rng::for_cell(Seed(7), Tick(0), Salt::named("test.kinematics.envelope"));
    let mut s = MotionState::at_rest(Vec3::new(0.0, g.height_at(0.0, 0.0).unwrap(), 0.0), Angle16(0));
    for n in 0..5000u32 {
        let bits = u16::try_from(rng.below(64)).unwrap();
        let buttons = MoveButtons::from_bits(bits).unwrap();
        let yaw = u16::try_from(rng.below(65_536)).unwrap();
        let next = m.step(&g, &s, &input(buttons, yaw, n), &mods, dt());
        let moved = (next.position - s.position).horizontal().length();
        assert!(moved <= bound * 1.000_001, "tick {n}: moved {moved} > {bound}");
        s = next;
    }
}

/// Exact output bits for a fixed input script over the test terrain. These
/// are what the client's prediction and the server's integration must both
/// produce; the cross-architecture soak compares the same values.
#[test]
fn pinned_trajectory() {
    let m = motion();
    let g = terrain();
    let mut s = MotionState::at_rest(
        Vec3::new(-10.0, g.height_at(-10.0, -10.0).unwrap(), -10.0),
        Angle16(0),
    );
    let script = [
        (MoveButtons::FORWARD, 8_000u16, 40u32),
        (MoveButtons::FORWARD.with(MoveButtons::JUMP), 8_000, 5),
        (MoveButtons::STRAFE_RIGHT, 20_000, 30),
        (MoveButtons::BACKWARD.with(MoveButtons::WALK), 40_000, 25),
        (MoveButtons::NONE, 40_000, 20),
    ];
    let mut seq = 0;
    let mut checkpoints = Vec::new();
    for (buttons, yaw, ticks) in script {
        for _ in 0..ticks {
            s = m.step(&g, &s, &input(buttons, yaw, seq), &MotionModifiers::NONE, dt());
            seq += 1;
        }
        checkpoints.push((
            s.position.x.to_bits(),
            s.position.y.to_bits(),
            s.position.z.to_bits(),
            s.grounded,
        ));
    }
    assert_eq!(checkpoints, PINNED_CHECKPOINTS.to_vec());
    assert_eq!(s.stable_hash(), PINNED_FINAL_HASH);
}

// Cross-checked bit for bit against an independent f32 emulation of `step`
// (every operation as a correctly rounded IEEE f32 op).
const PINNED_CHECKPOINTS: [(u32, u32, u32, bool); 5] = [
    (0xC06E_CACA, 0x3FFD_E41E, 0xC05F_BE1A, true),
    (0xC03A_F9C0, 0x403E_F20F, 0xC029_FB7C, false),
    (0xBFE2_42E9, 0x4005_23B2, 0xC064_3384, true),
    (0xBF93_28D7, 0x4007_CF86, 0xC041_84E0, true),
    (0xBF93_28D7, 0x4007_CF86, 0xC041_84E0, true),
];
// Cross-checked: reference XXH64 of the documented MotionState encoding.
const PINNED_FINAL_HASH: u64 = 14_255_723_096_528_822_559;

#[test]
fn angle16_conversions() {
    assert_eq!(Angle16(0).to_radians(), 0.0);
    assert_eq!(Angle16(16_384).to_radians(), core::f32::consts::FRAC_PI_2);
    assert_eq!(Angle16(32_768).to_radians(), core::f32::consts::PI);
    for u in [0u16, 1, 100, 16_384, 32_767, 32_768, 65_535] {
        assert_eq!(
            Angle16::from_radians(Angle16(u).to_radians()),
            Angle16(u),
            "round trip {u}"
        );
    }
    assert_eq!(
        Angle16::from_radians(-core::f32::consts::FRAC_PI_2),
        Angle16(49_152)
    );
    assert_eq!(Angle16::from_radians(core::f32::consts::TAU * 3.0), Angle16(0));
    assert_eq!(Angle16::from_radians(f32::NAN), Angle16(0));
    assert_eq!(Angle16::from_radians(f32::INFINITY), Angle16(0));
    assert_eq!(Angle16::from_radians(1.0e30), Angle16::from_radians(1.0e30));
    assert_eq!(Angle16(10).delta(Angle16(65_530)), 16);
    assert_eq!(Angle16(65_530).delta(Angle16(10)), -16);
}

#[test]
fn input_seq_serial_order() {
    assert!(InputSeq(1).is_newer_than(InputSeq(0)));
    assert!(!InputSeq(0).is_newer_than(InputSeq(1)));
    assert!(!InputSeq(5).is_newer_than(InputSeq(5)));
    assert!(InputSeq(2).is_newer_than(InputSeq(u32::MAX - 2)), "wraps");
    assert!(!InputSeq(u32::MAX - 2).is_newer_than(InputSeq(2)));
    assert!(
        !InputSeq(0x8000_0000).is_newer_than(InputSeq(0)),
        "ambiguous half-range"
    );
    assert!(!InputSeq(0).is_newer_than(InputSeq(0x8000_0000)));
    assert_eq!(InputSeq(u32::MAX).next(), InputSeq(0));
}

#[test]
fn buttons_refuse_unknown_bits() {
    assert_eq!(MoveButtons::from_bits(0b11_1111), Some(MoveButtons::ALL));
    assert_eq!(MoveButtons::from_bits(0b100_0000), None);
    assert_eq!(MoveButtons::from_bits(0x8000), None);
    let b = MoveButtons::FORWARD.with(MoveButtons::JUMP);
    assert!(b.contains(MoveButtons::JUMP));
    assert!(!b.without(MoveButtons::JUMP).contains(MoveButtons::JUMP));
    assert_eq!(b.bits(), 0b1_0001);
}

#[test]
fn heightfield_interpolates_and_validates() {
    let hf = Heightfield::new(0.0, 0.0, 2.0, 2, 2, vec![0.0, 2.0, 4.0, 6.0]).unwrap();
    assert_eq!(hf.height_at(0.0, 0.0), Some(0.0));
    assert_eq!(hf.height_at(2.0, 0.0), Some(2.0));
    assert_eq!(hf.height_at(0.0, 2.0), Some(4.0));
    assert_eq!(hf.height_at(2.0, 2.0), Some(6.0));
    assert_eq!(hf.height_at(1.0, 1.0), Some(3.0));
    assert_eq!(hf.height_at(-0.001, 1.0), None);
    assert_eq!(hf.height_at(1.0, 2.001), None);
    assert_eq!(hf.height_at(f32::NAN, 1.0), None);
    assert_eq!((hf.width(), hf.depth()), (2, 2));
    assert_eq!(
        Heightfield::new(0.0, 0.0, 1.0, 1, 2, vec![0.0; 2]),
        Err(HeightfieldError::BadDimensions)
    );
    assert_eq!(
        Heightfield::new(0.0, 0.0, 1.0, 2, 2, vec![0.0; 3]),
        Err(HeightfieldError::SampleCountMismatch)
    );
    assert_eq!(
        Heightfield::new(0.0, 0.0, 0.0, 2, 2, vec![0.0; 4]),
        Err(HeightfieldError::BadSpacing)
    );
    assert_eq!(
        Heightfield::new(0.0, 0.0, 1.0, 2, 2, vec![0.0, f32::NAN, 0.0, 0.0]),
        Err(HeightfieldError::NonFiniteHeight)
    );
}

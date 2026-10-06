//! The resolved schedule is printed and compared against a checked-in golden
//! file (plan 6.2). Regenerate after an intentional change with
//! `MANTIS_BLESS=1 cargo test -p mantis-core --test schedule_golden` and
//! review the diff.

// Test code: helper functions may unwrap and cast freely; the library rules
// (no unwrap, checked casts) apply to library code.
#![allow(clippy::unwrap_used, clippy::too_many_lines)]

use std::path::PathBuf;

use mantis_core::ecs::{Access, Component, Resource, World};
use mantis_core::schedule::{Phase, Schedule, SystemDesc, SystemError, TickContext};

macro_rules! component {
    ($ty:ident, $name:literal) => {
        #[derive(Clone, Copy, Debug)]
        struct $ty(u32);
        mantis_core::impl_state_hash!($ty { 0 });
        impl Component for $ty {
            const NAME: &'static str = $name;
        }
    };
}

component!(Position, "core.position");
component!(Velocity, "core.velocity");
component!(Cooldown, "core.cooldown");
component!(Health, "core.health");
component!(Effect, "core.effect");
component!(Brain, "core.brain");

struct Inbox;
mantis_core::impl_state_hash!(Inbox {});
impl Resource for Inbox {
    const NAME: &'static str = "core.inbox";
}
struct LogWriter;
mantis_core::impl_state_hash!(LogWriter {});
impl Resource for LogWriter {
    const NAME: &'static str = "core.log_writer";
}

/// A representative module set, registered in a deliberately scrambled order;
/// the resolved order must not depend on it.
fn reference_schedule() -> (World, Schedule) {
    let mut w = World::new();
    w.register::<Position>().unwrap();
    w.register::<Velocity>().unwrap();
    w.register::<Cooldown>().unwrap();
    w.register::<Health>().unwrap();
    w.register::<Effect>().unwrap();
    w.register::<Brain>().unwrap();

    let mut specs: Vec<(&'static str, Phase, i32, Access)> = vec![
        (
            "persist.flush_segment",
            Phase::Persist,
            0,
            Access::builder(&mut w)
                .write_resource::<LogWriter>()
                .build()
                .unwrap(),
        ),
        (
            "inbound.apply_intents",
            Phase::Inbound,
            0,
            Access::builder(&mut w)
                .write::<Velocity>()
                .write_resource::<Inbox>()
                .build()
                .unwrap(),
        ),
        (
            "movement.integrate",
            Phase::Movement,
            0,
            Access::builder(&mut w)
                .write::<Position>()
                .read::<Velocity>()
                .build()
                .unwrap(),
        ),
        (
            "movement.ground_snap",
            Phase::Movement,
            10,
            Access::builder(&mut w).write::<Position>().build().unwrap(),
        ),
        (
            "timers.cooldowns",
            Phase::Timers,
            0,
            Access::builder(&mut w).write::<Cooldown>().build().unwrap(),
        ),
        (
            "combat.resolve_hits",
            Phase::Combat,
            0,
            Access::builder(&mut w)
                .read::<Position>()
                .write::<Health>()
                .read::<Cooldown>()
                .build()
                .unwrap(),
        ),
        (
            "effects.tick",
            Phase::Effects,
            0,
            Access::builder(&mut w)
                .write::<Effect>()
                .write::<Health>()
                .build()
                .unwrap(),
        ),
        (
            "effects.expire",
            Phase::Effects,
            0,
            Access::builder(&mut w).write::<Effect>().build().unwrap(),
        ),
        (
            "ai.think",
            Phase::Ai,
            0,
            Access::builder(&mut w)
                .write::<Brain>()
                .read::<Position>()
                .read::<Health>()
                .build()
                .unwrap(),
        ),
        ("scripts.step", Phase::Scripts, 0, Access::NONE),
        (
            "interest.spatial_hash",
            Phase::Interest,
            0,
            Access::builder(&mut w).read::<Position>().build().unwrap(),
        ),
        (
            "outbound.encode",
            Phase::Outbound,
            0,
            Access::builder(&mut w)
                .read::<Position>()
                .read::<Velocity>()
                .read::<Health>()
                .build()
                .unwrap(),
        ),
        (
            "persist.checkpoint",
            Phase::Persist,
            100,
            Access::builder(&mut w)
                .read_resource::<LogWriter>()
                .build()
                .unwrap(),
        ),
    ];
    // Scramble deterministically.
    specs.reverse();
    specs.rotate_left(5);

    let mut s = Schedule::new();
    for (name, phase, priority, access) in specs {
        s.add(
            SystemDesc {
                name,
                phase,
                priority,
                access,
            },
            |_: &mut World, _: &TickContext| -> Result<(), SystemError> { Ok(()) },
        )
        .unwrap();
    }
    (w, s)
}

#[test]
fn resolved_schedule_matches_golden() {
    let (w, s) = reference_schedule();
    let rendered = s.render(&w);
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/golden/schedule.txt");
    if std::env::var_os("MANTIS_BLESS").is_some() {
        std::fs::write(&path, &rendered).unwrap();
        return;
    }
    let golden = std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("missing {}: {e}; run with MANTIS_BLESS=1", path.display()));
    let golden = golden.replace("\r\n", "\n");
    assert_eq!(
        rendered, golden,
        "resolved schedule changed; review and re-bless if intended"
    );
}

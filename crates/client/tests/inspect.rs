//! The client half of the live inspector: the simulation thread copies its state into the
//! inspect slot after every tick, and the copy matches the simulation.

// The shared rig carries more than this test reads.
#![expect(dead_code)]

mod support;

use mantis_client::core_api::AvatarKinematics;
use mantis_client::input::device::KeyCode;
use mantis_client::inspect::{InspectSlot, client_sim_observer};
use support::{Rig, TestResult, ground};

#[test]
fn the_slot_mirrors_the_simulation_after_every_tick() -> TestResult {
    let mut rig = Rig::new(3, 3, ground())?;
    let slot = InspectSlot::new();
    assert_eq!(slot.read().tick, 0);
    rig.session
        .sim
        .set_observer(Some(client_sim_observer(slot.clone())));
    for k in 0..120u64 {
        if k == 10 {
            rig.key(KeyCode::W, true)?;
        }
        rig.run_tick(4);
        let seen = slot.read();
        let sim = rig.session.sim.handler();
        assert_eq!(seen.stats, sim.stats(), "tick {k}");
        assert_eq!(seen.local, sim.local());
        assert_eq!(seen.position, sim.predictor().state().position());
        assert_eq!(seen.remotes as usize, sim.remote_count());
    }
    let seen = slot.read();
    assert!(seen.tick >= 119, "{seen:?}");
    assert!(seen.local.is_some() && seen.stats.moves_sent > 90, "{seen:?}");
    assert!(seen.position.z > 1.0, "moved forward: {seen:?}");
    assert_eq!(seen.correction_p99.is_some(), seen.corrections > 0);
    // Detached, the slot keeps the last copy.
    rig.session.sim.set_observer(None);
    rig.run_tick(4);
    assert_eq!(slot.read().tick, seen.tick);
    Ok(())
}

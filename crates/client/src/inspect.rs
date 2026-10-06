//! The client half of the live inspector (plan 16, development builds): what the
//! simulation thread holds, copied once per tick into an [`InspectSlot`] the render
//! thread reads, plus the render thread's own view (the render world it was handed).
//!
//! The simulation is never touched from another thread: [`client_sim_observer`] runs on
//! the simulation thread after each tick (through [`SimDriver::set_observer`]) and copies
//! plain values under a short lock, allocation-free.
//!
//! [`SimDriver::set_observer`]: crate::threads::sim_thread::SimDriver::set_observer

use std::sync::{Arc, Mutex, PoisonError};

use mantis_core::ecs::EntityId;

use crate::core_api::{AvatarKinematics, MotionStep, Tick, Vec3};
use crate::render_world::RenderWorld;
use crate::sim::{ClientSim, ClientSimStats, IntentSink};
use crate::threads::sim_thread::TickObserver;

/// The simulation's state as of its last tick.
#[derive(Clone, Copy, PartialEq, Debug, Default)]
pub struct SimInspect {
    /// The last tick run.
    pub tick: u64,
    /// Counters.
    pub stats: ClientSimStats,
    /// The local avatar, once known.
    pub local: Option<EntityId>,
    /// Predicted position of the local avatar.
    pub position: Vec3,
    /// Predicted velocity.
    pub velocity: Vec3,
    /// Remote entities tracked.
    pub remotes: u32,
    /// Corrections recorded.
    pub corrections: u64,
    /// p99 correction magnitude (meters), when any were recorded.
    pub correction_p99: Option<f32>,
    /// Largest correction (meters).
    pub correction_max: f32,
}

/// Where the simulation thread leaves its latest [`SimInspect`] for the render thread.
#[derive(Clone, Debug, Default)]
pub struct InspectSlot(Arc<Mutex<SimInspect>>);

impl InspectSlot {
    /// An empty slot.
    pub fn new() -> Self {
        Self::default()
    }

    /// Writes the latest state.
    pub fn publish(&self, s: SimInspect) {
        *self.0.lock().unwrap_or_else(PoisonError::into_inner) = s;
    }

    /// The latest state.
    pub fn read(&self) -> SimInspect {
        *self.0.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// An observer for the simulation driver that copies a [`ClientSim`]'s state into `slot`
/// after every tick.
pub fn client_sim_observer<M: MotionStep, O: IntentSink>(slot: InspectSlot) -> TickObserver<ClientSim<M, O>> {
    Box::new(move |sim: &ClientSim<M, O>, tick: Tick| {
        let state = sim.predictor().state();
        let corrections = sim.corrections();
        slot.publish(SimInspect {
            tick: tick.0,
            stats: sim.stats(),
            local: sim.local(),
            position: state.position(),
            velocity: state.velocity(),
            remotes: u32::try_from(sim.remote_count()).unwrap_or(u32::MAX),
            corrections: corrections.total(),
            correction_p99: corrections.quantile(0.99),
            correction_max: corrections.max(),
        });
    })
}

/// One entity of the render world, as the inspector lists it.
#[derive(Clone, Copy, PartialEq, Debug)]
pub struct EntityRow {
    /// The entity.
    pub id: EntityId,
    /// Whether it is the local avatar.
    pub local: bool,
    /// Its newest position in the render world.
    pub position: Vec3,
}

/// Every entity the render world holds: the local avatar first, then remotes by id.
pub fn render_world_rows(world: &RenderWorld) -> Vec<EntityRow> {
    let mut rows: Vec<EntityRow> = world
        .local()
        .map(|l| EntityRow {
            id: l.id,
            local: true,
            position: l.position,
        })
        .into_iter()
        .collect();
    let mut remotes: Vec<EntityRow> = world
        .remotes()
        .iter()
        .filter_map(|r| {
            r.samples().last().map(|s| EntityRow {
                id: r.id(),
                local: false,
                position: s.position,
            })
        })
        .collect();
    remotes.sort_by_key(|r| r.id);
    rows.extend(remotes);
    rows
}

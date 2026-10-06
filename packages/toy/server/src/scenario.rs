//! Reference scenarios of the budget table (plan 17), as described in
//! `crates/testkit/scenarios/`.
//!
//! [`Crowd`] is `cell-500-100`: one toy cell with 500 avatars, 100 of them
//! with attached native clients, at the package tick rate. Every avatar
//! receives an input each tick (most run in slow circles, one in four
//! stands), and every client acknowledges each snapshot it is sent, as a
//! client on a clean link does. The package's modules are installed, so
//! their systems run every tick.

use mantis_adapter_contract::core_types::{
    AimAngles, Angle16, EntityId, InputSeq, MoveButtons, MoveInput, Tick,
};
use mantis_adapter_contract::{ConnectionId, MovementMode};
use mantis_core::log::SessionId;
use mantis_core::math::Vec3;
use mantis_server::cell::{Cell, CellError, OutboundSink, TickReport};
use mantis_server::components::ReplicationId;
use mantis_server::intent::CellIntent;
use mantis_server::jobs::WorkerSet;

use crate::tunables::Tunables;
use crate::world;

/// Avatars in `cell-500-100`.
pub const ENTITIES: u64 = 500;

/// Attached clients in `cell-500-100`.
pub const CLIENTS: u64 = 100;

/// The `cell-500-100` scenario.
pub struct Crowd {
    /// The cell under test.
    pub cell: Cell,
    entities: u64,
    clients: u64,
    seq: u32,
}

impl Crowd {
    /// `entities` avatars on a 4 m grid, the first `clients` of them with
    /// clients attached to the native adapter.
    ///
    /// # Errors
    /// [`CellError`].
    pub fn new(t: &Tunables, entities: u64, clients: u64, seed: u64) -> Result<Self, CellError> {
        let set = world::module_set(&std::collections::BTreeMap::new())
            .map_err(|_| CellError::Config("modules cannot start"))?;
        Self::with_modules(t, entities, clients, seed, &set)
    }

    /// [`Crowd::new`] with a given module set (the package's, plus extras
    /// such as scripts).
    ///
    /// # Errors
    /// [`CellError`].
    pub fn with_modules(
        t: &Tunables,
        entities: u64,
        clients: u64,
        seed: u64,
        set: &mantis_server::modules::ModuleSet,
    ) -> Result<Self, CellError> {
        let mut cfg = world::cell_config(t, 0, seed);
        // A lone cell: no neighbour to transfer to.
        cfg.region = None;
        cfg.max_entities = usize::try_from(entities).unwrap_or(usize::MAX).max(1) + 16;
        cfg.max_clients = usize::try_from(entities).unwrap_or(usize::MAX).max(1);
        let content = world::gameplay().map_err(|_| CellError::Config("gameplay content"))?;
        let mut cell = Cell::new(
            cfg,
            world::ground(),
            world::adapters(t.content),
            None,
            std::sync::Arc::clone(&content.catalog),
            content.abilities.clone(),
        )?;
        // The package's modules run too: their systems are part of the tick.
        cell.install_modules(set)
            .map_err(|_| CellError::Config("modules cannot start"))?;
        for i in 0..entities {
            let s = SessionId(i + 1);
            let col = u16::try_from(i % 25).map_or(0.0, f32::from);
            let row = u16::try_from(i / 25).map_or(0.0, f32::from);
            cell.inbox().push(
                s,
                CellIntent::Join {
                    repl: ReplicationId(EntityId::new(u32::try_from(i).unwrap_or(u32::MAX), 0)),
                    spawn: Vec3::new(-10.0 - col * 4.0, 0.0, row * 4.0),
                    yaw: Angle16(0),
                    look: world::CLASS_LOOK,
                    mode: MovementMode::Predictive,
                    epoch: 1,
                    character: i + 1,
                },
            );
            if i < clients {
                cell.attach_client(s, ConnectionId(i + 1), 0, false)?;
            }
        }
        Ok(Self {
            cell,
            entities,
            clients,
            seq: 0,
        })
    }

    /// The standard scenario.
    ///
    /// # Errors
    /// [`CellError`].
    pub fn cell_500_100(t: &Tunables, seed: u64) -> Result<Self, CellError> {
        Self::new(t, ENTITIES, CLIENTS, seed)
    }

    /// Attached clients.
    #[must_use]
    pub fn clients(&self) -> u64 {
        self.clients
    }

    /// Pushes this tick's inputs. Allocation-free.
    pub fn push_inputs(&mut self) {
        self.seq = self.seq.wrapping_add(1);
        let inbox = self.cell.inbox();
        for i in 0..self.entities {
            let idle = i % 4 == 3;
            let phase = u16::try_from(i.wrapping_mul(2_654_435_761) & 0xFFFF).unwrap_or(0);
            let turn = u16::try_from((u64::from(self.seq) * 600) & 0xFFFF).unwrap_or(0);
            inbox.push(
                SessionId(i + 1),
                CellIntent::Move(MoveInput {
                    seq: InputSeq(self.seq),
                    tick: Tick(u64::from(self.seq)),
                    buttons: if idle {
                        MoveButtons::NONE
                    } else {
                        MoveButtons::FORWARD
                    },
                    yaw: Angle16(phase.wrapping_add(turn)),
                    aim: AimAngles::default(),
                }),
            );
        }
    }

    /// Every client acknowledges `tick`. Allocation-free.
    pub fn ack_all(&self, tick: Tick) {
        let inbox = self.cell.inbox();
        for i in 0..self.clients {
            inbox.ack(SessionId(i + 1), tick);
        }
    }

    /// One full scenario tick: inputs, the cell tick, acknowledgements.
    ///
    /// # Errors
    /// [`CellError`].
    pub fn tick(
        &mut self,
        sink: &mut dyn OutboundSink,
        workers: Option<&WorkerSet>,
    ) -> Result<TickReport, CellError> {
        self.push_inputs();
        let r = self.cell.tick(sink, workers)?;
        self.ack_all(r.tick);
        Ok(r)
    }
}

//! Seamless worlds: adjacent cells, ghosts, and ownership transfer (plan 7.1).
//!
//! Cells own disjoint x ranges. After every tick:
//! - each entity that left its cell's range is **transferred**: the lease
//!   moves with a new epoch, the destination's inbox receives a
//!   `TransferIn` (it spawns the entity on its next tick), the source's inbox
//!   receives a `TransferAck` (it despawns it on its next tick), and an
//!   avatar's session and whole replication state move to the destination,
//!   so its client keeps every baseline and every known entity;
//! - each cell's border strip is **ghosted** to its neighbours as read-only
//!   replication entries. The transferring entity is ghosted in the source
//!   cell at once, so observers there never see a gap: on the next tick it
//!   is a ghost, and from then on the destination's own export.
//!
//! Every transfer goes through the inboxes, so each cell's log still replays
//! that cell exactly. The zone steps its cells in lockstep on one thread;
//! that is the reference behaviour the threaded host must match.

use std::collections::BTreeMap;

use mantis_core::log::SessionId;

use crate::cell::{Cell, CellError, OutboundSink, TickReport};
use crate::components::ReplicationId;
use crate::intent::CellIntent;
use crate::interest::Replicated;
use crate::jobs::WorkerSet;
use crate::lease::{CharacterId, LeaseTable};

/// A completed or refused transfer, for tests and the inspector.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct TransferEvent {
    /// The entity.
    pub repl: ReplicationId,
    /// From cell index.
    pub from: usize,
    /// To cell index.
    pub to: usize,
    /// True when the lease moved and the transfer was issued.
    pub issued: bool,
}

/// Several cells stepping together.
pub struct Zone {
    cells: Vec<Cell>,
    regions: Vec<(f32, f32)>,
    leases: LeaseTable,
    characters: BTreeMap<SessionId, CharacterId>,
    routes: BTreeMap<SessionId, usize>,
    /// Transfers issued after the last step.
    pub transfers: Vec<TransferEvent>,
    instances: BTreeMap<usize, Occupancy>,
    release_grace: u64,
    released: Vec<usize>,
}

/// An instance cell's occupancy (plan 7.1: an instance is released when it
/// has been empty for the package's grace).
#[derive(Clone, Copy, Debug, Default)]
struct Occupancy {
    occupied: bool,
    empty_ticks: u64,
}

impl Zone {
    /// A zone over `cells`; `regions[i]` is the x range of `cells[i]`.
    ///
    /// # Errors
    /// [`CellError::Config`] when the lengths differ.
    pub fn new(cells: Vec<Cell>, regions: Vec<(f32, f32)>) -> Result<Self, CellError> {
        if cells.len() != regions.len() || cells.is_empty() {
            return Err(CellError::Config("one region per cell"));
        }
        Ok(Self {
            cells,
            regions,
            leases: LeaseTable::default(),
            characters: BTreeMap::new(),
            routes: BTreeMap::new(),
            transfers: Vec::new(),
            instances: BTreeMap::new(),
            release_grace: 1,
            released: Vec::new(),
        })
    }

    /// The cell index owning x.
    #[must_use]
    pub fn cell_for(&self, x: f32) -> Option<usize> {
        self.regions.iter().position(|(lo, hi)| x >= *lo && x < *hi)
    }

    /// The cells.
    #[must_use]
    pub fn cells(&self) -> &[Cell] {
        &self.cells
    }

    /// One cell, mutably (to attach clients, push intents).
    pub fn cell_mut(&mut self, i: usize) -> Option<&mut Cell> {
        self.cells.get_mut(i)
    }

    /// The cell currently serving `session`.
    #[must_use]
    pub fn route(&self, session: SessionId) -> Option<usize> {
        self.routes.get(&session).copied()
    }

    /// The lease table.
    #[must_use]
    pub fn leases(&self) -> &LeaseTable {
        &self.leases
    }

    /// Registers a session's character in cell `i` (acquiring its lease) and
    /// returns the lease epoch to put in the `Join`.
    ///
    /// # Errors
    /// [`CellError::Config`] when the lease is held elsewhere or `i` is unknown.
    pub fn register(
        &mut self,
        session: SessionId,
        character: CharacterId,
        i: usize,
    ) -> Result<u64, CellError> {
        let cell = self.cells.get(i).ok_or(CellError::Config("no such cell"))?.id();
        let lease = self
            .leases
            .acquire(character, cell)
            .map_err(|_| CellError::Config("lease held elsewhere"))?;
        self.characters.insert(session, character);
        self.routes.insert(session, i);
        Ok(lease.epoch)
    }

    /// Marks `cells` (indices) as instance cells, released after
    /// `grace_ticks` consecutive ticks with no session once they had one.
    pub fn set_instances(&mut self, cells: &[usize], grace_ticks: u64) {
        self.instances = cells.iter().map(|i| (*i, Occupancy::default())).collect();
        self.release_grace = grace_ticks.max(1);
    }

    /// Instance cells (indices) released since the last call: empty for
    /// the grace after being used. Hand them back to the realm.
    pub fn take_released(&mut self) -> Vec<usize> {
        std::mem::take(&mut self.released)
    }

    fn track_instances(&mut self) {
        for (i, o) in &mut self.instances {
            let used = self.routes.values().any(|r| r == i);
            if used {
                *o = Occupancy {
                    occupied: true,
                    empty_ticks: 0,
                };
            } else if o.occupied {
                o.empty_ticks += 1;
                if o.empty_ticks >= self.release_grace {
                    *o = Occupancy::default();
                    self.released.push(*i);
                }
            }
        }
    }

    /// Steps every cell once, then exchanges transfers and ghosts.
    ///
    /// # Errors
    /// The first cell error.
    pub fn step(
        &mut self,
        sink: &mut dyn OutboundSink,
        workers: Option<&WorkerSet>,
    ) -> Result<Vec<TickReport>, CellError> {
        let mut reports = Vec::with_capacity(self.cells.len());
        for c in &mut self.cells {
            reports.push(c.tick(sink, workers)?);
        }
        self.exchange();
        self.track_instances();
        Ok(reports)
    }

    /// Tells every cell its clock slipped `ticks` behind wall time (a stall
    /// or an overrun the host re-anchored after): a logged intent each cell
    /// applies at its next tick ([`CellIntent::ClockSlip`]).
    pub fn clock_slipped(&self, ticks: u32) {
        if ticks == 0 {
            return;
        }
        for c in &self.cells {
            c.inbox().push(SessionId(0), CellIntent::ClockSlip { ticks });
        }
    }

    fn exchange(&mut self) {
        self.transfers.clear();
        // Transfers.
        let mut moves = Vec::new();
        for (from, cell) in self.cells.iter().enumerate() {
            for t in cell.transfers_out() {
                if let Some(to) = self.cell_for(t.body.position.x).filter(|to| *to != from) {
                    moves.push((from, to, *t));
                }
            }
        }
        let mut transferring: Vec<Vec<Replicated>> = vec![Vec::new(); self.cells.len()];
        for (from, to, t) in moves {
            let (Some(from_id), Some(to_id)) = (
                self.cells.get(from).map(Cell::id),
                self.cells.get(to).map(Cell::id),
            ) else {
                continue;
            };
            let session = t.session.map(SessionId);
            let issued = match session.and_then(|s| self.characters.get(&s).copied()) {
                Some(ch) => self
                    .leases
                    .transfer(ch, from_id, t.epoch.saturating_sub(1), to_id)
                    .is_ok(),
                None => true, // not a character: no lease
            };
            self.transfers.push(TransferEvent {
                repl: t.repl,
                from,
                to,
                issued,
            });
            if !issued {
                continue;
            }
            if let Some(dst) = self.cells.get(to) {
                dst.inbox()
                    .push(session.unwrap_or(SessionId(0)), CellIntent::TransferIn(t));
                // Inputs the source buffered ahead follow the avatar, after
                // it arrives, as intents the destination logs: a crossing
                // never drops the lead a session built.
                if let Some((s, src)) = session.zip(self.cells.get(from))
                    && let Some(cs) = src.session(s)
                {
                    for input in cs.inputs.iter() {
                        dst.inbox().push(s, CellIntent::Move(*input));
                    }
                }
            }
            if let Some(src) = self.cells.get(from) {
                src.inbox()
                    .push(session.unwrap_or(SessionId(0)), CellIntent::TransferAck(t.repl));
            }
            if let Some(s) = session {
                let rep = self.cells.get_mut(from).and_then(|c| c.take_client(s));
                if let (Some(rep), Some(dst)) = (rep, self.cells.get_mut(to)) {
                    let _ = dst.put_client(rep);
                }
                self.routes.insert(s, to);
            }
            if let Some(list) = transferring.get_mut(from) {
                list.push(Replicated {
                    key: 0,
                    repl: t.repl,
                    position: t.body.position,
                    velocity: t.body.velocity,
                    yaw: t.body.yaw,
                    look: t.look,
                });
            }
        }
        // Ghosts: every neighbour's border exports, plus entities just
        // transferred out of this cell.
        let exports: Vec<Vec<Replicated>> = self.cells.iter().map(|c| c.ghost_exports().to_vec()).collect();
        for (i, cell) in self.cells.iter_mut().enumerate() {
            let neighbours = exports
                .iter()
                .enumerate()
                .filter(|(j, _)| *j != i)
                .flat_map(|(_, list)| list.iter().copied());
            let own = transferring.get(i).into_iter().flatten().copied();
            cell.set_ghosts(neighbours.chain(own));
        }
    }
}

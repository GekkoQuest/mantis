//! Per-client replication (plan 7.4): interest with priority accumulation,
//! one delta snapshot per client per tick against the client's last
//! acknowledged baseline, and the reliable-over-unreliable `entered` and
//! `removed` lists. Runs inside a per-client job (decision 0008): reads only
//! the cell's [`RepView`] and writes only this client's state and buffer.
//! Allocation-free after construction.

use mantis_adapter_contract::core_types::{EntityId, Tick};
use mantis_adapter_contract::{
    AdapterError, ConnectionId, RemoteSample, SnapshotFrame, SnapshotHeader, SnapshotVisitor, WireAdapter,
};
use mantis_core::log::SessionId;
use mantis_core::mem::BoundedVec;

use crate::components::ReplicationId;
use crate::interest::{RepView, Tier, TierConfig};

/// Frames kept for delta baselines.
pub const FRAME_RING: usize = 16;

#[derive(Clone, Copy, Debug)]
struct Known {
    repl: ReplicationId,
    entered: Tick,
    priority: u32,
    tier: Tier,
    /// Index into this tick's view, when present.
    view_index: Option<usize>,
}

/// Capacities of one client's replication state.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct ClientCaps {
    /// Entities a client can know at once.
    pub known: usize,
    /// Pending removals.
    pub leaving: usize,
    /// Remote samples per frame.
    pub remotes: usize,
    /// Entered events per frame.
    pub entered: usize,
    /// Markers per frame.
    pub markers: usize,
    /// Encode buffer bytes.
    pub out: usize,
    /// View entries (a cell's entities plus ghosts) covered by the lookup
    /// hint table; entries beyond it fall back to a binary search.
    pub view: usize,
}

impl ClientCaps {
    /// Defaults sized for the reference scenarios.
    pub const DEFAULT: Self = Self {
        known: 512,
        leaving: 128,
        remotes: 128,
        entered: 32,
        markers: 32,
        out: 4096,
        view: 4096,
    };
}

/// Replication counters for one client.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct RepStats {
    /// Snapshots encoded.
    pub frames: u64,
    /// Encoded bytes.
    pub bytes: u64,
    /// Frames shrunk to fit the payload limit.
    pub shrunk: u64,
    /// Items refused for capacity.
    pub overflow: u64,
    /// Encode failures.
    pub errors: u64,
}

/// One client's replication state.
pub struct ClientRep {
    /// The session.
    pub session: SessionId,
    /// Its connection.
    pub conn: ConnectionId,
    /// Index of its adapter in the cell's adapter table.
    pub adapter: usize,
    /// Reliable transports acknowledge every snapshot implicitly.
    pub implicit_ack: bool,
    /// Header facts for this tick, written by the cell thread before jobs.
    pub header: SnapshotHeader,
    /// The client's avatar.
    pub avatar: Option<ReplicationId>,
    known: BoundedVec<Known>,
    leaving: BoundedVec<(ReplicationId, Tick)>,
    selection: BoundedVec<(u32, u16)>,
    /// For view entry `i`, where it was in `known` after the last scan (a
    /// hint, checked before use: view order is stable while nothing spawns
    /// or crosses a grid cell).
    hint: Vec<u16>,
    acked: Option<Tick>,
    frames: Vec<SnapshotFrame>,
    next_slot: usize,
    /// The encoded snapshot of this tick.
    pub out: Vec<u8>,
    /// How long this tick's encode took, in nanoseconds (0 untimed).
    pub encode_nanos: u64,
    caps: ClientCaps,
    /// Counters.
    pub stats: RepStats,
}

impl ClientRep {
    /// Replication state for a new client. The only allocating call.
    #[must_use]
    pub fn new(
        session: SessionId,
        conn: ConnectionId,
        adapter: usize,
        implicit_ack: bool,
        caps: ClientCaps,
    ) -> Self {
        Self {
            session,
            conn,
            adapter,
            implicit_ack,
            header: SnapshotHeader::default(),
            avatar: None,
            known: BoundedVec::with_capacity(caps.known),
            leaving: BoundedVec::with_capacity(caps.leaving),
            selection: BoundedVec::with_capacity(caps.known),
            hint: vec![u16::MAX; caps.view],
            acked: None,
            encode_nanos: 0,
            frames: (0..FRAME_RING)
                .map(|_| SnapshotFrame::with_capacity(caps.entered, caps.remotes, caps.leaving, caps.markers))
                .collect(),
            next_slot: 0,
            out: Vec::with_capacity(caps.out),
            caps,
            stats: RepStats::default(),
        }
    }

    /// The newest acknowledged snapshot tick.
    #[must_use]
    pub fn acked(&self) -> Option<Tick> {
        self.acked
    }

    /// Records an acknowledgement. Ignored unless it names a frame still held
    /// and newer than the current baseline.
    pub fn on_ack(&mut self, tick: Tick) {
        let held = self
            .frames
            .iter()
            .any(|f| f.header.server_tick == tick && f.header.server_tick != Tick::ZERO);
        if held && self.acked.is_none_or(|a| tick > a) {
            self.acked = Some(tick);
        }
    }

    /// Number of entities this client currently knows.
    #[must_use]
    pub fn known_len(&self) -> usize {
        self.known.len()
    }

    fn update_interest(&mut self, view: &RepView, tiers: &TierConfig) {
        for k in self.known.iter_mut() {
            k.view_index = None;
        }
        let Some(center) = self.header.local.map(|l| l.state.position) else {
            return;
        };
        let tick = view.tick;
        let own = self.avatar;
        let mut overflow = 0u64;
        let known = &mut self.known;
        let hint = &self.hint;
        // Entries pushed during the scan are unsorted: search only the sorted
        // prefix (each view entry is visited once, so new ids never repeat).
        let sorted = known.len();
        view.for_each_within(center, tiers.far + tiers.hysteresis, |i, e, d| {
            if Some(e.repl) == own {
                return;
            }
            let guess = hint.get(i).map_or(usize::MAX, |h| usize::from(*h));
            let found = if guess < sorted && known.get(guess).is_some_and(|k| k.repl == e.repl) {
                Ok(guess)
            } else {
                known
                    .get(..sorted)
                    .unwrap_or(&[])
                    .binary_search_by_key(&e.repl, |k| k.repl)
            };
            match found {
                Ok(at) => {
                    if let Some(k) = known.get_mut(at) {
                        let tier = tiers.tier(d).unwrap_or(Tier::Far);
                        k.tier = tier;
                        k.priority = k.priority.saturating_add(tiers.weight(tier));
                        k.view_index = Some(i);
                    }
                }
                Err(_) => {
                    // New entities enter only inside the far radius (hysteresis).
                    if let Some(tier) = tiers.tier(d) {
                        let entry = Known {
                            repl: e.repl,
                            entered: tick,
                            priority: u32::MAX / 2,
                            tier,
                            view_index: Some(i),
                        };
                        if known.push(entry).is_err() {
                            overflow += 1;
                        }
                    }
                }
            }
        });
        self.known.sort_unstable_by_key(|k| k.repl);
        // Entities no longer in range (or despawned) leave interest.
        let mut i = 0;
        while i < self.known.len() {
            if self.known.get(i).is_some_and(|k| k.view_index.is_none()) {
                if let Some(k) = self.known.remove(i)
                    && self.leaving.push((k.repl, tick)).is_err()
                {
                    overflow += 1;
                }
            } else {
                i += 1;
            }
        }
        // Removals acknowledged by the client are done.
        let acked = self.acked;
        self.leaving.retain(|(_, t)| acked.is_none_or(|a| *t > a));
        self.stats.overflow += overflow;
        // Hints for the next scan.
        for (at, k) in self.known.iter().enumerate() {
            if let Some(slot) = k.view_index.and_then(|vi| self.hint.get_mut(vi)) {
                *slot = u16::try_from(at).unwrap_or(u16::MAX);
            }
        }
    }

    fn fill_frame(&mut self, slot: usize, view: &RepView, budget: usize) {
        let Some(frame) = self.frames.get_mut(slot) else {
            return;
        };
        frame.clear();
        frame.header(&self.header);
        let acked = self.acked;
        let unacked = |t: Tick| acked.is_none_or(|a| t > a);
        // Entered: repeated until acknowledged.
        for k in self.known.iter() {
            if unacked(k.entered)
                && let Some(e) = k.view_index.and_then(|i| view.entries.get(i))
            {
                frame.entered(k.repl.0, e.look);
            }
        }
        // Remotes: unacknowledged entries always, then the highest priorities.
        self.selection.clear();
        for (i, k) in self.known.iter().enumerate() {
            if k.view_index.is_some() {
                let p = if unacked(k.entered) { u32::MAX } else { k.priority };
                let _ = self.selection.push((p, u16::try_from(i).unwrap_or(u16::MAX)));
            }
        }
        let take = budget.min(self.selection.len());
        if take > 0 && take < self.selection.len() {
            self.selection
                .select_nth_unstable_by(take - 1, |a, b| b.0.cmp(&a.0).then(a.1.cmp(&b.1)));
        }
        for &(_, ki) in self.selection.iter().take(take) {
            let Some(k) = self.known.get_mut(usize::from(ki)) else {
                continue;
            };
            let Some(e) = k.view_index.and_then(|i| view.entries.get(i)) else {
                continue;
            };
            frame.remote(&RemoteSample {
                id: k.repl.0,
                tick: view.tick,
                position: e.position,
                velocity: e.velocity,
                yaw: e.yaw,
            });
            k.priority = 0;
        }
        for (repl, _) in self.leaving.iter() {
            frame.removed(repl.0);
        }
        // Markers that concern this client: its avatar or a known entity.
        let knows = |id: EntityId| {
            self.avatar.is_some_and(|a| a.0 == id)
                || self
                    .known
                    .binary_search_by_key(&ReplicationId(id), |k| k.repl)
                    .is_ok()
        };
        for m in view.markers.iter() {
            if knows(m.source) || m.target.is_some_and(knows) {
                frame.marker(m);
            }
        }
    }

    /// Runs interest, builds this tick's snapshot, and encodes it into
    /// [`ClientRep::out`], shrinking the remote budget until the encoding fits
    /// `max_payload`.
    ///
    /// # Errors
    /// [`AdapterError`] from the adapter's encoder.
    pub fn run(
        &mut self,
        view: &RepView,
        adapter: &dyn WireAdapter,
        tiers: &TierConfig,
        max_payload: usize,
    ) -> Result<(), AdapterError> {
        self.update_interest(view, tiers);
        let slot = self.next_slot;
        self.next_slot = (self.next_slot + 1) % FRAME_RING;
        let base_slot = self
            .acked
            .and_then(|a| self.frames.iter().position(|f| f.header.server_tick == a));
        let mut budget = tiers.budget.min(self.caps.remotes);
        loop {
            self.fill_frame(slot, view, budget);
            self.out.clear();
            let (frame, baseline) = match base_slot {
                Some(b) if b != slot => match self.frames.get_disjoint_mut([slot, b]) {
                    Ok([f, b]) => (&*f, Some(&*b)),
                    Err(_) => return Err(AdapterError::Unsupported("baseline slot")),
                },
                _ => (
                    self.frames
                        .get(slot)
                        .ok_or(AdapterError::Unsupported("frame slot"))?,
                    None,
                ),
            };
            let result = adapter.encode_snapshot(frame, baseline, &mut self.out);
            self.stats.overflow += u64::from(frame.overflowed);
            if let Err(e) = result {
                self.stats.errors += 1;
                return Err(e);
            }
            if self.out.len() <= max_payload || budget == 0 {
                break;
            }
            budget -= budget.div_ceil(4);
            self.stats.shrunk += 1;
        }
        self.stats.frames += 1;
        self.stats.bytes += self.out.len() as u64;
        if self.implicit_ack {
            self.acked = Some(view.tick);
            let acked = self.acked;
            self.leaving.retain(|(_, t)| acked.is_none_or(|a| *t > a));
        }
        Ok(())
    }
}

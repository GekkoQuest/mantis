//! Per-client replication (plan 7.4): interest with priority accumulation,
//! one delta snapshot per client per tick against the client's last
//! acknowledged baseline, and the reliable-over-unreliable `entered` and
//! `removed` lists. With `TierConfig::snapshot_own_bases` on (off by
//! default), a remote the baseline lacks (the budget rotated it out) deltas
//! against the newest other frame the client acknowledged that carries it,
//! within `own_base_max_age` ticks (`MASK_OWN_BASE`, per-remote baselines).
//! Runs inside a per-client job (decision 0008): reads only the cell's
//! [`RepView`] and writes only this client's state and buffer.
//! Allocation-free after construction.

use mantis_adapter_contract::core_types::{EntityId, Tick};
use mantis_adapter_contract::{
    AdapterError, ConnectionId, RemoteBases, RemoteSample, SnapshotFrame, SnapshotHeader, SnapshotVisitor,
    WireAdapter,
};
use mantis_core::log::SessionId;
use mantis_core::mem::BoundedVec;

use crate::components::ReplicationId;
use crate::interest::{RepView, Tier, TierConfig};

/// Frames kept for delta baselines.
pub const FRAME_RING: usize = 16;

/// Sends of one entity remembered for per-remote baselines.
const SENT_HISTORY: usize = 4;

/// Where an entity was sent: the frame's tick, its ring slot, and the
/// sample's position in the frame. `tick` zero is empty.
#[derive(Clone, Copy, Debug, Default)]
struct Sent {
    tick: Tick,
    pos: u16,
    slot: u8,
}

/// One entity's recent sends, in a direct-mapped table keyed by id: a
/// collision forgets the older entity's history (it is then sent in full or
/// against the frame-level baseline), never misattributes it, because a use
/// checks the frame's tick and the sample's id.
#[derive(Clone, Copy, Debug, Default)]
struct History {
    id: Option<EntityId>,
    /// Newest first.
    sent: [Sent; SENT_HISTORY],
}

fn history_slot(id: EntityId, len: usize) -> usize {
    // `len` is a power of two.
    let h = id.to_bits().wrapping_mul(0x9E37_79B9_7F4A_7C15);
    usize::try_from(h >> 32).unwrap_or(0) & len.wrapping_sub(1)
}

/// Per-remote baselines from the ring: for a remote, the newest frame the
/// client acknowledged that carries it, other than the frame being encoded
/// (which has a newer tick than any recorded send). The encoder applies the
/// codec's window; `oldest` applies `own_base_max_age`.
struct RingBases<'a> {
    frames: &'a [SnapshotFrame],
    history: &'a [History],
    acked: &'a [bool; FRAME_RING],
    /// The oldest tick a base may have.
    oldest: Tick,
}

impl RemoteBases for RingBases<'_> {
    fn base_for(&self, id: EntityId) -> Option<(Tick, &RemoteSample)> {
        let h = self
            .history
            .get(history_slot(id, self.history.len()))
            .filter(|h| h.id == Some(id))?;
        h.sent.iter().find_map(|s| {
            let slot = usize::from(s.slot);
            if s.tick == Tick::ZERO || s.tick < self.oldest || !self.acked.get(slot).copied().unwrap_or(false)
            {
                return None;
            }
            let f = self.frames.get(slot).filter(|f| f.header.server_tick == s.tick)?;
            let r = f.remotes.get(usize::from(s.pos)).filter(|r| r.id == id)?;
            Some((s.tick, r))
        })
    }
}

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
    /// Entries of the per-remote baseline table (rounded up to a power of
    /// two); 0 when per-remote baselines are off, so they cost nothing.
    pub own_base_history: usize,
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
        own_base_history: 0,
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
    /// Per slot: the client acknowledged that frame, so it holds it and an
    /// encode may delta a remote against it. Cleared when the slot is reused.
    slot_acked: [bool; FRAME_RING],
    /// Recent sends per entity, for per-remote baselines (empty when off).
    history: Vec<History>,
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
            slot_acked: [false; FRAME_RING],
            history: vec![
                History::default();
                if caps.own_base_history == 0 {
                    0
                } else {
                    caps.own_base_history.next_power_of_two()
                }
            ],
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

    /// Records an acknowledgement. Ignored unless it names a frame still
    /// held. The newest becomes the frame-level baseline; an older one (acks
    /// arrive out of order) still makes its frame a per-remote baseline.
    pub fn on_ack(&mut self, tick: Tick) {
        if tick == Tick::ZERO {
            return;
        }
        let Some(slot) = self.frames.iter().position(|f| f.header.server_tick == tick) else {
            return;
        };
        if let Some(flag) = self.slot_acked.get_mut(slot) {
            *flag = true;
        }
        if self.acked.is_none_or(|a| tick > a) {
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
        if let Some(flag) = self.slot_acked.get_mut(slot) {
            *flag = false;
        }
        let base_slot = self
            .acked
            .and_then(|a| self.frames.iter().position(|f| f.header.server_tick == a));
        let mut budget = tiers.budget.min(self.caps.remotes);
        loop {
            self.fill_frame(slot, view, budget);
            self.out.clear();
            let frame = self
                .frames
                .get(slot)
                .ok_or(AdapterError::Unsupported("frame slot"))?;
            let baseline = base_slot.filter(|b| *b != slot).and_then(|b| self.frames.get(b));
            let own_bases = RingBases {
                frames: &self.frames,
                history: &self.history,
                acked: &self.slot_acked,
                oldest: Tick(view.tick.0.saturating_sub(tiers.own_base_max_age)),
            };
            let result = if tiers.snapshot_own_bases && !self.history.is_empty() {
                adapter.encode_snapshot_based(frame, baseline, &own_bases, &mut self.out)
            } else {
                adapter.encode_snapshot(frame, baseline, &mut self.out)
            };
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
        // Remember where each remote went, for later per-remote baselines.
        if tiers.snapshot_own_bases && !self.history.is_empty() {
            let len = self.history.len();
            let slot_u8 = u8::try_from(slot).unwrap_or(u8::MAX);
            let frame = self.frames.get(slot).map_or(&[][..], |f| &f.remotes[..]);
            for (pos, r) in frame.iter().enumerate() {
                let Some(h) = self.history.get_mut(history_slot(r.id, len)) else {
                    continue;
                };
                if h.id != Some(r.id) {
                    *h = History {
                        id: Some(r.id),
                        sent: [Sent::default(); SENT_HISTORY],
                    };
                }
                h.sent.rotate_right(1);
                if let Some(newest) = h.sent.first_mut() {
                    *newest = Sent {
                        tick: view.tick,
                        pos: u16::try_from(pos).unwrap_or(u16::MAX),
                        slot: slot_u8,
                    };
                }
            }
        }
        self.stats.frames += 1;
        self.stats.bytes += self.out.len() as u64;
        if self.implicit_ack {
            if let Some(flag) = self.slot_acked.get_mut(slot) {
                *flag = true;
            }
            self.acked = Some(view.tick);
            let acked = self.acked;
            self.leaving.retain(|(_, t)| acked.is_none_or(|a| *t > a));
        }
        Ok(())
    }
}

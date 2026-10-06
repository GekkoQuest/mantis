//! Social: parties, friends, guilds, and cross-cell chat. Cells hold
//! read-only projections: a cell tells social which characters it hosts
//! and polls the changes for them (lines said to them, updates of their
//! party, friends, and guild), in sequence order.
//!
//! Parties, friends, and guilds are owned here (lead ruling, M8), with the
//! engine's rules in `mantis_core::social`: cells relay their modules'
//! operations, and every character's cell is sent the updates for it as
//! projections. A character arriving in a cell is sent its roster, lists,
//! and guild, so the projection follows it across transfers.
//!
//! Parties and friends are in memory: a restarted role starts a new epoch
//! and rebuilds them from what cells offer back (their projections).
//! Guilds are durable: every guild change is written through the
//! persistence writer before anyone is told of it, and a restarted role
//! reads the rows back ([`SocialService::load_guilds`]).

use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use mantis_core::social::{
    FRIEND_OP, FRIEND_UPDATE, FriendBook, FriendOp, FriendUpdate, GUILD_OP, GUILD_UPDATE, GuildBook,
    GuildChange, GuildOp, GuildOutcome, GuildUpdate, PARTY_OP, PARTY_UPDATE, PartyBook, PartyOp,
};
use mantis_core::wire::{BoundedArray, Wire, WireString, decode_exact};

use crate::generated::services as m;
use crate::host::RPC_TIMEOUT;
use crate::host::rpc::{Router, RpcClient, RpcError};
use crate::host::{now_ms, refused};
use crate::methods;
use crate::persist::{guild_change, guild_row};

/// The longest wait between attempts to make a guild change durable.
pub const DURABLE_RETRY_MAX: Duration = Duration::from_secs(1);

/// Channels social carries.
pub const GUILD: u8 = 2;
/// One character, anywhere.
pub const WHISPER: u8 = 3;
/// Everyone online.
pub const WORLD: u8 = 4;

/// Updates kept per cell before the oldest are dropped.
pub const QUEUE: usize = 4096;

#[derive(Default)]
struct State {
    epoch: u64,
    parties: PartyBook,
    friends: FriendBook,
    /// Per cell: the next projection sequence number, and the queue.
    projections: BTreeMap<u64, (u64, Vec<m::Projected>)>,
    /// Per cell: the last relayed operation applied.
    relayed: BTreeMap<u64, u64>,
    /// Cells whose projection this run has received (after a restart, a
    /// cell's operations wait until its host restored it).
    restored: BTreeSet<u64>,
    seq: u64,
    presence: BTreeMap<u64, u64>,
    cells: BTreeMap<u64, BTreeSet<u64>>,
    queues: BTreeMap<u64, Vec<m::SocialUpdate>>,
    guilds: GuildBook,
    /// The last guild batch made durable.
    guild_seq: u64,
}

impl State {
    /// Queues one update for the cell hosting its character (dropped when
    /// the character is in no cell: it gets a fresh view on arrival).
    fn project(&mut self, to: u64, topic: u16, update: &impl Wire) {
        let Some(cell) = self.presence.get(&to).copied() else {
            return;
        };
        self.project_to(cell, topic, update);
    }

    fn project_to(&mut self, cell: u64, topic: u16, update: &impl Wire) {
        let mut bytes = Vec::new();
        mantis_core::wire::encode_into(update, &mut bytes);
        let Some(payload) = BoundedArray::from_slice(&bytes) else {
            return;
        };
        let (next, q) = self.projections.entry(cell).or_insert((0, Vec::new()));
        *next += 1;
        if q.len() >= QUEUE {
            q.remove(0);
        }
        q.push(m::Projected {
            seq: *next,
            topic,
            payload,
        });
    }

    /// Everything `character` should see on arriving in `cell`.
    fn arrived(&mut self, cell: u64, character: u64) {
        if let Some(r) = self.parties.roster_for(character) {
            self.project_to(cell, PARTY_UPDATE, &r);
        }
        let views = self.friends.snapshot(character);
        let has_list = matches!(&views[0], FriendUpdate::List { friends, .. } if !friends.is_empty());
        let has_pending = matches!(&views[1], FriendUpdate::Pending { incoming, outgoing, .. } if !incoming.is_empty() || !outgoing.is_empty());
        if has_list || has_pending {
            for v in &views {
                self.project_to(cell, FRIEND_UPDATE, v);
            }
        }
        for u in self.guilds.arrived(character) {
            self.project_to(cell, GUILD_UPDATE, &u);
        }
    }

    fn project_guild(&mut self, updates: &[GuildUpdate]) {
        for u in updates {
            self.project(u.to(), GUILD_UPDATE, u);
        }
    }

    fn relay(&mut self, topic: u16, payload: &[u8], now_ms: u64) -> Result<(), RpcError> {
        match topic {
            PARTY_OP => {
                let op: PartyOp = decode_exact(payload).map_err(|_| RpcError::Malformed)?;
                for u in self.parties.apply(&op, now_ms) {
                    self.project(u.to(), PARTY_UPDATE, &u);
                }
            }
            FRIEND_OP => {
                let op: FriendOp = decode_exact(payload).map_err(|_| RpcError::Malformed)?;
                for u in self.friends.apply(&op) {
                    self.project(u.to(), FRIEND_UPDATE, &u);
                }
            }
            _ => return Err(refused("no such topic")),
        }
        Ok(())
    }

    /// Applies a guild operation; its updates wait until its changes are
    /// durable.
    fn relay_guild(&mut self, payload: &[u8], now_ms: u64) -> Result<GuildOutcome, RpcError> {
        let op: GuildOp = decode_exact(payload).map_err(|_| RpcError::Malformed)?;
        Ok(self.guilds.apply(&op, now_ms))
    }

    /// Queues a chat line for the cell hosting `to` (dropped when `to` is
    /// in no cell).
    fn deliver(&mut self, to: u64, from: u64, channel: u8, text: &str) {
        let Some(cell) = self.presence.get(&to).copied() else {
            return;
        };
        self.seq += 1;
        let q = self.queues.entry(cell).or_default();
        if q.len() >= QUEUE {
            q.remove(0);
        }
        q.push(m::SocialUpdate {
            seq: self.seq,
            to: m::CharacterId(to),
            kind: 1,
            from: m::CharacterId(from),
            channel,
            text: WireString::new(text).unwrap_or_default(),
        });
    }
}

/// The social role.
#[derive(Clone, Default)]
pub struct SocialService {
    state: Arc<Mutex<State>>,
    /// The persistence writer guild rows go through (none: guilds live in
    /// memory only, for tests of the rules alone).
    writer: Option<Arc<RpcClient>>,
    /// One operation at a time from apply to projection, so updates leave
    /// in the order their changes became durable.
    order: Arc<tokio::sync::Mutex<()>>,
}

impl SocialService {
    /// No guilds, nobody online, no writer (guilds in memory only). Its
    /// epoch is the start time, so a restarted role is a new epoch.
    #[must_use]
    pub fn new() -> Self {
        let s = Self::default();
        s.lock().epoch = now_ms().max(1);
        s
    }

    /// [`SocialService::new`] writing guild rows through `writer`. Call
    /// [`SocialService::load_guilds`] before serving.
    #[must_use]
    pub fn with_writer(writer: RpcClient) -> Self {
        let mut s = Self::new();
        s.writer = Some(Arc::new(writer));
        s
    }

    /// Reads every durable guild row back from the writer (after a
    /// restart, before serving). Invitations are not durable and lapse.
    ///
    /// # Errors
    /// The writer's refusal or a malformed row: the role must not serve.
    pub async fn load_guilds(&self) -> Result<usize, String> {
        let Some(writer) = &self.writer else {
            return Ok(0);
        };
        let mut rows = Vec::new();
        let mut seq = 0;
        for page in 0.. {
            let got = writer
                .call::<methods::LoadGuilds>(&m::ReadGuildRows { page }, RPC_TIMEOUT)
                .await
                .map_err(|e| format!("loading guilds: {e}"))?;
            seq = got.seq;
            for r in got.rows.iter() {
                rows.push(guild_change(r).ok_or("loading guilds: a malformed row")?);
            }
            if !got.more {
                break;
            }
        }
        let mut s = self.lock();
        s.guilds = GuildBook::from_rows(&rows);
        s.guild_seq = seq;
        Ok(s.guilds.len())
    }

    /// The guild `character` is in (Ops and tests).
    #[must_use]
    pub fn guild_of(&self, character: u64) -> Option<u32> {
        self.lock().guilds.guild_of(character)
    }

    /// `character`'s guild rank (Ops and tests).
    #[must_use]
    pub fn guild_rank(&self, character: u64) -> Option<u8> {
        self.lock().guilds.rank_of(character)
    }

    /// Makes `changes` durable through the writer (retrying the same batch
    /// until it is), then returns. Without a writer, returns at once.
    async fn durable(&self, changes: &[GuildChange]) {
        let Some(writer) = &self.writer else {
            return;
        };
        if changes.is_empty() {
            return;
        }
        let seq = {
            let mut s = self.lock();
            s.guild_seq += 1;
            s.guild_seq
        };
        let rows: Vec<m::GuildRow> = changes.iter().map(guild_row).collect();
        let req = m::StoreGuildRows {
            seq,
            rows: BoundedArray::from_slice(&rows).unwrap_or_default(),
        };
        let mut delay = Duration::from_millis(20);
        while writer
            .call::<methods::WriteGuilds>(&req, RPC_TIMEOUT)
            .await
            .is_err()
        {
            tokio::time::sleep(delay).await;
            delay = (delay * 2).min(DURABLE_RETRY_MAX);
        }
    }

    /// Applies a guild outcome: durable first, then told.
    async fn settle(&self, outcome: GuildOutcome) {
        self.durable(&outcome.changes).await;
        self.lock().project_guild(&outcome.updates);
    }

    /// This run's epoch.
    #[must_use]
    pub fn epoch(&self) -> u64 {
        self.lock().epoch
    }

    /// The party `character` is in (Ops and tests).
    #[must_use]
    pub fn party_of(&self, character: u64) -> Option<u32> {
        self.lock().parties.party_of(character)
    }

    /// True when `a` and `b` are friends (Ops and tests).
    #[must_use]
    pub fn are_friends(&self, a: u64, b: u64) -> bool {
        self.lock().friends.are_friends(a, b)
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        crate::host::lock(&self.state)
    }

    fn publish(&self, req: &m::Publish) -> Result<m::Empty, RpcError> {
        let mut s = self.lock();
        let from = req.from.0;
        let text = req.text.as_str();
        let recipients: Vec<u64> = match req.channel {
            GUILD => {
                let guild = u32::try_from(req.to).map_err(|_| refused("no such guild"))?;
                if s.guilds.name_of(guild).is_none() {
                    return Err(refused("no such guild"));
                }
                if s.guilds.guild_of(from) != Some(guild) {
                    return Err(refused("not a member"));
                }
                s.guilds.members(guild)
            }
            WHISPER => {
                if !s.presence.contains_key(&req.to) {
                    return Err(refused("not online"));
                }
                vec![req.to, from]
            }
            _ => s.presence.keys().copied().collect(),
        };
        for to in recipients {
            s.deliver(to, from, req.channel, text);
        }
        Ok(m::Empty {})
    }

    /// One relayed operation: applied once, or held back until the cell's
    /// projection was restored into this run. A guild operation's updates
    /// are sent once its changes are durable, and only then acknowledged.
    async fn relay_op(&self, req: &m::Relay) -> Result<m::RelayAck, RpcError> {
        let _order = self.order.lock().await;
        let guild = {
            let mut s = self.lock();
            let payload: Vec<u8> = req.payload.iter().copied().collect();
            if req.restore {
                s.relay(req.topic, &payload, now_ms())?;
                return Ok(m::RelayAck {
                    applied: true,
                    needs_restore: false,
                });
            }
            if !s.restored.contains(&req.cell.0) {
                return Ok(m::RelayAck {
                    applied: false,
                    needs_restore: true,
                });
            }
            let last = s.relayed.get(&req.cell.0).copied().unwrap_or(0);
            if req.seq != 0 && req.seq <= last {
                return Ok(m::RelayAck {
                    applied: false,
                    needs_restore: false,
                });
            }
            let guild = if req.topic == GUILD_OP {
                Some(s.relay_guild(&payload, now_ms())?)
            } else {
                s.relay(req.topic, &payload, now_ms())?;
                None
            };
            s.relayed.insert(req.cell.0, req.seq);
            guild
        };
        if let Some(outcome) = guild {
            self.settle(outcome).await;
        }
        Ok(m::RelayAck {
            applied: true,
            needs_restore: false,
        })
    }

    /// The role's RPC methods.
    #[must_use]
    pub fn router(&self) -> Router {
        let mut r = Router::validated(methods::validate);
        let me = self.clone();
        r.serve::<methods::PublishLine>(move |_, req| me.publish(&req));
        let me = self.clone();
        r.serve::<methods::Presence>(move |_, req| {
            let mut s = me.lock();
            let now: BTreeSet<u64> = req.characters.iter().map(|c| c.0).collect();
            let before = s.cells.insert(req.cell.0, now.clone()).unwrap_or_default();
            for gone in before.difference(&now) {
                if s.presence.get(gone) == Some(&req.cell.0) {
                    s.presence.remove(gone);
                }
            }
            for c in now {
                let moved = s.presence.insert(c, req.cell.0) != Some(req.cell.0);
                if moved {
                    s.arrived(req.cell.0, c);
                }
            }
            Ok(m::Empty {})
        });
        let me = self.clone();
        r.serve::<methods::Poll>(move |_, req| {
            let s = me.lock();
            let updates: Vec<m::SocialUpdate> = s
                .queues
                .get(&req.cell.0)
                .map(|q| q.iter().filter(|u| u.seq > req.since).take(32).copied().collect())
                .unwrap_or_default();
            Ok(m::SocialUpdates {
                updates: BoundedArray::from_slice(&updates).unwrap_or_default(),
                epoch: s.epoch,
            })
        });
        let me = self.clone();
        r.serve_later::<methods::RelayOp, _, _>(move |_, req| {
            let me = me.clone();
            async move { me.relay_op(&req).await }
        });
        let me = self.clone();
        r.serve::<methods::Restored>(move |_, req| {
            me.lock().restored.insert(req.cell.0);
            Ok(m::Empty {})
        });
        let me = self.clone();
        r.serve::<methods::Projection>(move |_, req| {
            let s = me.lock();
            let items: Vec<m::Projected> = s
                .projections
                .get(&req.cell.0)
                .map(|(_, q)| q.iter().filter(|p| p.seq > req.since).take(16).copied().collect())
                .unwrap_or_default();
            Ok(m::Projections {
                epoch: s.epoch,
                items: BoundedArray::from_slice(&items).unwrap_or_default(),
            })
        });
        let me = self.clone();
        r.serve_later::<methods::NewGuild, _, _>(move |_, req| {
            let me = me.clone();
            async move {
                let _order = me.order.lock().await;
                let (outcome, guild) = {
                    let mut s = me.lock();
                    let op = GuildOp::Create {
                        me: req.leader.0,
                        name: req.name.as_str().to_owned(),
                    };
                    let outcome = s.guilds.apply(&op, now_ms());
                    let guild = s.guilds.guild_of(req.leader.0);
                    (outcome, guild)
                };
                if outcome.changes.is_empty() {
                    return Err(refused("name taken, invalid, or already in a guild"));
                }
                me.settle(outcome).await;
                Ok(m::Guild {
                    guild: u64::from(guild.unwrap_or(0)),
                })
            }
        });
        let me = self.clone();
        r.serve_later::<methods::EnterGuild, _, _>(move |_, req| {
            let me = me.clone();
            async move {
                let _order = me.order.lock().await;
                let outcome = {
                    let mut s = me.lock();
                    let guild = u32::try_from(req.guild).map_err(|_| refused("no such guild"))?;
                    s.guilds
                        .admit(req.character.0, guild)
                        .ok_or_else(|| refused("no such guild, guild full, or already in a guild"))?
                };
                me.settle(outcome).await;
                Ok(m::Empty {})
            }
        });
        r
    }
}

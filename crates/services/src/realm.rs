//! Realm: characters, the cell directory, instance creation, and entry and
//! transfer tokens.
//!
//! Tokens are 32 random bytes held by the realm with what they grant
//! (character, cell, lease epoch, expiry, and for a returning character
//! where it enters) and redeemable once, by the cell they name.
//!
//! Characters are durable through the persistence writer
//! ([`RealmService::with_writer`]): creation, deletion and the placement
//! cell hosts report (where a character left the world or arrived after a
//! transfer) are written, as whole character rows in a numbered batch the
//! writer applies once, before the realm answers. A restarted realm reads
//! every row back ([`RealmService::load_durable`]) and places a returning
//! character in the world cell it left, where it left. Tokens and the cell
//! directory are not durable: a token issued before a restart is refused
//! after it (the client selects again), and cell hosts register again when
//! they see the new run.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex, MutexGuard};

use mantis_core::wire::{BoundedArray, WireString};
use ring::rand::SecureRandom;

use crate::generated::services as m;
use crate::host::clock::ServiceClock;
use crate::host::rpc::{Router, RpcClient, RpcError};
use crate::host::{Fence, RPC_TIMEOUT, refused, until_durable};
use crate::methods;
use crate::persist::{CharacterRecord, character_record, character_row, name_key};

/// How long a token stays valid, in milliseconds.
pub const TOKEN_MS: u64 = 60_000;

/// Living characters per account, at most.
pub const CHARACTERS_PER_ACCOUNT: usize = 16;

/// A registered cell.
#[derive(Clone, Debug, PartialEq)]
pub struct CellEntry {
    /// Its game address.
    pub address: String,
    /// Its x range (world cells).
    pub range: (f32, f32),
    /// An instance cell.
    pub instance: bool,
    /// An instance cell in use.
    pub busy: bool,
    /// The world it belongs to.
    pub world: u32,
}

/// What a token grants.
#[derive(Clone, Copy, Debug)]
struct Grant {
    character: u64,
    cell: u64,
    epoch: u64,
    expires_ms: u64,
    /// Where a returning character enters.
    spawn: Option<[f32; 3]>,
}

#[derive(Default)]
struct State {
    /// This run: hosts register their cells again when it changes.
    epoch: u64,
    cells: BTreeMap<u64, CellEntry>,
    /// The highest character id ever used (deleted ones included).
    next_character: u64,
    /// The last batch of character rows written.
    seq: u64,
    /// Durable characters by id, deleted ones included.
    characters: BTreeMap<u64, CharacterRecord>,
    tokens: BTreeMap<[u8; 32], Grant>,
}

/// The realm role.
#[derive(Clone, Default)]
pub struct RealmService {
    state: Arc<Mutex<State>>,
    rng: Arc<crate::host::Random>,
    clock: ServiceClock,
    /// The persistence writer character rows go through (none: in memory
    /// only, for tests of the rules alone).
    writer: Option<Arc<RpcClient>>,
    /// This instance's lease term (role failover).
    fence: Fence,
    /// One change at a time from durable write to answer.
    order: Arc<tokio::sync::Mutex<()>>,
}

impl RealmService {
    /// An empty realm in memory only (tests of the rules alone; a served
    /// realm uses [`RealmService::with_writer`]), a new run: its epoch is
    /// drawn at random, so a cell host polling [`methods::RealmRun`] sees a
    /// restart and registers its cells again.
    #[must_use]
    pub fn new() -> Self {
        let s = Self::default();
        let mut b = [0u8; 8];
        let epoch = ring::rand::SecureRandom::fill(&s.rng.0, &mut b)
            .map_or_else(|_| s.clock.now_ms(), |()| u64::from_le_bytes(b));
        s.lock().epoch = epoch.max(1);
        s
    }

    /// A new run writing every character row through `writer` before it
    /// answers. Call [`RealmService::load_durable`] before serving.
    #[must_use]
    pub fn with_writer(writer: RpcClient) -> Self {
        let mut s = Self::new();
        s.writer = Some(Arc::new(writer));
        s
    }

    /// The same role on `clock` (token lifetimes, creation times).
    #[must_use]
    pub fn clocked(mut self, clock: ServiceClock) -> Self {
        self.clock = clock;
        self
    }

    /// This run's epoch.
    #[must_use]
    pub fn epoch(&self) -> u64 {
        self.lock().epoch
    }

    /// The same role writing under lease term `fence` (role failover):
    /// once the term is over it writes nothing more and answers
    /// [`RpcError::Standby`].
    #[must_use]
    pub fn fenced(mut self, fence: Fence) -> Self {
        self.fence = fence;
        self
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        crate::host::lock(&self.state)
    }

    /// The cell directory.
    #[must_use]
    pub fn cells(&self) -> BTreeMap<u64, CellEntry> {
        self.lock().cells.clone()
    }

    /// The durable row of `character`, deleted or not (Ops and tests).
    #[must_use]
    pub fn character(&self, character: u64) -> Option<CharacterRecord> {
        self.lock().characters.get(&character).cloned()
    }

    /// Reads every durable character back from the writer (after a
    /// restart, before serving); returns how many are living.
    ///
    /// # Errors
    /// No writer (a served realm must have its store), or the writer's
    /// refusal: the realm must not serve.
    pub async fn load_durable(&self) -> Result<usize, String> {
        let writer = self
            .writer
            .as_ref()
            .ok_or("the realm has no persistence writer")?;
        let (mut rows, mut seq) = (Vec::new(), 0);
        for page in 0.. {
            // Boxed: a page of rows is large for a stack frame.
            let got =
                Box::pin(writer.call::<methods::LoadCharacters>(&m::ReadCharacterRows { page }, RPC_TIMEOUT))
                    .await
                    .map_err(|e| format!("loading characters: {e}"))?;
            seq = got.seq;
            rows.extend(got.rows.iter().map(character_record));
            if !got.more {
                break;
            }
        }
        let mut s = self.lock();
        s.seq = seq;
        s.next_character = rows.iter().map(|c| c.id).max().unwrap_or(0);
        s.characters = rows.into_iter().map(|c| (c.id, c)).collect();
        Ok(s.characters.values().filter(|c| !c.deleted).count())
    }

    /// Makes `row` durable (one numbered batch, retried until the writer
    /// answers), then makes it what the realm reads. Call with the order
    /// lock held.
    ///
    /// # Errors
    /// [`RpcError::Standby`]: the lease term is over; nothing was changed.
    async fn commit(&self, row: CharacterRecord) -> Result<(), RpcError> {
        if let Some(writer) = &self.writer {
            let seq = {
                let mut s = self.lock();
                s.seq += 1;
                s.seq
            };
            let req = m::StoreCharacterRows {
                epoch: self.fence.epoch(),
                seq,
                rows: BoundedArray::from_slice(&[character_row(&row)]).unwrap_or_default(),
            };
            until_durable::<methods::WriteCharacters>(writer, &self.fence, &req).await?;
        }
        let mut s = self.lock();
        s.next_character = s.next_character.max(row.id);
        s.characters.insert(row.id, row);
        Ok(())
    }

    fn token(&self, grant: Grant) -> Result<BoundedArray<u8, 32>, RpcError> {
        let mut t = [0u8; 32];
        self.rng.0.fill(&mut t).map_err(|_| refused("no randomness"))?;
        self.lock().tokens.insert(t, grant);
        Ok(BoundedArray::from_slice(&t).unwrap_or_default())
    }

    /// `character`, living and owned by `account`.
    fn owned(&self, account: u64, character: u64) -> Option<CharacterRecord> {
        self.lock()
            .characters
            .get(&character)
            .filter(|c| c.account == account && !c.deleted)
            .cloned()
    }

    fn select(&self, req: &m::SelectCharacter) -> Result<m::Placement, RpcError> {
        let c = self
            .owned(req.account.0, req.character.0)
            .ok_or_else(|| refused("not your character"))?;
        let (cell, address, spawn) = {
            let s = self.lock();
            // A returning character enters the world cell it left, where
            // it left; a new one (or one whose cell is gone or was an
            // instance) the world cell owning x = 0 in the lowest-numbered
            // world registered.
            let returning = s
                .cells
                .get(&c.cell)
                .filter(|e| !e.instance)
                .map(|e| (c.cell, e.address.clone(), Some(c.position)));
            let fresh = || {
                s.cells
                    .iter()
                    .filter(|(_, e)| !e.instance && e.range.0 <= 0.0 && 0.0 < e.range.1)
                    .min_by_key(|(id, e)| (e.world, **id))
                    .map(|(id, e)| (*id, e.address.clone(), None))
            };
            returning.or_else(fresh).ok_or_else(|| refused("no world cell"))?
        };
        Ok(m::Placement {
            cell: m::CellNo(cell),
            address: WireString::new(&address).unwrap_or_default(),
            token: self.token(Grant {
                character: c.id,
                cell,
                epoch: 1,
                expires_ms: self.clock.now_ms() + TOKEN_MS,
                spawn,
            })?,
        })
    }

    async fn create(&self, req: &m::CreateCharacter) -> Result<m::Created, RpcError> {
        let _order = self.order.lock().await;
        let id = {
            let s = self.lock();
            let living = s
                .characters
                .values()
                .filter(|c| c.account == req.account.0 && !c.deleted)
                .count();
            if living >= CHARACTERS_PER_ACCOUNT {
                return Err(refused("too many characters"));
            }
            let key = name_key(req.name.as_str());
            if s.characters
                .values()
                .any(|c| !c.deleted && name_key(&c.name) == key)
            {
                return Err(refused("name taken"));
            }
            s.next_character + 1
        };
        self.commit(CharacterRecord {
            id,
            account: req.account.0,
            name: req.name.as_str().to_owned(),
            kind: req.kind,
            created_ms: self.clock.now_ms(),
            deleted: false,
            cell: 0,
            world: 0,
            position: [0.0; 3],
            level: 1,
        })
        .await?;
        Ok(m::Created {
            character: m::CharacterId(id),
        })
    }

    async fn delete(&self, req: &m::DeleteCharacter) -> Result<m::Empty, RpcError> {
        let _order = self.order.lock().await;
        let mut c = self
            .owned(req.account.0, req.character.0)
            .ok_or_else(|| refused("not your character"))?;
        c.deleted = true;
        self.commit(c).await?;
        Ok(m::Empty {})
    }

    async fn place(&self, req: &m::CharacterPlaced) -> Result<m::Empty, RpcError> {
        let _order = self.order.lock().await;
        let Some(mut c) = self.character(req.character.0).filter(|c| !c.deleted) else {
            // A character the realm does not know (a test session, or one
            // deleted meanwhile): nothing to keep.
            return Ok(m::Empty {});
        };
        let before = c.clone();
        c.cell = req.cell.0;
        c.world = req.world;
        c.position = [req.x, req.y, req.z];
        if req.level != 0 {
            c.level = req.level;
        }
        // The same summary twice is applied once.
        if c != before {
            self.commit(c).await?;
        }
        Ok(m::Empty {})
    }

    fn instance(&self) -> Result<m::InstanceCell, RpcError> {
        let mut s = self.lock();
        let (id, c) = s
            .cells
            .iter_mut()
            .find(|(_, c)| c.instance && !c.busy)
            .ok_or_else(|| refused("no free instance cell"))?;
        c.busy = true;
        Ok(m::InstanceCell {
            cell: m::CellNo(*id),
            address: WireString::new(&c.address).unwrap_or_default(),
        })
    }

    fn redeem(&self, req: &m::Redeem) -> Result<m::Redeemed, RpcError> {
        self.redeem_on(&req.token, &[req.cell.0])
    }

    /// Where `token`'s session goes, without consuming it: its cell, and
    /// that cell's host address (empty while no host serves it).
    fn route(&self, req: &m::RouteToken) -> Result<m::EntryRoute, RpcError> {
        let bytes: Vec<u8> = req.token.iter().copied().collect();
        let token: [u8; 32] = bytes.try_into().map_err(|_| refused("bad token"))?;
        let state = self.lock();
        let grant = state
            .tokens
            .get(&token)
            .filter(|g| g.expires_ms >= self.clock.now_ms())
            .ok_or_else(|| refused("bad token"))?;
        let address = state
            .cells
            .get(&grant.cell)
            .map(|c| c.address.clone())
            .unwrap_or_default();
        Ok(m::EntryRoute {
            cell: m::CellNo(grant.cell),
            address: WireString::new(&address).unwrap_or_default(),
        })
    }

    /// Consumes `token` if it names one of `cells` and has not expired.
    fn redeem_on(&self, token: &BoundedArray<u8, 32>, cells: &[u64]) -> Result<m::Redeemed, RpcError> {
        let bytes: Vec<u8> = token.iter().copied().collect();
        let token: [u8; 32] = bytes.try_into().map_err(|_| refused("bad token"))?;
        let mut state = self.lock();
        let grant = *state.tokens.get(&token).ok_or_else(|| refused("bad token"))?;
        if !cells.contains(&grant.cell) {
            return Err(refused("token for another cell"));
        }
        state.tokens.remove(&token);
        if grant.expires_ms < self.clock.now_ms() {
            return Err(refused("expired token"));
        }
        let at = grant.spawn.unwrap_or([0.0; 3]);
        Ok(m::Redeemed {
            character: m::CharacterId(grant.character),
            epoch: grant.epoch,
            placed: grant.spawn.is_some(),
            x: at[0],
            y: at[1],
            z: at[2],
        })
    }

    /// The role's RPC methods.
    #[must_use]
    pub fn router(&self) -> Router {
        let mut r = Router::validated(methods::validate);
        let me = self.clone();
        r.serve::<methods::RealmRun>(move |_, _| Ok(m::RealmEpoch { epoch: me.epoch() }));
        let me = self.clone();
        r.serve::<methods::RegisterCellHost>(move |_, req| {
            me.lock().cells.insert(
                req.cell.0,
                CellEntry {
                    address: req.address.as_str().to_owned(),
                    range: (req.lo, req.hi),
                    instance: req.instance,
                    busy: false,
                    world: req.world,
                },
            );
            Ok(m::Empty {})
        });
        let me = self.clone();
        r.serve::<methods::Withdraw>(move |_, req| {
            me.lock().cells.remove(&req.cell.0);
            Ok(m::Empty {})
        });
        let me = self.clone();
        r.serve::<methods::ListAccountCharacters>(move |_, req| {
            let ids: Vec<m::CharacterId> = me
                .lock()
                .characters
                .values()
                .filter(|c| c.account == req.account.0 && !c.deleted)
                .map(|c| m::CharacterId(c.id))
                .collect();
            Ok(m::Characters {
                ids: BoundedArray::from_slice(&ids).unwrap_or_default(),
            })
        });
        let me = self.clone();
        r.serve_later::<methods::NewCharacter, _, _>(move |_, req| {
            let me = me.clone();
            async move { me.create(&req).await }
        });
        let me = self.clone();
        r.serve_later::<methods::RemoveCharacter, _, _>(move |_, req| {
            let me = me.clone();
            async move { me.delete(&req).await }
        });
        let me = self.clone();
        r.serve_later::<methods::PlaceCharacter, _, _>(move |_, req| {
            let me = me.clone();
            async move { me.place(&req).await }
        });
        let me = self.clone();
        r.serve::<methods::Select>(move |_, req| me.select(&req));
        let me = self.clone();
        r.serve::<methods::NewInstance>(move |_, _req| me.instance());
        let me = self.clone();
        r.serve::<methods::Transfer>(move |_, req| {
            Ok(m::TransferToken {
                token: me.token(Grant {
                    character: req.character.0,
                    cell: req.to.0,
                    epoch: req.epoch,
                    expires_ms: me.clock.now_ms() + TOKEN_MS,
                    spawn: None,
                })?,
            })
        });
        let me = self.clone();
        r.serve::<methods::RedeemToken>(move |_, req| me.redeem(&req));
        let me = self.clone();
        r.serve::<methods::RouteEntry>(move |_, req| me.route(&req));
        let me = self.clone();
        r.serve::<methods::RedeemForHost>(move |_, req| {
            let cells: Vec<u64> = req.cells.iter().map(|c| c.0).collect();
            me.redeem_on(&req.token, &cells)
        });
        r
    }
}

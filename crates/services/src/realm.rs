//! Realm: characters, the cell directory, instance creation, and entry and
//! transfer tokens.
//!
//! Tokens are 32 random bytes held by the realm with what they grant
//! (character, cell, lease epoch, expiry) and redeemable once, by the cell
//! they name.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex, MutexGuard};

use mantis_core::wire::{BoundedArray, WireString};
use ring::rand::SecureRandom;

use crate::generated::services as m;
use crate::host::rpc::{Router, RpcError};
use crate::host::{now_ms, refused};
use crate::methods;

/// How long a token stays valid, in milliseconds.
pub const TOKEN_MS: u64 = 60_000;

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
}

#[derive(Default)]
struct State {
    cells: BTreeMap<u64, CellEntry>,
    next_character: u64,
    characters: BTreeMap<u64, Vec<(u64, String)>>,
    tokens: BTreeMap<[u8; 32], (u64, u64, u64, u64)>,
}

/// The realm role.
#[derive(Clone, Default)]
pub struct RealmService {
    state: Arc<Mutex<State>>,
    rng: Arc<crate::host::Random>,
}

impl RealmService {
    /// An empty realm.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        crate::host::lock(&self.state)
    }

    /// The cell directory.
    #[must_use]
    pub fn cells(&self) -> BTreeMap<u64, CellEntry> {
        self.lock().cells.clone()
    }

    fn token(&self, character: u64, cell: u64, epoch: u64) -> Result<BoundedArray<u8, 32>, RpcError> {
        let mut t = [0u8; 32];
        self.rng.0.fill(&mut t).map_err(|_| refused("no randomness"))?;
        self.lock()
            .tokens
            .insert(t, (character, cell, epoch, now_ms() + TOKEN_MS));
        Ok(BoundedArray::from_slice(&t).unwrap_or_default())
    }

    fn owns(&self, account: u64, character: u64) -> bool {
        self.lock()
            .characters
            .get(&account)
            .is_some_and(|c| c.iter().any(|(id, _)| *id == character))
    }

    fn select(&self, req: &m::SelectCharacter) -> Result<m::Placement, RpcError> {
        if !self.owns(req.account.0, req.character.0) {
            return Err(refused("not your character"));
        }
        // New characters enter at x = 0: the world cell owning it.
        let (cell, address) = self
            .lock()
            .cells
            .iter()
            .find(|(_, c)| !c.instance && c.range.0 <= 0.0 && 0.0 < c.range.1)
            .map(|(id, c)| (*id, c.address.clone()))
            .ok_or_else(|| refused("no world cell"))?;
        Ok(m::Placement {
            cell: m::CellNo(cell),
            address: WireString::new(&address).unwrap_or_default(),
            token: self.token(req.character.0, cell, 1)?,
        })
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

    /// Consumes `token` if it names one of `cells` and has not expired.
    fn redeem_on(&self, token: &BoundedArray<u8, 32>, cells: &[u64]) -> Result<m::Redeemed, RpcError> {
        let bytes: Vec<u8> = token.iter().copied().collect();
        let token: [u8; 32] = bytes.try_into().map_err(|_| refused("bad token"))?;
        let mut s = self.lock();
        let (character, cell, epoch, expires) = *s.tokens.get(&token).ok_or_else(|| refused("bad token"))?;
        if !cells.contains(&cell) {
            return Err(refused("token for another cell"));
        }
        s.tokens.remove(&token);
        if expires < now_ms() {
            return Err(refused("expired token"));
        }
        Ok(m::Redeemed {
            character: m::CharacterId(character),
            epoch,
        })
    }

    /// The role's RPC methods.
    #[must_use]
    pub fn router(&self) -> Router {
        let mut r = Router::validated(methods::validate);
        let me = self.clone();
        r.serve::<methods::RegisterCellHost>(move |_, req| {
            me.lock().cells.insert(
                req.cell.0,
                CellEntry {
                    address: req.address.as_str().to_owned(),
                    range: (req.lo, req.hi),
                    instance: req.instance,
                    busy: false,
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
                .get(&req.account.0)
                .map(|c| c.iter().map(|(id, _)| m::CharacterId(*id)).collect())
                .unwrap_or_default();
            Ok(m::Characters {
                ids: BoundedArray::from_slice(&ids).unwrap_or_default(),
            })
        });
        let me = self.clone();
        r.serve::<methods::NewCharacter>(move |_, req| {
            let mut s = me.lock();
            if s.characters.get(&req.account.0).is_some_and(|c| c.len() >= 16) {
                return Err(refused("too many characters"));
            }
            s.next_character += 1;
            let id = s.next_character;
            s.characters
                .entry(req.account.0)
                .or_default()
                .push((id, req.name.as_str().to_owned()));
            Ok(m::Created {
                character: m::CharacterId(id),
            })
        });
        let me = self.clone();
        r.serve::<methods::Select>(move |_, req| me.select(&req));
        let me = self.clone();
        r.serve::<methods::NewInstance>(move |_, _req| me.instance());
        let me = self.clone();
        r.serve::<methods::Transfer>(move |_, req| {
            Ok(m::TransferToken {
                token: me.token(req.character.0, req.to.0, req.epoch)?,
            })
        });
        let me = self.clone();
        r.serve::<methods::RedeemToken>(move |_, req| me.redeem(&req));
        let me = self.clone();
        r.serve::<methods::RedeemForHost>(move |_, req| {
            let cells: Vec<u64> = req.cells.iter().map(|c| c.0).collect();
            me.redeem_on(&req.token, &cells)
        });
        r
    }
}

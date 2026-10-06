//! Matchmaking: queues and placement. When a queue holds a full group, the
//! matcher asks the realm for an instance cell and places the group there.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex, MutexGuard};

use mantis_core::wire::WireString;

use crate::generated::services as m;
use crate::host::rpc::{Router, RpcError};
use crate::methods;

/// Asks for an instance cell (the realm, over RPC, in a running cluster).
pub type InstanceSource = Arc<dyn Fn(u16) -> Result<(u64, String), RpcError> + Send + Sync>;

#[derive(Default)]
struct State {
    queues: BTreeMap<u16, Vec<u64>>,
    placed: BTreeMap<u64, (u64, String, u16)>,
}

/// The matchmaking role.
#[derive(Clone)]
pub struct MatchmakingService {
    state: Arc<Mutex<State>>,
    group: usize,
    instances: InstanceSource,
}

impl MatchmakingService {
    /// Groups of `group` characters per match; instances from `instances`.
    #[must_use]
    pub fn new(group: usize, instances: InstanceSource) -> Self {
        Self {
            state: Arc::default(),
            group: group.max(1),
            instances,
        }
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        crate::host::lock(&self.state)
    }

    fn enqueue(&self, req: &m::Enqueue) -> Result<m::Empty, RpcError> {
        let full = {
            let mut s = self.lock();
            if s.placed.contains_key(&req.character.0)
                || s.queues.values().any(|q| q.contains(&req.character.0))
            {
                return Err(RpcError::Refused("already queued or placed".to_owned()));
            }
            let q = s.queues.entry(req.queue).or_default();
            q.push(req.character.0);
            (q.len() >= self.group).then(|| q.drain(..self.group).collect::<Vec<u64>>())
        };
        if let Some(group) = full {
            match (self.instances)(req.queue) {
                Ok((cell, address)) => {
                    let mut s = self.lock();
                    for c in group {
                        s.placed.insert(c, (cell, address.clone(), req.queue));
                    }
                }
                Err(e) => {
                    // Put the group back at the front: nobody loses their place.
                    let mut s = self.lock();
                    let q = s.queues.entry(req.queue).or_default();
                    let rest = std::mem::take(q);
                    q.extend(group);
                    q.extend(rest);
                    return Err(e);
                }
            }
        }
        Ok(m::Empty {})
    }

    /// The role's RPC methods.
    #[must_use]
    pub fn router(&self) -> Router {
        let mut r = Router::validated(methods::validate);
        let me = self.clone();
        r.serve::<methods::Queue>(move |_, req| me.enqueue(&req));
        let me = self.clone();
        r.serve::<methods::MatchFor>(move |_, req| {
            let placed = me.lock().placed.remove(&req.character.0);
            Ok(match placed {
                Some((cell, address, queue)) => m::Match {
                    cell: m::CellNo(cell),
                    address: WireString::new(&address).unwrap_or_default(),
                    queue,
                },
                None => m::Match {
                    cell: m::CellNo(0),
                    address: WireString::default(),
                    queue: 0,
                },
            })
        });
        r
    }
}

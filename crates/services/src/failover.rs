//! Role failover: active and standby instances of a role.
//!
//! Every instance of a failover role (account, realm, social, Ops) runs a
//! [`Seat`]: a lease task on the role's injected clock, and a router that
//! answers [`RpcError::Standby`](crate::host::rpc::RpcError::Standby) while
//! the instance is a standby.
//!
//! - **One active per role**, chosen by a lease row in the persistence
//!   writer (owner, epoch, expiry on the clock). The active renews it every
//!   third of its lifetime; a standby asks as often and takes it over once
//!   it lapses, with the next epoch.
//! - **Takeover is a restart.** The new active builds a fresh service and
//!   loads its durable state, exactly as a restarted role does. Social's
//!   parties are rebuilt from cell presence, as after a restart.
//! - **Stepping down.** An active steps down when its lease is someone
//!   else's, when it cannot renew for a whole lifetime, or when the writer
//!   refuses one of its writes as stale (its [`Fence`] is lost). It then
//!   answers as a standby.
//! - **Fencing.** Every durable write carries the writer's epoch, and the
//!   persistence writer refuses an epoch that is not its role's current one
//!   ([`RpcError::StaleEpoch`](crate::host::rpc::RpcError::StaleEpoch)), so
//!   an instance that lost its lease, even one that does not know it yet,
//!   cannot write twice.
//! - **Callers** list every instance in one [`Endpoint`](crate::host::rpc::Endpoint):
//!   a call goes to the active one, and moves on from a standby or an
//!   unreachable instance.
//!
//! The persistence writer itself is one instance (see `docs/SERVER.md`).

use std::future::Future;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use mantis_core::wire::WireString;
use tokio::sync::watch;

use crate::generated::services as m;
use crate::host::clock::ServiceClock;
use crate::host::rpc::{Router, RpcClient, SeatSlot};
use crate::host::{Fence, RPC_TIMEOUT, Role, lock};
use crate::methods;

/// One instance's lease settings.
#[derive(Clone, Debug)]
pub struct LeaseConfig {
    /// The role.
    pub role: Role,
    /// This instance: unique in the cluster, the same across its restarts.
    pub owner: String,
    /// How long a lease lasts unless renewed. The active renews it, and a
    /// standby asks for it, every third of this.
    pub ttl: Duration,
}

fn ms(d: Duration) -> u64 {
    u64::try_from(d.as_millis()).unwrap_or(u64::MAX)
}

/// One instance of a failover role: its lease task and the service of its
/// current term, if it is the active instance.
pub struct Seat<S> {
    slot: SeatSlot,
    service: Arc<Mutex<Option<S>>>,
    /// The clock time the lease task's next round is due.
    due: watch::Receiver<u64>,
    clock: ServiceClock,
    task: tokio::task::AbortHandle,
}

impl<S: Clone + Send + 'static> Seat<S> {
    /// Starts the lease task (on the current runtime), calling the
    /// persistence writer through `persist`. On every takeover it runs
    /// `build` with the new term's fence: a fresh service that has loaded
    /// its durable state, and its router.
    #[must_use]
    pub fn start<B, F>(persist: Arc<RpcClient>, lease: LeaseConfig, clock: ServiceClock, build: B) -> Self
    where
        B: Fn(Fence) -> F + Send + Sync + 'static,
        F: Future<Output = Result<(S, Router), String>> + Send + 'static,
    {
        let slot = SeatSlot::default();
        let service = Arc::new(Mutex::new(None));
        let (tx, due) = watch::channel(0u64);
        let task = tokio::spawn(run(
            persist,
            lease,
            clock.clone(),
            build,
            slot.clone(),
            Arc::clone(&service),
            tx,
        ));
        Self {
            slot,
            service,
            due,
            clock,
            task: task.abort_handle(),
        }
    }

    /// The router to serve: the active term's, or standby answers.
    #[must_use]
    pub fn router(&self) -> Router {
        Router::seated(self.slot.clone())
    }

    /// The current term's lease epoch, while this instance is the active
    /// one.
    #[must_use]
    pub fn epoch(&self) -> Option<u64> {
        self.slot.epoch()
    }

    /// True while this instance is its role's active one.
    #[must_use]
    pub fn is_active(&self) -> bool {
        self.epoch().is_some()
    }

    /// The current term's service, while this instance is the active one.
    #[must_use]
    pub fn service(&self) -> Option<S> {
        if self.is_active() {
            lock(&self.service).clone()
        } else {
            None
        }
    }

    /// The service of the latest term this instance held, active or not.
    #[must_use]
    pub fn last_service(&self) -> Option<S> {
        lock(&self.service).clone()
    }

    /// Completes once the lease task has had every round due by the
    /// clock's time now (a takeover's durable load included). For tests on
    /// a manual clock: advance it, then settle every seat.
    pub async fn settled(&self) {
        let now = self.clock.now_ms();
        let mut due = self.due.clone();
        let _ = due.wait_for(|d| *d > now).await;
    }

    /// Stops the lease task while the seat keeps serving, as a hung
    /// process's would: its term ends only when the writer refuses one of
    /// its writes. For tests.
    pub fn stall(&self) {
        self.task.abort();
    }
}

impl<S> Drop for Seat<S> {
    fn drop(&mut self) {
        self.task.abort();
        self.slot.vacate();
    }
}

async fn run<S, B, F>(
    persist: Arc<RpcClient>,
    lease: LeaseConfig,
    clock: ServiceClock,
    build: B,
    slot: SeatSlot,
    service: Arc<Mutex<Option<S>>>,
    due: watch::Sender<u64>,
) where
    B: Fn(Fence) -> F + Send + Sync + 'static,
    F: Future<Output = Result<(S, Router), String>> + Send + 'static,
{
    let ttl_ms = ms(lease.ttl).max(3);
    let period = ttl_ms / 3;
    let owner = WireString::new(&lease.owner).unwrap_or_default();
    // The clock time of the last renewal held.
    let mut renewed: Option<u64> = None;
    loop {
        let now = clock.now_ms();
        let req = m::AcquireLease {
            role: lease.role as u8,
            owner,
            now_ms: now,
            ttl_ms,
        };
        match persist.call::<methods::Lease>(&req, RPC_TIMEOUT).await {
            Ok(held) if held.owner.as_str() == lease.owner => {
                renewed = Some(now);
                if slot.epoch() != Some(held.epoch) {
                    // A new term: a fresh service, its durable state loaded.
                    slot.vacate();
                    let fence = Fence::held(held.epoch);
                    match build(fence.clone()).await {
                        Ok((s, router)) => {
                            *lock(&service) = Some(s);
                            slot.fill(router, fence);
                        }
                        Err(e) => eprintln!(
                            "{} {}: taking over at epoch {} failed: {e}",
                            lease.role.name(),
                            lease.owner,
                            held.epoch
                        ),
                    }
                }
            }
            Ok(_) => {
                renewed = None;
                slot.vacate();
            }
            Err(_) => {
                if renewed.is_none_or(|t| now.saturating_sub(t) >= ttl_ms) {
                    slot.vacate();
                }
            }
        }
        let next = now.saturating_add(period);
        let _ = due.send(next);
        clock
            .sleep(Duration::from_millis(next.saturating_sub(clock.now_ms())))
            .await;
    }
}

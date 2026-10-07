//! A local cluster's failover roles in replicated mode
//! ([`ClusterConfig::instances`](super::ClusterConfig::instances) of two or
//! more): every instance of account, realm, social, matchmaking and Ops is
//! a [`Seat`] with its own RPC server, and callers reach a role through one
//! [`Endpoint`] listing its instances.

use std::collections::BTreeMap;
use std::future::Future;
use std::net::{IpAddr, SocketAddr};
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use super::{client_for, identity_for};
use crate::account::AccountService;
use crate::failover::{LeaseConfig, Seat};
use crate::generated::services as m;
use crate::host::clock::ServiceClock;
use crate::host::rpc::{Endpoint, Router, RpcClient, RpcServer};
use crate::host::{Fence, RPC_TIMEOUT, Role, lock};
use crate::matchmaking::{InstanceSource, MatchmakingService};
use crate::methods;
use crate::ops::OpsService;
use crate::ops::live::LiveSigner;
use crate::realm::RealmService;
use crate::social::SocialService;
use crate::tls::{TlsHandle, TlsIdentity};

/// How long starting a cluster waits for every failover role to have its
/// active instance.
const SETTLE_TIMEOUT: Duration = Duration::from_secs(30);

/// The latest term's service of a failover role.
pub(super) type Latest<S> = Arc<Mutex<Option<S>>>;

/// A role's service as the cluster sees it: the only instance's, or the
/// latest term's of a replicated role.
pub(super) struct Current<S> {
    latest: Latest<S>,
    first: S,
}

impl<S: Clone> Current<S> {
    /// The only instance's service.
    pub(super) fn new(first: S) -> Self {
        Self {
            latest: Arc::new(Mutex::new(Some(first.clone()))),
            first,
        }
    }

    /// The service now (the latest term's, for a replicated role).
    pub(super) fn get(&self) -> S {
        lock(&self.latest).clone().unwrap_or_else(|| self.first.clone())
    }

    /// A restarted instance's service.
    pub(super) fn set(&self, s: S) {
        *lock(&self.latest) = Some(s);
    }
}

/// Every role's service, as the cluster sees it.
pub(super) struct Roles {
    pub(super) account: Current<AccountService>,
    pub(super) realm: Current<RealmService>,
    pub(super) social: Current<SocialService>,
    pub(super) ops: Current<OpsService>,
}

/// The cells Ops inspects, (cell, inspector), for every Ops term.
pub(super) type Inspected = Arc<Mutex<Vec<(u64, SocketAddr)>>>;

/// What starting a role's instances needs.
#[derive(Clone)]
pub(super) struct Plan {
    pub(super) instances: usize,
    pub(super) ttl: Duration,
    pub(super) tls: Option<BTreeMap<Role, Arc<TlsIdentity>>>,
    pub(super) key: Vec<u8>,
    pub(super) persist: SocketAddr,
    pub(super) clock: ServiceClock,
    pub(super) bind: IpAddr,
    pub(super) group: usize,
}

impl Plan {
    /// `role`'s client of the persistence writer.
    fn writer(&self, role: Role) -> Result<RpcClient, String> {
        client_for(self.tls.as_ref(), self.persist, role, Role::Persist, &self.key)
    }

    /// A client calling `server` at `endpoint` as `caller`.
    fn client(&self, endpoint: &Endpoint, caller: Role, server: Role) -> Result<RpcClient, String> {
        let tls = identity_for(self.tls.as_ref(), caller)?.map(TlsHandle::from);
        RpcClient::with_endpoint(endpoint.clone(), caller, self.key.clone(), tls, server)
            .map_err(|e| format!("{} calling {}: {e}", caller.name(), server.name()))
    }

    /// Binds `role`'s RPC server at `addr` serving `router`.
    pub(super) fn bind(
        &self,
        runtime: &tokio::runtime::Runtime,
        role: Role,
        addr: SocketAddr,
        router: Router,
    ) -> Result<RpcServer, String> {
        let identity = identity_for(self.tls.as_ref(), role)?;
        runtime
            .block_on(RpcServer::bind_tls(
                addr,
                self.key.clone(),
                router,
                identity.map(TlsHandle::from),
            ))
            .map_err(|e| format!("{}: {e}", role.name()))
    }

    /// Binds `role`'s RPC server on this plan's address, any port.
    pub(super) fn bind_any(
        &self,
        runtime: &tokio::runtime::Runtime,
        role: Role,
        router: Router,
    ) -> Result<RpcServer, String> {
        self.bind(runtime, role, SocketAddr::new(self.bind, 0), router)
    }
}

/// Matchmaking's source of instance cells: the realm, over RPC, waited on
/// without blocking the runtime.
pub(super) fn instance_source(realm: Arc<RpcClient>, handle: tokio::runtime::Handle) -> InstanceSource {
    Arc::new(move |queue| {
        let req = m::CreateInstance {
            template: u32::from(queue),
        };
        let (realm, handle) = (Arc::clone(&realm), handle.clone());
        tokio::task::block_in_place(|| {
            handle.block_on(async move { realm.call::<methods::NewInstance>(&req, RPC_TIMEOUT).await })
        })
        .map(|i| (i.cell.0, i.address.as_str().to_owned()))
    })
}

/// A running seat, whatever its service.
pub(super) trait Seated: Send + Sync {
    /// The router its server serves.
    fn seat_router(&self) -> Router;
    /// Its term's epoch, while active.
    fn seat_epoch(&self) -> Option<u64>;
    /// Completes once every lease round due by the clock's now has run.
    fn seat_settled(&self) -> Pin<Box<dyn Future<Output = ()> + Send + '_>>;
    /// Stops its lease task; it keeps serving.
    fn seat_stall(&self);
}

impl<S: Clone + Send + Sync + 'static> Seated for Seat<S> {
    fn seat_router(&self) -> Router {
        self.router()
    }

    fn seat_epoch(&self) -> Option<u64> {
        self.epoch()
    }

    fn seat_settled(&self) -> Pin<Box<dyn Future<Output = ()> + Send + '_>> {
        Box::pin(self.settled())
    }

    fn seat_stall(&self) {
        self.stall();
    }
}

/// Starts one of a role's seats (on the current runtime).
type StartSeat = Arc<dyn Fn() -> Box<dyn Seated> + Send + Sync>;

/// One instance of a failover role.
pub(super) struct Replica {
    pub(super) role: Role,
    pub(super) index: usize,
    pub(super) addr: SocketAddr,
    /// Its seat and server, while it runs.
    pub(super) running: Option<(Box<dyn Seated>, RpcServer)>,
    start: StartSeat,
}

impl Replica {
    /// Starts it again at its address.
    pub(super) fn restart(&mut self, runtime: &tokio::runtime::Runtime, plan: &Plan) -> Result<(), String> {
        let seat = {
            let _guard = runtime.enter();
            (self.start)()
        };
        let server = plan.bind(runtime, self.role, self.addr, seat.seat_router())?;
        self.running = Some((seat, server));
        Ok(())
    }
}

/// A seat's build: a fresh service for a new term, its durable state
/// loaded, and its router.
type Built<S> = Pin<Box<dyn Future<Output = Result<(S, Router), String>> + Send>>;

/// Starts `plan.instances` instances of `role`, each a seat building its
/// service with `build`: their replicas, and the endpoint listing them.
fn replicate_role<S, B>(
    runtime: &tokio::runtime::Runtime,
    plan: &Plan,
    role: Role,
    build: B,
) -> Result<(Vec<Replica>, Endpoint), String>
where
    S: Clone + Send + Sync + 'static,
    B: Fn(Fence) -> Built<S> + Clone + Send + Sync + 'static,
{
    let mut out = Vec::new();
    for index in 0..plan.instances {
        let persist = Arc::new(plan.writer(role)?);
        let lease = LeaseConfig {
            role,
            owner: format!("{}-{}", role.name(), index + 1),
            ttl: plan.ttl,
        };
        let (clock, build) = (plan.clock.clone(), build.clone());
        let start: StartSeat = Arc::new(move || {
            Box::new(Seat::start(
                Arc::clone(&persist),
                lease.clone(),
                clock.clone(),
                build.clone(),
            )) as Box<dyn Seated>
        });
        let seat = {
            let _guard = runtime.enter();
            start()
        };
        let server = plan.bind_any(runtime, role, seat.seat_router())?;
        out.push(Replica {
            role,
            index,
            addr: server.addr(),
            running: Some((seat, server)),
            start,
        });
    }
    let addrs: Vec<SocketAddr> = out.iter().map(|r| r.addr).collect();
    Ok((out, Endpoint::instances(&addrs)))
}

/// Waits until every running seat has had each lease round due by now.
///
/// # Errors
/// They did not within [`SETTLE_TIMEOUT`].
pub(super) fn settle(runtime: &tokio::runtime::Runtime, replicas: &[Replica]) -> Result<(), String> {
    runtime
        .block_on(async {
            tokio::time::timeout(SETTLE_TIMEOUT, async {
                for r in replicas {
                    if let Some((seat, _)) = &r.running {
                        seat.seat_settled().await;
                    }
                }
            })
            .await
        })
        .map_err(|_| "the failover roles' lease tasks did not settle".to_owned())
}

fn account_build(
    plan: Plan,
    latest: Latest<AccountService>,
) -> impl Fn(Fence) -> Built<AccountService> + Clone {
    move |fence| {
        let (writer, clock, latest) = (
            plan.writer(Role::Account),
            plan.clock.clone(),
            Arc::clone(&latest),
        );
        Box::pin(async move {
            let s = AccountService::with_writer(writer?).clocked(clock).fenced(fence);
            s.load_durable().await?;
            *lock(&latest) = Some(s.clone());
            let router = s.router();
            Ok((s, router))
        })
    }
}

fn realm_build(plan: Plan, latest: Latest<RealmService>) -> impl Fn(Fence) -> Built<RealmService> + Clone {
    move |fence| {
        let (writer, clock, latest) = (plan.writer(Role::Realm), plan.clock.clone(), Arc::clone(&latest));
        Box::pin(async move {
            let s = RealmService::with_writer(writer?).clocked(clock).fenced(fence);
            s.load_durable().await?;
            *lock(&latest) = Some(s.clone());
            let router = s.router();
            Ok((s, router))
        })
    }
}

fn social_build(plan: Plan, latest: Latest<SocialService>) -> impl Fn(Fence) -> Built<SocialService> + Clone {
    move |fence| {
        let (writer, clock, latest) = (plan.writer(Role::Social), plan.clock.clone(), Arc::clone(&latest));
        Box::pin(async move {
            let s = SocialService::with_writer(writer?).clocked(clock).fenced(fence);
            s.load_durable().await?;
            *lock(&latest) = Some(s.clone());
            let router = s.router();
            Ok((s, router))
        })
    }
}

fn matchmaking_build(plan: Plan, realm: Endpoint) -> impl Fn(Fence) -> Built<MatchmakingService> + Clone {
    move |_fence| {
        let (to_realm, group) = (plan.client(&realm, Role::Matchmaking, Role::Realm), plan.group);
        Box::pin(async move {
            let source = instance_source(Arc::new(to_realm?), tokio::runtime::Handle::current());
            let s = MatchmakingService::new(group, source);
            let router = s.router();
            Ok((s, router))
        })
    }
}

fn ops_build(
    plan: Plan,
    account: Endpoint,
    pkcs8: Vec<u8>,
    inspected: Inspected,
    latest: Latest<OpsService>,
) -> impl Fn(Fence) -> Built<OpsService> + Clone {
    move |fence| {
        let persist = plan.writer(Role::Ops);
        let to_account = plan.client(&account, Role::Ops, Role::Account);
        let signer = LiveSigner::from_pkcs8(&pkcs8);
        let cells: Vec<(u64, Result<RpcClient, String>)> = lock(&inspected)
            .iter()
            .map(|(cell, at)| {
                let c = client_for(plan.tls.as_ref(), *at, Role::Ops, Role::Cell, &plan.key);
                (*cell, c)
            })
            .collect();
        let (clock, latest) = (plan.clock.clone(), Arc::clone(&latest));
        Box::pin(async move {
            let s = OpsService::remote(Arc::new(persist?), Arc::new(to_account?), signer?)
                .clocked(clock)
                .fenced(fence);
            s.load_live().await?;
            for (cell, client) in cells {
                s.add_cell(cell, Arc::new(client?));
            }
            *lock(&latest) = Some(s.clone());
            let router = s.router();
            Ok((s, router))
        })
    }
}

/// The service a role's first term built.
fn first<S: Clone>(latest: &Latest<S>, role: Role) -> Result<S, String> {
    lock(latest)
        .clone()
        .ok_or_else(|| format!("no {} instance took the lease", role.name()))
}

/// Starts every failover role's instances, waits until each role has its
/// active one, and returns the roles' services and the instances.
///
/// # Errors
/// A client, a bind, or a role whose instances never took the lease.
pub(super) fn replicate(
    runtime: &tokio::runtime::Runtime,
    plan: &Plan,
    pkcs8: &[u8],
    inspected: &Inspected,
) -> Result<(Roles, Vec<Replica>), String> {
    let (account, realm, social, ops): (Latest<_>, Latest<_>, Latest<_>, Latest<_>) =
        (Arc::default(), Arc::default(), Arc::default(), Arc::default());
    let (mut replicas, account_at) = replicate_role(
        runtime,
        plan,
        Role::Account,
        account_build(plan.clone(), Arc::clone(&account)),
    )?;
    let (more, realm_at) = replicate_role(
        runtime,
        plan,
        Role::Realm,
        realm_build(plan.clone(), Arc::clone(&realm)),
    )?;
    replicas.extend(more);
    let (more, _) = replicate_role(
        runtime,
        plan,
        Role::Social,
        social_build(plan.clone(), Arc::clone(&social)),
    )?;
    replicas.extend(more);
    let (more, _) = replicate_role(
        runtime,
        plan,
        Role::Matchmaking,
        matchmaking_build(plan.clone(), realm_at),
    )?;
    replicas.extend(more);
    let (more, _) = replicate_role(
        runtime,
        plan,
        Role::Ops,
        ops_build(
            plan.clone(),
            account_at,
            pkcs8.to_vec(),
            Arc::clone(inspected),
            Arc::clone(&ops),
        ),
    )?;
    replicas.extend(more);
    settle(runtime, &replicas)?;
    let roles = Roles {
        account: Current {
            first: first(&account, Role::Account)?,
            latest: account,
        },
        realm: Current {
            first: first(&realm, Role::Realm)?,
            latest: realm,
        },
        social: Current {
            first: first(&social, Role::Social)?,
            latest: social,
        },
        ops: Current {
            first: first(&ops, Role::Ops)?,
            latest: ops,
        },
    };
    Ok((roles, replicas))
}

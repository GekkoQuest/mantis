//! Ops: audited operator commands, signed live changes, and a read-only
//! inspector, behind a dashboard on its own HTTPS listener
//! ([`dashboard`]).
//!
//! Every command writes its audit row **before** it is dispatched; a
//! command whose row cannot be written is refused and never runs. After
//! dispatch the row is completed with the state before, the state after,
//! and how to undo it, or why there is no undo.
//!
//! Ops never sees game traffic: commands arrive only on the dashboard's
//! listener, which speaks HTTPS and nothing else, and reach the other
//! roles over the internal RPC as the `Ops` role, whose caller matrix
//! limits what it may call.

pub mod dashboard;
pub mod live;

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use mantis_core::wire::{BoundedArray, WireString};

use crate::generated::services as m;
use crate::host::RPC_TIMEOUT;
use crate::host::rpc::{Router, RpcClient, RpcError};
use crate::host::{lock, now_ms};
use crate::methods;
use crate::persist::{AuditRow, PersistService};
use live::LiveSigner;

/// One operator command.
#[derive(Clone, PartialEq, Debug)]
pub enum Command {
    /// Ban an account until a time (0 lifts the ban).
    Ban {
        /// The account.
        account: u64,
        /// Unix milliseconds the ban ends.
        until_ms: u64,
        /// Why.
        reason: String,
    },
    /// Maintenance mode on or off.
    Maintenance {
        /// On.
        on: bool,
    },
    /// A live flag: a module key (the whole module) or `<module key>.<flag>`.
    Flag {
        /// The flag.
        name: String,
        /// On.
        on: bool,
    },
    /// A live tunable.
    Tunable {
        /// The tunable's full name.
        name: String,
        /// Its value.
        value: f32,
    },
    /// Read one cell's summary (read-only).
    Inspect {
        /// The cell.
        cell: u64,
    },
    /// End a character's session on whichever cell host has it.
    Kick {
        /// The character.
        character: u64,
    },
    /// Maintenance drain: logins refused, cell hosts refuse new sessions and
    /// end the remaining ones after the grace period. `on: false` lifts it.
    Drain {
        /// Drain or lift.
        on: bool,
        /// Seconds before remaining sessions are ended.
        grace_seconds: u32,
    },
    /// Read a character's ledger rows (read-only).
    LedgerTrace {
        /// The character.
        character: u64,
    },
}

impl Command {
    /// The command's name in the audit trail.
    #[must_use]
    pub fn name(&self) -> &'static str {
        match self {
            Self::Ban { .. } => "ban",
            Self::Maintenance { .. } => "maintenance",
            Self::Flag { .. } => "flag",
            Self::Tunable { .. } => "tunable",
            Self::Inspect { .. } => "inspect",
            Self::Kick { .. } => "kick",
            Self::Drain { .. } => "drain",
            Self::LedgerTrace { .. } => "ledger",
        }
    }

    /// Its arguments, as the audit trail records them.
    #[must_use]
    pub fn args(&self) -> String {
        match self {
            Self::Ban {
                account,
                until_ms,
                reason,
            } => format!("account={account} until_ms={until_ms} reason={reason:?}"),
            Self::Maintenance { on } => format!("on={on}"),
            Self::Flag { name, on } => format!("name={name} on={on}"),
            Self::Tunable { name, value } => format!("name={name} value={value}"),
            Self::Inspect { cell } => format!("cell={cell}"),
            Self::Kick { character } | Self::LedgerTrace { character } => format!("character={character}"),
            Self::Drain { on, grace_seconds } => format!("on={on} grace_seconds={grace_seconds}"),
        }
    }

    fn check(&self) -> Result<(), String> {
        match self {
            Self::Flag { name, .. } | Self::Tunable { name, .. }
                if name.is_empty() || name.len() > 96 || name.chars().any(char::is_whitespace) =>
            {
                Err("names are 1 to 96 bytes without spaces".to_owned())
            }
            Self::Tunable { value, .. } if !value.is_finite() => Err("tunable values are finite".to_owned()),
            Self::Ban { reason, .. } if reason.len() > 128 => Err("reasons are at most 128 bytes".to_owned()),
            _ => Ok(()),
        }
    }
}

/// A command that ran.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Executed {
    /// Its audit row.
    pub audit: u64,
    /// State before.
    pub before: String,
    /// State after.
    pub after: String,
    /// How to undo it, or why there is no undo.
    pub undo: String,
}

/// Why a command did not run, or did not finish.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum OpsError {
    /// Malformed: refused before its audit row (nothing ran).
    Invalid(String),
    /// Its audit row could not be written, so it was not dispatched.
    Audit(String),
    /// It was dispatched and failed; the audit row says so.
    Failed {
        /// The audit row.
        audit: u64,
        /// Why.
        why: String,
    },
}

impl std::fmt::Display for OpsError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Invalid(why) => write!(f, "invalid: {why}"),
            Self::Audit(why) => write!(f, "not run: the audit row could not be written: {why}"),
            Self::Failed { audit, why } => write!(f, "failed (audit row {audit}): {why}"),
        }
    }
}

impl std::error::Error for OpsError {}

#[derive(Default)]
struct LiveState {
    changes: Vec<m::LiveChange>,
    current: BTreeMap<String, f32>,
}

/// The Ops role.
#[derive(Clone)]
pub struct OpsService {
    audit: PersistService,
    account: Arc<RpcClient>,
    cells: Arc<Mutex<BTreeMap<u64, Arc<RpcClient>>>>,
    signer: Arc<LiveSigner>,
    live: Arc<Mutex<LiveState>>,
}

impl OpsService {
    /// Ops with its audit store (the persistence writer's), a client of
    /// the account role (calling as `Ops`), and the live-data key.
    #[must_use]
    pub fn new(audit: PersistService, account: Arc<RpcClient>, signer: LiveSigner) -> Self {
        Self {
            audit,
            account,
            cells: Arc::default(),
            signer: Arc::new(signer),
            live: Arc::default(),
        }
    }

    /// Adds a cell host the inspector may read (a client calling as `Ops`).
    pub fn add_cell(&self, cell: u64, client: Arc<RpcClient>) {
        lock(&self.cells).insert(cell, client);
    }

    /// The live-data public key cells verify with.
    #[must_use]
    pub fn public_key(&self) -> Vec<u8> {
        self.signer.public_key()
    }

    /// The audit trail, oldest first.
    ///
    /// # Errors
    /// The store's failure.
    pub fn audit_rows(&self) -> Result<Vec<AuditRow>, String> {
        self.audit.with_store(|s| s.audit_rows()).map_err(|e| e.0)
    }

    /// Live changes after `since`, at most 32, oldest first.
    #[must_use]
    pub fn live_since(&self, since: u64) -> m::LiveChanges {
        let live = lock(&self.live);
        let next: Vec<m::LiveChange> = live
            .changes
            .iter()
            .filter(|c| c.seq > since)
            .take(32)
            .copied()
            .collect();
        m::LiveChanges {
            changes: BoundedArray::from_slice(&next).unwrap_or_default(),
        }
    }

    /// Runs one command for `actor`: audit row first, then dispatch, then
    /// the row's completion.
    ///
    /// # Errors
    /// [`OpsError`].
    pub async fn execute(&self, actor: &str, cmd: &Command) -> Result<Executed, OpsError> {
        cmd.check().map_err(OpsError::Invalid)?;
        let args = cmd.args();
        let audit = self
            .audit
            .with_store(|s| s.audit_begin(actor, cmd.name(), &args, now_ms()))
            .map_err(|e| OpsError::Audit(e.0))?;
        match self.dispatch(cmd).await {
            Ok((before, after, undo)) => {
                self.audit
                    .with_store(|s| s.audit_complete(audit, "done", &before, &after, &undo))
                    .map_err(|e| OpsError::Failed {
                        audit,
                        why: format!("it ran, but its audit row could not be completed: {}", e.0),
                    })?;
                Ok(Executed {
                    audit,
                    before,
                    after,
                    undo,
                })
            }
            Err(why) => {
                let _ = self.audit.with_store(|s| {
                    s.audit_complete(
                        audit,
                        "failed",
                        "unchanged",
                        "unchanged",
                        &format!("none: nothing changed ({why})"),
                    )
                });
                Err(OpsError::Failed { audit, why })
            }
        }
    }

    async fn dispatch(&self, cmd: &Command) -> Result<(String, String, String), String> {
        let rpc = |e: RpcError| e.to_string();
        match cmd {
            Command::Ban {
                account,
                until_ms,
                reason,
            } => {
                let req = m::Ban {
                    account: m::AccountId(*account),
                    until_ms: *until_ms,
                    reason: WireString::new(reason).unwrap_or_default(),
                };
                let prev = self
                    .account
                    .call::<methods::BanAccount>(&req, RPC_TIMEOUT)
                    .await
                    .map_err(rpc)?
                    .previous_until_ms;
                Ok((
                    format!("account={account} banned_until_ms={prev}"),
                    format!("account={account} banned_until_ms={until_ms}"),
                    format!("ban account={account} until_ms={prev}"),
                ))
            }
            Command::Maintenance { on } => {
                let was = self
                    .account
                    .call::<methods::Maintenance>(&m::SetMaintenance { on: *on }, RPC_TIMEOUT)
                    .await
                    .map_err(rpc)?
                    .was;
                Ok((
                    format!("maintenance={was}"),
                    format!("maintenance={on}"),
                    format!("maintenance on={was}"),
                ))
            }
            Command::Flag { name, on } => self.publish(name, live::FLAG, if *on { 1.0 } else { 0.0 }),
            Command::Tunable { name, value } => self.publish(name, live::TUNABLE, *value),
            Command::Kick { character } => self.kick(*character).await,
            Command::Drain { on, grace_seconds } => self.drain(*on, *grace_seconds).await,
            Command::LedgerTrace { character } => {
                let rows = self.ledger_trace(*character)?;
                let gold: i64 = rows.iter().filter(|r| r.item == 0).map(|r| r.delta).sum();
                let summary = format!("character={character} rows={} net_gold={gold}", rows.len());
                Ok((summary.clone(), summary, "none: read-only".to_owned()))
            }
            Command::Inspect { cell } => {
                let client = lock(&self.cells)
                    .get(cell)
                    .cloned()
                    .ok_or_else(|| format!("no cell {cell}"))?;
                let s = client
                    .call::<methods::InspectCell>(
                        &m::Inspect {
                            cell: m::CellNo(*cell),
                        },
                        RPC_TIMEOUT,
                    )
                    .await
                    .map_err(rpc)?;
                let summary = format!(
                    "cell={} tick={} state_hash={:016x} sessions={} entities={} cheats={}",
                    s.cell.0, s.tick, s.state_hash, s.sessions, s.entities, s.cheats
                );
                Ok((summary.clone(), summary, "none: read-only".to_owned()))
            }
        }
    }

    fn cell_client(&self, cell: u64) -> Result<Arc<RpcClient>, String> {
        lock(&self.cells)
            .get(&cell)
            .cloned()
            .ok_or_else(|| format!("no cell {cell}"))
    }

    /// A cell's system run times (the inspector; read-only telemetry, not
    /// audited: dashboards poll it).
    ///
    /// # Errors
    /// No such cell, or its host's refusal.
    pub async fn system_times(&self, cell: u64) -> Result<m::SystemTimes, String> {
        self.cell_client(cell)?
            .call::<methods::InspectSystemTimes>(
                &m::InspectSystems {
                    cell: m::CellNo(cell),
                },
                RPC_TIMEOUT,
            )
            .await
            .map_err(|e| e.to_string())
    }

    /// A cell's component names (the inspector; read-only, not audited).
    ///
    /// # Errors
    /// No such cell, or its host's refusal.
    pub async fn component_names(&self, cell: u64) -> Result<m::ComponentNames, String> {
        self.cell_client(cell)?
            .call::<methods::InspectComponentNames>(
                &m::InspectComponents {
                    cell: m::CellNo(cell),
                },
                RPC_TIMEOUT,
            )
            .await
            .map_err(|e| e.to_string())
    }

    /// A page of a cell's entities with `component`, every value as text.
    /// Audited: it reads simulation state (characters included).
    ///
    /// # Errors
    /// [`OpsError`]: a bad page size, the audit store, or the host's refusal.
    pub async fn entity_page(
        &self,
        actor: &str,
        cell: u64,
        component: &str,
        offset: u32,
        limit: u16,
    ) -> Result<m::EntityPage, OpsError> {
        if !(1..=methods::MAX_INSPECT_LIMIT).contains(&limit) {
            return Err(OpsError::Invalid("limit is 1 to 100".to_owned()));
        }
        let Some(name) = mantis_core::wire::WireString::new(component).filter(|_| !component.is_empty())
        else {
            return Err(OpsError::Invalid("component names are 1 to 64 bytes".to_owned()));
        };
        let args = format!("cell={cell} component={component} offset={offset} limit={limit}");
        let audit = self
            .audit
            .with_store(|s| s.audit_begin(actor, "inspect_entities", &args, now_ms()))
            .map_err(|e| OpsError::Audit(e.0))?;
        let req = m::InspectEntities {
            cell: m::CellNo(cell),
            component: name,
            offset,
            limit,
        };
        let result = match self.cell_client(cell) {
            Ok(c) => c
                .call::<methods::InspectEntityPage>(&req, RPC_TIMEOUT)
                .await
                .map_err(|e| e.to_string()),
            Err(e) => Err(e),
        };
        match result {
            Ok(page) => {
                let after = format!("total={} page={}", page.total, page.count);
                let _ = self
                    .audit
                    .with_store(|s| s.audit_complete(audit, "done", "unchanged", &after, "none: read-only"));
                Ok(page)
            }
            Err(why) => {
                let _ = self.audit.with_store(|s| {
                    s.audit_complete(audit, "failed", "unchanged", "unchanged", "none: read-only")
                });
                Err(OpsError::Failed { audit, why })
            }
        }
    }

    /// A character's ledger rows, oldest first (read-only).
    ///
    /// # Errors
    /// The store's failure.
    pub fn ledger_trace(&self, character: u64) -> Result<Vec<crate::persist::StoredLedger>, String> {
        self.audit.with_store(|s| s.ledger_of(character)).map_err(|e| e.0)
    }

    /// Every distinct cell host Ops knows.
    fn hosts(&self) -> Vec<Arc<RpcClient>> {
        let mut out: Vec<Arc<RpcClient>> = Vec::new();
        for c in lock(&self.cells).values() {
            if !out.iter().any(|o| o.addr() == c.addr()) {
                out.push(Arc::clone(c));
            }
        }
        out
    }

    async fn kick(&self, character: u64) -> Result<(String, String, String), String> {
        let req = m::KickCharacter {
            character: m::CharacterId(character),
        };
        let mut found = false;
        for host in self.hosts() {
            if let Ok(k) = host.call::<methods::Kick>(&req, RPC_TIMEOUT).await {
                found |= k.found;
            }
        }
        if !found {
            return Err(format!("character {character} is on no cell host"));
        }
        Ok((
            format!("character={character} online"),
            format!("character={character} session ended at the host's next tick"),
            "none: an ended session cannot be restored; the player may log in again".to_owned(),
        ))
    }

    async fn drain(&self, on: bool, grace_seconds: u32) -> Result<(String, String, String), String> {
        let was = self
            .account
            .call::<methods::Maintenance>(&m::SetMaintenance { on }, RPC_TIMEOUT)
            .await
            .map_err(|e| e.to_string())?
            .was;
        let req = m::DrainHost {
            on,
            grace_ms: grace_seconds.saturating_mul(1000),
        };
        let mut hosts = 0;
        for host in self.hosts() {
            host.call::<methods::Drain>(&req, RPC_TIMEOUT)
                .await
                .map_err(|e| format!("host {}: {e}", host.addr()))?;
            hosts += 1;
        }
        let undo = if on {
            "drain on=false grace_seconds=0 (sessions already ended are not restored)".to_owned()
        } else {
            format!("drain on=true grace_seconds={grace_seconds}")
        };
        Ok((
            format!("maintenance={was}"),
            format!(
                "maintenance={on}, {hosts} cell hosts {}",
                if on { "draining" } else { "admitting" }
            ),
            undo,
        ))
    }

    /// Signs and queues one live change for the cells.
    fn publish(&self, name: &str, kind: u8, value: f32) -> Result<(String, String, String), String> {
        let mut live = lock(&self.live);
        let seq = live.changes.last().map_or(1, |c| c.seq + 1);
        let change = self.signer.sign(seq, name, kind, value)?;
        let what = if kind == live::FLAG { "flag" } else { "tunable" };
        let show = |v: f32| {
            if kind == live::FLAG {
                (v != 0.0).to_string()
            } else {
                v.to_string()
            }
        };
        let prev = live.current.insert(name.to_owned(), value);
        // Every change is kept: a cell that starts later applies them all,
        // in order, and so reaches the current values.
        live.changes.push(change);
        let before = prev.map_or_else(
            || format!("{what} {name}: package default"),
            |p| format!("{what} {name}={}", show(p)),
        );
        let undo = prev.map_or_else(
            || {
                format!(
                    "none: {name} had no live value, so its package default applied; \
                     set it to the default in the package manifest to restore it"
                )
            },
            |p| format!("{what} name={name} value={}", show(p)),
        );
        Ok((
            before,
            format!("{what} {name}={} (live seq {seq})", show(value)),
            undo,
        ))
    }

    /// The role's RPC methods: cells poll live changes.
    #[must_use]
    pub fn router(&self) -> Router {
        let mut r = Router::validated(methods::validate);
        let me = self.clone();
        r.serve::<methods::Live>(move |_, req| Ok(me.live_since(req.since)));
        r
    }
}

//! Reconnecting a dropped session ([`Reconnector`]): a disconnect or a retryable refusal
//! becomes "reconnecting…" with backoff, never a dead client.
//!
//! The reconnector watches the [`NativeSession`]. Once the session was accepted:
//!
//! - **A dropped connection** (the transport closed) keeps the newest resume ticket and
//!   asks for a new connection after a backoff ([`ReconnectStep::Connect`]); the caller
//!   connects to the gateway again and calls [`NativeSession::reconnect`] with the token.
//! - **The token** is the resume ticket while it is valid (`valid_for` after the
//!   disconnect), else the launcher's entry token.
//! - **Refusals:** `Standby` (no host serves the cell yet) and `Full` and `Maintenance`
//!   retry with the same token after the next backoff; `StaleEpoch` and `BadToken` mean
//!   the ticket is spent or retired, so the next attempt uses the launcher token; a
//!   version, content, or module mismatch cannot get better by retrying and is reported
//!   ([`ReconnectStatus::Failed`]).
//! - **Backoff:** starts at [`ReconnectPolicy::first`], doubles per failed attempt, capped
//!   at [`ReconnectPolicy::max`]; an attempt with no answer for
//!   [`ReconnectPolicy::answer_within`] counts as failed. It keeps trying.
//!
//! Before the first acceptance nothing is retried: a refused first connect is the
//! launcher's to report.
//!
//! The status reaches the player through [`SharedStatus`]: the network thread sets it,
//! and the UI layer shows [`ReconnectStatus::notice`] while it is not live
//! ([`crate::ui_layer::UiLayer::set_connection`]).

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, PoisonError};
use std::time::Duration;

use mantis_adapter_contract::{RefuseReason, Transport};

use crate::net::{NativeSession, ResumeTicket, SessionState};
use crate::time::HostInstant;

/// Backoff tuning.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct ReconnectPolicy {
    /// The first wait.
    pub first: Duration,
    /// The longest wait.
    pub max: Duration,
    /// How long an attempt may go unanswered before it counts as failed.
    pub answer_within: Duration,
}

impl Default for ReconnectPolicy {
    /// 250 ms doubling to 4 s; 5 s to answer.
    fn default() -> Self {
        Self {
            first: Duration::from_millis(250),
            max: Duration::from_secs(4),
            answer_within: Duration::from_secs(5),
        }
    }
}

/// Where the connection is, for the player.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum ReconnectStatus {
    /// Connected and accepted (or not yet accepted the first time).
    #[default]
    Live,
    /// Reconnecting: this attempt number, since this instant.
    Reconnecting {
        /// Attempts made so far (1 for the first).
        attempt: u32,
        /// When the connection dropped.
        since: HostInstant,
    },
    /// A refusal retrying cannot fix.
    Failed(RefuseReason),
}

impl ReconnectStatus {
    /// What to tell the player, or `None` while live.
    pub fn notice(&self) -> Option<String> {
        match self {
            Self::Live => None,
            Self::Reconnecting { attempt, .. } => {
                Some(format!("Connection lost: reconnecting… (attempt {attempt})"))
            }
            Self::Failed(r) => Some(format!("Connection refused: {}", refusal_text(*r))),
        }
    }
}

/// A refusal in words.
pub fn refusal_text(reason: RefuseReason) -> &'static str {
    match reason {
        RefuseReason::VersionMismatch => "this client's protocol version is not supported; update the client",
        RefuseReason::ContentMismatch => {
            "this client's game content differs from the server's; update the client"
        }
        RefuseReason::ModuleRefused => "a module this client announced is not permitted",
        RefuseReason::Full => "the server is full",
        RefuseReason::BadToken => "the session token is invalid or already used",
        RefuseReason::Maintenance => "the server is in maintenance",
        RefuseReason::Standby => "no server is serving this area right now",
        RefuseReason::StaleEpoch => "the resume ticket was replaced by a newer one",
        _ => "refused",
    }
}

/// The reconnect status shared between the thread that drives the session and the one
/// that shows it. Setting an unchanged status does nothing; readers poll
/// [`SharedStatus::version`] and read only when it moved.
#[derive(Debug, Default)]
pub struct SharedStatus {
    version: AtomicU64,
    status: Mutex<ReconnectStatus>,
}

impl SharedStatus {
    /// A live status.
    pub fn new() -> Self {
        Self::default()
    }

    /// Sets the status (bumping the version when it changed).
    pub fn set(&self, status: ReconnectStatus) {
        let mut s = self.status.lock().unwrap_or_else(PoisonError::into_inner);
        if *s != status {
            *s = status;
            self.version.fetch_add(1, Ordering::Release);
        }
    }

    /// The status.
    pub fn get(&self) -> ReconnectStatus {
        *self.status.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Changes so far.
    pub fn version(&self) -> u64 {
        self.version.load(Ordering::Acquire)
    }
}

/// What to do now.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum ReconnectStep {
    /// Nothing.
    Nothing,
    /// Connect to the gateway again and call [`NativeSession::reconnect`] with `token`.
    Connect {
        /// The resume ticket, or the launcher's entry token.
        token: Vec<u8>,
    },
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Phase {
    Live,
    Waiting { until: HostInstant },
    Connecting { since: HostInstant },
    Failed(RefuseReason),
}

/// The reconnect driver for one session.
#[derive(Debug)]
pub struct Reconnector {
    policy: ReconnectPolicy,
    launcher_token: Vec<u8>,
    phase: Phase,
    accepted_once: bool,
    attempt: u32,
    dropped_at: Option<HostInstant>,
    ticket: Option<ResumeTicket>,
    reconnects: u64,
}

fn retryable(r: RefuseReason) -> bool {
    !matches!(
        r,
        RefuseReason::VersionMismatch | RefuseReason::ContentMismatch | RefuseReason::ModuleRefused
    )
}

impl Reconnector {
    /// A driver falling back to `launcher_token` when no resume ticket is valid.
    pub fn new(policy: ReconnectPolicy, launcher_token: &[u8]) -> Self {
        Self {
            policy,
            launcher_token: launcher_token.to_vec(),
            phase: Phase::Live,
            accepted_once: false,
            attempt: 0,
            dropped_at: None,
            ticket: None,
            reconnects: 0,
        }
    }

    /// Where the connection is.
    pub fn status(&self) -> ReconnectStatus {
        match self.phase {
            Phase::Live => ReconnectStatus::Live,
            Phase::Failed(r) => ReconnectStatus::Failed(r),
            Phase::Waiting { .. } | Phase::Connecting { .. } => ReconnectStatus::Reconnecting {
                attempt: self.attempt,
                since: self.dropped_at.unwrap_or(HostInstant::ZERO),
            },
        }
    }

    /// Successful reconnects so far.
    pub fn reconnects(&self) -> u64 {
        self.reconnects
    }

    fn backoff(&self) -> Duration {
        let shift = self.attempt.saturating_sub(1).min(16);
        self.policy
            .first
            .saturating_mul(1u32 << shift)
            .min(self.policy.max)
    }

    fn fail_attempt(&mut self, now: HostInstant) {
        self.attempt = self.attempt.saturating_add(1);
        self.phase = Phase::Waiting {
            until: now.saturating_add(self.backoff()),
        };
    }

    /// One step: watches `session` at `now` and says whether to connect again.
    pub fn step<T: Transport>(&mut self, now: HostInstant, session: &mut NativeSession<T>) -> ReconnectStep {
        match (session.state(), self.phase) {
            (_, Phase::Failed(_)) => ReconnectStep::Nothing,
            (SessionState::Welcomed { .. }, phase) => {
                if matches!(phase, Phase::Connecting { .. }) {
                    self.reconnects += 1;
                }
                self.accepted_once = true;
                self.phase = Phase::Live;
                self.attempt = 0;
                self.dropped_at = None;
                ReconnectStep::Nothing
            }
            (_, _) if !self.accepted_once => ReconnectStep::Nothing,
            (SessionState::Connecting, Phase::Connecting { since }) => {
                if now.saturating_since(since) > self.policy.answer_within {
                    self.fail_attempt(now);
                }
                ReconnectStep::Nothing
            }
            (SessionState::Refused(r), Phase::Connecting { .. }) => {
                if !retryable(r) {
                    self.phase = Phase::Failed(r);
                    return ReconnectStep::Nothing;
                }
                if matches!(r, RefuseReason::StaleEpoch | RefuseReason::BadToken) {
                    // The ticket is spent or retired: the next attempt uses the launcher's.
                    self.ticket = None;
                }
                self.fail_attempt(now);
                ReconnectStep::Nothing
            }
            (SessionState::Closed, Phase::Connecting { .. }) => {
                self.fail_attempt(now);
                ReconnectStep::Nothing
            }
            (SessionState::Closed | SessionState::Refused(_), Phase::Live) => {
                // The connection dropped: keep the newest ticket and start over.
                self.dropped_at = Some(now);
                self.ticket = session.take_resume_ticket();
                self.fail_attempt(now);
                ReconnectStep::Nothing
            }
            (_, Phase::Waiting { until }) if now >= until => {
                self.phase = Phase::Connecting { since: now };
                let valid = self.ticket.filter(|t| {
                    self.dropped_at
                        .is_some_and(|d| now.saturating_since(d) < t.valid_for)
                });
                let token = valid.map_or_else(|| self.launcher_token.clone(), |t| t.token().to_vec());
                ReconnectStep::Connect { token }
            }
            _ => ReconnectStep::Nothing,
        }
    }
}

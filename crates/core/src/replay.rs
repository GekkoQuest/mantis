//! Replay: run the core against a recorded log and assert the state hash of
//! every tick (plan 6.8, decision 0007). Divergence is an error, treated like
//! a crash.
//!
//! Replay is for one build: [`crate::log::LogReader::open`] already refuses a
//! log from another build or against other content.
//!
//! The protocol mirrors recording. For each tick `t`, starting at the header's
//! `start_tick` with no gaps:
//! 1. every `Intent`, `Seed`, `Command`, and `Outcome` record for `t` is
//!    handed to the simulation in log order (intents, seeds, and commands are
//!    inputs; outcomes are checked against what the simulation derived);
//! 2. `TickEnd(t)` makes the simulation run tick `t` and compares its state
//!    hash with the recorded one.

use core::fmt;

use crate::log::{LogEntry, LogError, LogReader, LogSchema, SessionId};
use crate::time::Tick;

/// A simulation that can be driven from its log.
pub trait Replayable {
    /// The log record types.
    type Schema: LogSchema;
    /// The simulation's own failure type.
    type Error;

    /// Delivers an intent for the coming tick.
    ///
    /// # Errors
    /// The simulation's error.
    fn apply_intent(
        &mut self,
        tick: Tick,
        session: SessionId,
        intent: &<Self::Schema as LogSchema>::Intent,
    ) -> Result<(), Self::Error>;

    /// Delivers injected entropy for the coming tick.
    ///
    /// # Errors
    /// The simulation's error.
    fn apply_seed(&mut self, tick: Tick, seed: u64) -> Result<(), Self::Error>;

    /// Delivers an economy command and **executes it**. Economy commands run
    /// on delivery, transactionally, so that their outcome can be logged and
    /// made durable before the acknowledgement (decision 0007). The outcome
    /// is therefore known before [`Replayable::check_outcome`] is called.
    ///
    /// # Errors
    /// The simulation's error.
    fn apply_command(
        &mut self,
        tick: Tick,
        command: &<Self::Schema as LogSchema>::Command,
    ) -> Result<(), Self::Error>;

    /// Checks a recorded economy outcome against what the simulation derived.
    ///
    /// # Errors
    /// The simulation's error when the outcome differs.
    fn check_outcome(
        &mut self,
        tick: Tick,
        outcome: &<Self::Schema as LogSchema>::Outcome,
    ) -> Result<(), Self::Error>;

    /// Runs tick `tick`.
    ///
    /// # Errors
    /// The simulation's error.
    fn step(&mut self, tick: Tick) -> Result<(), Self::Error>;

    /// The world state hash after the last step.
    fn state_hash(&self) -> u64;
}

/// What a replay covered.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct ReplayReport {
    /// Ticks replayed and verified.
    pub ticks: u64,
    /// The last verified tick.
    pub last_tick: Option<Tick>,
    /// True when the log ended in an incomplete record (a crash mid-write).
    /// Everything before it was verified.
    pub truncated_tail: bool,
    /// Records after the last `TickEnd` (an unfinished tick); not stepped.
    pub trailing_records: u64,
}

/// Replay failed.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ReplayError<E> {
    /// The log could not be read.
    Log(LogError),
    /// The simulation failed at `tick`.
    Sim {
        /// The tick.
        tick: Tick,
        /// The simulation's error.
        error: E,
    },
    /// The state hash after `tick` differs from the recording.
    Divergence {
        /// The first diverging tick.
        tick: Tick,
        /// The recorded hash.
        expected: u64,
        /// The replayed hash.
        actual: u64,
    },
    /// A record for a tick other than the one being assembled.
    TickOrder {
        /// The tick being assembled.
        expected: Tick,
        /// The record's tick.
        got: Tick,
    },
}

impl<E: fmt::Debug> fmt::Display for ReplayError<E> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Log(e) => write!(f, "replay: {e}"),
            Self::Sim { tick, error } => write!(f, "replay: simulation failed at {tick}: {error:?}"),
            Self::Divergence {
                tick,
                expected,
                actual,
            } => write!(
                f,
                "replay diverged at {tick}: recorded {expected:#018x}, replayed {actual:#018x}"
            ),
            Self::TickOrder { expected, got } => {
                write!(f, "replay: record for {got} while assembling {expected}")
            }
        }
    }
}

impl<E: fmt::Debug> std::error::Error for ReplayError<E> {}

/// Replays `reader` into `sim`, verifying every tick.
///
/// # Errors
/// The first [`ReplayError`]. A [`LogError::TruncatedTail`] is not an error:
/// it ends the replay and is reported in [`ReplayReport::truncated_tail`].
pub fn replay<R: Replayable>(
    sim: &mut R,
    reader: &mut LogReader<'_, R::Schema>,
) -> Result<ReplayReport, ReplayError<R::Error>> {
    replay_after(sim, reader, None)
}

/// Replays the records of `reader` after tick `after` into `sim`, which
/// already holds the state at the end of `after` (restored from a
/// snapshot, decision 0007); `None` replays from the log's start.
///
/// # Errors
/// As [`replay`].
pub fn replay_after<R: Replayable>(
    sim: &mut R,
    reader: &mut LogReader<'_, R::Schema>,
    after: Option<Tick>,
) -> Result<ReplayReport, ReplayError<R::Error>> {
    let mut report = ReplayReport::default();
    let mut current = after.map_or(reader.header().start_tick, Tick::next);
    let sim_err = |tick| move |error| ReplayError::Sim { tick, error };
    loop {
        let entry = match reader.next_entry() {
            Ok(Some(e)) => e,
            Ok(None) => break,
            Err(LogError::TruncatedTail { .. }) => {
                report.truncated_tail = true;
                break;
            }
            Err(e) => return Err(ReplayError::Log(e)),
        };
        let tick = entry.tick();
        if after.is_some_and(|a| tick <= a) {
            continue;
        }
        if tick != current {
            return Err(ReplayError::TickOrder {
                expected: current,
                got: tick,
            });
        }
        match entry {
            LogEntry::Intent { session, intent, .. } => {
                report.trailing_records += 1;
                sim.apply_intent(tick, session, &intent).map_err(sim_err(tick))?;
            }
            LogEntry::Seed { seed, .. } => {
                report.trailing_records += 1;
                sim.apply_seed(tick, seed).map_err(sim_err(tick))?;
            }
            LogEntry::Command { command, .. } => {
                report.trailing_records += 1;
                sim.apply_command(tick, &command).map_err(sim_err(tick))?;
            }
            LogEntry::Outcome { outcome, .. } => {
                report.trailing_records += 1;
                sim.check_outcome(tick, &outcome).map_err(sim_err(tick))?;
            }
            LogEntry::TickEnd { state_hash, .. } => {
                sim.step(tick).map_err(sim_err(tick))?;
                let actual = sim.state_hash();
                if actual != state_hash {
                    return Err(ReplayError::Divergence {
                        tick,
                        expected: state_hash,
                        actual,
                    });
                }
                report.ticks += 1;
                report.last_tick = Some(tick);
                report.trailing_records = 0;
                current = tick.next();
            }
        }
    }
    Ok(report)
}

//! Recovering cells after a crash or a deploy (decision 0007).
//!
//! - Same build: restore the newest snapshot, then replay the log after its
//!   tick, verifying every tick's state hash. Every outcome the log holds is
//!   pushed to the persistence writer again; batches are numbered by tick,
//!   so the ones already durable are no-ops and none is lost.
//! - Another build: the snapshot alone (a clean shutdown always ends in
//!   one). The old build's log segments are refused by their header.

use mantis_core::content::ContentHash;
use mantis_core::log::{BuildId, LogEntry, LogError, LogReader, LogWriter};
use mantis_core::replay::replay_after;
use mantis_core::time::Tick;
use mantis_server::cell::{BoxedSink, Cell};
use mantis_server::intent::CellLogSchema;
use mantis_services::cluster::CellOutcome;

use crate::tunables::Tunables;
use crate::world;

/// How a cell came back.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Recovered {
    /// The snapshot's tick.
    pub snapshot_tick: Tick,
    /// Ticks replayed from the log after it (the recovery time, in ticks).
    pub replayed: u64,
    /// The last tick the cell now holds.
    pub tick: Tick,
    /// The log was refused (another build): the snapshot alone was used.
    pub log_refused: bool,
    /// Bytes at the log's end discarded: a final record a crash tore, and
    /// the records of the tick it left incomplete. Recovery stops at the
    /// last complete tick.
    pub discarded_bytes: usize,
}

/// Recovers cell `index` from `snapshot` and its old `log`, as build
/// `build`. The cell then writes `new_log`.
///
/// # Errors
/// Why the cell cannot come back (a log that diverges, a snapshot that
/// does not fit).
pub fn recover_cell(
    t: &Tunables,
    index: usize,
    seed: u64,
    snapshot: &[u8],
    log: &[u8],
    build: BuildId,
    new_log: impl FnOnce(Tick) -> Option<LogWriter<CellLogSchema, BoxedSink>>,
) -> Result<(Cell, Recovered), String> {
    let mut cell =
        world::cell(t, index, seed, world::adapters(t.content), None).map_err(|e| format!("{e:?}"))?;
    let header = cell.restore(snapshot).map_err(|e| e.to_string())?;
    let mut recovered = Recovered {
        snapshot_tick: header.tick,
        replayed: 0,
        tick: header.tick,
        log_refused: false,
        discarded_bytes: 0,
    };
    match LogReader::<CellLogSchema>::open(log, build, t.content) {
        Ok(mut reader) => {
            let report = replay_after(&mut cell, &mut reader, Some(header.tick))
                .map_err(|e| format!("cell {index}: {e}"))?;
            recovered.replayed = report.ticks;
            recovered.tick = report.last_tick.unwrap_or(header.tick);
            if report.trailing_records > 0 || report.truncated_tail {
                // The tick after the last complete one never ended: none of
                // it runs, and its bytes are discarded.
                cell.discard_incomplete_tick();
                recovered.discarded_bytes = log.len().saturating_sub(complete_end(log, build, t)?);
            }
        }
        Err(LogError::BuildMismatch { .. }) if header.build != build => recovered.log_refused = true,
        Err(e) => return Err(format!("cell {index}: {e}")),
    }
    cell.set_log(new_log(recovered.tick.next()));
    Ok((cell, recovered))
}

/// Where the last complete tick of `log` ends (after its `TickEnd`), in
/// bytes; the header's end when no tick completed.
fn complete_end(log: &[u8], build: BuildId, t: &Tunables) -> Result<usize, String> {
    let mut reader = LogReader::<CellLogSchema>::open(log, build, t.content).map_err(|e| e.to_string())?;
    let mut end = reader.position();
    loop {
        match reader.next_entry() {
            Ok(Some(LogEntry::TickEnd { .. })) => end = reader.position(),
            Ok(Some(_)) => {}
            Ok(None) | Err(LogError::TruncatedTail { .. }) => return Ok(end),
            Err(e) => return Err(e.to_string()),
        }
    }
}

/// Writes `cell-<id>.snapshot` for every cell of `zone` into `dir`, each
/// beside its old one and then renamed: a crash never leaves half a
/// snapshot.
///
/// # Errors
/// The directory or a file cannot be written.
pub fn write_snapshots(
    dir: &std::path::Path,
    zone: &mantis_server::zone::Zone,
    t: &Tunables,
) -> Result<(), String> {
    std::fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    for cell in zone.cells() {
        let bytes = cell
            .snapshot(crate::build_id(), t.content)
            .map_err(|e| e.to_string())?;
        let path = dir.join(format!("cell-{}.snapshot", cell.id().0));
        let partial = path.with_extension("snapshot.partial");
        std::fs::write(&partial, &bytes).map_err(|e| format!("{}: {e}", partial.display()))?;
        std::fs::rename(&partial, &path).map_err(|e| format!("{}: {e}", path.display()))?;
    }
    Ok(())
}

/// Every outcome of every complete tick of a log, in order: what the
/// persistence writer must hold.
///
/// # Errors
/// The log cannot be read.
pub fn logged_outcomes(log: &[u8], build: BuildId, content: ContentHash) -> Result<Vec<CellOutcome>, String> {
    let mut reader = LogReader::<CellLogSchema>::open(log, build, content).map_err(|e| e.to_string())?;
    let mut done = Vec::new();
    let mut pending = Vec::new();
    loop {
        match reader.next_entry() {
            Ok(Some(LogEntry::Outcome { tick, outcome })) => {
                let bytes = outcome.payload.as_slice();
                let mut payload = [0u8; 512];
                for (to, from) in payload.iter_mut().zip(bytes) {
                    *to = *from;
                }
                pending.push(CellOutcome {
                    tick: tick.0,
                    kind: outcome.kind.0,
                    session: outcome.session.map_or(0, |s| s.0),
                    ok: outcome.result.is_ok(),
                    payload,
                    len: bytes.len().min(512),
                });
            }
            Ok(Some(LogEntry::TickEnd { .. })) => done.append(&mut pending),
            Ok(Some(_)) => {}
            Ok(None) | Err(LogError::TruncatedTail { .. }) => break,
            Err(e) => return Err(e.to_string()),
        }
    }
    Ok(done)
}

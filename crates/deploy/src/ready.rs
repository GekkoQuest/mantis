//! Readiness: a node serves only once every role it depends on is ready.
//!
//! The order follows from [`crate::matrix::dependencies`]: persist before
//! account, realm, social and Ops, persist and realm before matchmaking, and every service role a cell
//! host calls (realm, persist, social, matchmaking, Ops) before a cell
//! host. A dependency is ready when its health endpoint answers `/ready`
//! with 200 **and names the role and instance the registry lists there**,
//! so a registry pointing at the wrong process is caught, not trusted.

use std::time::{Duration, Instant};

use crate::health::{Status, probe};
use crate::matrix;
use crate::registry::Instance;

/// How often a dependency is asked.
pub const POLL: Duration = Duration::from_millis(100);

/// Whether one instance answers ready as itself (active or standby).
async fn instance_ready(dep: &Instance) -> Result<(), String> {
    let expect = format!("ready {} {}", matrix::name(dep.role), dep.name);
    match probe(&dep.health, "/ready", POLL.max(Duration::from_millis(500))).await {
        Ok((200, body)) if body.trim_end() == expect || body.starts_with(&format!("{expect} (")) => Ok(()),
        Ok((200, body)) => Err(format!(
            "{} answered as {:?}, not {expect:?}",
            dep.name,
            body.trim_end()
        )),
        Ok((code, body)) => Err(format!("{} {code} {}", dep.name, body.trim_end())),
        Err(e) => Err(format!("{}: {e}", dep.name)),
    }
}

/// Waits until every role in `deps` has an instance that is ready (each
/// entry: one role's instances; for a failover role any one of them, the
/// active or a standby), reporting what it waits for on `status`. Returns
/// how long it waited.
///
/// # Errors
/// A role has no ready instance within `timeout`, or a drain was requested.
pub async fn wait_for(
    deps: &[Vec<&Instance>],
    status: &Status,
    timeout: Duration,
    drain: &crate::drain::Drain,
) -> Result<Duration, String> {
    let start = Instant::now();
    for instances in deps {
        let Some(first) = instances.first() else {
            continue;
        };
        let names: Vec<String> = instances
            .iter()
            .map(|i| format!("{} at {}", i.name, i.health))
            .collect();
        status.set(
            crate::health::Phase::Starting,
            format!("waiting for {} ({})", matrix::name(first.role), names.join(", ")),
        );
        'role: loop {
            let mut last = Vec::new();
            for dep in instances {
                match instance_ready(dep).await {
                    Ok(()) => break 'role,
                    Err(e) => last.push(e),
                }
            }
            if let Some(why) = drain.requested() {
                return Err(format!("drained while starting ({why})"));
            }
            if start.elapsed() >= timeout {
                return Err(format!(
                    "no instance of {} was ready within {} s: {}",
                    matrix::name(first.role),
                    timeout.as_secs(),
                    last.join("; ")
                ));
            }
            tokio::time::sleep(POLL).await;
        }
    }
    Ok(start.elapsed())
}

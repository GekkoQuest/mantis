//! Readiness: a node serves only once every role it depends on is ready.
//!
//! The order follows from [`crate::matrix::dependencies`]: persist before
//! account, realm, social and Ops, realm before matchmaking, and every service role a cell
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

/// Waits until every instance in `deps` is ready, reporting what it waits
/// for on `status`. Returns how long it waited.
///
/// # Errors
/// A dependency is not ready within `timeout`, or a drain was requested.
pub async fn wait_for(
    deps: &[&Instance],
    status: &Status,
    timeout: Duration,
    drain: &crate::drain::Drain,
) -> Result<Duration, String> {
    let start = Instant::now();
    for dep in deps {
        let expect = format!("ready {} {}", matrix::name(dep.role), dep.name);
        status.set(
            crate::health::Phase::Starting,
            format!(
                "waiting for {} ({}) at {}",
                matrix::name(dep.role),
                dep.name,
                dep.health
            ),
        );
        loop {
            let last = match probe(&dep.health, "/ready", POLL.max(Duration::from_millis(500))).await {
                Ok((200, body)) if body.trim_end() == expect => break,
                Ok((200, body)) => format!("answered as {:?}, not {expect:?}", body.trim_end()),
                Ok((code, body)) => format!("{code} {}", body.trim_end()),
                Err(e) => e,
            };
            if let Some(why) = drain.requested() {
                return Err(format!("drained while starting ({why})"));
            }
            if start.elapsed() >= timeout {
                return Err(format!(
                    "{} ({}) at {} was not ready within {} s: {last}",
                    matrix::name(dep.role),
                    dep.name,
                    dep.health,
                    timeout.as_secs()
                ));
            }
            tokio::time::sleep(POLL).await;
        }
    }
    Ok(start.elapsed())
}

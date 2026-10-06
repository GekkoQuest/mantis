//! The clock service roles and cell-host links run on (principle 6: one
//! injected clock).
//!
//! Production runs on [`WallClock`]. Tests that must be deterministic run on
//! a [`ManualClock`] and advance it themselves: a cell host's link on a
//! manual clock is stepped (`CellLink::settle` gives its tasks their turns
//! in a fixed order once their clock time has come), so a test stepping
//! cells and the clock in turn counts the same ticks on every machine. RPC
//! timeouts stay on the runtime's timer: they bound waits on the network,
//! not behaviour.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// A future that completes after a clock's sleep.
pub type Sleep = Pin<Box<dyn Future<Output = ()> + Send + 'static>>;

/// A source of time and of sleeps.
pub trait Clock: Send + Sync + std::fmt::Debug {
    /// Unix milliseconds.
    fn now_ms(&self) -> u64;

    /// Completes once `d` of this clock's time has passed.
    fn sleep(&self, d: Duration) -> Sleep;

    /// Time moves only when a test moves it: whatever waits on this clock
    /// is stepped by the test.
    fn stepped(&self) -> bool {
        false
    }
}

/// Wall-clock time and the runtime's timer.
#[derive(Clone, Copy, Debug, Default)]
pub struct WallClock;

impl Clock for WallClock {
    fn now_ms(&self) -> u64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
    }

    fn sleep(&self, d: Duration) -> Sleep {
        Box::pin(tokio::time::sleep(d))
    }
}

/// Time that moves only when [`ManualClock::advance`] moves it.
#[derive(Clone, Debug)]
pub struct ManualClock {
    now: Arc<tokio::sync::watch::Sender<u64>>,
}

impl ManualClock {
    /// A clock reading `start_ms`.
    #[must_use]
    pub fn new(start_ms: u64) -> Self {
        Self {
            now: Arc::new(tokio::sync::watch::Sender::new(start_ms)),
        }
    }

    /// Moves time forward by `d`, waking every sleep it makes due.
    pub fn advance(&self, d: Duration) {
        let ms = u64::try_from(d.as_millis()).unwrap_or(u64::MAX);
        self.now.send_modify(|now| *now = now.saturating_add(ms));
    }
}

impl Clock for ManualClock {
    fn now_ms(&self) -> u64 {
        *self.now.borrow()
    }

    fn sleep(&self, d: Duration) -> Sleep {
        let deadline = self
            .now_ms()
            .saturating_add(u64::try_from(d.as_millis()).unwrap_or(u64::MAX));
        let mut rx = self.now.subscribe();
        Box::pin(async move {
            let _ = rx.wait_for(|now| *now >= deadline).await;
        })
    }

    fn stepped(&self) -> bool {
        true
    }
}

/// The clock a role or link runs on: shared, the wall clock by default.
#[derive(Clone, Debug)]
pub struct ServiceClock(pub Arc<dyn Clock>);

impl Default for ServiceClock {
    fn default() -> Self {
        Self(Arc::new(WallClock))
    }
}

impl ServiceClock {
    /// A shared manual clock.
    #[must_use]
    pub fn manual(clock: &ManualClock) -> Self {
        Self(Arc::new(clock.clone()))
    }

    /// Unix milliseconds.
    #[must_use]
    pub fn now_ms(&self) -> u64 {
        self.0.now_ms()
    }

    /// Completes once `d` of this clock's time has passed.
    #[must_use]
    pub fn sleep(&self, d: Duration) -> Sleep {
        self.0.sleep(d)
    }

    /// Whatever waits on this clock is stepped by a test.
    #[must_use]
    pub fn stepped(&self) -> bool {
        self.0.stepped()
    }
}

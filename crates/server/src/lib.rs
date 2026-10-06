//! mantis-server: the server host (plan 7).

#![forbid(unsafe_code)]

pub mod bots;
pub mod cell;
pub mod components;
pub mod gameplay;
pub mod harness;
pub mod host;
pub mod intent;
pub mod interest;
pub mod jobs;
pub mod lease;
pub mod limits;
pub mod modules;
pub mod movement;
pub mod replication;
pub mod rewind;
pub mod scripting;
pub mod service;
pub mod session;
pub mod simnet;
pub mod snapshot;
pub mod zone;

/// Locks `m`, recovering the data from a poisoned lock (a panicking job or
/// network thread must not take the cell down with it; the panic itself is
/// reported where it happened).
pub(crate) fn lock<T>(m: &std::sync::Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

//! Test helpers shared by unit tests. Tests return `TestResult` and use `?` instead of
//! `unwrap`, which the workspace lints deny everywhere.

use crate::core_api::TickRate;

/// Result type for tests.
pub type TestResult = Result<(), Box<dyn std::error::Error>>;

/// A tick rate, or an error for zero.
pub fn rate(hz: u32) -> Result<TickRate, Box<dyn std::error::Error>> {
    TickRate::new(hz).ok_or_else(|| "zero tick rate".into())
}

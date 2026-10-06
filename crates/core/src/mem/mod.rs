//! Hot-path memory: fixed-capacity containers, a per-tick arena, and pools
//! (plan 6.3).
//!
//! Every type here is sized at startup and **never grows afterwards**. When a
//! container is full, the operation fails and hands the value back; it never
//! silently reallocates. Together with the allocation harness in
//! `mantis-testkit`, this makes "zero engine allocation from `Movement`
//! through `Outbound`" a property that tests can check.
//!
//! All of it is safe Rust: handles are indices with epochs or generations,
//! never pointers, so a stale handle fails a lookup instead of reading reused
//! memory.

mod arena;
mod bounded;
mod pool;

pub use arena::{ArenaRef, ArenaSlice, TickArena};
pub use bounded::{BoundedVec, CapacityError};
pub use pool::{Pool, PoolHandle, Reset};

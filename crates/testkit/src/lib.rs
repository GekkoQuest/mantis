//! mantis-testkit: test tooling for the Mantis workspace.
//!
//! - [`alloc`]: the allocation harness that enforces zero engine allocation on
//!   the hot path (CLAUDE.md rule 4, plan section 6.3).
//! - [`arch`]: the dependency-arrow rules checked by `tests/architecture.rs`
//!   (plan section 5).
//!
//! This crate is a dev-dependency only; the architecture test forbids a
//! normal or build dependency on it. It is `deny(unsafe_code)` rather than
//! `forbid` because the counting allocator in [`alloc`] is the one unsafe
//! module decision 0015 permits.

#![deny(unsafe_code)]

// Decision 0015: the counting allocator is the one module of this crate that
// may contain unsafe code. The allow is scoped to exactly that module.
#[expect(unsafe_code)]
pub mod alloc;
pub mod arch;

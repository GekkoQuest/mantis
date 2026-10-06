//! Numerical tests of the mixer (no audio device): playback, spatialization, voice
//! stealing, buses and effects, and zero allocation on the hot path.
//!
//! One test binary so the counting allocator covers the zero-allocation test and the
//! support module is shared.

#![expect(
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation,
    clippy::indexing_slicing
)]

mod buses;
mod playback;
mod spatial;
mod stealing;
mod support;
mod zero_alloc;

#[global_allocator]
static ALLOC: mantis_testkit::alloc::CountingAllocator = mantis_testkit::alloc::CountingAllocator;

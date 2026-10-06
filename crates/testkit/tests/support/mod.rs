//! Shared fuzzing helpers: the iteration count and the mutator.

#![expect(clippy::cast_possible_truncation, clippy::indexing_slicing)]

use mantis_core::rng::Rng;

/// Iterations per message type: `MANTIS_FUZZ_ITERS` for long runs (the CI
/// fuzz job), 300 otherwise.
pub fn iterations() -> u32 {
    std::env::var("MANTIS_FUZZ_ITERS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(300)
}

/// The run's seed: `MANTIS_FUZZ_SEED` (decimal) when set, so a long run
/// explores new inputs and a failure is reproduced by setting it again;
/// `default` otherwise. Printed, so a failing run names it.
pub fn seed(default: u64) -> u64 {
    let s = std::env::var("MANTIS_FUZZ_SEED")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default);
    eprintln!("fuzz: seed {s} (reproduce with MANTIS_FUZZ_SEED={s})");
    s
}

/// One mutation of a valid encoding: a bit flip, a byte overwrite,
/// truncation, extension, an inflated length field, random garbage, or a
/// few inverted bytes.
pub fn mutate(rng: &mut Rng, valid: &[u8]) -> Vec<u8> {
    let mut b = valid.to_vec();
    match rng.below(7) {
        0 if !b.is_empty() => {
            let i = rng.below(b.len() as u32) as usize;
            b[i] ^= 1 << rng.below(8);
        }
        1 if !b.is_empty() => {
            let i = rng.below(b.len() as u32) as usize;
            b[i] = rng.next_u32() as u8;
        }
        2 => {
            let keep = rng.below(b.len() as u32 + 1) as usize;
            b.truncate(keep);
        }
        3 => {
            for _ in 0..=rng.below(8) {
                b.push(rng.next_u32() as u8);
            }
        }
        4 if b.len() >= 2 => {
            // Inflate a plausible length field.
            let i = rng.below(b.len() as u32 - 1) as usize;
            b[i] = 0xFF;
            b[i + 1] = 0xFF;
        }
        5 => {
            let n = rng.below(64) as usize;
            b = (0..n).map(|_| rng.next_u32() as u8).collect();
        }
        _ => {
            for _ in 0..=rng.below(4) {
                if !b.is_empty() {
                    let i = rng.below(b.len() as u32) as usize;
                    b[i] = !b[i];
                }
            }
        }
    }
    b
}

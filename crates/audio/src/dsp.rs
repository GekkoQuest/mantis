//! Signal-processing building blocks: linear ramps, the seeded generator, and bus effects.
//!
//! Presentation code, so `std` float math is used freely (the deterministic-math rule
//! binds simulation crates only).

use mantis_formats::mixer_graph::Effect;

/// Shortest fade for stops, steals, and bus gain changes, in milliseconds. Any gain change
/// is spread over at least this long so it never clicks.
pub const MIN_FADE_MS: f32 = 5.0;
/// Fade given to a voice that is stolen for a new one, in milliseconds.
pub const STEAL_FADE_MS: f32 = 5.0;
/// How long a voice takes to follow a change of its pan or distance gain, in milliseconds.
pub const GAIN_RAMP_MS: f32 = 5.0;
/// Longest fade an event may ask for, in milliseconds.
pub const MAX_FADE_MS: f32 = 60_000.0;

/// A sample rate or sample count (at most 2^24) as `f32`.
#[expect(clippy::cast_precision_loss)] // Callers pass rates and counts up to 2^24, exact in f32.
pub(crate) fn rate_f32(rate: u32) -> f32 {
    rate as f32
}

/// Samples in `ms` milliseconds at `rate`, at least 1. Non-finite or negative lengths
/// count as zero (then raised to 1).
#[expect(clippy::cast_possible_truncation, clippy::cast_sign_loss)] // Clamped to 1..=2^24 first.
pub(crate) fn samples_for_ms(ms: f32, rate: u32) -> u32 {
    let ms = if ms.is_finite() {
        ms.clamp(0.0, MAX_FADE_MS)
    } else {
        0.0
    };
    (ms * rate_f32(rate) / 1000.0).round().clamp(1.0, 16_777_216.0) as u32
}

/// A value that moves linearly to a target over a set number of samples.
#[derive(Clone, Copy, PartialEq, Debug)]
pub(crate) struct Ramp {
    value: f32,
    target: f32,
    step: f32,
    remaining: u32,
}

impl Ramp {
    /// Settled at `value`.
    pub(crate) fn new(value: f32) -> Ramp {
        Ramp {
            value,
            target: value,
            step: 0.0,
            remaining: 0,
        }
    }

    /// Starts moving from the current value to `target`, arriving after `samples` steps
    /// (immediately when `samples` is 0).
    pub(crate) fn set(&mut self, target: f32, samples: u32) {
        self.target = target;
        if samples == 0 {
            self.value = target;
            self.remaining = 0;
        } else {
            self.step = (target - self.value) / rate_f32(samples);
            self.remaining = samples;
        }
    }

    /// Advances one sample and returns the new value.
    pub(crate) fn next(&mut self) -> f32 {
        if self.remaining > 0 {
            self.remaining -= 1;
            self.value = if self.remaining == 0 {
                self.target
            } else {
                self.value + self.step
            };
        }
        self.value
    }

    /// Current value.
    pub(crate) fn value(&self) -> f32 {
        self.value
    }

    /// Where the ramp is heading.
    pub(crate) fn target(&self) -> f32 {
        self.target
    }

    /// True when the value has reached the target.
    pub(crate) fn is_settled(&self) -> bool {
        self.remaining == 0
    }
}

/// `SplitMix64`: small, fast, and fully determined by its seed.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Rng(u64);

impl Rng {
    pub(crate) fn new(seed: u64) -> Rng {
        Rng(seed)
    }

    pub(crate) fn next_u64(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        z ^ (z >> 31)
    }

    /// Uniform in `0..bound` (0 when `bound` is 0).
    #[expect(clippy::cast_possible_truncation)] // (x * bound) >> 64 < bound <= u32::MAX.
    pub(crate) fn below(&mut self, bound: u32) -> u32 {
        ((u128::from(self.next_u64()) * u128::from(bound)) >> 64) as u32
    }

    /// Uniform in `[0, 1)` with 24 bits of resolution.
    #[expect(clippy::cast_precision_loss)] // A 24-bit integer is exact in f32.
    pub(crate) fn unit(&mut self) -> f32 {
        (self.next_u64() >> 40) as f32 / 16_777_216.0
    }
}

/// Running state of one bus effect over interleaved stereo.
#[derive(Clone, Copy, PartialEq, Debug)]
pub(crate) enum EffectState {
    /// Linear gain.
    Gain(f32),
    /// One-pole low-pass: `y += a * (x - y)` per channel.
    LowPass { a: f32, y: [f32; 2] },
    /// One-pole high-pass: `x - lowpass(x)` per channel.
    HighPass { a: f32, y: [f32; 2] },
    /// Peak limiter with instant attack and exponential release of its envelope.
    Limiter { threshold: f32, release: f32, env: f32 },
}

impl EffectState {
    /// Fresh state for `effect` at `rate`.
    pub(crate) fn new(effect: Effect, rate: u32) -> EffectState {
        let fs = rate_f32(rate);
        let one_pole = |cutoff_hz: f32| 1.0 - (-core::f32::consts::TAU * cutoff_hz / fs).exp();
        match effect {
            Effect::Gain { db } => EffectState::Gain(10f32.powf(db / 20.0)),
            Effect::LowPass { cutoff_hz } => EffectState::LowPass {
                a: one_pole(cutoff_hz),
                y: [0.0; 2],
            },
            Effect::HighPass { cutoff_hz } => EffectState::HighPass {
                a: one_pole(cutoff_hz),
                y: [0.0; 2],
            },
            Effect::Limiter {
                threshold,
                release_ms,
            } => EffectState::Limiter {
                threshold,
                release: (-1000.0 / (release_ms * fs)).exp(),
                env: 0.0,
            },
        }
    }

    /// Processes interleaved stereo frames in place.
    pub(crate) fn process(&mut self, frames: &mut [[f32; 2]]) {
        match self {
            EffectState::Gain(g) => {
                for s in frames.iter_mut().flatten() {
                    *s *= *g;
                }
            }
            EffectState::LowPass { a, y } => {
                for frame in frames {
                    for (s, y) in frame.iter_mut().zip(y.iter_mut()) {
                        *y += *a * (*s - *y);
                        *s = *y;
                    }
                }
            }
            EffectState::HighPass { a, y } => {
                for frame in frames {
                    for (s, y) in frame.iter_mut().zip(y.iter_mut()) {
                        *y += *a * (*s - *y);
                        *s -= *y;
                    }
                }
            }
            EffectState::Limiter {
                threshold,
                release,
                env,
            } => {
                for frame in frames {
                    let peak = frame.iter().fold(0.0f32, |m, s| m.max(s.abs()));
                    *env = peak.max(*env * *release);
                    if *env > *threshold {
                        let g = *threshold / *env;
                        for s in frame.iter_mut() {
                            // The envelope is at least this frame's peak, so the scaled
                            // sample is within the threshold up to rounding; the clamp
                            // makes the ceiling exact.
                            *s = (*s * g).clamp(-*threshold, *threshold);
                        }
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    type TestResult = Result<(), Box<dyn std::error::Error>>;

    #[test]
    fn ramp_is_linear_and_lands_exactly() {
        let mut r = Ramp::new(1.0);
        r.set(0.0, 4);
        let got: Vec<f32> = (0..6).map(|_| r.next()).collect();
        assert_eq!(got, vec![0.75, 0.5, 0.25, 0.0, 0.0, 0.0]);
        assert!(r.is_settled());
        r.set(2.0, 0);
        assert_eq!((r.value(), r.target()), (2.0, 2.0));
    }

    #[test]
    fn samples_for_ms_rounds_and_floors_at_one() {
        assert_eq!(samples_for_ms(10.0, 48_000), 480);
        assert_eq!(samples_for_ms(0.0, 48_000), 1);
        assert_eq!(samples_for_ms(f32::NAN, 48_000), 1);
        assert_eq!(samples_for_ms(-3.0, 48_000), 1);
        assert_eq!(samples_for_ms(1e9, 8_000), 480_000);
    }

    #[test]
    fn rng_is_seeded_and_in_range() {
        let mut a = Rng::new(7);
        let mut b = Rng::new(7);
        for _ in 0..1000 {
            assert_eq!(a.next_u64(), b.next_u64());
            let x = a.unit();
            assert!((0.0..1.0).contains(&x));
            assert!(a.below(3) < 3);
            let _ = b.unit();
            let _ = b.below(3);
        }
        assert_eq!(Rng::new(1).below(0), 0);
    }

    #[test]
    fn limiter_never_exceeds_threshold() -> TestResult {
        let mut lim = EffectState::new(
            Effect::Limiter {
                threshold: 0.5,
                release_ms: 20.0,
            },
            48_000,
        );
        let mut rng = Rng::new(3);
        let mut frames: Vec<[f32; 2]> = (0..4096)
            .map(|_| [rng.unit() * 8.0 - 4.0, rng.unit() * 2.0 - 1.0])
            .collect();
        lim.process(&mut frames);
        let max = frames.iter().flatten().fold(0.0f32, |m, s| m.max(s.abs()));
        assert!(max <= 0.5, "{max}");
        assert!(max > 0.45, "the limiter still passes loud material: {max}");
        // Quiet material after release passes unchanged.
        let mut quiet = vec![[0.1f32, -0.1]; 48_000];
        lim.process(&mut quiet);
        let last = quiet.last().ok_or("frames")?;
        assert!((last[0] - 0.1).abs() < 1e-6);
        Ok(())
    }

    #[test]
    fn filters_pass_and_block_dc() -> TestResult {
        let mut lp = EffectState::new(Effect::LowPass { cutoff_hz: 1000.0 }, 48_000);
        let mut hp = EffectState::new(Effect::HighPass { cutoff_hz: 1000.0 }, 48_000);
        let mut a = vec![[1.0f32, -1.0]; 4800];
        let mut b = a.clone();
        lp.process(&mut a);
        hp.process(&mut b);
        let (la, lb) = (a.last().ok_or("a")?, b.last().ok_or("b")?);
        assert!((la[0] - 1.0).abs() < 1e-4 && (la[1] + 1.0).abs() < 1e-4);
        assert!(lb[0].abs() < 1e-4 && lb[1].abs() < 1e-4);
        let mut g = EffectState::new(Effect::Gain { db: -6.0 }, 48_000);
        let mut c = vec![[1.0f32, 1.0]];
        g.process(&mut c);
        assert!((c.first().ok_or("c")?[0] - 0.501_187).abs() < 1e-5);
        Ok(())
    }
}

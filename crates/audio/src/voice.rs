//! Clip playback: decoded clip data and one playing voice.

use mantis_formats::sound_bank::Clip;

use crate::dsp::Ramp;
use crate::ids::{EmitterId, VoiceHandle};

/// A clip decoded to `f32` once, at mixer construction.
#[derive(Clone, Debug)]
pub(crate) struct ClipData {
    channels: usize,
    frames: usize,
    samples: Vec<f32>,
}

impl ClipData {
    pub(crate) fn new(clip: &Clip) -> ClipData {
        ClipData {
            channels: usize::from(clip.channels.clamp(1, 2)),
            frames: clip.frames(),
            samples: clip.samples.to_f32(),
        }
    }

    /// Frame `i` as `[left, right]` (a mono clip feeds both); silence past the end.
    fn frame(&self, i: usize) -> [f32; 2] {
        if self.channels == 1 {
            let s = self.samples.get(i).copied().unwrap_or(0.0);
            [s, s]
        } else {
            let at = i * 2;
            [
                self.samples.get(at).copied().unwrap_or(0.0),
                self.samples.get(at + 1).copied().unwrap_or(0.0),
            ]
        }
    }

    /// The clip at fractional frame `pos` by linear interpolation. The frame after the
    /// last is frame 0 when looping (so the wrap is seamless) and silence otherwise.
    #[allow(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        clippy::cast_precision_loss
    )]
    // `pos` is non-negative and below `frames` (at most 2^26), so the floor fits usize and
    // converts back exactly.
    fn sample(&self, pos: f64, looping: bool) -> [f32; 2] {
        let i = pos as usize;
        let frac = (pos - i as f64) as f32;
        let a = self.frame(i);
        let next = i + 1;
        let b = if next < self.frames {
            self.frame(next)
        } else if looping {
            self.frame(0)
        } else {
            [0.0; 2]
        };
        [a[0] + (b[0] - a[0]) * frac, a[1] + (b[1] - a[1]) * frac]
    }
}

/// One playing (or fading-out) voice.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Voice {
    pub(crate) handle: VoiceHandle,
    /// Index into the mixer's sound table.
    pub(crate) sound: usize,
    /// Index into the mixer's clip table.
    pub(crate) clip: usize,
    /// Index into the mixer's bus table.
    pub(crate) bus: usize,
    /// Start order, for "oldest" stealing.
    pub(crate) serial: u64,
    pub(crate) priority: u8,
    pub(crate) emitter: Option<EmitterId>,
    pub(crate) position: Option<[f32; 3]>,
    /// Sound volume times the play's volume.
    pub(crate) volume: f32,
    pub(crate) looping: bool,
    /// Downmix stereo clips before panning.
    pub(crate) spatial: bool,
    /// Read position in clip frames.
    pub(crate) pos: f64,
    /// Clip frames advanced per output frame (pitch times rate ratio).
    pub(crate) step: f64,
    /// Per-channel pan and distance gain, ramped toward each block's target.
    pub(crate) gains: [Ramp; 2],
    /// Fade envelope, 1 while playing, ramping to 0 when stopping.
    pub(crate) fade: Ramp,
    pub(crate) stopping: bool,
    /// Pre-pan level times the fade, refreshed each block; used for stealing.
    pub(crate) audibility: f32,
}

impl Voice {
    /// Starts fading out over `samples`.
    pub(crate) fn stop(&mut self, samples: u32) {
        self.stopping = true;
        self.fade.set(0.0, samples);
    }

    /// Mixes this voice into `out`, ramping its channel gains toward `target` over
    /// `ramp` samples. Returns false once the voice has finished (clip end or fade out).
    #[allow(clippy::cast_precision_loss)] // Frame counts are at most 2^26, exact in f64.
    pub(crate) fn mix(&mut self, clip: &ClipData, target: [f32; 2], ramp: u32, out: &mut [[f32; 2]]) -> bool {
        if clip.frames == 0 {
            return false;
        }
        for (g, t) in self.gains.iter_mut().zip(target) {
            if g.target() != t {
                g.set(t, ramp);
            }
        }
        let frames = clip.frames as f64;
        for frame in out {
            let env = self.fade.next();
            let gl = self.gains[0].next() * env;
            let gr = self.gains[1].next() * env;
            let mut s = clip.sample(self.pos, self.looping);
            if self.spatial && clip.channels == 2 {
                let mono = s[0].midpoint(s[1]);
                s = [mono, mono];
            }
            frame[0] += s[0] * gl;
            frame[1] += s[1] * gr;
            self.pos += self.step;
            if self.pos >= frames {
                if !self.looping {
                    return false;
                }
                self.pos = self.pos.rem_euclid(frames);
            }
            if self.stopping && self.fade.is_settled() {
                return false;
            }
        }
        true
    }
}

#[cfg(test)]
mod tests {
    use mantis_formats::sound_bank::ClipSamples;

    use super::*;

    #[test]
    fn interpolates_and_wraps() {
        let clip = ClipData::new(&Clip {
            channels: 2,
            samples: ClipSamples::F32(vec![0.0, 1.0, 0.5, -1.0]),
        });
        assert_eq!(clip.sample(0.0, false), [0.0, 1.0]);
        assert_eq!(clip.sample(0.5, false), [0.25, 0.0]);
        // Past the last frame: toward silence, or toward frame 0 when looping.
        assert_eq!(clip.sample(1.5, false), [0.25, -0.5]);
        assert_eq!(clip.sample(1.5, true), [0.25, 0.0]);
        assert_eq!(clip.frame(9), [0.0, 0.0]);
    }
}

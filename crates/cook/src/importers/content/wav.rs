//! WAV (RIFF `WAVE`) decoding for the sound bank importer: 16-bit PCM or 32-bit IEEE
//! float, mono or stereo, including the `WAVE_FORMAT_EXTENSIBLE` spelling of those two.
//! Anything else is refused with a message naming what the file holds. Chunks other than
//! `fmt ` and `data` (`LIST`, `cue `, ...) are skipped.

use mantis_formats::sound_bank::{ClipSamples, MAX_CLIP_FRAMES};

/// A decoded WAV file.
#[derive(Clone, Debug, PartialEq)]
pub struct Wav {
    /// Frames per second.
    pub sample_rate: u32,
    /// 1 or 2.
    pub channels: u8,
    /// Interleaved samples.
    pub samples: ClipSamples,
}

const PCM: u16 = 1;
const FLOAT: u16 = 3;
const EXTENSIBLE: u16 = 0xFFFE;

fn u16_at(b: &[u8], at: usize) -> Option<u16> {
    b.get(at..at + 2)
        .and_then(|s| <[u8; 2]>::try_from(s).ok())
        .map(u16::from_le_bytes)
}

fn u32_at(b: &[u8], at: usize) -> Option<u32> {
    b.get(at..at + 4)
        .and_then(|s| <[u8; 4]>::try_from(s).ok())
        .map(u32::from_le_bytes)
}

struct Format {
    tag: u16,
    channels: u16,
    sample_rate: u32,
    byte_rate: u32,
    block_align: u16,
    bits: u16,
}

fn read_format(body: &[u8]) -> Result<Format, String> {
    let short = || format!("`fmt ` chunk of {} bytes is too short", body.len());
    let mut f = Format {
        tag: u16_at(body, 0).ok_or_else(short)?,
        channels: u16_at(body, 2).ok_or_else(short)?,
        sample_rate: u32_at(body, 4).ok_or_else(short)?,
        byte_rate: u32_at(body, 8).ok_or_else(short)?,
        block_align: u16_at(body, 12).ok_or_else(short)?,
        bits: u16_at(body, 14).ok_or_else(short)?,
    };
    if f.tag == EXTENSIBLE {
        // cbSize, valid bits, channel mask, then the sub-format GUID whose first two
        // bytes are the plain format tag.
        f.tag = u16_at(body, 24).ok_or_else(|| "extensible `fmt ` chunk is too short".to_owned())?;
    }
    Ok(f)
}

/// Decodes a WAV file.
///
/// # Errors
/// A message describing the first problem: not RIFF/WAVE, truncated, a missing `fmt ` or
/// `data` chunk, an encoding other than 16-bit PCM or 32-bit float, more than two
/// channels, inconsistent block sizes, no frames, or a float sample that is not finite
/// or outside -1 to 1.
pub fn decode(bytes: &[u8]) -> Result<Wav, String> {
    if bytes.get(0..4) != Some(b"RIFF".as_slice()) || bytes.get(8..12) != Some(b"WAVE".as_slice()) {
        return Err("not a RIFF WAVE file".to_owned());
    }
    let riff = u32_at(bytes, 4).ok_or("truncated RIFF header")? as usize;
    let end = riff.checked_add(8).filter(|e| *e <= bytes.len()).ok_or_else(|| {
        format!(
            "RIFF size {riff} runs past the end of the {}-byte file",
            bytes.len()
        )
    })?;
    let mut at = 12;
    let mut format = None;
    let mut data = None;
    while at + 8 <= end {
        let id = bytes.get(at..at + 4).unwrap_or(&[]);
        let size = u32_at(bytes, at + 4).ok_or("truncated chunk header")? as usize;
        let body = bytes
            .get(at + 8..at + 8 + size)
            .filter(|_| at + 8 + size <= end)
            .ok_or_else(|| {
                format!(
                    "chunk `{}` runs past the end of the file",
                    String::from_utf8_lossy(id)
                )
            })?;
        match id {
            b"fmt " => format = Some(read_format(body)?),
            b"data" => data = Some(body),
            _ => {}
        }
        // Chunks are padded to even sizes.
        at += 8 + size + (size & 1);
    }
    let f = format.ok_or("no `fmt ` chunk")?;
    let data = data.ok_or("no `data` chunk")?;
    if !(1..=2).contains(&f.channels) {
        return Err(format!(
            "{} channels; only mono and stereo are supported",
            f.channels
        ));
    }
    let width = match (f.tag, f.bits) {
        (PCM, 16) | (FLOAT, 32) => usize::from(f.bits / 8),
        (tag, bits) => {
            let kind = match tag {
                PCM => "integer PCM",
                FLOAT => "IEEE float",
                _ => "an unsupported format",
            };
            return Err(format!(
                "{kind} (format tag {tag}) with {bits} bits per sample; only 16-bit PCM and 32-bit IEEE float are supported"
            ));
        }
    };
    let block = width * usize::from(f.channels);
    if usize::from(f.block_align) != block
        || f.byte_rate != f.sample_rate.saturating_mul(u32::from(f.block_align))
    {
        return Err(format!(
            "inconsistent `fmt ` chunk: block align {} and byte rate {} for {} channels of {} bits at {} Hz",
            f.block_align, f.byte_rate, f.channels, f.bits, f.sample_rate
        ));
    }
    if data.is_empty() || data.len() % block != 0 {
        return Err(format!(
            "`data` holds {} bytes, not a positive whole number of {block}-byte frames",
            data.len()
        ));
    }
    let frames = data.len() / block;
    if frames > MAX_CLIP_FRAMES as usize {
        return Err(format!("{frames} frames; a clip has at most {MAX_CLIP_FRAMES}"));
    }
    let samples = if f.tag == PCM {
        ClipSamples::I16(
            data.as_chunks::<2>()
                .0
                .iter()
                .map(|b| i16::from_le_bytes(*b))
                .collect(),
        )
    } else {
        let mut v = Vec::with_capacity(frames * usize::from(f.channels));
        for (i, b) in data.as_chunks::<4>().0.iter().enumerate() {
            let s = f32::from_le_bytes(*b);
            if !s.is_finite() || !(-1.0..=1.0).contains(&s) {
                return Err(format!(
                    "float sample {i} is {s}; samples must be finite and within -1 to 1"
                ));
            }
            v.push(s);
        }
        ClipSamples::F32(v)
    };
    Ok(Wav {
        sample_rate: f.sample_rate,
        channels: u8::try_from(f.channels).unwrap_or(0),
        samples,
    })
}

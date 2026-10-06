//! Whole images to and from GPU blocks.
//!
//! Blocks are 4x4 texels in row-major block order. A partial block at the right or
//! bottom edge is padded by repeating the last column and row (clamped coordinates), so
//! padding never pulls the block's endpoints away from the real texels. BC4 stores the
//! red channel (gray images: the gray value) and BC5 red and green.

use mantis_formats::texture::Encoding;

use super::{bc1, bc4, bc5, bc7};

/// The encoder version recorded for `encoding` (0 for RGBA8, which is not encoded).
pub fn encoder_version(encoding: Encoding) -> u32 {
    match encoding {
        Encoding::Bc1 => bc1::ENCODER_VERSION,
        Encoding::Bc4 => bc4::ENCODER_VERSION,
        Encoding::Bc5 => bc5::ENCODER_VERSION,
        Encoding::Bc7 => bc7::ENCODER_VERSION,
        Encoding::Rgba8 => 0,
    }
}

fn at<T: Copy + Default>(data: &[T], width: u32, x: u32, y: u32) -> T {
    let i = u64::from(y) * u64::from(width) + u64::from(x);
    usize::try_from(i)
        .ok()
        .and_then(|i| data.get(i))
        .copied()
        .unwrap_or_default()
}

/// The 16 texels of block `(bx, by)`, edges clamped.
fn gather(texels: &[[u8; 4]], width: u32, height: u32, bx: u32, by: u32) -> [[u8; 4]; 16] {
    let mut block = [[0u8; 4]; 16];
    for (i, slot) in (0u32..).zip(block.iter_mut()) {
        let x = (bx * 4 + i % 4).min(width.saturating_sub(1));
        let y = (by * 4 + i / 4).min(height.saturating_sub(1));
        *slot = at(texels, width, x, y);
    }
    block
}

/// Encodes one `width x height` image of row-major RGBA texels. `alpha_cutoff` applies
/// to BC1 only.
pub fn encode_image(
    encoding: Encoding,
    width: u32,
    height: u32,
    texels: &[[u8; 4]],
    alpha_cutoff: Option<u8>,
) -> Vec<u8> {
    let capacity = usize::try_from(encoding.image_bytes(width, height)).unwrap_or(0);
    let mut out = Vec::with_capacity(capacity);
    if encoding == Encoding::Rgba8 {
        for t in texels {
            out.extend_from_slice(t);
        }
        return out;
    }
    for by in 0..height.div_ceil(4) {
        for bx in 0..width.div_ceil(4) {
            let block = gather(texels, width, height, bx, by);
            match encoding {
                Encoding::Bc1 => out.extend_from_slice(&bc1::encode_block(&block, alpha_cutoff)),
                Encoding::Bc4 => out.extend_from_slice(&bc4::encode_block(&block.map(|t| t[0]))),
                Encoding::Bc5 => {
                    out.extend_from_slice(&bc5::encode_block(&block.map(|t| t[0]), &block.map(|t| t[1])));
                }
                Encoding::Bc7 => out.extend_from_slice(&bc7::encode_block(&block)),
                Encoding::Rgba8 => {}
            }
        }
    }
    out
}

/// Decodes one image back to row-major RGBA texels (BC4 as `[r, 0, 0, 255]`, BC5 as
/// `[r, g, 0, 255]`). `None` when `bytes` has the wrong length or holds a BC7 mode the
/// reference decoder does not implement.
pub fn decode_image(encoding: Encoding, width: u32, height: u32, bytes: &[u8]) -> Option<Vec<[u8; 4]>> {
    if bytes.len() as u64 != encoding.image_bytes(width, height) {
        return None;
    }
    let count = usize::try_from(u64::from(width) * u64::from(height)).ok()?;
    if encoding == Encoding::Rgba8 {
        return Some(bytes.as_chunks::<4>().0.to_vec());
    }
    let mut out = vec![[0u8; 4]; count];
    let stride = if matches!(encoding, Encoding::Bc1 | Encoding::Bc4) {
        8
    } else {
        16
    };
    let blocks_wide = width.div_ceil(4);
    for (n, chunk) in (0u32..).zip(bytes.chunks_exact(stride)) {
        let decoded: [[u8; 4]; 16] = match (encoding, chunk) {
            (Encoding::Bc1, c) => bc1::decode_block(&c.try_into().ok()?),
            (Encoding::Bc4, c) => bc4::decode_block(&c.try_into().ok()?).map(|r| [r, 0, 0, 255]),
            (Encoding::Bc5, c) => {
                let block: [u8; 16] = c.try_into().ok()?;
                let (r, g) = bc5::decode_block(&block);
                let mut t = [[0u8; 4]; 16];
                for ((t, r), g) in t.iter_mut().zip(r).zip(g) {
                    *t = [r, g, 0, 255];
                }
                t
            }
            (Encoding::Bc7, c) => bc7::decode_block(&c.try_into().ok()?)?,
            _ => return None,
        };
        let (bx, by) = (n % blocks_wide, n / blocks_wide);
        for (i, texel) in (0u32..).zip(decoded) {
            let (x, y) = (bx * 4 + i % 4, by * 4 + i / 4);
            if x < width && y < height {
                let at = usize::try_from(u64::from(y) * u64::from(width) + u64::from(x)).ok()?;
                if let Some(slot) = out.get_mut(at) {
                    *slot = texel;
                }
            }
        }
    }
    Some(out)
}

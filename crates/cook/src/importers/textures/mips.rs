//! Mip chain generation.
//!
//! Each level halves the previous one (rounding down, at least 1 texel), the sizes of
//! [`mantis_formats::texture::mip_size`]. A texel of level `n + 1` is the box average of
//! the 2x2 texels `(2x, 2y)`, `(2x + 1, 2y)`, `(2x, 2y + 1)`, `(2x + 1, 2y + 1)` of level
//! `n`, with coordinates clamped to the level's last row and column (a 1-texel-wide
//! level averages its column with itself; the last column of an odd width is dropped,
//! as the halved size requires).
//!
//! Averaging runs on unquantized single-precision values carried from level to level:
//!
//! - **linear** data averages the 0-to-1 values;
//! - **sRGB** color decodes R, G, and B to linear light with the exact sRGB transfer
//!   function, averages, and re-encodes (alpha is linear);
//! - **normal maps** read R and G as X and Y in -1 to 1, reconstruct Z (`sqrt(1 - x² - y²)`,
//!   tangent space points +Z), average the vectors, and renormalize each texel.
//!
//! Level 0 is the source as is. Every value is quantized to 8 bits only for storage.
//! The sRGB curve is computed with in-crate `ln`/`exp` series on plain IEEE operations,
//! so the tables (and therefore the cooked bytes) are the same on every machine.

/// How the texels are interpreted while filtering.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Space {
    /// Every channel is linear.
    Linear,
    /// R, G, and B are sRGB-encoded; alpha is linear.
    Srgb,
    /// R and G hold a unit tangent-space normal's X and Y; B is written as its Z.
    Normal,
}

type W4 = [f32; 4];

/// The full set of `levels` levels of `width x height` texels (row-major RGBA) in
/// `space`, level 0 first. `levels` is at least 1 and at most the full chain length.
pub fn chain(width: u32, height: u32, texels: &[[u8; 4]], space: Space, levels: u32) -> Vec<Vec<[u8; 4]>> {
    let mut out = vec![texels.to_vec()];
    if levels <= 1 {
        return out;
    }
    let curve = SrgbCurve::new();
    let to_work = |t: [u8; 4]| curve.to_work(t, space);
    let (mut w, mut h) = (width, height);
    let mut current = downsample(w, h, |x, y| to_work(texel(texels, w, x, y)), space);
    for _ in 1..levels {
        (w, h) = ((w / 2).max(1), (h / 2).max(1));
        out.push(current.iter().map(|&v| curve.quantize(v, space)).collect());
        if out.len() >= usize::try_from(levels).unwrap_or(usize::MAX) {
            break;
        }
        let prev = current;
        current = downsample(w, h, |x, y| texel(&prev, w, x, y), space);
    }
    out
}

fn texel<T: Copy + Default>(data: &[T], width: u32, x: u32, y: u32) -> T {
    let at = u64::from(y) * u64::from(width) + u64::from(x);
    usize::try_from(at)
        .ok()
        .and_then(|i| data.get(i))
        .copied()
        .unwrap_or_default()
}

/// The next level down of a `width x height` level read through `fetch`.
fn downsample(width: u32, height: u32, fetch: impl Fn(u32, u32) -> W4, space: Space) -> Vec<W4> {
    let (nw, nh) = ((width / 2).max(1), (height / 2).max(1));
    let mut out = Vec::with_capacity(usize::try_from(u64::from(nw) * u64::from(nh)).unwrap_or(0));
    for y in 0..nh {
        let (y0, y1) = ((2 * y).min(height - 1), (2 * y + 1).min(height - 1));
        for x in 0..nw {
            let (x0, x1) = ((2 * x).min(width - 1), (2 * x + 1).min(width - 1));
            let quad = [fetch(x0, y0), fetch(x1, y0), fetch(x0, y1), fetch(x1, y1)];
            let mut avg = [0.0f32; 4];
            for (ch, slot) in avg.iter_mut().enumerate() {
                let get = |t: &W4| t.get(ch).copied().unwrap_or(0.0);
                let [top_left, top_right, bottom_left, bottom_right] = quad.each_ref().map(get);
                *slot = ((top_left + top_right) + (bottom_left + bottom_right)) * 0.25;
            }
            if space == Space::Normal {
                let normal = unit([avg[0], avg[1], avg[2]]);
                avg = [normal[0], normal[1], normal[2], avg[3]];
            }
            out.push(avg);
        }
    }
    out
}

/// `v` scaled to unit length; +Z for a zero vector.
fn unit(v: [f32; 3]) -> [f32; 3] {
    let len = (v[0] * v[0] + v[1] * v[1] + v[2] * v[2]).sqrt();
    if len > 1e-12 {
        v.map(|c| c / len)
    } else {
        [0.0, 0.0, 1.0]
    }
}

/// 8 bits to 0 to 1.
fn unorm(v: u8) -> f32 {
    f32::from(v) / 255.0
}

/// 0 to 1 to 8 bits, rounded.
fn to_unorm(v: f32) -> u8 {
    super::fit::round_clamp(v * 255.0, 255)
}

/// The sRGB transfer function as two tables: the linear value of each 8-bit code, and
/// the linear value at each midpoint between consecutive codes (so encoding is "how many
/// midpoints lie at or below", which is exactly `round(255 · srgb(linear))`).
pub struct SrgbCurve {
    decode: [f32; 256],
    midpoints: [f32; 255],
}

impl Default for SrgbCurve {
    fn default() -> Self {
        Self::new()
    }
}

impl SrgbCurve {
    /// Builds the tables.
    pub fn new() -> Self {
        let mut decode = [0.0f32; 256];
        for (code, slot) in (0u32..).zip(decode.iter_mut()) {
            *slot = narrow(srgb_to_linear(f64::from(code) / 255.0));
        }
        let mut midpoints = [0.0f32; 255];
        for (code, slot) in (0u32..).zip(midpoints.iter_mut()) {
            *slot = narrow(srgb_to_linear((f64::from(code) + 0.5) / 255.0));
        }
        Self { decode, midpoints }
    }

    /// The linear value of 8-bit sRGB `code`.
    pub fn decode(&self, code: u8) -> f32 {
        self.decode.get(usize::from(code)).copied().unwrap_or(0.0)
    }

    /// The nearest 8-bit sRGB code of `linear` (0 to 1, clamped).
    pub fn encode(&self, linear: f32) -> u8 {
        let n = self.midpoints.partition_point(|&m| m <= linear);
        u8::try_from(n).unwrap_or(u8::MAX)
    }

    fn to_work(&self, t: [u8; 4], space: Space) -> W4 {
        match space {
            Space::Linear => t.map(unorm),
            Space::Srgb => [
                self.decode(t[0]),
                self.decode(t[1]),
                self.decode(t[2]),
                unorm(t[3]),
            ],
            Space::Normal => {
                let nx = unorm(t[0]) * 2.0 - 1.0;
                let ny = unorm(t[1]) * 2.0 - 1.0;
                let nz = (1.0 - nx * nx - ny * ny).max(0.0).sqrt();
                let normal = unit([nx, ny, nz]);
                [normal[0], normal[1], normal[2], unorm(t[3])]
            }
        }
    }

    fn quantize(&self, v: W4, space: Space) -> [u8; 4] {
        match space {
            Space::Linear => v.map(to_unorm),
            Space::Srgb => [
                self.encode(v[0]),
                self.encode(v[1]),
                self.encode(v[2]),
                to_unorm(v[3]),
            ],
            Space::Normal => [
                to_unorm(v[0] * 0.5 + 0.5),
                to_unorm(v[1] * 0.5 + 0.5),
                to_unorm(v[2] * 0.5 + 0.5),
                to_unorm(v[3]),
            ],
        }
    }
}

#[expect(clippy::cast_possible_truncation)] // a value in 0 to 1 narrowed to single precision
fn narrow(v: f64) -> f32 {
    v as f32
}

/// The sRGB electro-optical transfer function (IEC 61966-2-1), `c` in 0 to 1.
pub fn srgb_to_linear(c: f64) -> f64 {
    if c <= 0.040_45 {
        c / 12.92
    } else {
        pow((c + 0.055) / 1.055, 2.4)
    }
}

/// The inverse transfer function, `l` in 0 to 1.
pub fn linear_to_srgb(l: f64) -> f64 {
    if l <= 0.003_130_8 {
        l * 12.92
    } else {
        1.055 * pow(l, 1.0 / 2.4) - 0.055
    }
}

/// `x^y` for `x` in (0, 1] and `y > 0`, as `exp(y · ln x)` with series accurate to
/// about 1e-15, using only IEEE add, multiply, and divide.
fn pow(x: f64, y: f64) -> f64 {
    if x <= 0.0 {
        return 0.0;
    }
    exp(y * ln(x))
}

/// Natural logarithm of a positive finite `x`.
fn ln(x: f64) -> f64 {
    // x = m · 2^k with m in [sqrt(1/2), sqrt(2)); scaling by 2 is exact.
    let (mut mantissa, mut exponent) = (x, 0.0f64);
    while mantissa < core::f64::consts::FRAC_1_SQRT_2 {
        mantissa *= 2.0;
        exponent -= 1.0;
    }
    while mantissa >= core::f64::consts::SQRT_2 {
        mantissa *= 0.5;
        exponent += 1.0;
    }
    // ln m = 2 · atanh(s), s = (m - 1) / (m + 1), |s| < 0.172.
    let s = (mantissa - 1.0) / (mantissa + 1.0);
    let s2 = s * s;
    let mut term = s;
    let mut sum = 0.0;
    let mut divisor = 1.0;
    for _ in 0..24 {
        sum += term / divisor;
        term *= s2;
        divisor += 2.0;
    }
    2.0 * sum + exponent * core::f64::consts::LN_2
}

/// `e^z` for `z <= 0` (and modest positive `z`).
fn exp(z: f64) -> f64 {
    // z = k · ln 2 + r with |r| <= ln 2 / 2; multiplying by 2^k is exact.
    let (mut r, mut k) = (z, 0i32);
    let half = core::f64::consts::LN_2 * 0.5;
    while r > half && k < 1100 {
        r -= core::f64::consts::LN_2;
        k += 1;
    }
    while r < -half && k > -1100 {
        r += core::f64::consts::LN_2;
        k -= 1;
    }
    let mut term = 1.0;
    let mut sum = 1.0;
    let mut n = 1.0;
    for _ in 0..24 {
        term *= r / n;
        sum += term;
        n += 1.0;
    }
    for _ in 0..k.unsigned_abs() {
        sum = if k > 0 { sum * 2.0 } else { sum * 0.5 };
    }
    sum
}

//! Shared block-fitting math for the BC encoders: four-component vectors, the principal
//! axis of a texel cloud by power iteration, and least-squares endpoint refinement.
//!
//! Everything is plain IEEE single-precision arithmetic (add, multiply, divide, square
//! root) in a fixed order with fixed iteration counts, so results are identical on every
//! machine. No `mul_add`, no SIMD, no library transcendentals.

/// A texel or endpoint as four floats on the 0 to 255 scale (RGBA; unused channels 0).
pub(crate) type V4 = [f32; 4];

/// Power iterations for the principal axis (fixed, so the result never depends on a
/// convergence test).
const POWER_ITERATIONS: usize = 8;

/// `a - b`.
pub(crate) fn sub(a: V4, b: V4) -> V4 {
    [a[0] - b[0], a[1] - b[1], a[2] - b[2], a[3] - b[3]]
}

/// `a + b * s`.
pub(crate) fn add_scaled(a: V4, b: V4, s: f32) -> V4 {
    [a[0] + b[0] * s, a[1] + b[1] * s, a[2] + b[2] * s, a[3] + b[3] * s]
}

/// Dot product.
pub(crate) fn dot(a: V4, b: V4) -> f32 {
    a[0] * b[0] + a[1] * b[1] + a[2] * b[2] + a[3] * b[3]
}

/// A texel as floats.
pub(crate) fn from_u8(t: [u8; 4]) -> V4 {
    t.map(f32::from)
}

/// `v` rounded to the nearest integer (halves up) and clamped to `0..=max`; NaN is 0.
#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)] // clamped to 0..=max first
pub(crate) fn round_clamp(v: f32, max: u8) -> u8 {
    let v = if v.is_nan() { 0.0 } else { v };
    (v.clamp(0.0, f32::from(max)) + 0.5).floor() as u8
}

/// A small integer that is known to fit in a byte, saturating otherwise.
pub(crate) fn byte(v: u32) -> u8 {
    u8::try_from(v).unwrap_or(u8::MAX)
}

/// Squared distance between two texels over the first `channels` channels.
pub(crate) fn texel_error(a: [u8; 4], b: [u8; 4], channels: usize) -> u32 {
    a.iter()
        .zip(b)
        .take(channels)
        .map(|(&x, y)| {
            let d = u32::from(x.abs_diff(y));
            d * d
        })
        .sum()
}

/// The two ends of the principal axis of `points`: `(high, low)`, where `high` has the
/// larger projection on the axis. The axis comes from power iteration on the covariance
/// matrix, starting from its row with the largest diagonal entry. A cloud without
/// variance (a solid block) returns its mean twice.
pub(crate) fn principal_endpoints(points: &[V4]) -> (V4, V4) {
    let mut count = 0.0f32;
    let mut sum = [0.0f32; 4];
    for &p in points {
        sum = add_scaled(sum, p, 1.0);
        count += 1.0;
    }
    if count == 0.0 {
        return ([0.0; 4], [0.0; 4]);
    }
    let mean = sum.map(|s| s / count);
    let mut cov = [[0.0f32; 4]; 4];
    for &p in points {
        let d = sub(p, mean);
        for (row, di) in cov.iter_mut().zip(d) {
            for (cell, dj) in row.iter_mut().zip(d) {
                *cell += di * dj;
            }
        }
    }
    let diagonal = [cov[0][0], cov[1][1], cov[2][2], cov[3][3]];
    let mut axis = cov[0];
    let mut largest = diagonal[0];
    for (row, d) in cov.iter().zip(diagonal) {
        if d > largest {
            largest = d;
            axis = *row;
        }
    }
    // Below this the block is solid for 8-bit purposes (sum of squares, not variance).
    if largest <= 1e-3 {
        return (mean, mean);
    }
    let Some(mut axis) = normalized(axis) else {
        return (mean, mean);
    };
    for _ in 0..POWER_ITERATIONS {
        match normalized(cov.map(|row| dot(row, axis))) {
            Some(next) => axis = next,
            None => break,
        }
    }
    let mut lo = f32::INFINITY;
    let mut hi = f32::NEG_INFINITY;
    for &p in points {
        let t = dot(sub(p, mean), axis);
        lo = lo.min(t);
        hi = hi.max(t);
    }
    (add_scaled(mean, axis, hi), add_scaled(mean, axis, lo))
}

fn normalized(v: V4) -> Option<V4> {
    let len = dot(v, v).sqrt();
    (len > 1e-20).then(|| v.map(|x| x / len))
}

/// The endpoints `(a, b)` that minimize the squared error of `point ≈ (1 - w)·a + w·b`
/// over `points` with their `weights`, clamped to 0 to 255; `None` when the system is
/// singular (every weight equal).
pub(crate) fn least_squares(points: &[V4], weights: &[f32]) -> Option<(V4, V4)> {
    let (mut aa, mut ab, mut bb) = (0.0f32, 0.0f32, 0.0f32);
    let mut ra = [0.0f32; 4];
    let mut rb = [0.0f32; 4];
    for (&p, &w) in points.iter().zip(weights) {
        let u = 1.0 - w;
        aa += u * u;
        ab += u * w;
        bb += w * w;
        ra = add_scaled(ra, p, u);
        rb = add_scaled(rb, p, w);
    }
    let det = aa * bb - ab * ab;
    if det.abs() <= 1e-6 {
        return None;
    }
    let inv = 1.0 / det;
    let mut a = [0.0f32; 4];
    let mut b = [0.0f32; 4];
    for (((a, b), ra), rb) in a.iter_mut().zip(b.iter_mut()).zip(ra).zip(rb) {
        *a = ((bb * ra - ab * rb) * inv).clamp(0.0, 255.0);
        *b = ((aa * rb - ab * ra) * inv).clamp(0.0, 255.0);
    }
    Some((a, b))
}

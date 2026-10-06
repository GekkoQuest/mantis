//! The lightmap baker: atlas packing, texel rasterization, and texel lighting.
//!
//! - **Rectangles**: each lightmapped placement gets a square of
//!   `ceil(sqrt(surface area) * lightmap_texels_per_meter)` texels (at least 2) for its
//!   whole `uv1` square, plus `lightmap_padding` texels on every side. A shelf packer
//!   (tallest first, ties by size then source order) places them in an atlas whose width
//!   is the smallest power of two that keeps the atlas no taller than wide, capped at
//!   `lightmap_max_size`; the height is what the shelves use. When nothing fits, every
//!   density is scaled by 0.8 until it does.
//! - **Layout**: `atlas_uv = uv1 * scale + offset` with `scale = side / atlas size` and
//!   `offset = rectangle interior corner / atlas size`. Atlas `v` grows downward with the
//!   rows, as texture coordinates do on the GPU.
//! - **Rasterization**: a texel belongs to the first triangle (mesh order) that contains
//!   its center, else, conservatively, to the overlapping triangle nearest its center
//!   (lit at the nearest point of that triangle). Texels no triangle overlaps stay empty
//!   and are then filled by `lightmap_padding` dilation passes (the mean of filled
//!   neighbors) inside the padded rectangle, so filtering never reads black.
//! - **Lighting** per texel and keyframe: direct sun with a shadow ray (when
//!   `lightmap_sun`), sky through `samples` cosine-distributed occlusion rays, and the
//!   one-bounce term ([`super::light`]), stored as irradiance divided by pi in
//!   `Rgb9e5Ufloat`.

use mantis_formats::lightmap::{Lightmap, rgb_to_rgb9e5};

use super::light::{OFFSET, incoming, sun, trace};
use super::math::{V3, cosine_directions, frame, to_frame};
use super::scene::{Receiver, ReceiverTriangle, Scene};
use super::settings::Settings;
use crate::importer::CookError;

/// A placed rectangle (texels, including padding).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Rect {
    /// Left column.
    pub x: u32,
    /// Top row.
    pub y: u32,
    /// Width.
    pub w: u32,
    /// Height.
    pub h: u32,
}

fn shelves(sizes: &[(u32, u32)], order: &[usize], width: u32) -> Option<(u32, Vec<Rect>)> {
    let mut rects = vec![
        Rect {
            x: 0,
            y: 0,
            w: 0,
            h: 0
        };
        sizes.len()
    ];
    let (mut cursor, mut shelf_top, mut shelf) = (0u32, 0u32, 0u32);
    for index in order {
        let (w, h) = sizes.get(*index).copied()?;
        if w > width {
            return None;
        }
        if cursor + w > width {
            shelf_top += shelf;
            (cursor, shelf) = (0, 0);
        }
        *rects.get_mut(*index)? = Rect {
            x: cursor,
            y: shelf_top,
            w,
            h,
        };
        cursor += w;
        shelf = shelf.max(h);
    }
    Some((shelf_top + shelf, rects))
}

/// Packs rectangles of `sizes` (width, height) into an atlas no larger than `max_edge`
/// on either side: `(width, height, rectangles)` in input order, or `None` when they do
/// not fit.
pub fn pack(sizes: &[(u32, u32)], max_edge: u32) -> Option<(u32, u32, Vec<Rect>)> {
    if sizes.is_empty() {
        return None;
    }
    let mut order: Vec<usize> = (0..sizes.len()).collect();
    let key = |index: usize| sizes.get(index).map_or((0, 0), |(w, h)| (*h, *w));
    order.sort_by(|first, second| key(*second).cmp(&key(*first)).then(first.cmp(second)));
    let area: u64 = sizes.iter().map(|(w, h)| u64::from(*w) * u64::from(*h)).sum();
    let widest = sizes.iter().map(|(w, _)| *w).max().unwrap_or(1);
    let mut width = 16u32;
    while u64::from(width) * u64::from(width) < area || width < widest {
        width = width.saturating_mul(2);
    }
    let mut candidates = Vec::new();
    while width < max_edge {
        candidates.push(width);
        width = width.saturating_mul(2);
    }
    candidates.push(max_edge);
    for w in candidates {
        if let Some((h, rects)) = shelves(sizes, &order, w)
            && h <= w.max(1).min(max_edge)
        {
            return Some((w, h.max(1), rects));
        }
    }
    None
}

/// The baked atlas and the layout of each lightmapped placement.
#[derive(Clone, Debug, PartialEq)]
pub struct Baked {
    /// The atlas.
    pub lightmap: Lightmap,
    /// Per placement, in source order: name and `[scale u, scale v, offset u, offset v]`.
    pub layout: Vec<(String, [f32; 4])>,
}

/// The texel that a triangle covers best: distance from the texel center (0 inside),
/// triangle index, barycentric weights.
type Coverage = Option<(f32, usize, [f32; 3])>;

type P2 = [f32; 2];

fn sub2(from: P2, to: P2) -> P2 {
    [from[0] - to[0], from[1] - to[1]]
}

fn cross2(first: P2, second: P2) -> f32 {
    first[0] * second[1] - second[0] * first[1]
}

fn barycentric(point: P2, tri: &[P2; 3]) -> Option<[f32; 3]> {
    let [ta, tb, tc] = *tri;
    let (edge_b, edge_c, rel) = (sub2(tb, ta), sub2(tc, ta), sub2(point, ta));
    let det = cross2(edge_b, edge_c);
    if det.abs() < 1e-12 {
        return None;
    }
    let wb = cross2(rel, edge_c) / det;
    let wc = cross2(edge_b, rel) / det;
    Some([1.0 - wb - wc, wb, wc])
}

/// The point of segment `start`-`end` nearest `point`, and its distance.
fn closest_on_segment(point: P2, start: P2, end: P2) -> (P2, f32) {
    let along = sub2(end, start);
    let len2 = along[0] * along[0] + along[1] * along[1];
    let rel = sub2(point, start);
    let frac = if len2 > 0.0 {
        ((rel[0] * along[0] + rel[1] * along[1]) / len2).clamp(0.0, 1.0)
    } else {
        0.0
    };
    let near = [start[0] + along[0] * frac, start[1] + along[1] * frac];
    let gap = sub2(point, near);
    (near, (gap[0] * gap[0] + gap[1] * gap[1]).sqrt())
}

/// Whether triangle `tri` and the texel square at `corner` (side 1) overlap with positive
/// area (separating axes: the square's axes and the triangle's edge normals).
fn overlaps(tri: &[P2; 3], corner: P2) -> bool {
    let [left, top] = corner;
    let corners = [
        [left, top],
        [left + 1.0, top],
        [left, top + 1.0],
        [left + 1.0, top + 1.0],
    ];
    let (mut lo, mut hi) = ([f32::MAX; 2], [f32::MIN; 2]);
    for vertex in tri {
        lo = [lo[0].min(vertex[0]), lo[1].min(vertex[1])];
        hi = [hi[0].max(vertex[0]), hi[1].max(vertex[1])];
    }
    if hi[0] <= left || lo[0] >= left + 1.0 || hi[1] <= top || lo[1] >= top + 1.0 {
        return false;
    }
    let [ta, tb, tc] = *tri;
    for (start, end, other) in [(ta, tb, tc), (tb, tc, ta), (tc, ta, tb)] {
        let edge = sub2(end, start);
        let side = |q: P2| cross2(edge, sub2(q, start));
        let inner = side(other);
        if corners.iter().all(|q| side(*q) * inner <= 0.0) {
            return false;
        }
    }
    true
}

/// Where a triangle (texel coordinates) covers the texel at `corner`: distance of the
/// texel center to the triangle (0 inside) and the barycentric weights of the lit point.
fn cover(tri: &[P2; 3], corner: P2) -> Option<(f32, [f32; 3])> {
    let center = [corner[0] + 0.5, corner[1] + 0.5];
    let weights = barycentric(center, tri)?;
    if weights.iter().all(|w| *w >= -1e-6) {
        return Some((0.0, weights));
    }
    if !overlaps(tri, corner) {
        return None;
    }
    let [ta, tb, tc] = *tri;
    let (near, gap) = [(ta, tb), (tb, tc), (tc, ta)]
        .into_iter()
        .map(|(start, end)| closest_on_segment(center, start, end))
        .fold(
            ([0.0; 2], f32::MAX),
            |best, cur| if cur.1 < best.1 { cur } else { best },
        );
    let weights = barycentric(near, tri)?.map(|w| w.max(0.0));
    let sum: f32 = weights.iter().sum();
    (sum > 0.0).then(|| (gap, weights.map(|w| w / sum)))
}

struct Texels {
    width: u32,
    /// Per keyframe, per texel: irradiance / pi.
    layers: Vec<Vec<V3>>,
    filled: Vec<bool>,
}

impl Texels {
    fn index(&self, x: u32, y: u32) -> usize {
        (y as usize) * (self.width as usize) + x as usize
    }
}

fn light_texel(
    scene: &Scene,
    settings: &Settings,
    dirs: &[V3],
    tri: &ReceiverTriangle,
    weights: [f32; 3],
) -> Vec<V3> {
    let [pa, pb, pc] = tri.positions;
    let [na, nb, nc] = tri.normals;
    let [wa, wb, wc] = weights;
    let point = pa * wa + pb * wb + pc * wc;
    let mut normal = (na * wa + nb * wb + nc * wc).normalized();
    if normal == V3::ZERO || normal.dot(tri.face) <= 0.0 {
        normal = tri.face;
    }
    let origin = point + tri.face * OFFSET;
    let (tangent, bitangent) = frame(normal);
    let seen: Vec<_> = dirs
        .iter()
        .map(|local| {
            let dir = to_frame(*local, normal, tangent, bitangent);
            // Rays below the geometric surface would hit it from behind: they bring nothing.
            (dir.dot(tri.face) > 0.0).then(|| trace(&scene.bvh, origin, dir))
        })
        .collect();
    #[allow(clippy::cast_precision_loss)] // Sample counts are at most 16384.
    let inv = 1.0 / dirs.len().max(1) as f32;
    settings
        .keyframes
        .iter()
        .map(|kf| {
            let sky = seen
                .iter()
                .flatten()
                .fold(V3::ZERO, |acc, ray| acc + incoming(&scene.bvh, settings, kf, ray))
                * inv;
            let direct = if settings.lightmap_sun {
                sun(&scene.bvh, kf, point, normal, tri.face)
            } else {
                V3::ZERO
            };
            sky + direct
        })
        .collect()
}

#[allow(clippy::cast_precision_loss)] // Texel coordinates are at most 8192.
fn rasterize(
    scene: &Scene,
    settings: &Settings,
    dirs: &[V3],
    receiver: &Receiver,
    rect: Rect,
    out: &mut Texels,
) {
    let pad = settings.padding;
    let side = rect.w - 2 * pad;
    let (left, top) = (rect.x + pad, rect.y + pad);
    let mut best: Vec<Coverage> = vec![None; (side as usize) * (side as usize)];
    let scale = side as f32;
    for (index, tri) in receiver.triangles.iter().enumerate() {
        let texel_tri = tri.uv.map(|[u, v]| [u * scale, v * scale]);
        let lo = texel_tri
            .iter()
            .fold([f32::MAX; 2], |m, q| [m[0].min(q[0]), m[1].min(q[1])]);
        let hi = texel_tri
            .iter()
            .fold([f32::MIN; 2], |m, q| [m[0].max(q[0]), m[1].max(q[1])]);
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)] // Clamped to the side.
        let clamp = |v: f32| v.floor().clamp(0.0, scale - 1.0) as u32;
        for row in clamp(lo[1])..=clamp(hi[1]) {
            for col in clamp(lo[0])..=clamp(hi[0]) {
                let Some((gap, weights)) = cover(&texel_tri, [col as f32, row as f32]) else {
                    continue;
                };
                if let Some(slot) = best.get_mut((row as usize) * (side as usize) + col as usize)
                    && slot.is_none_or(|(best_gap, _, _)| gap < best_gap)
                {
                    *slot = Some((gap, index, weights));
                }
            }
        }
    }
    for (texel, coverage) in best.iter().enumerate() {
        let Some((_, index, weights)) = coverage else {
            continue;
        };
        let Some(tri) = receiver.triangles.get(*index) else {
            continue;
        };
        let at = out.index(
            left + u32::try_from(texel % side as usize).unwrap_or(0),
            top + u32::try_from(texel / side as usize).unwrap_or(0),
        );
        for (layer, value) in out
            .layers
            .iter_mut()
            .zip(light_texel(scene, settings, dirs, tri, *weights))
        {
            if let Some(slot) = layer.get_mut(at) {
                *slot = value;
            }
        }
        if let Some(filled) = out.filled.get_mut(at) {
            *filled = true;
        }
    }
    dilate(out, rect, pad);
}

/// Fills empty texels of `rect` from filled 8-neighbors inside it, `passes` times.
#[allow(clippy::cast_precision_loss)] // At most 8 neighbors.
fn dilate(out: &mut Texels, rect: Rect, passes: u32) {
    for _ in 0..passes {
        let snapshot = out.filled.clone();
        let mut writes = Vec::new();
        for y in rect.y..rect.y + rect.h {
            for x in rect.x..rect.x + rect.w {
                if snapshot.get(out.index(x, y)).copied().unwrap_or(true) {
                    continue;
                }
                let mut sources = Vec::new();
                for row in y.saturating_sub(1).max(rect.y)..=(y + 1).min(rect.y + rect.h - 1) {
                    for col in x.saturating_sub(1).max(rect.x)..=(x + 1).min(rect.x + rect.w - 1) {
                        let at = out.index(col, row);
                        if snapshot.get(at).copied().unwrap_or(false) {
                            sources.push(at);
                        }
                    }
                }
                if !sources.is_empty() {
                    writes.push((out.index(x, y), sources));
                }
            }
        }
        for (index, sources) in writes {
            let inv = 1.0 / sources.len() as f32;
            for layer in &mut out.layers {
                let sum = sources.iter().fold(V3::ZERO, |acc, at| {
                    acc + layer.get(*at).copied().unwrap_or(V3::ZERO)
                });
                if let Some(slot) = layer.get_mut(index) {
                    *slot = sum * inv;
                }
            }
            if let Some(filled) = out.filled.get_mut(index) {
                *filled = true;
            }
        }
    }
}

/// The content side (texels) of each receiver at `density`.
#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)] // Clamped to the atlas.
fn sides(scene: &Scene, settings: &Settings, density: f32) -> Vec<u32> {
    let most = settings.max_size.saturating_sub(2 * settings.padding).max(2);
    scene
        .receivers
        .iter()
        .map(|r| {
            let side = (r.area.sqrt() * settings.texels_per_meter * density).ceil();
            (side.clamp(2.0, 8192.0) as u32).min(most)
        })
        .collect()
}

/// Bakes the atlas of every lightmapped placement (`None` when there is none).
///
/// # Errors
/// [`CookError`] at the first lightmapped placement when the rectangles cannot fit in
/// `lightmap_max_size` at any density.
#[allow(clippy::cast_precision_loss)] // Atlas sizes are at most 8192.
pub fn bake(scene: &Scene, settings: &Settings, path: &str) -> Result<Option<Baked>, CookError> {
    let Some(first) = scene.receivers.first() else {
        return Ok(None);
    };
    let pad = settings.padding;
    let mut density = 1.0f32;
    let mut packed = None;
    for _ in 0..64 {
        let content = sides(scene, settings, density);
        let sizes: Vec<(u32, u32)> = content.iter().map(|s| (s + 2 * pad, s + 2 * pad)).collect();
        if let Some(p) = pack(&sizes, settings.max_size) {
            packed = Some(p);
            break;
        }
        if content.iter().all(|s| *s <= 2) {
            break;
        }
        density *= 0.8;
    }
    let (width, height, rects) = packed.ok_or_else(|| {
        CookError::at(
            path,
            first.line,
            "the lightmapped placements do not fit in a `lightmap_max_size` atlas",
        )
    })?;
    let texels = (width as usize) * (height as usize);
    let mut out = Texels {
        width,
        layers: vec![vec![V3::ZERO; texels]; settings.keyframes.len()],
        filled: vec![false; texels],
    };
    let dirs = cosine_directions(settings.samples);
    let mut layout = Vec::with_capacity(rects.len());
    for (receiver, rect) in scene.receivers.iter().zip(&rects) {
        rasterize(scene, settings, &dirs, receiver, *rect, &mut out);
        let side = (rect.w - 2 * pad) as f32;
        let (atlas_w, atlas_h) = (width as f32, height as f32);
        layout.push((
            receiver.name.clone(),
            [
                side / atlas_w,
                side / atlas_h,
                (rect.x + pad) as f32 / atlas_w,
                (rect.y + pad) as f32 / atlas_h,
            ],
        ));
    }
    Ok(Some(Baked {
        lightmap: Lightmap {
            width,
            height,
            keyframes: settings.keyframes.iter().map(|k| k.time).collect(),
            layers: out
                .layers
                .iter()
                .map(|layer| layer.iter().map(|c| rgb_to_rgb9e5(c.to_array())).collect())
                .collect(),
        },
        layout,
    }))
}

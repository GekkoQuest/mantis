//! The renderable ground of a heightfield ([`GroundGrid`]): the cook draws a sector's
//! ground with it, and clients without a sector's visuals draw its collision with it.
//!
//! The mesh is in sector-local coordinates (place it at `origin_x, 0, origin_z`), uv0 one
//! unit per cell, uv1 0 to 1 across the sector, normals by central differences, and
//! triangles wound outward-up (decision 0019), in meshlets of 7 x 7 cells.

use super::GroundGrid;
use crate::mesh::{MeshAsset, MeshVertex, Meshlet};

/// Edge of a ground meshlet patch in cells: 7 x 7 cells is 64 vertices and 98 triangles,
/// within the meshlet limits.
const PATCH: u32 = 7;

fn grow(lo: &mut [f32; 3], hi: &mut [f32; 3], p: [f32; 3]) {
    for ((l, h), v) in lo.iter_mut().zip(hi.iter_mut()).zip(p) {
        *l = l.min(v);
        *h = h.max(v);
    }
}

fn patch_meshlet(vertices: &[MeshVertex], indices: &[u32], first: u32) -> Meshlet {
    let points: Vec<[f32; 3]> = indices
        .get(first as usize..)
        .unwrap_or(&[])
        .iter()
        .filter_map(|k| vertices.get(*k as usize).map(|v| v.position))
        .collect();
    let (mut lo, mut hi) = ([f32::INFINITY; 3], [f32::NEG_INFINITY; 3]);
    for p in &points {
        grow(&mut lo, &mut hi, *p);
    }
    let [l0, l1, l2] = lo;
    let [h0, h1, h2] = hi;
    let center = [
        f32::midpoint(l0, h0),
        f32::midpoint(l1, h1),
        f32::midpoint(l2, h2),
    ];
    let radius = points
        .iter()
        .map(|p| {
            p.iter()
                .zip(center)
                .map(|(v, c)| (v - c) * (v - c))
                .sum::<f32>()
                .sqrt()
        })
        .fold(0.0f32, f32::max);
    let count = u32::try_from(indices.len()).unwrap_or(u32::MAX) - first;
    Meshlet {
        first_index: first,
        index_count: count,
        center,
        radius,
        // No cone: the ground faces up, but slopes make a tight cone rarely useful.
        cone_axis: [0.0; 3],
        cone_cutoff: -1.0,
    }
}

/// The renderable ground of a heightfield, in sector-local coordinates.
#[allow(clippy::cast_precision_loss)] // Grid indices are at most 4097.
pub fn mesh(grid: &GroundGrid) -> MeshAsset {
    let (cols, rows) = (grid.width.max(2), grid.depth.max(2));
    let cell = grid.cell_size;
    let height = |col: u32, row: u32| {
        grid.heights
            .get((row.min(rows - 1) * cols + col.min(cols - 1)) as usize)
            .copied()
            .unwrap_or(0.0)
    };
    let extent_x = ((cols - 1) as f32 * cell).max(f32::MIN_POSITIVE);
    let extent_z = ((rows - 1) as f32 * cell).max(f32::MIN_POSITIVE);
    let mut vertices = Vec::with_capacity((cols * rows) as usize);
    let (mut lo, mut hi) = ([f32::INFINITY; 3], [f32::NEG_INFINITY; 3]);
    for row in 0..rows {
        for col in 0..cols {
            let (px, pz) = (col as f32 * cell, row as f32 * cell);
            let position = [px, height(col, row), pz];
            grow(&mut lo, &mut hi, position);
            // Central differences, one-sided at the border.
            let (left, right) = (col.saturating_sub(1), (col + 1).min(cols - 1));
            let (back, front) = (row.saturating_sub(1), (row + 1).min(rows - 1));
            let slope_x = (height(right, row) - height(left, row)) / ((right - left) as f32 * cell);
            let slope_z = (height(col, front) - height(col, back)) / ((front - back) as f32 * cell);
            let len = (slope_x * slope_x + 1.0 + slope_z * slope_z).sqrt();
            vertices.push(MeshVertex {
                position,
                normal: [-slope_x / len, 1.0 / len, -slope_z / len],
                uv0: [col as f32, row as f32],
                uv1: [px / extent_x, pz / extent_z],
            });
        }
    }
    let mut indices = Vec::new();
    let mut meshlets = Vec::new();
    for patch_row in (0..rows - 1).step_by(PATCH as usize) {
        for patch_col in (0..cols - 1).step_by(PATCH as usize) {
            let first = u32::try_from(indices.len()).unwrap_or(u32::MAX);
            for row in patch_row..(patch_row + PATCH).min(rows - 1) {
                for col in patch_col..(patch_col + PATCH).min(cols - 1) {
                    let corner = row * cols + col;
                    let (east, north, far) = (corner + 1, corner + cols, corner + cols + 1);
                    // (east - corner) x (north - corner) points down; these two point up.
                    indices.extend_from_slice(&[corner, north, east, east, north, far]);
                }
            }
            meshlets.push(patch_meshlet(&vertices, &indices, first));
        }
    }
    MeshAsset {
        vertices,
        skin: None,
        indices,
        meshlets,
        bounds_min: lo,
        bounds_max: hi,
    }
}

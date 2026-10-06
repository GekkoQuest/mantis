//! Shared vertex and index buffers. Meshes are placed in free ranges (first fit, with
//! coalescing frees: [`MeshStore::remove`] returns a mesh's ranges for reuse) and
//! referenced by range; the GPU-driven passes draw every mesh from the same two buffers,
//! so batches never rebind vertex state.

use crate::deform::RangeAllocator;
use crate::gpu_types::{SkinVertex, Vertex};
use crate::math::Aabb;
use crate::scene::MeshRange;

/// Mesh storage errors.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum MeshError {
    /// The vertex or index buffer is full.
    Full,
    /// No vertices or indices, an index out of range, a count not divisible by three, or a
    /// non-finite position.
    Invalid,
}

impl core::fmt::Display for MeshError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            MeshError::Full => f.write_str("mesh storage full"),
            MeshError::Invalid => f.write_str("invalid mesh"),
        }
    }
}

impl std::error::Error for MeshError {}

/// The shared mesh buffers.
#[derive(Debug)]
pub struct MeshStore {
    /// Vertex buffer.
    pub vertices: wgpu::Buffer,
    /// Index buffer (`u32`).
    pub indices: wgpu::Buffer,
    /// Skinning stream, parallel to `vertices` (written only for skinned meshes).
    pub skin: wgpu::Buffer,
    vertex_free: RangeAllocator,
    index_free: RangeAllocator,
}

impl MeshStore {
    /// Storage for up to `vertices` vertices and `indices` indices.
    pub fn new(device: &wgpu::Device, vertices: u32, indices: u32) -> Self {
        let mk = |label: &str, size: u64, usage: wgpu::BufferUsages| {
            device.create_buffer(&wgpu::BufferDescriptor {
                label: Some(label),
                size: size.max(4),
                usage: usage | wgpu::BufferUsages::COPY_DST,
                mapped_at_creation: false,
            })
        };
        Self {
            vertices: mk(
                "mesh.vertices",
                u64::from(vertices) * core::mem::size_of::<Vertex>() as u64,
                wgpu::BufferUsages::VERTEX,
            ),
            indices: mk("mesh.indices", u64::from(indices) * 4, wgpu::BufferUsages::INDEX),
            skin: mk(
                "mesh.skin",
                u64::from(vertices) * core::mem::size_of::<SkinVertex>() as u64,
                wgpu::BufferUsages::VERTEX,
            ),
            vertex_free: RangeAllocator::new(vertices),
            index_free: RangeAllocator::new(indices),
        }
    }

    /// Vertices and indices free (in total; a mesh needs one contiguous range of each).
    pub fn free(&self) -> (u32, u32) {
        (self.vertex_free.free_total(), self.index_free.free_total())
    }

    /// Returns a mesh's vertex and index ranges for reuse. Its instances must be gone
    /// (the scene refuses to remove a mesh in use).
    pub fn remove(&mut self, range: &MeshRange) {
        if let Ok(base) = u32::try_from(range.base_vertex) {
            self.vertex_free.free(base, range.vertex_count);
        }
        self.index_free.free(range.first_index, range.index_count);
    }

    /// Uploads a mesh and returns its range (with a bounding sphere of its positions).
    ///
    /// # Errors
    /// [`MeshError::Invalid`] or [`MeshError::Full`]; nothing is uploaded on error.
    pub fn add(
        &mut self,
        queue: &wgpu::Queue,
        vertices: &[Vertex],
        indices: &[u32],
    ) -> Result<MeshRange, MeshError> {
        let vcount = u32::try_from(vertices.len()).map_err(|_| MeshError::Full)?;
        let icount = u32::try_from(indices.len()).map_err(|_| MeshError::Full)?;
        if vcount == 0 || icount == 0 || icount % 3 != 0 || indices.iter().any(|i| *i >= vcount) {
            return Err(MeshError::Invalid);
        }
        if vertices.iter().flat_map(|v| v.position).any(|c| !c.is_finite()) {
            return Err(MeshError::Invalid);
        }
        let vstart = self.vertex_free.alloc(vcount, 1).ok_or(MeshError::Full)?;
        let Some(istart) = self.index_free.alloc(icount, 1) else {
            self.vertex_free.free(vstart, vcount);
            return Err(MeshError::Full);
        };
        let (min, max) = vertices.iter().fold(
            (glam::Vec3::splat(f32::MAX), glam::Vec3::splat(f32::MIN)),
            |(lo, hi), v| {
                let p = glam::Vec3::from(v.position);
                (lo.min(p), hi.max(p))
            },
        );
        let range = MeshRange {
            index_count: icount,
            first_index: istart,
            base_vertex: i32::try_from(vstart).map_err(|_| MeshError::Full)?,
            vertex_count: vcount,
            bounds: Aabb { min, max }.bounding_sphere(),
        };
        queue.write_buffer(
            &self.vertices,
            u64::from(vstart) * core::mem::size_of::<Vertex>() as u64,
            bytemuck::cast_slice(vertices),
        );
        queue.write_buffer(
            &self.indices,
            u64::from(istart) * 4,
            bytemuck::cast_slice(indices),
        );
        Ok(range)
    }

    /// Uploads a skinned mesh: its vertices, the parallel skinning stream, and indices.
    /// Bone indices are checked against the skeleton when an instance is spawned.
    ///
    /// # Errors
    /// [`MeshError::Invalid`] (also for a skinning stream of another length or a vertex
    /// without weight) or [`MeshError::Full`]; nothing is uploaded on error.
    pub fn add_skinned(
        &mut self,
        queue: &wgpu::Queue,
        vertices: &[Vertex],
        skin: &[SkinVertex],
        indices: &[u32],
    ) -> Result<MeshRange, MeshError> {
        if skin.len() != vertices.len() || skin.iter().any(|s| s.weights == [0; 4]) {
            return Err(MeshError::Invalid);
        }
        let range = self.add(queue, vertices, indices)?;
        let first = u32::try_from(range.base_vertex).unwrap_or(0);
        queue.write_buffer(
            &self.skin,
            u64::from(first) * core::mem::size_of::<SkinVertex>() as u64,
            bytemuck::cast_slice(skin),
        );
        Ok(range)
    }
}

/// A unit cube centered on the origin (24 vertices, per-face normals).
pub fn cube() -> (Vec<Vertex>, Vec<u32>) {
    let faces: [([f32; 3], [f32; 3], [f32; 3]); 6] = [
        ([1.0, 0.0, 0.0], [0.0, 1.0, 0.0], [0.0, 0.0, -1.0]),
        ([-1.0, 0.0, 0.0], [0.0, 1.0, 0.0], [0.0, 0.0, 1.0]),
        ([0.0, 1.0, 0.0], [0.0, 0.0, -1.0], [1.0, 0.0, 0.0]),
        ([0.0, -1.0, 0.0], [0.0, 0.0, 1.0], [1.0, 0.0, 0.0]),
        ([0.0, 0.0, 1.0], [0.0, 1.0, 0.0], [1.0, 0.0, 0.0]),
        ([0.0, 0.0, -1.0], [0.0, 1.0, 0.0], [-1.0, 0.0, 0.0]),
    ];
    let mut vertices = Vec::with_capacity(24);
    let mut indices = Vec::with_capacity(36);
    for (normal, up, right) in faces {
        let (n, u, r) = (
            glam::Vec3::from(normal),
            glam::Vec3::from(up),
            glam::Vec3::from(right),
        );
        let base = u32::try_from(vertices.len()).unwrap_or(0);
        for (su, sr, uv) in [
            (-1.0f32, -1.0f32, [0.0, 1.0]),
            (-1.0, 1.0, [1.0, 1.0]),
            (1.0, 1.0, [1.0, 0.0]),
            (1.0, -1.0, [0.0, 0.0]),
        ] {
            let p = (n + u * su + r * sr) * 0.5;
            vertices.push(Vertex {
                position: p.to_array(),
                normal,
                uv0: uv,
                uv1: uv,
            });
        }
        // Clockwise seen from outside in the left-handed world (decision 0019): the cross
        // product of the edges points outward. Pipelines use `gpu_types::FRONT_FACE`.
        indices.extend_from_slice(&[base, base + 1, base + 2, base, base + 2, base + 3]);
    }
    (vertices, indices)
}

/// A square in the xz plane facing +y, `size` across.
pub fn plane(size: f32) -> (Vec<Vertex>, Vec<u32>) {
    let h = size * 0.5;
    let v = |x: f32, z: f32, uv: [f32; 2]| Vertex {
        position: [x, 0.0, z],
        normal: [0.0, 1.0, 0.0],
        uv0: uv,
        uv1: uv,
    };
    (
        vec![
            v(-h, -h, [0.0, 0.0]),
            v(-h, h, [0.0, 1.0]),
            v(h, h, [1.0, 1.0]),
            v(h, -h, [1.0, 0.0]),
        ],
        vec![0, 1, 2, 0, 2, 3],
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn winding_is_clockwise_from_outside_in_the_left_handed_world() {
        let position = |verts: &[Vertex], k: u32| {
            verts
                .get(k as usize)
                .map_or(glam::Vec3::ZERO, |x| glam::Vec3::from(x.position))
        };
        let normal = |verts: &[Vertex], k: u32| {
            verts
                .get(k as usize)
                .map_or(glam::Vec3::ZERO, |x| glam::Vec3::from(x.normal))
        };
        let (verts, indices) = cube();
        assert_eq!((verts.len(), indices.len()), (24, 36));
        for tri in indices.as_chunks::<3>().0 {
            let [i0, i1, i2] = *tri;
            let face = (position(&verts, i1) - position(&verts, i0))
                .cross(position(&verts, i2) - position(&verts, i0));
            assert!(face.dot(normal(&verts, i0)) > 0.0, "triangle {tri:?}");
        }
        let (plane_verts, plane_indices) = plane(2.0);
        for tri in plane_indices.as_chunks::<3>().0 {
            let [i0, i1, i2] = *tri;
            let face = (position(&plane_verts, i1) - position(&plane_verts, i0))
                .cross(position(&plane_verts, i2) - position(&plane_verts, i0));
            assert!(face.y > 0.0);
        }
    }
}

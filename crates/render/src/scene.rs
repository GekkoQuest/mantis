//! The GPU-driven scene: mesh ranges, instances, and per-material draw batches.
//!
//! Instances are grouped into batches keyed by `(material, deformation, lightmapped,
//! mesh)`, ordered by material first so pipeline and material state change once per
//! material. Each frame,
//! [`Scene::prepare`] writes the instance array, the batch table, and one indirect draw
//! per batch (with an instance count of zero) into a preallocated [`FramePrep`]. The cull
//! compute pass then increments each batch's instance count for every visible instance and
//! appends the instance index to the batch's slice of the visible list. Frame preparation
//! never allocates; capacities are fixed at construction and overflow fails closed and is
//! counted.

use bytemuck::{Pod, Zeroable};
use glam::Mat4;

pub use mantis_formats::material::Deform;

use crate::math::{Frustum, Sphere};

/// A mesh's range in the shared vertex and index buffers.
#[derive(Clone, Copy, PartialEq, Debug)]
pub struct MeshRange {
    /// Indices to draw.
    pub index_count: u32,
    /// First index in the shared index buffer.
    pub first_index: u32,
    /// Offset added to each index (the first vertex in the shared buffer).
    pub base_vertex: i32,
    /// Vertices the mesh occupies.
    pub vertex_count: u32,
    /// Bounding sphere in mesh space.
    pub bounds: Sphere,
}

/// A registered mesh.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, PartialOrd, Ord)]
pub struct MeshId(u32);

/// A material (resolved to a pipeline permutation and parameters elsewhere).
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, PartialOrd, Ord)]
pub struct MaterialId(pub u32);

/// A live instance. Generational: a stale handle is rejected.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct InstanceHandle {
    index: u32,
    generation: u32,
}

/// Instance data as the GPU reads it (std430-compatible, 128 bytes).
#[repr(C)]
#[derive(Clone, Copy, PartialEq, Debug, Pod, Zeroable)]
pub struct GpuInstance {
    /// Model matrix, column-major.
    pub model: [[f32; 4]; 4],
    /// World bounding sphere: center xyz, radius w.
    pub sphere: [f32; 4],
    /// Batch index.
    pub batch: u32,
    /// Lightmapped instances: the lightmap page table slot.
    pub lightmap_page: u32,
    /// Padding to 16 bytes.
    pub pad: [u32; 2],
    /// Deformation data (see `Instance.deform` in the shared WGSL): skinned `[palette
    /// base, 0, 0, 0]`; vertex animation `[first texel - base vertex (wrapping), vertices
    /// per frame, frame position bits, frame count]`; static zero.
    pub deform: [u32; 4],
    /// Lightmapped instances: lightmap uv = uv1 * xy + zw in page coordinates.
    pub lightmap_rect: [f32; 4],
}

/// Where a static instance reads its cooked lightmap.
#[derive(Clone, Copy, PartialEq, Debug)]
pub struct LightmapBinding {
    /// The page table slot (`WorldLight::add_lightmap`).
    pub page: u32,
    /// The uv rectangle in page coordinates (`WorldLight::page_rect`).
    pub rect: [f32; 4],
}

/// How an instance's vertices are deformed, with the GPU-side data.
#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Deformation {
    /// Rigid.
    Static,
    /// Skinned from a bone palette in the deformation pool.
    Skinned {
        /// First pool slot of the palette.
        palette_base: u32,
        /// Bones in the palette.
        bones: u32,
    },
    /// Played from a baked vertex animation in the deformation pool.
    Vat {
        /// First texel of the animation.
        first_texel: u32,
        /// Vertices per frame (the mesh's vertex count).
        vertices: u32,
        /// Frames.
        frames: u32,
        /// Frame position: the integer part a frame, the fraction the blend to the next.
        frame: f32,
    },
}

impl Deformation {
    /// The permutation family this needs.
    pub fn kind(&self) -> Deform {
        match self {
            Deformation::Static => Deform::Static,
            Deformation::Skinned { .. } => Deform::Skinned,
            Deformation::Vat { .. } => Deform::Vat,
        }
    }

    fn gpu(&self, base_vertex: i32) -> [u32; 4] {
        match *self {
            Deformation::Static => [0; 4],
            Deformation::Skinned { palette_base, .. } => [palette_base, 0, 0, 0],
            Deformation::Vat {
                first_texel,
                vertices,
                frames,
                frame,
            } => [
                first_texel.wrapping_sub(base_vertex.cast_unsigned()),
                vertices,
                frame.to_bits(),
                frames,
            ],
        }
    }
}

impl GpuInstance {
    /// The world bounding sphere.
    pub fn world_sphere(&self) -> Sphere {
        let [cx, cy, cz, radius] = self.sphere;
        Sphere {
            center: glam::Vec3::new(cx, cy, cz),
            radius,
        }
    }
}

/// Batch data as the GPU reads it.
#[repr(C)]
#[derive(Clone, Copy, PartialEq, Eq, Debug, Pod, Zeroable)]
pub struct GpuBatch {
    /// First slot of this batch in the visible list.
    pub base: u32,
    /// Slots reserved (the batch's instance count).
    pub capacity: u32,
    /// Padding.
    pub pad: [u32; 2],
}

/// `wgpu`'s indexed indirect draw layout.
#[repr(C)]
#[derive(Clone, Copy, PartialEq, Eq, Debug, Pod, Zeroable)]
pub struct DrawIndexedIndirect {
    /// Indices per instance.
    pub index_count: u32,
    /// Instances to draw (written by the cull pass).
    pub instance_count: u32,
    /// First index.
    pub first_index: u32,
    /// Base vertex.
    pub base_vertex: i32,
    /// First instance (the batch's visible-list base when the device supports a nonzero
    /// first instance in indirect draws; zero otherwise).
    pub first_instance: u32,
}

/// One batch's draw, for the CPU side of the forward pass.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct BatchDraw {
    /// Material.
    pub material: MaterialId,
    /// Deformation (selects the permutation and, for skinning, the extra vertex stream).
    pub deform: Deform,
    /// Lightmapped (static only; selects the lightmapped forward permutation).
    pub lightmapped: bool,
    /// Mesh.
    pub mesh: MeshId,
    /// Index of this batch's args in the indirect buffer.
    pub batch: u32,
    /// The batch's visible-list base.
    pub base: u32,
}

/// One frame's prepared GPU data. Allocated once with fixed capacities.
#[derive(Clone, Debug)]
pub struct FramePrep {
    /// Instance array.
    pub instances: Vec<GpuInstance>,
    /// Batch table.
    pub batches: Vec<GpuBatch>,
    /// Indirect args, one per batch.
    pub args: Vec<DrawIndexedIndirect>,
    /// Draw list, one per batch, in material order.
    pub draws: Vec<BatchDraw>,
    /// Each instance's model matrix in the previous prepared frame, parallel to
    /// `instances` (motion vectors).
    pub previous_models: Vec<[[f32; 4]; 4]>,
    instance_capacity: usize,
    batch_capacity: usize,
}

impl FramePrep {
    /// Storage for up to `instances` instances in up to `batches` batches.
    pub fn with_capacity(instances: usize, batches: usize) -> Self {
        Self {
            instances: Vec::with_capacity(instances),
            batches: Vec::with_capacity(batches),
            args: Vec::with_capacity(batches),
            draws: Vec::with_capacity(batches),
            previous_models: Vec::with_capacity(instances),
            instance_capacity: instances,
            batch_capacity: batches,
        }
    }

    fn clear(&mut self) {
        self.instances.clear();
        self.batches.clear();
        self.args.clear();
        self.draws.clear();
        self.previous_models.clear();
    }

    /// Total visible-list slots (the sum of batch capacities).
    pub fn visible_slots(&self) -> u32 {
        self.batches.last().map_or(0, |b| b.base + b.capacity)
    }
}

/// Rejects non-finite transforms and anything but a similarity.
fn check_transform(model: &Mat4) -> Result<(), SceneError> {
    if !model.is_finite() {
        return Err(SceneError::NonFinite);
    }
    let axes = [
        model.x_axis.truncate().to_array(),
        model.y_axis.truncate().to_array(),
        model.z_axis.truncate().to_array(),
    ];
    if mantis_formats::sector::is_similarity(axes) {
        Ok(())
    } else {
        Err(SceneError::NonUniformScale)
    }
}

/// Rejects non-finite or out-of-range vertex animation frame positions.
fn check_deformation(d: &Deformation) -> Result<(), SceneError> {
    match *d {
        Deformation::Vat { frame, frames, .. } => {
            #[expect(clippy::cast_precision_loss)] // Frame counts are far below 2^24.
            let ok = frame.is_finite() && frame >= 0.0 && frame < frames.max(1) as f32;
            if ok { Ok(()) } else { Err(SceneError::NonFinite) }
        }
        Deformation::Static | Deformation::Skinned { .. } => Ok(()),
    }
}

/// Scene errors.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum SceneError {
    /// Instance capacity reached.
    InstancesFull,
    /// Batch capacity reached (too many distinct material and mesh pairs).
    BatchesFull,
    /// Unknown mesh.
    UnknownMesh,
    /// The handle is stale or was never valid.
    StaleHandle,
    /// The transform or deformation data is not finite.
    NonFinite,
    /// The transform scales non-uniformly or shears. Normals are transformed by the model
    /// matrix, which is only correct for similarities; this fails closed until instances
    /// carry a normal matrix.
    NonUniformScale,
    /// A deformation update changed the deformation kind.
    DeformationKind,
    /// The mesh or material still has live instances.
    InUse,
}

impl core::fmt::Display for SceneError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        let s = match self {
            SceneError::InstancesFull => "instance capacity reached",
            SceneError::BatchesFull => "batch capacity reached",
            SceneError::UnknownMesh => "unknown mesh",
            SceneError::StaleHandle => "stale instance handle",
            SceneError::NonFinite => "non-finite transform",
            SceneError::NonUniformScale => "non-uniform scale or shear",
            SceneError::DeformationKind => "deformation kind changed",
            SceneError::InUse => "mesh or material still has instances",
        };
        f.write_str(s)
    }
}

impl std::error::Error for SceneError {}

#[derive(Clone, Copy, Debug)]
struct Instance {
    mesh: MeshId,
    batch: usize,
    model: Mat4,
    sphere: Sphere,
    deformation: Deformation,
    /// Model-space bounds replacing the mesh's (animated extents).
    bounds: Option<Sphere>,
    lightmap: Option<LightmapBinding>,
    /// The model matrix at the previous prepare (motion vectors).
    previous: Mat4,
}

#[derive(Clone, Copy, Debug)]
struct Batch {
    material: MaterialId,
    deform: Deform,
    lightmapped: bool,
    mesh: MeshId,
    live: u32,
}

/// The scene.
#[derive(Debug)]
pub struct Scene {
    meshes: Vec<Option<MeshRange>>,
    free_meshes: Vec<u32>,
    slots: Vec<Option<Instance>>,
    generations: Vec<u32>,
    free: Vec<u32>,
    /// Sorted by (material, deformation, lightmapped, mesh).
    batches: Vec<Batch>,
    batch_capacity: usize,
    first_instance_supported: bool,
}

impl Scene {
    /// A scene holding up to `instances` instances in up to `batches` batches.
    /// `first_instance_supported` selects how batches find their visible-list slice (see
    /// [`DrawIndexedIndirect::first_instance`]).
    pub fn new(instances: usize, batches: usize, first_instance_supported: bool) -> Self {
        let cap = u32::try_from(instances).unwrap_or(u32::MAX);
        Self {
            meshes: Vec::new(),
            free_meshes: Vec::new(),
            slots: vec![None; instances],
            generations: vec![0; instances],
            free: (0..cap).rev().collect(),
            batches: Vec::with_capacity(batches),
            batch_capacity: batches,
            first_instance_supported,
        }
    }

    /// Registers a mesh.
    pub fn add_mesh(&mut self, range: MeshRange) -> MeshId {
        if let Some(id) = self.free_meshes.pop()
            && let Some(slot) = self.meshes.get_mut(id as usize)
        {
            *slot = Some(range);
            return MeshId(id);
        }
        self.meshes.push(Some(range));
        MeshId(u32::try_from(self.meshes.len() - 1).unwrap_or(u32::MAX))
    }

    /// Unregisters a mesh with no live instances and returns its range (for
    /// [`crate::mesh::MeshStore::remove`]). Its empty batches are dropped; the id may be
    /// reused by a later [`Scene::add_mesh`].
    ///
    /// # Errors
    /// [`SceneError::UnknownMesh`] or [`SceneError::InUse`].
    pub fn remove_mesh(&mut self, mesh: MeshId) -> Result<MeshRange, SceneError> {
        if self.batches.iter().any(|b| b.mesh == mesh && b.live > 0) {
            return Err(SceneError::InUse);
        }
        let range = self
            .meshes
            .get_mut(mesh.0 as usize)
            .and_then(Option::take)
            .ok_or(SceneError::UnknownMesh)?;
        self.drop_batches(|b| b.mesh == mesh);
        self.free_meshes.push(mesh.0);
        Ok(range)
    }

    /// Drops the (empty) batches of a material about to be unloaded.
    ///
    /// # Errors
    /// [`SceneError::InUse`] when an instance still uses the material.
    pub fn release_material(&mut self, material: MaterialId) -> Result<(), SceneError> {
        if self.batches.iter().any(|b| b.material == material && b.live > 0) {
            return Err(SceneError::InUse);
        }
        self.drop_batches(|b| b.material == material);
        Ok(())
    }

    /// Meshes registered.
    pub fn mesh_count(&self) -> usize {
        self.meshes.iter().flatten().count()
    }

    /// Removes empty batches matching `pred`, renumbering the instances of later batches.
    fn drop_batches(&mut self, pred: impl Fn(&Batch) -> bool) {
        let mut i = self.batches.len();
        while i > 0 {
            i -= 1;
            if self.batches.get(i).is_some_and(|b| b.live == 0 && pred(b)) {
                self.batches.remove(i);
                for inst in self.slots.iter_mut().flatten() {
                    if inst.batch > i {
                        inst.batch -= 1;
                    }
                }
            }
        }
    }

    /// Live instances.
    pub fn instance_count(&self) -> usize {
        self.slots.len() - self.free.len()
    }

    fn batch_for(
        &mut self,
        material: MaterialId,
        deform: Deform,
        lightmapped: bool,
        mesh: MeshId,
    ) -> Result<usize, SceneError> {
        match self
            .batches
            .binary_search_by_key(&(material, deform, lightmapped, mesh), |b| {
                (b.material, b.deform, b.lightmapped, b.mesh)
            }) {
            Ok(i) => Ok(i),
            Err(i) => {
                if self.batches.len() >= self.batch_capacity {
                    return Err(SceneError::BatchesFull);
                }
                self.batches.insert(
                    i,
                    Batch {
                        material,
                        deform,
                        lightmapped,
                        mesh,
                        live: 0,
                    },
                );
                // Indices at or after i shifted; fix instances that point past it.
                for inst in self.slots.iter_mut().flatten() {
                    if inst.batch >= i {
                        inst.batch += 1;
                    }
                }
                Ok(i)
            }
        }
    }

    fn world_sphere(&self, mesh: MeshId, model: &Mat4, bounds: Option<Sphere>) -> Option<Sphere> {
        let m = self.meshes.get(mesh.0 as usize)?.as_ref()?;
        let local = bounds.unwrap_or(m.bounds);
        let center = model.transform_point3(local.center);
        let scale = model
            .x_axis
            .truncate()
            .length()
            .max(model.y_axis.truncate().length())
            .max(model.z_axis.truncate().length());
        Some(Sphere {
            center,
            radius: local.radius * scale,
        })
    }

    /// Adds an instance.
    ///
    /// # Errors
    /// [`SceneError::UnknownMesh`], [`SceneError::NonFinite`],
    /// [`SceneError::InstancesFull`], or [`SceneError::BatchesFull`].
    pub fn spawn(
        &mut self,
        mesh: MeshId,
        material: MaterialId,
        model: Mat4,
    ) -> Result<InstanceHandle, SceneError> {
        self.spawn_with(mesh, material, model, Deformation::Static, None)
    }

    /// Adds a static instance lit by a cooked lightmap (drawn with the material's
    /// lightmapped forward permutation).
    ///
    /// # Errors
    /// As [`Scene::spawn`]; [`SceneError::NonFinite`] for a non-finite uv rectangle.
    pub fn spawn_lightmapped(
        &mut self,
        mesh: MeshId,
        material: MaterialId,
        model: Mat4,
        lightmap: LightmapBinding,
    ) -> Result<InstanceHandle, SceneError> {
        if !lightmap.rect.iter().all(|v| v.is_finite()) {
            return Err(SceneError::NonFinite);
        }
        self.insert(mesh, material, model, Deformation::Static, None, Some(lightmap))
    }

    /// Adds a deformed instance. `bounds` (model space) replaces the mesh's bounds for
    /// culling, so animated extents stay inside the culling sphere.
    ///
    /// # Errors
    /// As [`Scene::spawn`]; [`SceneError::NonFinite`] for a non-finite frame position.
    pub fn spawn_with(
        &mut self,
        mesh: MeshId,
        material: MaterialId,
        model: Mat4,
        deformation: Deformation,
        bounds: Option<Sphere>,
    ) -> Result<InstanceHandle, SceneError> {
        self.insert(mesh, material, model, deformation, bounds, None)
    }

    fn insert(
        &mut self,
        mesh: MeshId,
        material: MaterialId,
        model: Mat4,
        deformation: Deformation,
        bounds: Option<Sphere>,
        lightmap: Option<LightmapBinding>,
    ) -> Result<InstanceHandle, SceneError> {
        check_transform(&model)?;
        check_deformation(&deformation)?;
        let sphere = self
            .world_sphere(mesh, &model, bounds)
            .ok_or(SceneError::UnknownMesh)?;
        if self.free.is_empty() {
            return Err(SceneError::InstancesFull);
        }
        let batch = self.batch_for(material, deformation.kind(), lightmap.is_some(), mesh)?;
        let index = self.free.pop().ok_or(SceneError::InstancesFull)?;
        if let Some(b) = self.batches.get_mut(batch) {
            b.live += 1;
        }
        if let Some(slot) = self.slots.get_mut(index as usize) {
            *slot = Some(Instance {
                mesh,
                batch,
                model,
                sphere,
                deformation,
                bounds,
                lightmap,
                previous: model,
            });
        }
        let generation = self.generations.get(index as usize).copied().unwrap_or(0);
        Ok(InstanceHandle { index, generation })
    }

    fn live(&mut self, h: InstanceHandle) -> Result<&mut Instance, SceneError> {
        if self.generations.get(h.index as usize) != Some(&h.generation) {
            return Err(SceneError::StaleHandle);
        }
        self.slots
            .get_mut(h.index as usize)
            .and_then(Option::as_mut)
            .ok_or(SceneError::StaleHandle)
    }

    /// Moves an instance.
    ///
    /// # Errors
    /// [`SceneError::StaleHandle`] or [`SceneError::NonFinite`].
    pub fn set_transform(&mut self, h: InstanceHandle, model: Mat4) -> Result<(), SceneError> {
        check_transform(&model)?;
        let (mesh, bounds) = {
            let inst = self.live(h)?;
            (inst.mesh, inst.bounds)
        };
        let sphere = self
            .world_sphere(mesh, &model, bounds)
            .ok_or(SceneError::UnknownMesh)?;
        let inst = self.live(h)?;
        inst.model = model;
        inst.sphere = sphere;
        Ok(())
    }

    /// Updates an instance's deformation data (for example the frame position of a vertex
    /// animation). The deformation kind cannot change; despawn and respawn instead.
    ///
    /// # Errors
    /// [`SceneError::StaleHandle`], [`SceneError::NonFinite`], or
    /// [`SceneError::DeformationKind`].
    pub fn set_deformation(&mut self, h: InstanceHandle, deformation: Deformation) -> Result<(), SceneError> {
        check_deformation(&deformation)?;
        let inst = self.live(h)?;
        if inst.deformation.kind() != deformation.kind() {
            return Err(SceneError::DeformationKind);
        }
        inst.deformation = deformation;
        Ok(())
    }

    /// An instance's deformation.
    ///
    /// # Errors
    /// [`SceneError::StaleHandle`].
    pub fn deformation(&mut self, h: InstanceHandle) -> Result<Deformation, SceneError> {
        Ok(self.live(h)?.deformation)
    }

    /// Removes an instance.
    ///
    /// # Errors
    /// [`SceneError::StaleHandle`].
    pub fn despawn(&mut self, h: InstanceHandle) -> Result<(), SceneError> {
        let batch = self.live(h)?.batch;
        if let Some(slot) = self.slots.get_mut(h.index as usize) {
            *slot = None;
        }
        if let Some(g) = self.generations.get_mut(h.index as usize) {
            *g = g.wrapping_add(1);
        }
        self.free.push(h.index);
        if let Some(b) = self.batches.get_mut(batch) {
            b.live = b.live.saturating_sub(1);
        }
        Ok(())
    }

    /// Writes this frame's GPU data into `out`, with each instance's model matrix from
    /// the previous call (its current one when it is new), then remembers the current
    /// ones for the next call. Allocation-free when `out` was created with this scene's
    /// capacities. Returns the number of instances written.
    pub fn prepare(&mut self, out: &mut FramePrep) -> u32 {
        out.clear();
        // Batch table and args, in batch key order; empty batches keep their slot
        // (instances index batches by position) and simply draw nothing.
        let mut base = 0u32;
        for (i, b) in self.batches.iter().enumerate() {
            if out.batches.len() >= out.batch_capacity {
                break;
            }
            let Some(mesh) = self.meshes.get(b.mesh.0 as usize).and_then(Option::as_ref) else {
                continue;
            };
            out.batches.push(GpuBatch {
                base,
                capacity: b.live,
                pad: [0; 2],
            });
            out.args.push(DrawIndexedIndirect {
                index_count: mesh.index_count,
                instance_count: 0,
                first_index: mesh.first_index,
                base_vertex: mesh.base_vertex,
                first_instance: if self.first_instance_supported { base } else { 0 },
            });
            let batch = u32::try_from(i).unwrap_or(u32::MAX);
            out.draws.push(BatchDraw {
                material: b.material,
                deform: b.deform,
                lightmapped: b.lightmapped,
                mesh: b.mesh,
                batch,
                base,
            });
            base = base.saturating_add(b.live);
        }
        for inst in self.slots.iter_mut().flatten() {
            if out.instances.len() >= out.instance_capacity {
                break;
            }
            out.previous_models.push(inst.previous.to_cols_array_2d());
            inst.previous = inst.model;
            let s = inst.sphere;
            let base_vertex = self
                .meshes
                .get(inst.mesh.0 as usize)
                .and_then(Option::as_ref)
                .map_or(0, |m| m.base_vertex);
            out.instances.push(GpuInstance {
                model: inst.model.to_cols_array_2d(),
                sphere: [s.center.x, s.center.y, s.center.z, s.radius],
                batch: u32::try_from(inst.batch).unwrap_or(u32::MAX),
                lightmap_page: inst.lightmap.map_or(0, |l| l.page),
                pad: [0; 2],
                deform: inst.deformation.gpu(base_vertex),
                lightmap_rect: inst.lightmap.map_or([0.0; 4], |l| l.rect),
            });
        }
        u32::try_from(out.instances.len()).unwrap_or(u32::MAX)
    }
}

/// The CPU reference of the GPU cull: the visible instance indices of each batch, in
/// instance order. Used by tests to check the compute pass.
pub fn cull_reference(prep: &FramePrep, frustum: &Frustum) -> Vec<Vec<u32>> {
    let mut out = vec![Vec::new(); prep.batches.len()];
    for (i, inst) in prep.instances.iter().enumerate() {
        if frustum.intersects_sphere(&inst.world_sphere())
            && let Some(list) = out.get_mut(inst.batch as usize)
        {
            list.push(u32::try_from(i).unwrap_or(u32::MAX));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use glam::Vec3;

    type TestResult = Result<(), Box<dyn std::error::Error>>;

    fn cube() -> MeshRange {
        MeshRange {
            index_count: 36,
            first_index: 0,
            base_vertex: 0,
            vertex_count: 24,
            bounds: Sphere {
                center: Vec3::ZERO,
                radius: 1.0,
            },
        }
    }

    #[test]
    fn batches_are_sorted_by_material_then_mesh_and_indexed_consistently() -> TestResult {
        let mut s = Scene::new(16, 8, true);
        let m0 = s.add_mesh(cube());
        let m1 = s.add_mesh(MeshRange {
            first_index: 36,
            ..cube()
        });
        let _ = s.spawn(m1, MaterialId(2), Mat4::IDENTITY)?;
        let _ = s.spawn(m0, MaterialId(2), Mat4::IDENTITY)?;
        let _ = s.spawn(m0, MaterialId(1), Mat4::IDENTITY)?;
        let _ = s.spawn(m0, MaterialId(2), Mat4::from_translation(Vec3::X))?;
        let mut prep = FramePrep::with_capacity(16, 8);
        assert_eq!(s.prepare(&mut prep), 4);
        let keys: Vec<(u32, MeshId)> = prep.draws.iter().map(|d| (d.material.0, d.mesh)).collect();
        assert_eq!(keys, vec![(1, m0), (2, m0), (2, m1)]);
        let caps: Vec<(u32, u32)> = prep.batches.iter().map(|b| (b.base, b.capacity)).collect();
        assert_eq!(caps, vec![(0, 1), (1, 2), (3, 1)]);
        assert_eq!(prep.visible_slots(), 4);
        // Every instance points at the batch matching its own mesh and material.
        let per_batch: Vec<usize> = (0..3)
            .map(|b| prep.instances.iter().filter(|i| i.batch == b).count())
            .collect();
        assert_eq!(per_batch, vec![1, 2, 1]);
        assert!(prep.args.iter().all(|a| a.instance_count == 0));
        assert_eq!(
            prep.args.get(2).map(|a| (a.first_index, a.first_instance)),
            Some((36, 3))
        );
        Ok(())
    }

    #[test]
    fn non_similarity_transforms_fail_closed_and_previous_models_trail_one_prepare() -> TestResult {
        let mut s = Scene::new(4, 2, false);
        let m = s.add_mesh(cube());
        let stretched = Mat4::from_scale(Vec3::new(1.0, 2.0, 1.0));
        assert_eq!(
            s.spawn(m, MaterialId(0), stretched),
            Err(SceneError::NonUniformScale)
        );
        let uniform = Mat4::from_scale_rotation_translation(
            Vec3::splat(3.0),
            glam::Quat::from_rotation_y(0.7),
            Vec3::X,
        );
        let a = s.spawn(m, MaterialId(0), uniform)?;
        assert_eq!(s.set_transform(a, stretched), Err(SceneError::NonUniformScale));
        let mut prep = FramePrep::with_capacity(4, 2);
        let _ = s.prepare(&mut prep);
        assert_eq!(
            prep.previous_models,
            vec![uniform.to_cols_array_2d()],
            "a new instance has no motion"
        );
        let moved = Mat4::from_translation(Vec3::Y);
        s.set_transform(a, moved)?;
        let _ = s.prepare(&mut prep);
        assert_eq!(prep.previous_models, vec![uniform.to_cols_array_2d()]);
        assert_eq!(
            prep.instances.first().map(|i| i.model),
            Some(moved.to_cols_array_2d())
        );
        let _ = s.prepare(&mut prep);
        assert_eq!(
            prep.previous_models,
            vec![moved.to_cols_array_2d()],
            "at rest again"
        );
        Ok(())
    }

    #[test]
    fn handles_are_generational_and_capacity_fails_closed() -> TestResult {
        let mut s = Scene::new(2, 1, false);
        let m = s.add_mesh(cube());
        let a = s.spawn(m, MaterialId(0), Mat4::IDENTITY)?;
        let _b = s.spawn(m, MaterialId(0), Mat4::IDENTITY)?;
        assert_eq!(
            s.spawn(m, MaterialId(0), Mat4::IDENTITY),
            Err(SceneError::InstancesFull)
        );
        s.despawn(a)?;
        assert_eq!(s.despawn(a), Err(SceneError::StaleHandle));
        assert_eq!(s.set_transform(a, Mat4::IDENTITY), Err(SceneError::StaleHandle));
        let c = s.spawn(m, MaterialId(0), Mat4::IDENTITY)?;
        assert_ne!(a, c, "the slot is reused with a new generation");
        assert_eq!(
            s.spawn(m, MaterialId(9), Mat4::IDENTITY),
            Err(SceneError::InstancesFull)
        );
        s.despawn(c)?;
        assert_eq!(
            s.spawn(m, MaterialId(9), Mat4::IDENTITY),
            Err(SceneError::BatchesFull)
        );
        let nan = Mat4::from_translation(Vec3::new(f32::NAN, 0.0, 0.0));
        assert_eq!(s.spawn(m, MaterialId(0), nan), Err(SceneError::NonFinite));
        Ok(())
    }

    #[test]
    fn world_spheres_follow_transforms_and_scale() -> TestResult {
        let mut s = Scene::new(4, 4, false);
        let m = s.add_mesh(cube());
        let h = s.spawn(m, MaterialId(0), Mat4::from_translation(Vec3::new(5.0, 0.0, 0.0)))?;
        s.set_transform(
            h,
            Mat4::from_scale_rotation_translation(
                Vec3::splat(3.0),
                glam::Quat::from_rotation_y(0.5),
                Vec3::Z,
            ),
        )?;
        let mut prep = FramePrep::with_capacity(4, 4);
        let _ = s.prepare(&mut prep);
        assert_eq!(
            prep.instances.first().map(|i| i.sphere),
            Some([0.0, 0.0, 1.0, 3.0])
        );
        assert_eq!(
            prep.args.first().map(|a| a.first_instance),
            Some(0),
            "no first-instance support"
        );
        Ok(())
    }
}

//! The deformation pool: one storage buffer holding bone palettes (near crowd tier,
//! skinned meshes) and baked vertex animations (mid and far tiers), read by the vertex
//! stage of deformed permutations (`mantis_shadergen` binds it at scene-group binding
//! [`mantis_shadergen::layout::DEFORM_POOL_BINDING`]).
//!
//! Layout, in `vec4<f32>` units: the first `palette_capacity` slots are palettes (three
//! affine rows per bone, rewritten as animation plays and mirrored on the CPU so a frame's
//! changes upload as one span); the rest are vertex animations (two slots per texel,
//! position then normal, frame-major, uploaded once). Each region has a first-fit,
//! coalescing range allocator; exhaustion fails closed with [`DeformError::Full`].

use bytemuck::Zeroable;

/// One bone's skinning matrix: the top three rows of an affine transform.
pub type PaletteRows = [[f32; 4]; 3];

/// Pool errors.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum DeformError {
    /// The region has no free range large enough.
    Full,
    /// Mismatched lengths, zero counts, or non-finite values.
    Invalid,
    /// The handle is unknown or already released.
    Stale,
}

impl core::fmt::Display for DeformError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str(match self {
            DeformError::Full => "deformation pool full",
            DeformError::Invalid => "invalid deformation data",
            DeformError::Stale => "stale deformation handle",
        })
    }
}

impl std::error::Error for DeformError {}

/// First-fit range allocator over `[0, capacity)` with coalescing frees.
#[derive(Clone, Debug)]
pub struct RangeAllocator {
    /// Free ranges `(start, len)`, sorted by start, never adjacent.
    free: Vec<(u32, u32)>,
    capacity: u32,
}

impl RangeAllocator {
    /// Everything free.
    pub fn new(capacity: u32) -> Self {
        let free = if capacity > 0 {
            vec![(0, capacity)]
        } else {
            Vec::new()
        };
        Self { free, capacity }
    }

    /// Allocates `len` units starting at a multiple of `align` (at least 1).
    pub fn alloc(&mut self, len: u32, align: u32) -> Option<u32> {
        let align = align.max(1);
        if len == 0 {
            return None;
        }
        let (index, start, pad) = self.free.iter().enumerate().find_map(|(i, &(s, l))| {
            let aligned = s.checked_next_multiple_of(align)?;
            let pad = aligned - s;
            (l >= pad.checked_add(len)?).then_some((i, aligned, pad))
        })?;
        let &(s, l) = self.free.get(index)?;
        let tail_start = start + len;
        let tail_len = s + l - tail_start;
        // Replace the range with its leading pad and trailing remainder (either may be
        // empty).
        let mut parts = [(s, pad), (tail_start, tail_len)].into_iter().filter(|p| p.1 > 0);
        match (parts.next(), parts.next()) {
            (Some(a), Some(b)) => {
                if let Some(slot) = self.free.get_mut(index) {
                    *slot = a;
                }
                self.free.insert(index + 1, b);
            }
            (Some(a), None) => {
                if let Some(slot) = self.free.get_mut(index) {
                    *slot = a;
                }
            }
            _ => {
                self.free.remove(index);
            }
        }
        Some(start)
    }

    /// Returns `[start, start + len)` to the free list, merging with neighbors.
    pub fn free(&mut self, start: u32, len: u32) {
        if len == 0 || start.saturating_add(len) > self.capacity {
            return;
        }
        let at = self.free.partition_point(|&(s, _)| s < start);
        self.free.insert(at, (start, len));
        // Merge with the next, then with the previous.
        if let (Some(&(s, l)), Some(&(ns, nl))) = (self.free.get(at), self.free.get(at + 1))
            && s + l == ns
        {
            if let Some(slot) = self.free.get_mut(at) {
                *slot = (s, l + nl);
            }
            self.free.remove(at + 1);
        }
        if at > 0
            && let (Some(&(ps, pl)), Some(&(s, l))) = (self.free.get(at - 1), self.free.get(at))
            && ps + pl == s
        {
            if let Some(slot) = self.free.get_mut(at - 1) {
                *slot = (ps, pl + l);
            }
            self.free.remove(at);
        }
    }

    /// Units free in total.
    pub fn free_total(&self) -> u32 {
        self.free.iter().map(|r| r.1).sum()
    }

    /// The largest free range.
    pub fn largest_free(&self) -> u32 {
        self.free.iter().map(|r| r.1).max().unwrap_or(0)
    }
}

/// A palette allocation: `bones` bones from pool slot `base`.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct PaletteRange {
    /// First pool slot (three per bone).
    pub base: u32,
    /// Bones.
    pub bones: u32,
}

/// A registered vertex animation.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, PartialOrd, Ord)]
pub struct VatId(u32);

/// A vertex animation to upload (the layout `mantis_anim::bake_vat` produces).
#[derive(Clone, Copy, Debug)]
pub struct VatUpload<'a> {
    /// Vertices per frame.
    pub vertex_count: u32,
    /// Frames.
    pub frame_count: u32,
    /// Seconds between frames.
    pub seconds_per_frame: f32,
    /// Whether playback wraps.
    pub looping: bool,
    /// Positions, frame-major.
    pub positions: &'a [[f32; 4]],
    /// Normals, frame-major.
    pub normals: &'a [[f32; 4]],
    /// Bounds of every baked position (model space).
    pub bounds_min: [f32; 3],
    /// Bounds of every baked position (model space).
    pub bounds_max: [f32; 3],
}

/// A registered vertex animation's placement and playback parameters.
#[derive(Clone, Copy, PartialEq, Debug)]
pub struct VatInfo {
    /// First texel (pool slot / 2).
    pub first_texel: u32,
    /// Vertices per frame.
    pub vertex_count: u32,
    /// Frames.
    pub frame_count: u32,
    /// Seconds between frames.
    pub seconds_per_frame: f32,
    /// Whether playback wraps.
    pub looping: bool,
    /// Model-space bounds of every frame.
    pub bounds: crate::math::Aabb,
}

impl VatInfo {
    /// The frame position (integer part a frame, fraction the blend to the next) at
    /// `seconds` of playback.
    pub fn frame_at(&self, seconds: f32) -> f32 {
        #[expect(clippy::cast_precision_loss)] // Frame counts are far below 2^24.
        let frames = self.frame_count.max(1) as f32;
        let f = if self.seconds_per_frame > 0.0 && seconds.is_finite() {
            seconds.max(0.0) / self.seconds_per_frame
        } else {
            0.0
        };
        if self.looping {
            let wrapped = f % frames;
            if wrapped < frames { wrapped } else { 0.0 }
        } else {
            f.min(frames - 1.0)
        }
    }
}

/// The pool.
#[derive(Debug)]
pub struct DeformPool {
    buffer: wgpu::Buffer,
    palette_capacity: u32,
    palettes: RangeAllocator,
    palette_mirror: Vec<[f32; 4]>,
    dirty: Option<(u32, u32)>,
    vat_alloc: RangeAllocator,
    vats: Vec<Option<VatInfo>>,
}

/// Bytes per pool slot.
const SLOT: u64 = 16;

impl DeformPool {
    /// A pool of `palette_slots + vat_slots` `vec4` slots.
    pub fn new(device: &wgpu::Device, palette_slots: u32, vat_slots: u32) -> Self {
        let total = u64::from(palette_slots) + u64::from(vat_slots);
        let buffer = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("deform.pool"),
            size: (total * SLOT).max(SLOT),
            usage: wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        Self {
            buffer,
            palette_capacity: palette_slots,
            palettes: RangeAllocator::new(palette_slots),
            palette_mirror: vec![Zeroable::zeroed(); palette_slots as usize],
            dirty: None,
            vat_alloc: RangeAllocator::new(vat_slots),
            vats: Vec::new(),
        }
    }

    /// The storage buffer.
    pub fn buffer(&self) -> &wgpu::Buffer {
        &self.buffer
    }

    /// Reserves a palette for `bones` bones (identity until written).
    ///
    /// # Errors
    /// [`DeformError::Invalid`] for zero bones, [`DeformError::Full`].
    pub fn alloc_palette(&mut self, bones: u32) -> Result<PaletteRange, DeformError> {
        let slots = bones
            .checked_mul(3)
            .filter(|s| *s > 0)
            .ok_or(DeformError::Invalid)?;
        let base = self.palettes.alloc(slots, 1).ok_or(DeformError::Full)?;
        let range = PaletteRange { base, bones };
        let identity: PaletteRows = [[1.0, 0.0, 0.0, 0.0], [0.0, 1.0, 0.0, 0.0], [0.0, 0.0, 1.0, 0.0]];
        for b in 0..bones {
            self.write_rows(base + b * 3, &identity);
        }
        Ok(range)
    }

    /// Releases a palette.
    pub fn free_palette(&mut self, range: PaletteRange) {
        self.palettes.free(range.base, range.bones * 3);
    }

    fn write_rows(&mut self, slot: u32, rows: &PaletteRows) {
        let start = slot as usize;
        if let Some(dst) = self.palette_mirror.get_mut(start..start + 3) {
            dst.copy_from_slice(rows);
            let (lo, hi) = self.dirty.unwrap_or((slot, slot + 3));
            self.dirty = Some((lo.min(slot), hi.max(slot + 3)));
        }
    }

    /// Writes a palette (fewer entries than bones leaves the rest unchanged). Returns
    /// whether anything changed. Allocation-free.
    ///
    /// # Errors
    /// [`DeformError::Invalid`] for more entries than bones or non-finite values.
    pub fn write_palette(
        &mut self,
        range: PaletteRange,
        palette: &[PaletteRows],
    ) -> Result<bool, DeformError> {
        if palette.len() > range.bones as usize || palette.iter().flatten().flatten().any(|v| !v.is_finite())
        {
            return Err(DeformError::Invalid);
        }
        let mut changed = false;
        for (b, rows) in (0u32..).zip(palette) {
            let slot = range.base + b * 3;
            let start = slot as usize;
            if self.palette_mirror.get(start..start + 3) != Some(rows.as_slice()) {
                self.write_rows(slot, rows);
                changed = true;
            }
        }
        Ok(changed)
    }

    /// Uploads this frame's palette changes as one span.
    pub fn upload(&mut self, queue: &wgpu::Queue) {
        if let Some((lo, hi)) = self.dirty.take()
            && let Some(span) = self.palette_mirror.get(lo as usize..hi as usize)
        {
            queue.write_buffer(&self.buffer, u64::from(lo) * SLOT, bytemuck::cast_slice(span));
        }
    }

    /// Uploads a vertex animation.
    ///
    /// # Errors
    /// [`DeformError::Invalid`] for inconsistent sizes or non-finite data,
    /// [`DeformError::Full`].
    pub fn add_vat(&mut self, queue: &wgpu::Queue, vat: &VatUpload<'_>) -> Result<VatId, DeformError> {
        let texels = u64::from(vat.vertex_count) * u64::from(vat.frame_count);
        let consistent = texels > 0
            && vat.positions.len() as u64 == texels
            && vat.normals.len() as u64 == texels
            && vat.seconds_per_frame.is_finite()
            && vat.seconds_per_frame > 0.0
            && vat
                .positions
                .iter()
                .chain(vat.normals)
                .flatten()
                .all(|v| v.is_finite())
            && vat
                .bounds_min
                .iter()
                .chain(&vat.bounds_max)
                .all(|v| v.is_finite());
        if !consistent {
            return Err(DeformError::Invalid);
        }
        let slots = u32::try_from(texels * 2).map_err(|_| DeformError::Full)?;
        let start = self.vat_alloc.alloc(slots, 2).ok_or(DeformError::Full)?;
        let absolute = self.palette_capacity + start;
        // Interleave position and normal per texel.
        let mut interleaved: Vec<[f32; 4]> = Vec::with_capacity(slots as usize);
        for (p, n) in vat.positions.iter().zip(vat.normals) {
            interleaved.push(*p);
            interleaved.push(*n);
        }
        queue.write_buffer(
            &self.buffer,
            u64::from(absolute) * SLOT,
            bytemuck::cast_slice(&interleaved),
        );
        let info = VatInfo {
            first_texel: absolute / 2,
            vertex_count: vat.vertex_count,
            frame_count: vat.frame_count,
            seconds_per_frame: vat.seconds_per_frame,
            looping: vat.looping,
            bounds: crate::math::Aabb {
                min: vat.bounds_min.into(),
                max: vat.bounds_max.into(),
            },
        };
        let id = if let Some(slot) = self.vats.iter_mut().position(|v| v.is_none()) {
            if let Some(entry) = self.vats.get_mut(slot) {
                *entry = Some(info);
            }
            slot
        } else {
            self.vats.push(Some(info));
            self.vats.len() - 1
        };
        Ok(VatId(u32::try_from(id).map_err(|_| DeformError::Full)?))
    }

    /// A vertex animation's placement.
    pub fn vat(&self, id: VatId) -> Option<&VatInfo> {
        self.vats.get(id.0 as usize).and_then(Option::as_ref)
    }

    /// Releases a vertex animation (instances still using it draw garbage; the caller
    /// despawns them first).
    ///
    /// # Errors
    /// [`DeformError::Stale`].
    pub fn remove_vat(&mut self, id: VatId) -> Result<(), DeformError> {
        let info = self
            .vats
            .get_mut(id.0 as usize)
            .and_then(Option::take)
            .ok_or(DeformError::Stale)?;
        let start = info.first_texel * 2 - self.palette_capacity;
        self.vat_alloc
            .free(start, info.vertex_count * info.frame_count * 2);
        Ok(())
    }

    /// Free palette and vertex animation slots.
    pub fn free_slots(&self) -> (u32, u32) {
        (self.palettes.free_total(), self.vat_alloc.free_total())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ranges_allocate_first_fit_with_alignment_and_coalesce() {
        let mut a = RangeAllocator::new(100);
        assert_eq!(a.alloc(10, 1), Some(0));
        assert_eq!(a.alloc(5, 4), Some(12), "aligned past the 2-unit pad");
        assert_eq!(a.alloc(3, 1), Some(17), "the 2-unit pad is too small");
        assert_eq!(a.alloc(2, 1), Some(10), "the pad is reused first-fit");
        assert_eq!(a.alloc(100, 1), None);
        a.free(0, 10);
        a.free(10, 2);
        a.free(17, 3);
        assert_eq!(a.largest_free(), 100 - 17, "the tail");
        a.free(12, 5);
        assert_eq!((a.free_total(), a.largest_free()), (100, 100), "fully coalesced");
        assert_eq!(a.alloc(0, 1), None);
        a.free(90, 20); // Out of range: ignored.
        assert_eq!(a.free_total(), 100);
    }

    #[test]
    fn vat_frames_wrap_or_clamp() {
        let info = |looping| VatInfo {
            first_texel: 0,
            vertex_count: 1,
            frame_count: 10,
            seconds_per_frame: 0.1,
            looping,
            bounds: crate::math::Aabb {
                min: glam::Vec3::ZERO,
                max: glam::Vec3::ONE,
            },
        };
        assert!((info(true).frame_at(0.25) - 2.5).abs() < 1e-4);
        assert!((info(true).frame_at(1.25) - 2.5).abs() < 1e-3, "wraps");
        assert!(
            (info(false).frame_at(5.0) - 9.0).abs() < 1e-6,
            "clamps on the last frame"
        );
        assert_eq!(info(true).frame_at(f32::NAN), 0.0);
    }
}

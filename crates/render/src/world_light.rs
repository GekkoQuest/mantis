//! Cooked indirect light for a streamed world (plan 8.3, decision 0003): every resident
//! sector's lightmaps and probe volume, bound at once so one frame draws many sectors.
//!
//! - **Lightmap pages.** One `Rgb9e5Ufloat` 2D array of square pages. A sector lightmap
//!   (at most a page in each dimension) takes one layer per time-of-day keyframe; its
//!   [`LightmapPage`] is a slot in the page table, which holds the two layers to blend and
//!   the weight for the current time. A lightmapped instance carries its page slot and
//!   its uv rectangle in page coordinates ([`WorldLight::page_rect`]), so batches spanning
//!   many sectors share one pipeline and one binding.
//! - **Probe atlas.** Three `Rgba16Float` 3D textures (red, green, blue L1 coefficients)
//!   divided into bricks; a sector's volume (at most a brick per axis) is blended to the
//!   current time, dilated over invalid probes, and written into its brick. A toroidal
//!   table of `side x side` world sectors maps the sector containing a shaded point to its
//!   brick; points in sectors without probes fall back to the sky.
//!
//! Loading and unloading write textures and tables through the queue. [`WorldLight::flush`]
//! (once per frame) is allocation-free. Changing the time re-blends the probe bricks; the
//! caller decides how often. Page blends are recomputed every flush at no upload cost
//! unless they changed.

use mantis_formats::lightmap::Lightmap;
use mantis_formats::probe_volume::ProbeVolume;
use mantis_formats::sh::ShL1;
use mantis_formats::time_of_day::keyframe_blend;

use crate::lighting::indirect::blend_probes;
use crate::textures::dilate;

/// Capacities of the world's indirect light.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct WorldLightConfig {
    /// Edge of a lightmap page in texels.
    pub page_size: u32,
    /// Layers in the page array (each lightmap keyframe takes one).
    pub page_layers: u32,
    /// Lightmaps resident at once (page table slots).
    pub pages: u32,
    /// Probes per axis of a brick (the largest sector volume).
    pub brick: [u32; 3],
    /// Bricks across x and z of the atlas.
    pub bricks: [u32; 2],
    /// Side of the toroidal sector table: sectors resident at once must differ in
    /// coordinates modulo this.
    pub table_side: u32,
}

impl Default for WorldLightConfig {
    fn default() -> Self {
        Self {
            page_size: 512,
            page_layers: 16,
            pages: 16,
            brick: [16, 8, 16],
            bricks: [4, 4],
            table_side: 8,
        }
    }
}

/// A resident lightmap's page table slot.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct LightmapPage(pub u32);

/// Why indirect light could not be loaded.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum WorldLightError {
    /// The lightmap is larger than a page, or has no keyframe layers.
    LightmapSize,
    /// Every page table slot is in use.
    PagesFull,
    /// Not enough free layers for the lightmap's keyframes.
    LayersFull,
    /// The probe volume is larger than a brick, or malformed.
    ProbeSize,
    /// Every brick is in use.
    BricksFull,
    /// Another resident sector uses the same table slot (coordinates equal modulo the
    /// table side), or this sector's probes are already loaded.
    TableCollision,
    /// The sector size is not positive or differs from the resident sectors'.
    SectorSize,
    /// No such page or sector is resident.
    NotResident,
}

impl core::fmt::Display for WorldLightError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        let what = match self {
            WorldLightError::LightmapSize => "lightmap larger than a page or without layers",
            WorldLightError::PagesFull => "lightmap page table full",
            WorldLightError::LayersFull => "lightmap page layers full",
            WorldLightError::ProbeSize => "probe volume larger than a brick or malformed",
            WorldLightError::BricksFull => "probe bricks full",
            WorldLightError::TableCollision => "probe sector table slot taken",
            WorldLightError::SectorSize => "sector size invalid or differs from resident sectors",
            WorldLightError::NotResident => "not resident",
        };
        f.write_str(what)
    }
}

impl std::error::Error for WorldLightError {}

/// One probe sector table slot as the GPU reads it (80 bytes).
#[repr(C)]
#[derive(Clone, Copy, PartialEq, Debug, bytemuck::Pod, bytemuck::Zeroable)]
pub struct GpuProbeSector {
    /// Sector x, sector z, 1 when resident, unused.
    pub coord: [i32; 4],
    /// World position of probe (0, 0, 0).
    pub origin: [f32; 4],
    /// `1 / (spacing * (n - 1))` per axis (0 for a single probe).
    pub inv_extent: [f32; 4],
    /// Center of the brick's first texel in the atlas, in texels.
    pub brick: [f32; 4],
    /// Probes minus one per axis.
    pub span: [f32; 4],
}

#[derive(Debug)]
struct Page {
    width: u32,
    height: u32,
    keyframes: Vec<f32>,
    layers: Vec<u32>,
}

#[derive(Debug)]
struct ProbeSector {
    coord: (i32, i32),
    brick: u32,
    volume: ProbeVolume,
}

/// The world's indirect light.
#[derive(Debug)]
pub struct WorldLight {
    config: WorldLightConfig,
    page_array: wgpu::Texture,
    page_view: wgpu::TextureView,
    page_table: wgpu::Buffer,
    probe_textures: [wgpu::Texture; 3],
    probe_views: [wgpu::TextureView; 3],
    probe_table: wgpu::Buffer,
    pages: Vec<Option<Page>>,
    free_layers: Vec<u32>,
    sectors: Vec<Option<ProbeSector>>,
    free_bricks: Vec<u32>,
    sector_size: Option<f32>,
    time: f32,
    page_blends: Vec<[f32; 4]>,
    table: Vec<GpuProbeSector>,
    blended: Vec<ShL1>,
    bytes: Vec<u8>,
    tables_dirty: bool,
}

fn storage(device: &wgpu::Device, label: &str, size: u64) -> wgpu::Buffer {
    device.create_buffer(&wgpu::BufferDescriptor {
        label: Some(label),
        size: size.max(16),
        usage: wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_DST,
        mapped_at_creation: false,
    })
}

fn texture_copy(texture: &wgpu::Texture, [x, y, z]: [u32; 3]) -> wgpu::TexelCopyTextureInfo<'_> {
    wgpu::TexelCopyTextureInfo {
        texture,
        mip_level: 0,
        origin: wgpu::Origin3d { x, y, z },
        aspect: wgpu::TextureAspect::All,
    }
}

impl WorldLight {
    /// Empty indirect light with `config`'s capacities (zero sizes are raised to one, and
    /// layers to two).
    pub fn new(device: &wgpu::Device, config: WorldLightConfig) -> Self {
        let config = WorldLightConfig {
            page_size: config.page_size.max(1),
            page_layers: config.page_layers.max(2),
            pages: config.pages.max(1),
            brick: config.brick.map(|b| b.max(1)),
            bricks: config.bricks.map(|b| b.max(1)),
            table_side: config.table_side.max(1),
        };
        let page_array = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("lightmap.pages"),
            size: wgpu::Extent3d {
                width: config.page_size,
                height: config.page_size,
                depth_or_array_layers: config.page_layers,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::Rgb9e5Ufloat,
            usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
            view_formats: &[],
        });
        let page_view = page_array.create_view(&wgpu::TextureViewDescriptor {
            dimension: Some(wgpu::TextureViewDimension::D2Array),
            ..Default::default()
        });
        let [bx, by, bz] = config.brick;
        let [nx, nz] = config.bricks;
        let atlas = wgpu::Extent3d {
            width: bx.saturating_mul(nx),
            height: by,
            depth_or_array_layers: bz.saturating_mul(nz),
        };
        let probe_texture = || {
            device.create_texture(&wgpu::TextureDescriptor {
                label: Some("probes.atlas"),
                size: atlas,
                mip_level_count: 1,
                sample_count: 1,
                dimension: wgpu::TextureDimension::D3,
                format: wgpu::TextureFormat::Rgba16Float,
                usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
                view_formats: &[],
            })
        };
        let probe_textures = [probe_texture(), probe_texture(), probe_texture()];
        let probe_views = probe_textures
            .each_ref()
            .map(|t| t.create_view(&wgpu::TextureViewDescriptor::default()));
        let side = config.table_side as usize;
        let table_len = side * side;
        let page_table = storage(device, "lightmap.page_table", u64::from(config.pages) * 16);
        let probe_table = storage(
            device,
            "probes.sector_table",
            (table_len * core::mem::size_of::<GpuProbeSector>()) as u64,
        );
        Self {
            config,
            page_array,
            page_view,
            page_table,
            probe_textures,
            probe_views,
            probe_table,
            pages: (0..config.pages).map(|_| None).collect(),
            free_layers: (0..config.page_layers).rev().collect(),
            sectors: (0..table_len).map(|_| None).collect(),
            free_bricks: (0..nx.saturating_mul(nz)).rev().collect(),
            sector_size: None,
            time: 0.0,
            page_blends: vec![[0.0; 4]; config.pages as usize],
            table: vec![bytemuck::Zeroable::zeroed(); table_len],
            blended: Vec::new(),
            bytes: Vec::new(),
            tables_dirty: true,
        }
    }

    /// The capacities in use.
    pub fn config(&self) -> &WorldLightConfig {
        &self.config
    }

    /// The page array view (forward binding 11).
    pub fn page_view(&self) -> &wgpu::TextureView {
        &self.page_view
    }

    /// The page table (forward binding 13).
    pub fn page_table(&self) -> &wgpu::Buffer {
        &self.page_table
    }

    /// The red, green, and blue probe atlas views (forward bindings 7 to 9).
    pub fn probe_views(&self) -> &[wgpu::TextureView; 3] {
        &self.probe_views
    }

    /// The probe sector table (forward binding 14).
    pub fn probe_table(&self) -> &wgpu::Buffer {
        &self.probe_table
    }

    /// Resident lightmaps.
    pub fn resident_pages(&self) -> usize {
        self.pages.iter().flatten().count()
    }

    /// Resident probe sectors.
    pub fn resident_sectors(&self) -> usize {
        self.sectors.iter().flatten().count()
    }

    /// Free lightmap layers.
    pub fn free_layers(&self) -> usize {
        self.free_layers.len()
    }

    /// Uploads a sector lightmap into free layers.
    ///
    /// # Errors
    /// [`WorldLightError::LightmapSize`], [`WorldLightError::PagesFull`], or
    /// [`WorldLightError::LayersFull`]; nothing is written.
    pub fn add_lightmap(
        &mut self,
        queue: &wgpu::Queue,
        lightmap: &Lightmap,
    ) -> Result<LightmapPage, WorldLightError> {
        let (w, h) = (lightmap.width, lightmap.height);
        let texels = (w as usize).saturating_mul(h as usize);
        if w == 0
            || h == 0
            || w > self.config.page_size
            || h > self.config.page_size
            || lightmap.layers.is_empty()
            || lightmap.layers.iter().any(|l| l.len() != texels)
        {
            return Err(WorldLightError::LightmapSize);
        }
        let slot = self
            .pages
            .iter()
            .position(Option::is_none)
            .ok_or(WorldLightError::PagesFull)?;
        if self.free_layers.len() < lightmap.layers.len() {
            return Err(WorldLightError::LayersFull);
        }
        let mut layers = Vec::with_capacity(lightmap.layers.len());
        for texels in &lightmap.layers {
            let Some(layer) = self.free_layers.pop() else {
                return Err(WorldLightError::LayersFull);
            };
            self.bytes.clear();
            self.bytes.extend(texels.iter().flat_map(|t| t.to_le_bytes()));
            queue.write_texture(
                texture_copy(&self.page_array, [0, 0, layer]),
                &self.bytes,
                wgpu::TexelCopyBufferLayout {
                    offset: 0,
                    bytes_per_row: Some(w * 4),
                    rows_per_image: Some(h),
                },
                wgpu::Extent3d {
                    width: w,
                    height: h,
                    depth_or_array_layers: 1,
                },
            );
            layers.push(layer);
        }
        if let Some(entry) = self.pages.get_mut(slot) {
            *entry = Some(Page {
                width: w,
                height: h,
                keyframes: lightmap.keyframes.clone(),
                layers,
            });
        }
        self.tables_dirty = true;
        Ok(LightmapPage(u32::try_from(slot).unwrap_or(u32::MAX)))
    }

    /// Releases a lightmap's page and layers. Instances still naming it read whatever
    /// the slot holds next; despawn them first.
    ///
    /// # Errors
    /// [`WorldLightError::NotResident`].
    pub fn remove_lightmap(&mut self, page: LightmapPage) -> Result<(), WorldLightError> {
        let entry = self
            .pages
            .get_mut(page.0 as usize)
            .and_then(Option::take)
            .ok_or(WorldLightError::NotResident)?;
        self.free_layers.extend(entry.layers);
        if let Some(b) = self.page_blends.get_mut(page.0 as usize) {
            *b = [0.0; 4];
        }
        self.tables_dirty = true;
        Ok(())
    }

    /// A placement's lightmap uv rectangle (`uv_scale`, `uv_offset`, in the sector
    /// lightmap's 0 to 1 space) in page coordinates: `[scale u, scale v, offset u,
    /// offset v]` for the instance.
    ///
    /// # Errors
    /// [`WorldLightError::NotResident`].
    #[allow(clippy::cast_precision_loss)] // Page sizes are far below 2^24.
    pub fn page_rect(
        &self,
        page: LightmapPage,
        uv_scale: [f32; 2],
        uv_offset: [f32; 2],
    ) -> Result<[f32; 4], WorldLightError> {
        let p = self
            .pages
            .get(page.0 as usize)
            .and_then(Option::as_ref)
            .ok_or(WorldLightError::NotResident)?;
        let size = self.config.page_size as f32;
        let (su, sv) = (p.width as f32 / size, p.height as f32 / size);
        let [scale_u, scale_v] = uv_scale;
        let [offset_u, offset_v] = uv_offset;
        Ok([scale_u * su, scale_v * sv, offset_u * su, offset_v * sv])
    }

    /// Uploads a sector's probe volume, blended to the current time, into a free brick.
    ///
    /// # Errors
    /// [`WorldLightError::ProbeSize`], [`WorldLightError::SectorSize`],
    /// [`WorldLightError::TableCollision`], or [`WorldLightError::BricksFull`]; nothing is
    /// written.
    pub fn add_probes(
        &mut self,
        queue: &wgpu::Queue,
        coord: (i32, i32),
        sector_size: f32,
        volume: ProbeVolume,
    ) -> Result<(), WorldLightError> {
        let [bx, by, bz] = self.config.brick;
        let [nx, ny, nz] = volume.dims;
        let count = (nx as usize) * (ny as usize) * (nz as usize);
        if nx == 0
            || ny == 0
            || nz == 0
            || nx > bx
            || ny > by
            || nz > bz
            || volume.probes.is_empty()
            || volume.probes.iter().any(|k| k.len() != count)
        {
            return Err(WorldLightError::ProbeSize);
        }
        if !(sector_size.is_finite() && sector_size > 0.0) {
            return Err(WorldLightError::SectorSize);
        }
        if let Some(size) = self.sector_size
            && self.resident_sectors() > 0
            && (size - sector_size).abs() > size * 1e-6
        {
            return Err(WorldLightError::SectorSize);
        }
        let slot = self.table_slot(coord);
        if self.sectors.get(slot).is_none_or(Option::is_some) {
            return Err(WorldLightError::TableCollision);
        }
        let brick = self.free_bricks.pop().ok_or(WorldLightError::BricksFull)?;
        self.sector_size = Some(sector_size);
        let sector = ProbeSector { coord, brick, volume };
        self.write_brick(queue, &sector);
        if let Some(entry) = self.sectors.get_mut(slot) {
            *entry = Some(sector);
        }
        self.tables_dirty = true;
        Ok(())
    }

    /// Releases a sector's probe brick.
    ///
    /// # Errors
    /// [`WorldLightError::NotResident`].
    pub fn remove_probes(&mut self, coord: (i32, i32)) -> Result<(), WorldLightError> {
        let slot = self.table_slot(coord);
        let entry = self.sectors.get_mut(slot).ok_or(WorldLightError::NotResident)?;
        if entry.as_ref().is_none_or(|s| s.coord != coord) {
            return Err(WorldLightError::NotResident);
        }
        if let Some(sector) = entry.take() {
            self.free_bricks.push(sector.brick);
        }
        self.tables_dirty = true;
        Ok(())
    }

    /// Sets the time of day (day fraction) and re-blends every resident probe brick when
    /// it changed.
    pub fn set_time(&mut self, queue: &wgpu::Queue, time: f32) {
        if time.to_bits() == self.time.to_bits() {
            return;
        }
        self.time = time;
        let sectors = core::mem::take(&mut self.sectors);
        for sector in sectors.iter().flatten() {
            self.write_brick(queue, sector);
        }
        self.sectors = sectors;
        self.tables_dirty = true;
    }

    /// The time of day in use.
    pub fn time(&self) -> f32 {
        self.time
    }

    fn table_slot(&self, (x, z): (i32, i32)) -> usize {
        let side = i32::try_from(self.config.table_side).unwrap_or(1).max(1);
        usize::try_from(x.rem_euclid(side) + z.rem_euclid(side) * side).unwrap_or(0)
    }

    fn brick_origin(&self, brick: u32) -> [u32; 3] {
        let [bx, _, bz] = self.config.brick;
        let across = self.config.bricks[0].max(1);
        [(brick % across) * bx, 0, (brick / across) * bz]
    }

    fn write_brick(&mut self, queue: &wgpu::Queue, sector: &ProbeSector) {
        let volume = &sector.volume;
        blend_probes(volume, self.time, &mut self.blended);
        dilate(volume, &mut self.blended);
        let [nx, ny, nz] = volume.dims;
        let origin = self.brick_origin(sector.brick);
        for (channel, texture) in self.probe_textures.iter().enumerate() {
            self.bytes.clear();
            for sh in &self.blended {
                for c in sh.rgb.get(channel).copied().unwrap_or([0.0; 4]) {
                    self.bytes
                        .extend_from_slice(&mantis_formats::half::f32_to_f16(c).to_le_bytes());
                }
            }
            queue.write_texture(
                texture_copy(texture, origin),
                &self.bytes,
                wgpu::TexelCopyBufferLayout {
                    offset: 0,
                    bytes_per_row: Some(nx * 8),
                    rows_per_image: Some(ny),
                },
                wgpu::Extent3d {
                    width: nx,
                    height: ny,
                    depth_or_array_layers: nz,
                },
            );
        }
    }

    /// The frame uniform's `probe_grid` and `probe_atlas` (with `ambient` in w).
    #[allow(clippy::cast_precision_loss)] // Table and atlas sizes are far below 2^24.
    pub fn frame_params(&self, ambient: f32) -> ([f32; 4], [f32; 4]) {
        let size = self.sector_size.unwrap_or(1.0);
        let any = if self.resident_sectors() > 0 { 1.0 } else { 0.0 };
        let [bx, by, bz] = self.config.brick;
        let [nx, nz] = self.config.bricks;
        (
            [1.0 / size, self.config.table_side as f32, any, 0.0],
            [
                1.0 / (bx * nx) as f32,
                1.0 / by as f32,
                1.0 / (bz * nz) as f32,
                ambient,
            ],
        )
    }

    #[allow(clippy::cast_precision_loss)] // Probe counts and texel offsets are small.
    fn gpu_sector(&self, s: &ProbeSector) -> GpuProbeSector {
        let v = &s.volume;
        let [ox, oy, oz] = self.brick_origin(s.brick);
        let axis = |i: usize| {
            let n = v.dims.get(i).copied().unwrap_or(1);
            let spacing = v.spacing.get(i).copied().unwrap_or(1.0);
            let inv = if n > 1 && spacing > 0.0 {
                1.0 / (spacing * (n - 1) as f32)
            } else {
                0.0
            };
            (
                inv,
                n.saturating_sub(1) as f32,
                v.origin.get(i).copied().unwrap_or(0.0),
            )
        };
        let (ix, sx, px) = axis(0);
        let (iy, sy, py) = axis(1);
        let (iz, sz, pz) = axis(2);
        GpuProbeSector {
            coord: [s.coord.0, s.coord.1, 1, 0],
            origin: [px, py, pz, 0.0],
            inv_extent: [ix, iy, iz, 0.0],
            brick: [ox as f32 + 0.5, oy as f32 + 0.5, oz as f32 + 0.5, 0.0],
            span: [sx, sy, sz, 0.0],
        }
    }

    /// Recomputes the page blends for the current time and writes the tables when
    /// anything changed. Allocation-free.
    #[allow(clippy::cast_precision_loss)] // Layer indices are small.
    pub fn flush(&mut self, queue: &wgpu::Queue) {
        for (blend, page) in self.page_blends.iter_mut().zip(&self.pages) {
            let Some(page) = page else { continue };
            let (a, b, t) = keyframe_blend(&page.keyframes, self.time);
            let la = page.layers.get(a).or(page.layers.first()).copied().unwrap_or(0);
            let lb = page.layers.get(b).copied().unwrap_or(la);
            let next = [la as f32, lb as f32, t, 0.0];
            if *blend != next {
                *blend = next;
                self.tables_dirty = true;
            }
        }
        if !self.tables_dirty {
            return;
        }
        self.tables_dirty = false;
        for i in 0..self.table.len() {
            let gpu = match self.sectors.get(i).and_then(Option::as_ref) {
                Some(s) => self.gpu_sector(s),
                None => bytemuck::Zeroable::zeroed(),
            };
            if let Some(slot) = self.table.get_mut(i) {
                *slot = gpu;
            }
        }
        queue.write_buffer(&self.probe_table, 0, bytemuck::cast_slice(&self.table));
        queue.write_buffer(&self.page_table, 0, bytemuck::cast_slice(&self.page_blends));
    }
}

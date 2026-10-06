//! Helpers for GPU tests in any Mantis crate.
//!
//! Two tiers:
//! - [`noop`]: the `wgpu` no-op backend. API-validation tests use it and always run; a
//!   failure to create it fails the test.
//! - [`hardware_or_skip`]: a real headless adapter for pixel-readback tests. When no
//!   adapter exists the test is skipped, and the skip is never silent: it is counted and
//!   printed as one greppable line,
//!   `MANTIS-GPU-SKIP test=<name> reason=<why>`, so a run summary can report
//!   "N GPU tests skipped (no adapter)" with
//!   `cargo test 2>&1 | grep -c MANTIS-GPU-SKIP`.
//!
//! No test opens a window.

use std::sync::atomic::{AtomicU32, Ordering};

use crate::gpu::{GpuContext, GpuError, HeadlessKind};

static SKIPPED: AtomicU32 = AtomicU32::new(0);

/// The greppable prefix of every skip line.
pub const SKIP_MARKER: &str = "MANTIS-GPU-SKIP";

/// A real headless adapter, or `None` after recording and printing a skip.
pub fn hardware_or_skip(test: &str) -> Option<GpuContext> {
    match GpuContext::headless(HeadlessKind::Hardware) {
        Ok(ctx) => Some(ctx),
        Err(e) => {
            record_skip(test, &e.to_string());
            None
        }
    }
}

/// The no-op backend context.
///
/// # Errors
/// [`GpuError`] if the no-op backend is unavailable (the `noop` feature is off).
pub fn noop() -> Result<GpuContext, GpuError> {
    GpuContext::headless(HeadlessKind::Noop)
}

/// Records and prints one skip.
pub fn record_skip(test: &str, reason: &str) {
    SKIPPED.fetch_add(1, Ordering::Relaxed);
    eprintln!("{SKIP_MARKER} test={test} reason={reason}");
}

/// Skips recorded in this process.
pub fn skipped() -> u32 {
    SKIPPED.load(Ordering::Relaxed)
}

/// Runs `f` inside a validation error scope and returns the first validation error, if
/// any, as text.
pub fn validation_errors(device: &wgpu::Device, f: impl FnOnce()) -> Option<String> {
    let scope = device.push_error_scope(wgpu::ErrorFilter::Validation);
    f();
    pollster::block_on(scope.pop()).map(|e| e.to_string())
}

/// Reads back a 2D texture with a 4-byte texel format (for example `Rgba8Unorm`) as tightly
/// packed rows. The texture needs `COPY_SRC`.
///
/// # Errors
/// A description of any mapping or polling failure.
pub fn read_texture_4bpp(ctx: &GpuContext, texture: &wgpu::Texture) -> Result<Vec<u8>, String> {
    let (w, h) = (texture.width(), texture.height());
    let row = w * 4;
    let padded = row.next_multiple_of(wgpu::COPY_BYTES_PER_ROW_ALIGNMENT);
    let buffer = ctx.device.create_buffer(&wgpu::BufferDescriptor {
        label: Some("readback"),
        size: u64::from(padded) * u64::from(h),
        usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
        mapped_at_creation: false,
    });
    let mut encoder = ctx
        .device
        .create_command_encoder(&wgpu::CommandEncoderDescriptor {
            label: Some("readback"),
        });
    encoder.copy_texture_to_buffer(
        wgpu::TexelCopyTextureInfo {
            texture,
            mip_level: 0,
            origin: wgpu::Origin3d::ZERO,
            aspect: wgpu::TextureAspect::All,
        },
        wgpu::TexelCopyBufferInfo {
            buffer: &buffer,
            layout: wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(padded),
                rows_per_image: Some(h),
            },
        },
        wgpu::Extent3d {
            width: w,
            height: h,
            depth_or_array_layers: 1,
        },
    );
    let _ = ctx.queue.submit(Some(encoder.finish()));
    let (tx, rx) = std::sync::mpsc::channel();
    buffer.map_async(wgpu::MapMode::Read, .., move |r| {
        let _ = tx.send(r);
    });
    ctx.device
        .poll(wgpu::PollType::wait_indefinitely())
        .map_err(|e| e.to_string())?;
    rx.recv().map_err(|e| e.to_string())?.map_err(|e| e.to_string())?;
    let data = buffer.get_mapped_range(..).map_err(|e| e.to_string())?;
    let mut out = Vec::with_capacity((row * h) as usize);
    for y in 0..h as usize {
        let start = y * padded as usize;
        out.extend_from_slice(data.get(start..start + row as usize).ok_or("short readback")?);
    }
    drop(data);
    buffer.unmap();
    Ok(out)
}

/// Reads back `size` bytes of a buffer that has `COPY_SRC`.
///
/// # Errors
/// A description of any mapping or polling failure.
pub fn read_buffer(ctx: &GpuContext, source: &wgpu::Buffer, size: u64) -> Result<Vec<u8>, String> {
    let size = size.next_multiple_of(wgpu::COPY_BUFFER_ALIGNMENT);
    let staging = ctx.device.create_buffer(&wgpu::BufferDescriptor {
        label: Some("readback"),
        size,
        usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
        mapped_at_creation: false,
    });
    let mut encoder = ctx
        .device
        .create_command_encoder(&wgpu::CommandEncoderDescriptor {
            label: Some("readback"),
        });
    encoder.copy_buffer_to_buffer(source, 0, &staging, 0, size);
    let _ = ctx.queue.submit(Some(encoder.finish()));
    let (tx, rx) = std::sync::mpsc::channel();
    staging.map_async(wgpu::MapMode::Read, .., move |r| {
        let _ = tx.send(r);
    });
    ctx.device
        .poll(wgpu::PollType::wait_indefinitely())
        .map_err(|e| e.to_string())?;
    rx.recv().map_err(|e| e.to_string())?.map_err(|e| e.to_string())?;
    let data = staging.get_mapped_range(..).map_err(|e| e.to_string())?.to_vec();
    staging.unmap();
    Ok(data)
}

/// Parses and validates WGSL with `naga`, without a device.
///
/// # Errors
/// The parse or validation error, rendered with the source.
pub fn validate_wgsl(source: &str) -> Result<(), String> {
    use wgpu::naga;
    let module = naga::front::wgsl::parse_str(source).map_err(|e| e.emit_to_string(source))?;
    naga::valid::Validator::new(
        naga::valid::ValidationFlags::all(),
        naga::valid::Capabilities::all(),
    )
    .validate(&module)
    .map_err(|e| e.emit_to_string(source))?;
    Ok(())
}

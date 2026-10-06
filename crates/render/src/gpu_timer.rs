//! GPU frame timing with timestamp queries (plan 17's GPU frame time row).
//!
//! [`FrameTimer::begin`] and [`FrameTimer::end`] each record an empty compute pass whose
//! only job is a timestamp write, so a frame's commands recorded between them into the
//! same encoder are bracketed without the `TIMESTAMP_QUERY_INSIDE_ENCODERS` feature.
//! `end` also resolves both timestamps and copies them to a mappable buffer; after the
//! submission, [`FrameTimer::read`] waits for the GPU and returns the elapsed time.
//!
//! A device without [`crate::gpu::Capabilities::timestamp_queries`] has no timer
//! ([`FrameTimer::new`] returns `None`): callers record a counted skip.

/// Bytes of the two resolved timestamps.
const RESOLVED: u64 = 2 * 8;

/// Brackets a frame's GPU work with two timestamps.
#[derive(Debug)]
pub struct FrameTimer {
    queries: wgpu::QuerySet,
    resolve: wgpu::Buffer,
    readback: wgpu::Buffer,
    period_ns: f64,
}

impl FrameTimer {
    /// A timer for `device`, or `None` when the device was created without timestamp
    /// queries.
    pub fn new(device: &wgpu::Device, queue: &wgpu::Queue) -> Option<Self> {
        if !device.features().contains(wgpu::Features::TIMESTAMP_QUERY) {
            return None;
        }
        let queries = device.create_query_set(&wgpu::QuerySetDescriptor {
            label: Some("frame timer"),
            ty: wgpu::QueryType::Timestamp,
            count: 2,
        });
        let resolve = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("frame timer resolve"),
            size: RESOLVED,
            usage: wgpu::BufferUsages::QUERY_RESOLVE | wgpu::BufferUsages::COPY_SRC,
            mapped_at_creation: false,
        });
        let readback = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("frame timer readback"),
            size: RESOLVED,
            usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
            mapped_at_creation: false,
        });
        Some(Self {
            queries,
            resolve,
            readback,
            period_ns: f64::from(queue.get_timestamp_period()),
        })
    }

    fn stamp(&self, encoder: &mut wgpu::CommandEncoder, beginning: Option<u32>, end: Option<u32>) {
        let pass = encoder.begin_compute_pass(&wgpu::ComputePassDescriptor {
            label: Some("frame timer"),
            timestamp_writes: Some(wgpu::ComputePassTimestampWrites {
                query_set: &self.queries,
                beginning_of_pass_write_index: beginning,
                end_of_pass_write_index: end,
            }),
        });
        drop(pass);
    }

    /// Records the frame's start timestamp (record the frame's commands next).
    pub fn begin(&self, encoder: &mut wgpu::CommandEncoder) {
        self.stamp(encoder, Some(0), None);
    }

    /// Records the frame's end timestamp and copies both to the readback buffer.
    pub fn end(&self, encoder: &mut wgpu::CommandEncoder) {
        self.stamp(encoder, None, Some(1));
        encoder.resolve_query_set(&self.queries, 0..2, &self.resolve, 0);
        encoder.copy_buffer_to_buffer(&self.resolve, 0, &self.readback, 0, RESOLVED);
    }

    /// Waits for the submitted frame and returns its GPU time in milliseconds, or `None`
    /// when the timestamps are unusable (the end precedes the start, as some drivers
    /// report across a power-state change).
    ///
    /// # Errors
    /// A description of a mapping or polling failure.
    pub fn read(&self, device: &wgpu::Device) -> Result<Option<f64>, String> {
        let (tx, rx) = std::sync::mpsc::channel();
        self.readback.map_async(wgpu::MapMode::Read, .., move |r| {
            let _ = tx.send(r);
        });
        device
            .poll(wgpu::PollType::wait_indefinitely())
            .map_err(|e| e.to_string())?;
        rx.recv().map_err(|e| e.to_string())?.map_err(|e| e.to_string())?;
        let data = self.readback.get_mapped_range(..).map_err(|e| e.to_string())?;
        let (stamps, _) = data.as_chunks::<8>();
        let ticks: Vec<u64> = stamps.iter().map(|b| u64::from_le_bytes(*b)).collect();
        drop(data);
        self.readback.unmap();
        let [start, end] = ticks.as_slice() else {
            return Err("timestamp readback is not two values".to_owned());
        };
        if end <= start {
            return Ok(None);
        }
        #[allow(clippy::cast_precision_loss)] // A frame's tick count is far below 2^52.
        let ms = (end - start) as f64 * self.period_ns / 1.0e6;
        Ok(Some(ms))
    }
}

//! GPU context: instance, adapter, device, queue, and the optional capabilities the
//! renderer adapts to.
//!
//! Windowed contexts are created by the client's platform layer and wrapped with
//! [`GpuContext::from_parts`]. Headless contexts (tests, tools) come from
//! [`GpuContext::headless`]: a real adapter with no surface, or the `wgpu` no-op backend,
//! which runs full API validation without a GPU.

/// Which device a headless context uses.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum HeadlessKind {
    /// A real adapter (any backend), no surface.
    Hardware,
    /// The `wgpu` no-op backend: validates every call, executes nothing. Available when
    /// `wgpu`'s `noop` feature is enabled (it is in this crate's tests).
    Noop,
}

/// Optional features the renderer uses when present and works around when absent.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
#[allow(clippy::struct_excessive_bools)] // Independent device features, not a state machine.
pub struct Capabilities {
    /// Arrays of sampled textures indexed non-uniformly in shaders (bindless).
    pub bindless_textures: bool,
    /// Indirect draws may use a nonzero first instance.
    pub indirect_first_instance: bool,
    /// Multi-draw indirect with a GPU-written draw count.
    pub multi_draw_indirect_count: bool,
    /// BC1 to BC7 block-compressed textures (cooked textures are BC-encoded).
    pub compressed_textures: bool,
    /// Timestamp queries written at pass boundaries (GPU frame timing,
    /// [`crate::gpu_timer::FrameTimer`]).
    pub timestamp_queries: bool,
}

impl Capabilities {
    /// Features to request for these capabilities.
    pub fn features(self) -> wgpu::Features {
        let mut f = wgpu::Features::empty();
        if self.bindless_textures {
            f |= wgpu::Features::TEXTURE_BINDING_ARRAY
                | wgpu::Features::SAMPLED_TEXTURE_AND_STORAGE_BUFFER_ARRAY_NON_UNIFORM_INDEXING;
        }
        if self.indirect_first_instance {
            f |= wgpu::Features::INDIRECT_FIRST_INSTANCE;
        }
        if self.multi_draw_indirect_count {
            f |= wgpu::Features::MULTI_DRAW_INDIRECT_COUNT;
        }
        if self.compressed_textures {
            f |= wgpu::Features::TEXTURE_COMPRESSION_BC;
        }
        if self.timestamp_queries {
            f |= wgpu::Features::TIMESTAMP_QUERY;
        }
        f
    }

    /// The capabilities an adapter offers.
    pub fn of(features: wgpu::Features) -> Self {
        Self {
            bindless_textures: features.contains(
                wgpu::Features::TEXTURE_BINDING_ARRAY
                    | wgpu::Features::SAMPLED_TEXTURE_AND_STORAGE_BUFFER_ARRAY_NON_UNIFORM_INDEXING,
            ),
            indirect_first_instance: features.contains(wgpu::Features::INDIRECT_FIRST_INSTANCE),
            multi_draw_indirect_count: features.contains(wgpu::Features::MULTI_DRAW_INDIRECT_COUNT),
            compressed_textures: features.contains(wgpu::Features::TEXTURE_COMPRESSION_BC),
            timestamp_queries: features.contains(wgpu::Features::TIMESTAMP_QUERY),
        }
    }
}

/// GPU context errors.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum GpuError {
    /// No adapter matched.
    NoAdapter(String),
    /// Device creation failed.
    Device(String),
}

impl core::fmt::Display for GpuError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            GpuError::NoAdapter(e) => write!(f, "no adapter: {e}"),
            GpuError::Device(e) => write!(f, "device: {e}"),
        }
    }
}

impl std::error::Error for GpuError {}

/// A device and everything needed to use it.
#[derive(Debug)]
pub struct GpuContext {
    /// The instance.
    pub instance: wgpu::Instance,
    /// The adapter.
    pub adapter: wgpu::Adapter,
    /// The device.
    pub device: wgpu::Device,
    /// The queue.
    pub queue: wgpu::Queue,
    /// Enabled optional capabilities.
    pub capabilities: Capabilities,
    /// Whether this is the no-op backend (nothing executes; readbacks are meaningless).
    pub is_noop: bool,
}

impl GpuContext {
    /// Creates a headless context, enabling every optional capability the adapter offers.
    ///
    /// # Errors
    /// [`GpuError::NoAdapter`] or [`GpuError::Device`].
    pub fn headless(kind: HeadlessKind) -> Result<Self, GpuError> {
        let mut desc = wgpu::InstanceDescriptor::new_without_display_handle();
        if kind == HeadlessKind::Noop {
            desc.backends = wgpu::Backends::NOOP;
            desc.backend_options.noop = wgpu::NoopBackendOptions::enabled();
            // GPU-side validation of indirect arguments runs shaders against the adapter's
            // reported limits, which the no-op adapter fills with values no real device
            // has; nothing executes on no-op anyway, so only API validation applies.
            desc.flags.remove(wgpu::InstanceFlags::VALIDATION_INDIRECT_CALL);
        } else {
            desc.backends = wgpu::Backends::PRIMARY;
        }
        let instance = wgpu::Instance::new(desc);
        let adapter = pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
            power_preference: wgpu::PowerPreference::HighPerformance,
            ..Default::default()
        }))
        .map_err(|e| GpuError::NoAdapter(e.to_string()))?;
        Self::with_adapter(instance, adapter, kind == HeadlessKind::Noop)
    }

    fn with_adapter(
        instance: wgpu::Instance,
        adapter: wgpu::Adapter,
        is_noop: bool,
    ) -> Result<Self, GpuError> {
        let capabilities = Capabilities::of(adapter.features());
        let (device, queue) = pollster::block_on(adapter.request_device(&wgpu::DeviceDescriptor {
            label: Some("mantis-render"),
            required_features: capabilities.features(),
            // The no-op adapter advertises limits no real device has; validate against the
            // portable defaults instead (with its binding-array limits, for bindless).
            required_limits: if is_noop {
                let a = adapter.limits();
                wgpu::Limits {
                    max_binding_array_elements_per_shader_stage: a
                        .max_binding_array_elements_per_shader_stage,
                    max_binding_array_sampler_elements_per_shader_stage: a
                        .max_binding_array_sampler_elements_per_shader_stage,
                    ..wgpu::Limits::default()
                }
            } else {
                adapter.limits()
            },
            ..Default::default()
        }))
        .map_err(|e| GpuError::Device(e.to_string()))?;
        Ok(Self {
            instance,
            adapter,
            device,
            queue,
            capabilities,
            is_noop,
        })
    }

    /// Wraps objects created elsewhere (the client's windowed platform layer).
    pub fn from_parts(
        instance: wgpu::Instance,
        adapter: wgpu::Adapter,
        device: wgpu::Device,
        queue: wgpu::Queue,
    ) -> Self {
        let capabilities = Capabilities::of(device.features());
        Self {
            instance,
            adapter,
            device,
            queue,
            capabilities,
            is_noop: false,
        }
    }

    /// Adapter name and backend, for logs.
    pub fn describe(&self) -> String {
        let info = self.adapter.get_info();
        format!("{} ({:?})", info.name, info.backend)
    }
}

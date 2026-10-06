//! Render graph execution on `wgpu`: API validation on the no-op backend (always runs) and
//! pixel results on a real headless adapter (skipped, and counted, when none exists).

use mantis_render::gpu::GpuContext;
use mantis_render::gpu_test::{hardware_or_skip, noop, read_texture_4bpp, validation_errors};
use mantis_render::graph::exec::{
    ExecError, GraphPass, ImportedTexture, Imports, PassContext, RenderGraph, texture_descriptor,
};
use mantis_render::graph::{
    CompiledGraph, GraphBuilder, PassId, PassKind, TextureAccess, TextureDesc, TextureHandle, WriteMode,
};

type TestResult = Result<(), Box<dyn std::error::Error>>;

const SIZE: u32 = 16;
const FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::Rgba8Unorm;

struct Clear {
    target: TextureHandle,
    color: wgpu::Color,
}

impl GraphPass<()> for Clear {
    fn execute(&mut self, ctx: &mut PassContext<'_>, _frame: &()) {
        let Some(view) = ctx.texture_view(self.target) else {
            return;
        };
        let view = view.clone();
        let _pass = ctx.encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
            label: Some("clear"),
            color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                view: &view,
                depth_slice: None,
                resolve_target: None,
                ops: wgpu::Operations {
                    load: wgpu::LoadOp::Clear(self.color),
                    store: wgpu::StoreOp::Store,
                },
            })],
            ..Default::default()
        });
    }
}

struct Copy {
    src: TextureHandle,
    dst: TextureHandle,
}

impl GraphPass<()> for Copy {
    fn execute(&mut self, ctx: &mut PassContext<'_>, _frame: &()) {
        let (Some(src), Some(dst)) = (ctx.texture(self.src).cloned(), ctx.texture(self.dst).cloned()) else {
            return;
        };
        ctx.encoder.copy_texture_to_texture(
            src.as_image_copy(),
            dst.as_image_copy(),
            wgpu::Extent3d {
                width: SIZE,
                height: SIZE,
                depth_or_array_layers: 1,
            },
        );
    }
}

/// Two chains whose transients alias onto one physical texture: clear A red and copy it
/// to output 1, then clear B green and copy it to output 2.
struct Setup {
    compiled: CompiledGraph,
    passes: Vec<(PassId, Box<dyn GraphPass<()>>)>,
    out1: TextureHandle,
    out2: TextureHandle,
}

fn setup() -> Result<Setup, Box<dyn std::error::Error>> {
    let desc = TextureDesc::d2(SIZE, SIZE, FORMAT);
    let mut g = GraphBuilder::new();
    let ta = g.create_texture("a", desc);
    let tb = g.create_texture("b", desc);
    let out1 = g.import_texture("out1", desc);
    let out2 = g.import_texture("out2", desc);
    let mut passes: Vec<(PassId, Box<dyn GraphPass<()>>)> = Vec::new();

    let (clear_a, a1) = {
        let mut p = g.add_pass("clear_a", PassKind::Render);
        let h = p.write_texture(ta, TextureAccess::ColorTarget, WriteMode::Discard);
        (p.id(), h)
    };
    passes.push((
        clear_a,
        Box::new(Clear {
            target: a1,
            color: wgpu::Color::RED,
        }),
    ));
    let (copy_a, o1) = {
        let mut p = g.add_pass("copy_a", PassKind::Copy);
        p.read_texture(a1, TextureAccess::CopySrc);
        let h = p.write_texture(out1, TextureAccess::CopyDst, WriteMode::Discard);
        (p.id(), h)
    };
    passes.push((copy_a, Box::new(Copy { src: a1, dst: o1 })));
    // B is cleared only after output 1 is written, so its lifetime starts after A's ends.
    let (clear_b, b1) = {
        let mut p = g.add_pass("clear_b", PassKind::Render);
        p.read_texture(o1, TextureAccess::CopySrc);
        let h = p.write_texture(tb, TextureAccess::ColorTarget, WriteMode::Discard);
        (p.id(), h)
    };
    passes.push((
        clear_b,
        Box::new(Clear {
            target: b1,
            color: wgpu::Color::GREEN,
        }),
    ));
    let (copy_b, o2) = {
        let mut p = g.add_pass("copy_b", PassKind::Copy);
        p.read_texture(b1, TextureAccess::CopySrc);
        let h = p.write_texture(out2, TextureAccess::CopyDst, WriteMode::Discard);
        (p.id(), h)
    };
    passes.push((copy_b, Box::new(Copy { src: b1, dst: o2 })));
    g.output(o1);
    g.output(o2);
    let compiled = g.compile()?;
    Ok(Setup {
        compiled,
        passes,
        out1,
        out2,
    })
}

fn output_texture(ctx: &GpuContext, label: &str) -> wgpu::Texture {
    let usage = wgpu::TextureUsages::COPY_DST | wgpu::TextureUsages::COPY_SRC;
    ctx.device.create_texture(&texture_descriptor(
        label,
        &TextureDesc::d2(SIZE, SIZE, FORMAT),
        usage,
    ))
}

/// Runs the setup graph once; returns the two output textures.
fn run(ctx: &GpuContext) -> Result<(wgpu::Texture, wgpu::Texture), Box<dyn std::error::Error>> {
    let s = setup()?;
    assert_eq!(
        s.compiled.physical().len(),
        1,
        "A and B alias onto one physical texture"
    );
    let mut graph = RenderGraph::new(&ctx.device, s.compiled, s.passes)?;
    let t1 = output_texture(ctx, "out1");
    let t2 = output_texture(ctx, "out2");
    let v1 = t1.create_view(&wgpu::TextureViewDescriptor::default());
    let v2 = t2.create_view(&wgpu::TextureViewDescriptor::default());
    let imported = [
        ImportedTexture {
            resource: s.out1.resource(),
            texture: &t1,
            view: &v1,
        },
        ImportedTexture {
            resource: s.out2.resource(),
            texture: &t2,
            view: &v2,
        },
    ];
    let mut encoder = ctx
        .device
        .create_command_encoder(&wgpu::CommandEncoderDescriptor { label: Some("frame") });
    let ran = graph.execute(
        &ctx.device,
        &ctx.queue,
        &mut encoder,
        &Imports {
            textures: &imported,
            buffers: &[],
        },
        &(),
    )?;
    assert_eq!(ran, 4);
    let _ = ctx.queue.submit(Some(encoder.finish()));
    Ok((t1, t2))
}

#[test]
fn graph_execution_passes_api_validation() -> TestResult {
    let ctx = noop()?;
    let mut result = Ok((output_texture(&ctx, "x"), output_texture(&ctx, "y")));
    let errors = validation_errors(&ctx.device, || result = run(&ctx));
    let _ = result?;
    assert_eq!(errors, None);
    Ok(())
}

#[test]
fn missing_or_unusable_imports_stop_the_frame() -> TestResult {
    let ctx = noop()?;
    let s = setup()?;
    let mut graph = RenderGraph::new(&ctx.device, s.compiled, s.passes)?;
    let mut encoder = ctx
        .device
        .create_command_encoder(&wgpu::CommandEncoderDescriptor { label: None });
    let missing = graph.execute(&ctx.device, &ctx.queue, &mut encoder, &Imports::default(), &());
    assert_eq!(missing, Err(ExecError::MissingImport("out1".to_owned())));

    let sampled_only = ctx.device.create_texture(&texture_descriptor(
        "wrong",
        &TextureDesc::d2(SIZE, SIZE, FORMAT),
        wgpu::TextureUsages::TEXTURE_BINDING,
    ));
    let view = sampled_only.create_view(&wgpu::TextureViewDescriptor::default());
    let imported = [
        ImportedTexture {
            resource: s.out1.resource(),
            texture: &sampled_only,
            view: &view,
        },
        ImportedTexture {
            resource: s.out2.resource(),
            texture: &sampled_only,
            view: &view,
        },
    ];
    let wrong = graph.execute(
        &ctx.device,
        &ctx.queue,
        &mut encoder,
        &Imports {
            textures: &imported,
            buffers: &[],
        },
        &(),
    );
    assert_eq!(wrong, Err(ExecError::ImportUsage("out1".to_owned())));
    Ok(())
}

#[test]
fn a_kept_pass_without_an_implementation_is_rejected() -> TestResult {
    let ctx = noop()?;
    let s = setup()?;
    let partial: Vec<_> = s.passes.into_iter().skip(1).collect();
    let err = RenderGraph::new(&ctx.device, s.compiled, partial).err();
    assert_eq!(err, Some(ExecError::MissingPass("clear_a".to_owned())));
    Ok(())
}

#[test]
fn aliased_transients_produce_correct_pixels() -> TestResult {
    let Some(ctx) = hardware_or_skip("aliased_transients_produce_correct_pixels") else {
        return Ok(());
    };
    let (t1, t2) = run(&ctx)?;
    let red = read_texture_4bpp(&ctx, &t1)?;
    let green = read_texture_4bpp(&ctx, &t2)?;
    assert!(
        red.as_chunks::<4>().0.iter().all(|p| *p == [255, 0, 0, 255]),
        "output 1 is red"
    );
    assert!(
        green.as_chunks::<4>().0.iter().all(|p| *p == [0, 255, 0, 255]),
        "output 2 is green"
    );
    Ok(())
}

#[test]
fn the_noop_backend_really_validates() -> TestResult {
    // Guards the meaning of the validation tests above: an invalid copy (out of bounds)
    // must be reported on the no-op backend too.
    let ctx = noop()?;
    let t = output_texture(&ctx, "small");
    let errors = validation_errors(&ctx.device, || {
        let mut encoder = ctx
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor { label: None });
        encoder.copy_texture_to_texture(
            t.as_image_copy(),
            t.as_image_copy(),
            wgpu::Extent3d {
                width: SIZE * 2,
                height: SIZE,
                depth_or_array_layers: 1,
            },
        );
        let _ = ctx.queue.submit(Some(encoder.finish()));
    });
    assert!(errors.is_some(), "an out-of-bounds copy must fail validation");
    Ok(())
}

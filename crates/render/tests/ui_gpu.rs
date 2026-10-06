//! The UI pass: the `mantis_ui` draw list (rounded panels, borders, MSDF text) drawn by
//! the renderer over the composited frame. API validation runs on the no-op backend; on
//! a real adapter the pixels must match the UI crate's CPU reference rasterizer, which
//! defines the shading math.

#![allow(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    clippy::cast_precision_loss
)] // Pixel math.

use glam::Vec3;
use mantis_formats::sh::ShL1;
use mantis_render::gpu::GpuContext;
use mantis_render::gpu_test::{hardware_or_skip, noop, read_texture_4bpp, validation_errors};
use mantis_render::math::Camera;
use mantis_render::post::PostSettings;
use mantis_render::renderer::{FrameInputs, Renderer, RendererConfig};
use mantis_ui::draw::reference::rasterize;
use mantis_ui::{FontLibrary, Ui, test_font};

type TestResult = Result<(), Box<dyn std::error::Error>>;

const W: u32 = 256;
const H: u32 = 192;
const SCALE: f32 = 0.5;
const OUTPUT: wgpu::TextureFormat = wgpu::TextureFormat::Rgba8Unorm;

const THEME: &str = r#"
theme {
  style title { font = "ui" size = 28 color = #ffffff }
  style panel { background = #20242cee radius = 10 padding = 12 border = 2 border_color = #ff8040ff }
  style button { background = #3060c0 radius = 6 }
}
"#;

const LAYOUT: &str = r#"
panel id=root style=panel direction=column gap=8 width=440 height=320 {
  text id=name bind="player.name" style=title
  text id=hp template="HP {hp} / {hp_max}"
  row id=buttons gap=12 {
    button id=ok text="OK" intent="dialog.confirm"
    button id=cancel text="Cancel" intent="dialog.cancel"
  }
}
"#;

fn ui() -> Result<Ui, Box<dyn std::error::Error>> {
    let mut fonts = FontLibrary::new();
    let latin = fonts.add_font(test_font::latin())?;
    fonts.define_stack("ui", &[latin])?;
    let mut ui = Ui::new(fonts, LAYOUT, Some(THEME))?;
    let props = ui.properties_mut();
    let name = props.intern("player.name");
    props.set_text(name, "player");
    let hp = props.intern("hp");
    props.set_int(hp, 90);
    let max = props.intern("hp_max");
    props.set_int(max, 100);
    Ok(ui)
}

fn inputs() -> FrameInputs {
    FrameInputs {
        camera: Camera {
            position: Vec3::ZERO,
            yaw: 0.0,
            pitch: 0.0,
            fov_y: 1.0,
            aspect: 4.0 / 3.0,
            near: 0.1,
        },
        time: 0.0,
        sun_direction: Vec3::NEG_Y,
        sun_color: Vec3::ONE,
        shadows: false,
        sky: ShL1::ZERO,
        ambient_intensity: 1.0,
        exposure: 1.0,
        clear_color: [0.0, 0.0, 0.0, 1.0],
        ssao_strength: 0.0,
        post: PostSettings::OFF,
    }
}

fn frame(ctx: &GpuContext, r: &mut Renderer, ui: &mut Ui) -> Result<Vec<u8>, Box<dyn std::error::Error>> {
    let list = ui.frame([W as f32, H as f32], SCALE).quads.clone();
    r.set_ui(&ctx.device, &ctx.queue, &list, ui.atlas_mut());
    let tex = ctx.device.create_texture(&wgpu::TextureDescriptor {
        label: Some("output"),
        size: wgpu::Extent3d {
            width: W,
            height: H,
            depth_or_array_layers: 1,
        },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: OUTPUT,
        usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
        view_formats: &[],
    });
    let view = tex.create_view(&wgpu::TextureViewDescriptor::default());
    let _ = r.prepare(&inputs());
    let mut encoder = ctx
        .device
        .create_command_encoder(&wgpu::CommandEncoderDescriptor { label: Some("frame") });
    r.encode(&ctx.device, &ctx.queue, &mut encoder, (&tex, &view))?;
    let _ = ctx.queue.submit(Some(encoder.finish()));
    Ok(read_texture_4bpp(ctx, &tex)?)
}

fn renderer(ctx: &GpuContext) -> Result<Renderer, Box<dyn std::error::Error>> {
    let mut config = RendererConfig::new(W, H, OUTPUT);
    config.max_instances = 16;
    config.max_batches = 4;
    config.max_vertices = 64;
    config.max_indices = 64;
    config.shadow_resolution = 64;
    Ok(Renderer::new(&ctx.device, &ctx.queue, false, config)?)
}

#[test]
fn the_ui_pass_passes_api_validation() -> TestResult {
    let ctx = noop()?;
    let mut result: TestResult = Ok(());
    let errors = validation_errors(&ctx.device, || {
        result = (|| {
            let mut r = renderer(&ctx)?;
            let mut ui = ui()?;
            for _ in 0..3 {
                let _ = frame(&ctx, &mut r, &mut ui)?;
            }
            assert!(r.ui_stats().quads > 10, "{:?}", r.ui_stats());
            assert_eq!(r.ui_stats().atlas_rebuilds, 1, "the atlas texture is built once");
            Ok(())
        })();
    });
    result?;
    assert_eq!(errors, None);
    Ok(())
}

#[test]
fn ui_pixels_match_the_reference_rasterizer() -> TestResult {
    let Some(ctx) = hardware_or_skip("ui_pixels_match_the_reference_rasterizer") else {
        return Ok(());
    };
    let mut rend = renderer(&ctx)?;
    let mut ui = ui()?;
    let gpu = frame(&ctx, &mut rend, &mut ui)?;
    let atlas = ui.atlas();
    let reference = rasterize(ui.draw_list(), atlas.pixels(), atlas.size(), W, H);
    let mut off = 0usize;
    let mut worst = 0u8;
    let mut glyph_texels = 0usize;
    for y in 0..H {
        for x in 0..W {
            let i = ((y * W + x) * 4) as usize;
            let got = gpu.get(i..i + 3).ok_or("pixel")?;
            let want = reference.get(x, y);
            for (c, (gv, ev)) in got.iter().zip(want).enumerate() {
                let ev = (ev.clamp(0.0, 1.0) * 255.0).round() as u8;
                let d = gv.abs_diff(ev);
                worst = worst.max(d);
                if d > 6 {
                    off += 1;
                }
                if c == 0 && *gv > 200 && want[2] > 0.78 {
                    glyph_texels += 1;
                }
            }
        }
    }
    println!(
        "ui pixels off by more than 6/255: {off} of {} (worst {worst})",
        W * H * 3
    );
    // The GPU matches the reference to rounding (measured worst: 1/255).
    assert!(worst <= 3, "worst channel difference {worst}/255 ({off} over 6)");
    assert!(glyph_texels > 50, "white text is visible: {glyph_texels}");
    Ok(())
}

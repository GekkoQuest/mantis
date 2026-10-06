//! MSDF quality: inside/outside at texel centers matches the analytic shape,
//! and sharp corners stay sharp under bilinear upsampling.

#![allow(
    clippy::indexing_slicing,
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation
)]

use mantis_ui::msdf::{MsdfGlyph, generate_glyph, median};
use mantis_ui::test_font::{GlyphShape, TestFontBuilder};
use rustybuzz::ttf_parser;

type TestResult = Result<(), Box<dyn std::error::Error>>;

const EM: f32 = 32.0;
const RANGE: f32 = 4.0;
const PAD: u32 = 5;

fn glyph(shape: GlyphShape) -> Result<MsdfGlyph, Box<dyn std::error::Error>> {
    let bytes = TestFontBuilder::new().glyph('a', shape).build();
    let face = ttf_parser::Face::parse(&bytes, 0)?;
    let gid = face.glyph_index('a').ok_or("unmapped")?;
    Ok(generate_glyph(&face, gid.0, EM, RANGE, PAD).ok_or("no outline")?)
}

fn texel(g: &MsdfGlyph, col: i64, row: i64) -> [f32; 4] {
    let c = col.clamp(0, i64::from(g.width) - 1);
    let r = row.clamp(0, i64::from(g.height) - 1);
    let i = usize::try_from((r * i64::from(g.width) + c) * 4).unwrap_or(0);
    let mut out = [0.0; 4];
    for (k, o) in out.iter_mut().enumerate() {
        *o = f32::from(g.pixels.get(i + k).copied().unwrap_or(0)) / 255.0;
    }
    out
}

/// Bilinear sample at a point in texels from the top-left (y down).
fn sample(g: &MsdfGlyph, tx: f32, ty: f32) -> [f32; 4] {
    let (sx, sy) = (tx - 0.5, ty - 0.5);
    let (fx, fy) = (sx.floor(), sy.floor());
    let (ax, ay) = (sx - fx, sy - fy);
    let (c0, r0) = (fx as i64, fy as i64);
    let t00 = texel(g, c0, r0);
    let t10 = texel(g, c0 + 1, r0);
    let t01 = texel(g, c0, r0 + 1);
    let t11 = texel(g, c0 + 1, r0 + 1);
    let mut out = [0.0; 4];
    for (k, o) in out.iter_mut().enumerate() {
        let top = t00[k] + (t10[k] - t00[k]) * ax;
        let bottom = t01[k] + (t11[k] - t01[k]) * ax;
        *o = top + (bottom - top) * ay;
    }
    out
}

/// Inside test for every texel center farther than one texel from an edge
/// (no point on a radius-1 circle around it changes classification).
fn check_texel_centers(shape: GlyphShape) -> TestResult {
    let g = glyph(shape)?;
    let mut checked = 0;
    for row in 0..g.height {
        for col in 0..g.width {
            let (cx, cy) = (col as f32 + 0.5, row as f32 + 0.5);
            let [x, y] = g.texel_to_font(cx, cy);
            let inside = shape.contains(1000, x, y);
            let near_edge = (0..32).any(|k| {
                let a = k as f32 / 32.0 * std::f32::consts::TAU;
                let [px, py] = g.texel_to_font(cx + a.cos() * 1.0, cy + a.sin() * 1.0);
                shape.contains(1000, px, py) != inside
            });
            if near_edge {
                continue;
            }
            let t = texel(&g, i64::from(col), i64::from(row));
            let decoded = median(t[0], t[1], t[2]) > 0.5;
            assert_eq!(
                decoded, inside,
                "{shape:?}: texel ({col},{row}) decoded inside={decoded}, analytic {inside}"
            );
            assert_eq!(
                t[3] > 0.5,
                inside,
                "{shape:?}: alpha (true SDF) sign at ({col},{row})"
            );
            checked += 1;
        }
    }
    assert!(checked > 100, "only {checked} texels checked");
    Ok(())
}

#[test]
fn rectangle_inside_outside_matches() -> TestResult {
    check_texel_centers(GlyphShape::Rect)
}

#[test]
fn square_with_hole_inside_outside_matches() -> TestResult {
    check_texel_centers(GlyphShape::SquareWithHole)
}

#[test]
fn triangle_and_circle_inside_outside_match() -> TestResult {
    check_texel_centers(GlyphShape::Triangle)?;
    check_texel_centers(GlyphShape::Circle)
}

/// Distance (font units) from a point to the boundary of the rectangle
/// 100..500 x 0..700.
fn rect_boundary_distance(x: f32, y: f32) -> f32 {
    let (x0, x1, y0, y1) = (100.0_f32, 500.0_f32, 0.0_f32, 700.0_f32);
    let inside = (x0..=x1).contains(&x) && (y0..=y1).contains(&y);
    if inside {
        (x - x0).min(x1 - x).min(y - y0).min(y1 - y)
    } else {
        let dx = (x0 - x).max(0.0).max(x - x1);
        let dy = (y0 - y).max(0.0).max(y - y1);
        dx.hypot(dy)
    }
}

#[test]
fn corners_stay_sharp_under_upsampling() -> TestResult {
    let g = glyph(GlyphShape::Rect)?;
    let units_per_texel = 1.0 / g.scale;
    let corners = [(100.0, 0.0), (100.0, 700.0), (500.0, 0.0), (500.0, 700.0)];
    let (mut samples, mut msdf_errors, mut sdf_errors) = (0, 0, 0);
    let mut msdf_worst = 0.0_f32;
    let mut sdf_worst = 0.0_f32;
    let up = 4;
    for j in 0..g.height * up {
        for i in 0..g.width * up {
            let tx = (i as f32 + 0.5) / up as f32;
            let ty = (j as f32 + 0.5) / up as f32;
            let [x, y] = g.texel_to_font(tx, ty);
            let near_corner = corners.iter().any(|&(cx, cy)| {
                (x - cx).abs() < 3.0 * units_per_texel && (y - cy).abs() < 3.0 * units_per_texel
            });
            if !near_corner {
                continue;
            }
            let boundary = rect_boundary_distance(x, y) / units_per_texel;
            if boundary < 0.125 {
                continue;
            }
            samples += 1;
            let inside = GlyphShape::Rect.contains(1000, x, y);
            let s = sample(&g, tx, ty);
            if (median(s[0], s[1], s[2]) > 0.5) != inside {
                msdf_errors += 1;
                msdf_worst = msdf_worst.max(boundary);
            }
            if (s[3] > 0.5) != inside {
                sdf_errors += 1;
                sdf_worst = sdf_worst.max(boundary);
            }
        }
    }
    println!(
        "corner samples={samples} msdf_errors={msdf_errors} (worst {msdf_worst:.3} texel) \
         sdf_errors={sdf_errors} (worst {sdf_worst:.3} texel)"
    );
    assert!(samples > 500);
    assert!(
        msdf_errors * 100 <= samples,
        "MSDF misclassified {msdf_errors} of {samples} corner samples"
    );
    assert!(msdf_worst < 0.3, "MSDF corner error reaches {msdf_worst} texel");
    assert!(
        sdf_errors > msdf_errors,
        "a single-channel SDF should round corners more ({sdf_errors} vs {msdf_errors})"
    );
    Ok(())
}

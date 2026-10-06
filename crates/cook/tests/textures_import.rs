//! The texture importer: Netpbm sources and sidecar settings through the cook, mip
//! chains, linear-light and normal-map filtering, encoder versions, located errors, and
//! determinism.

#![allow(
    clippy::indexing_slicing,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss
)]

use mantis_cook::importer::{CookError, Importer};
use mantis_cook::importers::textures::encode::decode_image;
use mantis_cook::importers::textures::mips::{self, Space};
use mantis_cook::importers::textures::{IMPORTER_VERSION, Netpbm, bc1, bc4, bc5, bc7};
use mantis_cook::tree::ContentTree;
use mantis_cook::{Cook, CookOutput, importers};
use mantis_formats::bundle::{AssetKind, Domain};
use mantis_formats::texture::{Encoding, FLAG_NORMAL_MAP, FLAG_SRGB, TextureAsset, full_chain, mip_size};

type TestResult = Result<(), Box<dyn std::error::Error>>;

fn ppm(w: u32, h: u32, texel: impl Fn(u32, u32) -> [u8; 3]) -> Vec<u8> {
    let mut out = format!("P6\n# generated\n{w} {h}\n255\n").into_bytes();
    for y in 0..h {
        for x in 0..w {
            out.extend_from_slice(&texel(x, y));
        }
    }
    out
}

fn pgm(w: u32, h: u32, value: impl Fn(u32, u32) -> u8) -> Vec<u8> {
    let mut out = format!("P5 {w} {h} 255\n").into_bytes();
    for y in 0..h {
        for x in 0..w {
            out.push(value(x, y));
        }
    }
    out
}

fn pam(w: u32, h: u32, depth: usize, tupltype: &str, sample: impl Fn(u32, u32, usize) -> u8) -> Vec<u8> {
    let mut out =
        format!("P7\nWIDTH {w}\nHEIGHT {h}\nDEPTH {depth}\nMAXVAL 255\nTUPLTYPE {tupltype}\nENDHDR\n")
            .into_bytes();
    for y in 0..h {
        for x in 0..w {
            for c in 0..depth {
                out.push(sample(x, y, c));
            }
        }
    }
    out
}

fn cook(tree: &ContentTree) -> Result<CookOutput, String> {
    let cook = Cook::new(importers::builtin()).map_err(|e| e.to_string())?;
    cook.run(tree).map_err(|e| format!("{e:?}"))
}

fn errors(tree: &ContentTree) -> Result<Vec<CookError>, String> {
    let cook = Cook::new(importers::builtin()).map_err(|e| e.to_string())?;
    cook.run(tree).err().ok_or_else(|| "expected errors".to_owned())
}

fn texture(out: &CookOutput, name: &str) -> Result<TextureAsset, Box<dyn std::error::Error>> {
    let asset = out.get(name).ok_or_else(|| format!("no output {name}"))?;
    assert_eq!(asset.kind, AssetKind::Texture);
    assert_eq!(asset.domain, Domain::Presentation);
    Ok(TextureAsset::parse(&asset.bytes)?)
}

fn sample_tree() -> ContentTree {
    let mut t = ContentTree::new();
    t.insert(
        "art/crate.ppm",
        ppm(8, 4, |x, y| [(x * 10) as u8, (y * 12) as u8, 90]),
    );
    t.insert("art/mask.pgm", pgm(12, 4, |x, y| (x * 20 + y * 7) as u8));
    t.insert(
        "art/leaf.pam",
        pam(8, 8, 4, "RGB_ALPHA", |x, y, c| {
            if c == 3 { (x * 32) as u8 } else { (y * 30) as u8 }
        }),
    );
    t.insert(
        "art/fog.pam",
        pam(4, 4, 2, "GRAYSCALE_ALPHA", |x, _, c| (x * 60 + c as u32) as u8),
    );
    t.insert(
        "art/gray.pam",
        pam(4, 4, 1, "GRAYSCALE", |x, _, _| (x * 60) as u8),
    );
    t.insert(
        "art/sky.pam",
        pam(4, 8, 3, "RGB", |x, y, c| (x * 50 + y + c as u32) as u8),
    );
    t
}

#[test]
fn images_cook_to_textures_with_defaults_by_channel_count() -> TestResult {
    let out = cook(&sample_tree())?;
    assert_eq!(out.importers.get("texture.netpbm"), Some(&IMPORTER_VERSION));
    assert_eq!(Netpbm.version(), IMPORTER_VERSION);
    assert_eq!(
        IMPORTER_VERSION,
        1 + bc1::ENCODER_VERSION + bc4::ENCODER_VERSION + bc7::ENCODER_VERSION,
        "the importer version grows with every encoder version"
    );
    // (output, encoding, flags, width, height)
    let expected = [
        ("art/crate.tex", Encoding::Bc1, FLAG_SRGB, 8, 4),
        ("art/mask.tex", Encoding::Bc4, 0, 12, 4),
        ("art/leaf.tex", Encoding::Bc7, FLAG_SRGB, 8, 8),
        ("art/fog.tex", Encoding::Bc7, FLAG_SRGB, 4, 4),
        ("art/gray.tex", Encoding::Bc4, 0, 4, 4),
        ("art/sky.tex", Encoding::Bc1, FLAG_SRGB, 4, 8),
    ];
    for (name, encoding, flags, w, h) in expected {
        let t = texture(&out, name)?;
        assert_eq!(
            (t.encoding, t.flags, t.width, t.height),
            (encoding, flags, w, h),
            "{name}"
        );
        assert_eq!(
            t.mips.len() as u32,
            full_chain(w, h),
            "{name}: full chain by default"
        );
        let version = match encoding {
            Encoding::Bc1 => bc1::ENCODER_VERSION,
            Encoding::Bc4 => bc4::ENCODER_VERSION,
            _ => bc7::ENCODER_VERSION,
        };
        assert_eq!(t.encoder_version, version, "{name}: the current encoder version");
    }
    assert_eq!(out.bundle(Domain::Presentation, 1).entries.len(), expected.len());
    // Level 0 decodes close to the source (a planar gradient, so a one-line BC1 fit is
    // off by up to about half a block's range at the corners).
    let crate_tex = texture(&out, "art/crate.tex")?;
    let back = decode_image(Encoding::Bc1, 8, 4, &crate_tex.mips[0]).ok_or("decodes")?;
    let mut total = 0u32;
    for (i, t) in back.iter().enumerate() {
        let (x, y) = (i as u32 % 8, i as u32 / 8);
        let src = [(x * 10) as u8, (y * 12) as u8, 90];
        for c in 0..3 {
            let d = t[c].abs_diff(src[c]);
            assert!(d <= 24, "texel {i} channel {c}: {} vs {}", t[c], src[c]);
            total += u32::from(d);
        }
    }
    assert!(total <= 32 * 3 * 6, "mean error {}", f64::from(total) / 96.0);
    Ok(())
}

#[test]
fn sidecar_settings_apply() -> TestResult {
    let mut t = ContentTree::new();
    let image = ppm(4, 4, |x, y| [(x * 60) as u8, (y * 60) as u8, 128]);
    for name in ["raw", "flat", "normal", "linear", "cut", "gray5"] {
        t.insert(&format!("art/{name}.ppm"), image.clone());
    }
    t.insert("art/raw.ppm.texture.toml", "encoding = \"rgba8\"\nmips = false\n");
    t.insert("art/flat.ppm.texture.toml", "# level 0 only\nmips = false\n");
    t.insert("art/normal.ppm.texture.toml", "normal_map = true\n");
    t.insert(
        "art/linear.ppm.texture.toml",
        "encoding = \"bc7\"\nsrgb = false\n",
    );
    t.insert(
        "art/cut.ppm.texture.toml",
        "encoding = \"bc1\"\nalpha_cutoff = 0.5\n",
    );
    t.insert("art/gray5.ppm.texture.toml", "encoding = \"bc5\"\n");
    t.insert(
        "art/holes.pam",
        pam(4, 4, 4, "RGB_ALPHA", |x, _, c| {
            if c == 3 { if x < 2 { 0 } else { 255 } } else { 200 }
        }),
    );
    t.insert(
        "art/holes.pam.texture.toml",
        "encoding = \"bc1\"\nalpha_cutoff = 0.5\n",
    );
    let out = cook(&t)?;
    assert!(
        out.get("art/raw.ppm.texture.toml").is_none(),
        "a sidecar is not cooked on its own"
    );

    let raw = texture(&out, "art/raw.tex")?;
    assert_eq!(
        (raw.encoding, raw.flags, raw.encoder_version, raw.mips.len()),
        (Encoding::Rgba8, FLAG_SRGB, 0, 1)
    );
    let expected: Vec<u8> = image[image.len() - 48..]
        .chunks(3)
        .flat_map(|c| [c[0], c[1], c[2], 255])
        .collect();
    assert_eq!(raw.mips[0], expected, "rgba8 level 0 is the source");

    let flat = texture(&out, "art/flat.tex")?;
    assert_eq!((flat.encoding, flat.mips.len()), (Encoding::Bc1, 1));

    let normal = texture(&out, "art/normal.tex")?;
    assert_eq!((normal.encoding, normal.flags), (Encoding::Bc5, FLAG_NORMAL_MAP));
    assert_eq!(normal.encoder_version, bc5::ENCODER_VERSION);

    let linear = texture(&out, "art/linear.tex")?;
    assert_eq!((linear.encoding, linear.flags), (Encoding::Bc7, 0));

    let gray5 = texture(&out, "art/gray5.tex")?;
    assert_eq!(
        (gray5.encoding, gray5.flags),
        (Encoding::Bc5, 0),
        "bc5 defaults to linear"
    );

    let holes = texture(&out, "art/holes.tex")?;
    let back = decode_image(Encoding::Bc1, 4, 4, &holes.mips[0]).ok_or("decodes")?;
    for (i, t) in back.iter().enumerate() {
        assert_eq!(
            t[3],
            if i % 4 < 2 { 0 } else { 255 },
            "texel {i}: punch-through alpha"
        );
    }
    Ok(())
}

#[test]
fn srgb_mips_average_in_linear_light() -> TestResult {
    // A 2x2 black and white checker: in linear light the average is 0.5, which sRGB
    // encodes as 255 · (1.055 · 0.5^(1/2.4) - 0.055) = 187.5..., stored as 188. Averaging
    // the encoded values instead would give 128.
    let mut t = ContentTree::new();
    let checker = ppm(2, 2, |x, y| if (x + y) % 2 == 0 { [0; 3] } else { [255; 3] });
    t.insert("art/srgb.ppm", checker.clone());
    t.insert("art/srgb.ppm.texture.toml", "encoding = \"rgba8\"\n");
    t.insert("art/data.ppm", checker);
    t.insert(
        "art/data.ppm.texture.toml",
        "encoding = \"rgba8\"\nsrgb = false\n",
    );
    let out = cook(&t)?;
    let expected = (mips::linear_to_srgb(0.5) * 255.0 + 0.5).floor() as u8;
    assert_eq!(expected, 188);
    assert_eq!(texture(&out, "art/srgb.tex")?.mips[1], [188, 188, 188, 255]);
    assert_eq!(texture(&out, "art/data.tex")?.mips[1], [128, 128, 128, 255]);

    // The curve: decode and encode are inverse on every code, and match the formulas.
    let curve = mips::SrgbCurve::new();
    for code in 0..=255u8 {
        assert_eq!(curve.encode(curve.decode(code)), code);
        let exact = mips::srgb_to_linear(f64::from(code) / 255.0);
        assert!((f64::from(curve.decode(code)) - exact).abs() < 1e-6);
        let again = mips::linear_to_srgb(exact) * 255.0;
        assert!((again - f64::from(code)).abs() < 1e-6, "{code}: {again}");
    }
    assert_eq!(curve.encode(0.0), 0);
    assert_eq!(curve.encode(1.0), 255);
    Ok(())
}

#[test]
fn normal_map_mips_are_renormalized() -> TestResult {
    let to_byte = |v: f32| ((v * 0.5 + 0.5) * 255.0 + 0.5).floor() as u8;
    // Two normals tilted 53 degrees left and right: their plain average is (0, 0, 0.6);
    // renormalized it is straight up, Z = 1 (stored as 255).
    let left = [to_byte(-0.8), to_byte(0.0), to_byte(0.6), 255];
    let right = [to_byte(0.8), to_byte(0.0), to_byte(0.6), 255];
    let chain = mips::chain(2, 2, &[left, right, right, left], Space::Normal, 2);
    let up = chain[1][0];
    assert!(up[0].abs_diff(128) <= 1 && up[1].abs_diff(128) <= 1, "{up:?}");
    assert_eq!(up[2], 255, "renormalized to unit length");

    // A noisy normal map: every texel of every lower level is unit length.
    let mut seed = 0x0246_8ACE_u32;
    let mut next = move || {
        seed ^= seed << 13;
        seed ^= seed >> 17;
        seed ^= seed << 5;
        (seed >> 24) as u8
    };
    let texels: Vec<[u8; 4]> = (0..16 * 8).map(|_| [next(), next(), 255, 255]).collect();
    let chain = mips::chain(16, 8, &texels, Space::Normal, full_chain(16, 8));
    assert_eq!(chain.len(), 5);
    for level in &chain[1..] {
        for t in level {
            let v: Vec<f32> = t[..3].iter().map(|&c| f32::from(c) / 255.0 * 2.0 - 1.0).collect();
            let len = (v[0] * v[0] + v[1] * v[1] + v[2] * v[2]).sqrt();
            assert!((len - 1.0).abs() < 0.02, "{t:?} has length {len}");
        }
    }

    // Through the importer: BC5 holds X and Y of the renormalized level.
    let mut t = ContentTree::new();
    t.insert(
        "art/bumps.ppm",
        ppm(4, 4, |x, y| {
            if (x + y) % 2 == 0 {
                [left[0], left[1], left[2]]
            } else {
                [right[0], right[1], right[2]]
            }
        }),
    );
    t.insert("art/bumps.ppm.texture.toml", "normal_map = true\nsrgb = false\n");
    let tex = texture(&cook(&t)?, "art/bumps.tex")?;
    let mip1 = decode_image(Encoding::Bc5, 2, 2, &tex.mips[1]).ok_or("decodes")?;
    assert!(
        mip1[0][0].abs_diff(128) <= 1 && mip1[0][1].abs_diff(128) <= 1,
        "{:?}",
        mip1[0]
    );
    Ok(())
}

#[test]
fn mip_chain_sizes_for_non_square_and_odd_images() -> TestResult {
    let mut t = ContentTree::new();
    // (name, width, height)
    let sizes = [
        ("wide", 7, 3),
        ("tall", 1, 9),
        ("odd", 13, 13),
        ("strip", 33, 2),
        ("one", 1, 1),
    ];
    for (name, w, h) in sizes {
        t.insert(
            &format!("art/{name}.ppm"),
            ppm(w, h, |x, y| [(x * 7) as u8, (y * 5) as u8, 3]),
        );
        t.insert(&format!("art/{name}.ppm.texture.toml"), "encoding = \"rgba8\"\n");
    }
    let out = cook(&t)?;
    // Block compression needs whole blocks at level 0; lower levels of a BC chain may be
    // smaller than a block (they are stored as whole blocks).
    let mut bc = ContentTree::new();
    bc.insert("art/wide.ppm", ppm(7, 3, |_, _| [1, 2, 3]));
    let refused = errors(&bc)?;
    assert!(has(&refused, "art/wide.ppm", 0, "multiple of 4"), "{refused:?}");
    bc.insert("art/wide.ppm", ppm(8, 4, |_, _| [1, 2, 3]));
    let tex = texture(&cook(&bc)?, "art/wide.tex")?;
    let sizes: Vec<usize> = tex.mips.iter().map(Vec::len).collect();
    assert_eq!(sizes, vec![16, 8, 8, 8], "8x4, 4x2, 2x1, 1x1 as BC1 blocks");
    let chains = [
        ("wide", vec![(7, 3), (3, 1), (1, 1)]),
        ("tall", vec![(1, 9), (1, 4), (1, 2), (1, 1)]),
        ("odd", vec![(13, 13), (6, 6), (3, 3), (1, 1)]),
        ("strip", vec![(33, 2), (16, 1), (8, 1), (4, 1), (2, 1), (1, 1)]),
        ("one", vec![(1, 1)]),
    ];
    for (name, levels) in chains {
        let raw = texture(&out, &format!("art/{name}.tex"))?;
        assert_eq!(raw.mips.len(), levels.len(), "{name}");
        for (level, &(w, h)) in levels.iter().enumerate() {
            assert_eq!(mip_size(raw.width, raw.height, level as u32), (w, h));
            assert_eq!(raw.mips[level].len() as u32, w * h * 4, "{name} level {level}");
        }
    }
    // Odd sizes: 3x1 -> 1x1 averages columns 0 and 1 (rows clamp to row 0); the last
    // column of an odd width is dropped, as the halved size requires.
    let line = mips::chain(3, 1, &[[0; 4], [100; 4], [250; 4]], Space::Linear, 2);
    assert_eq!(line[1], [[50; 4]]);
    // 1x3 -> 1x1: column clamps to itself, rows 0 and 1.
    let column = mips::chain(1, 3, &[[20; 4], [60; 4], [255; 4]], Space::Linear, 2);
    assert_eq!(column[1], [[40; 4]]);
    Ok(())
}

#[test]
fn importer_writes_the_current_encoder_versions() -> TestResult {
    let mut t = ContentTree::new();
    let cases = [
        ("bc1", bc1::ENCODER_VERSION),
        ("bc4", bc4::ENCODER_VERSION),
        ("bc5", bc5::ENCODER_VERSION),
        ("bc7", bc7::ENCODER_VERSION),
        ("rgba8", 0),
    ];
    for (enc, _) in cases {
        t.insert(
            &format!("art/{enc}.ppm"),
            ppm(8, 8, |x, y| [(x * 30) as u8, (y * 30) as u8, 77]),
        );
        t.insert(
            &format!("art/{enc}.ppm.texture.toml"),
            format!("encoding = \"{enc}\"\n"),
        );
    }
    let out = cook(&t)?;
    for (enc, version) in cases {
        let asset = out.get(&format!("art/{enc}.tex")).ok_or("output")?;
        let tex = TextureAsset::parse(&asset.bytes)?;
        assert_eq!(tex.encoder_version, version, "{enc}");
        // The version sits at bytes 12..16 of the MTEX header.
        assert_eq!(asset.bytes[12..16], version.to_le_bytes(), "{enc}");
    }
    Ok(())
}

#[test]
fn cooking_is_deterministic() -> TestResult {
    let mut t = sample_tree();
    t.insert(
        "art/n.ppm",
        ppm(8, 8, |x, y| [(x * 28) as u8, (y * 28) as u8, 255]),
    );
    t.insert("art/n.ppm.texture.toml", "normal_map = true\n");
    let a = cook(&t)?;
    let b = cook(&t)?;
    assert_eq!(
        a.bundle(Domain::Presentation, 1).hash(),
        b.bundle(Domain::Presentation, 1).hash()
    );
    for (name, asset) in &a.assets {
        assert_eq!(Some(&asset.bytes), b.get(name).map(|x| &x.bytes), "{name}");
    }
    Ok(())
}

fn has(errors: &[CookError], file: &str, line: usize, text: &str) -> bool {
    errors
        .iter()
        .any(|e| e.file == file && e.line == line && e.message.contains(text))
}

#[test]
fn malformed_images_are_reported_with_their_location() -> TestResult {
    let mut t = ContentTree::new();
    let good_raster = vec![7u8; 4 * 4 * 3];
    let with = |header: &str, raster: &[u8]| {
        let mut b = header.as_bytes().to_vec();
        b.extend_from_slice(raster);
        b
    };
    t.insert("bad/magic.ppm", with("P5\n4 4\n255\n", &good_raster));
    t.insert("bad/ascii.ppm", with("P3\n4 4\n255\n", &good_raster));
    t.insert("bad/height.ppm", with("P6\n4 x\n255\n", &good_raster));
    t.insert("bad/maxval.ppm", with("P6\n4 4\n# deep\n65535\n", &good_raster));
    t.insert("bad/short.ppm", with("P6\n4 4\n255\n", &good_raster[1..]));
    t.insert(
        "bad/long.ppm",
        with("P6\n4 4\n255\n", &[good_raster.as_slice(), &[0]].concat()),
    );
    t.insert("bad/huge.ppm", with("P6\n16385 1\n255\n", &[]));
    t.insert("bad/zero.pgm", with("P5\n0 4\n255\n", &[]));
    t.insert("bad/eof.pgm", with("P5\n4", &[]));
    t.insert(
        "bad/depth.pam",
        with(
            "P7\nWIDTH 1\nHEIGHT 1\nDEPTH 5\nMAXVAL 255\nTUPLTYPE RGB\nENDHDR\n",
            &[0; 5],
        ),
    );
    t.insert(
        "bad/tuple.pam",
        with(
            "P7\nWIDTH 1\nHEIGHT 1\nDEPTH 3\nMAXVAL 255\nTUPLTYPE RGB_ALPHA\nENDHDR\n",
            &[0; 3],
        ),
    );
    t.insert(
        "bad/notuple.pam",
        with("P7\nWIDTH 1\nHEIGHT 1\nDEPTH 1\nMAXVAL 255\nENDHDR\n", &[0]),
    );
    t.insert(
        "bad/pammax.pam",
        with(
            "P7\nWIDTH 1\nHEIGHT 1\nDEPTH 1\nMAXVAL 1023\nTUPLTYPE GRAYSCALE\nENDHDR\n",
            &[0, 0],
        ),
    );
    t.insert("bad/noend.pam", with("P7\nWIDTH 1\nHEIGHT 1\n", &[]));
    t.insert(
        "bad/wide.pam",
        with(
            "P7\nWIDTH 20000\nHEIGHT 1\nDEPTH 1\nMAXVAL 255\nTUPLTYPE GRAYSCALE\nENDHDR\n",
            &[],
        ),
    );
    let errors = errors(&t)?;
    let shown: Vec<String> = errors.iter().map(ToString::to_string).collect();
    let expect = [
        ("bad/magic.ppm", 1, "expected magic `P6`"),
        ("bad/ascii.ppm", 1, "found `P3`"),
        ("bad/height.ppm", 2, "the height"),
        ("bad/maxval.ppm", 4, "unsupported MAXVAL 65535"),
        ("bad/short.ppm", 0, "the raster has 47 bytes"),
        ("bad/long.ppm", 0, "1 bytes follow the raster"),
        ("bad/huge.ppm", 2, "width 16385 is out of range"),
        ("bad/zero.pgm", 2, "width 0 is out of range"),
        ("bad/eof.pgm", 2, "the header ends before the height"),
        ("bad/depth.pam", 4, "unsupported DEPTH 5"),
        ("bad/tuple.pam", 6, "does not match DEPTH 3"),
        ("bad/notuple.pam", 6, "`TUPLTYPE` is missing"),
        ("bad/pammax.pam", 5, "unsupported MAXVAL 1023"),
        ("bad/noend.pam", 3, "before `ENDHDR`"),
        ("bad/wide.pam", 2, "width 20000 is out of range"),
    ];
    for (file, line, text) in expect {
        assert!(
            has(&errors, file, line, text),
            "{file}:{line}: {text}\n{shown:#?}"
        );
    }
    assert_eq!(errors.len(), expect.len(), "{shown:#?}");
    Ok(())
}

#[test]
fn a_maximum_size_image_is_accepted_up_to_the_header() -> TestResult {
    // 16384 is the format's limit and passes the header; the missing raster is then the
    // (located) problem, not the size.
    let mut t = ContentTree::new();
    t.insert("art/edge.pgm", "P5\n16384 1\n255\n");
    let errors = errors(&t)?;
    assert!(has(&errors, "art/edge.pgm", 0, "needs 16384"), "{errors:?}");
    Ok(())
}

#[test]
fn bad_sidecar_values_are_reported_at_their_line() -> TestResult {
    let mut t = ContentTree::new();
    let rgb = ppm(4, 4, |_, _| [1, 2, 3]);
    let gray = pgm(4, 4, |_, _| 9);
    let cases: [(&str, &[u8], &str, usize, &str); 13] = [
        (
            "unknown",
            &rgb,
            "mips = true\ncolour = 1\n",
            2,
            "unknown key `colour`",
        ),
        (
            "encoding",
            &rgb,
            "\nencoding = \"bc9\"\n",
            2,
            "unknown encoding `bc9`",
        ),
        (
            "enctype",
            &rgb,
            "encoding = 1\n",
            1,
            "`encoding` must be a string",
        ),
        (
            "srgb4",
            &rgb,
            "encoding = \"bc4\"\nsrgb = true\n",
            2,
            "`srgb` is allowed with bc1, bc7, and rgba8 only",
        ),
        ("srgbgray", &gray, "srgb = true\n", 1, "not bc4"),
        (
            "normal1",
            &rgb,
            "encoding = \"bc1\"\nnormal_map = true\n",
            2,
            "`encoding` (line 1) is bc1",
        ),
        (
            "normalsrgb",
            &rgb,
            "normal_map = true\nsrgb = true\n",
            2,
            "a normal map is linear data",
        ),
        (
            "normalgray",
            &gray,
            "normal_map = true\n",
            1,
            "needs an RGB or RGBA image",
        ),
        (
            "cutbc7",
            &rgb,
            "encoding = \"bc7\"\nalpha_cutoff = 0.5\n",
            2,
            "applies to bc1 only",
        ),
        ("cutrange", &rgb, "alpha_cutoff = 1.5\n", 1, "outside 0 to 1"),
        (
            "mipstype",
            &rgb,
            "mips = \"yes\"\n",
            1,
            "`mips` must be true or false",
        ),
        (
            "table",
            &rgb,
            "mips = true\n\n[extra]\nsrgb = true\n",
            3,
            "no tables",
        ),
        ("syntax", &rgb, "mips = true\nsrgb = \n", 2, ""),
    ];
    for (name, image, sidecar, _, _) in &cases {
        let ext = if image.starts_with(b"P5") { "pgm" } else { "ppm" };
        t.insert(&format!("art/{name}.{ext}"), image.to_vec());
        t.insert(&format!("art/{name}.{ext}.texture.toml"), *sidecar);
    }
    let errors = errors(&t)?;
    let shown: Vec<String> = errors.iter().map(ToString::to_string).collect();
    for (name, image, _, line, text) in cases {
        let ext = if image.starts_with(b"P5") { "pgm" } else { "ppm" };
        let file = format!("art/{name}.{ext}.texture.toml");
        assert!(
            has(&errors, &file, line, text),
            "{file}:{line}: {text}\n{shown:#?}"
        );
    }
    assert_eq!(errors.len(), cases.len(), "{shown:#?}");
    Ok(())
}

#[test]
fn sidecars_are_inputs_only_for_images() -> TestResult {
    let mut t = ContentTree::new();
    t.insert("art/a.ppm", ppm(4, 4, |_, _| [1, 2, 3]));
    t.insert("art/a.ppm.texture.toml", "mips = false\n");
    t.insert("art/b.png.texture.toml", "mips = false\n");
    let errors = errors(&t)?;
    assert_eq!(errors.len(), 1, "{errors:?}");
    assert!(
        has(&errors, "art/b.png.texture.toml", 0, "no importer"),
        "{errors:?}"
    );
    let importer = Netpbm;
    assert!(importer.accepts("x/y.PPM") && importer.accepts("y.pgm") && importer.accepts("z.pam"));
    assert!(!importer.accepts("y.ppm.texture.toml") && !importer.accepts("y.png"));
    assert!(importer.inputs("y.pam.texture.toml") && !importer.inputs("y.texture.toml"));
    Ok(())
}

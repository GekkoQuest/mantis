//! The `<image>.texture.toml` sidecar: per-texture cook settings, with line-located
//! errors. See the [module documentation](super) for the keys and defaults.

use crate::source::{Doc, Fields};
use mantis_formats::texture::Encoding;

use crate::importer::{CookError, Source};

/// Resolved settings for one texture.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Settings {
    /// Storage.
    pub encoding: Encoding,
    /// Color data in sRGB.
    pub srgb: bool,
    /// A tangent-space normal map (BC5, R and G hold X and Y).
    pub normal_map: bool,
    /// Generate the full mip chain.
    pub mips: bool,
    /// BC1 punch-through: texels with alpha below this become transparent.
    pub alpha_cutoff: Option<u8>,
}

const KEYS: [&str; 5] = ["encoding", "srgb", "normal_map", "mips", "alpha_cutoff"];

/// The settings for an image with `channels` channels, from its optional `sidecar`.
///
/// # Errors
/// [`CookError`] at the sidecar line of a syntax error, an unknown key, a value of the
/// wrong type, or a combination the texture format refuses.
pub fn resolve(sidecar: Option<&Source<'_>>, channels: u8) -> Result<Settings, CookError> {
    let default_encoding = match channels {
        1 => Encoding::Bc4,
        3 => Encoding::Bc1,
        _ => Encoding::Bc7,
    };
    let Some(sidecar) = sidecar else {
        return Ok(Settings {
            encoding: default_encoding,
            srgb: default_encoding != Encoding::Bc4,
            normal_map: false,
            mips: true,
            alpha_cutoff: None,
        });
    };
    let doc = Doc::from_source(sidecar)?;
    if let Some(t) = doc.tables().next() {
        return Err(t.err(t.line(), "texture settings take no tables, only top-level keys"));
    }
    let f = doc.root();
    f.only(&KEYS)?;
    let encoding = f
        .opt_str("encoding")?
        .map(|(s, line)| match parse_encoding(s) {
            Some(e) => Ok((e, line)),
            None => Err(f.err(
                line,
                &format!("unknown encoding `{s}` (expected bc1, bc4, bc5, bc7, or rgba8)"),
            )),
        })
        .transpose()?;
    let normal_map = f.opt_bool("normal_map")?;
    let srgb = f.opt_bool("srgb")?;
    let mips = f.opt_bool("mips")?;
    let cutoff = f.opt_f32("alpha_cutoff")?;

    let is_normal = normal_map.is_some_and(|(n, _)| n);
    if let Some((_, line)) = normal_map.filter(|(n, _)| *n) {
        if let Some((e, eline)) = encoding.filter(|(e, _)| *e != Encoding::Bc5) {
            return Err(f.err(
                line,
                &format!(
                    "a normal map is stored as bc5, but `encoding` (line {eline}) is {}",
                    name(e)
                ),
            ));
        }
        if channels < 3 {
            return Err(f.err(
                line,
                "a normal map needs an RGB or RGBA image (R and G hold X and Y)",
            ));
        }
    }
    let encoding = match encoding {
        Some((e, _)) => e,
        None if is_normal => Encoding::Bc5,
        None => default_encoding,
    };
    let (srgb, alpha_cutoff) = color_options(&f, encoding, is_normal, srgb, cutoff)?;
    Ok(Settings {
        encoding,
        srgb,
        normal_map: is_normal,
        mips: mips.is_none_or(|(m, _)| m),
        alpha_cutoff,
    })
}

/// The sRGB flag and alpha cutoff, checked against the resolved encoding.
fn color_options(
    f: &Fields<'_>,
    encoding: Encoding,
    is_normal: bool,
    srgb: Option<(bool, usize)>,
    cutoff: Option<(f32, usize)>,
) -> Result<(bool, Option<u8>), CookError> {
    let srgb_allowed = matches!(encoding, Encoding::Bc1 | Encoding::Bc7 | Encoding::Rgba8) && !is_normal;
    let srgb = match srgb {
        Some((true, line)) if !srgb_allowed => {
            return Err(f.err(
                line,
                &if is_normal {
                    "a normal map is linear data; `srgb` must be false".to_owned()
                } else {
                    format!(
                        "`srgb` is allowed with bc1, bc7, and rgba8 only, not {}",
                        name(encoding)
                    )
                },
            ));
        }
        Some((s, _)) => s,
        None => srgb_allowed,
    };
    let alpha_cutoff = match cutoff {
        Some((_, line)) if encoding != Encoding::Bc1 => {
            return Err(f.err(
                line,
                &format!("`alpha_cutoff` applies to bc1 only, not {}", name(encoding)),
            ));
        }
        Some((v, line)) if !(0.0..=1.0).contains(&v) => {
            return Err(f.err(line, &format!("`alpha_cutoff` {v} is outside 0 to 1")));
        }
        Some((v, _)) => Some(super::fit::round_clamp(v * 255.0, 255)),
        None => None,
    };
    Ok((srgb, alpha_cutoff))
}

/// The encoding spelled `s` in a sidecar.
pub fn parse_encoding(s: &str) -> Option<Encoding> {
    [
        Encoding::Bc1,
        Encoding::Bc4,
        Encoding::Bc5,
        Encoding::Bc7,
        Encoding::Rgba8,
    ]
    .into_iter()
    .find(|&e| name(e) == s)
}

/// The sidecar spelling of an encoding.
pub fn name(e: Encoding) -> &'static str {
    match e {
        Encoding::Bc1 => "bc1",
        Encoding::Bc4 => "bc4",
        Encoding::Bc5 => "bc5",
        Encoding::Bc7 => "bc7",
        Encoding::Rgba8 => "rgba8",
    }
}

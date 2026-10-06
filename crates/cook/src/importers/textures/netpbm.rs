//! Binary Netpbm images: P5 (PGM, gray), P6 (PPM, RGB), and P7 (PAM, 1 to 4 channels),
//! 8 bits per sample (`MAXVAL` 255), one image per file.
//!
//! P5 and P6: the magic, then width, height, and `MAXVAL` as decimal numbers separated by
//! whitespace (`#` starts a comment that runs to the end of the line), then exactly one
//! whitespace byte, then the raster: rows top to bottom, samples interleaved.
//!
//! P7: the magic and a newline, then header lines `WIDTH n`, `HEIGHT n`, `DEPTH n`,
//! `MAXVAL n`, and `TUPLTYPE t` (each exactly once; `#` lines are comments), then
//! `ENDHDR` and a newline, then the raster. `DEPTH` and `TUPLTYPE` must agree:
//! 1 `GRAYSCALE`, 2 `GRAYSCALE_ALPHA`, 3 `RGB`, 4 `RGB_ALPHA`.
//!
//! The raster must hold exactly `width · height · channels` bytes. Width and height are
//! 1 to [`mantis_formats::texture::MAX_SIZE`].

use mantis_formats::texture::MAX_SIZE;

/// A decoded image.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Image {
    /// Width in texels.
    pub width: u32,
    /// Height in texels.
    pub height: u32,
    /// Channels in the source: 1 gray, 2 gray and alpha, 3 RGB, 4 RGBA.
    pub channels: u8,
    /// Row-major texels as RGBA (gray replicated to R, G, and B; alpha 255 when absent).
    pub texels: Vec<[u8; 4]>,
}

/// A parse error. `line` is the 1-based header line, or 0 for the raster.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct NetpbmError {
    /// Header line (1-based), or 0 when the problem is in the raster.
    pub line: usize,
    /// What is wrong.
    pub message: String,
}

impl NetpbmError {
    fn at(line: usize, message: impl Into<String>) -> Self {
        Self {
            line,
            message: message.into(),
        }
    }
}

/// Which Netpbm format a file is.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Format {
    /// P5 gray map.
    Pgm,
    /// P6 pixel map.
    Ppm,
    /// P7 arbitrary map.
    Pam,
}

impl Format {
    /// The magic bytes.
    pub fn magic(self) -> &'static str {
        match self {
            Format::Pgm => "P5",
            Format::Ppm => "P6",
            Format::Pam => "P7",
        }
    }
}

/// Parses an image of `format`.
///
/// # Errors
/// [`NetpbmError`] for a different or malformed header, an unsupported `MAXVAL`, a size
/// out of range, or a raster of the wrong length.
pub fn parse(bytes: &[u8], format: Format) -> Result<Image, NetpbmError> {
    let magic = bytes.get(..2).unwrap_or(bytes);
    if magic != format.magic().as_bytes() {
        let shown = String::from_utf8_lossy(magic);
        return Err(NetpbmError::at(
            1,
            format!(
                "expected magic `{}` (binary {}), found `{shown}`",
                format.magic(),
                match format {
                    Format::Pgm => "PGM",
                    Format::Ppm => "PPM",
                    Format::Pam => "PAM",
                }
            ),
        ));
    }
    let mut header = Header {
        bytes,
        at: 2,
        line: 1,
    };
    let (width, height, channels) = match format {
        Format::Pgm | Format::Ppm => header.classic(if format == Format::Pgm { 1 } else { 3 })?,
        Format::Pam => header.pam()?,
    };
    let raster = bytes.get(header.at..).unwrap_or(&[]);
    let needed = u64::from(width) * u64::from(height) * u64::from(channels);
    let have = raster.len() as u64;
    if have < needed {
        return Err(NetpbmError::at(
            0,
            format!("the raster has {have} bytes; {width}x{height} with {channels} channels needs {needed}"),
        ));
    }
    if have > needed {
        return Err(NetpbmError::at(
            0,
            format!(
                "{} bytes follow the raster ({width}x{height} with {channels} channels is {needed} bytes; one image per file)",
                have - needed
            ),
        ));
    }
    let texels = raster
        .chunks_exact(usize::from(channels))
        .map(|s| match *s {
            [g] => [g, g, g, 255],
            [g, a] => [g, g, g, a],
            [r, g, b] => [r, g, b, 255],
            [r, g, b, a] => [r, g, b, a],
            _ => [0; 4],
        })
        .collect();
    Ok(Image {
        width,
        height,
        channels,
        texels,
    })
}

struct Header<'a> {
    bytes: &'a [u8],
    at: usize,
    line: usize,
}

fn is_space(b: u8) -> bool {
    matches!(b, b' ' | b'\t' | b'\n' | b'\r' | 0x0b | 0x0c)
}

fn check_size(what: &str, v: u32, line: usize) -> Result<u32, NetpbmError> {
    if (1..=MAX_SIZE).contains(&v) {
        Ok(v)
    } else {
        Err(NetpbmError::at(
            line,
            format!("{what} {v} is out of range (1 to {MAX_SIZE})"),
        ))
    }
}

fn check_maxval(v: u32, line: usize) -> Result<(), NetpbmError> {
    if v == 255 {
        Ok(())
    } else {
        Err(NetpbmError::at(
            line,
            format!("unsupported MAXVAL {v} (only 255, 8 bits per sample)"),
        ))
    }
}

fn number(text: &str, what: &str, line: usize) -> Result<u32, NetpbmError> {
    if text.is_empty() || !text.bytes().all(|b| b.is_ascii_digit()) {
        return Err(NetpbmError::at(
            line,
            format!("expected {what} as a decimal number, found `{text}`"),
        ));
    }
    text.parse::<u32>()
        .map_err(|_| NetpbmError::at(line, format!("{what} `{text}` is too large")))
}

impl Header<'_> {
    fn peek(&self) -> Option<u8> {
        self.bytes.get(self.at).copied()
    }

    /// Skips whitespace and comments, counting lines.
    fn skip(&mut self) {
        while let Some(b) = self.peek() {
            if b == b'#' {
                while self.peek().is_some_and(|b| b != b'\n' && b != b'\r') {
                    self.at += 1;
                }
            } else if is_space(b) {
                if b == b'\n' {
                    self.line += 1;
                }
                self.at += 1;
            } else {
                break;
            }
        }
    }

    /// A whitespace-separated decimal field of a P5 or P6 header.
    fn field(&mut self, what: &str) -> Result<(u32, usize), NetpbmError> {
        if self.peek().is_none() {
            return Err(NetpbmError::at(
                self.line,
                format!("the header ends before {what}"),
            ));
        }
        if !self.peek().is_some_and(|b| is_space(b) || b == b'#') {
            return Err(NetpbmError::at(
                self.line,
                format!("expected whitespace before {what}"),
            ));
        }
        self.skip();
        let start = self.at;
        while self.peek().is_some_and(|b| !is_space(b) && b != b'#') {
            self.at += 1;
        }
        let text = String::from_utf8_lossy(self.bytes.get(start..self.at).unwrap_or(&[])).into_owned();
        if text.is_empty() {
            return Err(NetpbmError::at(
                self.line,
                format!("the header ends before {what}"),
            ));
        }
        Ok((number(&text, what, self.line)?, self.line))
    }

    fn classic(&mut self, channels: u8) -> Result<(u32, u32, u8), NetpbmError> {
        let (w, wl) = self.field("the width")?;
        let (h, hl) = self.field("the height")?;
        let (m, ml) = self.field("MAXVAL")?;
        let width = check_size("width", w, wl)?;
        let height = check_size("height", h, hl)?;
        check_maxval(m, ml)?;
        match self.peek() {
            Some(b) if is_space(b) => self.at += 1,
            _ => {
                return Err(NetpbmError::at(
                    self.line,
                    "expected one whitespace byte between MAXVAL and the raster",
                ));
            }
        }
        Ok((width, height, channels))
    }

    /// The next header line of a P7 file (without its newline).
    fn next_line(&mut self) -> Option<&str> {
        let rest = self.bytes.get(self.at..)?;
        if rest.is_empty() {
            return None;
        }
        let len = rest.iter().position(|&b| b == b'\n').unwrap_or(rest.len());
        self.at += len + 1;
        self.line += 1;
        core::str::from_utf8(rest.get(..len)?).ok().or(Some("\u{fffd}"))
    }

    fn pam(&mut self) -> Result<(u32, u32, u8), NetpbmError> {
        if self.peek() != Some(b'\n') {
            return Err(NetpbmError::at(1, "expected a newline after `P7`"));
        }
        self.at += 1;
        let mut fields: [Option<(u32, usize)>; 4] = [None; 4];
        let mut tupltype: Option<(String, usize)> = None;
        loop {
            let Some(text) = self.next_line() else {
                return Err(NetpbmError::at(self.line, "the header ends before `ENDHDR`"));
            };
            let text = text.trim().to_owned();
            let line = self.line;
            if text.is_empty() || text.starts_with('#') {
                continue;
            }
            let (key, value) = text
                .split_once(char::is_whitespace)
                .unwrap_or((text.as_str(), ""));
            let value = value.trim();
            let slot = match key {
                "ENDHDR" if value.is_empty() => break,
                "WIDTH" => 0,
                "HEIGHT" => 1,
                "DEPTH" => 2,
                "MAXVAL" => 3,
                "TUPLTYPE" => {
                    if tupltype.is_some() {
                        return Err(NetpbmError::at(line, "`TUPLTYPE` appears twice"));
                    }
                    tupltype = Some((value.to_owned(), line));
                    continue;
                }
                _ => return Err(NetpbmError::at(line, format!("unexpected header line `{text}`"))),
            };
            let n = number(value, key, line)?;
            match fields.get_mut(slot) {
                Some(Some(_)) => return Err(NetpbmError::at(line, format!("`{key}` appears twice"))),
                Some(f) => *f = Some((n, line)),
                None => {}
            }
        }
        let end = self.line;
        let need = |i: usize, key: &str| {
            fields
                .get(i)
                .copied()
                .flatten()
                .ok_or_else(|| NetpbmError::at(end, format!("`{key}` is missing")))
        };
        let (w, wl) = need(0, "WIDTH")?;
        let (h, hl) = need(1, "HEIGHT")?;
        let (d, dl) = need(2, "DEPTH")?;
        let (m, ml) = need(3, "MAXVAL")?;
        let width = check_size("width", w, wl)?;
        let height = check_size("height", h, hl)?;
        check_maxval(m, ml)?;
        let channels = u8::try_from(d)
            .ok()
            .filter(|d| (1..=4).contains(d))
            .ok_or_else(|| NetpbmError::at(dl, format!("unsupported DEPTH {d} (1 to 4)")))?;
        let expected = match channels {
            1 => "GRAYSCALE",
            2 => "GRAYSCALE_ALPHA",
            3 => "RGB",
            _ => "RGB_ALPHA",
        };
        let Some((tuple, tl)) = tupltype else {
            return Err(NetpbmError::at(end, "`TUPLTYPE` is missing"));
        };
        if tuple != expected {
            return Err(NetpbmError::at(
                tl,
                format!("TUPLTYPE `{tuple}` does not match DEPTH {channels} (expected `{expected}`)"),
            ));
        }
        Ok((width, height, channels))
    }
}

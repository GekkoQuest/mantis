//! Wavefront OBJ reading: `v`, `vt`, `vt2` (second UV set, an engine extension), `vn`,
//! and `f` with every index form, negative indices included. Faces are fan-triangulated.
//! Positions and normals stay in the source's right-handed frame here; [`super::mesh`]
//! converts them.

use crate::importer::{CookError, Source};

/// One face corner: 0-based position, texture, and normal indices.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) struct Corner {
    /// Position index.
    pub(crate) v: u32,
    /// Texture coordinate index (also indexes `vt2`).
    pub(crate) vt: Option<u32>,
    /// Normal index.
    pub(crate) vn: Option<u32>,
}

/// A parsed OBJ file.
#[derive(Clone, Debug, Default)]
pub(crate) struct Obj {
    /// `v` positions (extra components ignored).
    pub(crate) positions: Vec<[f32; 3]>,
    /// The 1-based line of each `v`.
    pub(crate) position_lines: Vec<usize>,
    /// `vt` coordinates (`u v`, missing `v` reads as 0).
    pub(crate) uvs: Vec<[f32; 2]>,
    /// `vt2` coordinates, parallel to `uvs`.
    pub(crate) uvs2: Vec<[f32; 2]>,
    /// Line of the first `vt2`, for errors.
    pub(crate) uv2_line: usize,
    /// `vn` normals, unit length.
    pub(crate) normals: Vec<[f32; 3]>,
    /// Triangles in the source's winding (counter-clockwise from outside).
    pub(crate) triangles: Vec<[Corner; 3]>,
}

fn floats<'s>(
    path: &str,
    line: usize,
    words: impl Iterator<Item = &'s str>,
    min: usize,
    max: usize,
) -> Result<Vec<f32>, CookError> {
    let mut out = Vec::new();
    for w in words {
        let v = w
            .parse::<f32>()
            .ok()
            .filter(|v| v.is_finite())
            .ok_or_else(|| CookError::at(path, line, &format!("`{w}` is not a finite number")))?;
        out.push(v);
    }
    if out.len() < min || out.len() > max {
        return Err(CookError::at(
            path,
            line,
            &format!("expected {min} to {max} numbers, found {}", out.len()),
        ));
    }
    Ok(out)
}

/// Resolves a 1-based (or negative, relative) OBJ index against `count` elements.
fn index(path: &str, line: usize, word: &str, count: usize, what: &str) -> Result<u32, CookError> {
    let bad = || CookError::at(path, line, &format!("bad {what} index `{word}`"));
    let i: i64 = word.parse().map_err(|_| bad())?;
    let count = i64::try_from(count).map_err(|_| bad())?;
    let resolved = match i {
        0 => {
            return Err(CookError::at(
                path,
                line,
                &format!("{what} index 0 (OBJ indices start at 1)"),
            ));
        }
        i if i > 0 => i - 1,
        i => count + i,
    };
    if resolved < 0 || resolved >= count {
        return Err(CookError::at(
            path,
            line,
            &format!("{what} index `{word}` is out of range ({count} defined so far)"),
        ));
    }
    u32::try_from(resolved).map_err(|_| bad())
}

impl Obj {
    fn corner(&self, path: &str, line: usize, word: &str) -> Result<Corner, CookError> {
        let mut parts = word.split('/');
        let v = parts.next().unwrap_or("");
        let vt = parts.next().filter(|s| !s.is_empty());
        let vn = parts.next().filter(|s| !s.is_empty());
        if parts.next().is_some() {
            return Err(CookError::at(path, line, &format!("bad face corner `{word}`")));
        }
        Ok(Corner {
            v: index(path, line, v, self.positions.len(), "position")?,
            vt: vt
                .map(|w| index(path, line, w, self.uvs.len(), "texture"))
                .transpose()?,
            vn: vn
                .map(|w| index(path, line, w, self.normals.len(), "normal"))
                .transpose()?,
        })
    }

    /// Parses OBJ text.
    pub(crate) fn parse(source: &Source<'_>) -> Result<Obj, CookError> {
        let path = source.path;
        let text = source.text()?;
        let mut obj = Obj::default();
        for (n, raw) in text.lines().enumerate() {
            let line = n + 1;
            let content = raw.split('#').next().unwrap_or("");
            let mut words = content.split_whitespace();
            let Some(keyword) = words.next() else {
                continue;
            };
            match keyword {
                "v" => {
                    let f = floats(path, line, words, 3, 7)?;
                    obj.positions.push([
                        f.first().copied().unwrap_or(0.0),
                        f.get(1).copied().unwrap_or(0.0),
                        f.get(2).copied().unwrap_or(0.0),
                    ]);
                    obj.position_lines.push(line);
                }
                "vt" => {
                    let f = floats(path, line, words, 1, 3)?;
                    obj.uvs.push([
                        f.first().copied().unwrap_or(0.0),
                        f.get(1).copied().unwrap_or(0.0),
                    ]);
                }
                "vt2" => {
                    let f = floats(path, line, words, 2, 2)?;
                    if obj.uvs2.is_empty() {
                        obj.uv2_line = line;
                    }
                    obj.uvs2.push([
                        f.first().copied().unwrap_or(0.0),
                        f.get(1).copied().unwrap_or(0.0),
                    ]);
                }
                "vn" => {
                    let f = floats(path, line, words, 3, 3)?;
                    let v = [
                        f.first().copied().unwrap_or(0.0),
                        f.get(1).copied().unwrap_or(0.0),
                        f.get(2).copied().unwrap_or(0.0),
                    ];
                    let len = (v[0] * v[0] + v[1] * v[1] + v[2] * v[2]).sqrt();
                    if !len.is_finite() || len <= 1e-12 {
                        return Err(CookError::at(path, line, "a normal of zero length"));
                    }
                    obj.normals.push(v.map(|c| c / len));
                }
                "f" => {
                    let corners = words
                        .map(|w| obj.corner(path, line, w))
                        .collect::<Result<Vec<_>, _>>()?;
                    let Some((first, rest)) = corners.split_first() else {
                        return Err(CookError::at(path, line, "a face needs at least 3 corners"));
                    };
                    if rest.len() < 2 {
                        return Err(CookError::at(path, line, "a face needs at least 3 corners"));
                    }
                    for pair in rest.windows(2) {
                        if let [b, c] = pair {
                            obj.triangles.push([*first, *b, *c]);
                        }
                    }
                }
                "o" | "g" | "s" | "usemtl" | "mtllib" => {}
                other => {
                    return Err(CookError::at(
                        path,
                        line,
                        &format!("unsupported OBJ statement `{other}`"),
                    ));
                }
            }
        }
        if obj.triangles.is_empty() {
            return Err(CookError::at(path, 0, "no faces"));
        }
        Ok(obj)
    }
}

//! Line-located readers over the strict TOML subset of `mantis_core::module::toml`:
//! required and optional typed fields, unknown-key rejection, and header lines for
//! errors about a whole table.

use mantis_core::module::toml::{self, Document, Entry, Table, Value};

use crate::importer::{CookError, Source};

/// A parsed structured source with the line of every table header.
pub(crate) struct Doc<'a> {
    path: &'a str,
    document: Document,
    headers: Vec<(String, usize)>,
}

impl<'a> Doc<'a> {
    /// Parses `source` as TOML.
    pub(crate) fn parse(source: &Source<'a>) -> Result<Self, CookError> {
        let text = source.text()?;
        let doc = toml::parse(text).map_err(|e| CookError::at(source.path, e.line, e.what))?;
        let headers = text
            .lines()
            .enumerate()
            .filter_map(|(n, line)| header_name(line).map(|name| (name, n + 1)))
            .collect();
        Ok(Self {
            path: source.path,
            document: doc,
            headers,
        })
    }

    /// The source path.
    pub(crate) fn path(&self) -> &'a str {
        self.path
    }

    /// The root table (keys before the first header).
    pub(crate) fn root(&self) -> Fields<'_> {
        self.fields_of(self.document.tables.first())
    }

    /// Every named table, in file order.
    pub(crate) fn tables(&self) -> impl Iterator<Item = Fields<'_>> {
        self.document
            .tables
            .iter()
            .filter(|t| !t.name.is_empty())
            .map(|t| self.fields_of(Some(t)))
    }

    /// Named tables whose name starts with `prefix.`, with the rest of the name.
    pub(crate) fn tables_under<'s>(
        &'s self,
        prefix: &'s str,
    ) -> impl Iterator<Item = (&'s str, Fields<'s>)> + 's {
        self.tables().filter_map(move |f| {
            let rest = f.name().strip_prefix(prefix)?.strip_prefix('.')?;
            Some((rest, f))
        })
    }

    fn fields_of<'s>(&'s self, table: Option<&'s Table>) -> Fields<'s> {
        let name = table.map_or("", |t| t.name.as_str());
        let line = if name.is_empty() {
            0
        } else {
            self.headers
                .iter()
                .find(|(n, _)| n == name)
                .map_or(0, |(_, l)| *l)
        };
        Fields {
            path: self.path,
            table,
            line,
        }
    }

    /// Rejects every table whose name does not start with one of `prefixes` (each
    /// followed by `.`) and is not one of `exact`.
    pub(crate) fn only_tables(&self, prefixes: &[&str], exact: &[&str]) -> Result<(), CookError> {
        for t in self.tables() {
            let name = t.name();
            let known = exact.contains(&name)
                || prefixes.iter().any(|p| {
                    name.strip_prefix(p)
                        .and_then(|r| r.strip_prefix('.'))
                        .is_some_and(|r| !r.is_empty())
                });
            if !known {
                return Err(t.err(t.line(), &format!("unknown table `[{name}]`")));
            }
        }
        Ok(())
    }
}

/// The name of a `[header]` line, if the line is one.
fn header_name(line: &str) -> Option<String> {
    let body = line.trim().strip_prefix('[')?;
    let body = body.split(']').next()?.trim();
    Some(match body.strip_prefix('"').and_then(|b| b.strip_suffix('"')) {
        Some(quoted) => quoted.to_owned(),
        None => body.to_owned(),
    })
}

/// One table's fields, with line-located errors.
#[derive(Clone, Copy)]
pub(crate) struct Fields<'a> {
    path: &'a str,
    table: Option<&'a Table>,
    line: usize,
}

impl<'a> Fields<'a> {
    /// The table name (`""` for the root).
    pub(crate) fn name(&self) -> &'a str {
        self.table.map_or("", |t| t.name.as_str())
    }

    /// The header line (0 for the root).
    pub(crate) fn line(&self) -> usize {
        self.line
    }

    /// An error at `line` of this source.
    pub(crate) fn err(&self, line: usize, message: &str) -> CookError {
        CookError::at(self.path, line, message)
    }

    fn entry(&self, key: &str) -> Option<&'a Entry> {
        self.table?.entries.iter().find(|e| e.key == key)
    }

    /// Whether `key` is present.
    pub(crate) fn has(&self, key: &str) -> bool {
        self.entry(key).is_some()
    }

    /// The line of `key`, or the header line when it is absent.
    pub(crate) fn line_of(&self, key: &str) -> usize {
        self.entry(key).map_or(self.line, |e| e.line)
    }

    /// Rejects keys not in `allowed`.
    pub(crate) fn only(&self, allowed: &[&str]) -> Result<(), CookError> {
        for e in self.table.map_or(&[][..], |t| t.entries.as_slice()) {
            if !allowed.contains(&e.key.as_str()) {
                let place = if self.name().is_empty() {
                    String::new()
                } else {
                    format!(" in `[{}]`", self.name())
                };
                return Err(self.err(e.line, &format!("unknown key `{}`{place}", e.key)));
            }
        }
        Ok(())
    }

    fn required(&self, key: &str) -> Result<&'a Entry, CookError> {
        self.entry(key).ok_or_else(|| {
            let place = if self.name().is_empty() {
                String::new()
            } else {
                format!(" in `[{}]`", self.name())
            };
            self.err(self.line, &format!("missing `{key}`{place}"))
        })
    }

    fn wrong(&self, e: &Entry, what: &str) -> CookError {
        self.err(e.line, &format!("`{}` must be {what}", e.key))
    }

    /// A required string.
    pub(crate) fn str(&self, key: &str) -> Result<&'a str, CookError> {
        let e = self.required(key)?;
        e.value.as_str().ok_or_else(|| self.wrong(e, "a string"))
    }

    /// An optional string.
    pub(crate) fn opt_str(&self, key: &str) -> Result<Option<&'a str>, CookError> {
        match self.entry(key) {
            None => Ok(None),
            Some(e) => e
                .value
                .as_str()
                .map(Some)
                .ok_or_else(|| self.wrong(e, "a string")),
        }
    }

    /// A required finite number.
    pub(crate) fn f32(&self, key: &str) -> Result<f32, CookError> {
        let e = self.required(key)?;
        crate::source::float_of(&e.value).ok_or_else(|| self.wrong(e, "a finite number"))
    }

    /// An optional finite number.
    pub(crate) fn opt_f32(&self, key: &str) -> Result<Option<f32>, CookError> {
        match self.entry(key) {
            None => Ok(None),
            Some(e) => crate::source::float_of(&e.value)
                .map(Some)
                .ok_or_else(|| self.wrong(e, "a finite number")),
        }
    }

    /// An optional boolean.
    pub(crate) fn opt_bool(&self, key: &str) -> Result<Option<bool>, CookError> {
        match self.entry(key) {
            None => Ok(None),
            Some(e) => e
                .value
                .as_bool()
                .map(Some)
                .ok_or_else(|| self.wrong(e, "true or false")),
        }
    }

    /// A required array of finite numbers.
    pub(crate) fn floats(&self, key: &str) -> Result<Vec<f32>, CookError> {
        let e = self.required(key)?;
        let Value::Array(items) = &e.value else {
            return Err(self.wrong(e, "an array of numbers"));
        };
        items
            .iter()
            .map(crate::source::float_of)
            .collect::<Option<Vec<f32>>>()
            .ok_or_else(|| self.wrong(e, "an array of finite numbers"))
    }

    /// An optional array of exactly `N` finite numbers.
    pub(crate) fn opt_vec<const N: usize>(&self, key: &str) -> Result<Option<[f32; N]>, CookError> {
        if !self.has(key) {
            return Ok(None);
        }
        let v = self.floats(key)?;
        <[f32; N]>::try_from(v.as_slice())
            .map(Some)
            .map_err(|_| self.err(self.line_of(key), &format!("`{key}` must have {N} numbers")))
    }

    /// A required array of integers.
    pub(crate) fn ints(&self, key: &str) -> Result<Vec<i64>, CookError> {
        let e = self.required(key)?;
        let Value::Array(items) = &e.value else {
            return Err(self.wrong(e, "an array of integers"));
        };
        items
            .iter()
            .map(|v| match v {
                Value::Int(i) => Some(*i),
                _ => None,
            })
            .collect::<Option<Vec<i64>>>()
            .ok_or_else(|| self.wrong(e, "an array of integers"))
    }

    /// A required array of strings.
    pub(crate) fn strings(&self, key: &str) -> Result<Vec<String>, CookError> {
        let e = self.required(key)?;
        e.value
            .as_strings()
            .ok_or_else(|| self.wrong(e, "an array of strings"))
    }

    /// An optional array of strings (empty when absent).
    pub(crate) fn opt_strings(&self, key: &str) -> Result<Vec<String>, CookError> {
        if self.has(key) {
            self.strings(key)
        } else {
            Ok(Vec::new())
        }
    }
}

/// `path` with `suffix` replaced by `ext` (output names: `a/b.clip.toml` to `a/b.clip`).
pub(crate) fn output_name(path: &str, suffix: &str, ext: &str) -> String {
    format!("{}{ext}", path.strip_suffix(suffix).unwrap_or(path))
}

/// Whether the file name of `path` ends with `suffix` and has a non-empty stem.
pub(crate) fn has_suffix(path: &str, suffix: &str) -> bool {
    let name = path.rsplit('/').next().unwrap_or(path);
    name.len() > suffix.len() && name.ends_with(suffix)
}

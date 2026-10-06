//! Typed, line-located access to the strict TOML subset (`mantis_core::module::toml`)
//! shared by every content importer. Every reader rejects unknown tables and keys, so a
//! misspelled field fails the cook at its line instead of silently taking a default.

use std::collections::BTreeMap;

use mantis_core::module::toml::{self, Document, Entry, Table, Value};

use crate::importer::{CookError, Source};

/// A parsed source document and the line of each table header.
pub(crate) struct Doc<'a> {
    path: &'a str,
    parsed: Document,
    headers: BTreeMap<String, usize>,
}

impl<'a> Doc<'a> {
    /// Parses `source` as the TOML subset.
    pub(crate) fn parse(source: &Source<'a>) -> Result<Doc<'a>, CookError> {
        let text = source.text()?;
        let doc = toml::parse(text).map_err(|e| CookError::at(source.path, e.line, e.what))?;
        Ok(Doc {
            path: source.path,
            parsed: doc,
            headers: header_lines(text),
        })
    }

    /// The source path.
    pub(crate) fn path(&self) -> &'a str {
        self.path
    }

    /// An error at `line` of this source.
    pub(crate) fn err(&self, line: usize, message: &str) -> CookError {
        CookError::at(self.path, line, message)
    }

    fn fields<'d>(&'d self, table: &'d Table) -> Fields<'d> {
        let line = if table.name.is_empty() {
            0
        } else {
            self.headers
                .get(&table.name)
                .copied()
                .or_else(|| table.entries.first().map(|e| e.line.saturating_sub(1)))
                .unwrap_or(0)
        };
        Fields {
            path: self.path,
            name: &table.name,
            line,
            table,
        }
    }

    /// The root table (keys before the first header).
    pub(crate) fn root(&self) -> Option<Fields<'_>> {
        self.parsed.tables.first().map(|t| self.fields(t))
    }

    /// The table named exactly `name`.
    pub(crate) fn table(&self, name: &str) -> Option<Fields<'_>> {
        self.parsed.table(name).map(|t| self.fields(t))
    }

    /// Every table whose name starts with `prefix` (`"node."`), in file order, with the
    /// rest of its name.
    pub(crate) fn prefixed<'d>(
        &'d self,
        prefix: &'d str,
    ) -> impl Iterator<Item = (&'d str, Fields<'d>)> + 'd {
        self.parsed
            .tables
            .iter()
            .filter_map(move |t| t.name.strip_prefix(prefix).map(|rest| (rest, self.fields(t))))
    }

    /// Rejects any table that is not the root, one of `exact`, or `<prefix><name>` for
    /// one of `prefixes` with a non-empty name.
    pub(crate) fn only_tables(&self, exact: &[&str], prefixes: &[&str]) -> Result<(), CookError> {
        for t in self.parsed.tables.iter().skip(1) {
            let known = exact.contains(&t.name.as_str())
                || prefixes
                    .iter()
                    .any(|p| t.name.strip_prefix(p).is_some_and(|rest| !rest.is_empty()));
            if !known {
                let mut expected: Vec<String> = exact.iter().map(|e| format!("[{e}]")).collect();
                expected.extend(prefixes.iter().map(|p| format!("[{p}<name>]")));
                return Err(self.err(
                    self.fields(t).line,
                    &format!("unknown table `[{}]` (expected {})", t.name, expected.join(", ")),
                ));
            }
        }
        Ok(())
    }
}

/// The line of each `[name]` header.
fn header_lines(text: &str) -> BTreeMap<String, usize> {
    let mut out = BTreeMap::new();
    for (n, raw) in text.lines().enumerate() {
        let Some(rest) = raw.trim().strip_prefix('[') else {
            continue;
        };
        let Some(end) = rest.find(']') else {
            continue;
        };
        let name = rest.get(..end).unwrap_or("").trim();
        let name = name
            .strip_prefix('"')
            .and_then(|n| n.strip_suffix('"'))
            .unwrap_or(name);
        out.entry(name.to_owned()).or_insert(n + 1);
    }
    out
}

fn kind_of(v: &Value) -> &'static str {
    match v {
        Value::Str(_) => "a string",
        Value::Int(_) => "an integer",
        Value::Float(_) => "a float",
        Value::Bool(_) => "a boolean",
        Value::Array(_) => "an array",
    }
}

/// One table's fields.
#[derive(Clone, Copy)]
pub(crate) struct Fields<'d> {
    path: &'d str,
    /// The table name (`""` for the root).
    pub(crate) name: &'d str,
    /// The header line (0 for the root).
    pub(crate) line: usize,
    table: &'d Table,
}

impl<'d> Fields<'d> {
    /// An error at `line`.
    pub(crate) fn err(&self, line: usize, message: &str) -> CookError {
        CookError::at(self.path, line, message)
    }

    fn entry(&self, key: &str) -> Option<&'d Entry> {
        self.table.entries.iter().find(|e| e.key == key)
    }

    /// Whether `key` is present.
    pub(crate) fn has(&self, key: &str) -> bool {
        self.entry(key).is_some()
    }

    /// The line of `key`, or the header line when it is absent.
    pub(crate) fn line_of(&self, key: &str) -> usize {
        self.entry(key).map_or(self.line, |e| e.line)
    }

    /// Every entry, in file order.
    pub(crate) fn entries(&self) -> &'d [Entry] {
        &self.table.entries
    }

    fn where_(&self) -> String {
        if self.name.is_empty() {
            "the root table".to_owned()
        } else {
            format!("`[{}]`", self.name)
        }
    }

    /// Rejects keys not in `allowed`.
    pub(crate) fn only(&self, allowed: &[&str]) -> Result<(), CookError> {
        match self
            .table
            .entries
            .iter()
            .find(|e| !allowed.contains(&e.key.as_str()))
        {
            Some(e) => Err(self.err(
                e.line,
                &format!(
                    "unknown key `{}` in {} (expected one of: {})",
                    e.key,
                    self.where_(),
                    allowed.join(", ")
                ),
            )),
            None => Ok(()),
        }
    }

    fn required(&self, key: &str) -> Result<&'d Entry, CookError> {
        self.entry(key)
            .ok_or_else(|| self.err(self.line, &format!("{} is missing `{key}`", self.where_())))
    }

    fn wrong(&self, e: &Entry, expected: &str) -> CookError {
        self.err(
            e.line,
            &format!("`{}` must be {expected}, not {}", e.key, kind_of(&e.value)),
        )
    }

    /// A required string.
    pub(crate) fn str(&self, key: &str) -> Result<(&'d str, usize), CookError> {
        let e = self.required(key)?;
        match &e.value {
            Value::Str(s) => Ok((s, e.line)),
            _ => Err(self.wrong(e, "a string")),
        }
    }

    /// An optional string.
    pub(crate) fn opt_str(&self, key: &str) -> Result<Option<(&'d str, usize)>, CookError> {
        if self.has(key) {
            self.str(key).map(Some)
        } else {
            Ok(None)
        }
    }

    /// A required finite number.
    pub(crate) fn f32(&self, key: &str) -> Result<(f32, usize), CookError> {
        let e = self.required(key)?;
        crate::source::float_of(&e.value)
            .map(|v| (v, e.line))
            .ok_or_else(|| self.wrong(e, "a finite number"))
    }

    /// An optional finite number.
    pub(crate) fn opt_f32(&self, key: &str, default: f32) -> Result<(f32, usize), CookError> {
        if self.has(key) {
            self.f32(key)
        } else {
            Ok((default, self.line))
        }
    }

    /// A required integer that fits in `T`.
    pub(crate) fn int<T: TryFrom<i64>>(&self, key: &str) -> Result<(T, usize), CookError> {
        let e = self.required(key)?;
        let Value::Int(i) = e.value else {
            return Err(self.wrong(e, "an integer"));
        };
        let short = std::any::type_name::<T>();
        T::try_from(i)
            .map(|v| (v, e.line))
            .map_err(|_| self.err(e.line, &format!("`{key}` = {i} does not fit in {short}")))
    }

    /// An optional integer that fits in `T`.
    pub(crate) fn opt_int<T: TryFrom<i64>>(&self, key: &str, default: T) -> Result<(T, usize), CookError> {
        if self.has(key) {
            self.int(key)
        } else {
            Ok((default, self.line))
        }
    }

    /// An optional boolean.
    pub(crate) fn opt_bool(&self, key: &str, default: bool) -> Result<(bool, usize), CookError> {
        match self.entry(key) {
            None => Ok((default, self.line)),
            Some(e) => match e.value {
                Value::Bool(b) => Ok((b, e.line)),
                _ => Err(self.wrong(e, "a boolean")),
            },
        }
    }

    /// A required array of finite numbers.
    pub(crate) fn f32s(&self, key: &str) -> Result<(Vec<f32>, usize), CookError> {
        let e = self.required(key)?;
        let Value::Array(items) = &e.value else {
            return Err(self.wrong(e, "an array of numbers"));
        };
        let values: Option<Vec<f32>> = items.iter().map(crate::source::float_of).collect();
        values
            .map(|v| (v, e.line))
            .ok_or_else(|| self.err(e.line, &format!("`{key}` must hold only finite numbers")))
    }

    /// A required array of integers, each fitting in `T`.
    pub(crate) fn ints<T: TryFrom<i64>>(&self, key: &str) -> Result<(Vec<T>, usize), CookError> {
        let e = self.required(key)?;
        let Value::Array(items) = &e.value else {
            return Err(self.wrong(e, "an array of integers"));
        };
        let values: Option<Vec<T>> = items
            .iter()
            .map(|v| match v {
                Value::Int(i) => T::try_from(*i).ok(),
                _ => None,
            })
            .collect();
        let short = std::any::type_name::<T>();
        values.map(|v| (v, e.line)).ok_or_else(|| {
            self.err(
                e.line,
                &format!("`{key}` must hold only integers that fit in {short}"),
            )
        })
    }

    /// A required array of exactly `N` finite numbers.
    pub(crate) fn array<const N: usize>(&self, key: &str) -> Result<([f32; N], usize), CookError> {
        let (values, line) = self.f32s(key)?;
        let n = values.len();
        <[f32; N]>::try_from(values)
            .map(|a| (a, line))
            .map_err(|_| self.err(line, &format!("`{key}` must hold {N} numbers, not {n}")))
    }

    /// An optional array of exactly `N` finite numbers.
    pub(crate) fn opt_array<const N: usize>(
        &self,
        key: &str,
        default: [f32; N],
    ) -> Result<([f32; N], usize), CookError> {
        if self.has(key) {
            self.array(key)
        } else {
            Ok((default, self.line))
        }
    }

    /// A required array of strings.
    pub(crate) fn strs(&self, key: &str) -> Result<(Vec<&'d str>, usize), CookError> {
        let e = self.required(key)?;
        let Value::Array(items) = &e.value else {
            return Err(self.wrong(e, "an array of strings"));
        };
        let values: Option<Vec<&'d str>> = items
            .iter()
            .map(|v| match v {
                Value::Str(s) => Some(s.as_str()),
                _ => None,
            })
            .collect();
        values
            .map(|v| (v, e.line))
            .ok_or_else(|| self.err(e.line, &format!("`{key}` must hold only strings")))
    }

    /// An optional array of strings (empty when absent).
    pub(crate) fn opt_strs(&self, key: &str) -> Result<(Vec<&'d str>, usize), CookError> {
        if self.has(key) {
            self.strs(key)
        } else {
            Ok((Vec::new(), self.line))
        }
    }

    /// One of `options` by name; `default` when absent (required when `None`).
    pub(crate) fn choice<T: Copy>(
        &self,
        key: &str,
        options: &[(&str, T)],
        default: Option<T>,
    ) -> Result<(T, usize), CookError> {
        let (word, line) = match (self.opt_str(key)?, default) {
            (Some(found), _) => found,
            (None, Some(d)) => return Ok((d, self.line)),
            (None, None) => {
                return Err(self
                    .required(key)
                    .err()
                    .unwrap_or_else(|| self.err(self.line, key)));
            }
        };
        options
            .iter()
            .find(|(name, _)| *name == word)
            .map(|(_, v)| (*v, line))
            .ok_or_else(|| {
                let names: Vec<&str> = options.iter().map(|(n, _)| *n).collect();
                self.err(
                    line,
                    &format!("`{key}` = \"{word}\" is not one of: {}", names.join(", ")),
                )
            })
    }
}

/// The output name: `path` without `suffix` (which it ends with), plus `ext`.
pub(crate) fn output_name(path: &str, suffix: &str, ext: &str) -> String {
    format!("{}{ext}", path.strip_suffix(suffix).unwrap_or(path))
}

/// Whether `v` is within `lo..=hi`.
pub(crate) fn within(v: f32, lo: f32, hi: f32) -> bool {
    (lo..=hi).contains(&v)
}

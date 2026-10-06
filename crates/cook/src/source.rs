//! Typed, line-located reading of structured sources: the strict TOML subset of
//! `mantis_core::module::toml`. Every importer reads its sources through this one
//! reader ([`Doc`], [`Fields`]).
//!
//! Every typed read returns the value together with the line it came from (the key's
//! line, or the table header's line when an optional key is absent), so a later check can
//! place its error exactly. Unknown tables and keys are refused, so a misspelled field
//! fails the cook at its line instead of silently taking a default. Error texts are the
//! same for every importer:
//!
//! | case | text |
//! |---|---|
//! | absent required key | ``missing `key` in `[table]` `` (at the header line) |
//! | wrong type | `` `key` must be a string, not an integer `` |
//! | unknown key | ``unknown key `key` in `[table]` (expected one of: …)`` |
//! | unknown table | ``unknown table `[name]` (expected [a], [b.<name>])`` |

use std::collections::BTreeMap;

use mantis_core::module::toml::{self, Document, Entry, Table, Value};

use crate::importer::{CookError, Source};

/// A parsed structured source and the line of each table header.
pub struct Doc<'a> {
    path: &'a str,
    parsed: Document,
    headers: BTreeMap<String, usize>,
}

/// The line of each `[name]` header (quoted names unquoted).
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

impl<'a> Doc<'a> {
    /// Parses `text` read from `path`.
    ///
    /// # Errors
    /// [`CookError`] at the first malformed line.
    pub fn parse(path: &'a str, text: &str) -> Result<Doc<'a>, CookError> {
        let parsed = toml::parse(text).map_err(|e| CookError::at(path, e.line, e.what))?;
        Ok(Doc {
            path,
            parsed,
            headers: header_lines(text),
        })
    }

    /// Parses an importer's source.
    ///
    /// # Errors
    /// [`CookError`]: not UTF-8, or the first malformed line.
    pub fn from_source(source: &Source<'a>) -> Result<Doc<'a>, CookError> {
        Self::parse(source.path, source.text()?)
    }

    /// The source path.
    pub fn path(&self) -> &'a str {
        self.path
    }

    /// An error at `line` of this source.
    pub fn err(&self, line: usize, message: &str) -> CookError {
        CookError::at(self.path, line, message)
    }

    fn fields<'d>(&'d self, table: Option<&'d Table>) -> Fields<'d> {
        let name = table.map_or("", |t| t.name.as_str());
        let line = if name.is_empty() {
            0
        } else {
            self.headers
                .get(name)
                .copied()
                .or_else(|| {
                    table
                        .and_then(|t| t.entries.first())
                        .map(|e| e.line.saturating_sub(1))
                })
                .unwrap_or(0)
        };
        Fields {
            path: self.path,
            name,
            line,
            table,
        }
    }

    /// The root table (keys before the first header); empty when there are none.
    pub fn root(&self) -> Fields<'_> {
        self.fields(self.parsed.tables.iter().find(|t| t.name.is_empty()))
    }

    /// The table named exactly `name`.
    pub fn table(&self, name: &str) -> Option<Fields<'_>> {
        self.parsed.table(name).map(|t| self.fields(Some(t)))
    }

    /// The table named exactly `name`, required.
    ///
    /// # Errors
    /// [`CookError`] when it is missing.
    pub fn require(&self, name: &str) -> Result<Fields<'_>, CookError> {
        self.table(name)
            .ok_or_else(|| self.err(0, &format!("missing table `[{name}]`")))
    }

    /// Every named table, in file order.
    pub fn tables(&self) -> impl Iterator<Item = Fields<'_>> {
        self.parsed
            .tables
            .iter()
            .filter(|t| !t.name.is_empty())
            .map(|t| self.fields(Some(t)))
    }

    /// Every table named `<prefix>.<item>` with a non-empty item name (for
    /// `[placement.tree_01]`-style items), with the item name, in file order.
    pub fn items<'d>(&'d self, prefix: &'d str) -> impl Iterator<Item = (&'d str, Fields<'d>)> + 'd {
        self.tables().filter_map(move |f| {
            let rest = f.name.strip_prefix(prefix)?.strip_prefix('.')?;
            (!rest.is_empty()).then_some((rest, f))
        })
    }

    /// Refuses every named table that is not one of `exact` or `<prefix>.<item>` for one
    /// of `prefixes`.
    ///
    /// # Errors
    /// [`CookError`] at the unknown table's header, listing what is expected.
    pub fn only_tables(&self, exact: &[&str], prefixes: &[&str]) -> Result<(), CookError> {
        for t in self.tables() {
            let known = exact.contains(&t.name)
                || prefixes.iter().any(|p| {
                    t.name
                        .strip_prefix(p)
                        .and_then(|r| r.strip_prefix('.'))
                        .is_some_and(|r| !r.is_empty())
                });
            if !known {
                let mut expected: Vec<String> = exact.iter().map(|e| format!("[{e}]")).collect();
                expected.extend(prefixes.iter().map(|p| format!("[{p}.<name>]")));
                return Err(self.err(
                    t.line,
                    &format!("unknown table `[{}]` (expected {})", t.name, expected.join(", ")),
                ));
            }
        }
        Ok(())
    }

    /// Refuses keys outside any table (for sources whose every key belongs to a table).
    ///
    /// # Errors
    /// [`CookError`] at the first such key.
    pub fn no_root_keys(&self) -> Result<(), CookError> {
        match self.root().entries().first() {
            Some(e) => Err(self.err(e.line, "keys must be inside a table")),
            None => Ok(()),
        }
    }
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

/// A number as a finite `f32` (integers are accepted where floats are expected).
fn float_of(v: &Value) -> Option<f32> {
    let f = match v {
        Value::Float(s) => s.parse::<f32>().ok()?,
        #[expect(clippy::cast_precision_loss)] // Authoring values are small integers.
        Value::Int(i) => *i as f32,
        _ => return None,
    };
    f.is_finite().then_some(f)
}

/// One table's fields. Every typed read returns `(value, line)`.
#[derive(Clone, Copy)]
pub struct Fields<'d> {
    path: &'d str,
    name: &'d str,
    line: usize,
    table: Option<&'d Table>,
}

impl<'d> Fields<'d> {
    /// The table name (`""` for the root).
    pub fn name(&self) -> &'d str {
        self.name
    }

    /// The header line (0 for the root).
    pub fn line(&self) -> usize {
        self.line
    }

    /// An error at `line`.
    pub fn err(&self, line: usize, message: &str) -> CookError {
        CookError::at(self.path, line, message)
    }

    /// An error at `key`'s line (the header line when it is absent).
    pub fn error(&self, key: &str, message: &str) -> CookError {
        self.err(self.line_of(key), message)
    }

    fn entry(&self, key: &str) -> Option<&'d Entry> {
        self.table?.entries.iter().find(|e| e.key == key)
    }

    /// Whether `key` is present.
    pub fn has(&self, key: &str) -> bool {
        self.entry(key).is_some()
    }

    /// The line of `key`, or the header line when it is absent.
    pub fn line_of(&self, key: &str) -> usize {
        self.entry(key).map_or(self.line, |e| e.line)
    }

    /// Every entry, in file order.
    pub fn entries(&self) -> &'d [Entry] {
        self.table.map_or(&[], |t| t.entries.as_slice())
    }

    fn place(&self) -> String {
        if self.name.is_empty() {
            "the root table".to_owned()
        } else {
            format!("`[{}]`", self.name)
        }
    }

    /// Refuses keys not in `allowed`.
    ///
    /// # Errors
    /// [`CookError`] at the unknown key, listing the allowed ones.
    pub fn only(&self, allowed: &[&str]) -> Result<(), CookError> {
        match self.entries().iter().find(|e| !allowed.contains(&e.key.as_str())) {
            Some(e) => Err(self.err(
                e.line,
                &format!(
                    "unknown key `{}` in {} (expected one of: {})",
                    e.key,
                    self.place(),
                    allowed.join(", ")
                ),
            )),
            None => Ok(()),
        }
    }

    fn required(&self, key: &str) -> Result<&'d Entry, CookError> {
        self.entry(key)
            .ok_or_else(|| self.err(self.line, &format!("missing `{key}` in {}", self.place())))
    }

    fn wrong(&self, e: &Entry, expected: &str) -> CookError {
        self.err(
            e.line,
            &format!("`{}` must be {expected}, not {}", e.key, kind_of(&e.value)),
        )
    }

    /// Reads `key` with `read` when present.
    fn optional<T>(
        &self,
        key: &str,
        read: impl FnOnce(&Self, &str) -> Result<(T, usize), CookError>,
    ) -> Result<Option<(T, usize)>, CookError> {
        if self.has(key) {
            read(self, key).map(Some)
        } else {
            Ok(None)
        }
    }

    /// A required string.
    ///
    /// # Errors
    /// [`CookError`] when missing or not a string.
    pub fn str(&self, key: &str) -> Result<(&'d str, usize), CookError> {
        let e = self.required(key)?;
        match &e.value {
            Value::Str(s) => Ok((s, e.line)),
            _ => Err(self.wrong(e, "a string")),
        }
    }

    /// An optional string.
    ///
    /// # Errors
    /// [`CookError`] when present but not a string.
    pub fn opt_str(&self, key: &str) -> Result<Option<(&'d str, usize)>, CookError> {
        self.optional(key, Self::str)
    }

    /// A required finite number (integers accepted).
    ///
    /// # Errors
    /// [`CookError`] when missing or not a finite number.
    pub fn f32(&self, key: &str) -> Result<(f32, usize), CookError> {
        let e = self.required(key)?;
        float_of(&e.value)
            .map(|v| (v, e.line))
            .ok_or_else(|| self.wrong(e, "a finite number"))
    }

    /// An optional finite number.
    ///
    /// # Errors
    /// [`CookError`] when present but not a finite number.
    pub fn opt_f32(&self, key: &str) -> Result<Option<(f32, usize)>, CookError> {
        self.optional(key, Self::f32)
    }

    /// A finite number, `default` when absent.
    ///
    /// # Errors
    /// [`CookError`] when present but not a finite number.
    pub fn f32_or(&self, key: &str, default: f32) -> Result<(f32, usize), CookError> {
        Ok(self.opt_f32(key)?.unwrap_or((default, self.line)))
    }

    /// A required integer that fits in `T`.
    ///
    /// # Errors
    /// [`CookError`] when missing, not an integer, or out of `T`'s range.
    pub fn int<T: TryFrom<i64>>(&self, key: &str) -> Result<(T, usize), CookError> {
        let e = self.required(key)?;
        let Value::Int(i) = e.value else {
            return Err(self.wrong(e, "an integer"));
        };
        let short = std::any::type_name::<T>();
        T::try_from(i)
            .map(|v| (v, e.line))
            .map_err(|_| self.err(e.line, &format!("`{key}` = {i} does not fit in {short}")))
    }

    /// An integer that fits in `T`, `default` when absent.
    ///
    /// # Errors
    /// [`CookError`] when present but not an integer in range.
    pub fn int_or<T: TryFrom<i64>>(&self, key: &str, default: T) -> Result<(T, usize), CookError> {
        Ok(self.optional(key, Self::int)?.unwrap_or((default, self.line)))
    }

    /// A required integer in `lo..=hi`.
    ///
    /// # Errors
    /// [`CookError`] when missing, not an integer, or out of range.
    pub fn int_in(&self, key: &str, lo: i64, hi: i64) -> Result<(i64, usize), CookError> {
        let e = self.required(key)?;
        match e.value {
            Value::Int(i) if (lo..=hi).contains(&i) => Ok((i, e.line)),
            _ => Err(self.err(e.line, &format!("`{key}` must be an integer from {lo} to {hi}"))),
        }
    }

    /// A required boolean.
    ///
    /// # Errors
    /// [`CookError`] when missing or not a boolean.
    pub fn bool(&self, key: &str) -> Result<(bool, usize), CookError> {
        let e = self.required(key)?;
        match e.value {
            Value::Bool(b) => Ok((b, e.line)),
            _ => Err(self.wrong(e, "true or false")),
        }
    }

    /// An optional boolean.
    ///
    /// # Errors
    /// [`CookError`] when present but not a boolean.
    pub fn opt_bool(&self, key: &str) -> Result<Option<(bool, usize)>, CookError> {
        self.optional(key, Self::bool)
    }

    /// A boolean, `default` when absent.
    ///
    /// # Errors
    /// [`CookError`] when present but not a boolean.
    pub fn bool_or(&self, key: &str, default: bool) -> Result<(bool, usize), CookError> {
        Ok(self.opt_bool(key)?.unwrap_or((default, self.line)))
    }

    /// A required array of finite numbers, any length.
    ///
    /// # Errors
    /// [`CookError`] when missing, not an array, or holding anything but finite numbers.
    pub fn f32s(&self, key: &str) -> Result<(Vec<f32>, usize), CookError> {
        let e = self.required(key)?;
        let Value::Array(items) = &e.value else {
            return Err(self.wrong(e, "an array of numbers"));
        };
        items
            .iter()
            .map(float_of)
            .collect::<Option<Vec<f32>>>()
            .map(|v| (v, e.line))
            .ok_or_else(|| self.err(e.line, &format!("`{key}` must hold only finite numbers")))
    }

    /// A required array of exactly `N` finite numbers.
    ///
    /// # Errors
    /// [`CookError`] as [`Fields::f32s`], or another length.
    pub fn array<const N: usize>(&self, key: &str) -> Result<([f32; N], usize), CookError> {
        let (values, line) = self.f32s(key)?;
        let n = values.len();
        <[f32; N]>::try_from(values)
            .map(|a| (a, line))
            .map_err(|_| self.err(line, &format!("`{key}` must hold {N} numbers, not {n}")))
    }

    /// An optional array of exactly `N` finite numbers.
    ///
    /// # Errors
    /// [`CookError`] when present but not `N` finite numbers.
    pub fn opt_array<const N: usize>(&self, key: &str) -> Result<Option<([f32; N], usize)>, CookError> {
        self.optional(key, Self::array::<N>)
    }

    /// An array of exactly `N` finite numbers, `default` when absent.
    ///
    /// # Errors
    /// [`CookError`] when present but not `N` finite numbers.
    pub fn array_or<const N: usize>(
        &self,
        key: &str,
        default: [f32; N],
    ) -> Result<([f32; N], usize), CookError> {
        Ok(self.opt_array(key)?.unwrap_or((default, self.line)))
    }

    /// A required array of integers, each fitting in `T`.
    ///
    /// # Errors
    /// [`CookError`] when missing, not an array, or holding anything but such integers.
    pub fn ints<T: TryFrom<i64>>(&self, key: &str) -> Result<(Vec<T>, usize), CookError> {
        let e = self.required(key)?;
        let Value::Array(items) = &e.value else {
            return Err(self.wrong(e, "an array of integers"));
        };
        let short = std::any::type_name::<T>();
        items
            .iter()
            .map(|v| match v {
                Value::Int(i) => T::try_from(*i).ok(),
                _ => None,
            })
            .collect::<Option<Vec<T>>>()
            .map(|v| (v, e.line))
            .ok_or_else(|| {
                self.err(
                    e.line,
                    &format!("`{key}` must hold only integers that fit in {short}"),
                )
            })
    }

    /// A required array of strings.
    ///
    /// # Errors
    /// [`CookError`] when missing, not an array, or holding anything but strings.
    pub fn strs(&self, key: &str) -> Result<(Vec<&'d str>, usize), CookError> {
        let e = self.required(key)?;
        let Value::Array(items) = &e.value else {
            return Err(self.wrong(e, "an array of strings"));
        };
        items
            .iter()
            .map(|v| match v {
                Value::Str(s) => Some(s.as_str()),
                _ => None,
            })
            .collect::<Option<Vec<&'d str>>>()
            .map(|v| (v, e.line))
            .ok_or_else(|| self.err(e.line, &format!("`{key}` must hold only strings")))
    }

    /// An array of strings, empty when absent.
    ///
    /// # Errors
    /// [`CookError`] when present but not an array of strings.
    pub fn strs_or_empty(&self, key: &str) -> Result<(Vec<&'d str>, usize), CookError> {
        Ok(self.optional(key, Self::strs)?.unwrap_or((Vec::new(), self.line)))
    }

    /// One of `options` by name; `default` when absent (required when `None`).
    ///
    /// # Errors
    /// [`CookError`] when missing without a default, not a string, or not an option.
    pub fn choice<T: Copy>(
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

/// The output name: `path` without `suffix` (which it ends with), plus `ext`
/// (`a/b.clip.toml` to `a/b.clip`).
pub fn output_name(path: &str, suffix: &str, ext: &str) -> String {
    format!("{}{ext}", path.strip_suffix(suffix).unwrap_or(path))
}

/// Whether the file name of `path` ends with `suffix` and has a non-empty stem.
pub fn has_suffix(path: &str, suffix: &str) -> bool {
    let name = path.rsplit('/').next().unwrap_or(path);
    name.len() > suffix.len() && name.ends_with(suffix)
}

/// Whether `v` is within `lo..=hi`.
pub fn within(v: f32, lo: f32, hi: f32) -> bool {
    (lo..=hi).contains(&v)
}

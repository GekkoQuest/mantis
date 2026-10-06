//! Typed reading of structured sources (the strict TOML subset of
//! `mantis_core::module::toml`) with errors located at the source line.

use mantis_core::module::toml::{Document, Table, Value};

use crate::importer::CookError;

/// A parsed structured source.
pub struct Doc<'a> {
    /// The source path (for errors).
    pub path: &'a str,
    /// The document.
    pub doc: Document,
}

impl<'a> Doc<'a> {
    /// Parses `text` from `path`.
    ///
    /// # Errors
    /// [`CookError`] at the first malformed line.
    pub fn parse(path: &'a str, text: &str) -> Result<Doc<'a>, CookError> {
        let doc = mantis_core::module::toml::parse(text).map_err(|e| CookError::at(path, e.line, e.what))?;
        Ok(Doc { path, doc })
    }

    /// The table named `name`.
    pub fn table(&self, name: &str) -> Option<Fields<'_>> {
        self.doc.table(name).map(|t| Fields {
            path: self.path,
            table: t,
        })
    }

    /// The table named `name`, required.
    ///
    /// # Errors
    /// [`CookError`] when it is missing.
    pub fn require(&self, name: &str) -> Result<Fields<'_>, CookError> {
        self.table(name)
            .ok_or_else(|| CookError::at(self.path, 0, &format!("missing table [{name}]")))
    }

    /// Every table whose name starts with `prefix.` (for `[placement.tree_01]`-style
    /// items), with the item name after the prefix, in file order.
    pub fn items(&self, prefix: &str) -> Vec<(&str, Fields<'_>)> {
        self.doc
            .tables
            .iter()
            .filter_map(|t| {
                let rest = t.name.strip_prefix(prefix)?.strip_prefix('.')?;
                (!rest.is_empty()).then_some((
                    rest,
                    Fields {
                        path: self.path,
                        table: t,
                    },
                ))
            })
            .collect()
    }

    /// Fails on any table not in `known` (exact names) or `known_prefixes` (item tables).
    ///
    /// # Errors
    /// [`CookError`] naming the unknown table.
    pub fn only_tables(&self, known: &[&str], known_prefixes: &[&str]) -> Result<(), CookError> {
        for t in &self.doc.tables {
            let ok = t.name.is_empty()
                || known.contains(&t.name.as_str())
                || known_prefixes
                    .iter()
                    .any(|p| t.name.strip_prefix(p).is_some_and(|r| r.starts_with('.')));
            if !ok {
                let line = t.entries.first().map_or(0, |e| e.line.saturating_sub(1));
                return Err(CookError::at(
                    self.path,
                    line,
                    &format!("unknown table [{}]", t.name),
                ));
            }
        }
        if let Some(e) = self.doc.tables.first().and_then(|t| t.entries.first()) {
            return Err(CookError::at(self.path, e.line, "keys must be inside a table"));
        }
        Ok(())
    }
}

/// One table's fields.
#[derive(Clone, Copy)]
pub struct Fields<'a> {
    path: &'a str,
    table: &'a Table,
}

/// A number as a finite `f32` (integers are accepted where floats are expected): the one
/// number reading every structured source shares.
pub(crate) fn float_of(v: &Value) -> Option<f32> {
    let f = match v {
        Value::Float(s) => s.parse::<f32>().ok()?,
        #[allow(clippy::cast_precision_loss)] // Authoring values are small integers.
        Value::Int(i) => *i as f32,
        _ => return None,
    };
    f.is_finite().then_some(f)
}

impl<'a> Fields<'a> {
    /// The line of `key`, or the table's first line.
    pub fn line(&self, key: &str) -> usize {
        self.table
            .entries
            .iter()
            .find(|e| e.key == key)
            .or(self.table.entries.first())
            .map_or(0, |e| e.line)
    }

    /// An error at `key`'s line.
    pub fn error(&self, key: &str, message: &str) -> CookError {
        CookError::at(self.path, self.line(key), message)
    }

    /// Fails on keys not in `known`.
    ///
    /// # Errors
    /// [`CookError`] at the unknown key.
    pub fn only(&self, known: &[&str]) -> Result<(), CookError> {
        match self
            .table
            .entries
            .iter()
            .find(|e| !known.contains(&e.key.as_str()))
        {
            Some(e) => Err(CookError::at(
                self.path,
                e.line,
                &format!("unknown key `{}`", e.key),
            )),
            None => Ok(()),
        }
    }

    /// Whether `key` is present.
    pub fn has(&self, key: &str) -> bool {
        self.table.get(key).is_some()
    }

    /// A required finite float.
    ///
    /// # Errors
    /// [`CookError`] when missing or not a finite number.
    pub fn float(&self, key: &str) -> Result<f32, CookError> {
        let v = self
            .table
            .get(key)
            .ok_or_else(|| self.error(key, &format!("missing `{key}`")))?;
        float_of(v).ok_or_else(|| self.error(key, &format!("`{key}` must be a finite number")))
    }

    /// An optional finite float with a default.
    ///
    /// # Errors
    /// [`CookError`] when present but not a finite number.
    pub fn float_or(&self, key: &str, default: f32) -> Result<f32, CookError> {
        if self.has(key) {
            self.float(key)
        } else {
            Ok(default)
        }
    }

    /// A required integer in `lo..=hi`.
    ///
    /// # Errors
    /// [`CookError`] when missing, not an integer, or out of range.
    pub fn int(&self, key: &str, lo: i64, hi: i64) -> Result<i64, CookError> {
        match self.table.get(key) {
            Some(Value::Int(i)) if (lo..=hi).contains(i) => Ok(*i),
            Some(_) => Err(self.error(key, &format!("`{key}` must be an integer from {lo} to {hi}"))),
            None => Err(self.error(key, &format!("missing `{key}`"))),
        }
    }

    /// An optional boolean with a default.
    ///
    /// # Errors
    /// [`CookError`] when present but not a boolean.
    pub fn bool_or(&self, key: &str, default: bool) -> Result<bool, CookError> {
        match self.table.get(key) {
            Some(v) => v
                .as_bool()
                .ok_or_else(|| self.error(key, &format!("`{key}` must be true or false"))),
            None => Ok(default),
        }
    }

    /// A required string.
    ///
    /// # Errors
    /// [`CookError`] when missing or not a string.
    pub fn str(&self, key: &str) -> Result<&'a str, CookError> {
        match self.table.get(key) {
            Some(Value::Str(s)) => Ok(s.as_str()),
            Some(_) => Err(self.error(key, &format!("`{key}` must be a string"))),
            None => Err(self.error(key, &format!("missing `{key}`"))),
        }
    }

    /// An optional string.
    ///
    /// # Errors
    /// [`CookError`] when present but not a string.
    pub fn str_opt(&self, key: &str) -> Result<Option<&'a str>, CookError> {
        if self.has(key) {
            self.str(key).map(Some)
        } else {
            Ok(None)
        }
    }

    /// An optional array of strings (empty when absent).
    ///
    /// # Errors
    /// [`CookError`] when present but not an array of strings.
    pub fn strs_or_empty(&self, key: &str) -> Result<Vec<&'a str>, CookError> {
        match self.table.get(key) {
            None => Ok(Vec::new()),
            Some(Value::Array(items)) => items
                .iter()
                .map(|v| match v {
                    Value::Str(s) => Ok(s.as_str()),
                    _ => Err(self.error(key, &format!("`{key}` must hold strings"))),
                })
                .collect(),
            Some(_) => Err(self.error(key, &format!("`{key}` must be an array of strings"))),
        }
    }

    /// A required array of exactly `N` finite floats.
    ///
    /// # Errors
    /// [`CookError`] when missing, of another length, or not numbers.
    pub fn floats<const N: usize>(&self, key: &str) -> Result<[f32; N], CookError> {
        let v = self.floats_any(key)?;
        <[f32; N]>::try_from(v.as_slice())
            .map_err(|_| self.error(key, &format!("`{key}` must have {N} numbers")))
    }

    /// A required array of finite floats, any length.
    ///
    /// # Errors
    /// [`CookError`] when missing or not numbers.
    pub fn floats_any(&self, key: &str) -> Result<Vec<f32>, CookError> {
        match self.table.get(key) {
            Some(Value::Array(items)) => items
                .iter()
                .map(|v| {
                    float_of(v).ok_or_else(|| self.error(key, &format!("`{key}` must hold finite numbers")))
                })
                .collect(),
            Some(_) => Err(self.error(key, &format!("`{key}` must be an array of numbers"))),
            None => Err(self.error(key, &format!("missing `{key}`"))),
        }
    }
}

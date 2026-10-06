//! A strict, typed reader over one table of `mantis_core`'s TOML subset.
//!
//! Every key read is marked; [`Fields::finish`] refuses any key that was
//! not, so an unknown or misspelt key is an error with its line number,
//! never ignored. Values with a unit carry it in the key's suffix (`_ms`,
//! `_s`, `_ticks`); the accessors for durations check the suffix, so a
//! duration key without its unit cannot be declared.

use std::collections::BTreeSet;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::time::Duration;

use mantis_core::module::toml::{Table, Value};

/// A configuration or registry error, with where it is.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct FieldError(pub String);

impl std::fmt::Display for FieldError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for FieldError {}

impl From<FieldError> for String {
    fn from(e: FieldError) -> Self {
        e.0
    }
}

/// One table being read.
pub struct Fields<'a> {
    file: &'a str,
    table: &'a Table,
    read: BTreeSet<&'a str>,
}

impl<'a> Fields<'a> {
    /// Reads `table` of `file` (the file name is for messages).
    #[must_use]
    pub fn new(file: &'a str, table: &'a Table) -> Self {
        Self {
            file,
            table,
            read: BTreeSet::new(),
        }
    }

    fn name(&self) -> String {
        if self.table.name.is_empty() {
            "the top level".to_owned()
        } else {
            format!("[{}]", self.table.name)
        }
    }

    /// An error about `key`, with its line when present.
    #[must_use]
    pub fn error(&self, key: &str, what: &str) -> FieldError {
        match self.table.entries.iter().find(|e| e.key == key) {
            Some(e) => FieldError(format!(
                "{} line {}: {} `{key}`: {what}",
                self.file,
                e.line,
                self.name()
            )),
            None => FieldError(format!("{}: {} `{key}`: {what}", self.file, self.name())),
        }
    }

    fn take(&mut self, key: &'a str) -> Option<&'a Value> {
        self.read.insert(key);
        self.table.get(key)
    }

    fn need(&mut self, key: &'a str) -> Result<&'a Value, FieldError> {
        self.take(key)
            .ok_or_else(|| self.error(key, "missing (required)"))
    }

    /// A required string.
    ///
    /// # Errors
    /// Missing or not a string.
    pub fn str(&mut self, key: &'a str) -> Result<&'a str, FieldError> {
        self.need(key)?
            .as_str()
            .ok_or_else(|| self.error(key, "expected a string"))
    }

    /// An optional string.
    ///
    /// # Errors
    /// Present and not a string.
    pub fn opt_str(&mut self, key: &'a str) -> Result<Option<&'a str>, FieldError> {
        match self.take(key) {
            None => Ok(None),
            Some(v) => v
                .as_str()
                .map(Some)
                .ok_or_else(|| self.error(key, "expected a string")),
        }
    }

    /// A required integer within `range`.
    ///
    /// # Errors
    /// Missing, not an integer, or out of range.
    pub fn int(&mut self, key: &'a str, range: std::ops::RangeInclusive<i64>) -> Result<i64, FieldError> {
        match self.need(key)? {
            Value::Int(i) if range.contains(i) => Ok(*i),
            Value::Int(i) => Err(self.error(
                key,
                &format!("{i} is outside {}..={}", range.start(), range.end()),
            )),
            _ => Err(self.error(key, "expected an integer")),
        }
    }

    /// An optional integer within `range`, `default` when absent.
    ///
    /// # Errors
    /// Present and not an integer in range.
    pub fn int_or(
        &mut self,
        key: &'a str,
        range: std::ops::RangeInclusive<i64>,
        default: i64,
    ) -> Result<i64, FieldError> {
        if self.table.get(key).is_none() {
            self.read.insert(key);
            return Ok(default);
        }
        self.int(key, range)
    }

    /// A required unsigned integer within `range`.
    ///
    /// # Errors
    /// [`Fields::int`].
    pub fn uint(&mut self, key: &'a str, range: std::ops::RangeInclusive<u64>) -> Result<u64, FieldError> {
        let lo = i64::try_from(*range.start()).unwrap_or(i64::MAX);
        let hi = i64::try_from(*range.end()).unwrap_or(i64::MAX);
        let v = self.int(key, lo..=hi)?;
        u64::try_from(v).map_err(|_| self.error(key, "expected a non-negative integer"))
    }

    /// An optional boolean, `default` when absent.
    ///
    /// # Errors
    /// Present and not a boolean.
    pub fn bool_or(&mut self, key: &'a str, default: bool) -> Result<bool, FieldError> {
        match self.take(key) {
            None => Ok(default),
            Some(v) => v
                .as_bool()
                .ok_or_else(|| self.error(key, "expected true or false")),
        }
    }

    /// A required socket address (`ip:port`; names are not resolved).
    ///
    /// # Errors
    /// Missing or not an address.
    pub fn addr(&mut self, key: &'a str) -> Result<SocketAddr, FieldError> {
        let s = self.str(key)?;
        s.parse()
            .map_err(|_| self.error(key, &format!("{s:?} is not an ip:port address")))
    }

    /// A required path, relative to `base` unless absolute.
    ///
    /// # Errors
    /// Missing, not a string, or empty.
    pub fn path(&mut self, key: &'a str, base: &Path) -> Result<PathBuf, FieldError> {
        let s = self.str(key)?;
        if s.is_empty() {
            return Err(self.error(key, "an empty path"));
        }
        Ok(base.join(s))
    }

    /// An optional path, relative to `base` unless absolute.
    ///
    /// # Errors
    /// Present and not a non-empty string.
    pub fn opt_path(&mut self, key: &'a str, base: &Path) -> Result<Option<PathBuf>, FieldError> {
        if self.table.get(key).is_none() {
            self.read.insert(key);
            return Ok(None);
        }
        self.path(key, base).map(Some)
    }

    /// A duration whose key ends in `_ms` or `_s` (the unit), within
    /// `range` of that unit, `default` (in the same unit) when absent.
    ///
    /// # Errors
    /// The key names no unit (a programming error, reported as such),
    /// or the value is not an integer in range.
    pub fn duration_or(
        &mut self,
        key: &'a str,
        range: std::ops::RangeInclusive<u64>,
        default: u64,
    ) -> Result<Duration, FieldError> {
        let unit: fn(u64) -> Duration = if key.ends_with("_ms") {
            Duration::from_millis
        } else if key.ends_with("_s") {
            Duration::from_secs
        } else {
            return Err(self.error(key, "a duration key names its unit (_ms or _s)"));
        };
        if self.table.get(key).is_none() {
            self.read.insert(key);
            return Ok(unit(default));
        }
        self.uint(key, range).map(unit)
    }

    /// A required array of strings.
    ///
    /// # Errors
    /// Missing, or not an array of strings.
    pub fn strs(&mut self, key: &'a str) -> Result<Vec<String>, FieldError> {
        self.need(key)?
            .as_strings()
            .ok_or_else(|| self.error(key, "expected an array of strings"))
    }

    /// A required array of unsigned integers.
    ///
    /// # Errors
    /// Missing, or not an array of non-negative integers.
    pub fn uints(&mut self, key: &'a str) -> Result<Vec<u64>, FieldError> {
        match self.need(key)? {
            Value::Array(items) => items
                .iter()
                .map(|v| match v {
                    Value::Int(i) => u64::try_from(*i).map_err(|_| self.error(key, "negative")),
                    _ => Err(self.error(key, "expected integers")),
                })
                .collect(),
            _ => Err(self.error(key, "expected an array of integers")),
        }
    }

    /// Refuses every key that was not read.
    ///
    /// # Errors
    /// The first unknown key.
    pub fn finish(self) -> Result<(), FieldError> {
        match self
            .table
            .entries
            .iter()
            .find(|e| !self.read.contains(e.key.as_str()))
        {
            Some(e) => Err(FieldError(format!(
                "{} line {}: {} has no key `{}`",
                self.file,
                e.line,
                self.name(),
                e.key
            ))),
            None => Ok(()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use mantis_core::module::toml::parse;

    #[test]
    fn unknown_keys_missing_keys_and_units_are_refused() {
        let doc = parse("a = 1\nwait_ms = 5\nb = \"x\"\n").unwrap();
        let t = doc.table("").unwrap();
        let mut f = Fields::new("t.toml", t);
        assert_eq!(f.int("a", 0..=9).unwrap(), 1);
        assert_eq!(
            f.duration_or("wait_ms", 1..=10, 3).unwrap(),
            Duration::from_millis(5)
        );
        let e = f.finish().unwrap_err();
        assert_eq!(e.0, "t.toml line 3: the top level has no key `b`");

        let mut f = Fields::new("t.toml", t);
        assert!(f.str("c").unwrap_err().0.contains("missing"));
        assert!(f.int("a", 2..=9).unwrap_err().0.contains("outside 2..=9"));
        assert!(
            f.duration_or("wait", 1..=10, 3)
                .unwrap_err()
                .0
                .contains("names its unit")
        );
        assert!(f.str("a").unwrap_err().0.contains("expected a string"));
    }
}

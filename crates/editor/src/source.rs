//! Format-preserving edits of structured content sources (the strict TOML subset of
//! `mantis_core::module::toml` that every cook source uses).
//!
//! The editor writes back to the files people author, so an edit changes only the lines
//! it must: setting a key rewrites that key's line (keeping its trailing comment) or
//! inserts one line after the table's last entry; removing a table removes its header,
//! its entries, and the comment lines directly above the header. Every other byte,
//! comments and blank lines included, is kept. After every edit the text is parsed again
//! and an edit that would leave it unparseable is refused and undone.

use core::fmt;

use mantis_core::module::toml::{self, Document, Value};

/// Why an edit was refused.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum EditError {
    /// The source does not parse (the line and what is wrong).
    Parse(usize, &'static str),
    /// No such table.
    NoTable(String),
    /// The table already exists.
    TableExists(String),
    /// A name or key the subset cannot hold (empty, or with characters it does not allow).
    BadName(String),
}

impl fmt::Display for EditError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            EditError::Parse(line, what) => write!(f, "line {line}: {what}"),
            EditError::NoTable(t) => write!(f, "no table `[{t}]`"),
            EditError::TableExists(t) => write!(f, "`[{t}]` already exists"),
            EditError::BadName(n) => write!(f, "`{n}` is not a valid name"),
        }
    }
}

impl std::error::Error for EditError {}

/// Formats a value as the subset writes it.
pub fn format_value(v: &Value) -> String {
    match v {
        Value::Str(s) => {
            let mut out = String::with_capacity(s.len() + 2);
            out.push('"');
            for c in s.chars() {
                match c {
                    '"' => out.push_str("\\\""),
                    '\\' => out.push_str("\\\\"),
                    '\n' => out.push_str("\\n"),
                    '\t' => out.push_str("\\t"),
                    c => out.push(c),
                }
            }
            out.push('"');
            out
        }
        Value::Int(i) => i.to_string(),
        Value::Float(f) => f.clone(),
        Value::Bool(b) => b.to_string(),
        Value::Array(items) => {
            let parts: Vec<String> = items.iter().map(format_value).collect();
            format!("[{}]", parts.join(", "))
        }
    }
}

/// A float as the subset writes it: the shortest text that reads back to the same `f32`,
/// always with a decimal point.
pub fn float(v: f32) -> Value {
    let mut s = format!("{v}");
    if !s.contains('.') && !s.contains('e') && !s.contains("inf") && !s.contains("NaN") {
        s.push_str(".0");
    }
    Value::Float(s)
}

/// An array of floats.
pub fn floats(values: &[f32]) -> Value {
    Value::Array(values.iter().map(|v| float(*v)).collect())
}

/// A string value.
pub fn text(s: &str) -> Value {
    Value::Str(s.to_owned())
}

fn valid_name(name: &str) -> bool {
    !name.is_empty()
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.'))
}

/// Where a table sits in the text.
struct Span {
    /// The header line index (`None` for the root).
    header: Option<usize>,
    /// The first line after the table (the next header, or the end).
    end: usize,
}

/// A source text being edited.
#[derive(Clone, Debug)]
pub struct SourceDoc {
    lines: Vec<String>,
    trailing_newline: bool,
    parsed: Document,
}

fn header_name(line: &str) -> Option<&str> {
    let t = line.trim();
    t.strip_prefix('[')?.split(']').next().map(str::trim)
}

/// The byte offset of a `#` comment outside a string, if any.
fn comment_start(line: &str) -> Option<usize> {
    let mut in_string = false;
    let mut escaped = false;
    for (i, c) in line.char_indices() {
        match c {
            '\\' if in_string && !escaped => {
                escaped = true;
                continue;
            }
            '"' if !escaped => in_string = !in_string,
            '#' if !in_string => return Some(i),
            _ => {}
        }
        escaped = false;
    }
    None
}

fn key_of(line: &str) -> Option<&str> {
    let code = &line[..comment_start(line).unwrap_or(line.len())];
    let (key, _) = code.split_once('=')?;
    let key = key.trim();
    Some(
        key.strip_prefix('"')
            .and_then(|k| k.strip_suffix('"'))
            .unwrap_or(key),
    )
}

impl SourceDoc {
    /// Parses `text`.
    ///
    /// # Errors
    /// [`EditError::Parse`] when it is not the strict subset.
    pub fn parse(text: &str) -> Result<Self, EditError> {
        let parsed = toml::parse(text).map_err(|e| EditError::Parse(e.line, e.what))?;
        Ok(Self {
            lines: text.lines().map(str::to_owned).collect(),
            trailing_newline: text.ends_with('\n') || text.is_empty(),
            parsed,
        })
    }

    /// The text.
    pub fn text(&self) -> String {
        let mut out = self.lines.join("\n");
        if self.trailing_newline {
            out.push('\n');
        }
        out
    }

    /// The parsed document (always current).
    pub fn document(&self) -> &Document {
        &self.parsed
    }

    /// The value of `key` in `table` (`""` is the root).
    pub fn get(&self, table: &str, key: &str) -> Option<&Value> {
        self.parsed.table(table).and_then(|t| t.get(key))
    }

    /// Names of the tables that start with `prefix` followed by a non-empty rest, in file
    /// order, with that rest (`"placement."` gives each placement's name).
    pub fn tables_under(&self, prefix: &str) -> Vec<String> {
        self.parsed
            .tables
            .iter()
            .filter_map(|t| {
                t.name
                    .strip_prefix(prefix)
                    .filter(|r| !r.is_empty())
                    .map(str::to_owned)
            })
            .collect()
    }

    fn span(&self, table: &str) -> Option<Span> {
        let header = if table.is_empty() {
            None
        } else {
            Some(self.lines.iter().position(|l| header_name(l) == Some(table))?)
        };
        let start = header.map_or(0, |h| h + 1);
        let end = self
            .lines
            .iter()
            .enumerate()
            .skip(start)
            .find(|(_, l)| header_name(l).is_some())
            .map_or(self.lines.len(), |(i, _)| i);
        Some(Span { header, end })
    }

    /// Applies `edit` to the lines and reparses; a text that no longer parses is restored.
    fn commit(&mut self, edit: impl FnOnce(&mut Vec<String>)) -> Result<(), EditError> {
        let before = self.lines.clone();
        edit(&mut self.lines);
        match toml::parse(&self.text()) {
            Ok(parsed) => {
                self.parsed = parsed;
                Ok(())
            }
            Err(e) => {
                self.lines = before;
                Err(EditError::Parse(e.line, e.what))
            }
        }
    }

    /// Sets `key` in `table` to `value`: the key's line is rewritten in place (its
    /// trailing comment kept), or a new line goes after the table's last entry.
    ///
    /// # Errors
    /// [`EditError::NoTable`], [`EditError::BadName`], or [`EditError::Parse`].
    pub fn set(&mut self, table: &str, key: &str, value: &Value) -> Result<(), EditError> {
        if !valid_name(key) {
            return Err(EditError::BadName(key.to_owned()));
        }
        let span = self
            .span(table)
            .ok_or_else(|| EditError::NoTable(table.to_owned()))?;
        let start = span.header.map_or(0, |h| h + 1);
        let formatted = format!("{key} = {}", format_value(value));
        let existing = (start..span.end).find(|i| {
            self.lines
                .get(*i)
                .is_some_and(|l| header_name(l).is_none() && key_of(l) == Some(key))
        });
        self.commit(|lines| {
            if let Some(i) = existing {
                if let Some(line) = lines.get_mut(i) {
                    let indent: String = line.chars().take_while(|c| c.is_whitespace()).collect();
                    let comment = comment_start(line).map(|c| line[c..].to_owned());
                    *line = match comment {
                        Some(c) => format!("{indent}{formatted}  {c}"),
                        None => format!("{indent}{formatted}"),
                    };
                }
            } else {
                // Right after the table's last entry (its trailing blank lines and the
                // next table's leading comments stay where they are), or after the header.
                let at = (start..span.end)
                    .rev()
                    .find(|i| lines.get(*i).is_some_and(|l| key_of(l).is_some()))
                    .map_or(start, |i| i + 1);
                lines.insert(at, formatted);
            }
        })
    }

    /// Removes `key` from `table`; true when it was there.
    ///
    /// # Errors
    /// [`EditError::NoTable`] or [`EditError::Parse`].
    pub fn remove(&mut self, table: &str, key: &str) -> Result<bool, EditError> {
        let span = self
            .span(table)
            .ok_or_else(|| EditError::NoTable(table.to_owned()))?;
        let start = span.header.map_or(0, |h| h + 1);
        let Some(i) = (start..span.end).find(|i| {
            self.lines
                .get(*i)
                .is_some_and(|l| header_name(l).is_none() && key_of(l) == Some(key))
        }) else {
            return Ok(false);
        };
        self.commit(|lines| {
            lines.remove(i);
        })?;
        Ok(true)
    }

    /// Appends `[name]` with `entries` at the end of the file, after a blank line.
    ///
    /// # Errors
    /// [`EditError::TableExists`], [`EditError::BadName`], or [`EditError::Parse`].
    pub fn add_table(&mut self, name: &str, entries: &[(&str, Value)]) -> Result<(), EditError> {
        if !valid_name(name) || entries.iter().any(|(k, _)| !valid_name(k)) {
            return Err(EditError::BadName(name.to_owned()));
        }
        if self.parsed.table(name).is_some() {
            return Err(EditError::TableExists(name.to_owned()));
        }
        self.commit(|lines| {
            while lines.last().is_some_and(|l| l.trim().is_empty()) {
                lines.pop();
            }
            if !lines.is_empty() {
                lines.push(String::new());
            }
            lines.push(format!("[{name}]"));
            for (k, v) in entries {
                lines.push(format!("{k} = {}", format_value(v)));
            }
        })?;
        self.trailing_newline = true;
        Ok(())
    }

    /// Removes `[name]`, its entries and trailing blank lines, and the comment lines
    /// directly above its header. The blank line that separated it from the table before
    /// stays, so the spacing of the rest is unchanged.
    ///
    /// # Errors
    /// [`EditError::NoTable`] or [`EditError::Parse`].
    pub fn remove_table(&mut self, name: &str) -> Result<(), EditError> {
        let span = self
            .span(name)
            .ok_or_else(|| EditError::NoTable(name.to_owned()))?;
        let Some(header) = span.header else {
            return Err(EditError::NoTable(name.to_owned()));
        };
        let mut from = header;
        while from > 0
            && self
                .lines
                .get(from - 1)
                .is_some_and(|l| l.trim_start().starts_with('#'))
        {
            from -= 1;
        }
        let mut end = span.end;
        // Keep the comment block that belongs to the next table.
        while end > header + 1
            && self
                .lines
                .get(end - 1)
                .is_some_and(|l| l.trim_start().starts_with('#'))
        {
            end -= 1;
        }
        let to_end = span.end == self.lines.len();
        self.commit(|lines| {
            lines.drain(from..end);
            if to_end {
                while lines.last().is_some_and(|l| l.trim().is_empty()) {
                    lines.pop();
                }
            }
        })
    }
}

#[cfg(test)]
mod tests;

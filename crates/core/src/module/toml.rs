//! A strict reader for the small TOML subset that module manifests and
//! package manifests use: `[table]` headers, `key = value` lines, values that
//! are strings, integers, booleans, or single-line arrays of those, and `#`
//! comments. Keys may be bare or quoted; a dotted bare key is one literal
//! key, not a nested table. Anything else is refused with its line number.

use core::fmt;

/// A value.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Value {
    /// A string.
    Str(String),
    /// An integer.
    Int(i64),
    /// A finite float, kept as written (readers parse it at the precision
    /// they need).
    Float(String),
    /// A boolean.
    Bool(bool),
    /// A single-line array.
    Array(Vec<Value>),
}

impl Value {
    /// The string, if this is one.
    #[must_use]
    pub fn as_str(&self) -> Option<&str> {
        match self {
            Self::Str(s) => Some(s),
            _ => None,
        }
    }

    /// The boolean, if this is one.
    #[must_use]
    pub fn as_bool(&self) -> Option<bool> {
        match self {
            Self::Bool(b) => Some(*b),
            _ => None,
        }
    }

    /// The strings of an array of strings.
    #[must_use]
    pub fn as_strings(&self) -> Option<Vec<String>> {
        match self {
            Self::Array(items) => items.iter().map(|v| v.as_str().map(str::to_owned)).collect(),
            _ => None,
        }
    }
}

/// One `key = value` line.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Entry {
    /// The key.
    pub key: String,
    /// The value.
    pub value: Value,
    /// 1-based line number.
    pub line: usize,
}

/// One table: the root (named `""`) or a `[name]` section.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Table {
    /// Its name.
    pub name: String,
    /// Its entries in file order.
    pub entries: Vec<Entry>,
}

impl Table {
    /// The value of `key`.
    #[must_use]
    pub fn get(&self, key: &str) -> Option<&Value> {
        self.entries.iter().find(|e| e.key == key).map(|e| &e.value)
    }
}

/// A parsed document.
#[derive(Clone, PartialEq, Eq, Debug, Default)]
pub struct Document {
    /// Tables in file order, the root first.
    pub tables: Vec<Table>,
}

impl Document {
    /// The table called `name` (`""` is the root).
    #[must_use]
    pub fn table(&self, name: &str) -> Option<&Table> {
        self.tables.iter().find(|t| t.name == name)
    }
}

/// A syntax error.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct TomlError {
    /// 1-based line number.
    pub line: usize,
    /// What is wrong.
    pub what: &'static str,
}

impl fmt::Display for TomlError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "line {}: {}", self.line, self.what)
    }
}

impl std::error::Error for TomlError {}

/// Parses a document.
///
/// # Errors
/// [`TomlError`] at the first malformed line, a duplicate table, or a
/// duplicate key within a table.
pub fn parse(text: &str) -> Result<Document, TomlError> {
    let mut doc = Document {
        tables: vec![Table {
            name: String::new(),
            entries: Vec::new(),
        }],
    };
    for (n, raw) in text.lines().enumerate() {
        let line = n + 1;
        let err = |what| TomlError { line, what };
        let mut cur = Cursor { s: raw.trim(), line };
        if cur.s.is_empty() || cur.s.starts_with('#') {
            continue;
        }
        if let Some(rest) = cur.s.strip_prefix('[') {
            cur.s = rest.trim_start();
            let name = cur.key()?;
            cur.skip_ws();
            cur.s = cur.s.strip_prefix(']').ok_or(err("expected `]`"))?;
            cur.end()?;
            if doc.table(&name).is_some() {
                return Err(err("duplicate table"));
            }
            doc.tables.push(Table {
                name,
                entries: Vec::new(),
            });
            continue;
        }
        let key = cur.key()?;
        cur.skip_ws();
        cur.s = cur.s.strip_prefix('=').ok_or(err("expected `=`"))?;
        cur.skip_ws();
        let value = cur.value()?;
        cur.end()?;
        let table = doc.tables.last_mut().ok_or(err("no table"))?;
        if table.get(&key).is_some() {
            return Err(err("duplicate key"));
        }
        table.entries.push(Entry { key, value, line });
    }
    Ok(doc)
}

struct Cursor<'a> {
    s: &'a str,
    line: usize,
}

impl Cursor<'_> {
    fn err(&self, what: &'static str) -> TomlError {
        TomlError {
            line: self.line,
            what,
        }
    }

    fn skip_ws(&mut self) {
        self.s = self.s.trim_start();
    }

    /// Only whitespace or a comment may follow.
    fn end(&mut self) -> Result<(), TomlError> {
        self.skip_ws();
        if self.s.is_empty() || self.s.starts_with('#') {
            Ok(())
        } else {
            Err(self.err("unexpected text after the value"))
        }
    }

    fn key(&mut self) -> Result<String, TomlError> {
        if self.s.starts_with('"') {
            return self.string();
        }
        let len = self
            .s
            .find(|c: char| !(c.is_ascii_alphanumeric() || c == '_' || c == '-' || c == '.'))
            .unwrap_or(self.s.len());
        if len == 0 {
            return Err(self.err("expected a key"));
        }
        let (key, rest) = self.s.split_at(len);
        self.s = rest;
        Ok(key.to_owned())
    }

    fn string(&mut self) -> Result<String, TomlError> {
        let body = self.s.strip_prefix('"').ok_or(self.err("expected a string"))?;
        let mut out = String::new();
        let mut chars = body.char_indices();
        while let Some((i, c)) = chars.next() {
            match c {
                '"' => {
                    self.s = body.get(i + 1..).unwrap_or("");
                    return Ok(out);
                }
                '\\' => match chars.next() {
                    Some((_, '"')) => out.push('"'),
                    Some((_, '\\')) => out.push('\\'),
                    Some((_, 'n')) => out.push('\n'),
                    Some((_, 't')) => out.push('\t'),
                    _ => return Err(self.err("unsupported escape")),
                },
                c if c.is_control() => return Err(self.err("control character in a string")),
                c => out.push(c),
            }
        }
        Err(self.err("unterminated string"))
    }

    fn value(&mut self) -> Result<Value, TomlError> {
        if self.s.starts_with('"') {
            return self.string().map(Value::Str);
        }
        if let Some(rest) = self.s.strip_prefix('[') {
            self.s = rest;
            let mut items = Vec::new();
            loop {
                self.skip_ws();
                if let Some(rest) = self.s.strip_prefix(']') {
                    self.s = rest;
                    return Ok(Value::Array(items));
                }
                let v = self.value()?;
                if matches!(v, Value::Array(_)) {
                    return Err(self.err("nested arrays are not supported"));
                }
                items.push(v);
                self.skip_ws();
                if let Some(rest) = self.s.strip_prefix(',') {
                    self.s = rest;
                } else if !self.s.starts_with(']') {
                    return Err(self.err("expected `,` or `]`"));
                }
            }
        }
        let len = self
            .s
            .find(|c: char| c.is_whitespace() || c == ',' || c == ']' || c == '#')
            .unwrap_or(self.s.len());
        let (word, rest) = self.s.split_at(len);
        let v = match word {
            "true" => Value::Bool(true),
            "false" => Value::Bool(false),
            w => {
                let digits = w.replace('_', "");
                if let Ok(i) = digits.parse::<i64>() {
                    Value::Int(i)
                } else if digits.parse::<f64>().is_ok_and(f64::is_finite) {
                    Value::Float(digits)
                } else {
                    return Err(self.err("expected a string, number, boolean, or array"));
                }
            }
        };
        self.s = rest;
        Ok(v)
    }
}

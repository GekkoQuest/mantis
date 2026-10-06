//! A small strict JSON reader for the Ops inspector's responses (objects, arrays,
//! strings, integers, booleans, null). Numbers with a fraction or exponent are refused:
//! the inspector sends integers and strings only.

use std::collections::BTreeMap;

/// A JSON value.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Json {
    /// `null`.
    Null,
    /// `true` or `false`.
    Bool(bool),
    /// An integer.
    Int(i64),
    /// A string.
    Str(String),
    /// An array.
    Array(Vec<Json>),
    /// An object (keys sorted; a repeated key is refused).
    Object(BTreeMap<String, Json>),
}

impl Json {
    /// Field `key` of an object.
    pub fn get(&self, key: &str) -> Option<&Json> {
        match self {
            Json::Object(m) => m.get(key),
            _ => None,
        }
    }

    /// The integer, if this is one.
    pub fn int(&self) -> Option<i64> {
        match self {
            Json::Int(i) => Some(*i),
            _ => None,
        }
    }

    /// The string, if this is one.
    pub fn str(&self) -> Option<&str> {
        match self {
            Json::Str(s) => Some(s),
            _ => None,
        }
    }

    /// The items, if this is an array.
    pub fn items(&self) -> &[Json] {
        match self {
            Json::Array(v) => v,
            _ => &[],
        }
    }
}

/// Deepest nesting accepted.
const MAX_DEPTH: usize = 32;

struct Parser<'a> {
    s: &'a [u8],
    i: usize,
}

impl Parser<'_> {
    fn peek(&self) -> Option<u8> {
        self.s.get(self.i).copied()
    }

    fn ws(&mut self) {
        while self
            .peek()
            .is_some_and(|c| matches!(c, b' ' | b'\t' | b'\n' | b'\r'))
        {
            self.i += 1;
        }
    }

    fn expect(&mut self, word: &[u8]) -> Result<(), String> {
        if self.s.get(self.i..self.i + word.len()) == Some(word) {
            self.i += word.len();
            Ok(())
        } else {
            Err(format!(
                "expected `{}` at byte {}",
                String::from_utf8_lossy(word),
                self.i
            ))
        }
    }

    fn value(&mut self, depth: usize) -> Result<Json, String> {
        if depth > MAX_DEPTH {
            return Err("nested too deeply".to_owned());
        }
        self.ws();
        match self.peek() {
            Some(b'n') => self.expect(b"null").map(|()| Json::Null),
            Some(b't') => self.expect(b"true").map(|()| Json::Bool(true)),
            Some(b'f') => self.expect(b"false").map(|()| Json::Bool(false)),
            Some(b'"') => self.string().map(Json::Str),
            Some(b'[') => {
                self.i += 1;
                let mut items = Vec::new();
                self.ws();
                if self.peek() == Some(b']') {
                    self.i += 1;
                    return Ok(Json::Array(items));
                }
                loop {
                    items.push(self.value(depth + 1)?);
                    self.ws();
                    match self.peek() {
                        Some(b',') => self.i += 1,
                        Some(b']') => {
                            self.i += 1;
                            return Ok(Json::Array(items));
                        }
                        _ => return Err(format!("expected `,` or `]` at byte {}", self.i)),
                    }
                }
            }
            Some(b'{') => {
                self.i += 1;
                let mut map = BTreeMap::new();
                self.ws();
                if self.peek() == Some(b'}') {
                    self.i += 1;
                    return Ok(Json::Object(map));
                }
                loop {
                    self.ws();
                    let key = self.string()?;
                    self.ws();
                    self.expect(b":")?;
                    let v = self.value(depth + 1)?;
                    if map.insert(key.clone(), v).is_some() {
                        return Err(format!("key `{key}` repeats"));
                    }
                    self.ws();
                    match self.peek() {
                        Some(b',') => self.i += 1,
                        Some(b'}') => {
                            self.i += 1;
                            return Ok(Json::Object(map));
                        }
                        _ => return Err(format!("expected `,` or `}}` at byte {}", self.i)),
                    }
                }
            }
            Some(c) if c == b'-' || c.is_ascii_digit() => self.int(),
            _ => Err(format!("unexpected input at byte {}", self.i)),
        }
    }

    fn int(&mut self) -> Result<Json, String> {
        let start = self.i;
        if self.peek() == Some(b'-') {
            self.i += 1;
        }
        while self.peek().is_some_and(|c| c.is_ascii_digit()) {
            self.i += 1;
        }
        if self.peek().is_some_and(|c| matches!(c, b'.' | b'e' | b'E')) {
            return Err(format!("a non-integer number at byte {start}"));
        }
        let text =
            std::str::from_utf8(self.s.get(start..self.i).unwrap_or(&[])).map_err(|e| e.to_string())?;
        text.parse()
            .map(Json::Int)
            .map_err(|_| format!("`{text}` is not an integer"))
    }

    fn string(&mut self) -> Result<String, String> {
        self.expect(b"\"")?;
        let mut out = Vec::new();
        loop {
            let c = self.peek().ok_or("an unterminated string")?;
            self.i += 1;
            match c {
                b'"' => break,
                b'\\' => {
                    let e = self.peek().ok_or("an unterminated escape")?;
                    self.i += 1;
                    match e {
                        b'"' => out.push(b'"'),
                        b'\\' => out.push(b'\\'),
                        b'/' => out.push(b'/'),
                        b'n' => out.push(b'\n'),
                        b't' => out.push(b'\t'),
                        b'r' => out.push(b'\r'),
                        b'b' => out.push(8),
                        b'f' => out.push(12),
                        b'u' => {
                            let hex = self.s.get(self.i..self.i + 4).ok_or("a short \\u escape")?;
                            self.i += 4;
                            let code =
                                u32::from_str_radix(std::str::from_utf8(hex).map_err(|e| e.to_string())?, 16)
                                    .map_err(|e| e.to_string())?;
                            let ch = char::from_u32(code).unwrap_or('\u{fffd}');
                            let mut buf = [0u8; 4];
                            out.extend_from_slice(ch.encode_utf8(&mut buf).as_bytes());
                        }
                        _ => return Err(format!("a bad escape at byte {}", self.i)),
                    }
                }
                c if c < 0x20 => return Err("a control character in a string".to_owned()),
                c => out.push(c),
            }
        }
        String::from_utf8(out).map_err(|e| e.to_string())
    }
}

/// Parses one JSON document.
///
/// # Errors
/// A description of the first problem.
pub fn parse(text: &str) -> Result<Json, String> {
    let mut p = Parser {
        s: text.as_bytes(),
        i: 0,
    };
    let v = p.value(0)?;
    p.ws();
    if p.i != p.s.len() {
        return Err(format!("trailing input at byte {}", p.i));
    }
    Ok(v)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_the_inspector_shapes() -> Result<(), String> {
        let v = parse(
            r#" {"cell": 3, "systems": [{"name": "core.movement", "micros_p99": 41}], "ok": true, "x": null, "s": "a\"bA"} "#,
        )?;
        assert_eq!(v.get("cell").and_then(Json::int), Some(3));
        let first = v
            .get("systems")
            .map(Json::items)
            .and_then(<[Json]>::first)
            .cloned();
        assert_eq!(
            first.as_ref().and_then(|s| s.get("name")).and_then(Json::str),
            Some("core.movement")
        );
        assert_eq!(v.get("s").and_then(Json::str), Some("a\"bA"));
        assert_eq!(v.get("ok"), Some(&Json::Bool(true)));
        Ok(())
    }

    #[test]
    fn malformed_documents_fail() {
        for bad in [
            "",
            "{",
            "[1,]",
            "{\"a\":1,\"a\":2}",
            "1.5",
            "\"unterminated",
            "{} x",
            "[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[1]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]",
        ] {
            assert!(parse(bad).is_err(), "{bad}");
        }
    }
}

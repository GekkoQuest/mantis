//! Tokens of the schema language.

use crate::IdlError;

/// A source position (1-based).
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct Pos {
    /// Line.
    pub line: u32,
    /// Column, in characters.
    pub col: u32,
}

/// A token.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Tok {
    /// An identifier or keyword.
    Ident(String),
    /// A non-negative integer.
    Int(u64),
    /// The text of a `///` comment, trimmed.
    Doc(String),
    /// One of `; : { } < > , =`.
    Sym(char),
}

/// Splits `source` into tokens. `//` comments are dropped; `///` comments are
/// kept as [`Tok::Doc`].
///
/// # Errors
/// An [`IdlError`] at the first character that starts no token.
pub fn lex(source: &str) -> Result<Vec<(Tok, Pos)>, IdlError> {
    let mut out = Vec::new();
    let mut chars = source.chars().peekable();
    let mut pos = Pos { line: 1, col: 1 };
    let advance = |c: char, pos: &mut Pos| {
        if c == '\n' {
            pos.line += 1;
            pos.col = 1;
        } else {
            pos.col += 1;
        }
    };
    while let Some(&c) = chars.peek() {
        let start = pos;
        if c.is_whitespace() {
            chars.next();
            advance(c, &mut pos);
        } else if c == '/' {
            chars.next();
            advance(c, &mut pos);
            if chars.peek() != Some(&'/') {
                return Err(IdlError::at(start, "expected `//` comment"));
            }
            chars.next();
            advance('/', &mut pos);
            let doc = chars.peek() == Some(&'/');
            if doc {
                chars.next();
                advance('/', &mut pos);
            }
            let mut text = String::new();
            while let Some(&n) = chars.peek() {
                if n == '\n' {
                    break;
                }
                text.push(n);
                chars.next();
                advance(n, &mut pos);
            }
            if doc {
                out.push((Tok::Doc(text.trim().to_owned()), start));
            }
        } else if c.is_ascii_alphabetic() || c == '_' {
            let mut ident = String::new();
            while let Some(&n) = chars.peek() {
                if !(n.is_ascii_alphanumeric() || n == '_') {
                    break;
                }
                ident.push(n);
                chars.next();
                advance(n, &mut pos);
            }
            out.push((Tok::Ident(ident), start));
        } else if c.is_ascii_digit() {
            let mut value: u64 = 0;
            while let Some(&n) = chars.peek() {
                if n == '_' {
                    chars.next();
                    advance(n, &mut pos);
                    continue;
                }
                let Some(d) = n.to_digit(10) else { break };
                value = value
                    .checked_mul(10)
                    .and_then(|v| v.checked_add(u64::from(d)))
                    .ok_or_else(|| IdlError::at(start, "integer too large"))?;
                chars.next();
                advance(n, &mut pos);
            }
            out.push((Tok::Int(value), start));
        } else if matches!(c, ';' | ':' | '{' | '}' | '<' | '>' | ',' | '=') {
            chars.next();
            advance(c, &mut pos);
            out.push((Tok::Sym(c), start));
        } else {
            return Err(IdlError::at(start, &format!("unexpected character `{c}`")));
        }
    }
    Ok(out)
}

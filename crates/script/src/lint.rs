//! Static checks on server script source, before it is loaded.
//!
//! - **No `^` operator.** Luau evaluates `x ^ y` with the platform `pow`,
//!   which differs between `x86_64` and `aarch64` (decision 0013). Scripts use
//!   `math.pow`, routed to the engine's deterministic implementation.
//! - **No table or function keys.** Luau hashes table, function, and full
//!   userdata keys by address, so iteration order differs between runs
//!   (decision 0001). Keys written as a table constructor or a function
//!   expression (`[{...}]`, `[function ...]`) are refused here; keys computed
//!   at run time are caught by the runtime key check over script state.
//!
//! The scanner understands comments, quoted, long, and interpolated
//! strings, so text inside them never trips a check.

use core::fmt;

/// One finding.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Finding {
    /// 1-based line.
    pub line: usize,
    /// What is wrong.
    pub what: &'static str,
}

impl fmt::Display for Finding {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "line {}: {}", self.line, self.what)
    }
}

/// The source with comments and string contents blanked (newlines kept),
/// so later checks see only code.
fn code_only(src: &str) -> String {
    let b = src.as_bytes();
    let mut out = String::with_capacity(src.len());
    let mut i = 0;
    let blank = |out: &mut String, s: &[u8]| {
        for c in s {
            out.push(if *c == b'\n' { '\n' } else { ' ' });
        }
    };
    let long_close = |b: &[u8], at: usize| -> Option<(usize, usize)> {
        // `[` `=`* `[` at `at`: returns (level, length of opener)
        if b.get(at) != Some(&b'[') {
            return None;
        }
        let mut j = at + 1;
        while b.get(j) == Some(&b'=') {
            j += 1;
        }
        (b.get(j) == Some(&b'[')).then_some((j - at - 1, j - at + 1))
    };
    while let Some(&c) = b.get(i) {
        if c == b'-' && b.get(i + 1) == Some(&b'-') {
            // Comment: long or to end of line.
            let start = i;
            if let Some((level, open)) = long_close(b, i + 2) {
                let close: Vec<u8> = std::iter::once(b']')
                    .chain(std::iter::repeat_n(b'=', level))
                    .chain(std::iter::once(b']'))
                    .collect();
                let body = i + 2 + open;
                let end = b
                    .get(body..)
                    .and_then(|rest| find(rest, &close))
                    .map_or(b.len(), |p| body + p + close.len());
                blank(&mut out, b.get(start..end).unwrap_or(&[]));
                i = end;
            } else {
                let end = b
                    .get(i..)
                    .and_then(|rest| rest.iter().position(|c| *c == b'\n'))
                    .map_or(b.len(), |p| i + p);
                blank(&mut out, b.get(start..end).unwrap_or(&[]));
                i = end;
            }
            continue;
        }
        if c == b'"' || c == b'\'' || c == b'`' {
            let start = i;
            i += 1;
            while let Some(&d) = b.get(i) {
                i += 1;
                if d == b'\\' {
                    i += 1;
                } else if d == c || d == b'\n' {
                    break;
                }
            }
            out.push('"');
            blank(
                &mut out,
                b.get(start + 1..i.saturating_sub(1).max(start + 1))
                    .unwrap_or(&[]),
            );
            out.push('"');
            continue;
        }
        if let Some((level, open)) = long_close(b, i) {
            let close: Vec<u8> = std::iter::once(b']')
                .chain(std::iter::repeat_n(b'=', level))
                .chain(std::iter::once(b']'))
                .collect();
            let body = i + open;
            let end = b
                .get(body..)
                .and_then(|rest| find(rest, &close))
                .map_or(b.len(), |p| body + p + close.len());
            out.push('"');
            blank(&mut out, b.get(i + 1..end.saturating_sub(1)).unwrap_or(&[]));
            out.push('"');
            i = end;
            continue;
        }
        out.push(char::from(c));
        i += 1;
    }
    out
}

fn find(hay: &[u8], needle: &[u8]) -> Option<usize> {
    hay.windows(needle.len()).position(|w| w == needle)
}

/// Checks server script source; empty when clean.
#[must_use]
pub fn check_server_source(src: &str) -> Vec<Finding> {
    let code = code_only(src);
    let mut out = Vec::new();
    for (n, line) in code.lines().enumerate() {
        if line.contains('^') {
            out.push(Finding {
                line: n + 1,
                what: "the `^` operator is not deterministic across architectures; use math.pow",
            });
        }
        let squeezed: String = line.chars().filter(|c| !c.is_whitespace()).collect();
        if squeezed.contains("[{") || squeezed.contains("[function") {
            out.push(Finding {
                line: n + 1,
                what: "a table or function used as a key iterates in address order; key by a primitive",
            });
        }
    }
    out
}

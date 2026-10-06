//! The append-only message id registry (plan 6.7, decision 0017).
//!
//! Format: one entry per line, `<id> <MessageName>` or
//! `<id> retired <MessageName>`; `#` starts a comment. Ids are 1..=65535
//! and strictly ascending, so the only valid edit is appending. A message
//! removed from the schema stays as `retired`, so its id is never reused.
//!
//! A schema's `.lock` file is a frozen prefix of its registry:
//! [`check_lock`] refuses a registry that does not start with every lock entry
//! unchanged, so renumbering requires editing the lock too, in plain view.

use crate::IdlError;
use crate::lexer::Pos;

/// One registry line.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Entry {
    /// The message id.
    pub id: u16,
    /// The message name.
    pub name: String,
    /// True for a removed message whose id stays reserved.
    pub retired: bool,
}

/// A parsed registry.
#[derive(Clone, PartialEq, Eq, Debug, Default)]
pub struct Registry {
    /// Entries in file order (strictly ascending ids).
    pub entries: Vec<Entry>,
}

impl Registry {
    /// The live (not retired) entry for `name`.
    #[must_use]
    pub fn live(&self, name: &str) -> Option<&Entry> {
        self.entries.iter().find(|e| e.name == name && !e.retired)
    }
}

/// Parses a registry file.
///
/// # Errors
/// A malformed line, an id out of range or not strictly ascending, or a
/// duplicate name.
pub fn parse_registry(source: &str) -> Result<Registry, IdlError> {
    let mut entries: Vec<Entry> = Vec::new();
    for (n, raw) in source.lines().enumerate() {
        let line = u32::try_from(n + 1).unwrap_or(u32::MAX);
        let pos = Pos { line, col: 1 };
        let text = raw.split('#').next().unwrap_or("").trim();
        if text.is_empty() {
            continue;
        }
        let words: Vec<&str> = text.split_whitespace().collect();
        let (id, name, retired) = match words.as_slice() {
            [id, name] => (*id, *name, false),
            [id, "retired", name] => (*id, *name, true),
            _ => {
                return Err(IdlError::at(
                    pos,
                    "expected `<id> <Name>` or `<id> retired <Name>`",
                ));
            }
        };
        let id: u16 = id
            .parse()
            .ok()
            .filter(|v| *v > 0)
            .ok_or_else(|| IdlError::at(pos, "id must be 1..=65535"))?;
        if let Some(last) = entries.last()
            && id <= last.id
        {
            return Err(IdlError::at(
                pos,
                "ids must be strictly ascending: the registry is append-only",
            ));
        }
        if entries.iter().any(|e| e.name == name) {
            return Err(IdlError::at(pos, &format!("`{name}` is registered twice")));
        }
        entries.push(Entry {
            id,
            name: name.to_owned(),
            retired,
        });
    }
    Ok(Registry { entries })
}

/// Checks that `registry` starts with every entry of `lock`, unchanged.
///
/// # Errors
/// The first lock entry that was renumbered, renamed, removed, or
/// un-retired. Retiring a locked entry is allowed (it keeps its id).
pub fn check_lock(lock: &Registry, registry: &Registry) -> Result<(), IdlError> {
    for (i, locked) in lock.entries.iter().enumerate() {
        let pos = Pos {
            line: u32::try_from(i + 1).unwrap_or(u32::MAX),
            col: 1,
        };
        match registry.entries.get(i) {
            Some(e) if e.id == locked.id && e.name == locked.name && (e.retired || !locked.retired) => {}
            _ => {
                return Err(IdlError::at(
                    pos,
                    &format!(
                        "locked entry `{} {}` was renumbered, renamed, removed, or reordered",
                        locked.id, locked.name
                    ),
                ));
            }
        }
    }
    Ok(())
}

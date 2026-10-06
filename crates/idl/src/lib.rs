//! mantis-idl: the schema language, its checks, the append-only id registry,
//! and the Rust code generator (plan 6.7, decision 0017).
//!
//! Generated Rust is **checked in** next to the schema and depends only on
//! `mantis_core::wire`. A freshness test in `mantis-testkit` regenerates every
//! schema in the workspace and fails on drift. This crate is a tool: nothing
//! depends on it at build time.
//!
//! # Schema language
//!
//! ```text
//! /// Docs are required on the package, every item, field, and variant.
//! package intents;
//!
//! /// A newtype over an integer primitive.
//! newtype AbilityId : u32;
//!
//! /// Enums carry explicit wire values; undefined values are refused.
//! enum Stance : u8 {
//!     /// Standing.
//!     Stand = 0;
//!     /// Crouching.
//!     Crouch = 1;
//! }
//!
//! /// Structs are inline and `Copy`.
//! struct Slot {
//!     /// Which bag.
//!     bag: u8;
//!     /// Optional label.
//!     label: option<string<16>>;
//! }
//!
//! /// Messages say who sends them. Ids come from the registry, never from here.
//! message Cast inbound {
//!     /// What to cast.
//!     ability: AbilityId;
//!     /// At whom.
//!     target: option<entity>;
//!     /// Up to four slots.
//!     slots: list<Slot, 4>;
//! }
//! ```
//!
//! Types: `u8 u16 u32 u64 i8 i16 i32 i64 f32 bool` (`f32` is finite only),
//! the core builtins `entity tick vec3 angle16 input_seq buttons move_input
//! content_hash`, schema items, `list<T, N>`, `string<N>`, and `option<T>`.
//! Bounds are 1..=65535. Recursive types are refused.
//!
//! # Generated API
//!
//! For each item: a Rust type with `Wire` and `FuzzSample`. For each message:
//! `Message` with its registered id. For inbound messages: `enum Inbound`, a
//! `trait Validators` with one **required** method per message, and
//! `decode_inbound(id, bytes, &impl Validators)`. For outbound messages:
//! `enum Outbound` and `decode_outbound(id, bytes)`.

mod codegen;
mod lexer;
mod model;
mod parser;
mod registry;

pub use codegen::generate;
pub use lexer::Pos;
pub use model::{Builtin, Direction, EnumDef, Field, Item, ItemKind, Prim, Schema, Type, Variant};
pub use parser::parse;
pub use registry::{Entry, Registry, check_lock, parse_registry};

use core::fmt;

/// A schema, registry, or codegen error at a position.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct IdlError {
    /// Where.
    pub pos: Pos,
    /// What.
    pub message: String,
}

impl IdlError {
    pub(crate) fn at(pos: Pos, message: &str) -> Self {
        Self {
            pos,
            message: message.to_owned(),
        }
    }
}

impl fmt::Display for IdlError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}:{}: {}", self.pos.line, self.pos.col, self.message)
    }
}

impl std::error::Error for IdlError {}

/// Paths of one schema unit: `<dir>/<name>.idl`, `<dir>/<name>.registry`,
/// `<dir>/<name>.lock`, generated into `<crate>/src/generated/<name>.rs`.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Unit {
    /// The schema source text.
    pub schema: String,
    /// The registry text.
    pub registry: String,
    /// The lock text.
    pub lock: String,
}

/// Parses, checks the registry and its lock, and generates.
///
/// # Errors
/// The first [`IdlError`], prefixed with the file it belongs to.
pub fn compile(unit: &Unit, origin: &str) -> Result<String, IdlError> {
    fn tag(file: &'static str) -> impl Fn(IdlError) -> IdlError {
        move |e: IdlError| IdlError {
            pos: e.pos,
            message: format!("{file}: {}", e.message),
        }
    }
    let schema = parse(&unit.schema).map_err(tag("schema"))?;
    let registry = parse_registry(&unit.registry).map_err(tag("registry"))?;
    let lock = parse_registry(&unit.lock).map_err(tag("lock"))?;
    check_lock(&lock, &registry).map_err(tag("registry vs lock"))?;
    generate(&schema, &registry, origin).map_err(tag("codegen"))
}

#[cfg(test)]
mod tests;

//! The checked schema model.

use crate::lexer::Pos;

/// A parsed, checked schema.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Schema {
    /// Package documentation.
    pub package_docs: Vec<String>,
    /// Package name (`snake_case`).
    pub package: String,
    /// Items in source order.
    pub items: Vec<Item>,
}

impl Schema {
    /// The item named `name`.
    #[must_use]
    pub fn item(&self, name: &str) -> Option<&Item> {
        self.items.iter().find(|i| i.name == name)
    }

    /// Messages in source order.
    pub fn messages(&self) -> impl Iterator<Item = (&Item, Direction, &[Field])> {
        self.items.iter().filter_map(|i| match &i.kind {
            ItemKind::Message(d, f) => Some((i, *d, f.as_slice())),
            _ => None,
        })
    }
}

/// One top-level definition.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Item {
    /// Documentation lines.
    pub docs: Vec<String>,
    /// Type name (`PascalCase`).
    pub name: String,
    /// Where it was defined.
    pub pos: Pos,
    /// What it is.
    pub kind: ItemKind,
}

impl Item {
    /// The fields of a struct or message (empty otherwise).
    #[must_use]
    pub fn fields(&self) -> &[Field] {
        match &self.kind {
            ItemKind::Struct(f) | ItemKind::Message(_, f) => f,
            ItemKind::Enum(_) | ItemKind::Newtype(_) => &[],
        }
    }
}

/// Item kinds.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum ItemKind {
    /// `newtype Name : prim;`
    Newtype(Prim),
    /// `enum Name : u8 { ... }`
    Enum(EnumDef),
    /// `struct Name { ... }`
    Struct(Vec<Field>),
    /// `message Name inbound|outbound { ... }`
    Message(Direction, Vec<Field>),
}

/// Who sends a message.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Direction {
    /// Client to server; gets a required hand-written validator.
    Inbound,
    /// Server to client.
    Outbound,
}

/// An enum definition.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct EnumDef {
    /// `u8` or `u16`.
    pub repr: Prim,
    /// Variants in source order.
    pub variants: Vec<Variant>,
}

/// An enum variant.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Variant {
    /// Documentation lines.
    pub docs: Vec<String>,
    /// Name (`PascalCase`).
    pub name: String,
    /// Wire value.
    pub value: u64,
}

/// A struct or message field.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Field {
    /// Documentation lines.
    pub docs: Vec<String>,
    /// Name (`snake_case`).
    pub name: String,
    /// Type.
    pub ty: Type,
}

/// A field type.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Type {
    /// A primitive.
    Prim(Prim),
    /// A core type with a fixed wire form.
    Builtin(Builtin),
    /// Another item of the schema.
    Named(String, Pos),
    /// `list<T, N>`.
    List(Box<Type>, u16),
    /// `string<N>` (UTF-8 bytes).
    String(u16),
    /// `option<T>`.
    Option(Box<Type>),
}

/// Primitive types.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Prim {
    /// `u8`
    U8,
    /// `u16`
    U16,
    /// `u32`
    U32,
    /// `u64`
    U64,
    /// `i8`
    I8,
    /// `i16`
    I16,
    /// `i32`
    I32,
    /// `i64`
    I64,
    /// `f32` (finite only on the wire)
    F32,
    /// `bool`
    Bool,
}

impl Prim {
    /// The primitive with this schema name.
    #[must_use]
    pub fn from_name(s: &str) -> Option<Self> {
        Some(match s {
            "u8" => Self::U8,
            "u16" => Self::U16,
            "u32" => Self::U32,
            "u64" => Self::U64,
            "i8" => Self::I8,
            "i16" => Self::I16,
            "i32" => Self::I32,
            "i64" => Self::I64,
            "f32" => Self::F32,
            "bool" => Self::Bool,
            _ => return None,
        })
    }

    /// The Rust type.
    #[must_use]
    pub fn rust(self) -> &'static str {
        match self {
            Self::U8 => "u8",
            Self::U16 => "u16",
            Self::U32 => "u32",
            Self::U64 => "u64",
            Self::I8 => "i8",
            Self::I16 => "i16",
            Self::I32 => "i32",
            Self::I64 => "i64",
            Self::F32 => "f32",
            Self::Bool => "bool",
        }
    }
}

/// Core types with a fixed wire form (implemented in `mantis_core::wire`).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Builtin {
    /// `entity`: `EntityId`.
    Entity,
    /// `tick`: `Tick`.
    Tick,
    /// `vec3`: `Vec3` (finite).
    Vec3,
    /// `angle16`: `Angle16`.
    Angle16,
    /// `input_seq`: `InputSeq`.
    InputSeq,
    /// `buttons`: `MoveButtons` (undefined bits refused).
    Buttons,
    /// `move_input`: `MoveInput`.
    MoveInput,
    /// `content_hash`: `ContentHash`.
    ContentHash,
}

impl Builtin {
    /// The builtin with this schema name.
    #[must_use]
    pub fn from_name(s: &str) -> Option<Self> {
        Some(match s {
            "entity" => Self::Entity,
            "tick" => Self::Tick,
            "vec3" => Self::Vec3,
            "angle16" => Self::Angle16,
            "input_seq" => Self::InputSeq,
            "buttons" => Self::Buttons,
            "move_input" => Self::MoveInput,
            "content_hash" => Self::ContentHash,
            _ => return None,
        })
    }

    /// The fully qualified Rust type.
    #[must_use]
    pub fn rust(self) -> &'static str {
        match self {
            Self::Entity => "::mantis_core::ecs::EntityId",
            Self::Tick => "::mantis_core::time::Tick",
            Self::Vec3 => "::mantis_core::math::Vec3",
            Self::Angle16 => "::mantis_core::kinematics::Angle16",
            Self::InputSeq => "::mantis_core::kinematics::InputSeq",
            Self::Buttons => "::mantis_core::kinematics::MoveButtons",
            Self::MoveInput => "::mantis_core::kinematics::MoveInput",
            Self::ContentHash => "::mantis_core::content::ContentHash",
        }
    }
}

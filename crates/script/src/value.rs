//! Values that cross between scripts and the host.
//!
//! Only primitives cross: nil, booleans, numbers, strings, and entities.
//! An entity is a light userdata carrying the packed `EntityId` bits, so it
//! compares and hashes by value (decision 0001), never by address.

use mantis_core::ecs::EntityId;
use mantis_core::hash::StableHasher;
use mlua::{IntoLua, LightUserData, Lua, Value};

/// A value crossing the script boundary.
#[derive(Clone, PartialEq, Debug)]
pub enum ScriptValue {
    /// `nil`.
    Nil,
    /// A boolean.
    Bool(bool),
    /// A number (Luau numbers are doubles).
    Number(f64),
    /// A string.
    Str(String),
    /// An entity (light userdata).
    Entity(EntityId),
}

impl ScriptValue {
    /// The number, if this is one.
    #[must_use]
    pub fn as_number(&self) -> Option<f64> {
        match self {
            Self::Number(n) => Some(*n),
            _ => None,
        }
    }

    /// The entity, if this is one.
    #[must_use]
    pub fn as_entity(&self) -> Option<EntityId> {
        match self {
            Self::Entity(e) => Some(*e),
            _ => None,
        }
    }

    /// The string, if this is one.
    #[must_use]
    pub fn as_str(&self) -> Option<&str> {
        match self {
            Self::Str(s) => Some(s),
            _ => None,
        }
    }
}

/// The light userdata for an entity: its bits as the pointer value.
#[must_use]
pub fn entity_userdata(e: EntityId) -> LightUserData {
    let bits = usize::try_from(e.to_bits()).unwrap_or(usize::MAX);
    LightUserData(std::ptr::without_provenance_mut(bits))
}

/// The entity a light userdata carries.
#[must_use]
pub fn entity_of(u: LightUserData) -> EntityId {
    EntityId::from_bits(u64::try_from(u.0.addr()).unwrap_or(u64::MAX))
}

impl IntoLua for ScriptValue {
    fn into_lua(self, lua: &Lua) -> mlua::Result<Value> {
        Ok(match self {
            Self::Nil => Value::Nil,
            Self::Bool(b) => Value::Boolean(b),
            Self::Number(n) => Value::Number(n),
            Self::Str(s) => Value::String(lua.create_string(s)?),
            Self::Entity(e) => Value::LightUserData(entity_userdata(e)),
        })
    }
}

impl ScriptValue {
    /// Converts a Lua value; tables, functions, threads, and full userdata
    /// do not cross and read as `None`.
    #[must_use]
    pub fn from_lua(v: &Value) -> Option<Self> {
        Some(match v {
            Value::Nil => Self::Nil,
            Value::Boolean(b) => Self::Bool(*b),
            Value::Integer(i) => Self::Number(f64::from(i32::try_from(*i).ok()?)),
            Value::Number(n) => Self::Number(*n),
            Value::String(s) => Self::Str(s.to_str().ok()?.to_owned()),
            Value::LightUserData(u) => Self::Entity(entity_of(*u)),
            _ => return None,
        })
    }
}

/// Hashes a value canonically (allocation-free).
pub fn hash_value(v: &ScriptValue, h: &mut StableHasher) {
    match v {
        ScriptValue::Nil => h.write_u8(0),
        ScriptValue::Bool(b) => {
            h.write_u8(1);
            h.write_u8(u8::from(*b));
        }
        ScriptValue::Number(n) => {
            h.write_u8(2);
            h.write_f64(*n);
        }
        ScriptValue::Str(s) => {
            h.write_u8(3);
            h.write(s.as_bytes());
            h.write_u8(0);
        }
        ScriptValue::Entity(e) => {
            h.write_u8(4);
            h.write_u64(e.to_bits());
        }
    }
}

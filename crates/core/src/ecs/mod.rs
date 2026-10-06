//! Entity-component system (plan 6.1).
//!
//! - Archetype storage, structure of arrays: one dense table per component
//!   set, one `Vec<T>` column per component, rows packed by swap-remove.
//! - [`EntityId`] is `(index, generation)`. Despawn bumps the generation, so
//!   stale ids fail lookups instead of aliasing.
//! - Lookups return `Option` or `Result`; nothing panics.
//! - Components are registered by modules and have stable names. Queries are
//!   compiled once and reused.
//! - Change ticks: every component carries the tick of its last write.
//!   [`Write`] stamps it through [`Mut`]; [`Tracked`] exposes it through [`Ref`].
//! - No unsafe code: columns are type-erased behind a safe `Any` downcast done
//!   once per archetype per query, never per entity.
//! - Iteration order is a pure function of the history of structural changes,
//!   so it is identical under replay.

mod access;
mod archetype;
mod bundle;
mod column;
mod component;
mod entity;
mod query;
mod resource;
mod world;

#[cfg(test)]
mod tests;

pub use access::{Access, AccessBuilder};
pub use bundle::Bundle;
pub use component::{Component, ComponentId, ComponentSet, Inspect, MAX_COMPONENTS, ResourceId, ResourceSet};
pub use entity::EntityId;
pub use query::{
    Mut, Query, QueryAccess, QueryBuilder, QueryData, Read, ReadOnlyQueryData, ReadTerm, Ref, Term, Tracked,
    TrackedIter, Write, WriteIter,
};
pub use resource::{Resource, Resources, Saved};
pub use world::{Components, InspectPage, InspectedEntity, WORLD_HASH_V1, World};

use core::fmt;

/// ECS failure. Every fallible ECS operation returns one; none panics.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum EcsError {
    /// The id does not refer to a live entity.
    NoSuchEntity(EntityId),
    /// The component (by name) was never registered with this world.
    Unregistered(&'static str),
    /// A component appears twice in one bundle or query.
    DuplicateComponent(&'static str),
    /// Two different types claim the same component or resource name.
    NameClash(&'static str),
    /// More than [`MAX_COMPONENTS`] component or resource types.
    TooManyComponents,
    /// Every entity index is in use.
    EntityLimit,
    /// A query was used with a world it was not compiled against.
    WrongWorld,
    /// A read-only query ran against archetypes it has not seen; call
    /// [`Query::update`] first.
    StaleQuery,
    /// A query both requires and excludes a component.
    ContradictoryFilter,
    /// A storage invariant is broken. Never expected; reported instead of
    /// panicking.
    Internal(&'static str),
}

impl fmt::Display for EcsError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NoSuchEntity(id) => write!(f, "no live entity {id}"),
            Self::Unregistered(n) => write!(f, "component `{n}` is not registered"),
            Self::DuplicateComponent(n) => write!(f, "component `{n}` listed twice"),
            Self::NameClash(n) => write!(f, "two types share the name `{n}`"),
            Self::TooManyComponents => write!(f, "more than {MAX_COMPONENTS} component types"),
            Self::EntityLimit => f.write_str("entity index space exhausted"),
            Self::WrongWorld => f.write_str("query used with a different world"),
            Self::StaleQuery => f.write_str("read-only query is stale; update it first"),
            Self::ContradictoryFilter => f.write_str("query requires and excludes the same component"),
            Self::Internal(what) => write!(f, "ECS invariant broken: {what}"),
        }
    }
}

impl std::error::Error for EcsError {}

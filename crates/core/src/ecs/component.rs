//! Components, component ids, id sets, and the registry.

use core::any::TypeId;
use core::fmt;
use std::collections::BTreeMap;

use super::EcsError;
use super::column::{Column, TypedColumn};
use crate::hash::StateHash;
use crate::wire::{DecodeError, Decoder, Encoder};

/// A value as text for the Ops inspector (never parsed back, never sent to
/// clients). Every [`Component`] implements it; it comes from
/// `#[derive(Debug)]` through the blanket impl, so no component writes it
/// by hand (a hand-written `Debug` can hide a field).
pub trait Inspect {
    /// Writes the value as text.
    ///
    /// # Errors
    /// The writer's error.
    fn inspect(&self, out: &mut dyn fmt::Write) -> fmt::Result;
}

impl<T: fmt::Debug> Inspect for T {
    fn inspect(&self, out: &mut dyn fmt::Write) -> fmt::Result {
        write!(out, "{self:?}")
    }
}

/// Plain data attached to entities.
///
/// Components are registered by modules at startup ([`super::World::register`]).
/// Every component has a stable [`Component::NAME`] (used in schedules,
/// inspector output, and logs instead of compiler type names, which are not
/// stable), implements [`StateHash`] so per-tick state hashes cover it, and
/// [`Inspect`] (derive `Debug`) so the Ops inspector can show it.
pub trait Component: 'static + Send + Sync + StateHash + Inspect {
    /// Stable, unique, dotted name, for example `"core.position"`.
    const NAME: &'static str;

    /// Writes this value into a snapshot (decision 0007). The default
    /// refuses, so a world holding a component without snapshot support
    /// cannot be snapshotted by accident.
    fn save(&self, _e: &mut Encoder<'_>) -> bool {
        false
    }

    /// Reads a value written by [`Component::save`].
    ///
    /// # Errors
    /// The bytes are not one, or the component has no snapshot support.
    fn load(_d: &mut Decoder<'_>) -> Result<Self, DecodeError>
    where
        Self: Sized,
    {
        Err(DecodeError::Invalid("component has no snapshot support"))
    }
}

/// The maximum number of distinct component types (and, separately, resource
/// types) in one world.
pub const MAX_COMPONENTS: usize = 512;

const WORDS: usize = MAX_COMPONENTS / 64;

macro_rules! id_set {
    ($(#[$meta:meta])* $set:ident, $id:ident, $what:literal) => {
        $(#[$meta])*
        #[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
        pub struct $set([u64; WORDS]);

        impl $set {
            /// The empty set.
            pub const EMPTY: Self = Self([0; WORDS]);

            /// Adds `id`; returns false if it was already present.
            pub fn insert(&mut self, id: $id) -> bool {
                let (w, b) = (id.index() / 64, id.index() % 64);
                match self.0.get_mut(w) {
                    Some(word) => {
                        let had = *word & (1u64 << b) != 0;
                        *word |= 1u64 << b;
                        !had
                    }
                    None => false,
                }
            }

            /// Removes `id`; returns false if it was absent.
            pub fn remove(&mut self, id: $id) -> bool {
                let (w, b) = (id.index() / 64, id.index() % 64);
                match self.0.get_mut(w) {
                    Some(word) => {
                        let had = *word & (1u64 << b) != 0;
                        *word &= !(1u64 << b);
                        had
                    }
                    None => false,
                }
            }

            /// Membership test.
            #[must_use]
            pub fn contains(&self, id: $id) -> bool {
                let (w, b) = (id.index() / 64, id.index() % 64);
                self.0.get(w).is_some_and(|word| word & (1u64 << b) != 0)
            }

            /// True when empty.
            #[must_use]
            pub fn is_empty(&self) -> bool {
                self.0.iter().all(|w| *w == 0)
            }

            /// Number of members.
            #[must_use]
            pub fn len(&self) -> usize {
                self.0.iter().map(|w| w.count_ones() as usize).sum()
            }

            /// True when every member of `self` is in `other`.
            #[must_use]
            pub fn is_subset(&self, other: &Self) -> bool {
                self.0.iter().zip(other.0.iter()).all(|(a, b)| a & !b == 0)
            }

            /// True when the sets share a member.
            #[must_use]
            pub fn intersects(&self, other: &Self) -> bool {
                self.0.iter().zip(other.0.iter()).any(|(a, b)| a & b != 0)
            }

            /// Set union.
            #[must_use]
            pub fn union(&self, other: &Self) -> Self {
                let mut out = *self;
                for (a, b) in out.0.iter_mut().zip(other.0.iter()) {
                    *a |= b;
                }
                out
            }

            /// Set intersection.
            #[must_use]
            pub fn intersection(&self, other: &Self) -> Self {
                let mut out = *self;
                for (a, b) in out.0.iter_mut().zip(other.0.iter()) {
                    *a &= b;
                }
                out
            }

            /// Members in exactly one of the two sets.
            #[must_use]
            pub fn symmetric_difference(&self, other: &Self) -> Self {
                let mut out = *self;
                for (a, b) in out.0.iter_mut().zip(other.0.iter()) {
                    *a ^= b;
                }
                out
            }

            /// Members in ascending id order.
            pub fn iter(&self) -> impl Iterator<Item = $id> + '_ {
                self.0.iter().enumerate().flat_map(|(w, word)| {
                    let word = *word;
                    (0..64u16).filter(move |b| word & (1u64 << b) != 0).filter_map(move |b| {
                        u16::try_from(w * 64).ok().map(|base| $id(base + b))
                    })
                })
            }
        }

        impl fmt::Debug for $set {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.debug_set().entries(self.iter().map(|id| id.0)).finish()
            }
        }

        #[doc = concat!("Dense id of a registered ", $what, " type within one world.")]
        #[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
        pub struct $id(pub(crate) u16);

        impl $id {
            /// Dense index, `0..MAX_COMPONENTS`.
            #[must_use]
            pub const fn index(self) -> usize {
                self.0 as usize
            }
        }
    };
}

id_set!(
    /// A set of [`ComponentId`]s: the identity of an archetype and the unit of
    /// access declarations. Fixed size, `Copy`, allocation-free.
    ComponentSet,
    ComponentId,
    "component"
);

id_set!(
    /// A set of [`ResourceId`]s, for access declarations.
    ResourceSet,
    ResourceId,
    "resource"
);

/// Registration record of one component type.
pub struct ComponentInfo {
    pub(crate) name: &'static str,
    pub(crate) new_column: fn() -> Box<dyn Column>,
}

/// Component registry of one world. Ids are assigned densely in registration
/// order, which is deterministic for a given module set.
#[derive(Default)]
pub struct Registry {
    infos: Vec<ComponentInfo>,
    by_type: BTreeMap<TypeId, ComponentId>,
    by_name: BTreeMap<&'static str, ComponentId>,
}

impl Registry {
    pub(crate) fn register<T: Component>(&mut self) -> Result<ComponentId, EcsError> {
        let type_id = TypeId::of::<T>();
        if let Some(id) = self.by_type.get(&type_id) {
            return Ok(*id);
        }
        if self.by_name.contains_key(T::NAME) {
            return Err(EcsError::NameClash(T::NAME));
        }
        if self.infos.len() >= MAX_COMPONENTS {
            return Err(EcsError::TooManyComponents);
        }
        let id = ComponentId(u16::try_from(self.infos.len()).map_err(|_| EcsError::TooManyComponents)?);
        self.infos.push(ComponentInfo {
            name: T::NAME,
            new_column: || Box::new(TypedColumn::<T>::default()),
        });
        self.by_type.insert(type_id, id);
        self.by_name.insert(T::NAME, id);
        Ok(id)
    }

    /// The id of `T`, if registered.
    pub(crate) fn id_of<T: Component>(&self) -> Option<ComponentId> {
        self.by_type.get(&TypeId::of::<T>()).copied()
    }

    /// The id of `T`, or [`EcsError::Unregistered`].
    pub(crate) fn require<T: Component>(&self) -> Result<ComponentId, EcsError> {
        self.id_of::<T>().ok_or(EcsError::Unregistered(T::NAME))
    }

    pub(crate) fn info(&self, id: ComponentId) -> Option<&ComponentInfo> {
        self.infos.get(id.index())
    }

    pub(crate) fn name(&self, id: ComponentId) -> Option<&'static str> {
        self.info(id).map(|i| i.name)
    }

    pub(crate) fn by_name(&self, name: &str) -> Option<ComponentId> {
        self.by_name.get(name).copied()
    }

    pub(crate) fn len(&self) -> usize {
        self.infos.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn set_operations() {
        let mut a = ComponentSet::EMPTY;
        assert!(a.is_empty());
        assert!(a.insert(ComponentId(3)));
        assert!(!a.insert(ComponentId(3)));
        assert!(a.insert(ComponentId(70)));
        assert!(a.insert(ComponentId(511)));
        assert_eq!(a.len(), 3);
        assert!(a.contains(ComponentId(70)));
        assert!(!a.contains(ComponentId(71)));
        assert_eq!(
            a.iter().collect::<Vec<_>>(),
            vec![ComponentId(3), ComponentId(70), ComponentId(511)]
        );
        let mut b = ComponentSet::EMPTY;
        b.insert(ComponentId(70));
        assert!(b.is_subset(&a));
        assert!(!a.is_subset(&b));
        assert!(a.intersects(&b));
        assert_eq!(a.intersection(&b), b);
        assert_eq!(b.union(&a), a);
        assert!(a.remove(ComponentId(70)));
        assert!(!a.remove(ComponentId(70)));
        assert!(!a.intersects(&b));
        // Out-of-range ids are refused, never panic.
        assert!(!a.insert(ComponentId(600)));
        assert!(!a.contains(ComponentId(600)));
        assert_eq!(format!("{a:?}"), "{3, 511}");
    }
}

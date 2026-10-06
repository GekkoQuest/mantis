//! Bundles: the component tuples an entity is spawned with.

use super::EcsError;
use super::archetype::Archetype;
use super::column::typed_mut;
use super::component::{Component, ComponentSet, Registry};
use crate::time::Tick;

/// A tuple of distinct components, `()` through 8 elements.
pub trait Bundle: Send + Sync + 'static {
    #[doc(hidden)]
    fn component_set(reg: &Registry) -> Result<ComponentSet, EcsError>;
    /// Pushes every component onto its column. On error some columns may
    /// have been pushed; the caller restores the invariant by truncation.
    #[doc(hidden)]
    fn push_into(self, reg: &Registry, arch: &mut Archetype, tick: Tick) -> Result<(), EcsError>;
}

impl Bundle for () {
    fn component_set(_reg: &Registry) -> Result<ComponentSet, EcsError> {
        Ok(ComponentSet::EMPTY)
    }

    fn push_into(self, _reg: &Registry, _arch: &mut Archetype, _tick: Tick) -> Result<(), EcsError> {
        Ok(())
    }
}

pub(crate) fn push_one<T: Component>(
    reg: &Registry,
    arch: &mut Archetype,
    value: T,
    tick: Tick,
) -> Result<(), EcsError> {
    let id = reg.require::<T>()?;
    let col = arch
        .column_mut(id)
        .ok_or(EcsError::Internal("bundle column missing"))?;
    let col = typed_mut::<T>(col).ok_or(EcsError::Internal("bundle column type"))?;
    col.push(value, tick);
    Ok(())
}

macro_rules! impl_bundle {
    ($($T:ident $v:ident),+) => {
        impl<$($T: Component),+> Bundle for ($($T,)+) {
            fn component_set(reg: &Registry) -> Result<ComponentSet, EcsError> {
                let mut set = ComponentSet::EMPTY;
                $(
                    if !set.insert(reg.require::<$T>()?) {
                        return Err(EcsError::DuplicateComponent($T::NAME));
                    }
                )+
                Ok(set)
            }

            fn push_into(self, reg: &Registry, arch: &mut Archetype, tick: Tick) -> Result<(), EcsError> {
                let ($($v,)+) = self;
                $( push_one(reg, arch, $v, tick)?; )+
                Ok(())
            }
        }
    };
}

impl_bundle!(A a);
impl_bundle!(A a, B b);
impl_bundle!(A a, B b, C c);
impl_bundle!(A a, B b, C c, D d);
impl_bundle!(A a, B b, C c, D d, E e);
impl_bundle!(A a, B b, C c, D d, E e, F f);
impl_bundle!(A a, B b, C c, D d, E e, F f, G g);
impl_bundle!(A a, B b, C c, D d, E e, F f, G g, H h);

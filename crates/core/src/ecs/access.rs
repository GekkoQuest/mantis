//! Access declarations: what a system reads and writes.
//!
//! Declarations do not influence execution order (decision 0008). They feed
//! the registration-time conflict lint, the resolved-order report, and the
//! inspector, and they let the scheduler refuse writes in the parallel
//! per-client phases.

use super::EcsError;
use super::component::{Component, ComponentSet, ResourceSet};
use super::query::{Query, QueryData};
use super::resource::Resource;
use super::world::World;

/// Declared component and resource access of one system.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct Access {
    /// Components read.
    pub reads: ComponentSet,
    /// Components written.
    pub writes: ComponentSet,
    /// Resources read.
    pub resource_reads: ResourceSet,
    /// Resources written.
    pub resource_writes: ResourceSet,
}

impl Access {
    /// No access.
    pub const NONE: Self = Self {
        reads: ComponentSet::EMPTY,
        writes: ComponentSet::EMPTY,
        resource_reads: ResourceSet::EMPTY,
        resource_writes: ResourceSet::EMPTY,
    };

    /// Starts a declaration against `world` (ids are per world).
    #[must_use]
    pub fn builder(world: &mut World) -> AccessBuilder<'_> {
        AccessBuilder {
            world,
            access: Self::NONE,
            error: None,
        }
    }

    /// True when the declaration writes anything.
    #[must_use]
    pub fn writes_anything(&self) -> bool {
        !self.writes.is_empty() || !self.resource_writes.is_empty()
    }

    /// Components and resources on which `self` and `other` conflict: one
    /// writes what the other reads or writes.
    #[must_use]
    pub fn conflicts(&self, other: &Self) -> (ComponentSet, ResourceSet) {
        let comps = self
            .writes
            .intersection(&other.reads.union(&other.writes))
            .union(&other.writes.intersection(&self.reads));
        let res = self
            .resource_writes
            .intersection(&other.resource_reads.union(&other.resource_writes))
            .union(&other.resource_writes.intersection(&self.resource_reads));
        (comps, res)
    }
}

/// Builds an [`Access`]. Registers the resources it names, so a declaration
/// can precede the first insertion of the resource.
pub struct AccessBuilder<'w> {
    world: &'w mut World,
    access: Access,
    error: Option<EcsError>,
}

impl AccessBuilder<'_> {
    fn component<T: Component>(mut self, write: bool) -> Self {
        if self.error.is_none() {
            match self.world.components.component_id::<T>() {
                Some(id) => {
                    if write {
                        self.access.writes.insert(id);
                    } else {
                        self.access.reads.insert(id);
                    }
                }
                None => self.error = Some(EcsError::Unregistered(T::NAME)),
            }
        }
        self
    }

    fn resource<R: Resource>(mut self, write: bool) -> Self {
        if self.error.is_none() {
            match self.world.resources.register::<R>() {
                Ok(id) => {
                    if write {
                        self.access.resource_writes.insert(id);
                    } else {
                        self.access.resource_reads.insert(id);
                    }
                }
                Err(e) => self.error = Some(e),
            }
        }
        self
    }

    /// Declares a component read.
    #[must_use]
    pub fn read<T: Component>(self) -> Self {
        self.component::<T>(false)
    }

    /// Declares a component write.
    #[must_use]
    pub fn write<T: Component>(self) -> Self {
        self.component::<T>(true)
    }

    /// Declares a resource read.
    #[must_use]
    pub fn read_resource<R: Resource>(self) -> Self {
        self.resource::<R>(false)
    }

    /// Declares a resource write.
    #[must_use]
    pub fn write_resource<R: Resource>(self) -> Self {
        self.resource::<R>(true)
    }

    /// Declares everything `query` reads and writes.
    #[must_use]
    pub fn query<D: QueryData>(mut self, query: &Query<D>) -> Self {
        let qa = query.access();
        self.access.reads = self.access.reads.union(&qa.reads);
        self.access.writes = self.access.writes.union(&qa.writes);
        self
    }

    /// Finishes the declaration.
    ///
    /// # Errors
    /// The first error met: an unregistered component or a resource name clash.
    pub fn build(self) -> Result<Access, EcsError> {
        match self.error {
            Some(e) => Err(e),
            None => Ok(self.access),
        }
    }
}

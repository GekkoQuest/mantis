//! Compiled queries over archetype tables.
//!
//! A query is compiled once ([`Components::query`] or
//! [`Components::query_builder`]), which resolves component ids and validates
//! access. Running it visits matching archetypes in creation order and rows in
//! storage order, so iteration order is a pure function of the history of
//! structural changes and is identical under replay.
//!
//! Terms:
//! - [`Read<T>`] yields `&T`;
//! - [`Write<T>`] yields [`Mut<T>`], which stamps the world's change tick on
//!   first mutable dereference;
//! - [`Tracked<T>`] yields [`Ref<T>`], a shared reference that also exposes
//!   the component's change tick, for change-set publishing.

use core::fmt;
use core::marker::PhantomData;
use core::ops::{Deref, DerefMut};

use super::EcsError;
use super::archetype::Archetype;
use super::column::{Column, TypedColumn, typed, typed_mut};
use super::component::{Component, ComponentId, ComponentSet, Registry};
use super::entity::EntityId;
use super::world::{Components, WorldId};
use crate::time::Tick;

/// Exclusive access to a component that records a change on mutation.
pub struct Mut<'a, T> {
    value: &'a mut T,
    changed: &'a mut Tick,
    tick: Tick,
}

impl<'a, T> Mut<'a, T> {
    pub(crate) fn new(value: &'a mut T, changed: &'a mut Tick, tick: Tick) -> Self {
        Self { value, changed, tick }
    }

    /// The tick of the last recorded change.
    #[must_use]
    pub fn changed(&self) -> Tick {
        *self.changed
    }
}

impl<T> Deref for Mut<'_, T> {
    type Target = T;
    fn deref(&self) -> &T {
        self.value
    }
}

impl<T> DerefMut for Mut<'_, T> {
    fn deref_mut(&mut self) -> &mut T {
        *self.changed = self.tick;
        self.value
    }
}

impl<T: fmt::Debug> fmt::Debug for Mut<'_, T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.value.fmt(f)
    }
}

/// Shared access to a component together with its change tick.
pub struct Ref<'a, T> {
    value: &'a T,
    changed: Tick,
}

impl<T> Ref<'_, T> {
    /// The tick of the last recorded change (spawn and insert count).
    #[must_use]
    pub fn changed(&self) -> Tick {
        self.changed
    }

    /// True when the component changed strictly after `since`.
    #[must_use]
    pub fn is_changed_since(&self, since: Tick) -> bool {
        self.changed > since
    }
}

impl<T> Deref for Ref<'_, T> {
    type Target = T;
    fn deref(&self) -> &T {
        self.value
    }
}

impl<T: fmt::Debug> fmt::Debug for Ref<'_, T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.value.fmt(f)
    }
}

/// Query term: shared access to `T`.
pub struct Read<T>(PhantomData<fn() -> T>);
/// Query term: exclusive, change-tracked access to `T`.
pub struct Write<T>(PhantomData<fn() -> T>);
/// Query term: shared access to `T` with its change tick.
pub struct Tracked<T>(PhantomData<fn() -> T>);

/// Iterator over a [`Write`] column.
pub struct WriteIter<'a, T> {
    data: core::slice::IterMut<'a, T>,
    changed: core::slice::IterMut<'a, Tick>,
    tick: Tick,
}

impl<'a, T> Iterator for WriteIter<'a, T> {
    type Item = Mut<'a, T>;
    fn next(&mut self) -> Option<Mut<'a, T>> {
        Some(Mut::new(self.data.next()?, self.changed.next()?, self.tick))
    }
}

/// Iterator over a [`Tracked`] column.
pub struct TrackedIter<'a, T> {
    data: core::slice::Iter<'a, T>,
    changed: core::slice::Iter<'a, Tick>,
}

impl<'a, T> Iterator for TrackedIter<'a, T> {
    type Item = Ref<'a, T>;
    fn next(&mut self) -> Option<Ref<'a, T>> {
        Some(Ref {
            value: self.data.next()?,
            changed: *self.changed.next()?,
        })
    }
}

/// One element of a query tuple. Implemented by [`Read`], [`Write`], and
/// [`Tracked`]; not meant to be implemented elsewhere.
pub trait Term: 'static {
    /// The component accessed.
    type Component: Component;
    /// Whether the term writes.
    const WRITES: bool;
    /// What the term yields per entity.
    type Item<'a>;
    /// Per-archetype iterator.
    #[doc(hidden)]
    type Iter<'a>: Iterator<Item = Self::Item<'a>>;
    #[doc(hidden)]
    fn iter_mut(col: &mut dyn Column, tick: Tick) -> Option<Self::Iter<'_>>;
}

/// A [`Term`] that never writes, usable from shared (`&Components`) access.
pub trait ReadTerm: Term {
    #[doc(hidden)]
    fn iter_ref(col: &dyn Column) -> Option<Self::Iter<'_>>;
}

impl<T: Component> Term for Read<T> {
    type Component = T;
    const WRITES: bool = false;
    type Item<'a> = &'a T;
    type Iter<'a> = core::slice::Iter<'a, T>;
    fn iter_mut(col: &mut dyn Column, _tick: Tick) -> Option<Self::Iter<'_>> {
        Self::iter_ref(col)
    }
}

impl<T: Component> ReadTerm for Read<T> {
    fn iter_ref(col: &dyn Column) -> Option<Self::Iter<'_>> {
        typed::<T>(col).map(|c| c.data.iter())
    }
}

impl<T: Component> Term for Tracked<T> {
    type Component = T;
    const WRITES: bool = false;
    type Item<'a> = Ref<'a, T>;
    type Iter<'a> = TrackedIter<'a, T>;
    fn iter_mut(col: &mut dyn Column, _tick: Tick) -> Option<Self::Iter<'_>> {
        Self::iter_ref(col)
    }
}

impl<T: Component> ReadTerm for Tracked<T> {
    fn iter_ref(col: &dyn Column) -> Option<Self::Iter<'_>> {
        typed::<T>(col).map(|c| TrackedIter {
            data: c.data.iter(),
            changed: c.changed.iter(),
        })
    }
}

impl<T: Component> Term for Write<T> {
    type Component = T;
    const WRITES: bool = true;
    type Item<'a> = Mut<'a, T>;
    type Iter<'a> = WriteIter<'a, T>;
    fn iter_mut(col: &mut dyn Column, tick: Tick) -> Option<Self::Iter<'_>> {
        typed_mut::<T>(col).map(|c: &mut TypedColumn<T>| WriteIter {
            data: c.data.iter_mut(),
            changed: c.changed.iter_mut(),
            tick,
        })
    }
}

/// Components a query reads and writes, for access declarations.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct QueryAccess {
    /// Components read (including filters and tracked terms).
    pub reads: ComponentSet,
    /// Components written.
    pub writes: ComponentSet,
}

/// The data a query yields: a tuple of [`Term`]s, `(A,)` through 8 elements.
pub trait QueryData: 'static {
    /// What the query yields per entity.
    type Item<'a>;
    #[doc(hidden)]
    type Ids: Copy + AsRef<[ComponentId]>;
    #[doc(hidden)]
    fn resolve(reg: &Registry) -> Result<Self::Ids, EcsError>;
    #[doc(hidden)]
    fn access(ids: &Self::Ids) -> QueryAccess;
    #[doc(hidden)]
    fn run_mut<'a, F>(
        arch: &'a mut Archetype,
        ids: &Self::Ids,
        tick: Tick,
        f: &mut F,
    ) -> Result<(), EcsError>
    where
        F: FnMut(EntityId, Self::Item<'a>);
}

/// [`QueryData`] made only of [`ReadTerm`]s.
pub trait ReadOnlyQueryData: QueryData {
    #[doc(hidden)]
    fn run_ref<'a, F>(arch: &'a Archetype, ids: &Self::Ids, f: &mut F) -> Result<(), EcsError>
    where
        F: FnMut(EntityId, Self::Item<'a>);
}

const COLUMN_TYPE: EcsError = EcsError::Internal("column type mismatch");
const COLUMN_MISSING: EcsError = EcsError::Internal("archetype lacks a matched column");
const COLUMN_LEN: EcsError = EcsError::Internal("column shorter than entity list");

macro_rules! impl_query_data {
    ($n:literal; $($T:ident $c:ident),+) => {
        impl<$($T: Term),+> QueryData for ($($T,)+) {
            type Item<'a> = ($($T::Item<'a>,)+);
            type Ids = [ComponentId; $n];

            fn resolve(reg: &Registry) -> Result<Self::Ids, EcsError> {
                let ids = [$(reg.require::<$T::Component>()?,)+];
                let mut seen = ComponentSet::EMPTY;
                let names = [$(<$T::Component as Component>::NAME,)+];
                for (id, name) in ids.iter().zip(names) {
                    if !seen.insert(*id) {
                        return Err(EcsError::DuplicateComponent(name));
                    }
                }
                Ok(ids)
            }

            fn access(ids: &Self::Ids) -> QueryAccess {
                let mut acc = QueryAccess::default();
                let writes = [$($T::WRITES,)+];
                for (id, w) in ids.iter().zip(writes) {
                    if w {
                        acc.writes.insert(*id);
                    } else {
                        acc.reads.insert(*id);
                    }
                }
                acc
            }

            fn run_mut<'a, F>(arch: &'a mut Archetype, ids: &Self::Ids, tick: Tick, f: &mut F) -> Result<(), EcsError>
            where
                F: FnMut(EntityId, Self::Item<'a>),
            {
                let mut idx = [0usize; $n];
                for (slot, id) in idx.iter_mut().zip(ids.iter()) {
                    *slot = arch.column_index(*id).ok_or(COLUMN_MISSING)?;
                }
                let Archetype { columns, entities, .. } = arch;
                let [$($c,)+] = columns.get_disjoint_mut(idx).map_err(|_| COLUMN_MISSING)?;
                $( let mut $c = $T::iter_mut(&mut **$c, tick).ok_or(COLUMN_TYPE)?; )+
                for e in entities.iter() {
                    let item = ($( $c.next().ok_or(COLUMN_LEN)?, )+);
                    f(*e, item);
                }
                Ok(())
            }
        }

        impl<$($T: ReadTerm),+> ReadOnlyQueryData for ($($T,)+) {
            fn run_ref<'a, F>(arch: &'a Archetype, ids: &Self::Ids, f: &mut F) -> Result<(), EcsError>
            where
                F: FnMut(EntityId, Self::Item<'a>),
            {
                let mut cols = ids.iter().map(|id| arch.column(*id));
                $( let mut $c = $T::iter_ref(cols.next().flatten().ok_or(COLUMN_MISSING)?).ok_or(COLUMN_TYPE)?; )+
                for e in &arch.entities {
                    let item = ($( $c.next().ok_or(COLUMN_LEN)?, )+);
                    f(*e, item);
                }
                Ok(())
            }
        }
    };
}

impl_query_data!(1; A a);
impl_query_data!(2; A a, B b);
impl_query_data!(3; A a, B b, C c);
impl_query_data!(4; A a, B b, C c, D d);
impl_query_data!(5; A a, B b, C c, D d, E e);
impl_query_data!(6; A a, B b, C c, D d, E e, F2 f2);
impl_query_data!(7; A a, B b, C c, D d, E e, F2 f2, G g);
impl_query_data!(8; A a, B b, C c, D d, E e, F2 f2, G g, H h);

/// Archetypes a fresh query can track before its match list must grow. Growth
/// after warm-up allocates once and is caught by the allocation harness.
const INITIAL_MATCH_CAPACITY: usize = 32;

/// A compiled query. Bound to the world it was compiled against; using it
/// with another world is refused.
pub struct Query<D: QueryData> {
    world: WorldId,
    ids: D::Ids,
    required: ComponentSet,
    without: ComponentSet,
    access: QueryAccess,
    matched: Vec<u32>,
    seen: usize,
    _marker: PhantomData<fn() -> D>,
}

impl<D: QueryData> fmt::Debug for Query<D> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Query")
            .field("required", &self.required)
            .field("without", &self.without)
            .field("matched", &self.matched)
            .finish_non_exhaustive()
    }
}

impl<D: QueryData> Query<D> {
    /// What this query reads and writes.
    #[must_use]
    pub fn access(&self) -> QueryAccess {
        self.access
    }

    fn check_world(&self, comps: &Components) -> Result<(), EcsError> {
        if comps.world_id() == self.world {
            Ok(())
        } else {
            Err(EcsError::WrongWorld)
        }
    }

    /// Brings the archetype match list up to date with `comps`. Called by the
    /// `&mut` runners automatically; call it before sharing the query with
    /// read-only jobs.
    ///
    /// # Errors
    /// [`EcsError::WrongWorld`] for a world this query was not compiled against.
    pub fn update(&mut self, comps: &Components) -> Result<(), EcsError> {
        self.check_world(comps)?;
        let archetypes = comps.archetypes();
        for (index, arch) in archetypes.iter().enumerate().skip(self.seen) {
            if self.required.is_subset(&arch.set) && !self.without.intersects(&arch.set) {
                self.matched
                    .push(u32::try_from(index).map_err(|_| EcsError::Internal("archetype index"))?);
            }
        }
        self.seen = archetypes.len();
        Ok(())
    }

    /// Runs `f` for every matching entity with exclusive access.
    ///
    /// # Errors
    /// [`EcsError::WrongWorld`], or [`EcsError::Internal`] on a broken
    /// storage invariant (never expected).
    pub fn for_each<F>(&mut self, comps: &mut Components, mut f: F) -> Result<(), EcsError>
    where
        F: for<'a> FnMut(EntityId, D::Item<'a>),
    {
        self.update(comps)?;
        let tick = comps.change_tick();
        for &a in &self.matched {
            let arch = comps
                .archetype_mut(a)
                .ok_or(EcsError::Internal("matched archetype"))?;
            D::run_mut(arch, &self.ids, tick, &mut f)?;
        }
        Ok(())
    }

    /// Number of matching entities.
    ///
    /// # Errors
    /// [`EcsError::WrongWorld`].
    pub fn count(&mut self, comps: &Components) -> Result<usize, EcsError> {
        self.update(comps)?;
        Ok(self
            .matched
            .iter()
            .filter_map(|a| comps.archetypes().get(*a as usize))
            .map(Archetype::len)
            .sum())
    }
}

impl<D: ReadOnlyQueryData> Query<D> {
    /// Runs `f` for every matching entity with shared access. Usable from
    /// several threads at once (per-client jobs). The query must be up to
    /// date; see [`Query::update`].
    ///
    /// # Errors
    /// [`EcsError::WrongWorld`]; [`EcsError::StaleQuery`] if archetypes were
    /// created since the last update (fail closed rather than skip entities).
    pub fn for_each_ref<F>(&self, comps: &Components, mut f: F) -> Result<(), EcsError>
    where
        F: for<'a> FnMut(EntityId, D::Item<'a>),
    {
        self.check_world(comps)?;
        if self.seen != comps.archetypes().len() {
            return Err(EcsError::StaleQuery);
        }
        for &a in &self.matched {
            let arch = comps
                .archetypes()
                .get(a as usize)
                .ok_or(EcsError::Internal("matched archetype"))?;
            D::run_ref(arch, &self.ids, &mut f)?;
        }
        Ok(())
    }
}

/// Builds a [`Query`] with `with`/`without` filters.
pub struct QueryBuilder<'w, D: QueryData> {
    comps: &'w Components,
    with: ComponentSet,
    without: ComponentSet,
    error: Option<EcsError>,
    _marker: PhantomData<fn() -> D>,
}

impl<'w, D: QueryData> QueryBuilder<'w, D> {
    pub(crate) fn new(comps: &'w Components) -> Self {
        Self {
            comps,
            with: ComponentSet::EMPTY,
            without: ComponentSet::EMPTY,
            error: None,
            _marker: PhantomData,
        }
    }

    fn filter<T: Component>(mut self, with: bool) -> Self {
        if self.error.is_none() {
            match self.comps.registry().require::<T>() {
                Ok(id) => {
                    if with {
                        self.with.insert(id);
                    } else {
                        self.without.insert(id);
                    }
                }
                Err(e) => self.error = Some(e),
            }
        }
        self
    }

    /// Only entities that also have `T`.
    #[must_use]
    pub fn with<T: Component>(self) -> Self {
        self.filter::<T>(true)
    }

    /// Only entities that lack `T`.
    #[must_use]
    pub fn without<T: Component>(self) -> Self {
        self.filter::<T>(false)
    }

    /// Compiles the query.
    ///
    /// # Errors
    /// [`EcsError::Unregistered`] for an unregistered component,
    /// [`EcsError::DuplicateComponent`] for a component named twice in the
    /// data, [`EcsError::ContradictoryFilter`] when a component is both
    /// required and excluded.
    pub fn build(self) -> Result<Query<D>, EcsError> {
        if let Some(e) = self.error {
            return Err(e);
        }
        let ids = D::resolve(self.comps.registry())?;
        let mut required = self.with;
        for id in ids.as_ref() {
            required.insert(*id);
        }
        if required.intersects(&self.without) {
            return Err(EcsError::ContradictoryFilter);
        }
        let mut access = D::access(&ids);
        access.reads = access.reads.union(&self.with).union(&self.without);
        Ok(Query {
            world: self.comps.world_id(),
            ids,
            required,
            without: self.without,
            access,
            matched: Vec::with_capacity(INITIAL_MATCH_CAPACITY),
            seen: 0,
            _marker: PhantomData,
        })
    }
}

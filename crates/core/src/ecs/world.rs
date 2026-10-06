//! The world: entity tables plus resources.

use core::sync::atomic::{AtomicU64, Ordering};
use std::collections::BTreeMap;

use super::EcsError;
use super::archetype::Archetype;
use super::bundle::{Bundle, push_one};
use super::column::{Column, typed, typed_mut};
use super::component::{Component, ComponentId, ComponentSet, Registry};
use super::entity::{EntityAllocator, EntityId, Location};
use super::query::{Mut, Query, QueryBuilder, QueryData};
use super::resource::{Resource, Resources};
use crate::hash::StableHasher;
use crate::time::Tick;
use crate::wire::{DecodeError, Decoder, Encoder};

/// Format tag of the world state encoding.
pub const WORLD_HASH_V1: u64 = 0x6D61_6E74_6973_5701;

/// Identity of a world, used to refuse a query compiled against another
/// world. Never hashed, logged, or compared across processes.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) struct WorldId(u64);

static NEXT_WORLD_ID: AtomicU64 = AtomicU64::new(1);

impl WorldId {
    fn fresh() -> Self {
        Self(NEXT_WORLD_ID.fetch_add(1, Ordering::Relaxed))
    }
}

/// Entity tables: the entity allocator, the component registry, and the
/// archetypes. Owned by a [`World`] next to its [`Resources`], so a system can
/// iterate a query over `world.components` while reading `world.resources`.
pub struct Components {
    id: WorldId,
    entities: EntityAllocator,
    registry: Registry,
    archetypes: Vec<Archetype>,
    by_set: BTreeMap<ComponentSet, u32>,
    change_tick: Tick,
}

impl Default for Components {
    fn default() -> Self {
        Self::new()
    }
}

const NO_ENTITY: fn(EntityId) -> EcsError = EcsError::NoSuchEntity;

impl Components {
    /// Empty tables with the empty archetype.
    #[must_use]
    pub fn new() -> Self {
        let mut by_set = BTreeMap::new();
        by_set.insert(ComponentSet::EMPTY, 0);
        Self {
            id: WorldId::fresh(),
            entities: EntityAllocator::default(),
            registry: Registry::default(),
            archetypes: vec![Archetype::new(ComponentSet::EMPTY, Vec::new(), Vec::new())],
            by_set,
            change_tick: Tick::ZERO,
        }
    }

    pub(crate) fn world_id(&self) -> WorldId {
        self.id
    }

    pub(crate) fn registry(&self) -> &Registry {
        &self.registry
    }

    pub(crate) fn archetypes(&self) -> &[Archetype] {
        &self.archetypes
    }

    pub(crate) fn archetype_mut(&mut self, index: u32) -> Option<&mut Archetype> {
        self.archetypes.get_mut(index as usize)
    }

    /// Registers component `T` and returns its id. Idempotent.
    ///
    /// # Errors
    /// [`EcsError::NameClash`] if a different type already uses `T::NAME`;
    /// [`EcsError::TooManyComponents`] past the limit.
    pub fn register<T: Component>(&mut self) -> Result<ComponentId, EcsError> {
        self.registry.register::<T>()
    }

    /// The id of `T`, if registered.
    #[must_use]
    pub fn component_id<T: Component>(&self) -> Option<ComponentId> {
        self.registry.id_of::<T>()
    }

    /// The registered name of a component id.
    #[must_use]
    pub fn component_name(&self, id: ComponentId) -> Option<&'static str> {
        self.registry.name(id)
    }

    /// Number of registered component types.
    #[must_use]
    pub fn component_count(&self) -> usize {
        self.registry.len()
    }

    /// The tick stamped on component writes.
    #[must_use]
    pub fn change_tick(&self) -> Tick {
        self.change_tick
    }

    /// Sets the tick stamped on component writes. The scheduler calls this at
    /// the start of every tick.
    pub fn set_change_tick(&mut self, tick: Tick) {
        self.change_tick = tick;
    }

    /// Writes every entity and component into a snapshot: the change tick,
    /// the allocator, then every archetype in creation order with its rows
    /// in order, so iteration order after a restore is the same.
    ///
    /// # Errors
    /// The name of the first component without snapshot support.
    pub fn save(&self, e: &mut Encoder<'_>) -> Result<(), &'static str> {
        e.u64(self.change_tick.0);
        self.entities.save(e);
        e.u32(u32::try_from(self.archetypes.len()).unwrap_or(u32::MAX));
        for arch in &self.archetypes {
            e.u16(u16::try_from(arch.ids.len()).unwrap_or(u16::MAX));
            for id in &arch.ids {
                let name = self.registry.name(*id).unwrap_or("");
                e.u16(u16::try_from(name.len()).unwrap_or(u16::MAX));
                e.bytes(name.as_bytes());
            }
            e.u32(u32::try_from(arch.entities.len()).unwrap_or(u32::MAX));
            for (row, id) in arch.entities.iter().enumerate() {
                e.u64(id.to_bits());
                for (cid, col) in arch.ids.iter().zip(&arch.columns) {
                    if !col.save_row(row, e) {
                        return Err(self.registry.name(*cid).unwrap_or("component"));
                    }
                }
            }
        }
        Ok(())
    }

    /// Restores what [`Components::save`] wrote into tables with the same
    /// components registered and no entities.
    ///
    /// # Errors
    /// Why the snapshot does not fit these tables.
    pub fn load(&mut self, d: &mut Decoder<'_>) -> Result<(), String> {
        let text = |e: DecodeError| e.to_string();
        if self.entities.iter_live().next().is_some() {
            return Err("restore needs tables without entities".to_owned());
        }
        self.change_tick = Tick(d.u64().map_err(text)?);
        let live = self.entities.load(d).map_err(text)?;
        self.archetypes = vec![Archetype::new(ComponentSet::EMPTY, Vec::new(), Vec::new())];
        self.by_set = BTreeMap::from([(ComponentSet::EMPTY, 0)]);
        let count = d.u32().map_err(text)?;
        for index in 0..count {
            let n = usize::from(d.u16().map_err(text)?);
            let mut set = ComponentSet::EMPTY;
            for _ in 0..n {
                let len = usize::from(d.u16().map_err(text)?);
                let name = core::str::from_utf8(d.take(len).map_err(text)?)
                    .map_err(|_| "component name is not UTF-8".to_owned())?;
                let id = self
                    .registry
                    .by_name(name)
                    .ok_or_else(|| format!("snapshot component `{name}` is not registered"))?;
                set.insert(id);
            }
            let a = self.archetype_for(set).map_err(|e| e.to_string())?;
            if a != index {
                return Err(format!("archetype {index} restored at {a}"));
            }
            let rows = d.u32().map_err(text)?;
            for row in 0..rows {
                let id = EntityId::from_bits(d.u64().map_err(text)?);
                let arch = self
                    .archetypes
                    .get_mut(a as usize)
                    .ok_or_else(|| "restored archetype".to_owned())?;
                for col in &mut arch.columns {
                    col.load_row(d).map_err(text)?;
                }
                arch.entities.push(id);
                if !self.entities.place(id, Location { archetype: a, row }) {
                    return Err(format!("entity {id} is not live in the snapshot allocator"));
                }
            }
        }
        if u32::try_from(live.len()).ok() != Some(self.entities.live()) {
            return Err("snapshot rows do not match its live entities".to_owned());
        }
        Ok(())
    }

    /// Number of live entities.
    #[must_use]
    pub fn len(&self) -> u32 {
        self.entities.live()
    }

    /// True when no entity is live.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entities.live() == 0
    }

    /// Number of archetypes created so far.
    #[must_use]
    pub fn archetype_count(&self) -> usize {
        self.archetypes.len()
    }

    /// True when `id` refers to a live entity.
    #[must_use]
    pub fn is_alive(&self, id: EntityId) -> bool {
        self.entities.location(id).is_some()
    }

    fn archetype_for(&mut self, set: ComponentSet) -> Result<u32, EcsError> {
        if let Some(a) = self.by_set.get(&set) {
            return Ok(*a);
        }
        let index =
            u32::try_from(self.archetypes.len()).map_err(|_| EcsError::Internal("archetype count"))?;
        let ids: Vec<ComponentId> = set.iter().collect();
        let mut columns: Vec<Box<dyn Column>> = Vec::with_capacity(ids.len());
        for id in &ids {
            let info = self
                .registry
                .info(*id)
                .ok_or(EcsError::Internal("archetype component info"))?;
            columns.push((info.new_column)());
        }
        let mut created = Archetype::new(set, ids, columns);
        // Link the transition graph eagerly: every existing archetype that
        // differs by exactly one component gets its add/remove edge now, at
        // creation time, so moving entities between existing archetypes on
        // the hot path never allocates edge-cache nodes.
        for (other_index, other) in (0u32..).zip(self.archetypes.iter_mut()) {
            let diff = set.symmetric_difference(&other.set);
            let mut members = diff.iter();
            let (Some(c), None) = (members.next(), members.next()) else {
                continue;
            };
            if set.contains(c) {
                other.add_edges.insert(c, index);
                created.remove_edges.insert(c, other_index);
            } else {
                other.remove_edges.insert(c, index);
                created.add_edges.insert(c, other_index);
            }
        }
        self.archetypes.push(created);
        self.by_set.insert(set, index);
        Ok(index)
    }

    fn edge(&mut self, from: u32, id: ComponentId, add: bool) -> Result<u32, EcsError> {
        let arch = self
            .archetypes
            .get(from as usize)
            .ok_or(EcsError::Internal("edge source"))?;
        let cached = if add {
            arch.add_edges.get(&id)
        } else {
            arch.remove_edges.get(&id)
        };
        if let Some(to) = cached {
            return Ok(*to);
        }
        let mut set = arch.set;
        if add {
            set.insert(id);
        } else {
            set.remove(id);
        }
        let to = self.archetype_for(set)?;
        let arch = self
            .archetypes
            .get_mut(from as usize)
            .ok_or(EcsError::Internal("edge source"))?;
        if add {
            arch.add_edges.insert(id, to);
        } else {
            arch.remove_edges.insert(id, to);
        }
        Ok(to)
    }

    /// Pre-sizes the archetype of bundle `B` for `additional` more entities, so
    /// that spawning them later does not allocate. Also reserves entity slots.
    ///
    /// # Errors
    /// As [`Components::spawn`] for an invalid bundle.
    pub fn reserve<B: Bundle>(&mut self, additional: usize) -> Result<(), EcsError> {
        let set = B::component_set(&self.registry)?;
        let a = self.archetype_for(set)?;
        if let Some(arch) = self.archetypes.get_mut(a as usize) {
            arch.reserve(additional);
        }
        self.entities.reserve(additional);
        Ok(())
    }

    /// Spawns an entity with the components in `bundle`.
    ///
    /// # Errors
    /// [`EcsError::Unregistered`] for an unregistered component,
    /// [`EcsError::DuplicateComponent`] for a component listed twice,
    /// [`EcsError::EntityLimit`] when every index is in use.
    pub fn spawn<B: Bundle>(&mut self, bundle: B) -> Result<EntityId, EcsError> {
        let set = B::component_set(&self.registry)?;
        let a = self.archetype_for(set)?;
        let tick = self.change_tick;
        let arch = self
            .archetypes
            .get_mut(a as usize)
            .ok_or(EcsError::Internal("spawn archetype"))?;
        let row = u32::try_from(arch.len()).map_err(|_| EcsError::EntityLimit)?;
        if let Err(e) = bundle.push_into(&self.registry, arch, tick) {
            arch.truncate_columns();
            return Err(e);
        }
        let Some(id) = self.entities.alloc(Location { archetype: a, row }) else {
            arch.truncate_columns();
            return Err(EcsError::EntityLimit);
        };
        arch.entities.push(id);
        Ok(id)
    }

    /// Fixes the location of whichever entity was swapped into `row` of
    /// archetype `a` by a swap-remove.
    fn fix_swapped(&mut self, a: u32, row: u32) {
        let moved = self
            .archetypes
            .get(a as usize)
            .and_then(|arch| arch.entities.get(row as usize))
            .copied();
        if let Some(moved) = moved {
            self.entities.set_location(moved, Location { archetype: a, row });
        }
    }

    /// Despawns `id`, dropping its components and bumping its generation.
    ///
    /// # Errors
    /// [`EcsError::NoSuchEntity`] if `id` is not live.
    pub fn despawn(&mut self, id: EntityId) -> Result<(), EcsError> {
        let loc = self.entities.location(id).ok_or(NO_ENTITY(id))?;
        let arch = self
            .archetypes
            .get_mut(loc.archetype as usize)
            .ok_or(EcsError::Internal("despawn archetype"))?;
        let row = loc.row as usize;
        if row >= arch.entities.len() {
            return Err(EcsError::Internal("despawn row"));
        }
        for col in &mut arch.columns {
            col.swap_remove_drop(row);
        }
        arch.entities.swap_remove(row);
        self.fix_swapped(loc.archetype, loc.row);
        self.entities.free(id);
        Ok(())
    }

    /// Moves the row of `id` from archetype `loc.archetype` to `dst`, carrying
    /// every component `dst` also has and dropping the others, except `taken`,
    /// whose value the caller has already swap-removed from this row.
    /// Returns the new row.
    fn move_row(
        &mut self,
        id: EntityId,
        loc: Location,
        dst: u32,
        taken: Option<ComponentId>,
    ) -> Result<u32, EcsError> {
        let [src_a, dst_a] = self
            .archetypes
            .get_disjoint_mut([loc.archetype as usize, dst as usize])
            .map_err(|_| EcsError::Internal("move archetypes"))?;
        let row = loc.row as usize;
        if row >= src_a.entities.len() {
            return Err(EcsError::Internal("move row"));
        }
        // Validate first, so a failure cannot leave columns half-moved.
        for (cid, col) in src_a.ids.iter().zip(&src_a.columns) {
            if Some(*cid) != taken
                && (col.len() <= row || (dst_a.set.contains(*cid) && dst_a.column_index(*cid).is_none()))
            {
                return Err(EcsError::Internal("move column"));
            }
        }
        let new_row = u32::try_from(dst_a.entities.len()).map_err(|_| EcsError::EntityLimit)?;
        for (cid, col) in src_a.ids.iter().zip(src_a.columns.iter_mut()) {
            if Some(*cid) == taken {
                continue;
            }
            if let Some(dst_col) = dst_a.column_mut(*cid) {
                col.swap_remove_into(row, dst_col);
            } else {
                col.swap_remove_drop(row);
            }
        }
        src_a.entities.swap_remove(row);
        dst_a.entities.push(id);
        self.fix_swapped(loc.archetype, loc.row);
        self.entities.set_location(
            id,
            Location {
                archetype: dst,
                row: new_row,
            },
        );
        Ok(new_row)
    }

    /// Adds `value` to `id`, or replaces the existing `T` (recording a change).
    ///
    /// # Errors
    /// [`EcsError::NoSuchEntity`], [`EcsError::Unregistered`].
    pub fn insert<T: Component>(&mut self, id: EntityId, value: T) -> Result<(), EcsError> {
        let cid = self.registry.require::<T>()?;
        let loc = self.entities.location(id).ok_or(NO_ENTITY(id))?;
        let tick = self.change_tick;
        let src = self
            .archetypes
            .get_mut(loc.archetype as usize)
            .ok_or(EcsError::Internal("insert archetype"))?;
        if let Some(col) = src.column_mut(cid) {
            let col = typed_mut::<T>(col).ok_or(EcsError::Internal("insert column type"))?;
            let row = loc.row as usize;
            let (slot, changed) = col
                .data
                .get_mut(row)
                .zip(col.changed.get_mut(row))
                .ok_or(EcsError::Internal("insert row"))?;
            *slot = value;
            *changed = tick;
            return Ok(());
        }
        let dst = self.edge(loc.archetype, cid, true)?;
        self.move_row(id, loc, dst, None)?;
        let dst_arch = self
            .archetypes
            .get_mut(dst as usize)
            .ok_or(EcsError::Internal("insert dst"))?;
        // The moved row is the last; the new column is the only short one.
        push_one(&self.registry, dst_arch, value, tick)
    }

    /// Removes and returns `T` from `id`; `Ok(None)` if it had none.
    ///
    /// # Errors
    /// [`EcsError::NoSuchEntity`], [`EcsError::Unregistered`].
    pub fn remove<T: Component>(&mut self, id: EntityId) -> Result<Option<T>, EcsError> {
        let cid = self.registry.require::<T>()?;
        let loc = self.entities.location(id).ok_or(NO_ENTITY(id))?;
        let src = self
            .archetypes
            .get(loc.archetype as usize)
            .ok_or(EcsError::Internal("remove archetype"))?;
        if !src.set.contains(cid) {
            return Ok(None);
        }
        let dst = self.edge(loc.archetype, cid, false)?;
        // Take the value out first (same row in every column), then move the rest.
        let src = self
            .archetypes
            .get_mut(loc.archetype as usize)
            .ok_or(EcsError::Internal("remove archetype"))?;
        if (loc.row as usize) >= src.entities.len() {
            return Err(EcsError::Internal("remove row"));
        }
        let col = src.column_mut(cid).ok_or(EcsError::Internal("remove column"))?;
        let (value, _) = typed_mut::<T>(col)
            .and_then(|c| c.swap_remove(loc.row as usize))
            .ok_or(EcsError::Internal("remove value"))?;
        self.move_row(id, loc, dst, Some(cid))?;
        Ok(Some(value))
    }

    /// Shared access to `T` on `id`.
    #[must_use]
    pub fn get<T: Component>(&self, id: EntityId) -> Option<&T> {
        let cid = self.registry.id_of::<T>()?;
        let loc = self.entities.location(id)?;
        let col = self.archetypes.get(loc.archetype as usize)?.column(cid)?;
        typed::<T>(col)?.data.get(loc.row as usize)
    }

    /// Exclusive, change-tracked access to `T` on `id`.
    pub fn get_mut<T: Component>(&mut self, id: EntityId) -> Option<Mut<'_, T>> {
        let cid = self.registry.id_of::<T>()?;
        let loc = self.entities.location(id)?;
        let tick = self.change_tick;
        let col = self.archetypes.get_mut(loc.archetype as usize)?.column_mut(cid)?;
        let col = typed_mut::<T>(col)?;
        let row = loc.row as usize;
        let (value, changed) = col.data.get_mut(row).zip(col.changed.get_mut(row))?;
        Some(Mut::new(value, changed, tick))
    }

    /// The tick at which `T` on `id` last changed (spawn and insert count).
    #[must_use]
    pub fn changed_tick<T: Component>(&self, id: EntityId) -> Option<Tick> {
        let cid = self.registry.id_of::<T>()?;
        let loc = self.entities.location(id)?;
        self.archetypes
            .get(loc.archetype as usize)?
            .column(cid)?
            .changed_at(loc.row as usize)
    }

    /// True when `id` is live and has `T`.
    #[must_use]
    pub fn has<T: Component>(&self, id: EntityId) -> bool {
        self.get::<T>(id).is_some()
    }

    /// Compiles a query with no filters.
    ///
    /// # Errors
    /// See [`QueryBuilder::build`].
    pub fn query<D: QueryData>(&self) -> Result<Query<D>, EcsError> {
        QueryBuilder::new(self).build()
    }

    /// Starts a query with filters.
    #[must_use]
    pub fn query_builder<D: QueryData>(&self) -> QueryBuilder<'_, D> {
        QueryBuilder::new(self)
    }

    /// Live entities in ascending index order, with their component sets.
    pub fn iter_entities(&self) -> impl Iterator<Item = (EntityId, ComponentSet)> + '_ {
        self.entities
            .iter_live()
            .filter_map(|(id, loc)| self.archetypes.get(loc.archetype as usize).map(|a| (id, a.set)))
    }

    /// Feeds the components of `id` to `h`: for each component in ascending
    /// [`ComponentId`] order, the FNV-1a hash of its name as `u64`, then its
    /// [`crate::hash::StateHash`] encoding. False if `id` is not live.
    pub(crate) fn hash_entity(&self, id: EntityId, h: &mut StableHasher) -> bool {
        let Some(loc) = self.entities.location(id) else {
            return false;
        };
        let Some(arch) = self.archetypes.get(loc.archetype as usize) else {
            return false;
        };
        for (cid, col) in arch.ids.iter().zip(&arch.columns) {
            let name = self.registry.name(*cid).unwrap_or("");
            h.write_u64(crate::rng::Salt::named(name).get());
            if !col.hash_row(loc.row as usize, h) {
                return false;
            }
        }
        true
    }

    /// Feeds the whole entity state to `h`. The encoding (version 1) is part
    /// of the replay contract:
    ///
    /// 1. the number of live entities as `u64`;
    /// 2. for each live entity in ascending index order: its id bits as `u64`,
    ///    its component count as `u32`, then `Components::hash_entity`'s
    ///    encoding of its components.
    ///
    /// Independent of archetype layout and creation order. It depends on
    /// component registration order only through the order components are
    /// listed in, which is deterministic for a given module set. Allocation-free.
    pub fn state_hash(&self, h: &mut StableHasher) {
        h.write_u64(u64::from(self.entities.live()));
        for (id, loc) in self.entities.iter_live() {
            h.write_u64(id.to_bits());
            let count = self
                .archetypes
                .get(loc.archetype as usize)
                .map_or(0, |a| a.ids.len());
            h.write_u32(u32::try_from(count).unwrap_or(u32::MAX));
            self.hash_entity(id, h);
        }
    }

    /// Every registered component name, in registration order (the Ops
    /// inspector's component list).
    #[must_use]
    pub fn component_names(&self) -> Vec<&'static str> {
        (0..self.registry.len())
            .filter_map(|i| u16::try_from(i).ok())
            .filter_map(|i| self.registry.name(ComponentId(i)))
            .collect()
    }

    /// One page of the live entities that have component `name`, in
    /// ascending index order, each with every component's inspector text
    /// (in registration order). `None` when no component has that name.
    /// Allocates: an Ops tool, never called from a tick.
    #[must_use]
    pub fn inspect(&self, name: &str, offset: usize, limit: usize) -> Option<InspectPage> {
        let id = self.registry.by_name(name)?;
        let mut page = InspectPage {
            total: 0,
            entities: Vec::with_capacity(limit.min(256)),
        };
        for (entity, loc) in self.entities.iter_live() {
            let Some(arch) = self.archetypes.get(loc.archetype as usize) else {
                continue;
            };
            if !arch.set.contains(id) {
                continue;
            }
            page.total += 1;
            if page.total <= offset || page.entities.len() >= limit {
                continue;
            }
            let mut components = Vec::with_capacity(arch.ids.len());
            for (cid, col) in arch.ids.iter().zip(&arch.columns) {
                let mut text = String::new();
                if col.inspect_row(loc.row as usize, &mut text) {
                    components.push((self.registry.name(*cid).unwrap_or("?"), text));
                }
            }
            page.entities.push(InspectedEntity {
                id: entity,
                components,
            });
        }
        Some(page)
    }

    /// Entity slots retired because their generation was exhausted.
    #[must_use]
    pub fn retired_slots(&self) -> u32 {
        self.entities.retired()
    }
}

/// The simulation world of one cell (or one client): entity tables and
/// resources. Fields are public so a system can borrow both halves at once.
#[derive(Default)]
pub struct World {
    /// Entities and components.
    pub components: Components,
    /// Typed singletons.
    pub resources: Resources,
}

impl World {
    /// An empty world.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// The world state hash, the value replay compares every tick.
    ///
    /// Encoding (version 1, part of the replay contract): the `u64` format tag
    /// `WORLD_HASH_V1`, then [`Components::state_hash`], then
    /// [`Resources::state_hash`], all into one XXH64 [`StableHasher`].
    /// Allocation-free.
    #[must_use]
    pub fn state_hash(&self) -> u64 {
        let mut h = StableHasher::new();
        h.write_u64(WORLD_HASH_V1);
        self.components.state_hash(&mut h);
        self.resources.state_hash(&mut h);
        h.finish()
    }

    /// Writes the whole world into a snapshot: entities, then resources.
    ///
    /// # Errors
    /// The name of the first component or resource without snapshot support.
    pub fn save(&self, e: &mut Encoder<'_>) -> Result<(), &'static str> {
        self.components.save(e)?;
        self.resources.save(e)
    }

    /// Restores a snapshot into a world built the same way, with no
    /// entities. Its state hash then equals the saved world's.
    ///
    /// # Errors
    /// Why the snapshot does not fit this world.
    pub fn load(&mut self, d: &mut Decoder<'_>) -> Result<(), String> {
        self.components.load(d)?;
        self.resources.load(d)
    }

    /// Registers component `T`. See [`Components::register`].
    ///
    /// # Errors
    /// As [`Components::register`].
    pub fn register<T: Component>(&mut self) -> Result<ComponentId, EcsError> {
        self.components.register::<T>()
    }

    /// Spawns an entity. See [`Components::spawn`].
    ///
    /// # Errors
    /// As [`Components::spawn`].
    pub fn spawn<B: Bundle>(&mut self, bundle: B) -> Result<EntityId, EcsError> {
        self.components.spawn(bundle)
    }

    /// Despawns an entity. See [`Components::despawn`].
    ///
    /// # Errors
    /// As [`Components::despawn`].
    pub fn despawn(&mut self, id: EntityId) -> Result<(), EcsError> {
        self.components.despawn(id)
    }

    /// Adds or replaces a component. See [`Components::insert`].
    ///
    /// # Errors
    /// As [`Components::insert`].
    pub fn insert<T: Component>(&mut self, id: EntityId, value: T) -> Result<(), EcsError> {
        self.components.insert(id, value)
    }

    /// Removes a component. See [`Components::remove`].
    ///
    /// # Errors
    /// As [`Components::remove`].
    pub fn remove<T: Component>(&mut self, id: EntityId) -> Result<Option<T>, EcsError> {
        self.components.remove(id)
    }

    /// Shared component access.
    #[must_use]
    pub fn get<T: Component>(&self, id: EntityId) -> Option<&T> {
        self.components.get(id)
    }

    /// Exclusive, change-tracked component access.
    pub fn get_mut<T: Component>(&mut self, id: EntityId) -> Option<Mut<'_, T>> {
        self.components.get_mut(id)
    }

    /// True when `id` is live.
    #[must_use]
    pub fn is_alive(&self, id: EntityId) -> bool {
        self.components.is_alive(id)
    }

    /// Compiles a query. See [`Components::query`].
    ///
    /// # Errors
    /// As [`Components::query`].
    pub fn query<D: QueryData>(&self) -> Result<Query<D>, EcsError> {
        self.components.query()
    }

    /// Stores a resource. See [`Resources::insert`].
    ///
    /// # Errors
    /// As [`Resources::insert`].
    pub fn insert_resource<R: Resource>(&mut self, value: R) -> Result<Option<R>, EcsError> {
        self.resources.insert(value)
    }

    /// Shared resource access.
    #[must_use]
    pub fn resource<R: Resource>(&self) -> Option<&R> {
        self.resources.get()
    }

    /// Exclusive resource access.
    pub fn resource_mut<R: Resource>(&mut self) -> Option<&mut R> {
        self.resources.get_mut()
    }
}

/// One entity as the Ops inspector shows it.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct InspectedEntity {
    /// The entity.
    pub id: EntityId,
    /// Each component's name and inspector text, in registration order.
    pub components: Vec<(&'static str, String)>,
}

/// A page of [`Components::inspect`].
#[derive(Clone, PartialEq, Eq, Debug, Default)]
pub struct InspectPage {
    /// Live entities with the component, in all.
    pub total: usize,
    /// This page.
    pub entities: Vec<InspectedEntity>,
}

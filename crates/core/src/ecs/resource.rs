//! Resources: typed singletons stored beside the entity tables.

use core::any::{Any, TypeId};
use std::collections::BTreeMap;

use super::EcsError;
use super::component::{MAX_COMPONENTS, ResourceId};
use crate::hash::{StableHasher, StateHash};
use crate::rng::Salt;
use crate::wire::{DecodeError, Decoder, Encoder};

/// A typed singleton (configuration, the ground model, event buffers, ...).
///
/// Resources implement [`StateHash`] so that the world state hash covers
/// every piece of simulation state. A resource that is not simulation state
/// (for example tuning loaded from content, already covered by the content
/// hash) implements it by writing nothing, explicitly.
pub trait Resource: 'static + Send + Sync + StateHash {
    /// Stable, unique, dotted name, for example `"core.heightfield"`.
    const NAME: &'static str;

    /// Writes this resource into a snapshot (decision 0007). A snapshot is
    /// restored into a world freshly built the same way (cell, modules,
    /// content), so a resource that installation rebuilds exactly (content,
    /// configuration, an output drained every tick) answers
    /// [`Saved::Rebuilt`] and writes nothing. The default refuses, so a
    /// world holding simulation state without snapshot support cannot be
    /// snapshotted by accident.
    fn save(&self, _e: &mut Encoder<'_>) -> Saved {
        Saved::Unsupported
    }

    /// Restores, in place, the state [`Resource::save`] wrote.
    ///
    /// # Errors
    /// The bytes are not that state.
    fn load(&mut self, _d: &mut Decoder<'_>) -> Result<(), DecodeError> {
        Err(DecodeError::Invalid("resource has no snapshot support"))
    }
}

/// What [`Resource::save`] did.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Saved {
    /// The state was written.
    Written,
    /// Nothing to write: building the world again rebuilds it exactly.
    Rebuilt,
    /// The resource cannot be snapshotted.
    Unsupported,
}

type HashFn = fn(&dyn Any, &mut StableHasher);
type SaveFn = fn(&dyn Any, &mut Encoder<'_>) -> Saved;
type LoadFn = fn(&mut dyn Any, &mut Decoder<'_>) -> Result<(), DecodeError>;

struct Slot {
    name: &'static str,
    hash: HashFn,
    save: SaveFn,
    load: LoadFn,
    value: Option<Box<dyn Any + Send + Sync>>,
}

fn hash_as<R: Resource>(value: &dyn Any, h: &mut StableHasher) {
    if let Some(r) = value.downcast_ref::<R>() {
        r.state_hash(h);
    }
}

fn save_as<R: Resource>(value: &dyn Any, e: &mut Encoder<'_>) -> Saved {
    value
        .downcast_ref::<R>()
        .map_or(Saved::Unsupported, |r| r.save(e))
}

fn load_as<R: Resource>(value: &mut dyn Any, d: &mut Decoder<'_>) -> Result<(), DecodeError> {
    value
        .downcast_mut::<R>()
        .ok_or(DecodeError::Invalid("resource type"))?
        .load(d)
}

/// Typed resource storage. Each resource type gets a dense [`ResourceId`] on
/// first registration; the slot keeps that id even while the value is absent,
/// so inserting and removing a value never allocates registry nodes.
#[derive(Default)]
pub struct Resources {
    slots: Vec<Slot>,
    by_type: BTreeMap<TypeId, ResourceId>,
    by_name: BTreeMap<&'static str, ResourceId>,
}

impl Resources {
    /// Registers `R` without a value and returns its id. Idempotent.
    ///
    /// # Errors
    /// [`EcsError::NameClash`] if another type uses `R::NAME`;
    /// [`EcsError::TooManyComponents`] past [`MAX_COMPONENTS`] resource types.
    pub fn register<R: Resource>(&mut self) -> Result<ResourceId, EcsError> {
        if let Some(id) = self.by_type.get(&TypeId::of::<R>()) {
            return Ok(*id);
        }
        if self.by_name.contains_key(R::NAME) {
            return Err(EcsError::NameClash(R::NAME));
        }
        if self.slots.len() >= MAX_COMPONENTS {
            return Err(EcsError::TooManyComponents);
        }
        let id = ResourceId(u16::try_from(self.slots.len()).map_err(|_| EcsError::TooManyComponents)?);
        self.slots.push(Slot {
            name: R::NAME,
            hash: hash_as::<R>,
            save: save_as::<R>,
            load: load_as::<R>,
            value: None,
        });
        self.by_type.insert(TypeId::of::<R>(), id);
        self.by_name.insert(R::NAME, id);
        Ok(id)
    }

    /// Feeds every resource to `h` in ascending [`ResourceId`] order: the
    /// FNV-1a hash of its name as `u64`, then a tag byte (0 absent, 1
    /// present), then its [`StateHash`] encoding when present. Part of the
    /// world state encoding (version 1).
    pub fn state_hash(&self, h: &mut StableHasher) {
        for slot in &self.slots {
            h.write_u64(Salt::named(slot.name).get());
            match &slot.value {
                None => h.write_u8(0),
                Some(v) => {
                    h.write_u8(1);
                    let any: &dyn Any = &**v;
                    (slot.hash)(any, h);
                }
            }
        }
    }

    /// Writes every present resource that holds state, by name, each
    /// length-prefixed.
    ///
    /// # Errors
    /// The name of the first resource without snapshot support.
    pub fn save(&self, e: &mut Encoder<'_>) -> Result<(), &'static str> {
        let mut entries: Vec<(&'static str, Vec<u8>)> = Vec::new();
        for slot in &self.slots {
            let Some(v) = &slot.value else { continue };
            let mut bytes = Vec::new();
            let any: &dyn Any = &**v;
            match (slot.save)(any, &mut Encoder::new(&mut bytes)) {
                Saved::Written => entries.push((slot.name, bytes)),
                Saved::Rebuilt => {}
                Saved::Unsupported => return Err(slot.name),
            }
        }
        e.u32(u32::try_from(entries.len()).unwrap_or(u32::MAX));
        for (name, bytes) in entries {
            e.u16(u16::try_from(name.len()).unwrap_or(u16::MAX));
            e.bytes(name.as_bytes());
            e.u32(u32::try_from(bytes.len()).unwrap_or(u32::MAX));
            e.bytes(&bytes);
        }
        Ok(())
    }

    /// Restores, in place, every resource [`Resources::save`] wrote. Each
    /// must be present (the world was built the same way).
    ///
    /// # Errors
    /// The resource that is missing or did not read back, by name.
    pub fn load(&mut self, d: &mut Decoder<'_>) -> Result<(), String> {
        let n = d.u32().map_err(|e| e.to_string())?;
        for _ in 0..n {
            let len = usize::from(d.u16().map_err(|e| e.to_string())?);
            let name = core::str::from_utf8(d.take(len).map_err(|e| e.to_string())?)
                .map_err(|_| "resource name is not UTF-8".to_owned())?
                .to_owned();
            let size = d.u32().map_err(|e| e.to_string())? as usize;
            let bytes = d.take(size).map_err(|e| e.to_string())?;
            let id = self
                .by_name
                .get(name.as_str())
                .copied()
                .ok_or_else(|| format!("snapshot resource `{name}` is not in this world"))?;
            let slot = self
                .slots
                .get_mut(id.index())
                .ok_or_else(|| format!("resource `{name}` slot"))?;
            let load = slot.load;
            let value = slot
                .value
                .as_deref_mut()
                .ok_or_else(|| format!("snapshot resource `{name}` is absent in this world"))?;
            let any: &mut dyn Any = value;
            let mut inner = Decoder::new(bytes);
            load(any, &mut inner).map_err(|e| format!("resource `{name}`: {e}"))?;
            inner.finish().map_err(|e| format!("resource `{name}`: {e}"))?;
        }
        Ok(())
    }

    /// The id of `R`, if registered.
    #[must_use]
    pub fn id<R: Resource>(&self) -> Option<ResourceId> {
        self.by_type.get(&TypeId::of::<R>()).copied()
    }

    /// The registered name of a resource id.
    #[must_use]
    pub fn name(&self, id: ResourceId) -> Option<&'static str> {
        self.slots.get(id.index()).map(|s| s.name)
    }

    /// Stores `value`, registering `R` if needed. Returns the previous value.
    ///
    /// # Errors
    /// As [`Resources::register`].
    pub fn insert<R: Resource>(&mut self, value: R) -> Result<Option<R>, EcsError> {
        let id = self.register::<R>()?;
        let slot = self
            .slots
            .get_mut(id.index())
            .ok_or(EcsError::Internal("resource slot"))?;
        let old = slot.value.replace(Box::new(value));
        Ok(old
            .and_then(|b| (b as Box<dyn Any>).downcast::<R>().ok())
            .map(|b| *b))
    }

    /// Removes and returns the value of `R`.
    pub fn remove<R: Resource>(&mut self) -> Option<R> {
        let id = self.id::<R>()?;
        let boxed = self.slots.get_mut(id.index())?.value.take()?;
        (boxed as Box<dyn Any>).downcast::<R>().ok().map(|b| *b)
    }

    /// Shared access to `R`.
    #[must_use]
    pub fn get<R: Resource>(&self) -> Option<&R> {
        let id = self.id::<R>()?;
        let value: &dyn Any = self.slots.get(id.index())?.value.as_deref()?;
        value.downcast_ref::<R>()
    }

    /// Exclusive access to `R`.
    pub fn get_mut<R: Resource>(&mut self) -> Option<&mut R> {
        let id = self.id::<R>()?;
        let value: &mut dyn Any = self.slots.get_mut(id.index())?.value.as_deref_mut()?;
        value.downcast_mut::<R>()
    }

    /// True when `R` currently has a value.
    #[must_use]
    pub fn contains<R: Resource>(&self) -> bool {
        self.get::<R>().is_some()
    }
}

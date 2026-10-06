//! Cross-module communication through contract crates (plan 13, decision
//! 0010). A module never calls another module's implementation; it uses the
//! types the other module's contract crate declares:
//!
//! - an [`Event`] is **queued**: sent during tick N into [`Events<T>`], read by
//!   any module during tick N + 1, whatever order systems run in, so
//!   delivery is deterministic and independent of registration order;
//! - a [`Query`] is **synchronous**: the implementing module registers one
//!   handler, and callers get its answer immediately through [`Queries`],
//!   or [`QueryError::FeatureDisabled`] while that module is switched off.
//!
//! Both live in the world as resources, so they are simulation state: event
//! queues are hashed, and so is which query providers are enabled.

use core::any::{Any, TypeId};
use core::fmt;
use std::collections::BTreeMap;

use crate::ecs::{Resource, Saved, World};
use crate::hash::{StableHasher, StateHash};
use crate::mem::BoundedVec;
use crate::wire::{DecodeError, Decoder, Encoder};

/// A queued cross-module event, declared in a module's contract crate.
pub trait Event: Copy + Send + Sync + StateHash + 'static {
    /// Stable dotted name, for example `"std.party.formed"`.
    const NAME: &'static str;

    /// Writes this event into a snapshot. The default refuses: a queue
    /// holding events without snapshot support cannot be snapshotted.
    fn save(&self, _e: &mut Encoder<'_>) -> bool {
        false
    }

    /// Reads an event written by [`Event::save`].
    ///
    /// # Errors
    /// The bytes are not one.
    fn load(_d: &mut Decoder<'_>) -> Result<Self, DecodeError> {
        Err(DecodeError::Invalid("event has no snapshot support"))
    }
}

/// The queue of one event type: what was sent last tick (readable now) and
/// what is being sent this tick. Bounded: a full queue refuses and counts.
pub struct Events<T: Event> {
    ready: BoundedVec<T>,
    pending: BoundedVec<T>,
    dropped: u64,
}

impl<T: Event> Events<T> {
    /// A queue holding up to `capacity` events per tick.
    #[must_use]
    pub fn with_capacity(capacity: usize) -> Self {
        Self {
            ready: BoundedVec::with_capacity(capacity),
            pending: BoundedVec::with_capacity(capacity),
            dropped: 0,
        }
    }

    /// Queues `event` for delivery next tick. Returns false (and counts the
    /// drop) when this tick's queue is full.
    pub fn send(&mut self, event: T) -> bool {
        if self.pending.push(event).is_ok() {
            true
        } else {
            self.dropped += 1;
            false
        }
    }

    /// The events sent last tick.
    #[must_use]
    pub fn read(&self) -> &[T] {
        &self.ready
    }

    /// Events refused because a tick's queue was full.
    #[must_use]
    pub fn dropped(&self) -> u64 {
        self.dropped
    }

    /// Tick boundary: last tick's events are consumed, this tick's become
    /// readable. Allocation-free.
    pub fn advance(&mut self) {
        core::mem::swap(&mut self.ready, &mut self.pending);
        self.pending.clear();
    }
}

impl<T: Event> StateHash for Events<T> {
    fn state_hash(&self, h: &mut StableHasher) {
        self.ready[..].state_hash(h);
        self.pending[..].state_hash(h);
        h.write_u64(self.dropped);
    }
}

impl<T: Event> Resource for Events<T> {
    const NAME: &'static str = T::NAME;

    /// Both queues, in order (the drop counter is not state).
    fn save(&self, e: &mut Encoder<'_>) -> Saved {
        for q in [&self.ready, &self.pending] {
            e.u32(u32::try_from(q.len()).unwrap_or(u32::MAX));
            for ev in q.iter() {
                if !ev.save(e) {
                    return Saved::Unsupported;
                }
            }
        }
        Saved::Written
    }

    fn load(&mut self, d: &mut Decoder<'_>) -> Result<(), DecodeError> {
        for q in [&mut self.ready, &mut self.pending] {
            q.clear();
            let n = d.u32()?;
            for _ in 0..n {
                q.push(T::load(d)?)
                    .map_err(|_| DecodeError::Invalid("event queue full"))?;
            }
        }
        Ok(())
    }
}

/// Advances the queue of `T` in `world` (a host calls one of these per
/// registered event type at each tick boundary).
pub fn advance_events<T: Event>(world: &mut World) {
    if let Some(q) = world.resource_mut::<Events<T>>() {
        q.advance();
    }
}

/// A synchronous cross-module question, declared in a module's contract
/// crate. The implementing module answers it.
pub trait Query: 'static {
    /// The answer.
    type Response: 'static;
    /// Stable dotted name, for example `"std.party.members"`.
    const NAME: &'static str;
}

/// A query handler: reads the world, answers.
pub type QueryFn<Q> = fn(&World, &Q) -> <Q as Query>::Response;

/// Why a query got no answer.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum QueryError {
    /// No module registered a handler (the contract is not implemented).
    NoProvider(&'static str),
    /// The implementing module is disabled.
    FeatureDisabled(&'static str),
}

impl fmt::Display for QueryError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NoProvider(q) => write!(f, "no module answers {q}"),
            Self::FeatureDisabled(q) => write!(f, "the module answering {q} is disabled"),
        }
    }
}

impl std::error::Error for QueryError {}

/// A query was registered twice.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct DuplicateQuery(pub &'static str);

impl fmt::Display for DuplicateQuery {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} has two providers", self.0)
    }
}

impl std::error::Error for DuplicateQuery {}

struct Provider {
    module: String,
    enabled: bool,
    handler: Box<dyn Any + Send + Sync>,
}

/// Every query handler of the running modules, by query type.
#[derive(Default)]
pub struct Queries {
    providers: BTreeMap<TypeId, Provider>,
    /// Provider keys in query-name order (for hashing without allocating).
    by_name: Vec<(&'static str, TypeId)>,
}

impl Queries {
    /// Registers `module`'s handler for `Q`.
    ///
    /// # Errors
    /// [`DuplicateQuery`] when `Q` already has a provider.
    pub fn provide<Q: Query>(&mut self, module: &str, handler: QueryFn<Q>) -> Result<(), DuplicateQuery> {
        let id = TypeId::of::<Q>();
        if self.providers.contains_key(&id) {
            return Err(DuplicateQuery(Q::NAME));
        }
        self.providers.insert(
            id,
            Provider {
                module: module.to_owned(),
                enabled: true,
                handler: Box::new(handler),
            },
        );
        self.by_name.push((Q::NAME, id));
        self.by_name.sort_unstable_by_key(|(name, _)| *name);
        Ok(())
    }

    /// Enables or disables every query `module` provides.
    pub fn set_enabled(&mut self, module: &str, enabled: bool) {
        for p in self.providers.values_mut().filter(|p| p.module == module) {
            p.enabled = enabled;
        }
    }

    /// Asks `Q`. Allocation-free.
    ///
    /// # Errors
    /// [`QueryError`] when nobody provides `Q` or its provider is disabled.
    pub fn ask<Q: Query>(&self, world: &World, query: &Q) -> Result<Q::Response, QueryError> {
        let p = self
            .providers
            .get(&TypeId::of::<Q>())
            .ok_or(QueryError::NoProvider(Q::NAME))?;
        if !p.enabled {
            return Err(QueryError::FeatureDisabled(Q::NAME));
        }
        let handler = p
            .handler
            .downcast_ref::<QueryFn<Q>>()
            .ok_or(QueryError::NoProvider(Q::NAME))?;
        Ok(handler(world, query))
    }
}

impl StateHash for Queries {
    fn state_hash(&self, h: &mut StableHasher) {
        // Which answers are available is simulation state; the handlers
        // themselves are code, covered by the build id. Hash in name order,
        // never TypeId order (which differs between builds).
        h.write_u64(self.by_name.len() as u64);
        for (name, id) in &self.by_name {
            h.write(name.as_bytes());
            h.write_u8(0);
            h.write_u8(u8::from(self.providers.get(id).is_some_and(|p| p.enabled)));
        }
    }
}

impl Resource for Queries {
    const NAME: &'static str = "core.module.queries";

    /// Providers are registered by installation; the enabled switches follow
    /// the module states, which are snapshotted.
    fn save(&self, _e: &mut Encoder<'_>) -> Saved {
        Saved::Rebuilt
    }
}

/// Asks `Q` through the world's [`Queries`] resource.
///
/// # Errors
/// [`QueryError`].
pub fn ask<Q: Query>(world: &World, query: &Q) -> Result<Q::Response, QueryError> {
    world
        .resource::<Queries>()
        .ok_or(QueryError::NoProvider(Q::NAME))?
        .ask(world, query)
}

//! Scopes (plan 8.8): `App → Login → Character → World`.
//!
//! Each scope owns its services and resources and disposes them, by dropping, in reverse
//! order of insertion when the scope exits. Exiting a scope exits every scope inside it
//! first, innermost first. No resource outlives its scope, and because resources are
//! owned values (not registrations in a global), there is nowhere for per-session state
//! to hide at application lifetime.
//!
//! Resources are typed: one value per type across the active stack, looked up by type.
//! Inner scopes see outer resources; outer scopes never see inner ones.

use std::any::{Any, TypeId};

/// The four scope levels, outermost first.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, PartialOrd, Ord)]
pub enum ScopeKind {
    /// Process lifetime: device, window, settings, content index.
    App,
    /// Connected and authenticating.
    Login,
    /// A chosen character, before entering a world.
    Character,
    /// In a world: sim thread, render world, streaming of world sectors.
    World,
}

impl ScopeKind {
    /// The scope that must be active before this one is entered.
    pub const fn parent(self) -> Option<ScopeKind> {
        match self {
            ScopeKind::App => None,
            ScopeKind::Login => Some(ScopeKind::App),
            ScopeKind::Character => Some(ScopeKind::Login),
            ScopeKind::World => Some(ScopeKind::Character),
        }
    }
}

/// Scope errors.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ScopeError {
    /// `requested` cannot be entered while `current` is the innermost scope.
    WrongOrder {
        /// Innermost active scope.
        current: Option<ScopeKind>,
        /// Scope requested.
        requested: ScopeKind,
    },
    /// The scope is not active.
    NotActive(ScopeKind),
    /// No scope is active.
    NoScope,
    /// A resource of this type already exists in an active scope.
    Duplicate {
        /// Type name of the resource.
        type_name: &'static str,
        /// Scope that owns the existing one.
        owner: ScopeKind,
    },
}

impl core::fmt::Display for ScopeError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            ScopeError::WrongOrder { current, requested } => {
                write!(f, "cannot enter {requested:?} with {current:?} innermost")
            }
            ScopeError::NotActive(k) => write!(f, "scope {k:?} is not active"),
            ScopeError::NoScope => f.write_str("no scope is active"),
            ScopeError::Duplicate { type_name, owner } => {
                write!(f, "resource {type_name} already owned by scope {owner:?}")
            }
        }
    }
}

impl std::error::Error for ScopeError {}

struct Entry {
    type_id: TypeId,
    type_name: &'static str,
    value: Box<dyn Any + Send>,
}

struct Scope {
    kind: ScopeKind,
    entries: Vec<Entry>,
}

impl Drop for Scope {
    fn drop(&mut self) {
        // Reverse insertion order: later resources may depend on earlier ones.
        while let Some(e) = self.entries.pop() {
            drop(e.value);
        }
    }
}

/// The active scope stack.
#[derive(Default)]
pub struct ScopeStack {
    scopes: Vec<Scope>,
}

impl core::fmt::Debug for ScopeStack {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        let mut l = f.debug_list();
        for s in &self.scopes {
            l.entry(&(s.kind, s.entries.iter().map(|e| e.type_name).collect::<Vec<_>>()));
        }
        l.finish()
    }
}

impl Drop for ScopeStack {
    fn drop(&mut self) {
        // Innermost first; Vec's own drop would run outermost first.
        while let Some(s) = self.scopes.pop() {
            drop(s);
        }
    }
}

impl ScopeStack {
    /// An empty stack.
    pub fn new() -> Self {
        Self::default()
    }

    /// The innermost active scope.
    pub fn current(&self) -> Option<ScopeKind> {
        self.scopes.last().map(|s| s.kind)
    }

    /// True when `kind` is active.
    pub fn is_active(&self, kind: ScopeKind) -> bool {
        self.scopes.iter().any(|s| s.kind == kind)
    }

    /// Enters `kind`, which must be the child of the innermost scope (or `App` on an
    /// empty stack).
    ///
    /// # Errors
    /// [`ScopeError::WrongOrder`].
    pub fn enter(&mut self, kind: ScopeKind) -> Result<(), ScopeError> {
        if kind.parent() != self.current() {
            return Err(ScopeError::WrongOrder {
                current: self.current(),
                requested: kind,
            });
        }
        self.scopes.push(Scope {
            kind,
            entries: Vec::new(),
        });
        Ok(())
    }

    /// Exits `kind` and every scope inside it, innermost first, each disposing its
    /// resources in reverse insertion order.
    ///
    /// # Errors
    /// [`ScopeError::NotActive`].
    pub fn exit(&mut self, kind: ScopeKind) -> Result<(), ScopeError> {
        if !self.is_active(kind) {
            return Err(ScopeError::NotActive(kind));
        }
        while let Some(s) = self.scopes.pop() {
            let done = s.kind == kind;
            drop(s);
            if done {
                break;
            }
        }
        Ok(())
    }

    /// Moves `value` into the innermost scope.
    ///
    /// # Errors
    /// [`ScopeError::NoScope`], or [`ScopeError::Duplicate`] if any active scope already
    /// owns a `T` (shadowing would make "which one" depend on scope depth).
    pub fn insert<T: Any + Send>(&mut self, value: T) -> Result<(), ScopeError> {
        let type_id = TypeId::of::<T>();
        let type_name = std::any::type_name::<T>();
        if let Some(owner) = self.owner_of_id(type_id) {
            return Err(ScopeError::Duplicate { type_name, owner });
        }
        let top = self.scopes.last_mut().ok_or(ScopeError::NoScope)?;
        top.entries.push(Entry {
            type_id,
            type_name,
            value: Box::new(value),
        });
        Ok(())
    }

    fn owner_of_id(&self, id: TypeId) -> Option<ScopeKind> {
        self.scopes
            .iter()
            .find(|s| s.entries.iter().any(|e| e.type_id == id))
            .map(|s| s.kind)
    }

    /// The scope owning the `T`, if any.
    pub fn owner_of<T: Any>(&self) -> Option<ScopeKind> {
        self.owner_of_id(TypeId::of::<T>())
    }

    /// The `T` in any active scope.
    pub fn get<T: Any>(&self) -> Option<&T> {
        let id = TypeId::of::<T>();
        self.scopes
            .iter()
            .flat_map(|s| s.entries.iter())
            .find(|e| e.type_id == id)
            .and_then(|e| e.value.downcast_ref::<T>())
    }

    /// The `T` in any active scope, mutably.
    pub fn get_mut<T: Any>(&mut self) -> Option<&mut T> {
        let id = TypeId::of::<T>();
        self.scopes
            .iter_mut()
            .flat_map(|s| s.entries.iter_mut())
            .find(|e| e.type_id == id)
            .and_then(|e| e.value.downcast_mut::<T>())
    }

    /// Resources owned by `kind`, by type name, in insertion order (diagnostics).
    pub fn resource_names(&self, kind: ScopeKind) -> Vec<&'static str> {
        self.scopes
            .iter()
            .filter(|s| s.kind == kind)
            .flat_map(|s| s.entries.iter().map(|e| e.type_name))
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::TestResult;
    use std::sync::{Arc, Mutex, PoisonError};

    type Log = Arc<Mutex<Vec<&'static str>>>;

    /// A resource that logs its disposal and holds a leak tracker.
    struct Res<const N: usize> {
        name: &'static str,
        log: Log,
        _tracker: Arc<()>,
    }

    impl<const N: usize> Drop for Res<N> {
        fn drop(&mut self) {
            self.log
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .push(self.name);
        }
    }

    fn res<const N: usize>(name: &'static str, log: &Log, tracker: &Arc<()>) -> Res<N> {
        Res {
            name,
            log: Arc::clone(log),
            _tracker: Arc::clone(tracker),
        }
    }

    fn take(log: &Log) -> Vec<&'static str> {
        core::mem::take(&mut *log.lock().unwrap_or_else(PoisonError::into_inner))
    }

    #[test]
    fn scopes_enter_in_order_only() -> TestResult {
        let mut s = ScopeStack::new();
        assert_eq!(
            s.enter(ScopeKind::Login),
            Err(ScopeError::WrongOrder {
                current: None,
                requested: ScopeKind::Login
            })
        );
        s.enter(ScopeKind::App)?;
        assert!(s.enter(ScopeKind::World).is_err());
        s.enter(ScopeKind::Login)?;
        s.enter(ScopeKind::Character)?;
        s.enter(ScopeKind::World)?;
        assert!(s.enter(ScopeKind::World).is_err());
        assert_eq!(s.current(), Some(ScopeKind::World));
        Ok(())
    }

    #[test]
    fn exit_disposes_in_reverse_and_leaks_nothing() -> TestResult {
        let log: Log = Arc::default();
        let app_tracker = Arc::new(());
        let session_tracker = Arc::new(());
        let mut s = ScopeStack::new();
        s.enter(ScopeKind::App)?;
        s.insert(res::<0>("app.device", &log, &app_tracker))?;
        s.enter(ScopeKind::Login)?;
        s.insert(res::<1>("login.connection", &log, &session_tracker))?;
        s.insert(res::<2>("login.token", &log, &session_tracker))?;
        s.enter(ScopeKind::Character)?;
        s.insert(res::<3>("character.roster", &log, &session_tracker))?;
        s.enter(ScopeKind::World)?;
        s.insert(res::<4>("world.sim", &log, &session_tracker))?;
        s.insert(res::<5>("world.render_world", &log, &session_tracker))?;
        assert_eq!(Arc::strong_count(&session_tracker), 6);

        // Inner scopes see outer resources.
        assert_eq!(s.get::<Res<0>>().map(|r| r.name), Some("app.device"));

        // Exiting Login exits World and Character first, each in reverse insertion order.
        s.exit(ScopeKind::Login)?;
        assert_eq!(
            take(&log),
            vec![
                "world.render_world",
                "world.sim",
                "character.roster",
                "login.token",
                "login.connection"
            ]
        );
        assert_eq!(
            Arc::strong_count(&session_tracker),
            1,
            "no session resource survives its scope"
        );
        assert_eq!(Arc::strong_count(&app_tracker), 2, "app resources are untouched");
        assert_eq!(s.current(), Some(ScopeKind::App));
        assert!(s.get::<Res<4>>().is_none() && s.get::<Res<1>>().is_none());

        // Re-entering starts clean: nothing from the previous session is reachable.
        s.enter(ScopeKind::Login)?;
        assert!(s.resource_names(ScopeKind::Login).is_empty());
        assert!(s.get::<Res<2>>().is_none());

        drop(s);
        assert_eq!(take(&log), vec!["app.device"]);
        assert_eq!(Arc::strong_count(&app_tracker), 1);
        Ok(())
    }

    #[test]
    fn dropping_the_stack_disposes_innermost_first() -> TestResult {
        let log: Log = Arc::default();
        let t = Arc::new(());
        let mut s = ScopeStack::new();
        s.enter(ScopeKind::App)?;
        s.insert(res::<0>("app", &log, &t))?;
        s.enter(ScopeKind::Login)?;
        s.insert(res::<1>("login", &log, &t))?;
        drop(s);
        assert_eq!(take(&log), vec!["login", "app"]);
        Ok(())
    }

    #[test]
    fn duplicates_and_inactive_exits_are_rejected() -> TestResult {
        let mut s = ScopeStack::new();
        assert_eq!(s.insert(5u32), Err(ScopeError::NoScope));
        s.enter(ScopeKind::App)?;
        s.insert(5u32)?;
        s.enter(ScopeKind::Login)?;
        assert!(matches!(
            s.insert(6u32),
            Err(ScopeError::Duplicate {
                owner: ScopeKind::App,
                ..
            })
        ));
        assert_eq!(
            s.exit(ScopeKind::World),
            Err(ScopeError::NotActive(ScopeKind::World))
        );
        if let Some(v) = s.get_mut::<u32>() {
            *v = 9;
        }
        assert_eq!(
            (s.get::<u32>(), s.owner_of::<u32>()),
            (Some(&9), Some(ScopeKind::App))
        );
        Ok(())
    }
}

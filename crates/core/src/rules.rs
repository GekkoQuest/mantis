//! Pure rule functions (plan 8.6, principle 10).
//!
//! A rule is a pure function from typed input to typed output, parameterised
//! by content: a stat formula, a damage preview, a cooldown reduction. The
//! simulation calls rules to decide outcomes, and the client's view models
//! call **the same rules** to display derived values (stat sheets, tooltip
//! damage). No formula is ever reimplemented in UI code.
//!
//! Rules hold only immutable parameters and take `&self`, so a [`RuleBook`]
//! can be shared across threads. Lookup is by type, so callers get the
//! concrete rule with its typed `Input`/`Output`; there are no untyped calls.

use core::any::{Any, TypeId};
use core::fmt;
use std::collections::BTreeMap;

/// A pure, deterministic rule.
pub trait Rule: Send + Sync + 'static {
    /// Stable, unique, dotted name, for example `"core.example_rule"`.
    const NAME: &'static str;
    /// What the rule reads.
    type Input;
    /// What it computes.
    type Output;

    /// Evaluates the rule. Must be pure: same input, same output, no side
    /// effects, no allocation.
    fn eval(&self, input: &Self::Input) -> Self::Output;
}

/// Why a rule was refused.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum RuleError {
    /// Another rule type uses this name.
    NameClash(&'static str),
}

impl fmt::Display for RuleError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NameClash(n) => write!(f, "two rule types share the name `{n}`"),
        }
    }
}

impl std::error::Error for RuleError {}

/// The loaded rules, keyed by type. Built at load time; read-only afterwards.
#[derive(Default)]
pub struct RuleBook {
    rules: BTreeMap<TypeId, (&'static str, Box<dyn Any + Send + Sync>)>,
}

impl fmt::Debug for RuleBook {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_set()
            .entries(self.rules.values().map(|(n, _)| n))
            .finish()
    }
}

impl RuleBook {
    /// An empty book.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Adds or replaces (content reload) the rule of type `R`.
    ///
    /// # Errors
    /// [`RuleError::NameClash`] if a different rule type uses `R::NAME`.
    pub fn insert<R: Rule>(&mut self, rule: R) -> Result<(), RuleError> {
        let clash = self
            .rules
            .iter()
            .any(|(t, (name, _))| *name == R::NAME && *t != TypeId::of::<R>());
        if clash {
            return Err(RuleError::NameClash(R::NAME));
        }
        self.rules.insert(TypeId::of::<R>(), (R::NAME, Box::new(rule)));
        Ok(())
    }

    /// The rule of type `R`, if loaded.
    #[must_use]
    pub fn get<R: Rule>(&self) -> Option<&R> {
        let (_, boxed) = self.rules.get(&TypeId::of::<R>())?;
        let any: &dyn Any = &**boxed;
        any.downcast_ref::<R>()
    }

    /// Evaluates the rule of type `R`, or `None` if it is not loaded (fail
    /// closed: callers show "unavailable", never a guessed number).
    #[must_use]
    pub fn eval<R: Rule>(&self, input: &R::Input) -> Option<R::Output> {
        self.get::<R>().map(|r| r.eval(input))
    }

    /// Number of rules.
    #[must_use]
    pub fn len(&self) -> usize {
        self.rules.len()
    }

    /// True when empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.rules.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A generic example: armor-scaled damage with a floor.
    struct Mitigation {
        armor_per_percent: u32,
        floor_percent: u32,
    }
    impl Rule for Mitigation {
        const NAME: &'static str = "test.mitigation";
        type Input = (u32, u32); // (raw damage, armor)
        type Output = u32;
        fn eval(&self, &(raw, armor): &(u32, u32)) -> u32 {
            let reduction = (armor / self.armor_per_percent.max(1)).min(100 - self.floor_percent);
            raw * (100 - reduction) / 100
        }
    }

    struct Impostor;
    impl Rule for Impostor {
        const NAME: &'static str = "test.mitigation";
        type Input = ();
        type Output = ();
        fn eval(&self, (): &()) {}
    }

    #[test]
    fn typed_lookup_and_eval() {
        let mut book = RuleBook::new();
        assert_eq!(
            book.eval::<Mitigation>(&(100, 50)),
            None,
            "not loaded: fail closed"
        );
        book.insert(Mitigation {
            armor_per_percent: 10,
            floor_percent: 25,
        })
        .unwrap();
        assert_eq!(book.eval::<Mitigation>(&(100, 50)), Some(95));
        assert_eq!(book.eval::<Mitigation>(&(100, 10_000)), Some(25), "floor");
        assert_eq!(
            book.insert(Impostor),
            Err(RuleError::NameClash("test.mitigation"))
        );
        // Reload replaces.
        book.insert(Mitigation {
            armor_per_percent: 5,
            floor_percent: 25,
        })
        .unwrap();
        assert_eq!(book.eval::<Mitigation>(&(100, 50)), Some(90));
        assert_eq!(book.len(), 1);
    }

    #[test]
    fn shareable_across_threads() {
        let mut book = RuleBook::new();
        book.insert(Mitigation {
            armor_per_percent: 10,
            floor_percent: 0,
        })
        .unwrap();
        let book = std::sync::Arc::new(book);
        let handles: Vec<_> = (0..4)
            .map(|i| {
                let b = std::sync::Arc::clone(&book);
                std::thread::spawn(move || b.eval::<Mitigation>(&(100, i * 10)))
            })
            .collect();
        let results: Vec<_> = handles.into_iter().map(|h| h.join().unwrap()).collect();
        assert_eq!(results, vec![Some(100), Some(99), Some(98), Some(97)]);
    }
}

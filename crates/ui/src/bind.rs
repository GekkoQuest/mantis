//! Bindable properties and view models.
//!
//! The UI never computes a display value. View models observe the
//! simulation's change sets and write finished values into [`Properties`];
//! layout files bind to them by name (`bind="player.name"`,
//! `template="HP {hp} / {hp_max}"`, `visible="dialog.open"`). Numbers arrive
//! as integers or as [`Value::Fixed`] (a pre-computed fixed-point number the
//! UI only formats), never as floats for the UI to round.
//!
//! Every property has a version that advances when its value changes; the
//! store also keeps a global version, so a frame with no changes skips the
//! binding pass entirely. Nodes bound to a changed property are refreshed and
//! re-laid out.

use std::collections::HashMap;
use std::fmt::Write as _;

/// Interned property name.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct PropertyId(pub u32);

/// One record of a [`Value::List`]: named fields.
#[derive(Clone, Debug, PartialEq, Default)]
pub struct ListItem {
    fields: Vec<(String, Value)>,
}

impl ListItem {
    /// An item with no fields.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Builder: sets `name` to `value`.
    #[must_use]
    pub fn with(mut self, name: &str, value: Value) -> Self {
        self.set(name, value);
        self
    }

    /// Sets `name` to `value`.
    pub fn set(&mut self, name: &str, value: Value) {
        if let Some(slot) = self.fields.iter_mut().find(|(n, _)| n == name) {
            slot.1 = value;
        } else {
            self.fields.push((name.to_owned(), value));
        }
    }

    /// The field `name`.
    #[must_use]
    pub fn field(&self, name: &str) -> Option<&Value> {
        self.fields.iter().find(|(n, _)| n == name).map(|(_, v)| v)
    }
}

/// A property value. The UI formats values; it never derives them.
#[derive(Clone, Debug, PartialEq)]
pub enum Value {
    /// Text, shown as is.
    Text(String),
    /// An integer.
    Int(i64),
    /// A fixed-point number computed elsewhere: `value / 10^decimals`.
    Fixed {
        /// The scaled integer.
        value: i64,
        /// Digits after the decimal point (at most 18 are shown).
        decimals: u8,
    },
    /// A flag (used by `visible=`).
    Bool(bool),
    /// Records repeated by a `list` element.
    List(Vec<ListItem>),
}

impl Value {
    /// Builds a text value.
    #[must_use]
    pub fn text(s: &str) -> Self {
        Self::Text(s.to_owned())
    }

    /// Appends the display form: text as is, integers in decimal, fixed-point
    /// numbers with exactly `decimals` digits, flags as `true`/`false`, lists
    /// as their length.
    pub fn write_display(&self, out: &mut String) {
        match self {
            Self::Text(s) => out.push_str(s),
            Self::Int(v) => {
                let _ = write!(out, "{v}");
            }
            Self::Fixed { value, decimals } => write_fixed(out, *value, *decimals),
            Self::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
            Self::List(items) => {
                let _ = write!(out, "{}", items.len());
            }
        }
    }

    /// The flag value, if this is a [`Value::Bool`].
    #[must_use]
    pub fn as_bool(&self) -> Option<bool> {
        match self {
            Self::Bool(b) => Some(*b),
            _ => None,
        }
    }

    /// The records, if this is a [`Value::List`].
    #[must_use]
    pub fn as_list(&self) -> Option<&[ListItem]> {
        match self {
            Self::List(items) => Some(items),
            _ => None,
        }
    }
}

fn write_fixed(out: &mut String, value: i64, decimals: u8) {
    let decimals = decimals.min(18);
    if decimals == 0 {
        let _ = write!(out, "{value}");
        return;
    }
    let pow = 10_u64.pow(u32::from(decimals));
    let abs = value.unsigned_abs();
    if value < 0 {
        out.push('-');
    }
    let width = usize::from(decimals);
    let _ = write!(out, "{}.{:0width$}", abs / pow, abs % pow);
}

#[derive(Debug)]
struct Slot {
    name: String,
    value: Option<Value>,
    version: u64,
}

/// The property store shared by view models and the UI.
#[derive(Debug, Default)]
pub struct Properties {
    ids: HashMap<String, PropertyId>,
    slots: Vec<Slot>,
    version: u64,
}

impl Properties {
    /// An empty store.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// The id of `name`, creating an empty property on first use.
    pub fn intern(&mut self, name: &str) -> PropertyId {
        if let Some(id) = self.ids.get(name) {
            return *id;
        }
        let id = PropertyId(u32::try_from(self.slots.len()).unwrap_or(u32::MAX));
        self.slots.push(Slot {
            name: name.to_owned(),
            value: None,
            version: 0,
        });
        self.ids.insert(name.to_owned(), id);
        id
    }

    /// The id of `name` if it was interned.
    #[must_use]
    pub fn id(&self, name: &str) -> Option<PropertyId> {
        self.ids.get(name).copied()
    }

    /// The name of a property.
    #[must_use]
    pub fn name(&self, id: PropertyId) -> Option<&str> {
        self.slots.get(id.0 as usize).map(|s| s.name.as_str())
    }

    /// The current value (`None` until first set).
    #[must_use]
    pub fn get(&self, id: PropertyId) -> Option<&Value> {
        self.slots.get(id.0 as usize).and_then(|s| s.value.as_ref())
    }

    /// The property's version; it advances on every change.
    #[must_use]
    pub fn version(&self, id: PropertyId) -> u64 {
        self.slots.get(id.0 as usize).map_or(0, |s| s.version)
    }

    /// Advances on any change to any property.
    #[must_use]
    pub fn global_version(&self) -> u64 {
        self.version
    }

    fn bump(&mut self, id: PropertyId) {
        self.version += 1;
        let v = self.version;
        if let Some(s) = self.slots.get_mut(id.0 as usize) {
            s.version = v;
        }
    }

    /// Sets a value. Returns true when it changed (equal values do not bump
    /// the version).
    pub fn set(&mut self, id: PropertyId, value: Value) -> bool {
        let Some(slot) = self.slots.get_mut(id.0 as usize) else {
            return false;
        };
        if slot.value.as_ref() == Some(&value) {
            return false;
        }
        slot.value = Some(value);
        self.bump(id);
        true
    }

    /// Sets text, reusing the existing string's capacity.
    pub fn set_text(&mut self, id: PropertyId, text: &str) -> bool {
        let Some(slot) = self.slots.get_mut(id.0 as usize) else {
            return false;
        };
        match &mut slot.value {
            Some(Value::Text(s)) if s == text => return false,
            Some(Value::Text(s)) => {
                s.clear();
                s.push_str(text);
            }
            other => *other = Some(Value::Text(text.to_owned())),
        }
        self.bump(id);
        true
    }

    /// Sets an integer.
    pub fn set_int(&mut self, id: PropertyId, v: i64) -> bool {
        self.set(id, Value::Int(v))
    }

    /// Sets a fixed-point number.
    pub fn set_fixed(&mut self, id: PropertyId, value: i64, decimals: u8) -> bool {
        self.set(id, Value::Fixed { value, decimals })
    }

    /// Sets a flag.
    pub fn set_bool(&mut self, id: PropertyId, v: bool) -> bool {
        self.set(id, Value::Bool(v))
    }

    /// Edits a value in place and marks it changed.
    pub fn update(&mut self, id: PropertyId, f: impl FnOnce(&mut Option<Value>)) {
        if let Some(slot) = self.slots.get_mut(id.0 as usize) {
            f(&mut slot.value);
            self.bump(id);
        }
    }

    /// Clears a value (bindings show nothing; `visible=` treats it as true).
    pub fn clear(&mut self, id: PropertyId) {
        if let Some(slot) = self.slots.get_mut(id.0 as usize)
            && slot.value.is_some()
        {
            slot.value = None;
            self.bump(id);
        }
    }
}

/// A view model: observes state change sets of type `C` and writes display
/// values into the property store.
pub trait ViewModel<C> {
    /// Applies one change set.
    fn observe(&mut self, change: &C, props: &mut Properties);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn shown(v: &Value) -> String {
        let mut s = String::new();
        v.write_display(&mut s);
        s
    }

    #[test]
    fn values_format_without_computing() {
        assert_eq!(shown(&Value::Int(-42)), "-42");
        assert_eq!(
            shown(&Value::Fixed {
                value: 12345,
                decimals: 2
            }),
            "123.45"
        );
        assert_eq!(
            shown(&Value::Fixed {
                value: -5,
                decimals: 2
            }),
            "-0.05"
        );
        assert_eq!(
            shown(&Value::Fixed {
                value: 7,
                decimals: 0
            }),
            "7"
        );
        assert_eq!(
            shown(&Value::Fixed {
                value: i64::MIN,
                decimals: 18
            }),
            "-9.223372036854775808"
        );
        assert_eq!(shown(&Value::Bool(true)), "true");
        assert_eq!(shown(&Value::List(vec![ListItem::new(), ListItem::new()])), "2");
    }

    #[test]
    fn versions_advance_only_on_change() {
        let mut p = Properties::new();
        let hp = p.intern("hp");
        assert_eq!(p.intern("hp"), hp);
        assert_eq!(p.version(hp), 0);
        assert!(p.set_int(hp, 10));
        let v1 = p.version(hp);
        assert!(!p.set_int(hp, 10));
        assert_eq!(p.version(hp), v1);
        assert!(p.set_int(hp, 11));
        assert!(p.version(hp) > v1);
        let name = p.intern("player.name");
        assert!(p.set_text(name, "a"));
        assert!(!p.set_text(name, "a"));
        assert!(p.set_text(name, "b"));
        assert_eq!(p.get(name), Some(&Value::text("b")));
        assert_eq!(p.name(name), Some("player.name"));
    }

    struct Counter;
    impl ViewModel<i64> for Counter {
        fn observe(&mut self, change: &i64, props: &mut Properties) {
            let id = props.intern("count");
            props.set_int(id, *change);
        }
    }

    #[test]
    fn view_model_writes_properties() {
        let mut p = Properties::new();
        Counter.observe(&5, &mut p);
        let id = p.id("count");
        assert_eq!(id.and_then(|i| p.get(i)), Some(&Value::Int(5)));
    }
}

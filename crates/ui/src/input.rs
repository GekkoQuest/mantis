//! Input events in, intents out.
//!
//! The platform layer translates window events into [`UiEvent`]s and feeds
//! them to `Ui::handle`, which answers [`Handled::Consumed`] when the UI used
//! the event and [`Handled::Ignored`] otherwise, so the client can route
//! unconsumed input to game actions. Pointer coordinates are physical
//! pixels, the same space as the draw list.
//!
//! Widgets never call game code: a button click (press and release inside
//! the button) or a text input's Enter queues a [`UiIntent`], which the client
//! drains with `Ui::drain_intents`.

use crate::tree::{IntentId, WidgetId};

/// A pointer button.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum PointerButton {
    /// Primary (left) button.
    Left,
    /// Secondary (right) button.
    Right,
    /// Middle button.
    Middle,
    /// Any other button.
    Other(u16),
}

/// Keys the UI understands. Character input arrives as [`UiEvent::Text`];
/// `Char` exists for shortcuts such as Ctrl+A.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum UiKey {
    /// Tab (Shift+Tab traverses backwards).
    Tab,
    /// Enter / Return.
    Enter,
    /// Escape.
    Escape,
    /// Backspace.
    Backspace,
    /// Delete.
    Delete,
    /// Left arrow.
    Left,
    /// Right arrow.
    Right,
    /// Up arrow.
    Up,
    /// Down arrow.
    Down,
    /// Home.
    Home,
    /// End.
    End,
    /// Page up.
    PageUp,
    /// Page down.
    PageDown,
    /// Space bar.
    Space,
    /// A character key (for shortcuts).
    Char(char),
    /// Anything else, by platform code.
    Other(u32),
}

/// Modifier keys held during a key event.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
#[allow(clippy::struct_excessive_bools)] // mirrors the platform's independent modifier flags
pub struct Modifiers {
    /// Shift.
    pub shift: bool,
    /// Control.
    pub ctrl: bool,
    /// Alt / Option.
    pub alt: bool,
    /// Logo / Command.
    pub logo: bool,
}

/// An input event for the UI.
#[derive(Clone, Debug, PartialEq)]
pub enum UiEvent {
    /// The pointer moved to `x`, `y` (physical px).
    PointerMove {
        /// Physical x.
        x: f32,
        /// Physical y.
        y: f32,
    },
    /// A pointer button changed state at the last pointer position.
    PointerButton {
        /// Which button.
        button: PointerButton,
        /// True on press, false on release.
        pressed: bool,
    },
    /// Scroll wheel, in lines (positive `dy` scrolls content up).
    Wheel {
        /// Horizontal lines.
        dx: f32,
        /// Vertical lines.
        dy: f32,
    },
    /// A key changed state.
    Key {
        /// Which key.
        key: UiKey,
        /// True on press.
        pressed: bool,
        /// Modifiers held.
        modifiers: Modifiers,
    },
    /// Committed character input (not IME composition).
    Text(String),
    /// IME composition in progress. An empty `text` ends the composition.
    ImePreedit {
        /// The composition string.
        text: String,
        /// Caret or selection inside `text`, byte offsets.
        cursor: Option<(usize, usize)>,
    },
    /// IME composition committed.
    ImeCommit(String),
    /// The window lost focus.
    FocusLost,
}

/// Whether the UI used an event.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Handled {
    /// The UI used the event; do not route it to game actions.
    Consumed,
    /// The UI did not use the event.
    Ignored,
}

impl Handled {
    /// True for [`Handled::Consumed`].
    #[must_use]
    pub fn consumed(self) -> bool {
        self == Self::Consumed
    }
}

/// A widget asked for something to happen.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UiIntent {
    /// The intent name, interned (see `Ui::intent_name`).
    pub intent: IntentId,
    /// The widget that emitted it.
    pub widget: WidgetId,
    /// Extra data: a button's rendered `payload`, or a text input's text.
    pub payload: Option<String>,
}

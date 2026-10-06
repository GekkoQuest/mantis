//! Device-level input, independent of any windowing library.
//!
//! The platform layer translates OS events into [`RawInput`]; nothing past that point
//! knows which library produced them.

/// Physical keyboard keys, by position (layout-independent).
#[allow(missing_docs)] // Variant names are the documentation.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, PartialOrd, Ord)]
pub enum KeyCode {
    A,
    B,
    C,
    D,
    E,
    F,
    G,
    H,
    I,
    J,
    K,
    L,
    M,
    N,
    O,
    P,
    Q,
    R,
    S,
    T,
    U,
    V,
    W,
    X,
    Y,
    Z,
    Digit0,
    Digit1,
    Digit2,
    Digit3,
    Digit4,
    Digit5,
    Digit6,
    Digit7,
    Digit8,
    Digit9,
    F1,
    F2,
    F3,
    F4,
    F5,
    F6,
    F7,
    F8,
    F9,
    F10,
    F11,
    F12,
    Escape,
    Enter,
    Space,
    Tab,
    Backspace,
    CapsLock,
    ShiftLeft,
    ShiftRight,
    ControlLeft,
    ControlRight,
    AltLeft,
    AltRight,
    SuperLeft,
    SuperRight,
    ArrowUp,
    ArrowDown,
    ArrowLeft,
    ArrowRight,
    Insert,
    Delete,
    Home,
    End,
    PageUp,
    PageDown,
    Minus,
    Equal,
    BracketLeft,
    BracketRight,
    Backslash,
    Semicolon,
    Quote,
    Backquote,
    Comma,
    Period,
    Slash,
    Numpad0,
    Numpad1,
    Numpad2,
    Numpad3,
    Numpad4,
    Numpad5,
    Numpad6,
    Numpad7,
    Numpad8,
    Numpad9,
    NumpadAdd,
    NumpadSubtract,
    NumpadMultiply,
    NumpadDivide,
    NumpadEnter,
    NumpadDecimal,
}

/// Mouse buttons.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, PartialOrd, Ord)]
pub enum MouseButton {
    /// Primary.
    Left,
    /// Secondary.
    Right,
    /// Wheel click.
    Middle,
    /// Side button, back.
    Back,
    /// Side button, forward.
    Forward,
    /// Any other button by platform index.
    Other(u16),
}

/// Mouse wheel notches, delivered as instantaneous button presses.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, PartialOrd, Ord)]
pub enum WheelDirection {
    /// Away from the user.
    Up,
    /// Toward the user.
    Down,
}

/// Gamepad buttons, by position.
#[allow(missing_docs)] // Variant names are the documentation.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, PartialOrd, Ord)]
pub enum GamepadButton {
    South,
    East,
    West,
    North,
    LeftBumper,
    RightBumper,
    LeftStick,
    RightStick,
    Select,
    Start,
    Guide,
    DPadUp,
    DPadDown,
    DPadLeft,
    DPadRight,
}

/// Gamepad analog axes.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, PartialOrd, Ord)]
pub enum GamepadAxis {
    /// Left stick horizontal, right positive.
    LeftStickX,
    /// Left stick vertical, up positive.
    LeftStickY,
    /// Right stick horizontal, right positive.
    RightStickX,
    /// Right stick vertical, up positive.
    RightStickY,
    /// Left trigger, 0 to 1.
    LeftTrigger,
    /// Right trigger, 0 to 1.
    RightTrigger,
}

impl GamepadAxis {
    /// Number of axes.
    pub const COUNT: usize = 6;

    /// Dense index in `0..COUNT`.
    pub const fn index(self) -> usize {
        match self {
            GamepadAxis::LeftStickX => 0,
            GamepadAxis::LeftStickY => 1,
            GamepadAxis::RightStickX => 2,
            GamepadAxis::RightStickY => 3,
            GamepadAxis::LeftTrigger => 4,
            GamepadAxis::RightTrigger => 5,
        }
    }
}

/// Anything with a pressed and released state.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, PartialOrd, Ord)]
pub enum ButtonSource {
    /// A keyboard key.
    Key(KeyCode),
    /// A mouse button.
    Mouse(MouseButton),
    /// A wheel notch (pressed and released in the same frame).
    Wheel(WheelDirection),
    /// A gamepad button on any connected pad.
    Gamepad(GamepadButton),
}

/// Anything with a continuous value.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, PartialOrd, Ord)]
pub enum AxisSource {
    /// A gamepad axis on any connected pad.
    Gamepad(GamepadAxis),
}

impl AxisSource {
    /// Number of axis sources.
    pub const COUNT: usize = GamepadAxis::COUNT;

    /// Dense index in `0..COUNT`.
    pub const fn index(self) -> usize {
        match self {
            AxisSource::Gamepad(a) => a.index(),
        }
    }
}

/// One device event, already translated from the platform.
#[derive(Clone, Copy, PartialEq, Debug)]
pub enum RawInput {
    /// A button changed state. Platform key repeat arrives as repeated presses and is ignored.
    Button {
        /// The button.
        source: ButtonSource,
        /// New state.
        pressed: bool,
    },
    /// An axis reported a new value.
    Axis {
        /// The axis.
        source: AxisSource,
        /// New value; sticks in [-1, 1], triggers in [0, 1].
        value: f32,
    },
    /// Raw relative mouse motion in counts (not cursor position, not accelerated).
    MouseMotion {
        /// Horizontal counts, right positive.
        dx: f32,
        /// Vertical counts, down positive.
        dy: f32,
    },
    /// The window lost focus: every held input is released.
    FocusLost,
}

//! Layout files and themes: a small declarative syntax.
//!
//! # Syntax
//!
//! ```text
//! // A comment runs to the end of the line.
//! theme {
//!   style title  { font = "ui" size = 24 color = #ffffff }
//!   style panel  { background = #20242cee radius = 6 padding = 8 border = 1 border_color = #ffffff22 }
//!   style button { background = #303846 padding_x = 12 padding_y = 6 radius = 4 }
//!   style button:hover   { background = #3c4658 }
//!   style button:pressed { background = #262c38 }
//!   style button:focus   { border = 1 border_color = #8cb4ff }
//!   style button:disabled { background = #262a30 color = #808080 }
//! }
//!
//! panel id=root style=panel direction=column gap=4 {
//!   text id=name bind="player.name" style=title
//!   text id=hp template="HP {hp} / {hp_max}"
//!   row gap=8 {
//!     button id=ok text="OK" intent="dialog.confirm" enabled="dialog.can_confirm"
//!       disabled_text="Unavailable"
//!     button id=cancel text="Cancel" intent="dialog.cancel"
//!   }
//!   input id=chat placeholder="Say something" submit="chat.send"
//!   list id=items bind="inventory.items" height=120 { text bind="item.name" }
//! }
//! ```
//!
//! A file is a sequence of `theme { ... }` blocks and elements. A layout file
//! has exactly one root element; a theme file has only theme blocks. An
//! element is `kind attr=value ... { children }`; the braces are optional.
//! Values are strings (`"..."` with `\"`, `\\`, `\n`, `\t` escapes), numbers
//! (`8`, `1.5`, `-2`), colors (`#rgb`, `#rrggbb`, `#rrggbbaa`, sRGB), and
//! bare words (`column`, `center`, `space-between`, `fit`, `grow`, `grow:2`,
//! `true`).
//!
//! ## Elements
//!
//! | kind | attributes beyond style attributes |
//! |---|---|
//! | `panel` | children; `direction` defaults to `column` |
//! | `row`, `column` | children; a panel with that direction |
//! | `text` | exactly one of `text`, `bind`, `template` |
//! | `button` | label: one of `text`, `bind`, `template`; `intent`, `payload` (a template); `disabled_text` (a template shown instead of the label while disabled) |
//! | `input` | `placeholder`, `submit` (intent fired on Enter), `max_length` |
//! | `list` | `bind` (a list property); exactly one child, the item template |
//! | `spacer` | none; grows along its parent's direction by default |
//!
//! Every element accepts `id` (letters, digits, `_`, `-`; unique per file),
//! `style` (a theme style name), `visible` (a flag property; hidden when it
//! is `false`), and `enabled` (a flag property; disabled when it is `false`).
//!
//! An unset or cleared flag counts as `true`. `enabled` is inherited: a
//! disabled container disables its whole subtree. A disabled element is drawn
//! in its disabled style, and a disabled button or input never emits intents,
//! never takes focus (it loses focus when it becomes disabled), is skipped by
//! Tab, shows no hover style, ignores typing and IME, and still consumes
//! pointer presses so a click on it never falls through to game actions.
//!
//! ## Style attributes
//!
//! Usable inline and in theme styles: `direction` (`row`/`column`), `width`
//! and `height` (pixels, `fit`, `grow`, `grow:N`), `min_width`, `max_width`,
//! `min_height`, `max_height`, `padding`, `padding_x`, `padding_y`, `gap`,
//! `align` (`start`/`center`/`end`/`stretch`), `justify`
//! (`start`/`center`/`end`/`space-between`), `background`, `border`,
//! `border_color`, `radius`, `color` (text), `font` (a font stack name),
//! `size` (font size), `text_align` (`start`/`center`/`end`), `clip`
//! (`true`/`false`).
//!
//! Style resolution, weakest first: built-in defaults, the theme style named
//! after the element kind (`style button { ... }`), the style named by
//! `style=`, inline attributes. `name:hover`, `name:pressed`, `name:focus`,
//! and `name:disabled` variants override colors and borders in those states;
//! disabled wins over the others. Without a `:disabled` color, a disabled
//! element draws its background, border, and text colors at 40% alpha.
//!
//! ## Templates hold no formulas
//!
//! A template is literal text with `{property}` interpolation (`{item.field}`
//! inside a list item) and `{{` / `}}` for literal braces. Anything else in
//! braces, such as `{hp * 2}` or `{max(a, b)}`, is an error: derived values
//! are computed by a view model and bound, never computed by the UI.
//!
//! ## Errors
//!
//! Parsing never panics. Every error carries a 1-based line and column.

use std::collections::{BTreeMap, HashSet};
use std::fmt;

use crate::color::Color;
use crate::text::TextAlign;

/// A parse or validation error with its position.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MarkupError {
    /// 1-based line.
    pub line: u32,
    /// 1-based column, in characters.
    pub column: u32,
    /// What went wrong.
    pub message: String,
}

impl fmt::Display for MarkupError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}:{}: {}", self.line, self.column, self.message)
    }
}

impl std::error::Error for MarkupError {}

impl MarkupError {
    fn at(line: u32, column: u32, message: impl Into<String>) -> Self {
        Self {
            line,
            column,
            message: message.into(),
        }
    }
}

/// Element kinds.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ElementKind {
    /// A container (column by default).
    Panel,
    /// A container laid out left to right.
    Row,
    /// A container laid out top to bottom.
    Column,
    /// A text label.
    Text,
    /// A clickable button with a label.
    Button,
    /// A single-line text input.
    Input,
    /// Repeats its template child once per item of a list property.
    List,
    /// Empty space.
    Spacer,
}

impl ElementKind {
    fn from_name(name: &str) -> Option<Self> {
        Some(match name {
            "panel" => Self::Panel,
            "row" => Self::Row,
            "column" => Self::Column,
            "text" => Self::Text,
            "button" => Self::Button,
            "input" => Self::Input,
            "list" => Self::List,
            "spacer" => Self::Spacer,
            _ => return None,
        })
    }

    /// The keyword in layout files.
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::Panel => "panel",
            Self::Row => "row",
            Self::Column => "column",
            Self::Text => "text",
            Self::Button => "button",
            Self::Input => "input",
            Self::List => "list",
            Self::Spacer => "spacer",
        }
    }

    fn has_children(self) -> bool {
        matches!(self, Self::Panel | Self::Row | Self::Column | Self::List)
    }
}

/// Main axis of a container.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Flow {
    /// Left to right.
    Row,
    /// Top to bottom.
    Column,
}

/// Cross-axis alignment of children.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Align {
    /// Top or left.
    Start,
    /// Centered.
    Center,
    /// Bottom or right.
    End,
    /// Fill the cross axis.
    Stretch,
}

/// Main-axis distribution of children.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Justify {
    /// Packed at the start.
    Start,
    /// Packed in the middle.
    Center,
    /// Packed at the end.
    End,
    /// First at the start, last at the end, equal space between.
    SpaceBetween,
}

/// A width or height.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum SizeSpec {
    /// Fixed logical pixels.
    Px(f32),
    /// The content's size.
    Fit,
    /// A share of the free space, by weight.
    Grow(f32),
}

/// Style attributes; `None` means "not set here".
#[derive(Clone, Debug, Default, PartialEq)]
pub struct StyleSpec {
    /// Container direction.
    pub direction: Option<Flow>,
    /// Width.
    pub width: Option<SizeSpec>,
    /// Height.
    pub height: Option<SizeSpec>,
    /// Minimum width, px.
    pub min_width: Option<f32>,
    /// Maximum width, px.
    pub max_width: Option<f32>,
    /// Minimum height, px.
    pub min_height: Option<f32>,
    /// Maximum height, px.
    pub max_height: Option<f32>,
    /// Padding on all sides, px.
    pub padding: Option<f32>,
    /// Horizontal padding, px (overrides `padding`).
    pub padding_x: Option<f32>,
    /// Vertical padding, px (overrides `padding`).
    pub padding_y: Option<f32>,
    /// Space between children, px.
    pub gap: Option<f32>,
    /// Cross-axis alignment.
    pub align: Option<Align>,
    /// Main-axis distribution.
    pub justify: Option<Justify>,
    /// Fill color.
    pub background: Option<Color>,
    /// Border width, px.
    pub border: Option<f32>,
    /// Border color.
    pub border_color: Option<Color>,
    /// Corner radius, px.
    pub radius: Option<f32>,
    /// Text color.
    pub color: Option<Color>,
    /// Font stack name.
    pub font: Option<String>,
    /// Font size, px.
    pub size: Option<f32>,
    /// Text alignment.
    pub text_align: Option<TextAlign>,
    /// Clip children to this element's rectangle.
    pub clip: Option<bool>,
}

impl StyleSpec {
    /// Overrides every field that `over` sets.
    pub fn merge(&mut self, over: &Self) {
        fn take<T: Clone>(dst: &mut Option<T>, src: Option<&T>) {
            if let Some(v) = src {
                *dst = Some(v.clone());
            }
        }
        take(&mut self.direction, over.direction.as_ref());
        take(&mut self.width, over.width.as_ref());
        take(&mut self.height, over.height.as_ref());
        take(&mut self.min_width, over.min_width.as_ref());
        take(&mut self.max_width, over.max_width.as_ref());
        take(&mut self.min_height, over.min_height.as_ref());
        take(&mut self.max_height, over.max_height.as_ref());
        take(&mut self.padding, over.padding.as_ref());
        take(&mut self.padding_x, over.padding_x.as_ref());
        take(&mut self.padding_y, over.padding_y.as_ref());
        take(&mut self.gap, over.gap.as_ref());
        take(&mut self.align, over.align.as_ref());
        take(&mut self.justify, over.justify.as_ref());
        take(&mut self.background, over.background.as_ref());
        take(&mut self.border, over.border.as_ref());
        take(&mut self.border_color, over.border_color.as_ref());
        take(&mut self.radius, over.radius.as_ref());
        take(&mut self.color, over.color.as_ref());
        take(&mut self.font, over.font.as_ref());
        take(&mut self.size, over.size.as_ref());
        take(&mut self.text_align, over.text_align.as_ref());
        take(&mut self.clip, over.clip.as_ref());
    }
}

/// A named theme style with its state variants.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct StyleDef {
    /// Normal state.
    pub base: StyleSpec,
    /// Pointer over the element.
    pub hover: StyleSpec,
    /// Pointer pressed on the element.
    pub pressed: StyleSpec,
    /// Keyboard focus on the element.
    pub focus: StyleSpec,
    /// The element is disabled.
    pub disabled: StyleSpec,
    /// Where the style was (first) declared.
    pub line: u32,
    /// Column of the declaration.
    pub column: u32,
}

/// Named styles.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Theme {
    /// Styles by name.
    pub styles: BTreeMap<String, StyleDef>,
}

impl Theme {
    /// Adds `other`'s styles; for a name in both, `other`'s fields win.
    pub fn merge(&mut self, other: &Self) {
        for (name, def) in &other.styles {
            let slot = self.styles.entry(name.clone()).or_insert_with(|| StyleDef {
                line: def.line,
                column: def.column,
                ..StyleDef::default()
            });
            slot.base.merge(&def.base);
            slot.hover.merge(&def.hover);
            slot.pressed.merge(&def.pressed);
            slot.focus.merge(&def.focus);
            slot.disabled.merge(&def.disabled);
        }
    }
}

/// One piece of a template.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Segment {
    /// Literal text.
    Literal(String),
    /// A property path to interpolate.
    Property(String),
}

/// Literal text with `{property}` interpolation.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Template {
    /// Segments in order.
    pub segments: Vec<Segment>,
}

/// Where an element's text comes from.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TextSource {
    /// Fixed text.
    Literal(String),
    /// A property shown as is.
    Bind(String),
    /// A template.
    Template(Template),
}

/// A parsed element.
#[derive(Clone, Debug, PartialEq)]
pub struct ElementSpec {
    /// Kind.
    pub kind: ElementKind,
    /// Stable id.
    pub id: Option<String>,
    /// `style=` name.
    pub style_name: Option<String>,
    /// Inline style attributes.
    pub style: StyleSpec,
    /// Text or label source.
    pub text: Option<TextSource>,
    /// Button intent.
    pub intent: Option<String>,
    /// Button payload.
    pub payload: Option<Template>,
    /// Input submit intent.
    pub submit: Option<String>,
    /// Input placeholder.
    pub placeholder: Option<String>,
    /// Input maximum length in characters.
    pub max_length: Option<u32>,
    /// Visibility flag property.
    pub visible: Option<String>,
    /// Enabled flag property.
    pub enabled: Option<String>,
    /// Button label shown while disabled.
    pub disabled_text: Option<Template>,
    /// List property (lists only).
    pub bind: Option<String>,
    /// Children (for a list: the single item template).
    pub children: Vec<ElementSpec>,
    /// 1-based line of the element keyword.
    pub line: u32,
    /// 1-based column of the element keyword.
    pub column: u32,
}

impl ElementSpec {
    fn new(kind: ElementKind, line: u32, column: u32) -> Self {
        Self {
            kind,
            id: None,
            style_name: None,
            style: StyleSpec::default(),
            text: None,
            intent: None,
            payload: None,
            submit: None,
            placeholder: None,
            max_length: None,
            visible: None,
            enabled: None,
            disabled_text: None,
            bind: None,
            children: Vec::new(),
            line,
            column,
        }
    }

    /// Calls `f` on this element and every descendant, depth first.
    pub fn visit<'a>(&'a self, f: &mut impl FnMut(&'a Self)) {
        f(self);
        for c in &self.children {
            c.visit(f);
        }
    }
}

/// A parsed layout file.
#[derive(Clone, Debug, PartialEq)]
pub struct Document {
    /// Styles declared in the file.
    pub theme: Theme,
    /// The root element.
    pub root: ElementSpec,
}

/// Deepest element nesting accepted.
pub const MAX_DEPTH: usize = 64;

#[derive(Clone, Debug, PartialEq)]
enum Tok {
    Ident(String),
    Str(String),
    Num(f32),
    Color(Color),
    LBrace,
    RBrace,
    Eq,
    Colon,
    Eof,
}

#[derive(Clone, Debug)]
struct Token {
    tok: Tok,
    line: u32,
    column: u32,
}

struct Cursor<'a> {
    chars: std::iter::Peekable<std::str::Chars<'a>>,
    line: u32,
    column: u32,
}

impl Cursor<'_> {
    fn peek(&mut self) -> Option<char> {
        self.chars.peek().copied()
    }

    fn next(&mut self) -> Option<char> {
        let c = self.chars.next()?;
        if c == '\n' {
            self.line = self.line.saturating_add(1);
            self.column = 1;
        } else {
            self.column = self.column.saturating_add(1);
        }
        Some(c)
    }

    /// Consumes characters while `keep(word_so_far, c)` holds.
    fn take_while(&mut self, word: &mut String, keep: impl Fn(&str, char) -> bool) {
        while let Some(c) = self.peek() {
            if !keep(word, c) {
                break;
            }
            word.push(c);
            self.next();
        }
    }

    /// A string literal; the opening quote is already consumed.
    fn string(&mut self, line: u32, column: u32) -> Result<String, MarkupError> {
        let mut word = String::new();
        loop {
            let Some(n) = self.next() else {
                return Err(MarkupError::at(line, column, "unterminated string"));
            };
            match n {
                '"' => return Ok(word),
                '\n' => return Err(MarkupError::at(line, column, "unterminated string")),
                '\\' => {
                    let (el, ec) = (self.line, self.column.saturating_sub(1).max(1));
                    let Some(e) = self.next() else {
                        return Err(MarkupError::at(line, column, "unterminated string"));
                    };
                    word.push(match e {
                        'n' => '\n',
                        't' => '\t',
                        '"' => '"',
                        '\\' => '\\',
                        _ => {
                            return Err(MarkupError::at(
                                el,
                                ec,
                                format!("unknown escape `\\{}`", e.escape_debug()),
                            ));
                        }
                    });
                }
                _ => word.push(n),
            }
        }
    }

    fn token(&mut self, c: char, line: u32, column: u32) -> Result<Option<Tok>, MarkupError> {
        let mut word = String::new();
        let tok = match c {
            '{' | '}' | '=' | ':' => {
                self.next();
                match c {
                    '{' => Tok::LBrace,
                    '}' => Tok::RBrace,
                    '=' => Tok::Eq,
                    _ => Tok::Colon,
                }
            }
            '/' => {
                self.next();
                if self.peek() != Some('/') {
                    return Err(MarkupError::at(
                        line,
                        column,
                        "unexpected `/` (comments start with `//`)",
                    ));
                }
                self.take_while(&mut word, |_, n| n != '\n');
                return Ok(None);
            }
            '"' => {
                self.next();
                Tok::Str(self.string(line, column)?)
            }
            '#' => {
                self.take_while(&mut word, |w, n| {
                    (n == '#' && w.is_empty()) || n.is_ascii_alphanumeric()
                });
                Tok::Color(Color::from_hex(&word).ok_or_else(|| {
                    MarkupError::at(
                        line,
                        column,
                        format!("invalid color `{word}` (use #rgb, #rrggbb, or #rrggbbaa)"),
                    )
                })?)
            }
            _ if c.is_ascii_digit() || c == '-' => {
                self.take_while(&mut word, |w, n| {
                    n.is_ascii_digit() || n == '.' || (n == '-' && w.is_empty())
                });
                match word.parse::<f32>() {
                    Ok(v) if v.is_finite() && v.abs() <= 1e6 => Tok::Num(v),
                    _ => return Err(MarkupError::at(line, column, format!("invalid number `{word}`"))),
                }
            }
            _ if c.is_alphabetic() || c == '_' => {
                self.take_while(&mut word, |_, n| n.is_alphanumeric() || n == '_' || n == '-');
                Tok::Ident(word)
            }
            _ => {
                return Err(MarkupError::at(
                    line,
                    column,
                    format!("unexpected character `{c}`"),
                ));
            }
        };
        Ok(Some(tok))
    }
}

fn lex(src: &str) -> Result<Vec<Token>, MarkupError> {
    let mut out = Vec::new();
    let mut cur = Cursor {
        chars: src.chars().peekable(),
        line: 1,
        column: 1,
    };
    while let Some(c) = cur.peek() {
        if c.is_whitespace() {
            cur.next();
            continue;
        }
        let (line, column) = (cur.line, cur.column);
        if let Some(tok) = cur.token(c, line, column)? {
            out.push(Token { tok, line, column });
        }
    }
    out.push(Token {
        tok: Tok::Eof,
        line: cur.line,
        column: cur.column,
    });
    Ok(out)
}

/// An attribute value with its position.
struct AttrValue {
    tok: Tok,
    grow: Option<f32>,
    line: u32,
    column: u32,
}

struct Parser {
    toks: Vec<Token>,
    pos: usize,
}

const EOF: Token = Token {
    tok: Tok::Eof,
    line: 0,
    column: 0,
};

impl Parser {
    fn peek(&self) -> &Token {
        self.toks
            .get(self.pos)
            .or_else(|| self.toks.last())
            .unwrap_or(&EOF)
    }

    fn peek_at(&self, k: usize) -> &Tok {
        self.toks.get(self.pos + k).map_or(&Tok::Eof, |t| &t.tok)
    }

    fn bump(&mut self) -> Token {
        let t = self.peek().clone();
        if self.pos < self.toks.len() {
            self.pos += 1;
        }
        t
    }

    fn err(&self, message: impl Into<String>) -> MarkupError {
        let t = self.peek();
        MarkupError::at(t.line, t.column, message)
    }

    fn expect(&mut self, want: &Tok, what: &str) -> Result<Token, MarkupError> {
        if &self.peek().tok == want {
            Ok(self.bump())
        } else {
            Err(self.err(format!("expected {what}, found {}", describe(&self.peek().tok))))
        }
    }

    /// Parses the whole file into theme blocks and elements.
    fn file(&mut self) -> Result<(Theme, Vec<ElementSpec>), MarkupError> {
        let mut theme = Theme::default();
        let mut elements = Vec::new();
        loop {
            let t = self.peek().clone();
            match &t.tok {
                Tok::Eof => break,
                Tok::Ident(w) if w == "theme" && self.peek_at(1) == &Tok::LBrace => {
                    self.bump();
                    self.bump();
                    self.theme_block(&mut theme)?;
                }
                Tok::Ident(_) => elements.push(self.element(0)?),
                other => {
                    return Err(self.err(format!(
                        "expected `theme` or an element, found {}",
                        describe(other)
                    )));
                }
            }
        }
        Ok((theme, elements))
    }

    fn theme_block(&mut self, theme: &mut Theme) -> Result<(), MarkupError> {
        loop {
            let t = self.bump();
            match t.tok {
                Tok::RBrace => return Ok(()),
                Tok::Ident(w) if w == "style" => {
                    let name_tok = self.bump();
                    let Tok::Ident(name) = name_tok.tok else {
                        return Err(MarkupError::at(
                            name_tok.line,
                            name_tok.column,
                            "expected a style name",
                        ));
                    };
                    let mut state = None;
                    if self.peek().tok == Tok::Colon {
                        self.bump();
                        let st = self.bump();
                        match st.tok {
                            Tok::Ident(s)
                                if matches!(s.as_str(), "hover" | "pressed" | "focus" | "disabled") =>
                            {
                                state = Some(s);
                            }
                            _ => {
                                return Err(MarkupError::at(
                                    st.line,
                                    st.column,
                                    "expected a state: `hover`, `pressed`, `focus`, or `disabled`",
                                ));
                            }
                        }
                    }
                    self.expect(&Tok::LBrace, "`{` after the style name")?;
                    let mut spec = StyleSpec::default();
                    loop {
                        if self.peek().tok == Tok::RBrace {
                            self.bump();
                            break;
                        }
                        let (key, value) = self.attribute()?;
                        let known = apply_style_attr(&mut spec, &key, &value)
                            .map_err(|m| MarkupError::at(value.line, value.column, m))?;
                        if !known {
                            return Err(MarkupError::at(
                                value.line,
                                value.column,
                                format!("`{key}` is not a style attribute"),
                            ));
                        }
                    }
                    let def = theme.styles.entry(name).or_insert_with(|| StyleDef {
                        line: t.line,
                        column: t.column,
                        ..StyleDef::default()
                    });
                    let slot = match state.as_deref() {
                        Some("hover") => &mut def.hover,
                        Some("pressed") => &mut def.pressed,
                        Some("focus") => &mut def.focus,
                        Some("disabled") => &mut def.disabled,
                        _ => &mut def.base,
                    };
                    slot.merge(&spec);
                }
                Tok::Eof => return Err(MarkupError::at(t.line, t.column, "unclosed `theme {`")),
                other => {
                    return Err(MarkupError::at(
                        t.line,
                        t.column,
                        format!("expected `style` or `}}` in a theme, found {}", describe(&other)),
                    ));
                }
            }
        }
    }

    /// `name = value`
    fn attribute(&mut self) -> Result<(String, AttrValue), MarkupError> {
        let key_tok = self.bump();
        let Tok::Ident(key) = key_tok.tok else {
            return Err(MarkupError::at(
                key_tok.line,
                key_tok.column,
                format!("expected an attribute name, found {}", describe(&key_tok.tok)),
            ));
        };
        self.expect(&Tok::Eq, &format!("`=` after `{key}`"))?;
        let v = self.bump();
        let mut value = AttrValue {
            tok: v.tok,
            grow: None,
            line: v.line,
            column: v.column,
        };
        match &value.tok {
            Tok::Ident(w) if w == "grow" && self.peek().tok == Tok::Colon => {
                self.bump();
                let n = self.bump();
                match n.tok {
                    Tok::Num(k) if k > 0.0 => value.grow = Some(k),
                    _ => {
                        return Err(MarkupError::at(
                            n.line,
                            n.column,
                            "expected a positive weight after `grow:`",
                        ));
                    }
                }
            }
            Tok::Ident(_) | Tok::Str(_) | Tok::Num(_) | Tok::Color(_) => {}
            other => {
                return Err(MarkupError::at(
                    value.line,
                    value.column,
                    format!("expected a value for `{key}`, found {}", describe(other)),
                ));
            }
        }
        Ok((key, value))
    }

    fn element(&mut self, depth: usize) -> Result<ElementSpec, MarkupError> {
        let t = self.bump();
        let Tok::Ident(name) = t.tok else {
            return Err(MarkupError::at(
                t.line,
                t.column,
                format!("expected an element, found {}", describe(&t.tok)),
            ));
        };
        if depth >= MAX_DEPTH {
            return Err(MarkupError::at(
                t.line,
                t.column,
                format!("elements nest deeper than {MAX_DEPTH}"),
            ));
        }
        let kind = ElementKind::from_name(&name).ok_or_else(|| {
            MarkupError::at(
                t.line,
                t.column,
                format!("unknown element `{name}` (expected panel, row, column, text, button, input, list, or spacer)"),
            )
        })?;
        let mut el = ElementSpec::new(kind, t.line, t.column);
        while matches!(self.peek().tok, Tok::Ident(_)) && self.peek_at(1) == &Tok::Eq {
            let (key, value) = self.attribute()?;
            apply_element_attr(&mut el, &key, &value)?;
        }
        if self.peek().tok == Tok::LBrace {
            let open = self.bump();
            if !kind.has_children() {
                return Err(MarkupError::at(
                    open.line,
                    open.column,
                    format!("`{}` cannot have children", kind.name()),
                ));
            }
            loop {
                match &self.peek().tok {
                    Tok::RBrace => {
                        self.bump();
                        break;
                    }
                    Tok::Eof => return Err(MarkupError::at(open.line, open.column, "unclosed `{`")),
                    _ => el.children.push(self.element(depth + 1)?),
                }
            }
        }
        check_element(&el)?;
        Ok(el)
    }
}

fn describe(t: &Tok) -> String {
    match t {
        Tok::Ident(w) => format!("`{w}`"),
        Tok::Str(_) => "a string".to_owned(),
        Tok::Num(_) => "a number".to_owned(),
        Tok::Color(_) => "a color".to_owned(),
        Tok::LBrace => "`{`".to_owned(),
        Tok::RBrace => "`}`".to_owned(),
        Tok::Eq => "`=`".to_owned(),
        Tok::Colon => "`:`".to_owned(),
        Tok::Eof => "the end of the file".to_owned(),
    }
}

fn num(v: &AttrValue, key: &str) -> Result<f32, String> {
    match v.tok {
        Tok::Num(n) if n >= 0.0 => Ok(n),
        _ => Err(format!("`{key}` takes a non-negative number")),
    }
}

fn word<'a>(v: &'a AttrValue, key: &str) -> Result<&'a str, String> {
    match &v.tok {
        Tok::Ident(w) => Ok(w),
        _ => Err(format!("`{key}` takes a bare word")),
    }
}

fn string(v: &AttrValue, key: &str) -> Result<String, String> {
    match &v.tok {
        Tok::Str(s) | Tok::Ident(s) => Ok(s.clone()),
        _ => Err(format!("`{key}` takes a string")),
    }
}

fn color(v: &AttrValue, key: &str) -> Result<Color, String> {
    match v.tok {
        Tok::Color(c) => Ok(c),
        _ => Err(format!("`{key}` takes a color like #rrggbb")),
    }
}

fn size_spec(v: &AttrValue, key: &str) -> Result<SizeSpec, String> {
    if let Some(w) = v.grow {
        return Ok(SizeSpec::Grow(w));
    }
    match &v.tok {
        Tok::Num(n) if *n >= 0.0 => Ok(SizeSpec::Px(*n)),
        Tok::Ident(w) if w == "fit" => Ok(SizeSpec::Fit),
        Tok::Ident(w) if w == "grow" => Ok(SizeSpec::Grow(1.0)),
        _ => Err(format!("`{key}` takes pixels, `fit`, `grow`, or `grow:N`")),
    }
}

/// Applies a style attribute. `Ok(false)` when `key` is not a style key.
fn apply_style_attr(s: &mut StyleSpec, key: &str, v: &AttrValue) -> Result<bool, String> {
    match key {
        "direction" => {
            s.direction = Some(match word(v, key)? {
                "row" => Flow::Row,
                "column" => Flow::Column,
                _ => return Err("`direction` is `row` or `column`".to_owned()),
            });
        }
        "width" => s.width = Some(size_spec(v, key)?),
        "height" => s.height = Some(size_spec(v, key)?),
        "min_width" => s.min_width = Some(num(v, key)?),
        "max_width" => s.max_width = Some(num(v, key)?),
        "min_height" => s.min_height = Some(num(v, key)?),
        "max_height" => s.max_height = Some(num(v, key)?),
        "padding" => s.padding = Some(num(v, key)?),
        "padding_x" => s.padding_x = Some(num(v, key)?),
        "padding_y" => s.padding_y = Some(num(v, key)?),
        "gap" => s.gap = Some(num(v, key)?),
        "border" => s.border = Some(num(v, key)?),
        "radius" => s.radius = Some(num(v, key)?),
        "size" => s.size = Some(num(v, key)?),
        "align" => {
            s.align = Some(match word(v, key)? {
                "start" => Align::Start,
                "center" => Align::Center,
                "end" => Align::End,
                "stretch" => Align::Stretch,
                _ => return Err("`align` is `start`, `center`, `end`, or `stretch`".to_owned()),
            });
        }
        "justify" => {
            s.justify = Some(match word(v, key)? {
                "start" => Justify::Start,
                "center" => Justify::Center,
                "end" => Justify::End,
                "space-between" => Justify::SpaceBetween,
                _ => return Err("`justify` is `start`, `center`, `end`, or `space-between`".to_owned()),
            });
        }
        "text_align" => {
            s.text_align = Some(match word(v, key)? {
                "start" => TextAlign::Start,
                "center" => TextAlign::Center,
                "end" => TextAlign::End,
                _ => return Err("`text_align` is `start`, `center`, or `end`".to_owned()),
            });
        }
        "background" => s.background = Some(color(v, key)?),
        "border_color" => s.border_color = Some(color(v, key)?),
        "color" => s.color = Some(color(v, key)?),
        "font" => s.font = Some(string(v, key)?),
        "clip" => {
            s.clip = Some(match word(v, key)? {
                "true" => true,
                "false" => false,
                _ => return Err("`clip` is `true` or `false`".to_owned()),
            });
        }
        _ => return Ok(false),
    }
    Ok(true)
}

/// A non-negative count from a parsed number (clamped to a million).
#[expect(clippy::cast_possible_truncation, clippy::cast_sign_loss)] // clamped to 0..=1e6 first
fn count(n: f32) -> u32 {
    n.clamp(0.0, 1e6).round() as u32
}

fn is_valid_id(id: &str) -> bool {
    !id.is_empty()
        && id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
}

fn is_property_path(s: &str) -> bool {
    !s.is_empty()
        && s.split('.').all(|part| {
            let mut chars = part.chars();
            chars.next().is_some_and(|c| c.is_alphabetic() || c == '_')
                && chars.all(|c| c.is_alphanumeric() || c == '_')
        })
}

fn apply_element_attr(el: &mut ElementSpec, key: &str, v: &AttrValue) -> Result<(), MarkupError> {
    let at = |m: String| MarkupError::at(v.line, v.column, m);
    let kind = el.kind;
    let only = |allowed: &[ElementKind]| -> Result<(), MarkupError> {
        if allowed.contains(&kind) {
            Ok(())
        } else {
            Err(MarkupError::at(
                v.line,
                v.column,
                format!("`{}` does not take `{key}`", kind.name()),
            ))
        }
    };
    let set_text = |el: &mut ElementSpec, src: TextSource| -> Result<(), MarkupError> {
        if el.text.is_some() {
            return Err(MarkupError::at(
                v.line,
                v.column,
                "use only one of `text`, `bind`, and `template`",
            ));
        }
        el.text = Some(src);
        Ok(())
    };
    match key {
        "id" => {
            let id = string(v, key).map_err(at)?;
            if !is_valid_id(&id) {
                return Err(at(format!(
                    "invalid id `{id}` (letters, digits, `_`, and `-` only)"
                )));
            }
            el.id = Some(id);
        }
        "style" => el.style_name = Some(string(v, key).map_err(at)?),
        "visible" => el.visible = Some(property(v, key)?),
        "enabled" => el.enabled = Some(property(v, key)?),
        "disabled_text" => {
            only(&[ElementKind::Button])?;
            el.disabled_text = Some(template_attr(v, key)?);
        }
        "text" => {
            only(&[ElementKind::Text, ElementKind::Button])?;
            let s = string(v, key).map_err(at)?;
            set_text(el, TextSource::Literal(s))?;
        }
        "template" => {
            only(&[ElementKind::Text, ElementKind::Button])?;
            let t = template_attr(v, key)?;
            set_text(el, TextSource::Template(t))?;
        }
        "bind" => {
            if kind == ElementKind::List {
                el.bind = Some(property(v, key)?);
            } else {
                only(&[ElementKind::Text, ElementKind::Button])?;
                let p = property(v, key)?;
                set_text(el, TextSource::Bind(p))?;
            }
        }
        "intent" => {
            only(&[ElementKind::Button])?;
            el.intent = Some(intent_name(v, key)?);
        }
        "payload" => {
            only(&[ElementKind::Button])?;
            el.payload = Some(template_attr(v, key)?);
        }
        "submit" => {
            only(&[ElementKind::Input])?;
            el.submit = Some(intent_name(v, key)?);
        }
        "placeholder" => {
            only(&[ElementKind::Input])?;
            el.placeholder = Some(string(v, key).map_err(at)?);
        }
        "max_length" => {
            only(&[ElementKind::Input])?;
            el.max_length = Some(count(num(v, key).map_err(at)?));
        }
        _ => {
            let known = apply_style_attr(&mut el.style, key, v).map_err(at)?;
            if !known {
                return Err(at(format!("unknown attribute `{key}` on `{}`", kind.name())));
            }
        }
    }
    Ok(())
}

fn property(v: &AttrValue, key: &str) -> Result<String, MarkupError> {
    let s = string(v, key).map_err(|m| MarkupError::at(v.line, v.column, m))?;
    if is_property_path(&s) {
        Ok(s)
    } else {
        Err(MarkupError::at(
            v.line,
            v.column,
            format!("`{key}` takes a property name like `player.name`, not `{s}`; the UI holds no formulas"),
        ))
    }
}

fn intent_name(v: &AttrValue, key: &str) -> Result<String, MarkupError> {
    let s = string(v, key).map_err(|m| MarkupError::at(v.line, v.column, m))?;
    if is_property_path(&s) {
        Ok(s)
    } else {
        Err(MarkupError::at(
            v.line,
            v.column,
            format!("invalid intent name `{s}`"),
        ))
    }
}

fn template_attr(v: &AttrValue, key: &str) -> Result<Template, MarkupError> {
    let Tok::Str(s) = &v.tok else {
        return Err(MarkupError::at(
            v.line,
            v.column,
            format!("`{key}` takes a string"),
        ));
    };
    parse_template(s).map_err(|e| {
        let offset = s.get(..e.offset).map_or(0, |p| p.chars().count());
        let col = v
            .column
            .saturating_add(1)
            .saturating_add(u32::try_from(offset).unwrap_or(0));
        MarkupError::at(v.line, col, e.message)
    })
}

fn check_element(el: &ElementSpec) -> Result<(), MarkupError> {
    let at = |m: &str| Err(MarkupError::at(el.line, el.column, m));
    match el.kind {
        ElementKind::Text if el.text.is_none() => {
            at("a `text` element needs `text=`, `bind=`, or `template=`")
        }
        ElementKind::List if el.bind.is_none() => at("a `list` needs `bind=` naming a list property"),
        ElementKind::List if el.children.len() != 1 => {
            at("a `list` has exactly one child: the item template")
        }
        _ => Ok(()),
    }
}

/// A template error at a byte offset into the template string.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TemplateError {
    /// Byte offset of the problem.
    pub offset: usize,
    /// What went wrong.
    pub message: String,
}

/// Parses a template: literal text, `{property}` interpolation, `{{` and
/// `}}` escapes. Expressions are rejected.
///
/// # Errors
/// [`TemplateError`] for an unclosed or stray brace, or for braces holding
/// anything but a property path.
pub fn parse_template(s: &str) -> Result<Template, TemplateError> {
    let mut segments = Vec::new();
    let mut lit = String::new();
    let mut iter = s.char_indices().peekable();
    while let Some((i, c)) = iter.next() {
        match c {
            '{' if iter.peek().map(|p| p.1) == Some('{') => {
                iter.next();
                lit.push('{');
            }
            '}' if iter.peek().map(|p| p.1) == Some('}') => {
                iter.next();
                lit.push('}');
            }
            '}' => {
                return Err(TemplateError {
                    offset: i,
                    message: "stray `}` in a template (write `}}` for a literal brace)".to_owned(),
                });
            }
            '{' => {
                let mut inner = String::new();
                let mut closed = false;
                for (_, n) in iter.by_ref() {
                    if n == '}' {
                        closed = true;
                        break;
                    }
                    inner.push(n);
                }
                if !closed {
                    return Err(TemplateError {
                        offset: i,
                        message: "unclosed `{` in a template (write `{{` for a literal brace)".to_owned(),
                    });
                }
                let name = inner.trim();
                if !is_property_path(name) {
                    return Err(TemplateError {
                        offset: i,
                        message: format!(
                            "`{{{inner}}}` is not a property: templates only interpolate `{{property}}`; \
                             the UI holds no formulas, so compute the value in a view model and bind it"
                        ),
                    });
                }
                if !lit.is_empty() {
                    segments.push(Segment::Literal(std::mem::take(&mut lit)));
                }
                segments.push(Segment::Property(name.to_owned()));
            }
            _ => lit.push(c),
        }
    }
    if !lit.is_empty() {
        segments.push(Segment::Literal(lit));
    }
    Ok(Template { segments })
}

/// Parses a layout file: theme blocks and exactly one root element. Ids
/// must be unique.
///
/// # Errors
/// [`MarkupError`] with the position of the first problem.
pub fn parse_layout(src: &str) -> Result<Document, MarkupError> {
    let mut p = Parser {
        toks: lex(src)?,
        pos: 0,
    };
    let (theme, mut elements) = p.file()?;
    if elements.len() > 1 {
        let extra = elements.get(1).map_or((1, 1), |e| (e.line, e.column));
        return Err(MarkupError::at(
            extra.0,
            extra.1,
            "a layout has exactly one root element",
        ));
    }
    let Some(root) = elements.pop() else {
        return Err(MarkupError::at(1, 1, "a layout needs a root element"));
    };
    let mut seen = HashSet::new();
    let mut dup = None;
    root.visit(&mut |e| {
        if let Some(id) = &e.id
            && !seen.insert(id.as_str())
            && dup.is_none()
        {
            dup = Some(MarkupError::at(e.line, e.column, format!("duplicate id `{id}`")));
        }
    });
    if let Some(e) = dup {
        return Err(e);
    }
    Ok(Document { theme, root })
}

/// Parses a theme file (theme blocks only).
///
/// # Errors
/// [`MarkupError`] with the position of the first problem.
pub fn parse_theme(src: &str) -> Result<Theme, MarkupError> {
    let mut p = Parser {
        toks: lex(src)?,
        pos: 0,
    };
    let (theme, elements) = p.file()?;
    if let Some(e) = elements.first() {
        return Err(MarkupError::at(
            e.line,
            e.column,
            "a theme file holds only `theme { ... }` blocks",
        ));
    }
    Ok(theme)
}

/// Checks that every `style=` names a style of `theme`.
///
/// # Errors
/// [`MarkupError`] at the first element naming an unknown style.
pub fn check_styles(root: &ElementSpec, theme: &Theme) -> Result<(), MarkupError> {
    let mut err = None;
    root.visit(&mut |e| {
        if let Some(name) = &e.style_name
            && !theme.styles.contains_key(name)
            && err.is_none()
        {
            err = Some(MarkupError::at(
                e.line,
                e.column,
                format!("unknown style `{name}`"),
            ));
        }
    });
    err.map_or(Ok(()), Err)
}

#[cfg(test)]
mod tests {
    use super::*;

    pub(crate) const SAMPLE: &str = r#"
theme {
  style title { font = "ui" size = 24 color = #ffffff }
  style panel { background = #20242cee radius = 6 padding = 8 border = 1 border_color = #ffffff22 }
  style button:hover { background = #3c4658 }
}
panel id=root style=panel direction=column gap=4 {
  text id=name bind="player.name" style=title
  text id=hp template="HP {hp} / {hp_max}"
  row gap=8 { button id=ok text="OK" intent="dialog.confirm"  button id=cancel text="Cancel" intent="dialog.cancel" }
  input id=chat placeholder="Say something" submit="chat.send"
  list id=items bind="inventory.items" height=grow:2 { text bind="item.name" }
}
"#;

    #[test]
    fn sample_parses() -> Result<(), Box<dyn std::error::Error>> {
        let doc = parse_layout(SAMPLE)?;
        assert_eq!(doc.root.kind, ElementKind::Panel);
        assert_eq!(doc.root.children.len(), 5);
        assert!(doc.theme.styles.contains_key("title"));
        let hover = &doc.theme.styles.get("button").ok_or("button")?.hover;
        assert!(hover.background.is_some());
        let hp = doc.root.children.get(1).ok_or("hp")?;
        assert_eq!(
            hp.text,
            Some(TextSource::Template(Template {
                segments: vec![
                    Segment::Literal("HP ".into()),
                    Segment::Property("hp".into()),
                    Segment::Literal(" / ".into()),
                    Segment::Property("hp_max".into()),
                ]
            }))
        );
        let list = doc.root.children.get(4).ok_or("list")?;
        assert_eq!(list.style.height, Some(SizeSpec::Grow(2.0)));
        check_styles(&doc.root, &doc.theme)?;
        Ok(())
    }

    #[test]
    fn enabled_and_disabled_text_parse() -> Result<(), Box<dyn std::error::Error>> {
        let doc = parse_layout(
            r#"theme { style b:disabled { background = #101010 } }
            row enabled="dialog.open" {
              button id=buy text="Buy" enabled="shop.can_buy" disabled_text="Need {gold} gold"
            }"#,
        )?;
        assert_eq!(doc.root.enabled.as_deref(), Some("dialog.open"));
        let buy = doc.root.children.first().ok_or("buy")?;
        assert_eq!(buy.enabled.as_deref(), Some("shop.can_buy"));
        assert_eq!(buy.disabled_text.as_ref().map(|t| t.segments.len()), Some(3));
        let b = doc.theme.styles.get("b").ok_or("style b")?;
        assert!(b.disabled.background.is_some());
        assert!(parse_layout(r#"text text="a" disabled_text="b""#).is_err());
        assert!(parse_layout(r#"panel enabled="a + b""#).is_err());
        assert!(parse_layout(r#"button disabled_text="{n * 2}""#).is_err());
        assert!(parse_theme("theme { style b:bogus { } }").is_err());
        Ok(())
    }

    #[test]
    fn templates_reject_formulas() {
        for bad in ["{hp * 2}", "{max(a, b)}", "{hp+1}", "{-hp}", "{1}", "{a b}", "{}"] {
            let e = parse_template(bad).err();
            let msg = e.map(|e| e.message).unwrap_or_default();
            assert!(msg.contains("no formulas"), "{bad}: {msg}");
        }
        assert!(
            parse_template("{{literal}}")
                .is_ok_and(|t| t.segments == vec![Segment::Literal("{literal}".into())])
        );
        assert!(parse_template("{ item.name }").is_ok());
        assert!(parse_template("open {").is_err());
        assert!(parse_template("close }").is_err());
    }

    #[test]
    fn formula_in_layout_has_position() {
        let src = "panel {\n  text template=\"HP {hp - 1}\"\n}";
        let e = parse_layout(src).err();
        let e = e.as_ref();
        assert_eq!(e.map(|e| (e.line, e.column)), Some((2, 21)));
        assert!(e.is_some_and(|e| e.message.contains("no formulas")));
    }

    #[test]
    fn errors_carry_positions() {
        let cases = [
            ("panel {", (1, 7)),
            ("panel id=a { text }", (1, 14)),
            ("blob", (1, 1)),
            ("panel width=wide", (1, 13)),
            ("panel { text id=a text=\"x\" text id=a text=\"y\" }", (1, 28)),
            ("panel color=#12", (1, 13)),
            ("panel\n  bogus=1", (2, 9)),
            ("text text=\"a\" bind=\"b\"", (1, 20)),
        ];
        for (src, pos) in cases {
            let e = parse_layout(src).err();
            assert_eq!(e.as_ref().map(|e| (e.line, e.column)), Some(pos), "{src}: {e:?}");
        }
        assert!(parse_theme("panel {}").is_err());
        assert!(parse_theme("theme { style a { gap = 2 } }").is_ok());
    }

    #[test]
    fn deep_nesting_is_an_error_not_a_crash() {
        let src = "panel {".repeat(200);
        assert!(parse_layout(&src).is_err());
    }
}

//! mantis-ui: the retained-mode, data-bound user interface (plan section 8.6).
//!
//! The UI is pure CPU. Each frame on the render thread it applies property
//! changes written by view models, lays out what changed, rasterizes any new
//! glyphs into an MSDF atlas, and fills one draw list of instanced quads that
//! the renderer submits as a single batched pass (rounded rectangles and MSDF
//! text share one quad format). No formula lives in the UI: view models hand
//! it finished display values; templates only interpolate them; widgets emit
//! intents and never call game code.
//!
//! # Modules
//!
//! - [`color`]: linear premultiplied colors parsed from sRGB hex.
//! - [`font`]: the font library, coverage queries, metrics, fallback stacks.
//! - [`text`]: itemization (bidi, script, per-cluster font fallback), shaping
//!   with `rustybuzz`, line breaking, visual reordering, cached layout.
//! - [`msdf`]: multi-channel signed distance fields from glyph outlines.
//! - [`atlas`]: the on-demand glyph atlas (shelf packing, growth, dirty
//!   rectangles).
//! - [`markup`]: the layout and theme syntax and its parser.
//! - [`bind`]: properties, values, and the view model trait.
//! - [`tree`]: the retained node arena and style resolution.
//! - [`layout`]: the flex-lite layout pass.
//! - [`input`]: input events, `Consumed`/`Ignored`, and intents.
//! - [`draw`]: the draw list contract ([`draw::UiQuad`]) and the CPU
//!   reference rasterizer that defines the shader math.
//! - [`reload`]: hot reload support (file watcher, reload report).
//! - [`test_font`]: a fixture builder for minimal TrueType fonts, for tests
//!   and tools.
//!
//! The entry point is [`Ui`].

#![forbid(unsafe_code)]

pub mod atlas;
pub mod bind;
pub mod color;
pub mod draw;
pub mod font;
pub mod input;
pub mod layout;
pub mod markup;
pub mod msdf;
pub mod reload;
pub mod test_font;
pub mod text;
pub mod tree;
mod ui;

pub use atlas::{AtlasConfig, AtlasRect, GlyphAtlas};
pub use bind::{ListItem, Properties, PropertyId, Value, ViewModel};
pub use color::Color;
pub use draw::{DrawList, UiQuad};
pub use font::{FontId, FontLibrary, FontStack};
pub use input::{Handled, Modifiers, PointerButton, UiEvent, UiIntent, UiKey};
pub use layout::Rect;
pub use markup::MarkupError;
pub use reload::{ReloadReport, SourceWatcher};
pub use tree::{IntentId, WidgetId};
pub use ui::Ui;

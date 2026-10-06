//! The [`Ui`]: a retained, data-bound UI driven once per frame on the render
//! thread.
//!
//! ```text
//! view models --(Properties)--> Ui::frame --> DrawList + GlyphAtlas --> renderer
//! platform input --> Ui::handle --> Consumed / Ignored, UiIntents --> client
//! ```

use unicode_segmentation::UnicodeSegmentation;

use crate::atlas::{AtlasConfig, GlyphAtlas};
use crate::bind::{Properties, ViewModel};
use crate::draw::{self, DrawList, Interaction};
use crate::font::FontLibrary;
use crate::input::{Handled, Modifiers, PointerButton, UiEvent, UiIntent, UiKey};
use crate::layout::{Layouter, Rect, display_caret};
use crate::markup::{self, Document, MarkupError, Theme};
use crate::reload::ReloadReport;
use crate::text::Shaper;
use crate::tree::{
    BuildCtx, InputState, IntentId, Interner, NodeId, NodeKind, Tree, WidgetId, render_template,
};

/// Pixels scrolled per wheel line.
const WHEEL_LINE_PX: f32 = 40.0;

/// A retained, data-bound user interface. See the crate documentation.
pub struct Ui {
    fonts: FontLibrary,
    shaper: Shaper,
    atlas: GlyphAtlas,
    props: Properties,
    package_theme: Theme,
    theme: Theme,
    tree: Tree,
    ids: Vec<String>,
    widgets: Interner,
    intents: Interner,
    draw: DrawList,
    pending: Vec<UiIntent>,
    inter: Interaction,
    pointer: [f32; 2],
    viewport: [f32; 2],
    scale: f32,
    layout_dirty: bool,
    glyphs_dirty: bool,
    seen_props: Option<u64>,
    scratch: String,
}

impl std::fmt::Debug for Ui {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Ui")
            .field("ids", &self.ids)
            .field("quads", &self.draw.len())
            .finish_non_exhaustive()
    }
}

/// What the pointer is over.
#[derive(Clone, Copy, Debug, Default)]
struct Hit {
    interactive: Option<NodeId>,
    list: Option<NodeId>,
    opaque: bool,
}

/// Static ids of a layout, sorted.
fn ids_of(doc: &Document) -> Vec<String> {
    let mut ids = Vec::new();
    doc.root.visit(&mut |e| {
        if let Some(id) = &e.id {
            ids.push(id.clone());
        }
    });
    ids.sort();
    ids
}

fn merged_theme(package: &Theme, doc: &Document) -> Theme {
    let mut t = package.clone();
    t.merge(&doc.theme);
    t
}

impl Ui {
    /// Builds a UI from a layout file and an optional package theme file,
    /// with the default atlas configuration.
    ///
    /// # Errors
    /// [`MarkupError`] when a source does not parse, a `style=` names an
    /// unknown style, or a style names an unknown font stack.
    pub fn new(
        fonts: FontLibrary,
        layout_source: &str,
        theme_source: Option<&str>,
    ) -> Result<Self, MarkupError> {
        Self::with_atlas(fonts, layout_source, theme_source, AtlasConfig::default())
    }

    /// Like [`Ui::new`] with an explicit atlas configuration.
    ///
    /// # Errors
    /// See [`Ui::new`].
    pub fn with_atlas(
        fonts: FontLibrary,
        layout_source: &str,
        theme_source: Option<&str>,
        atlas: AtlasConfig,
    ) -> Result<Self, MarkupError> {
        let package_theme = match theme_source {
            Some(src) => markup::parse_theme(src)?,
            None => Theme::default(),
        };
        let doc = markup::parse_layout(layout_source)?;
        let theme = merged_theme(&package_theme, &doc);
        markup::check_styles(&doc.root, &theme)?;
        let mut props = Properties::new();
        let mut widgets = Interner::default();
        let mut intents = Interner::default();
        let tree = Tree::build(
            &doc.root,
            &mut BuildCtx {
                theme: &theme,
                fonts: &fonts,
                props: &mut props,
                widgets: &mut widgets,
                intents: &mut intents,
            },
        )?;
        let mut ui = Self {
            fonts,
            shaper: Shaper::new(),
            atlas: GlyphAtlas::new(atlas),
            props,
            package_theme,
            theme,
            tree,
            ids: ids_of(&doc),
            widgets,
            intents,
            draw: DrawList::default(),
            pending: Vec::new(),
            inter: Interaction::default(),
            pointer: [-1.0, -1.0],
            viewport: [0.0, 0.0],
            scale: 1.0,
            layout_dirty: true,
            glyphs_dirty: true,
            seen_props: None,
            scratch: String::new(),
        };
        ui.refresh_all_inputs();
        Ok(ui)
    }

    /// The property store (read).
    #[must_use]
    pub fn properties(&self) -> &Properties {
        &self.props
    }

    /// The property store; view models write display values here.
    pub fn properties_mut(&mut self) -> &mut Properties {
        &mut self.props
    }

    /// Lets a view model observe one change set.
    pub fn observe<C, V: ViewModel<C>>(&mut self, view_model: &mut V, change: &C) {
        view_model.observe(change, &mut self.props);
    }

    /// The fonts.
    #[must_use]
    pub fn fonts(&self) -> &FontLibrary {
        &self.fonts
    }

    /// The glyph atlas the draw list's glyph quads sample.
    #[must_use]
    pub fn atlas(&self) -> &GlyphAtlas {
        &self.atlas
    }

    /// The glyph atlas, mutably (for [`GlyphAtlas::take_dirty`]).
    pub fn atlas_mut(&mut self) -> &mut GlyphAtlas {
        &mut self.atlas
    }

    /// The node tree.
    #[must_use]
    pub fn tree(&self) -> &Tree {
        &self.tree
    }

    /// The last built draw list.
    #[must_use]
    pub fn draw_list(&self) -> &DrawList {
        &self.draw
    }

    /// The last frame's draw list together with the atlas, for a renderer that reads the
    /// quads while uploading the atlas's dirty region.
    pub fn draw_parts(&mut self) -> (&DrawList, &mut GlyphAtlas) {
        (&self.draw, &mut self.atlas)
    }

    /// The id of an intent name, if any widget uses it. Never allocates.
    #[must_use]
    pub fn intent_id(&self, name: &str) -> Option<IntentId> {
        self.intents.get(name).map(IntentId)
    }

    /// The name of an intent.
    #[must_use]
    pub fn intent_name(&self, id: IntentId) -> Option<&str> {
        self.intents.name(id.0)
    }

    /// The id of a widget by its stable id string. Never allocates.
    #[must_use]
    pub fn widget_id(&self, name: &str) -> Option<WidgetId> {
        self.widgets.get(name).map(WidgetId)
    }

    /// The stable id string of a widget.
    #[must_use]
    pub fn widget_name(&self, id: WidgetId) -> Option<&str> {
        self.widgets.name(id.0)
    }

    /// Element ids of the current layout, sorted.
    #[must_use]
    pub fn ids(&self) -> &[String] {
        &self.ids
    }

    /// Whether the element with this id is enabled (its own `enabled=` flag
    /// and every ancestor's). `None` when no element has the id.
    #[must_use]
    pub fn is_enabled(&self, id: &str) -> Option<bool> {
        self.tree
            .find(id)
            .map(|n| self.tree.is_enabled_in(n, &self.props))
    }

    /// The widget with keyboard focus.
    #[must_use]
    pub fn focused(&self) -> Option<WidgetId> {
        self.inter
            .focus
            .and_then(|f| self.tree.get(f))
            .and_then(|n| n.widget)
    }

    /// The interactive widget under the pointer.
    #[must_use]
    pub fn hovered(&self) -> Option<WidgetId> {
        self.inter
            .hover
            .and_then(|f| self.tree.get(f))
            .and_then(|n| n.widget)
    }

    /// Focuses the focusable widget with this id. Returns false when there is
    /// none.
    pub fn set_focus(&mut self, name: &str) -> bool {
        match self.tree.find(name) {
            Some(id)
                if self.tree.get(id).is_some_and(crate::tree::Node::focusable)
                    && self.tree.is_enabled_in(id, &self.props) =>
            {
                self.change_focus(Some(id));
                true
            }
            _ => false,
        }
    }

    /// The layout rectangle (logical px) of the element with this id.
    #[must_use]
    pub fn rect_of(&self, name: &str) -> Option<Rect> {
        self.tree
            .find(name)
            .and_then(|id| self.tree.get(id))
            .map(|n| n.rect)
    }

    /// The displayed text of a text, button, or input element.
    #[must_use]
    pub fn text_of(&self, name: &str) -> Option<&str> {
        let n = self.tree.get(self.tree.find(name)?)?;
        n.text.as_ref().map(|t| t.content.as_str())
    }

    /// How many times an element's text was shaped.
    #[must_use]
    pub fn reshape_count(&self, name: &str) -> Option<u64> {
        let n = self.tree.get(self.tree.find(name)?)?;
        n.text.as_ref().map(|t| t.reshapes)
    }

    /// The editing state of a text input.
    #[must_use]
    pub fn input_state(&self, name: &str) -> Option<&InputState> {
        self.tree.get(self.tree.find(name)?)?.input.as_ref()
    }

    /// Moves queued intents into `out` (appending).
    pub fn drain_intents(&mut self, out: &mut Vec<UiIntent>) {
        out.append(&mut self.pending);
    }

    /// Applies property changes, lays out when dirty, rasterizes new glyphs,
    /// and rebuilds the draw list for a viewport of `viewport_px` physical
    /// pixels at `scale` physical pixels per logical pixel.
    pub fn frame(&mut self, viewport_px: [f32; 2], scale: f32) -> &DrawList {
        let scale = if scale.is_finite() && scale > 0.0 {
            scale
        } else {
            1.0
        };
        let logical = [viewport_px[0] / scale, viewport_px[1] / scale];
        if logical != self.viewport || scale != self.scale {
            self.viewport = logical;
            self.scale = scale;
            self.layout_dirty = true;
        }
        if self.seen_props != Some(self.props.global_version()) {
            let mut ctx = BuildCtx {
                theme: &self.theme,
                fonts: &self.fonts,
                props: &mut self.props,
                widgets: &mut self.widgets,
                intents: &mut self.intents,
            };
            if self.tree.sync(&mut ctx, &mut self.scratch) {
                self.layout_dirty = true;
            }
            self.seen_props = Some(self.props.global_version());
            self.validate_interaction();
        }
        if self.layout_dirty {
            let mut l = Layouter {
                fonts: &self.fonts,
                shaper: &mut self.shaper,
            };
            l.layout(&mut self.tree, self.viewport);
            self.layout_dirty = false;
            self.glyphs_dirty = true;
        }
        if self.glyphs_dirty {
            for id in self.tree.ids() {
                let Some(tb) = self.tree.get(id).and_then(|n| n.text.as_ref()) else {
                    continue;
                };
                for g in &tb.layout().glyphs {
                    let _ = self.atlas.ensure(&self.fonts, g.font, g.glyph);
                }
            }
            self.glyphs_dirty = false;
        }
        draw::build(&self.tree, &self.atlas, &mut self.draw, self.scale, self.inter);
        &self.draw
    }

    /// Drops interaction references to nodes that no longer exist.
    fn validate_interaction(&mut self) {
        let alive = |t: &Tree, n: Option<NodeId>| n.filter(|id| t.get(*id).is_some());
        self.inter.hover = alive(&self.tree, self.inter.hover);
        self.inter.pressed = alive(&self.tree, self.inter.pressed);
        self.inter.focus = alive(&self.tree, self.inter.focus);
        // Disabled widgets show no hover or press and lose focus.
        let (tree, props) = (&self.tree, &self.props);
        let enabled = |n: Option<NodeId>| n.filter(|id| tree.is_enabled_in(*id, props));
        self.inter.hover = enabled(self.inter.hover);
        self.inter.pressed = enabled(self.inter.pressed);
        if self
            .inter
            .focus
            .is_some_and(|f| !self.tree.is_enabled_in(f, &self.props))
        {
            self.change_focus(None);
        }
    }

    /// The caret rectangle of the focused text input in physical pixels, for
    /// placing the platform's IME candidate window.
    #[must_use]
    pub fn ime_cursor_area(&self) -> Option<Rect> {
        let id = self.inter.focus?;
        let node = self.tree.get(id)?;
        let input = node.input.as_ref()?;
        let tb = node.text.as_ref()?;
        let layout = tb.layout();
        let line = layout.lines.first()?;
        let inner = node.rect.inset(node.style.padding[0], node.style.padding[1]);
        let x0 = inner.x - input.scroll_x;
        let y0 = inner.y + ((inner.h - layout.size[1]) * 0.5).max(0.0);
        let caret = if node.showing_placeholder {
            0.0
        } else {
            layout.caret_x(display_caret(input)).1
        };
        Some(Rect::new(x0 + caret, y0 + line.top, 1.0, line.height).scaled(self.scale))
    }

    /// Handles one input event. Returns [`Handled::Ignored`] when the client
    /// should route the event to game actions instead.
    pub fn handle(&mut self, ev: &UiEvent) -> Handled {
        match ev {
            UiEvent::PointerMove { x, y } => {
                self.pointer = [x / self.scale, y / self.scale];
                let hit = self.hit();
                self.inter.hover = hit.interactive;
                consumed(hit.opaque)
            }
            UiEvent::PointerButton { button, pressed } => self.pointer_button(*button, *pressed),
            UiEvent::Wheel { dy, .. } => {
                let hit = self.hit();
                if let Some(list) = hit.list
                    && let Some(l) = self.tree.get_mut(list).and_then(|n| n.list.as_mut())
                {
                    l.scroll -= dy * WHEEL_LINE_PX;
                    self.layout_dirty = true;
                    return Handled::Consumed;
                }
                consumed(hit.opaque)
            }
            UiEvent::Key {
                key,
                pressed,
                modifiers,
            } => self.key(*key, *pressed, *modifiers),
            UiEvent::Text(s) => self.edit_focused(|input, max| insert(input, s, max)),
            UiEvent::ImePreedit { text, cursor } => self.edit_focused(|input, _| {
                if !text.is_empty() && input.preedit.is_empty() {
                    delete_selection(input);
                }
                input.preedit.clear();
                input.preedit.push_str(text);
                input.preedit_cursor = cursor.filter(|_| !text.is_empty());
            }),
            UiEvent::ImeCommit(s) => self.edit_focused(|input, max| {
                input.preedit.clear();
                input.preedit_cursor = None;
                insert(input, s, max);
            }),
            UiEvent::FocusLost => {
                self.inter.hover = None;
                self.inter.pressed = None;
                let _ = self.edit_focused(|input, _| {
                    input.preedit.clear();
                    input.preedit_cursor = None;
                });
                Handled::Ignored
            }
        }
    }

    fn pointer_button(&mut self, button: PointerButton, pressed: bool) -> Handled {
        let hit = self.hit();
        if button != PointerButton::Left {
            return consumed(hit.opaque);
        }
        if pressed {
            self.inter.pressed = hit.interactive;
            let kind = hit.interactive.and_then(|i| self.tree.get(i)).map(|n| n.kind);
            if kind == Some(NodeKind::Input) {
                self.change_focus(hit.interactive);
                if let Some(id) = hit.interactive {
                    self.place_caret(id);
                }
            } else if self
                .inter
                .focus
                .and_then(|f| self.tree.get(f))
                .is_some_and(|n| n.kind == NodeKind::Input)
                || kind.is_none()
            {
                self.change_focus(None);
            }
            return consumed(hit.opaque);
        }
        let was = self.inter.pressed.take();
        if let Some(b) = was
            && hit.interactive == Some(b)
            && self.tree.get(b).is_some_and(|n| n.kind == NodeKind::Button)
        {
            self.click(b);
        }
        consumed(was.is_some() || hit.opaque)
    }

    fn key(&mut self, key: UiKey, pressed: bool, modifiers: Modifiers) -> Handled {
        if self
            .inter
            .focus
            .is_some_and(|f| !self.tree.is_enabled_in(f, &self.props))
        {
            // Disabled since the last frame: it cannot keep focus.
            self.change_focus(None);
        }
        let focus = self
            .inter
            .focus
            .and_then(|f| self.tree.get(f).map(|n| (f, n.kind)));
        let typing = matches!(focus, Some((_, NodeKind::Input)));
        if !pressed {
            return consumed(typing);
        }
        match (key, focus) {
            (UiKey::Tab, _) => consumed(self.traverse(modifiers.shift)),
            (UiKey::Escape, Some(_)) => {
                self.change_focus(None);
                Handled::Consumed
            }
            (UiKey::Enter | UiKey::Space, Some((id, NodeKind::Button))) => {
                self.click(id);
                Handled::Consumed
            }
            (_, Some((id, NodeKind::Input))) => {
                self.edit_key(id, key, modifiers);
                Handled::Consumed
            }
            _ => Handled::Ignored,
        }
    }

    /// Queues a button's intent.
    fn click(&mut self, id: NodeId) {
        if !self.tree.is_enabled_in(id, &self.props) {
            return;
        }
        let Some(node) = self.tree.get(id) else { return };
        let (Some(intent), Some(widget)) = (node.intent, node.widget) else {
            return;
        };
        let payload = node.payload.as_ref().map(|p| {
            let mut s = String::new();
            render_template(p, &self.props, &mut s);
            s
        });
        self.pending.push(UiIntent {
            intent,
            widget,
            payload,
        });
    }

    fn change_focus(&mut self, to: Option<NodeId>) {
        if self.inter.focus == to {
            return;
        }
        if let Some(old) = self.inter.focus {
            if let Some(input) = self.tree.get_mut(old).and_then(|n| n.input.as_mut()) {
                input.preedit.clear();
                input.preedit_cursor = None;
                input.anchor = input.caret;
            }
            self.refresh_input(old);
        }
        self.inter.focus = to;
    }

    /// Places the caret of input `id` at the pointer.
    fn place_caret(&mut self, id: NodeId) {
        let pointer = self.pointer;
        let Some(node) = self.tree.get_mut(id) else { return };
        if node.showing_placeholder {
            return;
        }
        let inner = node.rect.inset(node.style.padding[0], node.style.padding[1]);
        let (Some(input), Some(tb)) = (node.input.as_mut(), node.text.as_ref()) else {
            return;
        };
        if !input.preedit.is_empty() {
            return;
        }
        let x = pointer[0] - inner.x + input.scroll_x;
        let at = tb.layout().hit_test(x, 0.0).min(input.text.len());
        input.caret = at;
        input.anchor = at;
        self.layout_dirty = true;
    }

    /// Moves focus to the next (or previous) focusable widget in tree order.
    /// Returns false when nothing is focusable.
    fn traverse(&mut self, backwards: bool) -> bool {
        #[derive(Default)]
        struct Walk {
            current: Option<NodeId>,
            first: Option<NodeId>,
            last: Option<NodeId>,
            before: Option<NodeId>,
            after: Option<NodeId>,
            seen: bool,
        }
        fn visit(tree: &Tree, props: &Properties, id: NodeId, w: &mut Walk) {
            let Some(n) = tree.get(id) else { return };
            if !n.visible || !n.enabled_now(props) {
                return;
            }
            if n.focusable() {
                if w.first.is_none() {
                    w.first = Some(id);
                }
                if Some(id) == w.current {
                    w.seen = true;
                } else if w.seen {
                    if w.after.is_none() {
                        w.after = Some(id);
                    }
                } else {
                    w.before = Some(id);
                }
                w.last = Some(id);
            }
            for c in &n.children {
                visit(tree, props, *c, w);
            }
        }
        let mut w = Walk {
            current: self.inter.focus,
            ..Walk::default()
        };
        visit(&self.tree, &self.props, self.tree.root(), &mut w);
        let next = if !w.seen {
            if backwards { w.last } else { w.first }
        } else if backwards {
            w.before.or(w.last)
        } else {
            w.after.or(w.first)
        };
        if next.is_none() {
            return false;
        }
        self.change_focus(next);
        true
    }

    fn edit_key(&mut self, id: NodeId, key: UiKey, m: Modifiers) {
        let Some(node) = self.tree.get_mut(id) else { return };
        let submit = node.submit;
        let widget = node.widget;
        let Some(input) = node.input.as_mut() else { return };
        if !input.preedit.is_empty() {
            return;
        }
        let len = input.text.len();
        let mut submitted = None;
        match key {
            UiKey::Backspace => {
                if input.selection().is_empty() {
                    input.anchor = prev_boundary(&input.text, input.caret);
                }
                delete_selection(input);
            }
            UiKey::Delete => {
                if input.selection().is_empty() {
                    input.anchor = next_boundary(&input.text, input.caret);
                }
                delete_selection(input);
            }
            UiKey::Left | UiKey::Right => {
                let sel = input.selection();
                let left = key == UiKey::Left;
                input.caret = if !m.shift && !sel.is_empty() {
                    if left { sel.start } else { sel.end }
                } else if left {
                    prev_boundary(&input.text, input.caret)
                } else {
                    next_boundary(&input.text, input.caret)
                };
                if !m.shift {
                    input.anchor = input.caret;
                }
            }
            UiKey::Home | UiKey::End => {
                input.caret = if key == UiKey::Home { 0 } else { len };
                if !m.shift {
                    input.anchor = input.caret;
                }
            }
            UiKey::Char('a' | 'A') if m.ctrl => {
                input.anchor = 0;
                input.caret = len;
            }
            UiKey::Enter => {
                submitted = Some(std::mem::take(&mut input.text));
                input.caret = 0;
                input.anchor = 0;
                input.scroll_x = 0.0;
            }
            _ => return,
        }
        if let (Some(text), Some(intent), Some(widget)) = (submitted, submit, widget) {
            self.pending.push(UiIntent {
                intent,
                widget,
                payload: Some(text),
            });
        }
        self.refresh_input(id);
        // The caret moved: the input may need to scroll.
        self.layout_dirty = true;
    }

    /// Applies `f` to the focused text input. Ignored when no input has focus.
    fn edit_focused(&mut self, f: impl FnOnce(&mut InputState, Option<u32>)) -> Handled {
        let Some(id) = self.inter.focus else {
            return Handled::Ignored;
        };
        if !self.tree.is_enabled_in(id, &self.props) {
            return Handled::Ignored;
        }
        let Some(node) = self.tree.get_mut(id) else {
            return Handled::Ignored;
        };
        let max = node.max_length;
        let Some(input) = node.input.as_mut() else {
            return Handled::Ignored;
        };
        f(input, max);
        self.refresh_input(id);
        self.layout_dirty = true;
        Handled::Consumed
    }

    /// Rebuilds an input's display text (committed text with the preedit
    /// spliced in, or the placeholder) and marks layout dirty.
    fn refresh_input(&mut self, id: NodeId) {
        let Some(node) = self.tree.get_mut(id) else { return };
        let Some(input) = node.input.as_ref() else { return };
        let scratch = &mut self.scratch;
        scratch.clear();
        let placeholder = input.text.is_empty() && input.preedit.is_empty();
        if placeholder {
            scratch.push_str(node.placeholder.as_deref().unwrap_or(""));
        } else {
            let caret = input.caret.min(input.text.len());
            scratch.push_str(input.text.get(..caret).unwrap_or(""));
            scratch.push_str(&input.preedit);
            scratch.push_str(input.text.get(caret..).unwrap_or(""));
        }
        let changed = node.showing_placeholder != placeholder;
        node.showing_placeholder = placeholder;
        let text_changed = node.text.as_mut().is_some_and(|tb| tb.set(scratch));
        if changed || text_changed {
            self.layout_dirty = true;
        }
    }

    fn refresh_all_inputs(&mut self) {
        let inputs: Vec<NodeId> = self
            .tree
            .ids()
            .filter(|id| self.tree.get(*id).is_some_and(|n| n.kind == NodeKind::Input))
            .collect();
        for id in inputs {
            self.refresh_input(id);
        }
    }

    /// Hit test at the pointer.
    fn hit(&self) -> Hit {
        fn walk(tree: &Tree, id: NodeId, p: [f32; 2], clip: [f32; 4]) -> Option<NodeId> {
            let n = tree.get(id)?;
            if !n.visible {
                return None;
            }
            for c in n.children.iter().rev() {
                if let Some(h) = walk(tree, *c, p, n.clip) {
                    return Some(h);
                }
            }
            let in_clip = p[0] >= clip[0] && p[1] >= clip[1] && p[0] < clip[2] && p[1] < clip[3];
            (in_clip && n.rect.contains(p[0], p[1])).then_some(id)
        }
        let mut hit = Hit::default();
        let viewport = [0.0, 0.0, self.viewport[0], self.viewport[1]];
        let mut cur = walk(&self.tree, self.tree.root(), self.pointer, viewport);
        while let Some(id) = cur {
            let Some(n) = self.tree.get(id) else { break };
            match n.kind {
                NodeKind::Button | NodeKind::Input if hit.interactive.is_none() => {
                    // A disabled widget still swallows the pointer but is
                    // not interactive.
                    if self.tree.is_enabled_in(id, &self.props) {
                        hit.interactive = Some(id);
                    }
                    hit.opaque = true;
                }
                NodeKind::List if hit.list.is_none() => {
                    hit.list = Some(id);
                    hit.opaque = true;
                }
                _ => {}
            }
            if !n.style.background.is_transparent()
                || (n.style.border > 0.0 && !n.style.border_color.is_transparent())
            {
                hit.opaque = true;
            }
            cur = n.parent;
        }
        hit
    }

    /// Replaces the layout (and optionally the package theme) while running.
    ///
    /// On error nothing changes: the old tree stays live. On success the
    /// state of elements whose id still exists is preserved: focus, hover,
    /// text input contents and caret, and list scroll.
    ///
    /// # Errors
    /// [`MarkupError`] as for [`Ui::new`].
    pub fn reload(
        &mut self,
        layout_source: &str,
        theme_source: Option<&str>,
    ) -> Result<ReloadReport, MarkupError> {
        let package_theme = match theme_source {
            Some(src) => markup::parse_theme(src)?,
            None => self.package_theme.clone(),
        };
        let doc = markup::parse_layout(layout_source)?;
        let theme = merged_theme(&package_theme, &doc);
        markup::check_styles(&doc.root, &theme)?;
        let mut tree = Tree::build(
            &doc.root,
            &mut BuildCtx {
                theme: &theme,
                fonts: &self.fonts,
                props: &mut self.props,
                widgets: &mut self.widgets,
                intents: &mut self.intents,
            },
        )?;
        let new_ids = ids_of(&doc);
        // Carry state over by id.
        for name in &new_ids {
            let (Some(old), Some(new)) = (self.tree.find(name), tree.find(name)) else {
                continue;
            };
            let Some(o) = self.tree.get(old) else { continue };
            let (input, scroll) = (o.input.clone(), o.list.as_ref().map(|l| l.scroll));
            if let Some(n) = tree.get_mut(new) {
                if n.input.is_some()
                    && let Some(i) = input
                {
                    n.input = Some(i);
                }
                if let (Some(l), Some(s)) = (n.list.as_mut(), scroll) {
                    l.scroll = s;
                }
            }
        }
        let remap = |old: &Tree, new: &Tree, id: Option<NodeId>| -> Option<NodeId> {
            let name = old.get(id?)?.name.as_deref()?;
            new.find(name)
        };
        let inter = Interaction {
            hover: remap(&self.tree, &tree, self.inter.hover),
            pressed: None,
            focus: remap(&self.tree, &tree, self.inter.focus),
        };
        let report = ReloadReport {
            added: new_ids
                .iter()
                .filter(|i| !self.ids.contains(i))
                .cloned()
                .collect(),
            removed: self
                .ids
                .iter()
                .filter(|i| !new_ids.contains(i))
                .cloned()
                .collect(),
            kept: new_ids.iter().filter(|i| self.ids.contains(i)).cloned().collect(),
        };
        std::mem::swap(&mut self.tree, &mut tree);
        self.inter = inter;
        self.package_theme = package_theme;
        self.theme = theme;
        self.ids = new_ids;
        self.layout_dirty = true;
        self.seen_props = None;
        self.refresh_all_inputs();
        Ok(report)
    }
}

fn consumed(yes: bool) -> Handled {
    if yes { Handled::Consumed } else { Handled::Ignored }
}

fn prev_boundary(text: &str, at: usize) -> usize {
    text.get(..at)
        .and_then(|s| s.grapheme_indices(true).next_back())
        .map_or(0, |(i, _)| i)
}

fn next_boundary(text: &str, at: usize) -> usize {
    text.get(at..)
        .and_then(|s| s.graphemes(true).next())
        .map_or(text.len(), |g| at + g.len())
}

fn delete_selection(input: &mut InputState) {
    let sel = input.selection();
    if sel.is_empty() || input.text.get(sel.clone()).is_none() {
        input.anchor = input.caret;
        return;
    }
    input.text.replace_range(sel.clone(), "");
    input.caret = sel.start;
    input.anchor = sel.start;
}

fn insert(input: &mut InputState, s: &str, max: Option<u32>) {
    delete_selection(input);
    let have = input.text.chars().count();
    let room = max.map_or(usize::MAX, |m| (m as usize).saturating_sub(have));
    let mut at = input.caret.min(input.text.len());
    for c in s.chars().filter(|c| !c.is_control()).take(room) {
        if !input.text.is_char_boundary(at) {
            break;
        }
        input.text.insert(at, c);
        at += c.len_utf8();
    }
    input.caret = at;
    input.anchor = at;
}

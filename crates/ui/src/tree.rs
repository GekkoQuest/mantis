//! The retained node tree.
//!
//! A [`Tree`] is an arena of [`Node`]s built from a parsed layout
//! ([`crate::markup::ElementSpec`]) and a theme. Styles are resolved once at
//! build time: built-in defaults, then the theme style named after the
//! element kind, then the `style=` style, then inline attributes. Text,
//! visibility, and list bindings are resolved to interned [`PropertyId`]s.
//!
//! Nodes keep stable string ids (`id=` in the layout); list instances get
//! derived ids `list[index]` and `list[index].child`. Per-node caches (shaped
//! text, laid-out lines, the layout rectangle) live on the node, so a frame
//! with no changes touches nothing but the draw list.

use std::collections::HashMap;

use crate::bind::{Properties, PropertyId, Value};
use crate::color::Color;
use crate::font::{FontLibrary, StackId};
use crate::layout::Rect;
use crate::markup::{
    Align, ElementKind, ElementSpec, Flow, Justify, MarkupError, Segment, SizeSpec, StyleSpec, Template,
    TextSource, Theme,
};
use crate::text::{ShapedText, Shaper, TextAlign, TextLayout, TextStyle};

/// Index of a node in a [`Tree`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct NodeId(pub u32);

/// Interned widget id (a node's stable string id).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct WidgetId(pub u32);

/// Interned intent name.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct IntentId(pub u32);

/// String interner used for widget ids and intent names.
#[derive(Debug, Default, Clone)]
pub struct Interner {
    map: HashMap<String, u32>,
    names: Vec<String>,
}

impl Interner {
    /// The index of `name`, interning it on first use.
    pub fn intern(&mut self, name: &str) -> u32 {
        if let Some(i) = self.map.get(name) {
            return *i;
        }
        let i = u32::try_from(self.names.len()).unwrap_or(u32::MAX);
        self.names.push(name.to_owned());
        self.map.insert(name.to_owned(), i);
        i
    }

    /// The index of `name` if interned. Never allocates.
    #[must_use]
    pub fn get(&self, name: &str) -> Option<u32> {
        self.map.get(name).copied()
    }

    /// The name of an index.
    #[must_use]
    pub fn name(&self, i: u32) -> Option<&str> {
        self.names.get(i as usize).map(String::as_str)
    }
}

/// What a node is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NodeKind {
    /// A container (row or column).
    Panel,
    /// A text label.
    Text,
    /// A button with a label.
    Button,
    /// A single-line text input.
    Input,
    /// A container repeating a template per list item.
    List,
    /// Empty space.
    Spacer,
}

/// A style with every value resolved.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ResolvedStyle {
    /// Container direction.
    pub direction: Flow,
    /// Width.
    pub width: SizeSpec,
    /// Height.
    pub height: SizeSpec,
    /// Minimum width and height, px.
    pub min: [f32; 2],
    /// Maximum width and height, px.
    pub max: [f32; 2],
    /// Horizontal and vertical padding, px.
    pub padding: [f32; 2],
    /// Space between children, px.
    pub gap: f32,
    /// Cross-axis alignment.
    pub align: Align,
    /// Main-axis distribution.
    pub justify: Justify,
    /// Fill.
    pub background: Color,
    /// Border width, px.
    pub border: f32,
    /// Border color.
    pub border_color: Color,
    /// Corner radius, px.
    pub radius: f32,
    /// Text color.
    pub color: Color,
    /// Font stack.
    pub stack: StackId,
    /// Font size, px.
    pub size: f32,
    /// Text alignment.
    pub text_align: TextAlign,
    /// Clip children (and text) to the node rectangle.
    pub clip: bool,
}

/// Color and border overrides for one interaction state.
#[derive(Clone, Copy, Debug, PartialEq, Default)]
pub struct StateStyle {
    /// Fill.
    pub background: Option<Color>,
    /// Border width.
    pub border: Option<f32>,
    /// Border color.
    pub border_color: Option<Color>,
    /// Text color.
    pub color: Option<Color>,
}

impl StateStyle {
    fn from_spec(s: &StyleSpec) -> Self {
        Self {
            background: s.background,
            border: s.border,
            border_color: s.border_color,
            color: s.color,
        }
    }

    fn merge(&mut self, o: &Self) {
        self.background = o.background.or(self.background);
        self.border = o.border.or(self.border);
        self.border_color = o.border_color.or(self.border_color);
        self.color = o.color.or(self.color);
    }
}

/// A binding source.
#[derive(Clone, Debug, PartialEq)]
pub enum BindRef {
    /// A global property.
    Global(PropertyId),
    /// A field of one item of a list property.
    Item {
        /// The list property.
        list: PropertyId,
        /// The item index.
        index: usize,
        /// The field name.
        field: String,
    },
}

impl BindRef {
    fn property(&self) -> PropertyId {
        match self {
            Self::Global(p) | Self::Item { list: p, .. } => *p,
        }
    }

    fn resolve<'a>(&self, props: &'a Properties) -> Option<&'a Value> {
        match self {
            Self::Global(p) => props.get(*p),
            Self::Item { list, index, field } => props.get(*list)?.as_list()?.get(*index)?.field(field),
        }
    }
}

/// One part of a resolved template.
#[derive(Clone, Debug, PartialEq)]
pub enum TemplatePart {
    /// Literal text.
    Lit(String),
    /// An interpolated property.
    Prop(BindRef),
}

/// Where a node's text comes from.
#[derive(Clone, Debug, PartialEq)]
pub enum TextBinding {
    /// Fixed text.
    Literal(String),
    /// A property shown as is.
    Bind(BindRef),
    /// A template.
    Template(Vec<TemplatePart>),
}

/// Text content with its shaping and layout caches.
#[derive(Clone, Debug, Default)]
pub struct TextBox {
    /// The current text.
    pub content: String,
    pub(crate) shaped: ShapedText,
    pub(crate) shaped_valid: bool,
    pub(crate) layout: TextLayout,
    pub(crate) layout_max: Option<f32>,
    pub(crate) layout_valid: bool,
    /// Number of times the text was shaped (diagnostics and tests).
    pub reshapes: u64,
}

impl TextBox {
    fn new(content: String) -> Self {
        Self {
            content,
            ..Self::default()
        }
    }

    /// Replaces the content; returns true (and invalidates caches) when it
    /// changed. Reuses the string's capacity.
    pub fn set(&mut self, text: &str) -> bool {
        if self.content == text {
            return false;
        }
        self.content.clear();
        self.content.push_str(text);
        self.shaped_valid = false;
        self.layout_valid = false;
        true
    }

    /// Shapes (when the text or style changed) and lays out (when the wrap
    /// width changed in a way that can change the lines).
    pub fn ensure(
        &mut self,
        fonts: &FontLibrary,
        shaper: &mut Shaper,
        style: TextStyle,
        max_width: Option<f32>,
    ) -> &TextLayout {
        if !self.shaped_valid || self.shaped.style() != Some(style) {
            shaper.shape(fonts, &self.content, style, &mut self.shaped);
            self.shaped_valid = true;
            self.layout_valid = false;
            self.reshapes += 1;
        }
        let reusable = self.layout_valid && {
            let cw = self.layout.size[0];
            match (self.layout_max, max_width) {
                (None, None) => true,
                (None, Some(w)) => cw <= w + 0.01,
                (Some(_), None) => false,
                (Some(p), Some(w)) => cw <= w + 0.01 && w <= p + 0.01,
            }
        };
        if !reusable {
            self.shaped.layout(max_width, &mut self.layout);
            self.layout_max = max_width;
            self.layout_valid = true;
        }
        &self.layout
    }

    /// The current layout (possibly stale until the next layout pass).
    #[must_use]
    pub fn layout(&self) -> &TextLayout {
        &self.layout
    }
}

/// Editing state of a text input.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct InputState {
    /// Committed text.
    pub text: String,
    /// Caret byte index (a grapheme boundary).
    pub caret: usize,
    /// Selection anchor byte index (equal to `caret` when nothing is selected).
    pub anchor: usize,
    /// IME composition shown inline at the caret, not yet committed.
    pub preedit: String,
    /// Caret or selection inside the preedit, byte offsets.
    pub preedit_cursor: Option<(usize, usize)>,
    /// Horizontal scroll of the content, px.
    pub scroll_x: f32,
}

impl InputState {
    /// The selected byte range (empty when nothing is selected).
    #[must_use]
    pub fn selection(&self) -> std::ops::Range<usize> {
        self.caret.min(self.anchor)..self.caret.max(self.anchor)
    }
}

/// List repetition state.
#[derive(Clone, Debug)]
pub struct ListState {
    /// The bound list property.
    pub prop: PropertyId,
    /// The item template.
    pub template: ElementSpec,
    /// Number of instantiated items.
    pub count: usize,
    /// Vertical scroll, px.
    pub scroll: f32,
    /// Height of the items, px.
    pub content: f32,
    /// The list property version last applied (`None` before the first sync).
    pub synced: Option<u64>,
}

/// One node of the tree.
#[derive(Clone, Debug)]
#[allow(clippy::struct_excessive_bools)] // independent per-node flags (visible, enabled, label and placeholder state)
pub struct Node {
    /// Kind.
    pub kind: NodeKind,
    /// Stable id string, if any.
    pub name: Option<String>,
    /// Interned id used in intents.
    pub widget: Option<WidgetId>,
    /// Parent node.
    pub parent: Option<NodeId>,
    /// Children in order.
    pub children: Vec<NodeId>,
    /// Resolved style.
    pub style: ResolvedStyle,
    /// Hover overrides.
    pub hover: StateStyle,
    /// Pressed overrides.
    pub pressed: StateStyle,
    /// Focus overrides.
    pub focus: StateStyle,
    /// Disabled overrides (unset fields fall back to the built-in fade).
    pub disabled: StateStyle,
    /// Text source.
    pub binding: Option<TextBinding>,
    /// Text content and caches (text, button label, input display).
    pub text: Option<TextBox>,
    /// Button intent.
    pub intent: Option<IntentId>,
    /// Button payload template.
    pub payload: Option<Vec<TemplatePart>>,
    /// Input submit intent.
    pub submit: Option<IntentId>,
    /// Input placeholder.
    pub placeholder: Option<String>,
    /// Input maximum length, characters.
    pub max_length: Option<u32>,
    /// Visibility flag.
    pub visible_bind: Option<BindRef>,
    /// Current visibility of this node alone.
    pub visible: bool,
    /// Enabled flag.
    pub enabled_bind: Option<BindRef>,
    /// Current enabled flag of this node alone (see [`Tree::is_enabled`]).
    pub enabled: bool,
    /// Button label shown while disabled.
    pub disabled_text: Option<Vec<TemplatePart>>,
    /// The label currently shows `disabled_text`.
    pub showing_disabled_text: bool,
    /// List state.
    pub list: Option<ListState>,
    /// Input state.
    pub input: Option<InputState>,
    /// The input display shows the placeholder.
    pub showing_placeholder: bool,
    /// Highest property version this node's bindings have applied (`None`
    /// before the first sync).
    pub synced: Option<u64>,
    /// Layout rectangle, logical px.
    pub rect: Rect,
    /// Clip rectangle x0, y0, x1, y1, logical px.
    pub clip: [f32; 4],
}

impl Node {
    /// True for widgets that take keyboard focus.
    #[must_use]
    pub fn focusable(&self) -> bool {
        matches!(self.kind, NodeKind::Button | NodeKind::Input)
    }

    /// This node's own `enabled=` flag read from `props` now (true when
    /// unbound, unset, or not a flag).
    #[must_use]
    pub fn enabled_now(&self, props: &Properties) -> bool {
        self.enabled_bind
            .as_ref()
            .is_none_or(|b| b.resolve(props).and_then(Value::as_bool).unwrap_or(true))
    }

    /// The text style of this node.
    #[must_use]
    pub fn text_style(&self) -> TextStyle {
        TextStyle {
            stack: self.style.stack,
            size: self.style.size,
        }
    }
}

/// Context for building nodes.
pub struct BuildCtx<'a> {
    /// Merged theme.
    pub theme: &'a Theme,
    /// Fonts (for stack names).
    pub fonts: &'a FontLibrary,
    /// Property store (names are interned).
    pub props: &'a mut Properties,
    /// Widget id interner.
    pub widgets: &'a mut Interner,
    /// Intent interner.
    pub intents: &'a mut Interner,
}

/// The retained node arena.
#[derive(Clone, Debug)]
pub struct Tree {
    nodes: Vec<Option<Node>>,
    free: Vec<u32>,
    root: NodeId,
}

/// Item context for list instances: (list property, index, list id).
type ItemCtx<'a> = Option<(PropertyId, usize, &'a str)>;

impl Tree {
    /// Builds a tree from a layout root.
    ///
    /// # Errors
    /// [`MarkupError`] when a style names an unknown font stack.
    pub fn build(root: &ElementSpec, ctx: &mut BuildCtx<'_>) -> Result<Self, MarkupError> {
        let mut tree = Self {
            nodes: Vec::new(),
            free: Vec::new(),
            root: NodeId(0),
        };
        tree.root = tree.instantiate(root, None, None, ctx)?;
        Ok(tree)
    }

    /// The root node.
    #[must_use]
    pub fn root(&self) -> NodeId {
        self.root
    }

    /// A node.
    #[must_use]
    pub fn get(&self, id: NodeId) -> Option<&Node> {
        self.nodes.get(id.0 as usize).and_then(Option::as_ref)
    }

    /// A node, mutably.
    pub fn get_mut(&mut self, id: NodeId) -> Option<&mut Node> {
        self.nodes.get_mut(id.0 as usize).and_then(Option::as_mut)
    }

    /// Number of slots (live and free); node ids are below this.
    #[must_use]
    pub fn capacity(&self) -> usize {
        self.nodes.len()
    }

    /// Finds a node by its stable id string.
    #[must_use]
    pub fn find(&self, name: &str) -> Option<NodeId> {
        self.nodes.iter().enumerate().find_map(|(i, n)| {
            let n = n.as_ref()?;
            (n.name.as_deref() == Some(name)).then(|| NodeId(u32::try_from(i).unwrap_or(0)))
        })
    }

    /// Calls `f` for every live node id.
    pub fn ids(&self) -> impl Iterator<Item = NodeId> + '_ {
        self.nodes
            .iter()
            .enumerate()
            .filter_map(|(i, n)| n.as_ref().map(|_| NodeId(u32::try_from(i).unwrap_or(u32::MAX))))
    }

    /// True when the node and all its ancestors are enabled, reading the
    /// `enabled=` flags from `props` now (not as of the last frame). Never
    /// allocates.
    #[must_use]
    pub fn is_enabled_in(&self, id: NodeId, props: &Properties) -> bool {
        let mut cur = Some(id);
        while let Some(c) = cur {
            match self.get(c) {
                Some(n) if n.enabled_now(props) => cur = n.parent,
                _ => return false,
            }
        }
        true
    }

    /// True when the node and all its ancestors are enabled, as of the last
    /// frame's binding pass.
    #[must_use]
    pub fn is_enabled(&self, id: NodeId) -> bool {
        let mut cur = Some(id);
        while let Some(c) = cur {
            match self.get(c) {
                Some(n) if n.enabled => cur = n.parent,
                _ => return false,
            }
        }
        true
    }

    fn alloc(&mut self, node: Node) -> NodeId {
        if let Some(i) = self.free.pop() {
            if let Some(slot) = self.nodes.get_mut(i as usize) {
                *slot = Some(node);
            }
            return NodeId(i);
        }
        self.nodes.push(Some(node));
        NodeId(u32::try_from(self.nodes.len() - 1).unwrap_or(u32::MAX))
    }

    fn remove_subtree(&mut self, id: NodeId) {
        let children = self.get(id).map(|n| n.children.clone()).unwrap_or_default();
        for c in children {
            self.remove_subtree(c);
        }
        if let Some(slot) = self.nodes.get_mut(id.0 as usize) {
            *slot = None;
            self.free.push(id.0);
        }
    }

    fn instantiate(
        &mut self,
        spec: &ElementSpec,
        parent: Option<NodeId>,
        item: ItemCtx<'_>,
        ctx: &mut BuildCtx<'_>,
    ) -> Result<NodeId, MarkupError> {
        let mut node = make_node(spec, parent, item, ctx)?;
        let children: Vec<&ElementSpec> = if spec.kind == ElementKind::List {
            Vec::new()
        } else {
            spec.children.iter().collect()
        };
        node.children.clear();
        let id = self.alloc(node);
        let mut kids = Vec::with_capacity(children.len());
        for c in children {
            kids.push(self.instantiate(c, Some(id), item, ctx)?);
        }
        if let Some(n) = self.get_mut(id) {
            n.children = kids;
        }
        Ok(id)
    }

    /// Applies property changes: rebuilds list instances whose count changed,
    /// re-renders bound text, and re-evaluates visibility. Returns true when
    /// anything that affects layout changed.
    pub fn sync(&mut self, ctx: &mut BuildCtx<'_>, scratch: &mut String) -> bool {
        let mut dirty = false;
        let mut i = 0;
        while i < self.nodes.len() {
            let id = NodeId(u32::try_from(i).unwrap_or(u32::MAX));
            dirty |= self.sync_list(id, ctx);
            i += 1;
        }
        for slot in &mut self.nodes {
            let Some(node) = slot.as_mut() else { continue };
            dirty |= sync_node(node, ctx.props, scratch);
        }
        // Swap button labels whose effective enabled state changed.
        for i in 0..self.nodes.len() {
            let id = NodeId(u32::try_from(i).unwrap_or(u32::MAX));
            let Some(showing) = self
                .get(id)
                .filter(|n| n.disabled_text.is_some())
                .map(|n| n.showing_disabled_text)
            else {
                continue;
            };
            let disabled = !self.is_enabled(id);
            if disabled != showing
                && let Some(node) = self.get_mut(id)
            {
                node.showing_disabled_text = disabled;
                dirty |= render_text(node, ctx.props, scratch);
            }
        }
        dirty
    }

    fn sync_list(&mut self, id: NodeId, ctx: &mut BuildCtx<'_>) -> bool {
        let Some(node) = self.get(id) else {
            return false;
        };
        let Some(list) = &node.list else {
            return false;
        };
        let version = ctx.props.version(list.prop);
        if list.synced.is_some_and(|v| version <= v) {
            return false;
        }
        let want = ctx
            .props
            .get(list.prop)
            .and_then(Value::as_list)
            .map_or(0, <[_]>::len);
        let (prop, have, template) = (list.prop, list.count, list.template.clone());
        let list_name = node.name.clone().unwrap_or_default();
        if let Some(l) = self.get_mut(id).and_then(|n| n.list.as_mut()) {
            l.synced = Some(version);
        }
        if want == have {
            // Same count: item-bound descendants refresh through their own
            // versions (they depend on the list property).
            return false;
        }
        let old = self.get(id).map(|n| n.children.clone()).unwrap_or_default();
        for c in old {
            self.remove_subtree(c);
        }
        let mut kids = Vec::with_capacity(want);
        for index in 0..want {
            // Template errors (unknown font stack) were caught when the
            // template was first validated; skip an item rather than fail.
            if let Ok(c) = self.instantiate(&template, Some(id), Some((prop, index, &list_name)), ctx) {
                if let Some(n) = self.get_mut(c)
                    && n.name.is_none()
                {
                    let name = format!("{list_name}[{index}]");
                    n.widget = Some(WidgetId(ctx.widgets.intern(&name)));
                    n.name = Some(name);
                }
                kids.push(c);
            }
        }
        if let Some(n) = self.get_mut(id) {
            n.children = kids;
            if let Some(l) = n.list.as_mut() {
                l.count = want;
            }
        }
        true
    }
}

/// Refreshes one node's bindings. Returns true when layout is affected.
fn sync_node(node: &mut Node, props: &Properties, scratch: &mut String) -> bool {
    let mut deps_version = 0_u64;
    let mut any = false;
    let mut track = |b: &BindRef| {
        deps_version = deps_version.max(props.version(b.property()));
        any = true;
    };
    match &node.binding {
        Some(TextBinding::Bind(b)) => track(b),
        Some(TextBinding::Template(parts)) => {
            for p in parts {
                if let TemplatePart::Prop(b) = p {
                    track(b);
                }
            }
        }
        _ => {}
    }
    if let Some(parts) = &node.disabled_text {
        for p in parts {
            if let TemplatePart::Prop(b) = p {
                track(b);
            }
        }
    }
    for b in [&node.visible_bind, &node.enabled_bind].into_iter().flatten() {
        track(b);
    }
    if !any || node.synced.is_some_and(|v| deps_version <= v) {
        return false;
    }
    node.synced = Some(deps_version);
    let mut dirty = false;
    if let Some(b) = &node.visible_bind {
        let visible = b.resolve(props).and_then(Value::as_bool).unwrap_or(true);
        if visible != node.visible {
            node.visible = visible;
            dirty = true;
        }
    }
    if let Some(b) = &node.enabled_bind {
        // Paint-only: colors change, layout does not.
        node.enabled = b.resolve(props).and_then(Value::as_bool).unwrap_or(true);
    }
    dirty | render_text(node, props, scratch)
}

/// Renders a node's text from its binding (or its disabled label). Returns
/// true when the text changed.
fn render_text(node: &mut Node, props: &Properties, scratch: &mut String) -> bool {
    scratch.clear();
    match (&node.disabled_text, &node.binding) {
        (Some(parts), Some(_)) if node.showing_disabled_text => render_template(parts, props, scratch),
        (_, Some(TextBinding::Literal(s))) => scratch.push_str(s),
        (_, Some(TextBinding::Bind(b))) => {
            if let Some(v) = b.resolve(props) {
                v.write_display(scratch);
            }
        }
        (_, Some(TextBinding::Template(parts))) => render_template(parts, props, scratch),
        (_, None) => return false,
    }
    node.text.as_mut().is_some_and(|tb| tb.set(scratch))
}

/// Renders a resolved template into `out`.
pub fn render_template(parts: &[TemplatePart], props: &Properties, out: &mut String) {
    out.clear();
    for p in parts {
        match p {
            TemplatePart::Lit(s) => out.push_str(s),
            TemplatePart::Prop(b) => {
                if let Some(v) = b.resolve(props) {
                    v.write_display(out);
                }
            }
        }
    }
}

fn bind_ref(path: &str, item: ItemCtx<'_>, props: &mut Properties) -> BindRef {
    if let Some((list, index, _)) = item
        && let Some(field) = path.strip_prefix("item.")
    {
        return BindRef::Item {
            list,
            index,
            field: field.to_owned(),
        };
    }
    BindRef::Global(props.intern(path))
}

fn resolve_template(t: &Template, item: ItemCtx<'_>, props: &mut Properties) -> Vec<TemplatePart> {
    t.segments
        .iter()
        .map(|s| match s {
            Segment::Literal(l) => TemplatePart::Lit(l.clone()),
            Segment::Property(p) => TemplatePart::Prop(bind_ref(p, item, props)),
        })
        .collect()
}

fn defaults(kind: ElementKind) -> StyleSpec {
    let mut s = StyleSpec {
        color: Some(Color::WHITE),
        size: Some(16.0),
        ..StyleSpec::default()
    };
    match kind {
        ElementKind::Row => s.direction = Some(Flow::Row),
        ElementKind::List => s.clip = Some(true),
        ElementKind::Button => {
            s.padding_x = Some(12.0);
            s.padding_y = Some(6.0);
            s.radius = Some(4.0);
            s.background = Color::from_hex("#3a4150");
            s.text_align = Some(TextAlign::Center);
        }
        ElementKind::Input => {
            s.width = Some(SizeSpec::Px(200.0));
            s.padding = Some(6.0);
            s.radius = Some(3.0);
            s.border = Some(1.0);
            s.background = Color::from_hex("#15181e");
            s.border_color = Color::from_hex("#ffffff40");
            s.clip = Some(true);
        }
        ElementKind::Spacer => {
            s.width = Some(SizeSpec::Grow(1.0));
            s.height = Some(SizeSpec::Grow(1.0));
        }
        ElementKind::Panel | ElementKind::Column | ElementKind::Text => {}
    }
    s
}

/// Resolves a node's style and its state variants.
fn node_style(
    spec: &ElementSpec,
    ctx: &BuildCtx<'_>,
) -> Result<(ResolvedStyle, [StateStyle; 4]), MarkupError> {
    let mut merged = defaults(spec.kind);
    let mut states = [StateStyle::default(); 4];
    let mut apply = |def: Option<&crate::markup::StyleDef>, merged: &mut StyleSpec| {
        if let Some(d) = def {
            merged.merge(&d.base);
            for (st, s) in states
                .iter_mut()
                .zip([&d.hover, &d.pressed, &d.focus, &d.disabled])
            {
                st.merge(&StateStyle::from_spec(s));
            }
        }
    };
    apply(ctx.theme.styles.get(spec.kind.name()), &mut merged);
    if let Some(name) = &spec.style_name {
        apply(ctx.theme.styles.get(name), &mut merged);
    }
    merged.merge(&spec.style);
    Ok((resolve(&merged, spec, ctx.fonts)?, states))
}

fn list_state(spec: &ElementSpec, props: &mut Properties) -> Option<ListState> {
    if spec.kind != ElementKind::List {
        return None;
    }
    let prop = props.intern(spec.bind.as_deref()?);
    Some(ListState {
        prop,
        template: spec.children.first()?.clone(),
        count: 0,
        scroll: 0.0,
        content: 0.0,
        synced: None,
    })
}

fn make_node(
    spec: &ElementSpec,
    parent: Option<NodeId>,
    item: ItemCtx<'_>,
    ctx: &mut BuildCtx<'_>,
) -> Result<Node, MarkupError> {
    let (style, [hover, pressed, focus, disabled]) = node_style(spec, ctx)?;

    let name = match (&spec.id, item) {
        (Some(id), Some((_, index, list))) => Some(format!("{list}[{index}].{id}")),
        (Some(id), None) => Some(id.clone()),
        _ => None,
    };
    let kind = match spec.kind {
        ElementKind::Panel | ElementKind::Row | ElementKind::Column => NodeKind::Panel,
        ElementKind::Text => NodeKind::Text,
        ElementKind::Button => NodeKind::Button,
        ElementKind::Input => NodeKind::Input,
        ElementKind::List => NodeKind::List,
        ElementKind::Spacer => NodeKind::Spacer,
    };
    let interactive = matches!(kind, NodeKind::Button | NodeKind::Input);
    let widget = match (&name, interactive) {
        (Some(n), _) => Some(WidgetId(ctx.widgets.intern(n))),
        (None, true) => {
            let auto = format!("{}@{}:{}", spec.kind.name(), spec.line, spec.column);
            Some(WidgetId(ctx.widgets.intern(&auto)))
        }
        (None, false) => None,
    };
    let (binding, initial) = match &spec.text {
        Some(TextSource::Literal(s)) => (Some(TextBinding::Literal(s.clone())), s.clone()),
        Some(TextSource::Bind(p)) => (
            Some(TextBinding::Bind(bind_ref(p, item, ctx.props))),
            String::new(),
        ),
        Some(TextSource::Template(t)) => (
            Some(TextBinding::Template(resolve_template(t, item, ctx.props))),
            String::new(),
        ),
        None => (None, String::new()),
    };
    let has_text = matches!(kind, NodeKind::Text | NodeKind::Button | NodeKind::Input);
    let list = list_state(spec, ctx.props);
    Ok(Node {
        kind,
        name,
        widget,
        parent,
        children: Vec::new(),
        style,
        hover,
        pressed,
        focus,
        disabled,
        binding,
        text: has_text.then(|| TextBox::new(initial)),
        intent: spec.intent.as_deref().map(|i| IntentId(ctx.intents.intern(i))),
        payload: spec
            .payload
            .as_ref()
            .map(|t| resolve_template(t, item, ctx.props)),
        submit: spec.submit.as_deref().map(|i| IntentId(ctx.intents.intern(i))),
        placeholder: spec.placeholder.clone(),
        max_length: spec.max_length,
        visible_bind: spec.visible.as_deref().map(|v| bind_ref(v, item, ctx.props)),
        visible: true,
        enabled_bind: spec.enabled.as_deref().map(|v| bind_ref(v, item, ctx.props)),
        enabled: true,
        disabled_text: spec
            .disabled_text
            .as_ref()
            .map(|t| resolve_template(t, item, ctx.props)),
        showing_disabled_text: false,
        list,
        input: (kind == NodeKind::Input).then(InputState::default),
        showing_placeholder: false,
        synced: None,
        rect: Rect::default(),
        clip: [0.0; 4],
    })
}

fn resolve(s: &StyleSpec, spec: &ElementSpec, fonts: &FontLibrary) -> Result<ResolvedStyle, MarkupError> {
    let stack = match &s.font {
        Some(name) => fonts.stack_id(name).ok_or_else(|| MarkupError {
            line: spec.line,
            column: spec.column,
            message: format!("unknown font stack `{name}`"),
        })?,
        None => fonts.default_stack(),
    };
    let pad = s.padding.unwrap_or(0.0);
    let default_dir = if spec.kind == ElementKind::Row {
        Flow::Row
    } else {
        Flow::Column
    };
    Ok(ResolvedStyle {
        direction: s.direction.unwrap_or(default_dir),
        width: s.width.unwrap_or(SizeSpec::Fit),
        height: s.height.unwrap_or(SizeSpec::Fit),
        min: [s.min_width.unwrap_or(0.0), s.min_height.unwrap_or(0.0)],
        max: [
            s.max_width.unwrap_or(f32::INFINITY),
            s.max_height.unwrap_or(f32::INFINITY),
        ],
        padding: [s.padding_x.unwrap_or(pad), s.padding_y.unwrap_or(pad)],
        gap: s.gap.unwrap_or(0.0),
        align: s.align.unwrap_or(Align::Start),
        justify: s.justify.unwrap_or(Justify::Start),
        background: s.background.unwrap_or(Color::TRANSPARENT),
        border: s.border.unwrap_or(0.0),
        border_color: s.border_color.unwrap_or(Color::TRANSPARENT),
        radius: s.radius.unwrap_or(0.0),
        color: s.color.unwrap_or(Color::WHITE),
        stack,
        size: s.size.unwrap_or(16.0).max(1.0),
        text_align: s.text_align.unwrap_or(TextAlign::Start),
        clip: s.clip.unwrap_or(false),
    })
}

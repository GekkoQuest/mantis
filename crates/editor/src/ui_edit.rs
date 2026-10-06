//! UI layout editing with hot reload: attributes of layout elements (by `id`) and
//! properties of theme styles are edited in the markup text, checked with the runtime
//! parser, and applied to the live [`mantis_ui::Ui`] through `Ui::reload`, which keeps
//! node state by id. A source edited outside the editor is picked up by
//! [`LayoutEditor::poll`] and reloaded the same way. A text that does not parse is never
//! applied: the old tree stays live and the error names its line and column.

use std::path::{Path, PathBuf};

use mantis_ui::markup::{MarkupError, parse_layout, parse_theme};
use mantis_ui::{ReloadReport, SourceWatcher, Ui};

/// An attribute value as markup writes it.
#[derive(Clone, PartialEq, Debug)]
pub enum Attr {
    /// A quoted string.
    Text(String),
    /// A number.
    Number(f32),
    /// A bare word or color (`grow`, `column`, `#20242cee`), written as is.
    Word(String),
}

impl Attr {
    fn markup(&self) -> String {
        match self {
            Attr::Text(s) => {
                let escaped = s
                    .replace('\\', "\\\\")
                    .replace('"', "\\\"")
                    .replace('\n', "\\n")
                    .replace('\t', "\\t");
                format!("\"{escaped}\"")
            }
            Attr::Number(n) => format!("{n}"),
            Attr::Word(w) => w.clone(),
        }
    }
}

/// Errors of layout editing.
#[derive(Debug)]
pub enum LayoutEditError {
    /// A file could not be read or written.
    Io(PathBuf, std::io::Error),
    /// The edited text does not parse (nothing was applied).
    Markup(MarkupError),
    /// No element with this id, or no style with this name.
    NotFound(String),
}

impl core::fmt::Display for LayoutEditError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Io(p, e) => write!(f, "{}: {e}", p.display()),
            Self::Markup(e) => write!(f, "line {}, column {}: {}", e.line, e.column, e.message),
            Self::NotFound(what) => write!(f, "no {what}"),
        }
    }
}

impl std::error::Error for LayoutEditError {}

/// Byte ranges of the `key=value` tokens on a markup line (outside strings).
fn attr_spans(line: &str) -> Vec<(String, core::ops::Range<usize>)> {
    let mut out = Vec::new();
    let bytes = line.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        // A token starts at a non-space after a space (or at the start).
        while i < bytes.len() && bytes.get(i).is_some_and(u8::is_ascii_whitespace) {
            i += 1;
        }
        let start = i;
        let mut in_string = false;
        let mut escaped = false;
        while let Some(&b) = bytes.get(i) {
            if in_string {
                if escaped {
                    escaped = false;
                } else if b == b'\\' {
                    escaped = true;
                } else if b == b'"' {
                    in_string = false;
                }
            } else if b == b'"' {
                in_string = true;
            } else if b.is_ascii_whitespace() {
                break;
            }
            i += 1;
        }
        if let Some(token) = line.get(start..i)
            && let Some((key, _)) = token.split_once('=')
            && !key.is_empty()
            && key.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
        {
            out.push((key.to_owned(), start..i));
        }
    }
    out
}

fn has_id(line: &str, id: &str) -> bool {
    attr_spans(line)
        .iter()
        .any(|(k, r)| k == "id" && line.get(r.clone()) == Some(&format!("id={id}")))
}

/// A layout (and optional theme) source being edited.
#[derive(Debug)]
pub struct LayoutEditor {
    layout_path: PathBuf,
    theme_path: Option<PathBuf>,
    layout: String,
    theme: Option<String>,
    layout_watch: SourceWatcher,
    theme_watch: Option<SourceWatcher>,
    dirty: bool,
}

fn read(path: &Path) -> Result<String, LayoutEditError> {
    std::fs::read_to_string(path).map_err(|e| LayoutEditError::Io(path.to_path_buf(), e))
}

impl LayoutEditor {
    /// Opens a layout file and its optional theme file.
    ///
    /// # Errors
    /// [`LayoutEditError::Io`] or [`LayoutEditError::Markup`].
    pub fn open(layout: &Path, theme: Option<&Path>) -> Result<Self, LayoutEditError> {
        let layout_text = read(layout)?;
        parse_layout(&layout_text).map_err(LayoutEditError::Markup)?;
        let theme_text = theme.map(read).transpose()?;
        if let Some(t) = &theme_text {
            parse_theme(t).map_err(LayoutEditError::Markup)?;
        }
        let mut layout_watch = SourceWatcher::new(layout);
        let _ = layout_watch.poll();
        let theme_watch = theme.map(|t| {
            let mut w = SourceWatcher::new(t);
            let _ = w.poll();
            w
        });
        Ok(Self {
            layout_path: layout.to_path_buf(),
            theme_path: theme.map(Path::to_path_buf),
            layout: layout_text,
            theme: theme_text,
            layout_watch,
            theme_watch,
            dirty: false,
        })
    }

    /// The layout text as edited.
    pub fn layout(&self) -> &str {
        &self.layout
    }

    /// The theme text as edited.
    pub fn theme(&self) -> Option<&str> {
        self.theme.as_deref()
    }

    /// Sets attribute `attr` of the element with `id`: the token is rewritten in place, or
    /// added after the id when absent.
    ///
    /// # Errors
    /// [`LayoutEditError::NotFound`] or [`LayoutEditError::Markup`] (the text is unchanged).
    pub fn set_attr(&mut self, id: &str, attr: &str, value: &Attr) -> Result<(), LayoutEditError> {
        let mut lines: Vec<String> = self.layout.lines().map(str::to_owned).collect();
        let line = lines
            .iter_mut()
            .find(|l| has_id(l, id))
            .ok_or_else(|| LayoutEditError::NotFound(format!("element `{id}`")))?;
        let token = format!("{attr}={}", value.markup());
        let spans = attr_spans(line);
        if let Some((_, range)) = spans.iter().find(|(k, _)| k == attr) {
            line.replace_range(range.clone(), &token);
        } else if let Some((_, range)) = spans.iter().find(|(k, _)| k == "id") {
            line.insert_str(range.end, &format!(" {token}"));
        }
        let mut text = lines.join("\n");
        if self.layout.ends_with('\n') {
            text.push('\n');
        }
        parse_layout(&text).map_err(LayoutEditError::Markup)?;
        self.layout = text;
        self.dirty = true;
        Ok(())
    }

    /// Sets property `prop` of theme style `style` (`style <name> { ... }`) to the markup
    /// value `value`, in the theme file (or the layout's own theme block when there is no
    /// theme file).
    ///
    /// # Errors
    /// [`LayoutEditError::NotFound`] or [`LayoutEditError::Markup`] (the text is unchanged).
    pub fn set_style(&mut self, style: &str, prop: &str, value: &Attr) -> Result<(), LayoutEditError> {
        let source = self.theme.as_ref().unwrap_or(&self.layout);
        let mut lines: Vec<String> = source.lines().map(str::to_owned).collect();
        let opener = format!("style {style} ");
        let line = lines
            .iter_mut()
            .find(|l| l.trim_start().starts_with(&opener) || l.trim_start() == format!("style {style}{{"))
            .ok_or_else(|| LayoutEditError::NotFound(format!("style `{style}`")))?;
        let open = line
            .find('{')
            .ok_or_else(|| LayoutEditError::NotFound(format!("style `{style}` on one line")))?;
        let close = line.rfind('}').unwrap_or(line.len());
        let body = line.get(open + 1..close).unwrap_or("").to_owned();
        let assignment = format!("{prop} = {}", value.markup());
        let mut parts: Vec<String> = Vec::new();
        let mut words = body.split_whitespace().peekable();
        let mut replaced = false;
        while let Some(key) = words.next() {
            if words.peek() == Some(&"=") {
                let _ = words.next();
                let mut v = words.next().unwrap_or("").to_owned();
                // A quoted value may contain spaces.
                while v.starts_with('"') && !(v.len() > 1 && v.ends_with('"')) {
                    match words.next() {
                        Some(more) => {
                            v.push(' ');
                            v.push_str(more);
                        }
                        None => break,
                    }
                }
                if key == prop {
                    parts.push(assignment.clone());
                    replaced = true;
                } else {
                    parts.push(format!("{key} = {v}"));
                }
            } else {
                parts.push(key.to_owned());
            }
        }
        if !replaced {
            parts.push(assignment);
        }
        let head = line.get(..open).unwrap_or("").to_owned();
        let tail = line.get(close..).unwrap_or("}").to_owned();
        *line = format!("{head}{{ {} {tail}", parts.join(" "));
        let mut text = lines.join("\n");
        if source.ends_with('\n') {
            text.push('\n');
        }
        if self.theme.is_some() {
            parse_theme(&text).map_err(LayoutEditError::Markup)?;
            self.theme = Some(text);
        } else {
            parse_layout(&text).map_err(LayoutEditError::Markup)?;
            self.layout = text;
        }
        self.dirty = true;
        Ok(())
    }

    /// Hot-reloads the edited text into `ui` (node state carried over by id).
    ///
    /// # Errors
    /// [`LayoutEditError::Markup`] when the live UI refuses it (an unknown style or font
    /// stack); the old tree stays live.
    pub fn apply(&self, ui: &mut Ui) -> Result<ReloadReport, LayoutEditError> {
        ui.reload(&self.layout, self.theme.as_deref())
            .map_err(LayoutEditError::Markup)
    }

    /// Writes the edited files.
    ///
    /// # Errors
    /// [`LayoutEditError::Io`].
    pub fn save(&mut self) -> Result<(), LayoutEditError> {
        std::fs::write(&self.layout_path, &self.layout)
            .map_err(|e| LayoutEditError::Io(self.layout_path.clone(), e))?;
        if let (Some(path), Some(text)) = (&self.theme_path, &self.theme) {
            std::fs::write(path, text).map_err(|e| LayoutEditError::Io(path.clone(), e))?;
        }
        // Our own write is not an outside change.
        let _ = self.layout_watch.poll();
        if let Some(w) = self.theme_watch.as_mut() {
            let _ = w.poll();
        }
        self.dirty = false;
        Ok(())
    }

    /// Whether there are unsaved edits.
    pub fn is_dirty(&self) -> bool {
        self.dirty
    }

    /// Picks up changes made to the files outside the editor and reloads them into
    /// `ui`. Returns the report when something was reloaded.
    ///
    /// # Errors
    /// [`LayoutEditError::Io`], or [`LayoutEditError::Markup`] for an outside edit that
    /// does not parse (the editor and the live UI keep the last good text).
    pub fn poll(&mut self, ui: &mut Ui) -> Result<Option<ReloadReport>, LayoutEditError> {
        let layout = self
            .layout_watch
            .poll()
            .map_err(|e| LayoutEditError::Io(self.layout_path.clone(), e))?;
        let theme = match (self.theme_watch.as_mut(), &self.theme_path) {
            (Some(w), Some(p)) => w.poll().map_err(|e| LayoutEditError::Io(p.clone(), e))?,
            _ => None,
        };
        if layout.is_none() && theme.is_none() {
            return Ok(None);
        }
        let layout = layout.unwrap_or_else(|| self.layout.clone());
        let theme = theme.or_else(|| self.theme.clone());
        let report = ui
            .reload(&layout, theme.as_deref())
            .map_err(LayoutEditError::Markup)?;
        self.layout = layout;
        self.theme = theme;
        Ok(Some(report))
    }
}

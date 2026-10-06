//! Loading and checking one mod folder ([`ModPackage`]), and scanning a mods directory
//! ([`scan`]).
//!
//! `client/mod.toml`:
//!
//! ```toml
//! [mod]
//! tier = "automation"     # or "presentation" (the default)
//! demote = true           # run at the presentation tier where automation is not permitted
//! screens = ["panel"]     # client/panel.layout, registered as screen `<key>.panel`
//! theme = "style"         # client/style.theme (optional)
//! scripts = ["main"]      # scripts/main.luau, loaded in this order
//! ```
//!
//! Every check here is local to the folder; the checks against the package (permitted
//! keys, names taken by modules, dependencies) are [`super::ModHost::new`]'s.

use std::path::Path;

use mantis_adapter_contract::ModTier;
use mantis_core::content::ContentHash;
use mantis_core::module::toml::{self, Document};
use mantis_core::module::{Dependency, Version, is_valid_key, parse_manifest};
use mantis_ui::markup::{ElementKind, ElementSpec, parse_layout, parse_theme};

use super::ModNotice;

/// Largest file a mod may hold, in bytes.
pub const MAX_FILE: usize = 256 * 1024;
/// Most screens one mod may register.
pub const MAX_SCREENS: usize = 16;
/// Most scripts one mod may load.
pub const MAX_SCRIPTS: usize = 16;
/// Most mod folders read from one directory.
pub const MAX_FOLDERS: usize = 64;

/// A mod folder was refused.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct ModError {
    /// The file, relative to the mod folder.
    pub file: String,
    /// Why.
    pub why: String,
}

impl core::fmt::Display for ModError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "{}: {}", self.file, self.why)
    }
}

impl std::error::Error for ModError {}

fn refuse(file: &str, why: impl Into<String>) -> ModError {
    ModError {
        file: file.to_owned(),
        why: why.into(),
    }
}

/// A screen a mod draws.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct ModScreen {
    /// The screen key, `<mod key>.<name>`.
    pub key: String,
    /// Layout markup (one root element, checked).
    pub layout: String,
}

/// One loaded and checked mod folder.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct ModPackage {
    /// The mod key (its manifest's `module.key`).
    pub key: String,
    /// Its version.
    pub version: Version,
    /// Contracts it depends on.
    pub dependencies: Vec<Dependency>,
    /// The tier it asks for.
    pub tier: ModTier,
    /// Whether an automation mod may run at the presentation tier where automation is
    /// not permitted (otherwise it is stopped there).
    pub demote: bool,
    /// Its screens, in the listed order.
    pub screens: Vec<ModScreen>,
    /// Theme source for its styles.
    pub theme: Option<String>,
    /// Its scripts: (name, source), in load order.
    pub scripts: Vec<(String, String)>,
    /// Content hash over every file read, in read order (path, length, bytes): the hash
    /// announced in `Hello`.
    pub hash: ContentHash,
    /// Where it came from (the folder name), for notices.
    pub origin: String,
}

/// Collects files as they are read, for the content hash.
struct Reader<'a> {
    read: &'a mut dyn FnMut(&str) -> Result<String, String>,
    hashed: Vec<u8>,
}

impl Reader<'_> {
    fn file(&mut self, path: &str) -> Result<String, ModError> {
        let text = (self.read)(path).map_err(|why| refuse(path, why))?;
        if text.len() > MAX_FILE {
            return Err(refuse(path, format!("larger than {MAX_FILE} bytes")));
        }
        self.hashed.extend_from_slice(path.as_bytes());
        self.hashed.push(0);
        self.hashed.extend_from_slice(&(text.len() as u64).to_le_bytes());
        self.hashed.extend_from_slice(text.as_bytes());
        Ok(text)
    }
}

/// True for a lowercase name of letters, digits, and underscores, starting with a letter.
fn is_name(s: &str) -> bool {
    s.starts_with(|c: char| c.is_ascii_lowercase())
        && s.chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
        && s.len() <= 32
}

/// The `[mod]` table of `client/mod.toml`.
struct ModSection {
    tier: ModTier,
    demote: bool,
    screens: Vec<String>,
    theme: Option<String>,
    scripts: Vec<String>,
}

const MOD_TOML: &str = "client/mod.toml";

fn names(doc: &Document, key: &'static str, most: usize) -> Result<Vec<String>, ModError> {
    let list = match doc.table("mod").and_then(|t| t.get(key)) {
        None => Vec::new(),
        Some(v) => v
            .as_strings()
            .ok_or_else(|| refuse(MOD_TOML, format!("`{key}`: expected an array of strings")))?,
    };
    if list.len() > most {
        return Err(refuse(MOD_TOML, format!("`{key}`: at most {most}")));
    }
    for (i, n) in list.iter().enumerate() {
        if !is_name(n) {
            return Err(refuse(
                MOD_TOML,
                format!("`{key}`: `{n}` is not a lowercase name (letters, digits, `_`)"),
            ));
        }
        if list.iter().take(i).any(|m| m == n) {
            return Err(refuse(MOD_TOML, format!("`{key}`: `{n}` is listed twice")));
        }
    }
    Ok(list)
}

fn parse_mod_section(text: &str) -> Result<ModSection, ModError> {
    let doc = toml::parse(text).map_err(|e| refuse(MOD_TOML, e.to_string()))?;
    for t in &doc.tables {
        let allowed: &[&str] = match t.name.as_str() {
            "" => &[],
            "mod" => &["tier", "demote", "screens", "theme", "scripts"],
            other => return Err(refuse(MOD_TOML, format!("unknown table `[{other}]`"))),
        };
        if let Some(e) = t.entries.iter().find(|e| !allowed.contains(&e.key.as_str())) {
            return Err(refuse(
                MOD_TOML,
                format!("line {}: unknown key `{}`", e.line, e.key),
            ));
        }
    }
    let table = doc.table("mod");
    let tier = match table.and_then(|t| t.get("tier")).map(toml::Value::as_str) {
        None | Some(Some("presentation")) => ModTier::Presentation,
        Some(Some("automation")) => ModTier::Automation,
        Some(_) => {
            return Err(refuse(
                MOD_TOML,
                "`tier`: expected \"presentation\" or \"automation\"",
            ));
        }
    };
    let demote = match table.and_then(|t| t.get("demote")) {
        None => false,
        Some(v) => v
            .as_bool()
            .ok_or_else(|| refuse(MOD_TOML, "`demote`: expected true or false"))?,
    };
    if demote && tier == ModTier::Presentation {
        return Err(refuse(MOD_TOML, "`demote` applies only to automation mods"));
    }
    let theme = match table.and_then(|t| t.get("theme")) {
        None => None,
        Some(v) => {
            let n = v
                .as_str()
                .filter(|n| is_name(n))
                .ok_or_else(|| refuse(MOD_TOML, "`theme`: expected a lowercase name"))?;
            Some(n.to_owned())
        }
    };
    Ok(ModSection {
        tier,
        demote,
        screens: names(&doc, "screens", MAX_SCREENS)?,
        theme,
        scripts: names(&doc, "scripts", MAX_SCRIPTS)?,
    })
}

/// True when `intent` is `<key>.<something>`.
pub(crate) fn own_intent(key: &str, intent: &str) -> bool {
    intent
        .strip_prefix(key)
        .and_then(|rest| rest.strip_prefix('.'))
        .is_some_and(|rest| !rest.is_empty())
}

/// Checks a screen layout: every element id carries `prefix`, every button and input has
/// an id (so each intent is attributed to its mod), no inline styles, and, for a
/// presentation mod, no intent outside its own namespace.
fn check_layout(file: &str, key: &str, prefix: &str, tier: ModTier, src: &str) -> Result<(), ModError> {
    let doc = parse_layout(src).map_err(|e| refuse(file, e.to_string()))?;
    if !doc.theme.styles.is_empty() {
        return Err(refuse(file, "styles belong in the mod's theme file"));
    }
    let mut stack: Vec<&ElementSpec> = vec![&doc.root];
    while let Some(el) = stack.pop() {
        let at = format!("line {}", el.line);
        let interactive = matches!(el.kind, ElementKind::Button | ElementKind::Input);
        let intents = [el.intent.as_deref(), el.submit.as_deref()];
        match &el.id {
            Some(id) if !id.starts_with(prefix) => {
                return Err(refuse(
                    file,
                    format!("{at}: element id `{id}` must start with `{prefix}`"),
                ));
            }
            None if interactive || intents.iter().any(Option::is_some) => {
                return Err(refuse(
                    file,
                    format!("{at}: every button and input needs an id starting with `{prefix}`"),
                ));
            }
            _ => {}
        }
        for intent in intents.into_iter().flatten() {
            if !is_valid_key(intent) {
                return Err(refuse(file, format!("{at}: `{intent}` is not an intent name")));
            }
            if tier == ModTier::Presentation && !own_intent(key, intent) {
                return Err(refuse(
                    file,
                    format!(
                        "{at}: a presentation mod's widgets may emit only its own intents \
                         (`{key}.*`), not `{intent}`"
                    ),
                ));
            }
        }
        stack.extend(el.children.iter());
    }
    Ok(())
}

fn check_theme(file: &str, prefix: &str, src: &str) -> Result<(), ModError> {
    let theme = parse_theme(src).map_err(|e| refuse(file, e.to_string()))?;
    if let Some(name) = theme.styles.keys().find(|n| !n.starts_with(prefix)) {
        return Err(refuse(
            file,
            format!("style `{name}` must start with `{prefix}` (a mod styles only its own elements)"),
        ));
    }
    Ok(())
}

impl ModPackage {
    /// The element id prefix every id in the mod's layouts carries (`toy.hud` →
    /// `toy_hud_`).
    pub fn id_prefix(&self) -> String {
        prefix_of(&self.key)
    }

    /// Loads a mod from files read through `read` (paths relative to the mod folder,
    /// `/`-separated), `origin` naming it in notices.
    ///
    /// # Errors
    /// [`ModError`]: a file that cannot be read, a malformed manifest or `client/mod.toml`,
    /// or a layout, theme, or script name that breaks the rules in [`super`].
    pub fn from_files(
        origin: &str,
        read: &mut dyn FnMut(&str) -> Result<String, String>,
    ) -> Result<Self, ModError> {
        let mut r = Reader {
            read,
            hashed: Vec::new(),
        };
        let manifest_text = r.file("manifest.toml")?;
        let manifest = parse_manifest(&manifest_text).map_err(|e| refuse("manifest.toml", e.to_string()))?;
        let server_side = [
            ("schemas", manifest.schemas.is_empty()),
            ("tables", manifest.tables.is_empty()),
            ("graph_actions", manifest.graph_actions.is_empty()),
        ];
        if let Some((field, _)) = server_side.iter().find(|(_, empty)| !empty) {
            return Err(refuse(
                "manifest.toml",
                format!("`module.{field}`: mods never run on the server"),
            ));
        }
        if manifest.contract != manifest.key {
            return Err(refuse(
                "manifest.toml",
                "`module.contract`: a mod implements no contract but its own key",
            ));
        }
        if manifest.key.starts_with("client.") || manifest.key == "client" {
            return Err(refuse("manifest.toml", "`client.*` keys are the client's own"));
        }
        let section = parse_mod_section(&r.file(MOD_TOML)?)?;
        let key = manifest.key;
        let prefix = prefix_of(&key);
        let mut screens = Vec::with_capacity(section.screens.len());
        for name in &section.screens {
            let file = format!("client/{name}.layout");
            let layout = r.file(&file)?;
            check_layout(&file, &key, &prefix, section.tier, &layout)?;
            screens.push(ModScreen {
                key: format!("{key}.{name}"),
                layout,
            });
        }
        let theme = match &section.theme {
            None => None,
            Some(name) => {
                let file = format!("client/{name}.theme");
                let text = r.file(&file)?;
                check_theme(&file, &prefix, &text)?;
                Some(text)
            }
        };
        let mut scripts = Vec::with_capacity(section.scripts.len());
        for name in &section.scripts {
            let source = r.file(&format!("scripts/{name}.luau"))?;
            scripts.push((name.clone(), source));
        }
        Ok(Self {
            key,
            version: manifest.version,
            dependencies: manifest.dependencies,
            tier: section.tier,
            demote: section.demote,
            screens,
            theme,
            scripts,
            hash: ContentHash::of(&r.hashed),
            origin: origin.to_owned(),
        })
    }

    /// Loads the mod in folder `dir`.
    ///
    /// # Errors
    /// See [`ModPackage::from_files`].
    pub fn load(dir: &Path) -> Result<Self, ModError> {
        let origin = dir
            .file_name()
            .map_or_else(|| dir.display().to_string(), |n| n.to_string_lossy().into_owned());
        Self::from_files(&origin, &mut |rel| {
            let path = rel.split('/').fold(dir.to_path_buf(), |p, part| p.join(part));
            let meta = std::fs::metadata(&path).map_err(|e| e.to_string())?;
            if meta.len() > MAX_FILE as u64 {
                return Err(format!("larger than {MAX_FILE} bytes"));
            }
            std::fs::read_to_string(&path).map_err(|e| e.to_string())
        })
    }
}

/// The element id prefix of `key`.
pub(crate) fn prefix_of(key: &str) -> String {
    format!("{}_", key.replace('.', "_"))
}

/// Loads every mod folder in `dir` (in name order; at most [`MAX_FOLDERS`]). A folder
/// that does not load becomes a notice naming it; a missing `dir` holds no mods.
pub fn scan(dir: &Path) -> (Vec<ModPackage>, Vec<ModNotice>) {
    let mut folders: Vec<std::path::PathBuf> = match std::fs::read_dir(dir) {
        Ok(entries) => entries
            .filter_map(Result::ok)
            .map(|e| e.path())
            .filter(|p| p.is_dir())
            .collect(),
        Err(_) => return (Vec::new(), Vec::new()),
    };
    folders.sort();
    let mut notices = Vec::new();
    if folders.len() > MAX_FOLDERS {
        notices.push(ModNotice {
            module: dir.display().to_string(),
            text: format!("only the first {MAX_FOLDERS} mod folders are read"),
        });
        folders.truncate(MAX_FOLDERS);
    }
    let mut mods = Vec::new();
    for folder in folders {
        match ModPackage::load(&folder) {
            Ok(m) => mods.push(m),
            Err(e) => notices.push(ModNotice {
                module: folder
                    .file_name()
                    .map_or_else(String::new, |n| n.to_string_lossy().into_owned()),
                text: format!("not loaded: {e}"),
            }),
        }
    }
    (mods, notices)
}

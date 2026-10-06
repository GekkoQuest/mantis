//! Module manifests (`manifest.toml`) and the module section of a package
//! manifest (`package.toml`).
//!
//! ```toml
//! [module]
//! key = "std.party"              # unique, dotted, lowercase
//! version = "0.1.0"
//! contract = "std.party"         # the contract it implements (default: key)
//! schemas = ["schema/party.idl"] # paths relative to the module folder
//! tables = ["std.party.limits"]  # content tables it reads
//! graph_actions = ["std.party.summon"] # gameplay graph actions it registers
//! optional = ["std.guild"]               # contracts used when the package has them
//!
//! [dependencies]                 # by contract, never by implementation
//! "std.chat" = "0.1"             # caret requirement
//!
//! [flags]                        # feature flags and their defaults
//! enabled = true                 # implicit and true when absent
//! cross_cell = false
//! ```

use core::fmt;
use std::collections::BTreeMap;

use super::toml::{self, Document, TomlError, Value};

/// A semantic version.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct Version {
    /// Major.
    pub major: u32,
    /// Minor.
    pub minor: u32,
    /// Patch.
    pub patch: u32,
}

impl Version {
    /// Parses `"1.2.3"`, `"1.2"`, or `"1"` (missing parts are zero).
    #[must_use]
    pub fn parse(s: &str) -> Option<Self> {
        let mut parts = s.split('.').map(|p| p.parse::<u32>().ok());
        let major = parts.next()??;
        let minor = parts.next().unwrap_or(Some(0))?;
        let patch = parts.next().unwrap_or(Some(0))?;
        if parts.next().is_some() {
            return None;
        }
        Some(Self { major, minor, patch })
    }

    /// True when `self` satisfies the caret requirement `req`: at least
    /// `req`, and below the next breaking version (the next major, or the
    /// next minor while the major is 0).
    #[must_use]
    pub fn satisfies(self, req: Self) -> bool {
        if self < req {
            return false;
        }
        if req.major == 0 {
            self.major == 0 && self.minor == req.minor
        } else {
            self.major == req.major
        }
    }
}

impl fmt::Display for Version {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}.{}.{}", self.major, self.minor, self.patch)
    }
}

/// A dependency on another module's contract.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Dependency {
    /// The contract key.
    pub contract: String,
    /// The caret requirement.
    pub version: Version,
}

/// A parsed module manifest.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Manifest {
    /// Unique module key, for example `"std.party"`.
    pub key: String,
    /// Its version.
    pub version: Version,
    /// The contract it implements (its own key unless it overrides another).
    pub contract: String,
    /// Schema files, relative to the module folder.
    pub schemas: Vec<String>,
    /// Content tables it reads.
    pub tables: Vec<String>,
    /// Gameplay graph action kinds it registers (dotted keys). The cook
    /// refuses a graph naming an action no resolved module declares; the
    /// server refuses a module registering an action it did not declare.
    pub graph_actions: Vec<String>,
    /// Dependencies by contract, sorted by contract key.
    pub dependencies: Vec<Dependency>,
    /// Contracts it uses when the package has them, sorted: a missing or
    /// disabled provider does not stop it (it resolves to a fact the module
    /// reads at registration, [`crate::module::OptionalProvider`]).
    pub optional: Vec<String>,
    /// Feature flags and their defaults; always contains `enabled`.
    pub flags: BTreeMap<String, bool>,
}

/// A manifest was refused.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum ManifestError {
    /// The text is not in the supported TOML subset.
    Syntax(TomlError),
    /// A required field is absent.
    Missing(&'static str),
    /// A field has the wrong type or form.
    Invalid {
        /// The field.
        field: String,
        /// Why.
        why: &'static str,
    },
    /// A table or key the format does not define.
    Unknown(String),
}

impl fmt::Display for ManifestError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Syntax(e) => write!(f, "{e}"),
            Self::Missing(field) => write!(f, "missing `{field}`"),
            Self::Invalid { field, why } => write!(f, "`{field}`: {why}"),
            Self::Unknown(what) => write!(f, "unknown `{what}`"),
        }
    }
}

impl std::error::Error for ManifestError {}

impl From<TomlError> for ManifestError {
    fn from(e: TomlError) -> Self {
        Self::Syntax(e)
    }
}

/// True for a dotted lowercase key like `std.party` or `std.party.limits`.
#[must_use]
pub fn is_valid_key(key: &str) -> bool {
    !key.is_empty()
        && key.split('.').all(|seg| {
            seg.starts_with(|c: char| c.is_ascii_lowercase())
                && seg
                    .chars()
                    .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
        })
}

fn invalid(field: &str, why: &'static str) -> ManifestError {
    ManifestError::Invalid {
        field: field.to_owned(),
        why,
    }
}

fn string(doc: &Document, table: &str, key: &'static str) -> Result<Option<String>, ManifestError> {
    match doc.table(table).and_then(|t| t.get(key)) {
        None => Ok(None),
        Some(v) => v
            .as_str()
            .map(|s| Some(s.to_owned()))
            .ok_or_else(|| invalid(key, "expected a string")),
    }
}

fn strings(doc: &Document, table: &str, key: &'static str) -> Result<Vec<String>, ManifestError> {
    match doc.table(table).and_then(|t| t.get(key)) {
        None => Ok(Vec::new()),
        Some(v) => v
            .as_strings()
            .ok_or_else(|| invalid(key, "expected an array of strings")),
    }
}

fn only(doc: &Document, tables: &[&str], keys: &[(&str, &[&str])]) -> Result<(), ManifestError> {
    for t in &doc.tables {
        if !tables.contains(&t.name.as_str()) {
            return Err(ManifestError::Unknown(format!("[{}]", t.name)));
        }
        if let Some((_, allowed)) = keys.iter().find(|(name, _)| *name == t.name) {
            for e in &t.entries {
                if !allowed.contains(&e.key.as_str()) {
                    return Err(ManifestError::Unknown(format!("{}.{}", t.name, e.key)));
                }
            }
        }
    }
    Ok(())
}

/// Parses a module manifest.
///
/// # Errors
/// [`ManifestError`]: syntax, a missing or malformed field, or an unknown
/// table or key (fail closed: a typo is never silently ignored).
pub fn parse_manifest(text: &str) -> Result<Manifest, ManifestError> {
    let doc = toml::parse(text)?;
    only(
        &doc,
        &["", "module", "dependencies", "flags"],
        &[
            ("", &[]),
            (
                "module",
                &[
                    "key",
                    "version",
                    "contract",
                    "schemas",
                    "tables",
                    "graph_actions",
                    "optional",
                ],
            ),
        ],
    )?;
    let key = string(&doc, "module", "key")?.ok_or(ManifestError::Missing("module.key"))?;
    if !is_valid_key(&key) {
        return Err(invalid("module.key", "expected a dotted lowercase key"));
    }
    let version = string(&doc, "module", "version")?.ok_or(ManifestError::Missing("module.version"))?;
    let version = Version::parse(&version).ok_or_else(|| invalid("module.version", "expected a version"))?;
    let contract = string(&doc, "module", "contract")?.unwrap_or_else(|| key.clone());
    if !is_valid_key(&contract) {
        return Err(invalid("module.contract", "expected a dotted lowercase key"));
    }
    let mut dependencies = Vec::new();
    for e in doc.table("dependencies").map_or(&[][..], |t| &t.entries[..]) {
        if !is_valid_key(&e.key) {
            return Err(invalid(&e.key, "expected a contract key"));
        }
        if e.key == contract {
            return Err(invalid(&e.key, "a module cannot depend on its own contract"));
        }
        let version = e
            .value
            .as_str()
            .and_then(Version::parse)
            .ok_or_else(|| invalid(&e.key, "expected a version requirement string"))?;
        dependencies.push(Dependency {
            contract: e.key.clone(),
            version,
        });
    }
    dependencies.sort_by(|a, b| a.contract.cmp(&b.contract));
    let mut flags = BTreeMap::new();
    flags.insert("enabled".to_owned(), true);
    for e in doc.table("flags").map_or(&[][..], |t| &t.entries[..]) {
        if e.key.contains('.') || !is_valid_key(&e.key) {
            return Err(invalid(&e.key, "expected a lowercase flag name without dots"));
        }
        let v = e
            .value
            .as_bool()
            .ok_or_else(|| invalid(&e.key, "expected true or false"))?;
        flags.insert(e.key.clone(), v);
    }
    let graph_actions = strings(&doc, "module", "graph_actions")?;
    for (i, a) in graph_actions.iter().enumerate() {
        if !is_valid_key(a) {
            return Err(invalid("module.graph_actions", "expected dotted lowercase keys"));
        }
        if graph_actions.iter().take(i).any(|b| b == a) {
            return Err(invalid("module.graph_actions", "an action is declared twice"));
        }
    }
    let mut optional = strings(&doc, "module", "optional")?;
    optional.sort();
    for (i, o) in optional.iter().enumerate() {
        if !is_valid_key(o) {
            return Err(invalid("module.optional", "expected contract keys"));
        }
        if *o == contract {
            return Err(invalid("module.optional", "a module cannot use its own contract"));
        }
        if optional.get(i + 1) == Some(o) {
            return Err(invalid("module.optional", "a contract is named twice"));
        }
        if dependencies.iter().any(|d| d.contract == *o) {
            return Err(invalid(
                "module.optional",
                "a contract is both a dependency and optional",
            ));
        }
    }
    Ok(Manifest {
        optional,
        key,
        version,
        contract,
        schemas: strings(&doc, "module", "schemas")?,
        tables: strings(&doc, "module", "tables")?,
        graph_actions,
        dependencies,
        flags,
    })
}

/// The module section of a package manifest.
///
/// ```toml
/// [package]
/// name = "toy"
/// uses = ["std"]                # packages whose modules this one includes
/// overrides = ["std.party"]     # modules it replaces with its own
/// client_mods = ["toy.hud"]     # client module keys sessions may run
///
/// [flags]                       # flag defaults for this package
/// "std.mail.enabled" = false
/// ```
///
/// Other tables of `package.toml` (tunables, for example) belong to other
/// readers and are ignored here.
#[derive(Clone, PartialEq, Eq, Debug, Default)]
pub struct PackageModules {
    /// The package name.
    pub name: String,
    /// Other packages whose modules are included.
    pub uses: Vec<String>,
    /// Module keys replaced by this package's own modules.
    pub overrides: Vec<String>,
    /// Client module keys sessions may run (at most 32, each at most 32
    /// bytes); a handshake naming another is refused.
    pub client_mods: Vec<String>,
    /// Flag defaults, by full flag name (`<module key>.<flag>`).
    pub flags: BTreeMap<String, bool>,
}

/// Client modules a package may permit (the wire's list bound).
pub const MAX_CLIENT_MODS: usize = 32;
/// Longest client module key (the wire's string bound).
pub const MAX_CLIENT_MOD_KEY: usize = 32;

/// Parses the module section of a package manifest.
///
/// # Errors
/// [`ManifestError`].
pub fn parse_package(text: &str) -> Result<PackageModules, ManifestError> {
    let doc = toml::parse(text)?;
    let name = string(&doc, "package", "name")?.ok_or(ManifestError::Missing("package.name"))?;
    let mut flags = BTreeMap::new();
    for e in doc.table("flags").map_or(&[][..], |t| &t.entries[..]) {
        let v = match &e.value {
            Value::Bool(b) => *b,
            _ => return Err(invalid(&e.key, "expected true or false")),
        };
        flags.insert(e.key.clone(), v);
    }
    let client_mods = strings(&doc, "package", "client_mods")?;
    if client_mods.len() > MAX_CLIENT_MODS {
        return Err(invalid("package.client_mods", "at most 32 client modules"));
    }
    for (i, m) in client_mods.iter().enumerate() {
        if !is_valid_key(m) || m.len() > MAX_CLIENT_MOD_KEY {
            return Err(invalid(
                "package.client_mods",
                "expected dotted lowercase keys of at most 32 bytes",
            ));
        }
        if client_mods.iter().take(i).any(|b| b == m) {
            return Err(invalid("package.client_mods", "a module is listed twice"));
        }
    }
    Ok(PackageModules {
        name,
        uses: strings(&doc, "package", "uses")?,
        overrides: strings(&doc, "package", "overrides")?,
        client_mods,
        flags,
    })
}

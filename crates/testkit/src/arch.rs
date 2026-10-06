//! Architecture rules: dependencies point down only (plan section 5).
//!
//! The rule table below is the single source of truth for which workspace
//! crate may depend on which. It is data, approved by the lead, and checked
//! against `cargo metadata` by `tests/architecture.rs`. Fail closed:
//!
//! - every workspace member must be classified, either listed in [`CRATE_RULES`]
//!   or located under a `packages/*/adapters/` directory;
//! - every normal, build, or dev dependency on another workspace member must
//!   be allowed by the depending crate's row;
//! - a dev-dependency on `mantis-testkit` is allowed everywhere, and a normal or
//!   build dependency on it is allowed nowhere (decision 0015);
//! - a crate under `packages/*/adapters/` may depend only on
//!   `mantis-adapter-contract` (plan section 9, decision 0004);
//! - the rule table itself must be acyclic.

use std::collections::BTreeMap;
use std::fmt;

/// The test-tooling crate, allowed as a dev-dependency everywhere.
pub const TESTKIT: &str = "mantis-testkit";

/// The only crate a wire adapter may depend on.
pub const ADAPTER_CONTRACT: &str = "mantis-adapter-contract";

/// What a crate may depend on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Allowed {
    /// Exactly these workspace crates (plus a dev-dependency on testkit).
    Only(&'static [&'static str]),
    /// Any workspace crate. Reserved for test tooling.
    Any,
}

/// The approved dependency arrows between engine crates, by package name.
pub const CRATE_RULES: &[(&str, Allowed)] = &[
    ("mantis-core", Allowed::Only(&[])),
    ("mantis-idl", Allowed::Only(&[])),
    ("mantis-modsync", Allowed::Only(&["mantis-core"])),
    ("mantis-formats", Allowed::Only(&["mantis-core"])),
    (
        "mantis-shadergen",
        Allowed::Only(&["mantis-core", "mantis-formats"]),
    ),
    ("mantis-adapter-contract", Allowed::Only(&["mantis-core"])),
    (
        "mantis-net",
        Allowed::Only(&["mantis-core", "mantis-adapter-contract"]),
    ),
    ("mantis-script", Allowed::Only(&["mantis-core"])),
    (
        "mantis-server",
        Allowed::Only(&[
            "mantis-core",
            "mantis-idl",
            "mantis-net",
            "mantis-adapter-contract",
            "mantis-script",
            "mantis-formats",
        ]),
    ),
    (
        "mantis-services",
        Allowed::Only(&[
            "mantis-core",
            "mantis-net",
            "mantis-adapter-contract",
            "mantis-idl",
        ]),
    ),
    (
        // Process-per-role deployment (deploy-engine): runs each service
        // role, and hosts a package's cells through the package binary.
        "mantis-deploy",
        Allowed::Only(&[
            "mantis-core",
            "mantis-services",
            "mantis-net",
            "mantis-adapter-contract",
            "mantis-idl",
            "mantis-server",
        ]),
    ),
    (
        "mantis-cook",
        // anim for the vertex-animation baker, ui so the cook validates
        // layouts with the runtime parser; no render edge (bakers are CPU-side).
        Allowed::Only(&[
            "mantis-core",
            "mantis-idl",
            "mantis-formats",
            "mantis-shadergen",
            "mantis-anim",
            "mantis-ui",
        ]),
    ),
    (
        "mantis-client",
        Allowed::Only(&[
            "mantis-core",
            "mantis-net",
            "mantis-adapter-contract",
            "mantis-script",
            "mantis-render",
            "mantis-anim",
            "mantis-ui",
            "mantis-audio",
            "mantis-formats",
        ]),
    ),
    (
        "mantis-render",
        Allowed::Only(&["mantis-core", "mantis-formats", "mantis-ui", "mantis-shadergen"]),
    ),
    ("mantis-anim", Allowed::Only(&["mantis-core", "mantis-formats"])),
    ("mantis-ui", Allowed::Only(&["mantis-core"])),
    ("mantis-audio", Allowed::Only(&["mantis-core", "mantis-formats"])),
    (
        "mantis-editor",
        Allowed::Only(&[
            "mantis-formats",
            "mantis-client",
            "mantis-render",
            "mantis-anim",
            "mantis-ui",
            "mantis-audio",
            "mantis-cook",
            "mantis-core",
            "mantis-script",
            "mantis-net",
            "mantis-shadergen",
        ]),
    ),
    (TESTKIT, Allowed::Any),
];

/// Simulation crates (decision 0013), as paths relative to the workspace
/// root. Each must carry a `clippy.toml` byte-identical to the canonical one
/// in `crates/core`, which bans `std` float transcendentals, wall clocks, and
/// randomly seeded hash maps.
pub const SIM_CRATES: &[&str] = &["crates/core", "crates/server", "crates/client"];

/// File-level lint expectations permitted in integration-test trees
/// (`tests/**` of any crate or package), as `#![expect(...)]`. Everything
/// else, and any file-level expectation under `src/`, fails the
/// architecture test (lead ruling).
pub const TEST_FILE_EXPECTS: &[&str] = &[
    "clippy::unwrap_used",
    "clippy::expect_used",
    "clippy::panic",
    "clippy::cast_possible_truncation",
    "clippy::cast_possible_wrap",
    "clippy::cast_sign_loss",
    "clippy::cast_precision_loss",
    "clippy::cast_lossless",
    "clippy::too_many_lines",
    "clippy::indexing_slicing",
    "dead_code",
];

/// Item-level `#[allow(clippy::...)]` sites that cannot be expectations:
/// `(file, lint)`. The wire codec's `FuzzSample` impl is written by a macro
/// for every integer type, and whether a cast lint fires depends on the
/// type, so some expansions would leave an `expect` unfulfilled.
pub const ITEM_ALLOWS: &[(&str, &str)] = &[
    ("crates/core/src/wire/mod.rs", "clippy::cast_possible_truncation"),
    ("crates/core/src/wire/mod.rs", "clippy::cast_possible_wrap"),
    ("crates/core/src/wire/mod.rs", "clippy::cast_lossless"),
];

/// A lint attribute's form.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LintForm {
    /// `allow`: silences the lint, needed or not.
    Allow,
    /// `expect`: silences it, and fails when the lint no longer fires.
    Expect,
}

/// One lint attribute: file-level (`#![...]`) or on an item (`#[...]`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LintAttr {
    /// 1-based line of the attribute.
    pub line: usize,
    /// `allow` or `expect`.
    pub form: LintForm,
    /// True for `#![...]`.
    pub file_level: bool,
    /// The lints named, whitespace removed.
    pub lints: Vec<String>,
}

/// Finds `allow` and `expect` attributes in Rust source text, file-level
/// and item-level, including ones spanning several lines. Line comments
/// inside the attribute are ignored; lines that are comments (or string
/// text not starting a line) are never attributes.
#[must_use]
pub fn lint_attrs(source: &str) -> Vec<LintAttr> {
    let mut out = Vec::new();
    let mut rows = source.lines().enumerate();
    while let Some((i, line)) = rows.next() {
        let t = line.trim_start();
        let (file_level, rest) = match t.strip_prefix("#!") {
            Some(r) => (true, r),
            None => (false, t.strip_prefix('#').unwrap_or("")),
        };
        let (form, rest) = if let Some(r) = rest.strip_prefix("[allow(") {
            (LintForm::Allow, r)
        } else if let Some(r) = rest.strip_prefix("[expect(") {
            (LintForm::Expect, r)
        } else {
            continue;
        };
        let mut body = String::new();
        let mut chunk = rest;
        loop {
            let code = chunk.split("//").next().unwrap_or("");
            if let Some(end) = code.find(")]") {
                body.push_str(code.get(..end).unwrap_or(""));
                break;
            }
            body.push_str(code);
            body.push(',');
            match rows.next() {
                Some((_, next)) => chunk = next,
                None => break,
            }
        }
        let lints = body
            .split(',')
            .map(|l| l.split_whitespace().collect::<String>())
            // `reason = "..."` is not a lint.
            .filter(|l| !l.is_empty() && !l.starts_with("reason="))
            .collect();
        out.push(LintAttr {
            line: i + 1,
            form,
            file_level,
            lints,
        });
    }
    out
}

/// Where a source file sits, for the lint-attribute rule.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TreeKind {
    /// Library or binary source (`src/**`, `build.rs`, and anything else).
    Src,
    /// Integration tests (`tests/**`).
    Tests,
}

/// Classifies a path by its directory components: any `src` component makes
/// it [`TreeKind::Src`]; otherwise any `tests` component makes it
/// [`TreeKind::Tests`]; anything else is [`TreeKind::Src`] (strict).
#[must_use]
pub fn classify_tree(path: &str) -> TreeKind {
    let normalized = path.replace('\\', "/");
    let mut dirs: Vec<&str> = normalized.split('/').collect();
    dirs.pop(); // the file name itself
    if dirs.contains(&"src") {
        TreeKind::Src
    } else if dirs.contains(&"tests") {
        TreeKind::Tests
    } else {
        TreeKind::Src
    }
}

/// Violations of the lint-attribute rule in one file's source:
///
/// - lints are silenced with `expect`, so one that no longer fires fails
///   the build; a file-level `#![expect]` only in `tests/**`, naming only
///   [`TEST_FILE_EXPECTS`];
/// - no `#[allow(clippy::...)]` on an item, except [`ITEM_ALLOWS`];
/// - no file-level `#![allow]`, except `dead_code` in a test module shared
///   by several test targets (`tests/**/mod.rs`), where each target uses a
///   different subset and an `expect` would go unfulfilled in some;
/// - generated code (`/generated/`) is the code generator's to decide.
#[must_use]
pub fn check_lint_attrs(path: &str, source: &str) -> Vec<String> {
    let normalized = path.replace('\\', "/");
    if normalized.contains("/generated/") {
        return Vec::new();
    }
    let kind = classify_tree(&normalized);
    let shared_test_module = kind == TreeKind::Tests && normalized.ends_with("/mod.rs");
    let mut out = Vec::new();
    for attr in lint_attrs(source) {
        for lint in &attr.lints {
            let why = match (attr.file_level, attr.form) {
                (true, LintForm::Expect) => match kind {
                    TreeKind::Src => Some("file-level expect under src"),
                    TreeKind::Tests if TEST_FILE_EXPECTS.contains(&lint.as_str()) => None,
                    TreeKind::Tests => Some("lint not permitted in a test file"),
                },
                (true, LintForm::Allow) if shared_test_module && lint == "dead_code" => None,
                (true, LintForm::Allow) => Some("file-level allow (use expect)"),
                (false, LintForm::Allow) if lint.starts_with("clippy::") => {
                    let excepted = ITEM_ALLOWS.iter().any(|(f, l)| *f == normalized && l == lint);
                    (!excepted).then_some("item-level allow (use expect)")
                }
                (false, _) => None,
            };
            if let Some(why) = why {
                out.push(format!("{normalized}:{}: {why}: {lint}", attr.line));
            }
        }
    }
    out
}

/// Kind of a dependency edge, as cargo reports it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum DepKind {
    /// `[dependencies]`.
    Normal,
    /// `[dev-dependencies]`.
    Dev,
    /// `[build-dependencies]`.
    Build,
}

impl fmt::Display for DepKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Normal => "normal",
            Self::Dev => "dev",
            Self::Build => "build",
        })
    }
}

/// One dependency of a workspace member.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Dependency {
    /// The depended-on package name (not the rename, if any).
    pub name: String,
    /// Edge kind.
    pub kind: DepKind,
}

/// One workspace member.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Member {
    /// Package name.
    pub name: String,
    /// Manifest path with separators normalised to `/`.
    pub manifest_path: String,
    /// All declared dependencies, internal and external.
    pub dependencies: Vec<Dependency>,
}

impl Member {
    /// True when the crate lives under a `packages/<name>/adapters/` directory.
    #[must_use]
    pub fn is_package_adapter(&self) -> bool {
        matches!(self.package_role(), Some((_, PackageRole::Adapter)))
    }

    /// The package this crate belongs to and its role, from its path:
    /// `packages/<package>/adapters/<name>/`, `packages/<package>/server/`,
    /// `packages/<package>/client/`, `packages/<package>/e2e/`, or
    /// `packages/<package>/modules/<feature>/{contract,server,client}/`.
    #[must_use]
    pub fn package_role(&self) -> Option<(String, PackageRole)> {
        let parts: Vec<&str> = self.manifest_path.split('/').collect();
        let at = parts.iter().rposition(|p| *p == "packages")?;
        let rest = parts.get(at + 1..)?;
        let role = match rest {
            [_, "adapters", ..] => PackageRole::Adapter,
            [_, "modules", _, "contract", ..] => PackageRole::ModuleContract,
            [_, "modules", _, "server", ..] => PackageRole::ModuleServer,
            [_, "modules", _, "client", ..] => PackageRole::ModuleClient,
            [_, "server", ..] => PackageRole::Server,
            [_, "client", ..] => PackageRole::Client,
            [_, "e2e", ..] => PackageRole::E2e,
            _ => return None,
        };
        rest.first().map(|pkg| ((*pkg).to_owned(), role))
    }

    /// For a module crate, its `(package, feature)`.
    #[must_use]
    pub fn module(&self) -> Option<(String, String)> {
        let parts: Vec<&str> = self.manifest_path.split('/').collect();
        let at = parts.iter().rposition(|p| *p == "packages")?;
        match parts.get(at + 1..)? {
            [pkg, "modules", feature, ..] => Some(((*pkg).to_owned(), (*feature).to_owned())),
            _ => None,
        }
    }
}

/// The role of a crate inside a game package.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PackageRole {
    /// `packages/<p>/adapters/<name>`: may depend only on the adapter contract.
    Adapter,
    /// `packages/<p>/server`: [`PACKAGE_SERVER_DEPS`] plus its own package's adapters.
    Server,
    /// `packages/<p>/client`: [`PACKAGE_CLIENT_DEPS`] plus its own package's adapters.
    Client,
    /// `packages/<p>/e2e`: [`PACKAGE_E2E_DEPS`] plus its own package's server,
    /// client, and adapters. Nothing may depend on an e2e crate.
    E2e,
    /// `packages/<p>/modules/<f>/contract`: a module's public events and
    /// queries. [`MODULE_CONTRACT_DEPS`] plus other modules' contract crates.
    ModuleContract,
    /// `packages/<p>/modules/<f>/server`: [`MODULE_SERVER_DEPS`] plus any
    /// module contract crate. Only package server crates may depend on it.
    ModuleServer,
    /// `packages/<p>/modules/<f>/client`: [`MODULE_CLIENT_DEPS`] plus any
    /// module contract crate. Only package client crates may depend on it.
    ModuleClient,
}

/// Engine crates a module contract crate may depend on.
pub const MODULE_CONTRACT_DEPS: &[&str] = &["mantis-core"];

/// Engine crates a module's server crate may depend on.
pub const MODULE_SERVER_DEPS: &[&str] = &["mantis-core", "mantis-server", "mantis-script"];

/// Engine crates a module's client crate may depend on.
pub const MODULE_CLIENT_DEPS: &[&str] = &[
    "mantis-core",
    "mantis-client",
    "mantis-ui",
    "mantis-render",
    "mantis-anim",
    "mantis-audio",
];

/// Engine crates a package's server crate may depend on.
pub const PACKAGE_SERVER_DEPS: &[&str] = &[
    "mantis-core",
    "mantis-server",
    "mantis-net",
    "mantis-adapter-contract",
    "mantis-formats",
    "mantis-script",
    // The cell host's link to the service roles, and the one-process
    // cluster for local development.
    "mantis-services",
    // The cell-host role in a per-process deployment
    // (`toy-server node cell-host --config FILE`).
    "mantis-deploy",
];

/// Engine crates a package's client crate may depend on.
pub const PACKAGE_CLIENT_DEPS: &[&str] = &[
    "mantis-core",
    "mantis-client",
    "mantis-render",
    "mantis-anim",
    "mantis-ui",
    "mantis-audio",
    "mantis-net",
    "mantis-adapter-contract",
    "mantis-formats",
    "mantis-script",
    // The editor module, loaded behind a flag (lead ruling, M10).
    "mantis-editor",
];

/// Engine crates a package's end-to-end test crate may depend on: both
/// hosts, so one test can drive a real server and a real client, plus the
/// cook and the renderer (an e2e test cooks content into a temporary store
/// and streams it into a headless renderer).
pub const PACKAGE_E2E_DEPS: &[&str] = &[
    "mantis-core",
    "mantis-net",
    "mantis-adapter-contract",
    "mantis-server",
    "mantis-client",
    TESTKIT,
    "mantis-cook",
    "mantis-render",
];

/// The workspace as seen through `cargo metadata --no-deps`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Workspace {
    /// Members sorted by name.
    pub members: Vec<Member>,
}

/// A broken architecture rule.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub enum Violation {
    /// A workspace member has no row in the rule table and is not an adapter.
    Unclassified {
        /// The crate.
        krate: String,
    },
    /// A dependency arrow that the rule table does not allow.
    ForbiddenEdge {
        /// Depending crate.
        from: String,
        /// Depended-on crate.
        to: String,
        /// Edge kind.
        kind: DepKind,
    },
    /// A package adapter depends on something other than the adapter contract.
    AdapterEdge {
        /// The adapter crate.
        from: String,
        /// Depended-on crate.
        to: String,
        /// Edge kind.
        kind: DepKind,
    },
    /// The rule table contains a cycle through this crate.
    RuleCycle {
        /// A crate on the cycle.
        krate: String,
    },
}

impl fmt::Display for Violation {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Unclassified { krate } => {
                write!(f, "{krate}: workspace member has no architecture rule")
            }
            Self::ForbiddenEdge { from, to, kind } => {
                write!(f, "{from} -> {to} ({kind}): dependency arrow not allowed")
            }
            Self::AdapterEdge { from, to, kind } => write!(
                f,
                "{from} -> {to} ({kind}): adapters may depend only on {ADAPTER_CONTRACT}"
            ),
            Self::RuleCycle { krate } => write!(f, "{krate}: rule table contains a cycle"),
        }
    }
}

/// Failure to read the workspace metadata.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MetadataError {
    /// The text is not JSON.
    Json(String),
    /// A required field is missing or has the wrong type.
    Shape(&'static str),
}

impl fmt::Display for MetadataError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Json(e) => write!(f, "cargo metadata is not valid JSON: {e}"),
            Self::Shape(what) => write!(f, "cargo metadata has an unexpected shape: {what}"),
        }
    }
}

impl std::error::Error for MetadataError {}

fn str_field<'a>(v: &'a serde_json::Value, key: &str, what: &'static str) -> Result<&'a str, MetadataError> {
    v.get(key)
        .and_then(serde_json::Value::as_str)
        .ok_or(MetadataError::Shape(what))
}

/// Parses the output of `cargo metadata --format-version 1 --no-deps`.
///
/// # Errors
/// [`MetadataError`] when the text is not JSON or lacks the expected fields.
pub fn parse_metadata(json: &str) -> Result<Workspace, MetadataError> {
    let root: serde_json::Value =
        serde_json::from_str(json).map_err(|e| MetadataError::Json(e.to_string()))?;
    let member_ids: Vec<&str> = root
        .get("workspace_members")
        .and_then(serde_json::Value::as_array)
        .ok_or(MetadataError::Shape("workspace_members"))?
        .iter()
        .map(|v| v.as_str().ok_or(MetadataError::Shape("workspace_members[]")))
        .collect::<Result<_, _>>()?;
    let packages = root
        .get("packages")
        .and_then(serde_json::Value::as_array)
        .ok_or(MetadataError::Shape("packages"))?;

    let mut members = Vec::new();
    for pkg in packages {
        let id = str_field(pkg, "id", "packages[].id")?;
        if !member_ids.contains(&id) {
            continue;
        }
        let name = str_field(pkg, "name", "packages[].name")?.to_owned();
        let manifest_path = str_field(pkg, "manifest_path", "packages[].manifest_path")?.replace('\\', "/");
        let deps = pkg
            .get("dependencies")
            .and_then(serde_json::Value::as_array)
            .ok_or(MetadataError::Shape("packages[].dependencies"))?;
        let mut dependencies = Vec::with_capacity(deps.len());
        for dep in deps {
            let dep_name = str_field(dep, "name", "dependencies[].name")?.to_owned();
            let kind = match dep.get("kind").and_then(serde_json::Value::as_str) {
                None => DepKind::Normal,
                Some("dev") => DepKind::Dev,
                Some("build") => DepKind::Build,
                Some(_) => return Err(MetadataError::Shape("dependencies[].kind")),
            };
            dependencies.push(Dependency { name: dep_name, kind });
        }
        members.push(Member {
            name,
            manifest_path,
            dependencies,
        });
    }
    members.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(Workspace { members })
}

fn rule_for<'r>(rules: &'r [(&'static str, Allowed)], krate: &str) -> Option<&'r Allowed> {
    rules.iter().find(|(name, _)| *name == krate).map(|(_, a)| a)
}

/// Checks that `rules` is acyclic. Returns one violation per crate on a cycle.
#[must_use]
pub fn check_rules_acyclic(rules: &[(&'static str, Allowed)]) -> Vec<Violation> {
    // Kahn's algorithm over the `Only` edges; `Any` rows have no fixed edges.
    let mut indegree: BTreeMap<&str, usize> = rules.iter().map(|(n, _)| (*n, 0)).collect();
    let edges: Vec<(&str, &str)> = rules
        .iter()
        .flat_map(|(from, allowed)| match allowed {
            Allowed::Only(tos) => tos.iter().map(|to| (*from, *to)).collect::<Vec<_>>(),
            Allowed::Any => Vec::new(),
        })
        .collect();
    for (_, to) in &edges {
        *indegree.entry(to).or_insert(0) += 1;
    }
    let mut ready: Vec<&str> = indegree
        .iter()
        .filter(|(_, d)| **d == 0)
        .map(|(n, _)| *n)
        .collect();
    let mut removed = 0usize;
    while let Some(node) = ready.pop() {
        removed += 1;
        for (_, to) in edges.iter().filter(|(from, _)| *from == node) {
            if let Some(d) = indegree.get_mut(to) {
                *d -= 1;
                if *d == 0 {
                    ready.push(to);
                }
            }
        }
    }
    if removed == indegree.len() {
        return Vec::new();
    }
    indegree
        .into_iter()
        .filter(|(_, d)| *d > 0)
        .map(|(n, _)| Violation::RuleCycle { krate: n.to_owned() })
        .collect()
}

/// What package-placed crates may reach, by role.
struct PackageAllowances<'a> {
    ws: &'a Workspace,
    contracts: Vec<&'a str>,
    module_servers: Vec<&'a str>,
    module_clients: Vec<&'a str>,
}

impl<'a> PackageAllowances<'a> {
    fn new(ws: &'a Workspace) -> Self {
        Self {
            contracts: Self::with_role(ws, |_, r| r == PackageRole::ModuleContract),
            module_servers: Self::with_role(ws, |_, r| r == PackageRole::ModuleServer),
            module_clients: Self::with_role(ws, |_, r| r == PackageRole::ModuleClient),
            ws,
        }
    }

    fn with_role(ws: &'a Workspace, keep: impl Fn(&str, PackageRole) -> bool) -> Vec<&'a str> {
        ws.members
            .iter()
            .filter(|m| m.package_role().is_some_and(|(p, r)| keep(&p, r)))
            .map(|m| m.name.as_str())
            .collect()
    }

    /// The engine crates and the package crates `placement` may depend on;
    /// `None` for crates outside packages (they use the rule table).
    fn allowed(&self, placement: &(String, PackageRole)) -> Option<(&'static [&'static str], Vec<&'a str>)> {
        let (pkg, role) = placement;
        let own = |roles: &[PackageRole]| Self::with_role(self.ws, |p, r| p == pkg && roles.contains(&r));
        let plus = |mut v: Vec<&'a str>, extra: &[&'a str]| {
            v.extend_from_slice(extra);
            v
        };
        match role {
            PackageRole::Adapter => None,
            PackageRole::Server => Some((
                PACKAGE_SERVER_DEPS,
                plus(
                    plus(own(&[PackageRole::Adapter]), &self.module_servers),
                    &self.contracts,
                ),
            )),
            PackageRole::Client => Some((
                PACKAGE_CLIENT_DEPS,
                plus(
                    plus(own(&[PackageRole::Adapter]), &self.module_clients),
                    &self.contracts,
                ),
            )),
            // An e2e crate may use its own package's server and client too.
            PackageRole::E2e => Some((
                PACKAGE_E2E_DEPS,
                own(&[PackageRole::Adapter, PackageRole::Server, PackageRole::Client]),
            )),
            PackageRole::ModuleContract => Some((MODULE_CONTRACT_DEPS, self.contracts.clone())),
            PackageRole::ModuleServer => Some((MODULE_SERVER_DEPS, self.contracts.clone())),
            PackageRole::ModuleClient => Some((MODULE_CLIENT_DEPS, self.contracts.clone())),
        }
    }
}

/// Checks every member of `ws` against `rules`. The result is sorted and empty
/// when the workspace obeys the architecture.
#[must_use]
pub fn check(ws: &Workspace, rules: &[(&'static str, Allowed)]) -> Vec<Violation> {
    let internal: Vec<&str> = ws.members.iter().map(|m| m.name.as_str()).collect();
    let mut out = check_rules_acyclic(rules);
    let allowances = PackageAllowances::new(ws);
    let e2e_crates = PackageAllowances::with_role(ws, |_, r| r == PackageRole::E2e);
    for member in &ws.members {
        let placement = member.package_role();
        let adapter = matches!(placement, Some((_, PackageRole::Adapter)));
        let package_host = placement.as_ref().and_then(|p| allowances.allowed(p));
        let rule = rule_for(rules, &member.name);
        if rule.is_none() && placement.is_none() {
            out.push(Violation::Unclassified {
                krate: member.name.clone(),
            });
            continue;
        }
        for dep in &member.dependencies {
            if !internal.contains(&dep.name.as_str()) {
                continue; // external crates are outside the arrow rules
            }
            let to = dep.name.as_str();
            if e2e_crates.contains(&to) {
                out.push(Violation::ForbiddenEdge {
                    from: member.name.clone(),
                    to: to.to_owned(),
                    kind: dep.kind,
                });
                continue;
            }
            if dep.kind == DepKind::Dev && to == TESTKIT {
                continue;
            }
            if adapter {
                if to != ADAPTER_CONTRACT {
                    out.push(Violation::AdapterEdge {
                        from: member.name.clone(),
                        to: to.to_owned(),
                        kind: dep.kind,
                    });
                }
                continue;
            }
            let allowed = match (&package_host, rule) {
                (Some((engine, adapters)), _) => engine.contains(&to) || adapters.contains(&to),
                (None, Some(Allowed::Any)) => true,
                (None, Some(Allowed::Only(list))) => list.contains(&to),
                (None, None) => false,
            };
            if !allowed {
                out.push(Violation::ForbiddenEdge {
                    from: member.name.clone(),
                    to: to.to_owned(),
                    kind: dep.kind,
                });
            }
        }
    }
    out.sort();
    out.dedup();
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn member(name: &str, path: &str, deps: &[(&str, DepKind)]) -> Member {
        Member {
            name: name.to_owned(),
            manifest_path: path.to_owned(),
            dependencies: deps
                .iter()
                .map(|(n, k)| Dependency {
                    name: (*n).to_owned(),
                    kind: *k,
                })
                .collect(),
        }
    }

    #[test]
    fn finds_lint_attributes() {
        let src = "//! doc\n#![expect(clippy::x)]\nfn f() {}\n    #![allow(dead_code)]\n// #![allow(no)]\n#[allow(item)]\n#![expect(\n    clippy::a, // why\n    clippy::b,\n    reason = \"r\"\n)]\nlet s = \"#[allow(not)]\";\n";
        let got: Vec<(usize, LintForm, bool, Vec<String>)> = lint_attrs(src)
            .into_iter()
            .map(|a| (a.line, a.form, a.file_level, a.lints))
            .collect();
        assert_eq!(
            got,
            vec![
                (2, LintForm::Expect, true, vec!["clippy::x".to_owned()]),
                (4, LintForm::Allow, true, vec!["dead_code".to_owned()]),
                (6, LintForm::Allow, false, vec!["item".to_owned()]),
                (
                    7,
                    LintForm::Expect,
                    true,
                    vec!["clippy::a".to_owned(), "clippy::b".to_owned()]
                ),
            ]
        );
    }

    #[test]
    fn the_lint_rule_by_tree_and_form() {
        assert_eq!(classify_tree("crates/x/src/a/tests.rs"), TreeKind::Src);
        assert_eq!(
            classify_tree("crates\\x\\tests\\support\\mod.rs"),
            TreeKind::Tests
        );
        assert_eq!(classify_tree("packages/p/adapters/n/tests/t.rs"), TreeKind::Tests);
        assert_eq!(classify_tree("crates/x/build.rs"), TreeKind::Src);
        let ok = "#![expect(clippy::unwrap_used, dead_code)]\n";
        assert!(check_lint_attrs("crates/x/tests/t.rs", ok).is_empty());
        assert_eq!(check_lint_attrs("crates/x/src/lib.rs", ok).len(), 2);
        let bad = "#![expect(clippy::disallowed_methods)]\n";
        assert_eq!(
            check_lint_attrs("crates/x/tests/t.rs", bad),
            vec![
                "crates/x/tests/t.rs:1: lint not permitted in a test file: clippy::disallowed_methods"
                    .to_owned()
            ]
        );
        // File-level allow: only dead_code, only in a shared test module.
        let shared = "#![allow(dead_code)]\n";
        assert!(check_lint_attrs("crates/x/tests/common/mod.rs", shared).is_empty());
        assert_eq!(check_lint_attrs("crates/x/tests/t.rs", shared).len(), 1);
        let shared_clippy = "#![allow(clippy::unwrap_used)]\n";
        assert_eq!(
            check_lint_attrs("crates/x/tests/common/mod.rs", shared_clippy).len(),
            1
        );
        // Item-level: expect anywhere; an allowed clippy lint only where excepted.
        let item = "#[allow(clippy::cast_lossless)] // why\nfn f() {}\n";
        assert_eq!(check_lint_attrs("crates/x/src/lib.rs", item).len(), 1);
        assert!(check_lint_attrs("crates/core/src/wire/mod.rs", item).is_empty());
        assert!(check_lint_attrs("crates/x/src/lib.rs", "#[expect(clippy::cast_lossless)]\n").is_empty());
        assert!(check_lint_attrs("crates/x/src/lib.rs", "#[allow(non_snake_case)]\n").is_empty());
        assert!(check_lint_attrs("crates/x/src/generated/m.rs", item).is_empty());
    }

    #[test]
    fn approved_table_is_acyclic() {
        assert_eq!(check_rules_acyclic(CRATE_RULES), Vec::new());
    }

    #[test]
    fn cyclic_table_is_rejected() {
        let rules: &[(&str, Allowed)] = &[("a", Allowed::Only(&["b"])), ("b", Allowed::Only(&["a"]))];
        let v = check_rules_acyclic(rules);
        assert_eq!(
            v,
            vec![
                Violation::RuleCycle { krate: "a".into() },
                Violation::RuleCycle { krate: "b".into() }
            ]
        );
    }

    #[test]
    fn upward_sideways_and_unclassified_edges_are_reported() {
        let ws = Workspace {
            members: vec![
                member(
                    "mantis-core",
                    "/w/crates/core/Cargo.toml",
                    &[("mantis-server", DepKind::Normal)],
                ),
                member(
                    "mantis-server",
                    "/w/crates/server/Cargo.toml",
                    &[("mantis-client", DepKind::Dev)],
                ),
                member("mantis-client", "/w/crates/client/Cargo.toml", &[]),
                member("mystery", "/w/crates/mystery/Cargo.toml", &[]),
            ],
        };
        let v = check(&ws, CRATE_RULES);
        assert_eq!(
            v,
            vec![
                Violation::Unclassified {
                    krate: "mystery".into()
                },
                Violation::ForbiddenEdge {
                    from: "mantis-core".into(),
                    to: "mantis-server".into(),
                    kind: DepKind::Normal
                },
                Violation::ForbiddenEdge {
                    from: "mantis-server".into(),
                    to: "mantis-client".into(),
                    kind: DepKind::Dev
                },
            ]
        );
    }

    #[test]
    fn testkit_is_dev_only() {
        let ws = Workspace {
            members: vec![
                member(
                    "mantis-core",
                    "/w/crates/core/Cargo.toml",
                    &[(TESTKIT, DepKind::Dev)],
                ),
                member(
                    "mantis-server",
                    "/w/crates/server/Cargo.toml",
                    &[(TESTKIT, DepKind::Normal)],
                ),
                member(
                    "mantis-net",
                    "/w/crates/net/Cargo.toml",
                    &[(TESTKIT, DepKind::Build)],
                ),
                member(
                    TESTKIT,
                    "/w/crates/testkit/Cargo.toml",
                    &[("mantis-core", DepKind::Normal)],
                ),
            ],
        };
        let v = check(&ws, CRATE_RULES);
        assert_eq!(
            v,
            vec![
                Violation::ForbiddenEdge {
                    from: "mantis-net".into(),
                    to: TESTKIT.into(),
                    kind: DepKind::Build
                },
                Violation::ForbiddenEdge {
                    from: "mantis-server".into(),
                    to: TESTKIT.into(),
                    kind: DepKind::Normal
                },
            ]
        );
    }

    #[test]
    fn adapters_reach_only_the_contract() {
        let ws = Workspace {
            members: vec![
                member(
                    "pkg-adapter-native",
                    "C:/w/packages/toy/adapters/native/Cargo.toml",
                    &[
                        (ADAPTER_CONTRACT, DepKind::Normal),
                        ("mantis-server", DepKind::Normal),
                        (TESTKIT, DepKind::Dev),
                        ("serde", DepKind::Normal),
                    ],
                ),
                member(ADAPTER_CONTRACT, "C:/w/crates/adapter-contract/Cargo.toml", &[]),
                member("mantis-server", "C:/w/crates/server/Cargo.toml", &[]),
            ],
        };
        assert!(ws.members.first().is_some_and(Member::is_package_adapter));
        let v = check(&ws, CRATE_RULES);
        assert_eq!(
            v,
            vec![Violation::AdapterEdge {
                from: "pkg-adapter-native".into(),
                to: "mantis-server".into(),
                kind: DepKind::Normal
            }]
        );
    }

    #[test]
    fn package_hosts_reach_engine_and_own_adapters_only() {
        let ws = Workspace {
            members: vec![
                member(
                    "a-native",
                    "/w/packages/a/adapters/native/Cargo.toml",
                    &[(ADAPTER_CONTRACT, DepKind::Normal)],
                ),
                member(
                    "b-native",
                    "/w/packages/b/adapters/native/Cargo.toml",
                    &[(ADAPTER_CONTRACT, DepKind::Normal)],
                ),
                member(
                    "a-server",
                    "/w/packages/a/server/Cargo.toml",
                    &[
                        ("mantis-server", DepKind::Normal),
                        ("a-native", DepKind::Normal),
                        ("b-native", DepKind::Normal),
                        ("mantis-client", DepKind::Normal),
                    ],
                ),
                member(
                    "a-client",
                    "/w/packages/a/client/Cargo.toml",
                    &[
                        ("mantis-client", DepKind::Normal),
                        ("a-native", DepKind::Normal),
                        ("mantis-server", DepKind::Normal),
                    ],
                ),
                member(ADAPTER_CONTRACT, "/w/crates/adapter-contract/Cargo.toml", &[]),
                member("mantis-server", "/w/crates/server/Cargo.toml", &[]),
                member("mantis-client", "/w/crates/client/Cargo.toml", &[]),
            ],
        };
        let v = check(&ws, CRATE_RULES);
        assert_eq!(
            v,
            vec![
                Violation::ForbiddenEdge {
                    from: "a-client".into(),
                    to: "mantis-server".into(),
                    kind: DepKind::Normal
                },
                Violation::ForbiddenEdge {
                    from: "a-server".into(),
                    to: "b-native".into(),
                    kind: DepKind::Normal
                },
                Violation::ForbiddenEdge {
                    from: "a-server".into(),
                    to: "mantis-client".into(),
                    kind: DepKind::Normal
                },
            ]
        );
    }

    #[test]
    fn e2e_crates_reach_both_hosts_and_their_own_package_and_nothing_reaches_them() {
        let ws = Workspace {
            members: vec![
                member(
                    "a-native",
                    "/w/packages/a/adapters/native/Cargo.toml",
                    &[(ADAPTER_CONTRACT, DepKind::Normal)],
                ),
                member(
                    "a-server",
                    "/w/packages/a/server/Cargo.toml",
                    &[("a-e2e", DepKind::Dev)],
                ),
                member("a-client", "/w/packages/a/client/Cargo.toml", &[]),
                member("b-server", "/w/packages/b/server/Cargo.toml", &[]),
                member(
                    "a-e2e",
                    "/w/packages/a/e2e/Cargo.toml",
                    &[
                        ("mantis-core", DepKind::Dev),
                        ("mantis-net", DepKind::Dev),
                        (ADAPTER_CONTRACT, DepKind::Dev),
                        ("mantis-server", DepKind::Dev),
                        ("mantis-client", DepKind::Dev),
                        (TESTKIT, DepKind::Normal),
                        ("a-server", DepKind::Dev),
                        ("a-client", DepKind::Dev),
                        ("a-native", DepKind::Dev),
                        ("b-server", DepKind::Dev),
                        ("mantis-ui", DepKind::Dev),
                    ],
                ),
                member(ADAPTER_CONTRACT, "/w/crates/adapter-contract/Cargo.toml", &[]),
                member("mantis-core", "/w/crates/core/Cargo.toml", &[]),
                member("mantis-net", "/w/crates/net/Cargo.toml", &[]),
                member("mantis-server", "/w/crates/server/Cargo.toml", &[]),
                member("mantis-client", "/w/crates/client/Cargo.toml", &[]),
                member("mantis-ui", "/w/crates/ui/Cargo.toml", &[]),
                member(
                    TESTKIT,
                    "/w/crates/testkit/Cargo.toml",
                    &[("a-e2e", DepKind::Normal)],
                ),
            ],
        };
        let v = check(&ws, CRATE_RULES);
        assert_eq!(
            v,
            vec![
                Violation::ForbiddenEdge {
                    from: "a-e2e".into(),
                    to: "b-server".into(),
                    kind: DepKind::Dev
                },
                Violation::ForbiddenEdge {
                    from: "a-e2e".into(),
                    to: "mantis-ui".into(),
                    kind: DepKind::Dev
                },
                Violation::ForbiddenEdge {
                    from: "a-server".into(),
                    to: "a-e2e".into(),
                    kind: DepKind::Dev
                },
                Violation::ForbiddenEdge {
                    from: TESTKIT.into(),
                    to: "a-e2e".into(),
                    kind: DepKind::Normal
                },
            ]
        );
    }

    #[test]
    fn modules_reach_contracts_only_and_only_hosts_reach_implementations() {
        let m = "/w/packages/std/modules";
        let ws = Workspace {
            members: vec![
                member(
                    "party-contract",
                    &format!("{m}/party/contract/Cargo.toml"),
                    &[("mantis-core", DepKind::Normal)],
                ),
                member(
                    "chat-contract",
                    &format!("{m}/chat/contract/Cargo.toml"),
                    &[
                        ("party-contract", DepKind::Normal),
                        ("mantis-server", DepKind::Normal),
                    ],
                ),
                member(
                    "party-server",
                    &format!("{m}/party/server/Cargo.toml"),
                    &[
                        ("mantis-server", DepKind::Normal),
                        ("party-contract", DepKind::Normal),
                        ("chat-contract", DepKind::Normal),
                        ("chat-server", DepKind::Normal),
                        ("mantis-client", DepKind::Normal),
                    ],
                ),
                member(
                    "chat-server",
                    &format!("{m}/chat/server/Cargo.toml"),
                    &[("mantis-server", DepKind::Normal)],
                ),
                member(
                    "party-client",
                    &format!("{m}/party/client/Cargo.toml"),
                    &[
                        ("mantis-ui", DepKind::Normal),
                        ("party-contract", DepKind::Normal),
                        ("party-server", DepKind::Normal),
                    ],
                ),
                member(
                    "g-server",
                    "/w/packages/g/server/Cargo.toml",
                    &[
                        ("party-server", DepKind::Normal),
                        ("party-contract", DepKind::Normal),
                        ("party-client", DepKind::Normal),
                    ],
                ),
                member("mantis-core", "/w/crates/core/Cargo.toml", &[]),
                member("mantis-server", "/w/crates/server/Cargo.toml", &[]),
                member("mantis-client", "/w/crates/client/Cargo.toml", &[]),
                member("mantis-ui", "/w/crates/ui/Cargo.toml", &[]),
            ],
        };
        let edge = |from: &str, to: &str| Violation::ForbiddenEdge {
            from: from.into(),
            to: to.into(),
            kind: DepKind::Normal,
        };
        assert_eq!(
            check(&ws, CRATE_RULES),
            vec![
                edge("chat-contract", "mantis-server"),
                edge("g-server", "party-client"),
                edge("party-client", "party-server"),
                edge("party-server", "chat-server"),
                edge("party-server", "mantis-client"),
            ]
        );
    }

    #[test]
    fn parses_cargo_metadata_shape() {
        let json = r#"{
            "packages": [
                {"id": "a 0.1.0", "name": "mantis-core", "manifest_path": "C:\\w\\crates\\core\\Cargo.toml",
                 "dependencies": [{"name": "mantis-testkit", "kind": "dev", "rename": null}]},
                {"id": "b 0.1.0", "name": "mantis-testkit", "manifest_path": "C:\\w\\crates\\testkit\\Cargo.toml",
                 "dependencies": [{"name": "serde_json", "kind": null}, {"name": "cc", "kind": "build"}]}
            ],
            "workspace_members": ["a 0.1.0", "b 0.1.0"]
        }"#;
        let ws = parse_metadata(json);
        assert_eq!(
            ws,
            Ok(Workspace {
                members: vec![
                    member(
                        "mantis-core",
                        "C:/w/crates/core/Cargo.toml",
                        &[(TESTKIT, DepKind::Dev)]
                    ),
                    member(
                        TESTKIT,
                        "C:/w/crates/testkit/Cargo.toml",
                        &[("serde_json", DepKind::Normal), ("cc", DepKind::Build)]
                    ),
                ]
            })
        );
        assert_eq!(
            parse_metadata("{"),
            Err(MetadataError::Json(
                "EOF while parsing an object at line 1 column 1".into()
            ))
        );
        assert_eq!(
            parse_metadata("{}"),
            Err(MetadataError::Shape("workspace_members"))
        );
    }
}

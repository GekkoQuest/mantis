//! mantis-modsync: links a package's modules into its host crate (plan 13,
//! decision 0010).
//!
//! Rust links only the crates a manifest names, so a package's server crate
//! cannot discover module crates at run time. This tool discovers them
//! instead: it reads every `packages/*/modules/*/manifest.toml`, resolves the
//! package's module graph (its own modules, those of the packages it `uses`,
//! minus its `overrides`), and writes
//!
//! - the module dependencies of `packages/<p>/<side>/Cargo.toml`, between
//!   the [`BEGIN`] and [`END`] marker lines, and
//! - `packages/<p>/<side>/src/modules.rs`: `linked()`, the linked module
//!   halves, and `MANIFESTS`, every manifest the host resolves at start-up
//!   (embedded, so the binary needs no module folders),
//!
//! for each host crate (`server`, `client`) that carries the markers.
//!
//! Nothing else may name a module implementation crate; an architecture test
//! enforces that. Removing a module is deleting its folder and running this
//! tool. A freshness test fails when the generated files are out of date.

#![forbid(unsafe_code)]

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};

use mantis_core::module::{Discovered, Manifest, parse_manifest, parse_package, resolve};

/// First line of the generated block in a host crate's `Cargo.toml`.
pub const BEGIN: &str = "# BEGIN GENERATED MODULES (mantis-modsync; do not edit)";

/// Last line of the generated block.
pub const END: &str = "# END GENERATED MODULES";

/// The tool failed; the message says where and why.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct SyncError(pub String);

impl std::fmt::Display for SyncError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for SyncError {}

fn err(what: impl Into<String>) -> SyncError {
    SyncError(what.into())
}

fn read(path: &Path) -> Result<String, SyncError> {
    std::fs::read_to_string(path).map_err(|e| err(format!("{}: {e}", path.display())))
}

/// One module found on disk.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct ModuleDir {
    /// The package folder it lives in.
    pub origin: String,
    /// Its folder name under `modules/`.
    pub feature: String,
    /// Its manifest.
    pub manifest: Manifest,
    /// The crate name of its `server/` crate, if it has one.
    pub server_crate: Option<String>,
    /// The crate name of its `client/` crate, if it has one.
    pub client_crate: Option<String>,
}

/// The `[package] name` of a crate manifest.
#[must_use]
pub fn crate_name(cargo_toml: &str) -> Option<String> {
    let mut in_package = false;
    for line in cargo_toml.lines().map(str::trim) {
        if line.starts_with('[') {
            in_package = line == "[package]";
            continue;
        }
        if in_package
            && let Some(rest) = line.strip_prefix("name")
            && let Some(value) = rest.trim_start().strip_prefix('=')
        {
            return Some(value.trim().trim_matches('"').to_owned());
        }
    }
    None
}

fn sorted_dirs(dir: &Path) -> Vec<PathBuf> {
    let mut out: Vec<PathBuf> = std::fs::read_dir(dir)
        .map(|rd| {
            rd.filter_map(Result::ok)
                .map(|e| e.path())
                .filter(|p| p.is_dir())
                .collect()
        })
        .unwrap_or_default();
    out.sort();
    out
}

fn file_name(p: &Path) -> String {
    p.file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default()
}

/// Every module under `root/packages/*/modules/*`, in path order.
///
/// # Errors
/// [`SyncError`] for an unreadable or invalid manifest.
pub fn discover(root: &Path) -> Result<Vec<ModuleDir>, SyncError> {
    let mut out = Vec::new();
    for package in sorted_dirs(&root.join("packages")) {
        for module in sorted_dirs(&package.join("modules")) {
            let manifest_path = module.join("manifest.toml");
            if !manifest_path.is_file() {
                continue;
            }
            let manifest = parse_manifest(&read(&manifest_path)?)
                .map_err(|e| err(format!("{}: {e}", manifest_path.display())))?;
            let crate_of = |side: &str| -> Result<Option<String>, SyncError> {
                let path = module.join(side).join("Cargo.toml");
                if path.is_file() {
                    crate_name(&read(&path)?)
                        .map(Some)
                        .ok_or_else(|| err(format!("{}: no package name", path.display())))
                } else {
                    Ok(None)
                }
            };
            out.push(ModuleDir {
                origin: file_name(&package),
                feature: file_name(&module),
                server_crate: crate_of("server")?,
                client_crate: crate_of("client")?,
                manifest,
            });
        }
    }
    Ok(out)
}

/// The two generated files of one package's server crate.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Generated {
    /// `packages/<p>/server/Cargo.toml`.
    pub cargo_toml_path: PathBuf,
    /// Its full new text.
    pub cargo_toml: String,
    /// `packages/<p>/server/src/modules.rs`.
    pub modules_rs_path: PathBuf,
    /// Its full new text.
    pub modules_rs: String,
}

/// Replaces the generated block of `cargo_toml` with `lines`.
///
/// # Errors
/// [`SyncError`] when the markers are missing or out of order.
pub fn replace_block(cargo_toml: &str, lines: &[String]) -> Result<String, SyncError> {
    let begin = cargo_toml
        .find(BEGIN)
        .ok_or_else(|| err(format!("missing the line `{BEGIN}` in the [dependencies] table")))?;
    let end = cargo_toml
        .find(END)
        .filter(|e| *e > begin)
        .ok_or_else(|| err(format!("missing the line `{END}`")))?;
    let mut out = String::with_capacity(cargo_toml.len());
    out.push_str(&cargo_toml[..begin]);
    out.push_str(BEGIN);
    out.push('\n');
    for l in lines {
        out.push_str(l);
        out.push('\n');
    }
    out.push_str(&cargo_toml[end..]);
    Ok(out)
}

/// `cargo_toml` with the generated block removed (for the check that no
/// other place names a module implementation crate).
#[must_use]
pub fn outside_block(cargo_toml: &str) -> String {
    match (cargo_toml.find(BEGIN), cargo_toml.find(END)) {
        (Some(b), Some(e)) if e > b => format!("{}{}", &cargo_toml[..b], &cargo_toml[e..]),
        _ => cargo_toml.to_owned(),
    }
}

/// Which host crate of a package.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Side {
    /// `packages/<p>/server`, linking module `server/` crates.
    Server,
    /// `packages/<p>/client`, linking module `client/` crates.
    Client,
}

impl Side {
    /// The folder name of the host crate and of the module crates.
    #[must_use]
    pub const fn dir(self) -> &'static str {
        match self {
            Self::Server => "server",
            Self::Client => "client",
        }
    }

    const fn trait_path(self) -> (&'static str, &'static str) {
        match self {
            Self::Server => ("mantis_server::modules::ServerModule", "ServerModule"),
            Self::Client => ("mantis_client::modules::ClientModule", "ClientModule"),
        }
    }

    fn crate_of(self, m: &ModuleDir) -> Option<&str> {
        match self {
            Self::Server => m.server_crate.as_deref(),
            Self::Client => m.client_crate.as_deref(),
        }
    }
}

/// Generates the server-side files of `package` (see [`generate_side`]).
///
/// # Errors
/// [`SyncError`].
pub fn generate(root: &Path, package: &str) -> Result<Generated, SyncError> {
    generate_side(root, package, Side::Server)
}

/// Generates one host crate's files for `package`. On the server side
/// every resolved module must have a server crate; on the client side,
/// modules without a client crate (server-only features) are skipped.
///
/// # Errors
/// [`SyncError`]: unreadable files, an unresolvable graph (the same refusal
/// the host would give at start-up), or a module without a server crate.
pub fn generate_side(root: &Path, package: &str, side: Side) -> Result<Generated, SyncError> {
    let found = discover(root)?;
    let package_toml = root.join("packages").join(package).join("package.toml");
    let modules_of =
        parse_package(&read(&package_toml)?).map_err(|e| err(format!("{}: {e}", package_toml.display())))?;
    let included: Vec<&ModuleDir> = found
        .iter()
        .filter(|m| m.origin == package || modules_of.uses.contains(&m.origin))
        .collect();
    let discovered: Vec<Discovered> = included
        .iter()
        .map(|m| Discovered {
            origin: m.origin.clone(),
            manifest: m.manifest.clone(),
        })
        .collect();
    let graph = resolve(&modules_of, &discovered, &BTreeMap::new())
        .map_err(|e| err(format!("package {package}: {e}")))?;
    let mut linked: Vec<(&str, &ModuleDir)> = Vec::new();
    for r in &graph.modules {
        let m = included
            .iter()
            .find(|m| m.manifest.key == r.key)
            .ok_or_else(|| err(format!("{} vanished", r.key)))?;
        match (side.crate_of(m), side) {
            (Some(krate), _) => linked.push((krate, m)),
            (None, Side::Server) => return Err(err(format!("module {} has no server crate", r.key))),
            (None, Side::Client) => {}
        }
    }
    linked.sort_by(|a, b| a.0.cmp(b.0));

    let host = root.join("packages").join(package).join(side.dir());
    let cargo_toml_path = host.join("Cargo.toml");
    let deps: Vec<String> = linked
        .iter()
        .map(|(krate, m)| {
            format!(
                "{krate} = {{ path = \"../../{}/modules/{}/{}\" }}",
                m.origin,
                m.feature,
                side.dir()
            )
        })
        .collect();
    let cargo_toml = replace_block(&read(&cargo_toml_path)?, &deps)?;

    let (path, name) = side.trait_path();
    let mut rs = String::new();
    let _ = writeln!(
        rs,
        "//! The modules linked into this package's {}. Generated by",
        side.dir()
    );
    let _ = writeln!(
        rs,
        "//! `mantis-modsync` for package `{package}`; do not edit. Regenerate with"
    );
    rs.push_str("//! `cargo run -p mantis-modsync -- <workspace root> <package>`.\n\n");
    let _ = writeln!(rs, "use std::sync::Arc;\n\nuse {path};\n");
    let _ = writeln!(
        rs,
        "/// Every linked module's {} half, in crate-name order.\n#[must_use]\n#[rustfmt::skip]",
        side.dir()
    );
    let _ = writeln!(rs, "pub fn linked() -> Vec<Arc<dyn {name}>> {{\n    vec![");
    for (krate, _) in &linked {
        let _ = writeln!(rs, "        Arc::new({}::Module),", krate.replace('-', "_"));
    }
    rs.push_str("    ]\n}\n\n");
    rs.push_str("/// Every module manifest the package resolves at start-up, as\n");
    rs.push_str("/// `(origin package, manifest text)`.\n#[rustfmt::skip]\n");
    rs.push_str("pub const MANIFESTS: &[(&str, &str)] = &[\n");
    for m in &included {
        let _ = writeln!(
            rs,
            "    (\"{}\", include_str!(\"../../../{}/modules/{}/manifest.toml\")),",
            m.origin, m.origin, m.feature
        );
    }
    rs.push_str("];\n");
    Ok(Generated {
        cargo_toml_path,
        cargo_toml,
        modules_rs_path: host.join("src").join("modules.rs"),
        modules_rs: rs,
    })
}

/// The generated files whose contents differ from disk.
#[must_use]
pub fn stale(g: &Generated) -> Vec<PathBuf> {
    [
        (&g.cargo_toml_path, &g.cargo_toml),
        (&g.modules_rs_path, &g.modules_rs),
    ]
    .into_iter()
    .filter(|(path, text)| std::fs::read_to_string(path).ok().as_deref() != Some(text.as_str()))
    .map(|(path, _)| path.clone())
    .collect()
}

/// Writes the generated files.
///
/// # Errors
/// [`SyncError`] when a file cannot be written.
pub fn write(g: &Generated) -> Result<(), SyncError> {
    for (path, text) in [
        (&g.cargo_toml_path, &g.cargo_toml),
        (&g.modules_rs_path, &g.modules_rs),
    ] {
        std::fs::write(path, text).map_err(|e| err(format!("{}: {e}", path.display())))?;
    }
    Ok(())
}

/// Every `(package, side)` whose host crate's `Cargo.toml` carries the
/// markers.
#[must_use]
pub fn hosts(root: &Path) -> Vec<(String, Side)> {
    let mut out = Vec::new();
    for p in sorted_dirs(&root.join("packages")) {
        for side in [Side::Server, Side::Client] {
            let marked = std::fs::read_to_string(p.join(side.dir()).join("Cargo.toml"))
                .is_ok_and(|t| t.contains(BEGIN));
            if marked {
                out.push((file_name(&p), side));
            }
        }
    }
    out
}

/// The dependency names a crate manifest declares, outside the generated
/// block, in any dependency table.
#[must_use]
pub fn declared_dependencies(cargo_toml: &str) -> Vec<String> {
    let text = outside_block(cargo_toml);
    let mut in_deps = false;
    let mut out = Vec::new();
    for line in text.lines().map(str::trim) {
        if line.starts_with('[') {
            in_deps = line.ends_with("dependencies]");
            continue;
        }
        if !in_deps || line.starts_with('#') {
            continue;
        }
        if let Some((name, _)) = line.split_once('=') {
            out.push(name.trim().trim_matches('"').to_owned());
        }
    }
    out
}

/// Checks every package override: for each module a package's
/// `package.toml` overrides, the package must have a module implementing
/// the overridden module's contract (its manifest's `contract`), and that
/// module's server crate must depend on the overridden module's contract
/// crate, so it implements the very types other modules use. Returns the
/// problems found (empty when every override is sound).
///
/// # Errors
/// [`SyncError`] when files cannot be read.
pub fn check_overrides(root: &Path) -> Result<Vec<String>, SyncError> {
    let found = discover(root)?;
    let mut problems = Vec::new();
    for package in sorted_dirs(&root.join("packages")) {
        let toml_path = package.join("package.toml");
        let Ok(text) = std::fs::read_to_string(&toml_path) else {
            continue;
        };
        let modules_of = parse_package(&text).map_err(|e| err(format!("{}: {e}", toml_path.display())))?;
        let name = file_name(&package);
        for o in &modules_of.overrides {
            let Some(old) = found.iter().find(|m| &m.manifest.key == o && m.origin != name) else {
                problems.push(format!("{name}: overrides {o}, which no used package provides"));
                continue;
            };
            let contract_dir = root
                .join("packages")
                .join(&old.origin)
                .join("modules")
                .join(&old.feature)
                .join("contract")
                .join("Cargo.toml");
            let contract_crate = std::fs::read_to_string(&contract_dir)
                .ok()
                .and_then(|t| crate_name(&t));
            let Some(contract_crate) = contract_crate else {
                problems.push(format!("{name}: {o} has no contract crate to implement"));
                continue;
            };
            let Some(new) = found
                .iter()
                .find(|m| m.origin == name && m.manifest.contract == old.manifest.contract)
            else {
                problems.push(format!(
                    "{name}: overrides {o} but no module of its own implements {}",
                    old.manifest.contract
                ));
                continue;
            };
            let server = root
                .join("packages")
                .join(&name)
                .join("modules")
                .join(&new.feature)
                .join("server")
                .join("Cargo.toml");
            let deps = std::fs::read_to_string(&server)
                .map(|t| declared_dependencies(&t))
                .unwrap_or_default();
            if !deps.contains(&contract_crate) {
                problems.push(format!(
                    "{name}: {} overrides {o} but its server crate does not depend on {contract_crate}",
                    new.manifest.key
                ));
            }
        }
    }
    Ok(problems)
}

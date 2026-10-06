//! Resolving a package's module graph: which modules are in, which are
//! enabled, in what order they register, and why a host must refuse to start.

use core::fmt;
use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;

use super::manifest::{Manifest, PackageModules, Version};

/// A module found on disk, with the package it came from.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Discovered {
    /// The package folder it lives in (`std`, `toy`, ...).
    pub origin: String,
    /// Its manifest.
    pub manifest: Manifest,
}

/// One module in a resolved graph.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Resolved {
    /// Module key.
    pub key: String,
    /// Its version.
    pub version: Version,
    /// The contract it implements.
    pub contract: String,
    /// The package it came from.
    pub origin: String,
    /// The module key it replaces, when it overrides one.
    pub overrides: Option<String>,
    /// Its `enabled` flag after package and live overrides.
    pub enabled: bool,
    /// Every flag, by short name, after overrides.
    pub flags: BTreeMap<String, bool>,
    /// Module keys it depends on (resolved from contracts), sorted.
    pub depends_on: Vec<String>,
    /// Gameplay graph actions it declares (its manifest's `graph_actions`).
    pub graph_actions: Vec<String>,
}

/// A package's resolved modules, in registration order (dependencies first;
/// ties broken by key, so the order is the same on every machine).
#[derive(Clone, PartialEq, Eq, Debug, Default)]
pub struct ModuleGraph {
    /// The package.
    pub package: String,
    /// Modules in registration order.
    pub modules: Vec<Resolved>,
}

impl ModuleGraph {
    /// Every declared gameplay graph action, in registration order (module
    /// order, then manifest order): the order a cell's catalog registers
    /// them in, so action ids are the same on every machine.
    #[must_use]
    pub fn graph_actions(&self) -> Vec<String> {
        self.modules
            .iter()
            .flat_map(|m| m.graph_actions.iter().cloned())
            .collect()
    }

    /// The module with `key`.
    #[must_use]
    pub fn get(&self, key: &str) -> Option<&Resolved> {
        self.modules.iter().find(|m| m.key == key)
    }

    /// The module implementing `contract`.
    #[must_use]
    pub fn implementer(&self, contract: &str) -> Option<&Resolved> {
        self.modules.iter().find(|m| m.contract == contract)
    }

    /// True when `key` is present and enabled.
    #[must_use]
    pub fn is_enabled(&self, key: &str) -> bool {
        self.get(key).is_some_and(|m| m.enabled)
    }

    /// The value of a full flag name (`<module key>.<flag>`).
    #[must_use]
    pub fn flag(&self, full: &str) -> Option<bool> {
        let (key, flag) = full.rsplit_once('.')?;
        self.get(key)?.flags.get(flag).copied()
    }

    /// The graph as printed at host startup.
    #[must_use]
    pub fn render(&self) -> String {
        let enabled = self.modules.iter().filter(|m| m.enabled).count();
        let mut s = format!(
            "modules of package {} ({} resolved, {} enabled):\n",
            self.package,
            self.modules.len(),
            enabled
        );
        for m in &self.modules {
            let _ = write!(
                s,
                "  {} {} [{}] {}",
                m.key,
                m.version,
                m.origin,
                if m.enabled { "enabled" } else { "DISABLED" }
            );
            if m.contract != m.key {
                let _ = write!(s, ", implements {}", m.contract);
            }
            if let Some(o) = &m.overrides {
                let _ = write!(s, ", overrides {o}");
            }
            if !m.depends_on.is_empty() {
                let _ = write!(s, ", needs {}", m.depends_on.join(", "));
            }
            let off: Vec<&str> = m
                .flags
                .iter()
                .filter(|(k, v)| k.as_str() != "enabled" && !**v)
                .map(|(k, _)| k.as_str())
                .collect();
            if !off.is_empty() {
                let _ = write!(s, ", off: {}", off.join(", "));
            }
            s.push('\n');
        }
        s
    }
}

/// Why a package's modules cannot start.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum ResolveError {
    /// Two modules have the same key.
    DuplicateKey(String),
    /// Two modules implement the same contract and neither overrides.
    DuplicateContract {
        /// The contract.
        contract: String,
        /// The modules.
        modules: [String; 2],
    },
    /// An override names a module that is not included.
    OverrideUnknown(String),
    /// An override has no module of the package implementing the replaced
    /// module's contract.
    OverrideUnimplemented {
        /// The replaced module.
        module: String,
        /// The contract nobody implements now.
        contract: String,
    },
    /// An enabled module needs a contract nobody implements.
    MissingDependency {
        /// The module.
        module: String,
        /// The contract.
        contract: String,
    },
    /// An enabled module needs a contract whose implementer is disabled.
    DisabledDependency {
        /// The module.
        module: String,
        /// The disabled implementer.
        dependency: String,
    },
    /// The implementer's version does not satisfy the requirement.
    VersionMismatch {
        /// The module.
        module: String,
        /// The implementer.
        dependency: String,
        /// Required.
        required: Version,
        /// Found.
        found: Version,
    },
    /// A flag override names a flag no module declares.
    UnknownFlag(String),
    /// Modules depend on each other in a cycle.
    Cycle(Vec<String>),
}

impl fmt::Display for ResolveError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::DuplicateKey(k) => write!(f, "two modules are named {k}"),
            Self::DuplicateContract { contract, modules } => write!(
                f,
                "{} and {} both implement {contract}; override one",
                modules[0], modules[1]
            ),
            Self::OverrideUnknown(m) => write!(f, "override of {m}, which is not included"),
            Self::OverrideUnimplemented { module, contract } => write!(
                f,
                "{module} is overridden but no module of the package implements {contract}"
            ),
            Self::MissingDependency { module, contract } => {
                write!(f, "{module} needs {contract}, which no module implements")
            }
            Self::DisabledDependency { module, dependency } => {
                write!(f, "{module} needs {dependency}, which is disabled")
            }
            Self::VersionMismatch {
                module,
                dependency,
                required,
                found,
            } => write!(f, "{module} needs {dependency} ^{required}, found {found}"),
            Self::UnknownFlag(flag) => write!(f, "no module declares the flag {flag}"),
            Self::Cycle(keys) => write!(f, "dependency cycle among {}", keys.join(", ")),
        }
    }
}

impl std::error::Error for ResolveError {}

type Flags<'a> = BTreeMap<&'a str, BTreeMap<String, bool>>;

/// The included modules after overrides, and the overridden key per contract.
fn include<'a>(
    package: &'a PackageModules,
    found: &'a [Discovered],
) -> Result<(Vec<&'a Discovered>, BTreeMap<&'a str, &'a str>), ResolveError> {
    let included: Vec<&Discovered> = found
        .iter()
        .filter(|d| d.origin == package.name || package.uses.contains(&d.origin))
        .collect();
    let mut keys = BTreeSet::new();
    for d in &included {
        if !keys.insert(d.manifest.key.as_str()) {
            return Err(ResolveError::DuplicateKey(d.manifest.key.clone()));
        }
    }
    let mut replaced = BTreeMap::new(); // contract -> overridden key
    for o in &package.overrides {
        let Some(old) = included
            .iter()
            .find(|d| &d.manifest.key == o && d.origin != package.name)
        else {
            return Err(ResolveError::OverrideUnknown(o.clone()));
        };
        let contract = old.manifest.contract.as_str();
        if !included
            .iter()
            .any(|d| d.origin == package.name && d.manifest.contract == contract)
        {
            return Err(ResolveError::OverrideUnimplemented {
                module: o.clone(),
                contract: contract.to_owned(),
            });
        }
        replaced.insert(contract, o.as_str());
    }
    let kept = included
        .into_iter()
        .filter(|d| !package.overrides.contains(&d.manifest.key))
        .collect();
    Ok((kept, replaced))
}

/// Manifest defaults, then package flags, then live flags.
fn flags<'a>(
    kept: &[&'a Discovered],
    package: &PackageModules,
    live: &BTreeMap<String, bool>,
) -> Result<Flags<'a>, ResolveError> {
    let mut flags: Flags<'a> = kept
        .iter()
        .map(|d| (d.manifest.key.as_str(), d.manifest.flags.clone()))
        .collect();
    // A package's flag defaults may name modules it does not include (a
    // module whose folder was removed): those are skipped, so removing a
    // module never breaks the package. A flag of an included module must
    // exist. Live (Ops) flags must always name a real flag.
    let defaults = package.flags.iter().map(|(k, v)| (k, *v, true));
    let ops = live.iter().map(|(k, v)| (k, *v, false));
    for (full, value, from_package) in defaults.chain(ops) {
        let (key, flag) = full
            .rsplit_once('.')
            .ok_or_else(|| ResolveError::UnknownFlag(full.clone()))?;
        let Some(module) = flags.get_mut(key) else {
            if from_package {
                continue;
            }
            return Err(ResolveError::UnknownFlag(full.clone()));
        };
        let slot = module
            .get_mut(flag)
            .ok_or_else(|| ResolveError::UnknownFlag(full.clone()))?;
        *slot = value;
    }
    Ok(flags)
}

fn enabled(flags: &Flags<'_>, key: &str) -> bool {
    flags
        .get(key)
        .and_then(|f| f.get("enabled"))
        .copied()
        .unwrap_or(true)
}

/// Each module's dependencies as module keys, checked for enabled modules.
fn edges<'a>(
    kept: &[&'a Discovered],
    flags: &Flags<'_>,
) -> Result<BTreeMap<&'a str, Vec<String>>, ResolveError> {
    let mut by_contract: BTreeMap<&str, &Discovered> = BTreeMap::new();
    for d in kept {
        if let Some(other) = by_contract.insert(d.manifest.contract.as_str(), d) {
            return Err(ResolveError::DuplicateContract {
                contract: d.manifest.contract.clone(),
                modules: [other.manifest.key.clone(), d.manifest.key.clone()],
            });
        }
    }
    let mut edges = BTreeMap::new();
    for d in kept {
        let m = &d.manifest;
        let on = enabled(flags, &m.key);
        let mut deps = Vec::new();
        for dep in &m.dependencies {
            let Some(target) = by_contract.get(dep.contract.as_str()) else {
                if on {
                    return Err(ResolveError::MissingDependency {
                        module: m.key.clone(),
                        contract: dep.contract.clone(),
                    });
                }
                continue;
            };
            let t = &target.manifest;
            if on && !enabled(flags, &t.key) {
                return Err(ResolveError::DisabledDependency {
                    module: m.key.clone(),
                    dependency: t.key.clone(),
                });
            }
            if on && !t.version.satisfies(dep.version) {
                return Err(ResolveError::VersionMismatch {
                    module: m.key.clone(),
                    dependency: t.key.clone(),
                    required: dep.version,
                    found: t.version,
                });
            }
            deps.push(t.key.clone());
        }
        deps.sort();
        edges.insert(m.key.as_str(), deps);
    }
    Ok(edges)
}

/// Registration order: dependencies first, ties by key.
fn order<'a>(edges: &BTreeMap<&'a str, Vec<String>>) -> Result<Vec<&'a str>, ResolveError> {
    let mut order: Vec<&str> = Vec::with_capacity(edges.len());
    let mut done: BTreeSet<&str> = BTreeSet::new();
    while order.len() < edges.len() {
        let next = edges
            .iter()
            .find(|(k, deps)| !done.contains(*k) && deps.iter().all(|d| done.contains(d.as_str())))
            .map(|(k, _)| *k);
        let Some(k) = next else {
            let stuck = edges
                .keys()
                .filter(|k| !done.contains(*k))
                .map(|k| (*k).to_owned())
                .collect();
            return Err(ResolveError::Cycle(stuck));
        };
        done.insert(k);
        order.push(k);
    }
    Ok(order)
}

/// Resolves `package`'s module graph from everything `found` on disk.
///
/// Included are the package's own modules and those of the packages it
/// `uses`, minus the ones it `overrides`. Flags start at each manifest's
/// defaults, then the package's `[flags]`, then `live` (Ops) overrides.
///
/// # Errors
/// The first [`ResolveError`], so the host refuses to start and says why.
pub fn resolve(
    package: &PackageModules,
    found: &[Discovered],
    live: &BTreeMap<String, bool>,
) -> Result<ModuleGraph, ResolveError> {
    let (kept, replaced) = include(package, found)?;
    let flags = flags(&kept, package, live)?;
    let edges = edges(&kept, &flags)?;
    let modules = order(&edges)?
        .into_iter()
        .filter_map(|key| {
            let d = kept.iter().find(|d| d.manifest.key == key)?;
            let m = &d.manifest;
            Some(Resolved {
                key: m.key.clone(),
                version: m.version,
                contract: m.contract.clone(),
                origin: d.origin.clone(),
                overrides: replaced
                    .get(m.contract.as_str())
                    .filter(|_| d.origin == package.name)
                    .map(|k| (*k).to_owned()),
                enabled: enabled(&flags, key),
                flags: flags.get(key).cloned().unwrap_or_default(),
                depends_on: edges.get(key).cloned().unwrap_or_default(),
                graph_actions: m.graph_actions.clone(),
            })
        })
        .collect();
    Ok(ModuleGraph {
        package: package.name.clone(),
        modules,
    })
}

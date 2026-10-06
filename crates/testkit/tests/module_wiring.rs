//! The module system's structural rules (plan 13, decision 0010):
//!
//! - every host crate's generated module wiring is fresh (`mantis-modsync`);
//! - nothing outside a generated block names a module implementation crate,
//!   so removing a module folder and re-running the tool never breaks a
//!   build;
//! - every package override implements the overridden module's contract
//!   crate;
//! - the removal matrix (ignored by default; run in CI): for every module,
//!   delete it and every module depending on its contract from a copy of
//!   the workspace, regenerate the wiring, and build and test.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::process::Command;

use mantis_modsync::{
    ModuleDir, check_overrides, declared_dependencies, discover, generate_side, hosts, stale,
};

fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("..").join("..")
}

/// Every `Cargo.toml` under `crates/` and `packages/`.
fn manifests(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for e in entries.flatten() {
        let p = e.path();
        if p.is_dir() {
            if p.file_name().is_some_and(|n| n == "target" || n == "src") {
                continue;
            }
            manifests(&p, out);
        } else if p.file_name().is_some_and(|n| n == "Cargo.toml") {
            out.push(p);
        }
    }
}

#[test]
fn generated_module_wiring_is_fresh() -> Result<(), String> {
    let root = workspace_root();
    let mut problems = Vec::new();
    for (package, side) in hosts(&root) {
        let g = generate_side(&root, &package, side).map_err(|e| e.to_string())?;
        for path in stale(&g) {
            problems.push(format!(
                "{} is stale; run `cargo run -p mantis-modsync -- . {package}`",
                path.display()
            ));
        }
    }
    if problems.is_empty() {
        Ok(())
    } else {
        Err(problems.join("\n"))
    }
}

#[test]
fn only_generated_blocks_name_module_implementations() -> Result<(), String> {
    let root = workspace_root();
    let modules = discover(&root).map_err(|e| e.to_string())?;
    let implementations: BTreeSet<String> = modules
        .iter()
        .flat_map(|m: &ModuleDir| [m.server_crate.clone(), m.client_crate.clone()])
        .flatten()
        .collect();
    let mut all = Vec::new();
    for group in ["crates", "packages"] {
        manifests(&root.join(group), &mut all);
    }
    let mut problems = Vec::new();
    for path in all {
        let text = std::fs::read_to_string(&path).map_err(|e| format!("{}: {e}", path.display()))?;
        for dep in declared_dependencies(&text) {
            if implementations.contains(&dep) {
                problems.push(format!("{} names module implementation {dep}", path.display()));
            }
        }
    }
    if problems.is_empty() {
        Ok(())
    } else {
        Err(problems.join("\n"))
    }
}

#[test]
fn every_override_implements_the_overridden_contract() -> Result<(), String> {
    let problems = check_overrides(&workspace_root()).map_err(|e| e.to_string())?;
    if problems.is_empty() {
        Ok(())
    } else {
        Err(problems.join("\n"))
    }
}

fn write(path: &Path, text: &str) -> Result<(), String> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    }
    std::fs::write(path, text).map_err(|e| e.to_string())
}

fn module(
    root: &Path,
    package: &str,
    feature: &str,
    manifest: &str,
    server_deps: &str,
) -> Result<(), String> {
    let m = root.join("packages").join(package).join("modules").join(feature);
    write(&m.join("manifest.toml"), manifest)?;
    write(
        &m.join("contract").join("Cargo.toml"),
        &format!("[package]\nname = \"{package}-{feature}-contract\"\n"),
    )?;
    write(
        &m.join("server").join("Cargo.toml"),
        &format!("[package]\nname = \"{package}-{feature}-server\"\n\n[dependencies]\n{server_deps}"),
    )
}

#[test]
fn the_override_check_catches_an_override_that_skips_the_contract() -> Result<(), String> {
    let root = std::env::temp_dir().join(format!("mantis-override-check-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    module(
        &root,
        "std",
        "party",
        "[module]\nkey = \"std.party\"\nversion = \"0.1.0\"\n",
        "",
    )?;
    write(
        &root.join("packages/game/package.toml"),
        "[package]\nname = \"game\"\nuses = [\"std\"]\noverrides = [\"std.party\"]\n",
    )?;
    let manifest = "[module]\nkey = \"game.party\"\nversion = \"0.1.0\"\ncontract = \"std.party\"\n";
    module(
        &root,
        "game",
        "party",
        manifest,
        "mantis-core = { path = \"x\" }\n",
    )?;
    let bad = check_overrides(&root).map_err(|e| e.to_string())?;
    module(
        &root,
        "game",
        "party",
        manifest,
        "std-party-contract = { path = \"../../std/party/contract\" }\n",
    )?;
    let good = check_overrides(&root).map_err(|e| e.to_string())?;
    write(
        &root.join("packages/game/package.toml"),
        "[package]\nname = \"game\"\nuses = [\"std\"]\noverrides = [\"std.chat\"]\n",
    )?;
    let unknown = check_overrides(&root).map_err(|e| e.to_string())?;
    let _ = std::fs::remove_dir_all(&root);
    if bad.len() != 1
        || !bad
            .first()
            .is_some_and(|p| p.contains("does not depend on std-party-contract"))
    {
        return Err(format!("missed the skipped contract: {bad:?}"));
    }
    if !good.is_empty() {
        return Err(format!("flagged a sound override: {good:?}"));
    }
    if unknown.len() != 1 || !unknown.first().is_some_and(|p| p.contains("std.chat")) {
        return Err(format!("missed the unknown override: {unknown:?}"));
    }
    Ok(())
}

fn copy_tree(from: &Path, to: &Path) -> Result<(), String> {
    std::fs::create_dir_all(to).map_err(|e| e.to_string())?;
    for e in std::fs::read_dir(from).map_err(|e| e.to_string())?.flatten() {
        let p = e.path();
        let name = e.file_name();
        if name == "target" || name == ".git" {
            continue;
        }
        let dest = to.join(&name);
        if p.is_dir() {
            copy_tree(&p, &dest)?;
        } else {
            std::fs::copy(&p, &dest).map_err(|e| format!("{}: {e}", p.display()))?;
        }
    }
    Ok(())
}

fn cargo(dir: &Path, target: &Path, args: &[&str]) -> Result<(), String> {
    let out = Command::new(std::env::var("CARGO").unwrap_or_else(|_| "cargo".to_owned()))
        .args(args)
        .current_dir(dir)
        .env("CARGO_TARGET_DIR", target)
        .output()
        .map_err(|e| e.to_string())?;
    if out.status.success() {
        return Ok(());
    }
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    let interesting: Vec<&str> = text
        .lines()
        .filter(|l| {
            l.contains("FAILED") || l.contains("panicked") || l.starts_with("error") || l.contains("-->")
        })
        .take(40)
        .collect();
    Err(format!(
        "cargo {} failed:
{}",
        args.join(" "),
        interesting.join(
            "
"
        )
    ))
}

/// The removal matrix. Slow (a workspace build per module), so it runs
/// in CI: `cargo test -p mantis-testkit --test module_wiring -- --ignored`.
#[test]
#[ignore = "removal matrix: a workspace build per module; run in CI"]
fn removing_any_module_folder_keeps_build_and_tests_green() -> Result<(), String> {
    let root = workspace_root();
    let modules = discover(&root).map_err(|e| e.to_string())?;
    let scratch = std::env::temp_dir().join(format!("mantis-removal-{}", std::process::id()));
    let target = scratch.join("target");
    for victim in &modules {
        // The module and everything depending on its contract, transitively.
        let mut gone: BTreeSet<String> = BTreeSet::from([victim.manifest.contract.clone()]);
        loop {
            let more: Vec<String> = modules
                .iter()
                .filter(|m| !gone.contains(&m.manifest.contract))
                .filter(|m| m.manifest.dependencies.iter().any(|d| gone.contains(&d.contract)))
                .map(|m| m.manifest.contract.clone())
                .collect();
            if more.is_empty() {
                break;
            }
            gone.extend(more);
        }
        let copy = scratch.join("ws");
        let _ = std::fs::remove_dir_all(&copy);
        copy_tree(&root, &copy)?;
        for m in modules.iter().filter(|m| gone.contains(&m.manifest.contract)) {
            let dir = copy
                .join("packages")
                .join(&m.origin)
                .join("modules")
                .join(&m.feature);
            std::fs::remove_dir_all(&dir).map_err(|e| format!("{}: {e}", dir.display()))?;
        }
        for (package, side) in hosts(&copy) {
            let g = generate_side(&copy, &package, side).map_err(|e| format!("without {gone:?}: {e}"))?;
            mantis_modsync::write(&g).map_err(|e| e.to_string())?;
        }
        cargo(&copy, &target, &["build", "--workspace", "--all-targets"])
            .map_err(|e| format!("without {gone:?}: {e}"))?;
        cargo(&copy, &target, &["test", "--workspace"]).map_err(|e| format!("without {gone:?}: {e}"))?;
    }
    let _ = std::fs::remove_dir_all(&scratch);
    Ok(())
}

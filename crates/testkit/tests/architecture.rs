//! Architecture test: dependencies point down only (plan section 5).
//!
//! Runs `cargo metadata` on the real workspace and checks every member against
//! `mantis_testkit::arch::CRATE_RULES`. A new crate fails this test until it is
//! classified.

use std::path::{Path, PathBuf};
use std::process::Command;

use mantis_testkit::arch::{CRATE_RULES, SIM_CRATES, check, check_lint_attrs, parse_metadata};

fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("..").join("..")
}

fn workspace_manifest() -> PathBuf {
    workspace_root().join("Cargo.toml")
}

#[test]
fn dependency_arrows_point_down() -> Result<(), String> {
    let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".to_owned());
    let out = Command::new(cargo)
        .args([
            "metadata",
            "--format-version",
            "1",
            "--no-deps",
            "--offline",
            "--manifest-path",
        ])
        .arg(workspace_manifest())
        .output()
        .map_err(|e| format!("failed to run cargo metadata: {e}"))?;
    if !out.status.success() {
        return Err(format!(
            "cargo metadata failed: {}",
            String::from_utf8_lossy(&out.stderr)
        ));
    }
    let json = String::from_utf8(out.stdout).map_err(|e| e.to_string())?;
    let ws = parse_metadata(&json).map_err(|e| e.to_string())?;
    if !ws.members.iter().any(|m| m.name == "mantis-core") {
        return Err("metadata does not contain mantis-core; wrong workspace?".to_owned());
    }
    let violations = check(&ws, CRATE_RULES);
    if violations.is_empty() {
        Ok(())
    } else {
        let lines: Vec<String> = violations.iter().map(ToString::to_string).collect();
        Err(format!("architecture violations:\n  {}", lines.join("\n  ")))
    }
}

/// Every simulation crate carries the canonical lint configuration
/// (decision 0013): no std float transcendentals, no wall clock, no randomly
/// seeded hash maps.
#[test]
fn simulation_crates_share_the_canonical_lint_config() -> Result<(), String> {
    let root = workspace_root();
    let read = |rel: &str| {
        std::fs::read(root.join(rel).join("clippy.toml")).map_err(|e| format!("{rel}/clippy.toml: {e}"))
    };
    let canonical = read("crates/core")?;
    let text = String::from_utf8_lossy(&canonical);
    for banned in [
        "f32::sin",
        "f64::exp",
        "f32::powf",
        "std::time::Instant::now",
        "std::collections::HashMap",
    ] {
        if !text.contains(&format!("\"{banned}\"")) {
            return Err(format!("canonical clippy.toml does not ban {banned}"));
        }
    }
    for rel in SIM_CRATES {
        if read(rel)? != canonical {
            return Err(format!("{rel}/clippy.toml differs from crates/core/clippy.toml"));
        }
    }
    Ok(())
}

fn rust_files_under(dir: &Path, out: &mut Vec<PathBuf>) -> Result<(), String> {
    let entries = std::fs::read_dir(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    for entry in entries {
        let path = entry.map_err(|e| e.to_string())?.path();
        if path.is_dir() {
            if path.file_name().is_some_and(|n| n == "target") {
                continue;
            }
            rust_files_under(&path, out)?;
        } else if path.extension().is_some_and(|x| x == "rs") {
            out.push(path);
        }
    }
    Ok(())
}

/// Lint attributes (lead rulings): lints are silenced with `expect`, so a
/// stale one fails the build; file-level only under `tests/**`, naming only
/// `TEST_FILE_EXPECTS`; item-level `allow` of a clippy lint only at
/// `ITEM_ALLOWS`; file-level `allow` only for `dead_code` in a shared test
/// module. Covers every crate and every package, at any depth.
#[test]
fn lint_attributes_follow_the_rule() -> Result<(), String> {
    let root = workspace_root();
    let mut files = Vec::new();
    for group in ["crates", "packages"] {
        let dir = root.join(group);
        if dir.is_dir() {
            rust_files_under(&dir, &mut files)?;
        }
    }
    if files.len() < 10 {
        return Err("found almost no Rust files; wrong workspace root?".to_owned());
    }
    let mut violations = Vec::new();
    for file in &files {
        let text = std::fs::read_to_string(file).map_err(|e| format!("{}: {e}", file.display()))?;
        let rel = file
            .strip_prefix(&root)
            .unwrap_or(file)
            .to_string_lossy()
            .replace('\\', "/");
        violations.extend(check_lint_attrs(&rel, &text));
    }
    if violations.is_empty() {
        Ok(())
    } else {
        Err(format!(
            "lint attribute violations (allow instead of expect, or not permitted):\n  {}",
            violations.join("\n  ")
        ))
    }
}

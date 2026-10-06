//! Every IDL schema in the workspace regenerates to exactly its checked-in
//! Rust (decision 0017), and every registry still extends its lock (the
//! append-only id rule of plan 6.7).
//!
//! Layout: `<crate>/schema/<name>.idl` with `<name>.registry` and
//! `<name>.lock` beside it, generated into `<crate>/src/generated/<name>.rs`.
//! Regenerate with
//! `cargo run -p mantis-idl -- <crate>/schema/<name>.idl <crate>/src/generated/<name>.rs`.

use std::path::{Path, PathBuf};

fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("..").join("..")
}

/// Every `schema/*.idl` under `crates/` and `packages/`, at any depth.
fn schemas(dir: &Path, out: &mut Vec<PathBuf>) -> Result<(), String> {
    let entries = std::fs::read_dir(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    for entry in entries {
        let path = entry.map_err(|e| e.to_string())?.path();
        if path.is_dir() {
            if path.file_name().is_some_and(|n| n == "target" || n == "src") {
                continue;
            }
            schemas(&path, out)?;
        } else if path.extension().is_some_and(|x| x == "idl")
            && path
                .parent()
                .and_then(Path::file_name)
                .is_some_and(|n| n == "schema")
        {
            out.push(path);
        }
    }
    Ok(())
}

#[test]
fn generated_code_is_fresh() -> Result<(), String> {
    let root = workspace_root();
    let mut found = Vec::new();
    for group in ["crates", "packages"] {
        let dir = root.join(group);
        if dir.is_dir() {
            schemas(&dir, &mut found)?;
        }
    }
    if found.is_empty() {
        return Err("no schemas found; wrong workspace root?".to_owned());
    }
    let mut problems = Vec::new();
    for schema_path in &found {
        let read = |ext: &str| {
            let p = schema_path.with_extension(ext);
            std::fs::read_to_string(&p).map_err(|e| format!("{}: {e}", p.display()))
        };
        let name = schema_path
            .file_stem()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_default();
        let unit = mantis_idl::Unit {
            schema: read("idl")?,
            registry: read("registry")?,
            lock: read("lock")?,
        };
        let origin = format!("{name}.idl");
        let generated = match mantis_idl::compile(&unit, &origin) {
            Ok(code) => code,
            Err(e) => {
                problems.push(format!("{}: {e}", schema_path.display()));
                continue;
            }
        };
        let crate_dir = schema_path
            .parent()
            .and_then(Path::parent)
            .ok_or("schema has no crate dir")?;
        let target = crate_dir.join("src").join("generated").join(format!("{name}.rs"));
        let current = std::fs::read_to_string(&target)
            .map_err(|e| format!("{}: {e}", target.display()))?
            .replace("\r\n", "\n");
        if current != generated {
            problems.push(format!(
                "{} is stale; run `cargo run -p mantis-idl -- {} {}`",
                target.display(),
                schema_path.display(),
                target.display()
            ));
        }
    }
    if problems.is_empty() {
        Ok(())
    } else {
        Err(problems.join("\n"))
    }
}

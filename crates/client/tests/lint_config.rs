//! The client is a simulation-side crate (decision 0013), so its `clippy.toml` must be a
//! byte-for-byte copy of the canonical one in `crates/core`. Drift is a test failure.

use std::path::Path;

#[test]
fn clippy_toml_matches_core_byte_for_byte() -> Result<(), Box<dyn std::error::Error>> {
    let here = Path::new(env!("CARGO_MANIFEST_DIR"));
    let ours = std::fs::read(here.join("clippy.toml"))?;
    let canonical = std::fs::read(here.join("../core/clippy.toml"))?;
    assert!(
        ours == canonical,
        "crates/client/clippy.toml differs from crates/core/clippy.toml; copy the canonical file"
    );
    Ok(())
}

/// Render-thread modules (paths under `src/`) that may use `glam`, render-side math. Every
/// other module is treated as simulation-side and must not (lead ruling: glam never in
/// simulation code). The list is an allowlist, so a new module is sim-side until added.
const GLAM_ALLOWED: &[&str] = &[
    "presentation.rs",
    "characters.rs",
    "media.rs",
    "media/",
    "world_view.rs",
    "world_stream.rs",
];

fn rust_files(dir: &Path, out: &mut Vec<std::path::PathBuf>) -> std::io::Result<()> {
    for entry in std::fs::read_dir(dir)? {
        let path = entry?.path();
        if path.is_dir() {
            rust_files(&path, out)?;
        } else if path.extension().is_some_and(|e| e == "rs") {
            out.push(path);
        }
    }
    Ok(())
}

#[test]
fn glam_stays_in_render_thread_modules() -> Result<(), Box<dyn std::error::Error>> {
    let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let mut files = Vec::new();
    rust_files(&src, &mut files)?;
    assert!(files.len() > 10, "found the sources");
    let mut offenders = Vec::new();
    for file in files {
        let relative = file
            .strip_prefix(&src)?
            .to_string_lossy()
            .replace(std::path::MAIN_SEPARATOR, "/");
        if GLAM_ALLOWED
            .iter()
            .any(|a| relative == *a || (a.ends_with('/') && relative.starts_with(a)))
        {
            continue;
        }
        let text = std::fs::read_to_string(&file)?;
        if text.contains("glam") {
            offenders.push(relative);
        }
    }
    assert!(
        offenders.is_empty(),
        "glam used in simulation-side modules: {offenders:?}"
    );
    Ok(())
}

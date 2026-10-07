//! The one way every toy subcommand (`serve`, `cluster`, `soak`, `replay`,
//! `bots`, `node`) resolves its content: the cooked, signed gameplay bundle,
//! verified, at [`world::COOKED_PATH`] (the checked-in cook) unless a
//! directory is given. The content hash goes into log headers and client
//! handshakes, and cells run that bundle's graphs, so a log `soak` writes
//! is one `replay` accepts with the same (default) arguments.

use std::path::{Path, PathBuf};

use mantis_adapter_contract::core_types::ContentHash;

use crate::tunables::Tunables;
use crate::world;

/// The content a process loaded, and where from.
#[derive(Clone, Debug)]
pub struct Loaded {
    /// The tunables, with [`Tunables::content`] the cook's hash.
    pub tunables: Tunables,
    /// The cooked package it came from.
    pub cooked: PathBuf,
}

/// Loads the tunables with the content hash of the cooked package in
/// `cooked` (default [`world::COOKED_PATH`]), verified with `key` (default
/// the development key that cook wrote), and makes its graphs the cells'.
/// Call once per process, before the first cell is built.
///
/// # Errors
/// Why the cook cannot be trusted (the subcommand must not run), or that
/// cells already loaded other content.
pub fn load(cooked: Option<&Path>, key: Option<&Path>) -> Result<Loaded, String> {
    let dir = cooked.map_or_else(|| PathBuf::from(world::COOKED_PATH), Path::to_path_buf);
    let mut t = Tunables::defaults().map_err(|e| e.to_string())?;
    t.content = world::cooked_content(&dir, key)
        .map_err(|e| format!("{e} (cook the package first: cargo run -p mantis-cook -- packages/toy)"))?;
    world::use_gameplay(&dir, key)?;
    Ok(Loaded {
        tunables: t,
        cooked: dir,
    })
}

fn short(h: ContentHash) -> String {
    h.as_bytes().iter().take(8).fold(String::new(), |mut s, b| {
        use std::fmt::Write as _;
        let _ = write!(s, "{b:02x}");
        s
    })
}

/// Checks that a log whose header names content `needed` was written with
/// the content `loaded` holds; otherwise explains which content was loaded
/// and which the log needs.
///
/// # Errors
/// The explanation.
pub fn check_log(loaded: &Loaded, needed: ContentHash) -> Result<(), String> {
    let have = loaded.tunables.content;
    if needed == have {
        return Ok(());
    }
    let what = if needed == world::uncooked_content() {
        " (the package manifest's hash: the log was written by a build that did not load the cook)".to_owned()
    } else {
        String::new()
    };
    Err(format!(
        "the log was written with content {}…{what}, but this replay loaded content {}… from the cook in {}; replay with the `--cooked DIR` (and `--key FILE`) of the run that wrote it",
        short(needed),
        short(have),
        loaded.cooked.display()
    ))
}

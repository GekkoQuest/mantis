//! The toy client with the editor module (`toy-client --editor`): the editor's tools work
//! on the toy package's content sources and the store the client streams.
//!
//! ```text
//! toy-client --editor [--content DIR] [--ops ADDR --ops-cert FILE --ops-token-file FILE [--ops-cell N]]
//! ```
//!
//! `F10` shows and hides the editor; `F11` toggles the UI preview. With `--ops`, the
//! inspector also reads the server cell through the Ops dashboard (read-only; the
//! certificate is the one `toy-server cluster --ops-cert-out` wrote).

use std::net::SocketAddr;
use std::path::{Path, PathBuf};

use mantis_client::inspect::InspectSlot;
use mantis_editor::module::EditorConfig;
pub use mantis_editor::ops::{OpsError, OpsInspector, SystemsView as OpsSystems};
use mantis_ui::FontLibrary;

/// The default content directory, relative to the workspace root.
pub const CONTENT_DIR: &str = "packages/toy/content";

/// The Ops dashboard the inspector reads, when given.
#[derive(Clone, Debug)]
pub struct OpsTarget {
    /// Dashboard address.
    pub addr: SocketAddr,
    /// The dashboard's certificate file (DER).
    pub cert: PathBuf,
    /// A file holding the operator token.
    pub token_file: PathBuf,
    /// The cell to inspect.
    pub cell: u64,
}

/// The editor's configuration for the toy package.
///
/// # Errors
/// A message naming an unreadable certificate or token file.
pub fn config(
    content: &Path,
    store: &Path,
    fonts: FontLibrary,
    sim: Option<InspectSlot>,
    ops: Option<&OpsTarget>,
) -> Result<EditorConfig, String> {
    let ops = match ops {
        Some(t) => {
            let cert = std::fs::read(&t.cert).map_err(|e| format!("{}: {e}", t.cert.display()))?;
            let token = std::fs::read_to_string(&t.token_file)
                .map_err(|e| format!("{}: {e}", t.token_file.display()))?;
            Some((
                OpsInspector::new(t.addr, &cert, token.trim()).map_err(|e| e.to_string())?,
                t.cell,
            ))
        }
        None => None,
    };
    Ok(EditorConfig {
        content: content.to_path_buf(),
        store: store.to_path_buf(),
        streaming: crate::world::streaming_config(),
        workers: 2,
        fonts,
        sim,
        ops,
        material: Some("materials/ground.material.toml".to_owned()),
        layout: Some((
            "ui/town/hud.layout".to_owned(),
            Some("ui/town/hud.theme".to_owned()),
        )),
    })
}

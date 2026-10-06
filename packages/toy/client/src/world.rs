//! The toy's cooked world: `packages/toy/content` cooked by `mantis-cook` into a signed
//! store, opened here and streamed by the client around the camera.
//!
//! ```text
//! cargo run -p mantis-cook -- packages/toy          # writes packages/toy/cooked
//! toy-client --world packages/toy/cooked ...
//! ```
//!
//! Every bundle is verified against the public key before anything is read: the store's
//! development key (`keys/dev.pub`, written by a development cook) unless a production
//! key is given. The toy world is four 32 m sectors around the origin, flat ground and a
//! few lightmapped crates each, baked with a noon and a dusk keyframe.

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use mantis_client::content_store::ContentStore;
use mantis_client::time::HostClock;
use mantis_client::world_stream::{HANDOFF_BUDGET, WorldIndex, WorldStreamer};
use mantis_client::world_view::StreamedWorld;
use mantis_core::content::ContentHash;
use mantis_formats::bundle::Domain;
use mantis_render::streaming::StreamingConfig;

/// The development public key file inside a store.
pub const DEV_PUBLIC_KEY: &str = "keys/dev.pub";

/// Streaming radii for the toy's small sectors.
pub fn streaming_config() -> StreamingConfig {
    StreamingConfig {
        load_radius: 48.0,
        unload_radius: 64.0,
        lookahead: 1.0,
        ..StreamingConfig::default()
    }
}

/// `given`, or the store's development key (`keys/dev.pub`) when `None`.
///
/// # Errors
/// A message naming the unreadable or malformed key file.
pub fn public_key(store_dir: &Path, given: Option<[u8; 32]>) -> Result<[u8; 32], String> {
    if let Some(k) = given {
        return Ok(k);
    }
    let path = store_dir.join(DEV_PUBLIC_KEY);
    let bytes = std::fs::read(&path).map_err(|e| format!("{}: {e}", path.display()))?;
    <[u8; 32]>::try_from(bytes.as_slice())
        .map_err(|_| format!("{}: a public key is 32 bytes", path.display()))
}

/// An opened world.
#[derive(Debug)]
pub struct OpenedWorld {
    /// The streamer, ready to attach to a view.
    pub streamer: WorldStreamer,
    /// The gameplay bundle hash (the handshake content hash).
    pub content_hash: ContentHash,
    /// Sectors in the world.
    pub sectors: usize,
}

/// Opens the store at `store_dir`, verifying every bundle against `public_key` (the
/// store's development key when `None`), and indexes the world.
///
/// # Errors
/// A message naming the missing key, refused bundle, or malformed sector.
pub fn open(store_dir: &Path, public_key: Option<[u8; 32]>, workers: usize) -> Result<OpenedWorld, String> {
    let key = self::public_key(store_dir, public_key)?;
    let store = ContentStore::open(store_dir);
    let gameplay = store.bundle(Domain::Gameplay, &key).map_err(|e| e.to_string())?;
    let presentation = store
        .bundle(Domain::Presentation, &key)
        .map_err(|e| e.to_string())?;
    let index = WorldIndex::build(&store, &gameplay, &presentation).map_err(|e| e.to_string())?;
    let sectors = index.sectors.len();
    let streamer =
        WorldStreamer::new(store, index, streaming_config(), workers).map_err(|e| e.to_string())?;
    Ok(OpenedWorld {
        streamer,
        content_hash: gameplay.hash(),
        sectors,
    })
}

/// The streamed world for a view, with the plan-17 hand-off budget.
pub fn streamed(streamer: WorldStreamer, clock: Arc<dyn HostClock>) -> StreamedWorld {
    StreamedWorld {
        streamer,
        clock,
        budget: HANDOFF_BUDGET,
        last: mantis_client::world_stream::WorldStats::default(),
    }
}

/// The hand-off budget, for reports.
pub const fn budget() -> Duration {
    HANDOFF_BUDGET
}

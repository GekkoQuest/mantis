//! The content hash both hosts agree on: the hash of the cooked, signed
//! gameplay bundle. The package is cooked per run into a temporary
//! directory; the server reads the hash the way `toy-server serve` does,
//! the client the way it opens its world, and they must match, stay
//! stable across cooks with different development keys, admit bots that
//! announce it, and refuse a tampered or wrongly signed bundle.

#![allow(clippy::unwrap_used, clippy::panic, clippy::indexing_slicing)]

use std::path::{Path, PathBuf};

use mantis_cook::package::{Signing, cook};
use mantis_server::bots::Profile;
use mantis_server::simnet::LinkConfig;
use toy_server::sim::{Side, Sim};
use toy_server::tunables::Tunables;
use toy_server::world;

struct TempDir(PathBuf);

impl TempDir {
    fn new(name: &str) -> Self {
        let p = std::env::temp_dir().join(format!("mantis-toy-hash-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&p);
        std::fs::create_dir_all(&p).unwrap();
        Self(p)
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn cooked(dir: &Path) -> PathBuf {
    let content = Path::new(env!("CARGO_MANIFEST_DIR")).join("../content");
    let out = dir.join("cooked");
    if let Err(errors) = cook(&content, &out, 1, &Signing::Development) {
        let text: Vec<String> = errors
            .iter()
            .map(|e| format!("{}:{}: {}", e.file, e.line, e.message))
            .collect();
        panic!("cook failed:\n{}", text.join("\n"));
    }
    out
}

#[test]
fn server_and_client_agree_on_the_cooked_gameplay_hash() {
    let (a, b) = (TempDir::new("a"), TempDir::new("b"));
    let (first, second) = (cooked(&a.0), cooked(&b.0));
    let server = world::cooked_content(&first, None).unwrap();
    let client = toy_client::world::open(&first, None, 1).unwrap().content_hash;
    assert_eq!(server, client);
    assert_ne!(server, world::uncooked_content());
    // A second cook signs with another development key; the hash is over
    // the signed content, not the signature.
    assert_eq!(world::cooked_content(&second, None).unwrap(), server);

    // Bots announcing the cooked hash are admitted by a server using it.
    let mut t = Tunables::defaults().unwrap();
    t.content = server;
    let mut sim = Sim::new(t, 3, |_| None).unwrap();
    sim.add_bot(Side::Native, Profile::Honest, LinkConfig::PERFECT)
        .unwrap();
    sim.add_bot(Side::Legacy, Profile::Honest, LinkConfig::PERFECT)
        .unwrap();
    for _ in 0..90 {
        sim.step().unwrap();
    }
    assert_eq!(sim.host.stats.joined, 2);
}

#[test]
fn a_tampered_or_wrongly_signed_bundle_is_refused() {
    let (a, b) = (TempDir::new("c"), TempDir::new("d"));
    let (first, second) = (cooked(&a.0), cooked(&b.0));
    // The other cook's key does not verify this cook's bundle.
    let other_key = second.join("keys").join("dev.pub");
    assert!(world::cooked_content(&first, Some(&other_key)).is_err());
    // One flipped byte in the bundle.
    let bundle = first.join("bundles").join("gameplay.bundle");
    let mut bytes = std::fs::read(&bundle).unwrap();
    let last = bytes.len() - 1;
    bytes[last] ^= 0x40;
    std::fs::write(&bundle, &bytes).unwrap();
    assert!(world::cooked_content(&first, None).is_err());
    // No cook at all.
    assert!(world::cooked_content(&a.0.join("missing"), None).is_err());
}

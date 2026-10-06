//! The cook pipeline: phases, cross references by source path, error collection with
//! file and line, determinism, and publishing to the store with signed bundles.

use std::sync::Arc;

use mantis_cook::importer::{CookError, Cooked, ImportContext, Importer, Source};
use mantis_cook::sign::SigningKey;
use mantis_cook::store::Store;
use mantis_cook::tree::ContentTree;
use mantis_cook::{Cook, importers};
use mantis_formats::bundle::{AssetKind, Domain};

type TestResult = Result<(), Box<dyn std::error::Error>>;

/// Phase 0: `*.leaf` files cook to their bytes uppercased.
struct Leaf;

impl Importer for Leaf {
    fn name(&self) -> &'static str {
        "test.leaf"
    }
    fn version(&self) -> u32 {
        1
    }
    fn phase(&self) -> u32 {
        0
    }
    fn accepts(&self, path: &str) -> bool {
        std::path::Path::new(path)
            .extension()
            .is_some_and(|e| e == "leaf")
    }
    fn import(&self, s: &Source<'_>, _ctx: &ImportContext<'_>) -> Result<Vec<Cooked>, CookError> {
        Ok(vec![Cooked {
            name: format!("{}.cooked", s.stem_path()),
            kind: AssetKind::Package(1000),
            domain: Domain::Presentation,
            bytes: s.bytes.to_ascii_uppercase(),
        }])
    }
}

/// Phase 10: `*.ref` files name a leaf on each line and cook to the leaves' hashes.
struct Refs;

impl Importer for Refs {
    fn name(&self) -> &'static str {
        "test.refs"
    }
    fn version(&self) -> u32 {
        1
    }
    fn phase(&self) -> u32 {
        10
    }
    fn accepts(&self, path: &str) -> bool {
        std::path::Path::new(path).extension().is_some_and(|e| e == "ref")
    }
    fn import(&self, s: &Source<'_>, ctx: &ImportContext<'_>) -> Result<Vec<Cooked>, CookError> {
        let mut bytes = Vec::new();
        for (n, line) in s.text()?.lines().enumerate() {
            let (hash, leaf) = ctx.resolve_bytes(line.trim(), AssetKind::Package(1000), s.path, n + 1)?;
            bytes.extend_from_slice(hash.as_bytes());
            bytes.extend_from_slice(leaf);
        }
        Ok(vec![Cooked {
            name: s.path.to_owned(),
            kind: AssetKind::Package(1001),
            domain: Domain::Gameplay,
            bytes,
        }])
    }
}

fn cook() -> Result<Cook, CookError> {
    let mut all = importers::builtin();
    all.push(Arc::new(Leaf));
    all.push(Arc::new(Refs));
    Cook::new(all)
}

fn tree() -> ContentTree {
    let mut t = ContentTree::new();
    t.insert("art/a.leaf", "alpha");
    t.insert("art/b.leaf", "beta");
    t.insert("lists/both.ref", "art/a.leaf\nart/b.leaf\n");
    t.insert(
        "tables/std.vendor.stock",
        "# vendor item price\n1 101 5\n1 102 12\n",
    );
    t.insert("README.md", "notes are skipped");
    t
}

#[test]
fn later_phases_resolve_earlier_outputs_and_bundles_split_by_domain() -> TestResult {
    let out = cook()?.run(&tree()).map_err(|e| format!("{e:?}"))?;
    let leaf = out.get("art/a.cooked").ok_or("leaf")?;
    assert_eq!(leaf.bytes, b"ALPHA");
    let list = out.get("lists/both.ref").ok_or("list")?;
    assert!(list.bytes.starts_with(leaf.hash.as_bytes()));
    let gameplay = out.bundle(Domain::Gameplay, 1);
    let names: Vec<_> = gameplay.entries.iter().map(|e| e.name.as_str()).collect();
    assert_eq!(names, ["lists/both.ref", "tables/std.vendor.stock"]);
    assert_eq!(out.bundle(Domain::Presentation, 1).entries.len(), 2);
    // Deterministic: the same sources cook to the same bundle hashes.
    let again = cook()?.run(&tree()).map_err(|e| format!("{e:?}"))?;
    assert_eq!(again.bundle(Domain::Gameplay, 1).hash(), gameplay.hash());
    Ok(())
}

#[test]
fn every_error_is_reported_with_its_location() -> TestResult {
    let mut t = tree();
    t.insert("lists/broken.ref", "art/a.leaf\nart/missing.leaf\n");
    t.insert("tables/bad", "1 2 3\n4 5\n");
    t.insert("loose.bin", [0u8, 1]);
    let errors = cook()?.run(&t).err().ok_or("expected errors")?;
    let shown: Vec<String> = errors.iter().map(ToString::to_string).collect();
    assert!(
        shown.iter().any(|e| e.starts_with("lists/broken.ref:2:")),
        "{shown:?}"
    );
    assert!(shown.iter().any(|e| e.starts_with("tables/bad:2:")), "{shown:?}");
    assert!(
        shown
            .iter()
            .any(|e| e.starts_with("loose.bin:") && e.contains("no importer")),
        "{shown:?}"
    );
    Ok(())
}

#[test]
fn a_reference_to_the_same_phase_is_refused() -> TestResult {
    let mut t = ContentTree::new();
    t.insert("lists/self.ref", "lists/other.ref\n");
    t.insert("lists/other.ref", "art/a.leaf\n");
    t.insert("art/a.leaf", "x");
    let errors = cook()?.run(&t).err().ok_or("expected an error")?;
    assert!(errors.iter().any(|e| e.file == "lists/self.ref" && e.line == 1));
    Ok(())
}

#[test]
fn publishing_stores_payloads_and_signed_bundles() -> TestResult {
    let out = cook()?.run(&tree()).map_err(|e| format!("{e:?}"))?;
    let dir = std::env::temp_dir().join(format!("mantis-cook-store-{}", std::process::id()));
    let store = Store::new(&dir);
    let key = SigningKey::generate()?;
    let hashes = store.publish(&out, 4, &key)?;
    let gameplay = store.load_bundle(Domain::Gameplay, &key.public_key())?;
    assert_eq!(gameplay.hash(), hashes.first().copied().ok_or("hash")?);
    let entry = gameplay.get("tables/std.vendor.stock").ok_or("table")?;
    assert_eq!(
        store.get(&entry.hash)?,
        b"# vendor item price\n1 101 5\n1 102 12\n"
    );
    // Another key, or a corrupted payload, is refused.
    let other = SigningKey::generate()?;
    assert!(store.load_bundle(Domain::Gameplay, &other.public_key()).is_err());
    let object = dir
        .join("objects")
        .join(entry.hash.to_string().get(..2).ok_or("shard")?)
        .join(entry.hash.to_string());
    std::fs::write(&object, b"tampered")?;
    assert!(store.load_bundle(Domain::Gameplay, &key.public_key()).is_err());
    let _ = std::fs::remove_dir_all(&dir);
    Ok(())
}

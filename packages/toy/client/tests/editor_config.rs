//! `toy-client --editor`: the editor opens on the toy package's content, and a missing
//! Ops certificate is refused by name.

use std::path::Path;

use mantis_editor::module::{Editor, Tab};
use toy_client::editor::{OpsTarget, config};
use toy_client::town::fixture_fonts;

type TestResult = Result<(), Box<dyn std::error::Error>>;

fn content() -> std::path::PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../content")
}

#[test]
fn the_editor_opens_on_the_toy_content() -> TestResult {
    let store = Path::new(env!("CARGO_MANIFEST_DIR")).join("../cooked");
    let editor = Editor::new(config(&content(), &store, fixture_fonts()?, None, None)?)?;
    assert_eq!(editor.tab(), Tab::Inspect);
    assert!(editor.visible());
    assert_eq!(editor.world().placements().len(), 10);
    Ok(())
}

#[test]
fn a_missing_ops_certificate_is_named() -> TestResult {
    let target = OpsTarget {
        addr: "127.0.0.1:7480".parse()?,
        cert: "no-such-ops-cert.der".into(),
        token_file: "no-such-token.txt".into(),
        cell: 1,
    };
    let err = config(
        &content(),
        Path::new("cooked"),
        fixture_fonts()?,
        None,
        Some(&target),
    )
    .err()
    .ok_or("expected a refusal")?;
    assert!(err.contains("no-such-ops-cert.der"), "{err}");
    Ok(())
}

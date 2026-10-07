//! The soak cross-architecture CI job's exact invocations, run locally with
//! fewer ticks: `toy-server soak --out DIR --ticks N --bots 64 --seed 1`,
//! then `toy-server replay DIR/cell-1.log DIR/cell-2.log` with default
//! arguments (crates/testkit/ci/soak-cross-arch.md). Every subcommand
//! resolves content the same way, so a soak's own logs replay; a log
//! written with other content is refused naming both hashes.

#![expect(clippy::unwrap_used)]

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

const TICKS: &str = "300";

fn toy(args: &[&str], paths: &[&Path]) -> Output {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_toy-server"));
    cmd.args(args);
    for p in paths {
        cmd.arg(p);
    }
    // As CI runs it: from the repository root.
    cmd.current_dir(Path::new(env!("CARGO_MANIFEST_DIR")).join("../../.."));
    cmd.output().unwrap()
}

fn text(out: &Output) -> String {
    format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    )
}

fn soak(dir: &Path) {
    let out = toy(
        &["soak", "--ticks", TICKS, "--bots", "64", "--seed", "1", "--out"],
        &[dir],
    );
    assert!(out.status.success(), "soak failed: {}", text(&out));
}

fn scratch(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("mantis-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    dir
}

#[test]
fn soak_then_replay_with_default_arguments_round_trips() {
    let (a, b) = (scratch("soak-a"), scratch("soak-b"));
    soak(&a);
    soak(&b);
    let hashes = |d: &Path| std::fs::read_to_string(d.join("hashes.txt")).unwrap();
    let trace = hashes(&a);
    assert_eq!(trace.lines().count(), 2 * 300, "one line per cell per tick");
    assert_eq!(trace, hashes(&b), "two soaks with the same seed are identical");

    let logs = [a.join("cell-1.log"), a.join("cell-2.log")];
    let out = toy(&["replay"], &[&logs[0], &logs[1]]);
    let said = text(&out);
    assert!(out.status.success(), "replay refused the soak's own logs: {said}");
    assert_eq!(said.matches("with no divergence").count(), 2, "{said}");

    // A log written with other content (the manifest hash) is refused with
    // both hashes explained.
    let mut bytes = std::fs::read(&logs[0]).unwrap();
    let header_len = mantis_core::log::HEADER_LEN;
    assert!(bytes.len() > header_len);
    let c = mantis_core::log::LogReader::<mantis_server::intent::CellLogSchema>::open(
        &bytes,
        toy_server::build_id(),
        toy_server::content::load(None, None).unwrap().tunables.content,
    )
    .map(|r| r.header().content)
    .unwrap();
    // Rewrite the header's content field and its checksum.
    let at = 8 + 2 + 32;
    let manifest = toy_server::world::uncooked_content();
    assert_ne!(c, manifest);
    bytes[at..at + 32].copy_from_slice(manifest.as_bytes());
    let sum = mantis_core::hash::StableHasher::hash_bytes(&bytes[..header_len - 8]);
    bytes[header_len - 8..header_len].copy_from_slice(&sum.to_le_bytes());
    let other = a.join("other-content.log");
    std::fs::write(&other, &bytes).unwrap();
    let out = toy(&["replay"], &[&other]);
    let said = text(&out);
    assert!(!out.status.success());
    assert!(
        said.contains("the log was written with content") && said.contains("the package manifest's hash"),
        "{said}"
    );
    let _ = std::fs::remove_dir_all(&a);
    let _ = std::fs::remove_dir_all(&b);
}

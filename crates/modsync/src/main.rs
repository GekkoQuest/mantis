//! `mantis-modsync <workspace root> [package...] [--check]`
//!
//! Regenerates the module wiring of every host crate (`server`, `client`)
//! whose `Cargo.toml` carries the markers, in the named packages (all
//! packages when none is named). With
//! `--check`, writes nothing and fails when a file is out of date.

#![forbid(unsafe_code)]

use std::path::PathBuf;
use std::process::ExitCode;

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let check = args.iter().any(|a| a == "--check");
    let mut positional = args.iter().filter(|a| !a.starts_with("--"));
    let Some(root) = positional.next().map(PathBuf::from) else {
        eprintln!("usage: mantis-modsync <workspace root> [package...] [--check]");
        return ExitCode::FAILURE;
    };
    let named: Vec<String> = positional.cloned().collect();
    let hosts: Vec<(String, mantis_modsync::Side)> = mantis_modsync::hosts(&root)
        .into_iter()
        .filter(|(p, _)| named.is_empty() || named.contains(p))
        .collect();
    let mut failed = false;
    for (p, side) in &hosts {
        let generated = match mantis_modsync::generate_side(&root, p, *side) {
            Ok(g) => g,
            Err(e) => {
                eprintln!("mantis-modsync: {e}");
                failed = true;
                continue;
            }
        };
        let stale = mantis_modsync::stale(&generated);
        if check {
            for path in &stale {
                eprintln!("mantis-modsync: {} is out of date", path.display());
                failed = true;
            }
        } else if !stale.is_empty() {
            if let Err(e) = mantis_modsync::write(&generated) {
                eprintln!("mantis-modsync: {e}");
                failed = true;
            } else {
                println!(
                    "mantis-modsync: {p} {}: wrote {} file(s)",
                    side.dir(),
                    stale.len()
                );
            }
        }
    }
    if failed {
        ExitCode::FAILURE
    } else {
        ExitCode::SUCCESS
    }
}

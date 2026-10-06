//! `mantis-cook`: cooks a package's content into its signed, content-addressed store.
//!
//! ```text
//! cargo run -p mantis-cook -- <package dir> [--content-version N] [--key <pkcs8 file>] [--out <dir>]
//! ```
//!
//! Without `--key` the bundles are signed with a key generated for this run and its
//! public key is written to `<out>/keys/dev.pub`. `--out` defaults to `<package>/cooked`.
//! Errors print as `file:line: message`, one per line, and the exit code is 1.

use std::path::PathBuf;
use std::process::ExitCode;

use mantis_cook::package::{Signing, cook, layout};

fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    bytes.iter().fold(String::new(), |mut s, b| {
        let _ = write!(s, "{b:02x}");
        s
    })
}

struct Args {
    package: PathBuf,
    version: u32,
    signing: Signing,
    out: Option<PathBuf>,
}

fn parse(mut it: impl Iterator<Item = String>) -> Result<Args, String> {
    let mut package = None;
    let mut version = 1;
    let mut signing = Signing::Development;
    let mut out = None;
    while let Some(a) = it.next() {
        match a.as_str() {
            "--content-version" => {
                version = it
                    .next()
                    .and_then(|v| v.parse().ok())
                    .ok_or("--content-version needs a number")?;
            }
            "--key" => signing = Signing::Production(it.next().ok_or("--key needs a path")?.into()),
            "--out" => out = Some(PathBuf::from(it.next().ok_or("--out needs a path")?)),
            _ if package.is_none() && !a.starts_with("--") => package = Some(PathBuf::from(a)),
            _ => return Err(format!("unexpected argument `{a}`")),
        }
    }
    Ok(Args {
        package: package
            .ok_or("usage: mantis-cook <package dir> [--content-version N] [--key <pkcs8>] [--out <dir>]")?,
        version,
        signing,
        out,
    })
}

fn main() -> ExitCode {
    let args = match parse(std::env::args().skip(1)) {
        Ok(a) => a,
        Err(e) => {
            eprintln!("{e}");
            return ExitCode::from(2);
        }
    };
    let (content, default_out) = layout(&args.package);
    let out = args.out.unwrap_or(default_out);
    match cook(&content, &out, args.version, &args.signing) {
        Ok(c) => {
            let [gameplay, server, presentation] = c.hashes;
            println!("cooked {} assets into {}", c.assets, out.display());
            println!("gameplay bundle (content hash): {}", hex(gameplay.as_bytes()));
            println!("server bundle: {}", hex(server.as_bytes()));
            println!("presentation bundle: {}", hex(presentation.as_bytes()));
            println!("public key: {}", hex(&c.public_key));
            ExitCode::SUCCESS
        }
        Err(errors) => {
            for e in &errors {
                eprintln!("{}:{}: {}", e.file, e.line, e.message);
            }
            ExitCode::from(1)
        }
    }
}

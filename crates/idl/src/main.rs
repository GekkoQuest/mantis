//! `mantis-idl <dir>/<name>.idl <out.rs>`: regenerates one schema unit. Reads
//! `<name>.registry` and `<name>.lock` beside the schema.

use std::path::Path;
use std::process::ExitCode;

fn run() -> Result<(), String> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let [schema_path, out_path] = args.as_slice() else {
        return Err("usage: mantis-idl <schema.idl> <out.rs>".to_owned());
    };
    let schema_path = Path::new(schema_path);
    let read = |ext: &str| {
        let p = schema_path.with_extension(ext);
        std::fs::read_to_string(&p).map_err(|e| format!("{}: {e}", p.display()))
    };
    let unit = mantis_idl::Unit {
        schema: read("idl")?,
        registry: read("registry")?,
        lock: read("lock")?,
    };
    let origin = schema_path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let code = mantis_idl::compile(&unit, &origin).map_err(|e| format!("{}: {e}", schema_path.display()))?;
    std::fs::write(out_path, code).map_err(|e| format!("{out_path}: {e}"))
}

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("{e}");
            ExitCode::FAILURE
        }
    }
}

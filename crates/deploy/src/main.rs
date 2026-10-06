//! `mantisd`: one service role per process (see `mantis_deploy::cli`).

#![forbid(unsafe_code)]

fn main() -> std::process::ExitCode {
    mantis_deploy::cli::main(std::env::args().skip(1).collect(), None)
}

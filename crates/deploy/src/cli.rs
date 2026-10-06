//! The command line of `mantisd`, shared with a package's server binary
//! (`<binary> node ...`), which passes its [`CellHost`].
//!
//! ```text
//! mantisd <role> --config FILE [--drain-on-stdin-eof]
//!         role: account | realm | social | matchmaking | persist | ops | cell-host
//! mantisd local --config FILE [--drain-on-stdin-eof]
//! mantisd keys --out DIR --cluster NAME [--ca-days N]
//! mantisd certs --keys DIR --registry REGISTRY.toml [--out DIR] [--days N] [--instance NAME]
//! mantisd registry sign --key DEPLOY.pk8 --in BODY.toml --out REGISTRY.toml
//! mantisd registry verify --deploy-key DEPLOY.pub REGISTRY.toml
//! mantisd graph --config FILE
//! mantisd probe IP:PORT PATH
//! ```
//!
//! `probe` asks a health endpoint for `PATH` (`/live`, `/ready`) and exits
//! 0 on a 200: container health checks use it, so images need no HTTP
//! client of their own.
//!
//! Exit status: 0 after a clean drain or a successful tool command, 1 when
//! the node is refused or fails, 2 on a usage error.

use std::path::{Path, PathBuf};
use std::process::ExitCode;

use mantis_services::host::Role;

use crate::cell::{CellHost, CellNode};
use crate::config::NodeConfig;
use crate::keys;
use crate::local::{self, LocalConfig};
use crate::matrix;
use crate::node::{Node, verified_registry};
use crate::registry::{Registry, sign};

const USAGE: &str = "usage:
  <program> <role> --config FILE [--drain-on-stdin-eof]
      role: account, realm, social, matchmaking, persist, ops, cell-host
  <program> local --config FILE [--drain-on-stdin-eof]
  <program> keys --out DIR --cluster NAME [--ca-days N]
  <program> certs --keys DIR --registry REGISTRY.toml [--out DIR] [--days N] [--instance NAME]
  <program> registry sign --key DEPLOY.pk8 --in BODY.toml --out REGISTRY.toml
  <program> registry verify --deploy-key DEPLOY.pub REGISTRY.toml
  <program> graph --config FILE
  <program> probe IP:PORT PATH";

enum Failure {
    Usage(String),
    Refused(String),
}

impl From<String> for Failure {
    fn from(e: String) -> Self {
        Self::Refused(e)
    }
}

struct Args {
    words: Vec<String>,
}

impl Args {
    fn value(&self, flag: &str) -> Result<Option<PathBuf>, Failure> {
        match self.words.iter().position(|a| a == flag) {
            None => Ok(None),
            Some(at) => match self.words.get(at + 1) {
                Some(v) if !v.starts_with("--") => Ok(Some(PathBuf::from(v))),
                _ => Err(Failure::Usage(format!("{flag} needs a value"))),
            },
        }
    }

    fn need(&self, flag: &str) -> Result<PathBuf, Failure> {
        self.value(flag)?
            .ok_or_else(|| Failure::Usage(format!("{flag} is required")))
    }

    fn days(&self, flag: &str, default: u32) -> Result<u32, Failure> {
        match self.value(flag)? {
            None => Ok(default),
            Some(v) => v
                .to_str()
                .and_then(|s| s.parse::<u32>().ok())
                .filter(|d| (1..=3650).contains(d))
                .ok_or_else(|| Failure::Usage(format!("{flag}: 1 to 3650 days"))),
        }
    }

    fn text(&self, flag: &str) -> Result<String, Failure> {
        self.need(flag)?
            .to_str()
            .map(str::to_owned)
            .ok_or_else(|| Failure::Usage(format!("{flag}: not text")))
    }

    fn flag(&self, flag: &str) -> bool {
        self.words.iter().any(|a| a == flag)
    }

    /// Refuses any word that is not one of `flags` (with its value when
    /// `valued`) or one of `positional` positional words.
    fn only(&self, flags: &[(&str, bool)], positional: usize) -> Result<Vec<&str>, Failure> {
        let mut rest = Vec::new();
        let mut words = self.words.iter();
        while let Some(w) = words.next() {
            match flags.iter().find(|(f, _)| f == w) {
                Some((_, true)) => {
                    let _ = words.next();
                }
                Some((_, false)) => {}
                None if w.starts_with("--") => return Err(Failure::Usage(format!("unknown option {w}"))),
                None => rest.push(w.as_str()),
            }
        }
        if rest.len() == positional {
            Ok(rest)
        } else {
            Err(Failure::Usage(format!(
                "unexpected arguments: {}",
                rest.join(" ")
            )))
        }
    }
}

/// Runs the command line `args` (without the program name). `cells` is the
/// package's cell host, or `None` in `mantisd` itself.
#[must_use]
pub fn main(args: Vec<String>, cells: Option<&dyn CellHost>) -> ExitCode {
    let program = cells.map_or("mantisd", CellHost::command);
    match dispatch(args, cells) {
        Ok(()) => ExitCode::SUCCESS,
        Err(Failure::Usage(e)) => {
            eprintln!("{program}: {e}\n{}", USAGE.replace("<program>", program));
            ExitCode::from(2)
        }
        Err(Failure::Refused(e)) => {
            eprintln!("{program}: {e}");
            ExitCode::FAILURE
        }
    }
}

fn dispatch(all: Vec<String>, cells: Option<&dyn CellHost>) -> Result<(), Failure> {
    let mut words = all.into_iter();
    let command = words.next().unwrap_or_default();
    let args = Args {
        words: words.collect(),
    };
    match command.as_str() {
        "local" => {
            args.only(&[("--config", true), ("--drain-on-stdin-eof", false)], 0)?;
            let config = LocalConfig::load(&args.need("--config")?)?;
            Ok(local::run(&config, args.flag("--drain-on-stdin-eof"))?)
        }
        "keys" => {
            args.only(&[("--out", true), ("--cluster", true), ("--ca-days", true)], 0)?;
            let dir = args.need("--out")?;
            let cluster = args.text("--cluster")?;
            keys::new_keys(&dir, &cluster, args.days("--ca-days", crate::pki::CA_DAYS)?)?;
            println!(
                "keys written to {}: {}",
                dir.display(),
                keys::files::ALL.join(", ")
            );
            Ok(())
        }
        "certs" => certs(&args),
        "registry" => registry(&args),
        "probe" => {
            let words = args.only(&[], 2)?;
            let (addr, path) = match words.as_slice() {
                [a, p] => (*a, *p),
                _ => return Err(Failure::Usage("probe IP:PORT PATH".to_owned())),
            };
            let addr: std::net::SocketAddr = addr
                .parse()
                .map_err(|_| Failure::Usage(format!("{addr:?} is not ip:port")))?;
            match crate::health::probe_blocking(addr, path, std::time::Duration::from_secs(3))? {
                (200, body) => {
                    print!("{body}");
                    Ok(())
                }
                (code, body) => Err(Failure::Refused(format!("{code} {}", body.trim_end()))),
            }
        }
        "graph" => {
            args.only(&[("--config", true)], 0)?;
            let config = NodeConfig::load(&args.need("--config")?)?;
            let registry = verified_registry(&config)?;
            print!("{}", registry.summary());
            println!("{} ({}):", matrix::name(config.role), config.instance);
            print!("{}", matrix::graph(config.role));
            Ok(())
        }
        role => match matrix::parse(role) {
            Some(r) => {
                args.only(&[("--config", true), ("--drain-on-stdin-eof", false)], 0)?;
                node(
                    r,
                    &args.need("--config")?,
                    args.flag("--drain-on-stdin-eof"),
                    cells,
                )
            }
            None if role.is_empty() => Err(Failure::Usage("no command".to_owned())),
            None => Err(Failure::Usage(format!("unknown command or role {role:?}"))),
        },
    }
}

fn certs(args: &Args) -> Result<(), Failure> {
    args.only(
        &[
            ("--keys", true),
            ("--registry", true),
            ("--out", true),
            ("--days", true),
            ("--instance", true),
        ],
        0,
    )?;
    let dir = args.need("--keys")?;
    let path = args.need("--registry")?;
    let deploy = keys::read_public_key(&dir.join(keys::files::DEPLOY_PUBLIC))?;
    let text = std::fs::read_to_string(&path).map_err(|e| format!("{}: {e}", path.display()))?;
    let registry = Registry::verify(&text, &deploy).map_err(|e| format!("{}: {e}", path.display()))?;
    let out = args.value("--out")?.unwrap_or_else(|| dir.clone());
    let days = args.days("--days", crate::pki::LEAF_DAYS)?;
    let only = match args.value("--instance")? {
        Some(_) => Some(args.text("--instance")?),
        None => None,
    };
    let validity = crate::pki::Validity::starting_now(std::time::SystemTime::now(), days);
    let issued =
        crate::pki::issue_registry(&keys::read_ca(&dir)?, &registry, &out, validity, only.as_deref())?;
    println!(
        "certificates for {} valid {days} days written to {}: {}",
        registry.cluster,
        out.display(),
        issued.join(", ")
    );
    Ok(())
}

fn registry(args: &Args) -> Result<(), Failure> {
    let sub = args.words.first().map(String::as_str);
    let rest = Args {
        words: args.words.iter().skip(1).cloned().collect(),
    };
    match sub {
        Some("sign") => {
            rest.only(&[("--key", true), ("--in", true), ("--out", true)], 0)?;
            let key = keys::read_key_pair(&rest.need("--key")?)?;
            let input = rest.need("--in")?;
            let body = std::fs::read_to_string(&input).map_err(|e| format!("{}: {e}", input.display()))?;
            // Refuse to sign what no node would accept.
            let parsed = Registry::parse_body(&body).map_err(|e| format!("{}: {e}", input.display()))?;
            let out = rest.need("--out")?;
            keys::write_secret(&out, sign(&body, &key).as_bytes())?;
            println!(
                "registry {} serial {} ({} instances) signed into {}",
                parsed.cluster,
                parsed.serial,
                parsed.instances.len(),
                out.display()
            );
            Ok(())
        }
        Some("verify") => {
            let files = rest.only(&[("--deploy-key", true)], 1)?;
            let deploy = keys::read_public_key(&rest.need("--deploy-key")?)?;
            let file = Path::new(files.first().copied().unwrap_or_default());
            let text = std::fs::read_to_string(file).map_err(|e| format!("{}: {e}", file.display()))?;
            let registry =
                Registry::verify(&text, &deploy).map_err(|e| format!("{}: {e}", file.display()))?;
            print!("{}", registry.summary());
            println!("signature verified");
            Ok(())
        }
        _ => Err(Failure::Usage("registry sign | registry verify".to_owned())),
    }
}

fn node(role: Role, config: &Path, stdin_eof: bool, cells: Option<&dyn CellHost>) -> Result<(), Failure> {
    let config = NodeConfig::load(config)?;
    if config.role != role {
        return Err(Failure::Refused(format!(
            "{} configures a {} node, not a {}",
            config.file.display(),
            matrix::name(config.role),
            matrix::name(role)
        )));
    }
    if role == Role::Cell && cells.is_none() {
        return Err(Failure::Refused(format!(
            "mantisd does not host cells: a cell host is package code (its world, modules and \
             adapters), so the package's server binary runs this role. Run \
             `<package server binary> node cell-host --config {}` (the toy package: \
             `toy-server node cell-host --config {}`).",
            config.file.display(),
            config.file.display()
        )));
    }
    let node = Node::start(config, stdin_eof)?;
    match (role, cells) {
        (Role::Cell, Some(host)) => {
            node.wait_for_dependencies()?;
            Ok(host.run(CellNode::new(node)?)?)
        }
        _ => Ok(crate::roles::run(&node)?),
    }
}

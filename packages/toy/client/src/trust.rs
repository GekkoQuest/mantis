//! Who the toy client trusts for the game listener, and how a refusal reads.
//!
//! - **Production:** `--ca-bundle FILE` (a PEM bundle: an operator-provided public CA, or
//!   the cluster CA for a private deployment, one or two certificates while a CA rotates)
//!   with `--server-name NAME`, defaulting to the host part of `--server`. The chain and
//!   the name are verified at every connect, so a reconnect after the server rotated its
//!   certificate verifies the new chain against the same bundle.
//! - **Public CA:** `--public-roots` trusts the Mozilla root set (`webpki-roots`), plus
//!   `--ca-bundle FILE` as extra roots when given, for a game port terminated with a
//!   publicly issued certificate. It needs the toy client built with its `public-roots`
//!   feature; without it the connect is refused with a notice saying so.
//! - **Development:** `--cert FILE`, the server's self-signed development leaf (DER),
//!   pinned exactly.
//!
//! A refused server is reported with its cause ([`notice`]): an unknown issuer, the wrong
//! name, an expired or not-yet-valid certificate.

use std::net::SocketAddr;
use std::path::{Path, PathBuf};

use mantis_net::NetError;
use mantis_net::quic::{ServerTrust, TrustFailure};

/// What the command line asked for.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum TrustArgs {
    /// `--cert`: pin exactly this self-signed development leaf.
    Pinned(PathBuf),
    /// `--ca-bundle` (with `--server-name`, or the host of the server address).
    Bundle {
        /// The PEM bundle.
        bundle: PathBuf,
        /// The name the server's certificate must carry (`None`: the host of the
        /// address).
        server_name: Option<String>,
    },
    /// `--public-roots`: the public root set, plus `--ca-bundle` as extra roots.
    Public {
        /// Extra roots (PEM), if any.
        extra: Option<PathBuf>,
        /// The name the server's certificate must carry (`None`: the host of the
        /// address).
        server_name: Option<String>,
    },
}

/// The trust flags as given.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct TrustFlags<'a> {
    /// `--public-roots`.
    pub public_roots: bool,
    /// `--ca-bundle`.
    pub ca_bundle: Option<&'a str>,
    /// `--server-name`.
    pub server_name: Option<&'a str>,
    /// `--cert`.
    pub cert: Option<&'a str>,
}

impl TrustArgs {
    /// From the flags: `--cert` excludes the others; `--public-roots` takes `--ca-bundle`
    /// as extra roots; neither means the development pin at `default_cert`.
    /// `--server-name` needs `--ca-bundle` or `--public-roots`.
    ///
    /// # Errors
    /// The conflicting or orphaned flag, in words.
    pub fn from_flags(flags: TrustFlags<'_>, default_cert: &str) -> Result<Self, String> {
        let TrustFlags {
            public_roots,
            ca_bundle,
            server_name,
            cert,
        } = flags;
        if public_roots {
            if cert.is_some() {
                return Err("--public-roots and --cert exclude each other".to_owned());
            }
            return Ok(Self::Public {
                extra: ca_bundle.map(PathBuf::from),
                server_name: server_name.map(str::to_owned),
            });
        }
        match (ca_bundle, cert) {
            (Some(_), Some(_)) => {
                Err("--ca-bundle and --cert exclude each other (--cert pins a development leaf)".to_owned())
            }
            (Some(bundle), None) => Ok(Self::Bundle {
                bundle: bundle.into(),
                server_name: server_name.map(str::to_owned),
            }),
            (None, cert) if server_name.is_none() => Ok(Self::Pinned(cert.unwrap_or(default_cert).into())),
            (None, _) => Err("--server-name needs --ca-bundle or --public-roots".to_owned()),
        }
    }

    /// Reads the files and builds the trust for a server at `addr`.
    ///
    /// # Errors
    /// A file that cannot be read, or a bundle with no certificate, in words.
    pub fn load(&self, addr: SocketAddr) -> Result<ServerTrust, String> {
        let read =
            |p: &Path, flag: &str| std::fs::read(p).map_err(|e| format!("{flag} {}: {e}", p.display()));
        match self {
            Self::Pinned(path) => Ok(ServerTrust::Pinned(read(path, "--cert")?)),
            Self::Bundle { bundle, server_name } => {
                let pem = read(bundle, "--ca-bundle")?;
                let name = server_name.clone().unwrap_or_else(|| addr.ip().to_string());
                ServerTrust::from_pem_bundle(&pem, &name)
                    .map_err(|e| format!("--ca-bundle {}: {e}", bundle.display()))
            }
            Self::Public { extra, server_name } => {
                let pem = match extra {
                    Some(p) => read(p, "--ca-bundle")?,
                    None => Vec::new(),
                };
                let name = server_name.clone().unwrap_or_else(|| addr.ip().to_string());
                ServerTrust::public_with_pem(&pem, &name).map_err(|e| format!("--public-roots: {e}"))
            }
        }
    }
}

/// The notice for a connection failure: a trust refusal says what was wrong with the
/// server's certificate and what to check.
pub fn notice(error: &NetError, trust: &ServerTrust) -> String {
    let NetError::Untrusted(why) = error else {
        if matches!(trust, ServerTrust::Public { .. }) && matches!(error, NetError::Tls(_)) {
            return format!(
                "could not connect: {error} (build the toy client with `--features public-roots` \
                 to trust public CAs, or pass the CA with --ca-bundle instead of --public-roots)"
            );
        }
        return format!("could not connect: {error}");
    };
    let name = match trust {
        ServerTrust::Roots { server_name, .. } | ServerTrust::Public { server_name, .. } => {
            server_name.as_str()
        }
        ServerTrust::Pinned(_) => "the pinned development certificate",
        _ => "the server name",
    };
    match why {
        TrustFailure::UnknownIssuer => match trust {
            ServerTrust::Pinned(_) => "refused the server: its certificate is not the pinned \
                 development certificate (--cert); use --ca-bundle for a CA-issued one"
                .to_owned(),
            ServerTrust::Roots { .. } => "refused the server: its certificate is not signed \
                 by a CA in --ca-bundle (an untrusted chain)"
                .to_owned(),
            ServerTrust::Public { .. } => "refused the server: its certificate is not signed \
                 by a public CA or a CA in --ca-bundle (an untrusted chain)"
                .to_owned(),
            _ => "refused the server: its certificate is not signed by a trusted CA".to_owned(),
        },
        TrustFailure::WrongName { expected } => format!(
            "refused the server: its certificate is not valid for `{expected}` (check --server \
             and --server-name; expected {name})"
        ),
        TrustFailure::Expired => "refused the server: its certificate has expired".to_owned(),
        TrustFailure::NotYetValid => {
            "refused the server: its certificate is not valid yet (check this machine's clock)".to_owned()
        }
        TrustFailure::Other(why) => format!("refused the server's certificate: {why}"),
        _ => format!("refused the server's certificate: {why}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn flags_choose_one_trust() {
        let flags = |public_roots, ca_bundle, server_name, cert| TrustFlags {
            public_roots,
            ca_bundle,
            server_name,
            cert,
        };
        let pick = |f| TrustArgs::from_flags(f, "dev.der");
        assert_eq!(
            pick(flags(false, None, None, None)),
            Ok(TrustArgs::Pinned("dev.der".into()))
        );
        assert_eq!(
            pick(flags(false, Some("ca.pem"), Some("game.test"), None)),
            Ok(TrustArgs::Bundle {
                bundle: "ca.pem".into(),
                server_name: Some("game.test".to_owned())
            })
        );
        assert_eq!(
            pick(flags(true, Some("extra.pem"), None, None)),
            Ok(TrustArgs::Public {
                extra: Some("extra.pem".into()),
                server_name: None
            })
        );
        assert!(pick(flags(false, Some("ca.pem"), None, Some("leaf.der"))).is_err());
        assert!(pick(flags(true, None, None, Some("leaf.der"))).is_err());
        assert!(pick(flags(false, None, Some("game.test"), None)).is_err());
    }

    #[test]
    fn notices_name_the_cause() {
        let public = ServerTrust::Public {
            extra: Vec::new(),
            server_name: "game.test".to_owned(),
        };
        let unbuilt = NetError::Tls("built without the public-roots feature".to_owned());
        assert!(notice(&unbuilt, &public).contains("--features public-roots"));
        let stranger = NetError::Untrusted(TrustFailure::UnknownIssuer);
        assert!(notice(&stranger, &public).contains("not signed by a public CA"));
        let wrong = NetError::Untrusted(TrustFailure::WrongName {
            expected: "game.test".to_owned(),
        });
        assert!(notice(&wrong, &public).contains("not valid for `game.test`"));
    }
}

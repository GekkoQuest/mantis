//! Versioned handshake with capability negotiation and the content bundle
//! hash (plan 9). The client's module list is checked against the permitted
//! list for logging and refusal, never as a security boundary (plan 12).

use std::collections::BTreeSet;

use mantis_adapter_contract::core_types::ContentHash;
use mantis_adapter_contract::{Hello, PROTOCOL_VERSION, RefuseReason};

/// What the server accepts.
#[derive(Clone, Debug)]
pub struct ServerPolicy {
    /// Protocol version required.
    pub protocol: u16,
    /// Capability bits the server supports.
    pub capabilities: u32,
    /// The server's gameplay content hash.
    pub content: ContentHash,
    /// Module keys clients may run. Empty means none are permitted.
    pub permitted_modules: BTreeSet<String>,
    /// Refuse everyone with `Maintenance`.
    pub maintenance: bool,
}

impl ServerPolicy {
    /// A policy for this build's protocol version.
    #[must_use]
    pub fn new(content: ContentHash, capabilities: u32) -> Self {
        Self {
            protocol: PROTOCOL_VERSION,
            capabilities,
            content,
            permitted_modules: BTreeSet::new(),
            maintenance: false,
        }
    }
}

/// The negotiated session parameters.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Accepted {
    /// Protocol version in use.
    pub protocol: u16,
    /// Capability bits in use: the intersection of both sides.
    pub capabilities: u32,
}

/// Decides a `Hello`. Checks run in a fixed order, so a refusal always names
/// the first failing reason: maintenance, protocol version, content hash,
/// modules, token, capacity.
///
/// # Errors
/// The [`RefuseReason`] to send.
pub fn negotiate(
    hello: &Hello,
    policy: &ServerPolicy,
    token_ok: impl FnOnce(&[u8]) -> bool,
    full: bool,
) -> Result<Accepted, RefuseReason> {
    if policy.maintenance {
        return Err(RefuseReason::Maintenance);
    }
    if hello.protocol != policy.protocol {
        return Err(RefuseReason::VersionMismatch);
    }
    if hello.content != policy.content {
        return Err(RefuseReason::ContentMismatch);
    }
    if hello
        .modules
        .iter()
        .any(|m| !policy.permitted_modules.contains(m.name.as_str()))
    {
        return Err(RefuseReason::ModuleRefused);
    }
    let token: Vec<u8> = hello.token.iter().copied().collect();
    if !token_ok(&token) {
        return Err(RefuseReason::BadToken);
    }
    if full {
        return Err(RefuseReason::Full);
    }
    Ok(Accepted {
        protocol: policy.protocol,
        capabilities: hello.capabilities & policy.capabilities,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use mantis_adapter_contract::ModuleEntry;
    use mantis_adapter_contract::core_types::{BoundedArray, WireString};

    fn hello() -> Hello {
        let mut modules = BoundedArray::new();
        modules
            .push(ModuleEntry {
                name: WireString::new("std.chat").unwrap(),
                hash: ContentHash::of(b"chat"),
            })
            .unwrap();
        Hello {
            protocol: PROTOCOL_VERSION,
            capabilities: 0b1011,
            content: ContentHash::of(b"content"),
            modules,
            token: BoundedArray::from_slice(b"tok").unwrap(),
        }
    }

    fn policy() -> ServerPolicy {
        let mut p = ServerPolicy::new(ContentHash::of(b"content"), 0b0110);
        p.permitted_modules.insert("std.chat".to_owned());
        p
    }

    #[test]
    fn accepts_and_intersects_capabilities() {
        assert_eq!(
            negotiate(&hello(), &policy(), |t| t == b"tok", false),
            Ok(Accepted {
                protocol: PROTOCOL_VERSION,
                capabilities: 0b0010
            })
        );
    }

    #[test]
    fn refusals_in_order() {
        let mut p = policy();
        p.maintenance = true;
        assert_eq!(
            negotiate(&hello(), &p, |_| true, true),
            Err(RefuseReason::Maintenance)
        );
        let mut h = hello();
        h.protocol += 1;
        assert_eq!(
            negotiate(&h, &policy(), |_| true, false),
            Err(RefuseReason::VersionMismatch)
        );
        let mut h = hello();
        h.content = ContentHash::of(b"other");
        assert_eq!(
            negotiate(&h, &policy(), |_| true, false),
            Err(RefuseReason::ContentMismatch)
        );
        let mut p = policy();
        p.permitted_modules.clear();
        assert_eq!(
            negotiate(&hello(), &p, |_| true, false),
            Err(RefuseReason::ModuleRefused)
        );
        assert_eq!(
            negotiate(&hello(), &policy(), |_| false, false),
            Err(RefuseReason::BadToken)
        );
        assert_eq!(
            negotiate(&hello(), &policy(), |_| true, true),
            Err(RefuseReason::Full)
        );
    }
}

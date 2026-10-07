//! The deployable roles, and the caller matrix applied per process.
//!
//! Each RPC server already refuses a caller its method does not list
//! (`mantis_services::methods`). A process adds the other half: it dials
//! only the roles its own role may call, so the registry addresses a node
//! is handed are exactly its role's row of the matrix, and a node whose
//! role would need more refuses to start.

use mantis_services::host::Role;
use mantis_services::methods;

/// The roles `mantisd` deploys, by their names in configuration and the
/// registry: the service roles, the gateway (the game's front door), and
/// the cell host (run by a package's binary).
pub const DEPLOYED: [Role; 8] = [
    Role::Account,
    Role::Realm,
    Role::Social,
    Role::Matchmaking,
    Role::Persist,
    Role::Ops,
    Role::Gateway,
    Role::Cell,
];

/// A role's name in configuration and the registry (`cell-host` for the
/// cell role).
#[must_use]
pub const fn name(role: Role) -> &'static str {
    match role {
        Role::Cell => "cell-host",
        other => other.name(),
    }
}

/// The role with that configuration name (deployable roles only).
#[must_use]
pub fn parse(name_text: &str) -> Option<Role> {
    DEPLOYED.into_iter().find(|r| name(*r) == name_text)
}

/// True when some method `server` serves lists `caller` among its callers.
#[must_use]
pub fn may_call(caller: Role, server: Role) -> bool {
    methods::matrix()
        .into_iter()
        .any(|(_, id, callers)| methods::server_of(id) == Some(server) && callers.contains(&caller))
}

/// The roles a process of `role` dials, which its readiness waits for.
///
/// Ops dials cell hosts too, but never waits for them: cell hosts come and
/// go, and they wait for Ops (live changes), so waiting would be a cycle.
#[must_use]
pub const fn dependencies(role: Role) -> &'static [Role] {
    match role {
        Role::Persist => &[],
        // The gateway checks entry tokens with the realm (`RouteEntry`);
        // it reaches cell hosts on their game listeners, never over RPC.
        Role::Gateway => &[Role::Realm],
        // Accounts, characters, guilds and friends are rows the writer
        // keeps; each of these roles reads them back before it serves.
        Role::Account | Role::Realm | Role::Social => &[Role::Persist],
        // Matchmaking holds its role lease through the writer (`Lease`).
        Role::Matchmaking => &[Role::Persist, Role::Realm],
        Role::Ops => &[Role::Account, Role::Persist],
        Role::Cell => &[
            Role::Realm,
            Role::Persist,
            Role::Social,
            Role::Matchmaking,
            Role::Ops,
        ],
    }
}

/// The roles a process of `role` may hold addresses of: its dependencies,
/// plus cell hosts for Ops.
#[must_use]
pub fn dials(role: Role) -> Vec<Role> {
    let mut out = dependencies(role).to_vec();
    if role == Role::Ops {
        out.push(Role::Cell);
    }
    out
}

/// Checks that every role a process of `role` dials is one the matrix
/// lets it call.
///
/// # Errors
/// The first role it would dial without a method to call there.
pub fn check(role: Role) -> Result<(), String> {
    match dials(role).into_iter().find(|to| !may_call(role, *to)) {
        Some(to) => Err(format!(
            "the caller matrix gives {} no method on {}",
            name(role),
            name(to)
        )),
        None => Ok(()),
    }
}

/// The service graph for one role: what it serves and to whom, and whom it
/// calls, as printed at start-up.
#[must_use]
pub fn graph(role: Role) -> String {
    use std::fmt::Write as _;
    let mut out = String::new();
    for (method, id, callers) in methods::matrix() {
        if methods::server_of(id) == Some(role) {
            let who: Vec<&str> = callers.iter().map(|r| name(*r)).collect();
            let _ = writeln!(out, "  serves {id:>3} {method}: {}", who.join(", "));
        }
    }
    for to in dials(role) {
        let _ = writeln!(out, "  calls  {}", name(to));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_method_is_served_by_a_deployed_role() {
        for (method, id, _) in methods::matrix() {
            let server = methods::server_of(id);
            assert!(
                server.is_some_and(|r| DEPLOYED.contains(&r)),
                "method {id} {method} is served by {server:?}, which mantisd does not deploy"
            );
        }
    }

    #[test]
    fn every_role_dials_only_what_the_matrix_allows() {
        for role in DEPLOYED {
            check(role).unwrap();
        }
        assert!(may_call(Role::Account, Role::Persist));
        assert!(may_call(Role::Realm, Role::Persist));
        assert!(!may_call(Role::Account, Role::Realm));
        assert!(may_call(Role::Social, Role::Persist));
        assert!(may_call(Role::Ops, Role::Cell));
        assert!(may_call(Role::Matchmaking, Role::Persist));
        assert!(may_call(Role::Gateway, Role::Realm));
        assert!(!may_call(Role::Gateway, Role::Persist));
    }

    #[test]
    fn names_round_trip() {
        for role in DEPLOYED {
            assert_eq!(parse(name(role)), Some(role));
        }
        assert_eq!(parse("gateway"), Some(Role::Gateway));
        assert_eq!(parse("cell"), None);
    }
}

//! Addresses and identities that change while roles run: an [`Endpoint`]
//! (`host:port`, resolved at every connect, moved by a registry reload) and
//! a [`TlsHandle`] (a renewed certificate or a changed CA list): clients
//! follow both at their next call, and a server closes the connections it
//! accepted under an identity it replaced.

use std::net::IpAddr;
use std::sync::Arc;
use std::time::Duration;

use mantis_services::account::AccountService;
use mantis_services::generated::services as m;
use mantis_services::host::Role;
use mantis_services::host::rpc::{Endpoint, Router, RpcClient, RpcError, RpcServer};
use mantis_services::methods;
use mantis_services::tls::TlsHandle;
use mantis_services::tls::dev::DevCa;

const KEY: &[u8] = b"cluster-key-for-endpoint-tests";
const T: Duration = Duration::from_secs(5);

fn local() -> IpAddr {
    IpAddr::from([127, 0, 0, 1])
}

/// A realm-epoch server answering `epoch`: which server answered.
fn answering(epoch: u64) -> Router {
    let mut r = Router::new();
    r.serve::<methods::RealmRun>(move |_, _| Ok(m::RealmEpoch { epoch }));
    r
}

async fn epoch(client: &RpcClient) -> Result<u64, RpcError> {
    client
        .call::<methods::RealmRun>(&m::PollRealm {}, T)
        .await
        .map(|e| e.epoch)
}

#[test]
fn endpoints_parse_host_and_port() {
    for good in [
        "127.0.0.1:7000",
        "[::1]:7000",
        "localhost:1",
        "realm-1.cluster.example:443",
    ] {
        assert!(Endpoint::new(good).is_ok(), "{good}");
    }
    for bad in [
        "",
        "localhost",
        "127.0.0.1",
        ":80",
        "host:port",
        "a b:1",
        "[::1]7000",
        "host:70000",
        "-..:1",
    ] {
        assert!(Endpoint::new(bad).is_err(), "{bad}");
    }
    let e = Endpoint::new("127.0.0.1:1").unwrap();
    assert_eq!(e.generation(), 0);
    assert!(!e.set("127.0.0.1:1").unwrap(), "the same target is no change");
    assert!(e.set("localhost:2").unwrap());
    assert_eq!((e.target().as_str(), e.generation()), ("localhost:2", 1));
    assert!(e.set("nonsense").is_err());
    let clone = e.clone();
    clone.set("localhost:3").unwrap();
    assert_eq!(e.target(), "localhost:3", "clones are the same endpoint");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_client_resolves_a_name_and_follows_a_moved_endpoint() {
    let a = RpcServer::bind("127.0.0.1:0".parse().unwrap(), KEY.to_vec(), answering(1))
        .await
        .unwrap();
    let b = RpcServer::bind("127.0.0.1:0".parse().unwrap(), KEY.to_vec(), answering(2))
        .await
        .unwrap();
    // A DNS name, resolved at connect (every address tried in order).
    let endpoint = Endpoint::new(&format!("localhost:{}", a.addr().port())).unwrap();
    let client =
        RpcClient::with_endpoint(endpoint.clone(), Role::Cell, KEY.to_vec(), None, Role::Realm).unwrap();
    assert_eq!(epoch(&client).await, Ok(1));
    assert_eq!(epoch(&client).await, Ok(1), "the connection is kept");
    // The registry moves the role: the next call goes to the new target.
    assert!(endpoint.set(&b.addr().to_string()).unwrap());
    assert_eq!(epoch(&client).await, Ok(2));
    assert_eq!(client.endpoint().target(), b.addr().to_string());
    // A name that does not resolve: disconnected, not hung.
    endpoint.set("no-such-host.invalid:1").unwrap();
    assert_eq!(epoch(&client).await, Err(RpcError::Disconnected));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_swapped_identity_closes_old_connections_and_clients_handshake_again() {
    let ca = DevCa::new("swap").unwrap();
    let server_tls = TlsHandle::new(ca.identity(Role::Realm, "realm-1", &[local()]).unwrap());
    let server = RpcServer::bind_tls(
        "127.0.0.1:0".parse().unwrap(),
        KEY.to_vec(),
        answering(7),
        Some(server_tls.clone()),
    )
    .await
    .unwrap();
    let client_tls = TlsHandle::new(ca.identity(Role::Cell, "cell-1", &[local()]).unwrap());
    let client = RpcClient::with_endpoint(
        Endpoint::fixed(server.addr()),
        Role::Cell,
        KEY.to_vec(),
        Some(client_tls.clone()),
        Role::Realm,
    )
    .unwrap();
    assert_eq!(epoch(&client).await, Ok(7));

    // A handle keeps who it is: another role or cluster is refused.
    assert!(
        server_tls
            .set(ca.identity(Role::Account, "account-1", &[local()]).unwrap())
            .is_err()
    );
    let elsewhere = DevCa::new("elsewhere").unwrap();
    assert!(
        server_tls
            .set(elsewhere.identity(Role::Realm, "realm-1", &[local()]).unwrap())
            .is_err()
    );
    assert_eq!(server_tls.generation(), 0);

    // Renewal: the server's certificate is reissued by the same CA. The
    // old connection closes; the client's next call handshakes again.
    server_tls
        .set(ca.identity(Role::Realm, "realm-1", &[local()]).unwrap())
        .unwrap();
    assert_eq!(server_tls.generation(), 1);
    let mut answered = epoch(&client).await;
    if answered == Err(RpcError::Disconnected) {
        // The close raced the call: the next call reconnects.
        answered = epoch(&client).await;
    }
    assert_eq!(answered, Ok(7));

    // A CA change: the cluster moves to a new CA. The server now trusts
    // only it; a client still on the old CA's identity is cut off and
    // refused, and comes back once its own identity is renewed.
    let next = DevCa::new("swap").unwrap();
    let both = |id: Arc<mantis_services::tls::TlsIdentity>| {
        let mut id = (*id).clone();
        id.cas = vec![next.der().to_vec()];
        Arc::new(id)
    };
    server_tls
        .set(both(next.identity(Role::Realm, "realm-1", &[local()]).unwrap()))
        .unwrap();
    let mut refused = epoch(&client).await;
    if refused.is_ok() {
        refused = epoch(&client).await;
    }
    assert_eq!(
        refused,
        Err(RpcError::Disconnected),
        "an identity of the old CA is refused"
    );
    client_tls
        .set(both(next.identity(Role::Cell, "cell-1", &[local()]).unwrap()))
        .unwrap();
    assert_eq!(epoch(&client).await, Ok(7), "renewed, it is served again");

    // The Arc form is the same handle.
    let account = RpcServer::bind_tls(
        "127.0.0.1:0".parse().unwrap(),
        KEY.to_vec(),
        AccountService::new().router(),
        Some(
            ca.identity(Role::Account, "account-1", &[local()])
                .unwrap()
                .into(),
        ),
    )
    .await
    .unwrap();
    drop(account);
}

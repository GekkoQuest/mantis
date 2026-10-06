//! The Ops inspector end to end (plan 13): a headless local cluster, the
//! zone ticking on its own thread, read through the dashboard over HTTPS
//! with an operator token. System run times move between reads, the
//! component list names the engine's components, entity pages carry every
//! value as text, and nothing is served without a token.

#![expect(clippy::unwrap_used, clippy::panic)]

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};

use rustls::pki_types::{CertificateDer, ServerName};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use toy_server::cluster::{LocalOptions, start_local};

async fn get(addr: SocketAddr, cert: &[u8], path: &str, token: Option<&str>) -> (u16, String) {
    let mut roots = rustls::RootCertStore::empty();
    roots.add(CertificateDer::from(cert.to_vec())).unwrap();
    let config =
        rustls::ClientConfig::builder_with_provider(Arc::new(rustls::crypto::ring::default_provider()))
            .with_safe_default_protocol_versions()
            .unwrap()
            .with_root_certificates(roots)
            .with_no_client_auth();
    let tcp = TcpStream::connect(addr).await.unwrap();
    let mut s = tokio_rustls::TlsConnector::from(Arc::new(config))
        .connect(ServerName::try_from("localhost").unwrap(), tcp)
        .await
        .unwrap();
    let auth = token
        .map(|t| format!("authorization: Bearer {t}\r\n"))
        .unwrap_or_default();
    let req = format!("GET {path} HTTP/1.1\r\nhost: localhost\r\nconnection: close\r\n{auth}\r\n");
    s.write_all(req.as_bytes()).await.unwrap();
    let mut out = Vec::new();
    let _ = s.read_to_end(&mut out).await;
    let text = String::from_utf8_lossy(&out).into_owned();
    let status = text.get(9..12).and_then(|c| c.parse().ok()).unwrap_or(0);
    let body = text
        .split_once("\r\n\r\n")
        .map_or(String::new(), |(_, b)| b.to_owned());
    (status, body)
}

/// The integer after `"key":` in `json` (the first one).
fn int(json: &str, key: &str) -> u64 {
    let at = json
        .find(&format!("\"{key}\":"))
        .unwrap_or_else(|| panic!("{key} in {json}"));
    json[at + key.len() + 3..]
        .chars()
        .take_while(char::is_ascii_digit)
        .collect::<String>()
        .parse()
        .unwrap()
}

#[test]
fn the_dashboard_serves_system_times_components_and_entity_pages() {
    let world = start_local(&LocalOptions { seed: 3, bots: 6 }).unwrap();
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let (addr, cert, token) = (world.ops_addr, world.ops_cert_der.clone(), world.token.clone());
    let read = |path: &str, token: Option<&str>| rt.block_on(get(addr, &cert, path, token));

    // No token, no data.
    assert_eq!(read("/inspect/1/systems", None).0, 401);
    assert_eq!(read("/inspect/1/entities?component=server.body", None).0, 401);

    // Run times appear once the host publishes them, and move.
    let start = Instant::now();
    let first = loop {
        let (status, body) = read("/inspect/1/systems", Some(&token));
        if status == 200 {
            break body;
        }
        assert!(start.elapsed() < Duration::from_secs(20), "{status} {body}");
        std::thread::sleep(Duration::from_millis(100));
    };
    assert!(first.starts_with("{\"cell\":1,\"tick\":"), "{first}");
    assert!(
        first.contains("\"name\":\"server.graphs\",\"phase\":\"Effects\""),
        "{first}"
    );
    assert!(first.contains("\"phase\":\"Movement\""), "{first}");
    for key in ["inbox_depth", "encode_micros_p99", "log_lag_ticks"] {
        let _ = int(&first, key);
    }
    std::thread::sleep(Duration::from_millis(1500));
    let (_, second) = read("/inspect/1/systems", Some(&token));
    assert!(int(&second, "tick") > int(&first, "tick"), "{first}\n{second}");
    assert!(int(&second, "runs") > int(&first, "runs"), "{first}\n{second}");

    // The component list, then a page of bodies with every value as text.
    let (status, names) = read("/inspect/1/components", Some(&token));
    assert_eq!(status, 200, "{names}");
    assert!(names.starts_with("{\"components\":["), "{names}");
    assert!(names.contains("\"server.body\""), "{names}");
    let (status, page) = read(
        "/inspect/1/entities?component=server.body&offset=0&limit=2",
        Some(&token),
    );
    assert_eq!(status, 200, "{page}");
    assert!(page.contains("\"component\":\"server.body\""), "{page}");
    assert!(int(&page, "total") >= 1, "{page}");
    assert!(page.contains("\"id\":\""), "{page}");
    assert!(page.contains("{\"name\":\"server.body\",\"value\":\""), "{page}");

    // Bad pages are refused before reaching the cell; unknown names after.
    assert_eq!(
        read("/inspect/1/entities?component=server.body&limit=0", Some(&token)).0,
        400
    );
    assert_eq!(
        read(
            "/inspect/1/entities?component=server.body&limit=101",
            Some(&token)
        )
        .0,
        400
    );
    assert_eq!(read("/inspect/1/entities", Some(&token)).0, 400);
    assert_eq!(read("/inspect/1/entities?component=no.such", Some(&token)).0, 502);
    assert_eq!(read("/inspect/99/systems", Some(&token)).0, 502);
    world.stop().unwrap();
}

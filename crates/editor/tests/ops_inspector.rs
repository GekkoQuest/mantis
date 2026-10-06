//! The server half of the inspector against a stand-in Ops dashboard (TLS with a
//! self-signed certificate, bearer token, JSON bodies, one of them chunked): every view
//! parses, a wrong token or an unpinned certificate is refused, and the client only ever
//! sends `GET`.

#![allow(clippy::unwrap_used, clippy::indexing_slicing)]

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener};
use std::sync::{Arc, Mutex};

use mantis_editor::ops::{OpsError, OpsInspector};
use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};

type TestResult = Result<(), Box<dyn std::error::Error>>;

const TOKEN: &str = "operator-token-0123456789";

fn body_for(path: &str) -> (bool, String) {
    match path {
        "/inspect/1" => (
            false,
            r#"{"audit":17,"before":"","after":"cell=1 tick=900 state_hash=0000000000003039 sessions=3 entities=42 cheats=0","undo":""}"#.to_owned(),
        ),
        "/inspect/1/systems" => (
            true,
            r#"{"cell":1,"tick":900,"systems":[{"name":"core.movement","phase":"Movement","micros_last":12,"micros_p99":31,"runs":900},{"name":"std.party.sync","phase":"Simulation","micros_last":3,"micros_p99":9,"runs":900}],"inbox_depth":2,"encode_micros_p99":14,"log_lag_ticks":1}"#.to_owned(),
        ),
        "/inspect/1/components" => (false, r#"{"components":["core.position","core.velocity"]}"#.to_owned()),
        "/inspect/1/entities?component=core.position&offset=0&limit=100" => (
            false,
            r#"{"cell":1,"tick":900,"component":"core.position","total":2,"entities":[{"id":"3:0","components":[{"name":"core.position","value":"(1.0, 0.0, 2.0)"}]},{"id":"5:1","components":[{"name":"core.position","value":"(4.0, 0.0, 8.0)"}]}]}"#.to_owned(),
        ),
        _ => (false, "{}".to_owned()),
    }
}

/// A dashboard that answers `requests` connections, recording each request line.
fn dashboard(requests: usize) -> (SocketAddr, Vec<u8>, Arc<Mutex<Vec<String>>>) {
    let ck = rcgen::generate_simple_self_signed(vec!["localhost".to_owned()]).unwrap();
    let cert = ck.cert.der().to_vec();
    let key = PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(ck.signing_key.serialize_der()));
    let config =
        rustls::ServerConfig::builder_with_provider(Arc::new(rustls::crypto::ring::default_provider()))
            .with_safe_default_protocol_versions()
            .unwrap()
            .with_no_client_auth()
            .with_single_cert(vec![CertificateDer::from(cert.clone())], key)
            .unwrap();
    let config = Arc::new(config);
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let seen = Arc::new(Mutex::new(Vec::new()));
    let log = Arc::clone(&seen);
    std::thread::spawn(move || {
        for _ in 0..requests {
            let Ok((tcp, _)) = listener.accept() else { return };
            let conn = rustls::ServerConnection::new(Arc::clone(&config)).unwrap();
            let mut s = rustls::StreamOwned::new(conn, tcp);
            let mut req = Vec::new();
            let mut buf = [0u8; 1024];
            while !req.windows(4).any(|w| w == b"\r\n\r\n") {
                match s.read(&mut buf) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => req.extend_from_slice(&buf[..n]),
                }
            }
            let text = String::from_utf8_lossy(&req).into_owned();
            let line = text.lines().next().unwrap_or("").to_owned();
            log.lock().unwrap().push(line.clone());
            let path = line.split(' ').nth(1).unwrap_or("");
            let authorized = text
                .lines()
                .any(|l| l.eq_ignore_ascii_case(&format!("authorization: Bearer {TOKEN}")));
            let response = if authorized {
                let (chunked, body) = body_for(path);
                if chunked {
                    let (a, b) = body.split_at(body.len() / 2);
                    format!(
                        "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ntransfer-encoding: chunked\r\n\r\n{:x}\r\n{a}\r\n{:x}\r\n{b}\r\n0\r\n\r\n",
                        a.len(),
                        b.len()
                    )
                } else {
                    format!("HTTP/1.1 200 OK\r\ncontent-length: {}\r\n\r\n{body}", body.len())
                }
            } else {
                "HTTP/1.1 401 Unauthorized\r\ncontent-length: 9\r\n\r\nno token.".to_owned()
            };
            let _ = s.write_all(response.as_bytes());
            s.conn.send_close_notify();
            let _ = s.flush();
        }
    });
    (addr, cert, seen)
}

#[test]
fn every_inspector_view_parses_over_pinned_tls() -> TestResult {
    let (addr, cert, seen) = dashboard(5);
    let ops = OpsInspector::new(addr, &cert, TOKEN)?;
    let cell = ops.cell(1)?;
    assert_eq!(
        (cell.tick, cell.sessions, cell.entities, cell.state_hash),
        (900, 3, 42, 12345)
    );
    let systems = ops.systems(1)?;
    assert_eq!(systems.systems.len(), 2);
    assert_eq!(
        systems.systems.first().map(|s| (s.name.as_str(), s.micros_p99)),
        Some(("core.movement", 31))
    );
    assert_eq!(
        (
            systems.inbox_depth,
            systems.encode_micros_p99,
            systems.log_lag_ticks
        ),
        (2, 14, 1)
    );
    assert_eq!(ops.components(1)?, ["core.position", "core.velocity"]);
    let page = ops.entities(1, "core.position", 0, 500)?;
    assert_eq!(page.total, 2);
    assert_eq!(
        page.entities
            .get(1)
            .map(|e| (e.id.as_str(), e.components.clone())),
        Some((
            "5:1",
            vec![("core.position".to_owned(), "(4.0, 0.0, 8.0)".to_owned())]
        ))
    );
    // A wrong token is the dashboard's 401.
    let bad = OpsInspector::new(addr, &cert, "wrong-token-0123456789")?;
    assert!(matches!(bad.cell(1), Err(OpsError::Status(401, _))));
    // Read-only: every request was a GET, and the page size was capped.
    let lines = seen.lock().map_err(|e| e.to_string())?.clone();
    assert_eq!(lines.len(), 5);
    assert!(lines.iter().all(|l| l.starts_with("GET /inspect/")), "{lines:?}");
    assert!(lines.iter().any(|l| l.contains("limit=100")));
    Ok(())
}

#[test]
fn a_dashboard_with_another_certificate_is_refused() -> TestResult {
    let (addr, _cert, _) = dashboard(1);
    let other = rcgen::generate_simple_self_signed(vec!["localhost".to_owned()])?;
    let ops = OpsInspector::new(addr, other.cert.der(), TOKEN)?;
    assert!(matches!(ops.cell(1), Err(OpsError::Transport(_))));
    Ok(())
}

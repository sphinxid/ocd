//! Certificate trust of the WebSocket transport against a local `wss://` server signed by a private CA.
//!
//! The trust is read from the environment once per process, so each case runs this test binary again as a child
//! (`probe_child`) with its own environment.

use std::io::{Read, Write};
use std::net::TcpListener;
use std::process::Command;
use std::sync::Arc;

use rcgen::{BasicConstraints, CertificateParams, DnType, IsCa, KeyPair, KeyUsagePurpose};
use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};
use tokio_tungstenite::tungstenite::handshake::derive_accept_key;

const CLEARED: [&str; 13] = [
    "NODE_EXTRA_CA_CERTS",
    "SSL_CERT_FILE",
    "SSL_CERT_DIR",
    "NODE_TLS_REJECT_UNAUTHORIZED",
    "NODE_USE_SYSTEM_CA",
    "HTTP_PROXY",
    "HTTPS_PROXY",
    "ALL_PROXY",
    "WS_PROXY",
    "WSS_PROXY",
    "http_proxy",
    "https_proxy",
    "all_proxy",
];

fn pki() -> (String, CertificateDer<'static>, PrivateKeyDer<'static>) {
    let ca_key = KeyPair::generate().unwrap();
    let mut ca = CertificateParams::new(Vec::<String>::new()).unwrap();
    ca.distinguished_name.push(DnType::CommonName, "ocd test CA");
    ca.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
    ca.key_usages = vec![KeyUsagePurpose::KeyCertSign, KeyUsagePurpose::CrlSign];
    let ca = ca.self_signed(&ca_key).unwrap();
    let leaf_key = KeyPair::generate().unwrap();
    let mut leaf = CertificateParams::new(vec!["127.0.0.1".to_string(), "localhost".to_string()]).unwrap();
    leaf.distinguished_name.push(DnType::CommonName, "127.0.0.1");
    let leaf = leaf.signed_by(&leaf_key, &ca, &ca_key).unwrap();
    (ca.pem(), leaf.der().clone(), PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(leaf_key.serialize_der())))
}

/// Accepts WebSocket upgrades over TLS and sends one text frame; returns the port.
fn serve(cert: CertificateDer<'static>, key: PrivateKeyDer<'static>) -> u16 {
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let config = rustls::ServerConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .unwrap()
        .with_no_client_auth()
        .with_single_cert(vec![cert], key)
        .unwrap();
    let config = Arc::new(config);
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    std::thread::spawn(move || {
        for socket in listener.incoming().flatten() {
            let config = config.clone();
            std::thread::spawn(move || {
                let Ok(conn) = rustls::ServerConnection::new(config) else { return };
                let mut tls = rustls::StreamOwned::new(conn, socket);
                let mut request = Vec::new();
                let mut buf = [0u8; 1024];
                while !request.windows(4).any(|w| w == b"\r\n\r\n") {
                    match tls.read(&mut buf) {
                        Ok(0) | Err(_) => return,
                        Ok(n) => request.extend_from_slice(&buf[..n]),
                    }
                }
                let text = String::from_utf8_lossy(&request).into_owned();
                let Some(key) = text.lines().find_map(|l| {
                    let (name, value) = l.split_once(':')?;
                    name.trim().eq_ignore_ascii_case("sec-websocket-key").then(|| value.trim().to_string())
                }) else {
                    return;
                };
                let accept = derive_accept_key(key.as_bytes());
                let response = format!(
                    "HTTP/1.1 101 Switching Protocols\r\nupgrade: websocket\r\nconnection: Upgrade\r\nsec-websocket-accept: {accept}\r\n\r\n"
                );
                let _ = tls.write_all(response.as_bytes());
                let _ = tls.write_all(b"\x81\x02hi");
                let _ = tls.flush();
                let _ = tls.read(&mut buf);
            });
        }
    });
    port
}

/// Child side: open `$OCD_WS_PROBE_URL` with the transport's connector and print the outcome.
#[test]
fn probe_child() {
    let Ok(url) = std::env::var("OCD_WS_PROBE_URL") else { return };
    let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
    runtime.block_on(async {
        match oc_ai::route::websocket::open(&url, &oc_ai::route::Headers::default()).await {
            Ok(_connection) => println!("PROBE OK"),
            Err(error) => println!("PROBE ERR {error:?}"),
        }
    });
}

fn probe(url: &str, env: &[(&str, &str)]) -> (bool, String) {
    let mut command = Command::new(std::env::current_exe().unwrap());
    command.args(["--exact", "probe_child", "--nocapture", "--test-threads=1"]);
    for name in CLEARED {
        command.env_remove(name);
    }
    command.env("OCD_WS_PROBE_URL", url);
    for (name, value) in env {
        command.env(name, value);
    }
    let output = command.output().unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    assert!(stdout.contains("PROBE "), "child did not probe: {stdout}\n{}", String::from_utf8_lossy(&output.stderr));
    (stdout.contains("PROBE OK"), stdout)
}

#[test]
fn environment_controls_websocket_trust() {
    let (ca_pem, cert, key) = pki();
    let port = serve(cert, key);
    let url = format!("wss://127.0.0.1:{port}/v1/responses");
    let dir = tempfile::tempdir().unwrap();
    let ca = dir.path().join("ca.pem");
    std::fs::write(&ca, &ca_pem).unwrap();
    let certs = dir.path().join("certs");
    std::fs::create_dir(&certs).unwrap();
    std::fs::write(certs.join("private-ca.pem"), &ca_pem).unwrap();
    let ca = ca.to_str().unwrap();

    let (ok, out) = probe(&url, &[]);
    assert!(!ok && out.contains("invalid peer certificate"), "{out}");
    for env in [
        ("NODE_EXTRA_CA_CERTS", ca),
        ("SSL_CERT_FILE", ca),
        ("SSL_CERT_DIR", certs.to_str().unwrap()),
        ("NODE_TLS_REJECT_UNAUTHORIZED", "0"),
    ] {
        let (ok, out) = probe(&url, &[env]);
        assert!(ok, "{env:?}: {out}");
    }
}

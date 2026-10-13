//! Certificate trust of the shared HTTP client against a local TLS server signed by a private CA.
//!
//! The trust is read from the environment once per process, so each case runs this test binary again as a child
//! (`probe_child`) with its own environment.

use std::io::{Read, Write};
use std::net::TcpListener;
use std::path::Path;
use std::process::Command;
use std::sync::Arc;

use rcgen::{BasicConstraints, CertificateParams, DnType, IsCa, KeyPair, KeyUsagePurpose};
use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};

const TLS_VARS: [&str; 5] = ["NODE_EXTRA_CA_CERTS", "SSL_CERT_FILE", "SSL_CERT_DIR", "NODE_TLS_REJECT_UNAUTHORIZED", "NODE_USE_SYSTEM_CA"];
const PROXY_VARS: [&str; 8] = ["HTTP_PROXY", "HTTPS_PROXY", "ALL_PROXY", "NO_PROXY", "http_proxy", "https_proxy", "all_proxy", "no_proxy"];

/// A private CA (PEM) and a server certificate for 127.0.0.1 / localhost signed by it.
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
    let key = PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(leaf_key.serialize_der()));
    (ca.pem(), leaf.der().clone(), key)
}

/// A self-signed certificate for `names` marked as a CA, as `openssl req -x509` makes by default.
fn self_signed_ca(names: &[&str]) -> (String, CertificateDer<'static>, PrivateKeyDer<'static>) {
    let key = KeyPair::generate().unwrap();
    let mut params = CertificateParams::new(names.iter().map(|n| n.to_string()).collect::<Vec<_>>()).unwrap();
    params.distinguished_name.push(DnType::CommonName, names[0]);
    params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
    let cert = params.self_signed(&key).unwrap();
    (cert.pem(), cert.der().clone(), PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(key.serialize_der())))
}

/// Serves `HTTP/1.1 200 ok` over TLS on a background thread; returns the port.
fn serve(cert: CertificateDer<'static>, key: PrivateKeyDer<'static>) -> u16 {
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let mut config = rustls::ServerConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .unwrap()
        .with_no_client_auth()
        .with_single_cert(vec![cert], key)
        .unwrap();
    config.alpn_protocols = vec![b"http/1.1".to_vec()];
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
                let _ = tls.write_all(b"HTTP/1.1 200 OK\r\ncontent-length: 2\r\nconnection: close\r\n\r\nok");
                tls.conn.send_close_notify();
                let _ = tls.flush();
            });
        }
    });
    port
}

/// Child side: GET `$OCD_TLS_PROBE_URL` with the shared client and print the outcome.
#[test]
fn probe_child() {
    let Ok(url) = std::env::var("OCD_TLS_PROBE_URL") else { return };
    let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
    runtime.block_on(async {
        match oc_util::http::client().get(&url).send().await {
            Ok(response) => println!("PROBE OK {} {}", response.status().as_u16(), response.text().await.unwrap_or_default()),
            Err(error) => {
                let mut chain = error.to_string();
                let mut source = std::error::Error::source(&error);
                while let Some(cause) = source {
                    chain.push_str(&format!(": {cause}"));
                    source = cause.source();
                }
                println!("PROBE ERR {chain}");
            }
        }
    });
}

struct Outcome {
    stdout: String,
    stderr: String,
}

impl Outcome {
    fn ok(&self) -> bool {
        self.stdout.contains("PROBE OK 200 ok")
    }
}

fn probe(url: &str, env: &[(&str, &str)]) -> Outcome {
    let mut command = Command::new(std::env::current_exe().unwrap());
    command.args(["--exact", "probe_child", "--nocapture", "--test-threads=1"]);
    for name in TLS_VARS.iter().chain(PROXY_VARS.iter()) {
        command.env_remove(name);
    }
    command.env("OCD_TLS_PROBE_URL", url);
    for (name, value) in env {
        command.env(name, value);
    }
    let output = command.output().unwrap();
    let outcome =
        Outcome { stdout: String::from_utf8_lossy(&output.stdout).into_owned(), stderr: String::from_utf8_lossy(&output.stderr).into_owned() };
    assert!(outcome.stdout.contains("PROBE "), "child did not probe: {}\n{}", outcome.stdout, outcome.stderr);
    outcome
}

fn path(p: &Path) -> &str {
    p.to_str().unwrap()
}

#[test]
fn environment_controls_certificate_trust() {
    let (ca_pem, cert, key) = pki();
    let port = serve(cert, key);
    let url = format!("https://127.0.0.1:{port}/");
    let dir = tempfile::tempdir().unwrap();
    let ca = dir.path().join("ca.pem");
    std::fs::write(&ca, &ca_pem).unwrap();
    let certs = dir.path().join("certs");
    std::fs::create_dir(&certs).unwrap();
    std::fs::write(certs.join("private-ca.pem"), &ca_pem).unwrap();
    let mixed = dir.path().join("mixed.pem");
    std::fs::write(&mixed, format!("not a certificate\n{ca_pem}")).unwrap();
    let garbage = dir.path().join("garbage.pem");
    std::fs::write(&garbage, "not a certificate\n").unwrap();
    let missing = dir.path().join("missing.pem");

    // Untrusted without any variable.
    let none = probe(&url, &[]);
    assert!(!none.ok(), "{}", none.stdout);
    assert!(none.stdout.contains("invalid peer certificate"), "{}", none.stdout);

    // NODE_EXTRA_CA_CERTS adds to the defaults; text around the PEM blocks is ignored.
    assert!(probe(&url, &[("NODE_EXTRA_CA_CERTS", path(&ca))]).ok());
    assert!(probe(&url, &[("NODE_EXTRA_CA_CERTS", path(&mixed))]).ok());

    // An unreadable or certificate-less file is skipped with upstream's warning.
    let gone = probe(&url, &[("NODE_EXTRA_CA_CERTS", path(&missing))]);
    assert!(!gone.ok());
    let warning = format!("warn: ignoring extra certs from {}, load failed: No such file or directory", missing.display());
    assert!(gone.stderr.contains(&warning), "{}", gone.stderr);
    let bad = probe(&url, &[("NODE_EXTRA_CA_CERTS", path(&garbage))]);
    assert!(!bad.ok());
    let warning = format!("warn: ignoring extra certs from {}, load failed: error:10000009:SSL routines:OPENSSL_internal:PEM routines", garbage.display());
    assert!(bad.stderr.contains(&warning), "{}", bad.stderr);

    // The system store locations.
    assert!(probe(&url, &[("SSL_CERT_FILE", path(&ca))]).ok());
    assert!(probe(&url, &[("SSL_CERT_DIR", path(&certs))]).ok());
    let list = format!("{}:{}", path(&dir.path().join("nonexistent")), path(&certs));
    assert!(probe(&url, &[("SSL_CERT_DIR", &list)]).ok());

    // Verification off only for exactly "0".
    assert!(probe(&url, &[("NODE_TLS_REJECT_UNAUTHORIZED", "0")]).ok());
    assert!(!probe(&url, &[("NODE_TLS_REJECT_UNAUTHORIZED", "false")]).ok());
    assert!(!probe(&url, &[("NODE_TLS_REJECT_UNAUTHORIZED", "1")]).ok());
}

#[test]
fn trusted_self_signed_ca_certificate_as_server_certificate() {
    // Accepted like BoringSSL does when the certificate itself is trusted, but still checked against the host name.
    let (pem, cert, key) = self_signed_ca(&["127.0.0.1", "localhost"]);
    let url = format!("https://127.0.0.1:{}/", serve(cert, key));
    let (other_pem, other_cert, other_key) = self_signed_ca(&["example.invalid"]);
    let other_url = format!("https://127.0.0.1:{}/", serve(other_cert, other_key));
    let dir = tempfile::tempdir().unwrap();
    let (trusted, other) = (dir.path().join("self.pem"), dir.path().join("other.pem"));
    std::fs::write(&trusted, pem).unwrap();
    std::fs::write(&other, other_pem).unwrap();

    assert!(!probe(&url, &[]).ok());
    assert!(probe(&url, &[("NODE_EXTRA_CA_CERTS", path(&trusted))]).ok());
    let wrong_host = probe(&other_url, &[("NODE_EXTRA_CA_CERTS", path(&other))]);
    assert!(!wrong_host.ok() && wrong_host.stdout.contains("certificate not valid for name"), "{}", wrong_host.stdout);
}

//! Outgoing HTTPS and WSS clients with the certificate trust of upstream's runtime.
//!
//! Upstream runs on Bun 1.4.4 (1.4.2 before 2.1), compiled with `--use-system-ca`. Its fetch and WebSocket clients trust, together:
//!
//! - Bun's bundled Mozilla roots (here: `webpki-roots`);
//! - the system store: every PEM certificate in the bundle files of [`SYSTEM_FILES`] (or only `$SSL_CERT_FILE` when
//!   that is set) and in every regular file directly inside [`SYSTEM_DIRS`] (or the `:`-separated `$SSL_CERT_DIR`
//!   when that is set). A set but empty variable means no files or directories;
//! - the PEM certificates in the file named by `$NODE_EXTRA_CA_CERTS`. If it can't be read or holds no certificate,
//!   Bun prints `warn: ignoring extra certs from <path>, load failed: <reason>` once on stderr and goes on.
//!
//! `NODE_TLS_REJECT_UNAUTHORIZED=0` (exactly `0`) turns certificate verification off.
//!
//! Verification is webpki's, plus one BoringSSL behaviour: a trusted self-signed certificate that is marked as a CA
//! (what a plain `openssl req -x509` makes) is accepted as the server certificate, after the host name check.
//!
//! The certificates are read once, at the first handshake (or by [`preload`]), and shared by every client.

use std::collections::HashSet;
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::sync::{Arc, LazyLock};

use rustls::client::WebPkiServerVerifier;
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::crypto::CryptoProvider;
use rustls::pki_types::pem::PemObject;
use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
use rustls::server::ParsedCertificate;
use rustls::{CertificateError, ClientConfig, DigitallySignedStruct, RootCertStore, SignatureScheme};

/// System bundle files, in the order Bun reads them (all that exist are loaded).
pub const SYSTEM_FILES: [&str; 8] = [
    "/etc/ssl/certs/ca-certificates.crt",
    "/etc/pki/tls/certs/ca-bundle.crt",
    "/etc/ssl/ca-bundle.pem",
    "/etc/pki/tls/cert.pem",
    "/etc/pki/ca-trust/extracted/pem/tls-ca-bundle.pem",
    "/etc/ssl/cert.pem",
    "/usr/local/etc/openssl/cert.pem",
    "/usr/local/share/ca-certificates/ca-certificates.crt",
];

/// System certificate directories, in the order Bun reads them (regular files directly inside, not recursive).
pub const SYSTEM_DIRS: [&str; 8] = [
    "/etc/ssl/certs",
    "/etc/pki/tls/certs",
    "/usr/share/ca-certificates",
    "/usr/local/share/certs",
    "/etc/openssl/certs",
    "/var/ssl/certs",
    "/usr/local/etc/openssl/certs",
    "/System/Library/OpenSSL/certs",
];

/// The certificates and verification switch every outgoing client uses.
#[derive(Clone)]
pub struct Trust {
    roots: usize,
    webpki: Option<Arc<WebPkiServerVerifier>>,
    /// Loaded self-signed certificates (DER). BoringSSL accepts one presented as the server certificate even when it
    /// is marked as a CA, as a plain `openssl req -x509` certificate is; webpki refuses that (`CaUsedAsEndEntity`).
    self_signed: Arc<HashSet<Vec<u8>>>,
    verify: bool,
    /// Lines upstream prints on stderr while loading (`warn: ignoring extra certs from ...`).
    pub warnings: Vec<String>,
}

impl std::fmt::Debug for Trust {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Trust").field("roots", &self.roots).field("verify", &self.verify).field("warnings", &self.warnings).finish()
    }
}

impl Trust {
    /// Reads the trust settings through `env` (a variable lookup), using the real system paths.
    pub fn load(env: impl Fn(&str) -> Option<OsString>) -> Trust {
        Self::load_with(env, &SYSTEM_FILES, &SYSTEM_DIRS)
    }

    /// [`Trust::load`] with other default system locations (tests).
    pub fn load_with(env: impl Fn(&str) -> Option<OsString>, files: &[&str], dirs: &[&str]) -> Trust {
        let mut certs = Certs::default();
        certs.anchors.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());

        let files: Vec<PathBuf> = match env("SSL_CERT_FILE") {
            Some(file) if file.is_empty() => Vec::new(),
            Some(file) => vec![PathBuf::from(file)],
            None => files.iter().map(PathBuf::from).collect(),
        };
        for file in &files {
            if let Ok(bytes) = std::fs::read(file) {
                certs.add_pem(&bytes);
            }
        }
        let dirs: Vec<PathBuf> = match env("SSL_CERT_DIR") {
            Some(list) => std::env::split_paths(&list).filter(|p| !p.as_os_str().is_empty()).collect(),
            None => dirs.iter().map(PathBuf::from).collect(),
        };
        for dir in &dirs {
            certs.add_dir(dir);
        }

        let mut warnings = Vec::new();
        if let Some(extra) = env("NODE_EXTRA_CA_CERTS").filter(|v| !v.is_empty()) {
            let path = PathBuf::from(&extra);
            let failure = match std::fs::read(&path) {
                Ok(bytes) if certs.add_pem(&bytes) > 0 => None,
                // BoringSSL's message for a file without a readable PEM certificate.
                Ok(_) => Some("error:10000009:SSL routines:OPENSSL_internal:PEM routines".to_string()),
                Err(error) => Some(io_reason(&error)),
            };
            if let Some(reason) = failure {
                warnings.push(format!("warn: ignoring extra certs from {}, load failed: {reason}", path.display()));
            }
        }

        let verify = env("NODE_TLS_REJECT_UNAUTHORIZED").is_none_or(|v| v != "0");
        let self_signed = certs.extra.iter().filter(|c| is_self_issued(c)).map(|c| c.as_ref().to_vec()).collect();
        let mut roots = RootCertStore::empty();
        roots.extend(certs.anchors);
        roots.add_parsable_certificates(certs.extra);
        let count = roots.len();
        let webpki = WebPkiServerVerifier::builder_with_provider(Arc::new(roots), provider()).build().ok();
        Trust { roots: count, webpki, self_signed: Arc::new(self_signed), verify, warnings }
    }

    /// The process-wide trust, read from the environment on first use. Load warnings go to stderr once.
    pub fn global() -> &'static Trust {
        static TRUST: LazyLock<Trust> = LazyLock::new(|| {
            let trust = Trust::load(|name| std::env::var_os(name));
            for line in &trust.warnings {
                tracing::warn!("{line}");
                eprintln!("{line}");
            }
            trust
        });
        &TRUST
    }

    /// Number of trusted roots.
    pub fn roots(&self) -> usize {
        self.roots
    }

    /// Whether certificates are verified (`NODE_TLS_REJECT_UNAUTHORIZED` is not `0`).
    pub fn verifies(&self) -> bool {
        self.verify
    }

    /// A rustls client configuration with this trust and the given ALPN protocols.
    pub fn client_config(&self, alpn: &[&[u8]]) -> ClientConfig {
        client_config(Source::Owned(Arc::new(self.clone())), alpn)
    }

    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        intermediates: &[CertificateDer<'_>],
        server_name: &ServerName<'_>,
        ocsp_response: &[u8],
        now: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        if !self.verify {
            return Ok(ServerCertVerified::assertion());
        }
        let Some(webpki) = &self.webpki else { return Err(CertificateError::UnknownIssuer.into()) };
        match webpki.verify_server_cert(end_entity, intermediates, server_name, ocsp_response, now) {
            // webpki checks the validity period before the CA flag, so this certificate is not expired.
            Err(rustls::Error::InvalidCertificate(CertificateError::Other(other)))
                if other.0.downcast_ref::<webpki::Error>() == Some(&webpki::Error::CaUsedAsEndEntity)
                    && self.self_signed.contains(end_entity.as_ref()) =>
            {
                rustls::client::verify_server_name(&ParsedCertificate::try_from(end_entity)?, server_name)?;
                Ok(ServerCertVerified::assertion())
            }
            result => result,
        }
    }
}

fn provider() -> Arc<CryptoProvider> {
    CryptoProvider::get_default().cloned().unwrap_or_else(|| Arc::new(rustls::crypto::ring::default_provider()))
}

/// Where a client's verifier finds the trust: the process-wide one is read on the first handshake (or by
/// [`preload`]), so building a client never waits for the certificate files.
#[derive(Debug)]
enum Source {
    Global,
    Owned(Arc<Trust>),
}

fn client_config(source: Source, alpn: &[&[u8]]) -> ClientConfig {
    let provider = provider();
    let mut config = ClientConfig::builder_with_provider(provider.clone())
        .with_safe_default_protocol_versions()
        .expect("default TLS versions")
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(Verifier { source, provider }))
        .with_no_client_auth();
    config.alpn_protocols = alpn.iter().map(|p| p.to_vec()).collect();
    config
}

#[derive(Default)]
struct Certs {
    /// The bundled roots.
    anchors: Vec<rustls::pki_types::TrustAnchor<'static>>,
    /// Certificates read from files, without duplicates.
    extra: Vec<CertificateDer<'static>>,
    seen: HashSet<Vec<u8>>,
    /// Files already read (device, inode).
    files: HashSet<(u64, u64)>,
}

#[cfg(unix)]
fn file_id(meta: &std::fs::Metadata) -> Option<(u64, u64)> {
    use std::os::unix::fs::MetadataExt;
    Some((meta.dev(), meta.ino()))
}

#[cfg(not(unix))]
fn file_id(_meta: &std::fs::Metadata) -> Option<(u64, u64)> {
    None
}

impl Certs {
    /// Adds every PEM certificate in `bytes` (up to the first malformed one); returns how many were found.
    fn add_pem(&mut self, bytes: &[u8]) -> usize {
        let mut found = 0;
        for cert in CertificateDer::pem_slice_iter(bytes).map_while(Result::ok) {
            found += 1;
            if self.seen.insert(cert.as_ref().to_vec()) {
                self.extra.push(cert);
            }
        }
        found
    }

    fn add_dir(&mut self, dir: &Path) {
        let Ok(entries) = std::fs::read_dir(dir) else { return };
        let mut paths: Vec<PathBuf> = entries.filter_map(|e| e.ok().map(|e| e.path())).collect();
        paths.sort();
        for path in paths {
            // Follows symlinks, as the hash links in /etc/ssl/certs are; each target file is read once.
            let Ok(meta) = std::fs::metadata(&path) else { continue };
            if meta.is_file()
                && file_id(&meta).is_none_or(|id| self.files.insert(id))
                && let Ok(bytes) = std::fs::read(&path)
            {
                self.add_pem(&bytes);
            }
        }
    }
}

/// The OS error text without Rust's ` (os error N)` suffix, as Bun prints it.
fn io_reason(error: &std::io::Error) -> String {
    let text = error.to_string();
    match text.rfind(" (os error ") {
        Some(at) => text[..at].to_string(),
        None => text,
    }
}

/// Whether the certificate's issuer name equals its subject name (DER walk of the TBSCertificate).
fn is_self_issued(der: &[u8]) -> bool {
    /// One DER element: (tag, content, rest).
    fn element(input: &[u8]) -> Option<(u8, &[u8], &[u8])> {
        let (&tag, rest) = input.split_first()?;
        let (&first, mut rest) = rest.split_first()?;
        let len = if first < 0x80 {
            first as usize
        } else {
            let n = (first & 0x7f) as usize;
            if n == 0 || n > 4 || rest.len() < n {
                return None;
            }
            let len = rest[..n].iter().fold(0usize, |acc, &b| (acc << 8) | b as usize);
            rest = &rest[n..];
            len
        };
        (rest.len() >= len).then(|| (tag, &rest[..len], &rest[len..]))
    }
    let names = || {
        let (_, certificate, _) = element(der)?;
        let (_, tbs, _) = element(certificate)?;
        let (tag, _, mut rest) = element(tbs)?;
        if tag == 0xa0 {
            rest = element(rest)?.2; // version, then the serial number
        }
        let rest = element(rest)?.2; // signature algorithm
        let (_, issuer, rest) = element(rest)?;
        let rest = element(rest)?.2; // validity
        let (_, subject, _) = element(rest)?;
        Some(issuer == subject)
    };
    names().unwrap_or(false)
}

/// webpki verification, plus BoringSSL's acceptance of a trusted self-signed certificate that is marked as a CA;
/// nothing is checked but the handshake signatures when `NODE_TLS_REJECT_UNAUTHORIZED=0`.
#[derive(Debug)]
struct Verifier {
    source: Source,
    provider: Arc<CryptoProvider>,
}

impl ServerCertVerifier for Verifier {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        intermediates: &[CertificateDer<'_>],
        server_name: &ServerName<'_>,
        ocsp_response: &[u8],
        now: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        let trust = match &self.source {
            Source::Global => Trust::global(),
            Source::Owned(trust) => trust,
        };
        trust.verify_server_cert(end_entity, intermediates, server_name, ocsp_response, now)
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls12_signature(message, cert, dss, &self.provider.signature_verification_algorithms)
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(message, cert, dss, &self.provider.signature_verification_algorithms)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.provider.signature_verification_algorithms.supported_schemes()
    }
}

static HTTP_TLS: LazyLock<ClientConfig> = LazyLock::new(|| client_config(Source::Global, &[b"h2", b"http/1.1"]));
static WS_TLS: LazyLock<Arc<ClientConfig>> = LazyLock::new(|| Arc::new(client_config(Source::Global, &[])));

/// A reqwest builder for outgoing requests, with upstream's certificate trust and `fetch`'s proxy rules
/// ([`crate::proxy::fetch_proxy`], read from the environment per request, instead of reqwest's own reading of the
/// proxy variables). Use it for every client that may reach a remote HTTPS server.
pub fn client_builder() -> reqwest::ClientBuilder {
    reqwest::Client::builder()
        .use_preconfigured_tls(HTTP_TLS.clone())
        .proxy(reqwest::Proxy::custom(|url| crate::proxy::fetch_proxy(&crate::proxy::process_env, url)))
}

/// `client_builder().build()`.
pub fn client() -> reqwest::Client {
    client_builder().build().expect("reqwest client")
}

/// Reads the certificates on a background thread (server start), so that the first handshake does not wait the few
/// milliseconds the system store takes.
pub fn preload() {
    std::thread::spawn(|| {
        Trust::global();
    });
}

/// The rustls configuration for outgoing WebSocket connections (no ALPN, like tungstenite's default).
pub fn websocket_tls() -> Arc<ClientConfig> {
    WS_TLS.clone()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn ca_pem(name: &str) -> String {
        let key = rcgen::KeyPair::generate().unwrap();
        let mut params = rcgen::CertificateParams::new(Vec::<String>::new()).unwrap();
        params.distinguished_name.push(rcgen::DnType::CommonName, name);
        params.is_ca = rcgen::IsCa::Ca(rcgen::BasicConstraints::Unconstrained);
        params.self_signed(&key).unwrap().pem()
    }

    fn load(env: &[(&str, &str)], files: &[&str], dirs: &[&str]) -> Trust {
        let env: HashMap<String, OsString> = env.iter().map(|(k, v)| (k.to_string(), OsString::from(v))).collect();
        Trust::load_with(|name| env.get(name).cloned(), files, dirs)
    }

    #[test]
    fn system_locations_and_overrides() {
        let base = webpki_roots::TLS_SERVER_ROOTS.len();
        let tmp = tempfile::tempdir().unwrap();
        let (a, b, c) = (ca_pem("a"), ca_pem("b"), ca_pem("c"));
        let bundle = tmp.path().join("bundle.crt");
        std::fs::write(&bundle, format!("{a}{b}")).unwrap();
        let dir = tmp.path().join("certs");
        std::fs::create_dir_all(dir.join("nested")).unwrap();
        // Same certificate under two names (hash link and file) counts once; subdirectories are not read.
        std::fs::write(dir.join("c.pem"), &c).unwrap();
        std::fs::write(dir.join("1234abcd.0"), &c).unwrap();
        std::fs::write(dir.join("nested").join("a.pem"), ca_pem("nested")).unwrap();
        let (bundle, dir) = (bundle.to_str().unwrap(), dir.to_str().unwrap());

        assert_eq!(load(&[], &[], &[]).roots(), base);
        assert_eq!(load(&[], &[bundle, "/nonexistent"], &[dir]).roots(), base + 3);
        // SSL_CERT_FILE replaces the bundle list, SSL_CERT_DIR the directory list; set but empty means none.
        assert_eq!(load(&[("SSL_CERT_FILE", dir)], &[bundle], &[dir]).roots(), base + 1);
        assert_eq!(load(&[("SSL_CERT_DIR", "")], &[bundle], &[dir]).roots(), base + 2);
        assert_eq!(load(&[("SSL_CERT_FILE", ""), ("SSL_CERT_DIR", "")], &[bundle], &[dir]).roots(), base);
        assert_eq!(load(&[("SSL_CERT_FILE", bundle), ("SSL_CERT_DIR", dir)], &[], &[]).roots(), base + 3);
        // NODE_EXTRA_CA_CERTS adds; an empty value is ignored without a warning.
        let extra = load(&[("NODE_EXTRA_CA_CERTS", bundle)], &[], &[]);
        assert_eq!((extra.roots(), extra.warnings.len()), (base + 2, 0));
        assert!(load(&[("NODE_EXTRA_CA_CERTS", "")], &[], &[]).warnings.is_empty());
        let dir_warning = load(&[("NODE_EXTRA_CA_CERTS", dir)], &[], &[]).warnings;
        assert_eq!(dir_warning, vec![format!("warn: ignoring extra certs from {dir}, load failed: Is a directory")]);
    }

    #[test]
    fn self_issued_names() {
        let key = rcgen::KeyPair::generate().unwrap();
        let mut ca = rcgen::CertificateParams::new(Vec::<String>::new()).unwrap();
        ca.distinguished_name.push(rcgen::DnType::CommonName, "ca");
        ca.is_ca = rcgen::IsCa::Ca(rcgen::BasicConstraints::Unconstrained);
        let ca = ca.self_signed(&key).unwrap();
        let leaf_key = rcgen::KeyPair::generate().unwrap();
        let mut leaf = rcgen::CertificateParams::new(vec!["localhost".to_string()]).unwrap();
        leaf.distinguished_name.push(rcgen::DnType::CommonName, "leaf");
        let leaf = leaf.signed_by(&leaf_key, &ca, &key).unwrap();
        assert!(is_self_issued(ca.der()));
        assert!(!is_self_issued(leaf.der()));
        assert!(!is_self_issued(b"\x30\x03\x02\x01"));
    }

    #[test]
    fn reject_unauthorized_switch() {
        assert!(load(&[], &[], &[]).verifies());
        assert!(!load(&[("NODE_TLS_REJECT_UNAUTHORIZED", "0")], &[], &[]).verifies());
        for value in ["", "1", "false", " 0", "00"] {
            assert!(load(&[("NODE_TLS_REJECT_UNAUTHORIZED", value)], &[], &[]).verifies(), "{value:?}");
        }
    }
}

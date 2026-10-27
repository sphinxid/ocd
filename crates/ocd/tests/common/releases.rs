//! A stand-in for GitHub's release API and downloads (127.0.0.1 only), for `ocd upgrade` and the update check.
//! The binary reaches it through `OCD_TEST_RELEASES_API` (test only); the unit tests in `src/release/tests.rs`
//! include this file too.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

/// The platform name of release archives built for this machine (`release::PLATFORM`).
pub const PLATFORM: Option<&str> = if cfg!(all(target_os = "linux", target_arch = "x86_64")) {
    Some("linux-x64")
} else if cfg!(all(target_os = "linux", target_arch = "aarch64")) {
    Some("linux-arm64")
} else if cfg!(all(target_os = "macos", target_arch = "aarch64")) {
    Some("darwin-arm64")
} else if cfg!(all(target_os = "windows", target_arch = "x86_64")) {
    Some("windows-x64")
} else {
    None
};

#[derive(Clone, Debug)]
pub struct Reply {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

impl Reply {
    pub fn json(value: serde_json::Value) -> Reply {
        Reply { status: 200, headers: vec![("content-type".into(), "application/json".into())], body: value.to_string().into_bytes() }
    }
    pub fn bytes(body: Vec<u8>) -> Reply {
        Reply { status: 200, headers: vec![("content-type".into(), "application/octet-stream".into())], body }
    }
    pub fn status(status: u16) -> Reply {
        Reply { status, headers: vec![("content-type".into(), "application/json".into())], body: br#"{"message":"Not Found"}"#.to_vec() }
    }
    pub fn header(mut self, k: &str, v: &str) -> Reply {
        self.headers.push((k.into(), v.into()));
        self
    }
}

#[derive(Default)]
struct State {
    routes: Mutex<HashMap<String, Reply>>,
    hits: Mutex<HashMap<String, usize>>,
    /// Request headers by path (the last request).
    seen: Mutex<HashMap<String, Vec<(String, String)>>>,
}

/// What [`MockGitHub::publish`] leaves out or breaks.
#[derive(Clone, Debug, Default)]
pub struct Publish {
    /// Also answer `/releases/latest` with this release.
    pub latest: bool,
    /// `SHA256SUMS` lists a wrong checksum for the archive.
    pub bad_checksum: bool,
    /// The archive for this platform is missing from the release.
    pub no_archive: bool,
    /// `SHA256SUMS` has no line for the archive.
    pub no_checksum_line: bool,
}

pub struct MockGitHub {
    /// `http://127.0.0.1:<port>`
    pub root: String,
    /// The API base to pass as `OCD_TEST_RELEASES_API`.
    pub api: String,
    state: Arc<State>,
}

impl MockGitHub {
    pub async fn start() -> MockGitHub {
        use axum::extract::Request;
        use axum::response::Response;
        let state = Arc::new(State::default());
        let shared = state.clone();
        let app = axum::Router::new().fallback(move |req: Request| {
            let state = shared.clone();
            async move {
                let path = req.uri().path().to_string();
                *state.hits.lock().unwrap().entry(path.clone()).or_default() += 1;
                let headers = req
                    .headers()
                    .iter()
                    .map(|(k, v)| (k.as_str().to_string(), v.to_str().unwrap_or_default().to_string()))
                    .collect();
                state.seen.lock().unwrap().insert(path.clone(), headers);
                let reply = state.routes.lock().unwrap().get(&path).cloned().unwrap_or_else(|| Reply::status(404));
                let mut res = Response::builder().status(reply.status);
                for (k, v) in &reply.headers {
                    res = res.header(k, v);
                }
                res.body(axum::body::Body::from(reply.body)).unwrap()
            }
        });
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let root = format!("http://{}", listener.local_addr().unwrap());
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        MockGitHub { api: format!("{root}/repos/sphinxid/ocd"), root, state }
    }

    pub fn set(&self, path: &str, reply: Reply) {
        self.state.routes.lock().unwrap().insert(path.to_string(), reply);
    }

    pub fn hits(&self, path: &str) -> usize {
        self.state.hits.lock().unwrap().get(path).copied().unwrap_or(0)
    }

    /// Requests whose path starts with `prefix`.
    pub fn hits_under(&self, prefix: &str) -> usize {
        self.state.hits.lock().unwrap().iter().filter(|(p, _)| p.starts_with(prefix)).map(|(_, n)| n).sum()
    }

    pub fn headers(&self, path: &str) -> Vec<(String, String)> {
        self.state.seen.lock().unwrap().get(path).cloned().unwrap_or_default()
    }

    pub fn latest_path(&self) -> String {
        "/repos/sphinxid/ocd/releases/latest".into()
    }

    pub fn tag_path(&self, tag: &str) -> String {
        format!("/repos/sphinxid/ocd/releases/tags/{tag}")
    }

    pub fn download_path(&self, tag: &str, name: &str) -> String {
        format!("/sphinxid/ocd/releases/download/{tag}/{name}")
    }

    /// Publishes release `tag` with this platform's archive holding `binary`, plus `SHA256SUMS`.
    pub fn publish(&self, tag: &str, binary: &[u8], opts: Publish) {
        let platform = PLATFORM.expect("a release platform");
        let name = archive_name(tag, platform);
        let archive = archive(tag, platform, binary);
        let mut sum = sha256_hex(&archive);
        if opts.bad_checksum {
            sum = sha256_hex(b"something else");
        }
        let mut sums = String::new();
        if !opts.no_checksum_line {
            sums.push_str(&format!("{sum}  {name}\n"));
        }
        sums.push_str(&format!("{}  ocd-{tag}-other-platform.tar.gz\n", sha256_hex(b"other")));
        let mut assets = vec![];
        if !opts.no_archive {
            self.set(&self.download_path(tag, &name), Reply::bytes(archive));
            assets.push(serde_json::json!({ "name": name, "browser_download_url": format!("{}{}", self.root, self.download_path(tag, &name)) }));
        }
        self.set(&self.download_path(tag, "SHA256SUMS"), Reply::bytes(sums.into_bytes()));
        assets.push(serde_json::json!({ "name": "SHA256SUMS", "browser_download_url": format!("{}{}", self.root, self.download_path(tag, "SHA256SUMS")) }));
        let release = serde_json::json!({
            "tag_name": tag,
            "html_url": format!("{}/sphinxid/ocd/releases/tag/{tag}", self.root),
            "draft": false,
            "prerelease": false,
            "assets": assets,
        });
        self.set(&self.tag_path(tag), Reply::json(release.clone()));
        if opts.latest {
            self.set(&self.latest_path(), Reply::json(release));
        }
    }
}

pub fn archive_name(tag: &str, platform: &str) -> String {
    let ext = if platform.starts_with("windows") { "zip" } else { "tar.gz" };
    format!("ocd-{tag}-{platform}.{ext}")
}

pub fn sha256_hex(bytes: &[u8]) -> String {
    use sha2::Digest;
    hex::encode(sha2::Sha256::digest(bytes))
}

/// A release archive laid out like the release workflow's: `ocd-<tag>-<platform>/` with the binary, the licenses
/// and the README.
pub fn archive(tag: &str, platform: &str, binary: &[u8]) -> Vec<u8> {
    let dir = format!("ocd-{tag}-{platform}");
    let exe = if platform.starts_with("windows") { "ocd.exe" } else { "ocd" };
    let entries: Vec<(String, &[u8], u32)> = vec![
        (format!("{dir}/LICENSE"), b"MIT".as_slice(), 0o644),
        (format!("{dir}/{exe}"), binary, 0o755),
        (format!("{dir}/README.md"), b"# ocd".as_slice(), 0o644),
    ];
    if platform.starts_with("windows") { zip(&entries) } else { tar_gz(&dir, &entries) }
}

fn tar_header(name: &str, size: usize, mode: u32, kind: u8) -> [u8; 512] {
    let mut h = [0u8; 512];
    h[..name.len()].copy_from_slice(name.as_bytes());
    let octal = |h: &mut [u8; 512], at: usize, len: usize, v: u64| {
        let s = format!("{v:0width$o}\0", width = len - 1);
        h[at..at + len].copy_from_slice(s.as_bytes());
    };
    octal(&mut h, 100, 8, mode as u64);
    octal(&mut h, 108, 8, 1001);
    octal(&mut h, 116, 8, 1001);
    octal(&mut h, 124, 12, size as u64);
    octal(&mut h, 136, 12, 1_793_000_000);
    h[156] = kind;
    h[257..263].copy_from_slice(b"ustar\0");
    h[263..265].copy_from_slice(b"00");
    h[148..156].copy_from_slice(b"        ");
    let sum: u32 = h.iter().map(|b| *b as u32).sum();
    let s = format!("{sum:06o}\0 ");
    h[148..156].copy_from_slice(s.as_bytes());
    h
}

pub fn tar_gz(dir: &str, entries: &[(String, &[u8], u32)]) -> Vec<u8> {
    use std::io::Write;
    let mut tar = Vec::new();
    tar.extend(tar_header(&format!("{dir}/"), 0, 0o755, b'5'));
    for (name, data, mode) in entries {
        tar.extend(tar_header(name, data.len(), *mode, b'0'));
        tar.extend_from_slice(data);
        tar.resize(tar.len().div_ceil(512) * 512, 0);
    }
    tar.extend([0u8; 1024]);
    let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
    gz.write_all(&tar).unwrap();
    gz.finish().unwrap()
}

/// A stored (uncompressed) zip archive.
pub fn zip(entries: &[(String, &[u8], u32)]) -> Vec<u8> {
    let mut out = Vec::new();
    let mut central = Vec::new();
    for (name, data, _) in entries {
        let offset = out.len() as u32;
        let mut local = Vec::new();
        local.extend(0x0403_4b50u32.to_le_bytes());
        local.extend([20, 0, 0, 0, 0, 0, 0, 0, 0, 0]);
        local.extend(0u32.to_le_bytes());
        local.extend((data.len() as u32).to_le_bytes());
        local.extend((data.len() as u32).to_le_bytes());
        local.extend((name.len() as u16).to_le_bytes());
        local.extend(0u16.to_le_bytes());
        local.extend(name.as_bytes());
        out.extend(local);
        out.extend_from_slice(data);
        central.extend(0x0201_4b50u32.to_le_bytes());
        central.extend([20, 0, 20, 0, 0, 0, 0, 0, 0, 0, 0, 0]);
        central.extend(0u32.to_le_bytes());
        central.extend((data.len() as u32).to_le_bytes());
        central.extend((data.len() as u32).to_le_bytes());
        central.extend((name.len() as u16).to_le_bytes());
        central.extend([0u8; 12]);
        central.extend(offset.to_le_bytes());
        central.extend(name.as_bytes());
    }
    let cd_offset = out.len() as u32;
    let cd_len = central.len() as u32;
    out.extend(central);
    out.extend(0x0605_4b50u32.to_le_bytes());
    out.extend([0u8; 4]);
    out.extend((entries.len() as u16).to_le_bytes());
    out.extend((entries.len() as u16).to_le_bytes());
    out.extend(cd_len.to_le_bytes());
    out.extend(cd_offset.to_le_bytes());
    out.extend(0u16.to_le_bytes());
    out
}

/// A stand-in for a release binary: prints `ocd v<build> (opencode 2.1.4)` for `--version`, and otherwise runs
/// `real` with the same arguments (so a background service started from it is a real one).
#[cfg(unix)]
pub fn fake_binary(build: &str, real: Option<&std::path::Path>) -> Vec<u8> {
    let rest = match real {
        Some(r) => format!("exec '{}' \"$@\"\n", r.display()),
        None => "echo \"unexpected arguments: $*\" >&2\nexit 3\n".to_string(),
    };
    format!("#!/bin/sh\nif [ \"$1\" = \"--version\" ]; then\n  echo \"ocd v{build} (opencode 2.1.4)\"\n  exit 0\nfi\n{rest}").into_bytes()
}

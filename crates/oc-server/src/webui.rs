//! Static web UI serving (spec 06 §10.5): SPA fallback, CSP, brotli passthrough.
//!
//! Assets come from either an archive file (`[u32 LE indexLen][JSON {"path":[offset,len]}]
//! [brotli bodies]`, offsets relative to the body) or a plain directory (dev builds).

use std::collections::HashMap;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::{Arc, LazyLock};

use anyhow::{Context, bail};
use axum::extract::{Request, State};
use axum::http::{HeaderMap, HeaderValue, Method, StatusCode, header};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use base64::Engine;
use bytes::Bytes;
use regex::Regex;
use sha2::{Digest, Sha256};

use crate::app::App;

enum Source {
    /// Brotli-compressed bytes from an archive.
    Brotli(Bytes),
    /// A file on disk.
    File(PathBuf),
}

/// Loaded web UI assets.
pub struct WebUi {
    assets: HashMap<String, Source>,
}

impl WebUi {
    /// Load from an archive file or a directory.
    pub fn load(path: &Path) -> anyhow::Result<Self> {
        if path.is_dir() {
            let mut assets = HashMap::new();
            walk(path, path, &mut assets)?;
            return Ok(Self { assets });
        }
        let data = std::fs::read(path).with_context(|| format!("reading web UI archive {}", path.display()))?;
        Self::from_archive(Bytes::from(data))
    }

    /// Decode an in-memory archive.
    pub fn from_archive(archive: Bytes) -> anyhow::Result<Self> {
        if archive.len() < 4 {
            bail!("web UI archive is truncated");
        }
        let len = u32::from_le_bytes([archive[0], archive[1], archive[2], archive[3]]) as usize;
        let body = 4 + len;
        if archive.len() < body {
            bail!("web UI archive index is truncated");
        }
        let index: HashMap<String, (usize, usize)> = serde_json::from_slice(&archive[4..body]).context("web UI archive index")?;
        let mut assets = HashMap::new();
        for (key, (offset, size)) in index {
            let start = body + offset;
            let end = start + size;
            if end > archive.len() {
                bail!("web UI archive entry {key} is out of range");
            }
            assets.insert(key, Source::Brotli(archive.slice(start..end)));
        }
        Ok(Self { assets })
    }

    /// Encode assets into the archive format (each body brotli-compressed by the caller).
    pub fn encode_archive(entries: &[(&str, &[u8])]) -> Vec<u8> {
        let mut at = 0usize;
        let mut index = serde_json::Map::new();
        for (key, body) in entries {
            index.insert(key.to_string(), serde_json::json!([at, body.len()]));
            at += body.len();
        }
        let index = serde_json::to_vec(&index).unwrap_or_default();
        let mut out = (index.len() as u32).to_le_bytes().to_vec();
        out.extend_from_slice(&index);
        for (_, body) in entries {
            out.extend_from_slice(body);
        }
        out
    }

    fn plain(&self, key: &str) -> Option<Bytes> {
        match self.assets.get(key)? {
            Source::Brotli(b) => {
                let mut out = Vec::new();
                brotli_decompressor::Decompressor::new(&b[..], 4096).read_to_end(&mut out).ok()?;
                Some(Bytes::from(out))
            }
            Source::File(p) => std::fs::read(p).ok().map(Bytes::from),
        }
    }

    fn brotli(&self, key: &str) -> Option<Bytes> {
        match self.assets.get(key)? {
            Source::Brotli(b) => Some(b.clone()),
            Source::File(_) => None,
        }
    }

    fn has(&self, key: &str) -> bool {
        self.assets.contains_key(key)
    }

    /// Serve one request (`path` = URL pathname, not decoded).
    pub(crate) fn serve(&self, method: &Method, path: &str, headers: &HeaderMap) -> Response {
        let key = path.strip_prefix('/').unwrap_or(path);
        let encoded = if key != "index.html" && accepts_brotli(headers) { self.brotli(key) } else { None };
        let exists = self.has(key);
        if (key.starts_with("_assets/") || key.starts_with("icons/")) && !exists {
            let mut res = StatusCode::NOT_FOUND.into_response();
            res.headers_mut().insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
            return res;
        }
        let name = if exists { key } else { "index.html" };
        if !self.has(name) {
            return StatusCode::NOT_FOUND.into_response();
        }
        if method != Method::GET && method != Method::HEAD {
            return StatusCode::METHOD_NOT_ALLOWED.into_response();
        }
        let html = name == "index.html";
        let body = match &encoded {
            Some(b) => b.clone(),
            None => match self.plain(name) {
                Some(b) => b,
                None => return StatusCode::NOT_FOUND.into_response(),
            },
        };
        let revalidate = html || name == "sw.js" || name == "registerSW.js";
        let mime = mime_guess::from_path(name).first_or_octet_stream();
        let mut res = if method == Method::HEAD { StatusCode::OK.into_response() } else { (StatusCode::OK, body.clone()).into_response() };
        let h = res.headers_mut();
        let ct = if mime.type_() == "text" || mime.subtype() == "javascript" || mime.subtype() == "json" {
            format!("{mime}; charset=utf-8")
        } else {
            mime.to_string()
        };
        h.insert(header::CONTENT_TYPE, HeaderValue::from_str(&ct).unwrap_or(HeaderValue::from_static("application/octet-stream")));
        h.insert(
            header::CACHE_CONTROL,
            HeaderValue::from_static(if revalidate { "no-cache" } else { "public, max-age=31536000, immutable" }),
        );
        let policy = if html { csp_for_html(&String::from_utf8_lossy(&body)) } else { csp("") };
        if let Ok(v) = HeaderValue::from_str(&policy) {
            h.insert(header::CONTENT_SECURITY_POLICY, v);
        }
        h.insert(header::X_CONTENT_TYPE_OPTIONS, HeaderValue::from_static("nosniff"));
        if !html && self.brotli(name).is_some() {
            h.insert(header::VARY, HeaderValue::from_static("accept-encoding"));
        }
        if encoded.is_some() {
            h.insert(header::CONTENT_ENCODING, HeaderValue::from_static("br"));
        }
        res
    }
}

fn walk(root: &Path, dir: &Path, out: &mut HashMap<String, Source>) -> anyhow::Result<()> {
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let p = entry.path();
        if p.is_dir() {
            walk(root, &p, out)?;
        } else if !p.to_string_lossy().ends_with(".map") {
            let rel = p.strip_prefix(root)?.to_string_lossy().replace('\\', "/");
            out.insert(rel, Source::File(p));
        }
    }
    Ok(())
}

fn accepts_brotli(headers: &HeaderMap) -> bool {
    let value = headers.get(header::ACCEPT_ENCODING).and_then(|v| v.to_str().ok()).unwrap_or("");
    static Q0: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^q=0(?:\.0*)?$").expect("regex"));
    value.split(',').any(|entry| {
        let parts: Vec<String> = entry.split(';').map(|p| p.trim().to_ascii_lowercase()).collect();
        parts[0] == "br" && !parts[1..].iter().any(|p| Q0.is_match(p))
    })
}

fn csp(hash: &str) -> String {
    let script = if hash.is_empty() { String::new() } else { format!(" 'sha256-{hash}'") };
    format!(
        "default-src 'self'; script-src 'self' 'wasm-unsafe-eval'{script}; style-src 'self' 'unsafe-inline'; img-src 'self' data: https: blob:; font-src 'self' data:; media-src 'self' data:; connect-src * data: blob:; worker-src 'self' blob:"
    )
}

fn csp_for_html(body: &str) -> String {
    // <script ... id="oc-theme-preload-script" ...> (no src) — the regex crate has no lookahead,
    // so match the open tag and reject `src=` separately.
    static OPEN: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r#"(?is)<script\b([^>]*)>(.*?)</script>"#).expect("regex")
    });
    static ID: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r#"(?i)\bid=(?:"oc-theme-preload-script"|'oc-theme-preload-script')"#).expect("regex"));
    static SRC: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?i)\bsrc\s*=").expect("regex"));
    for cap in OPEN.captures_iter(body) {
        let attrs = &cap[1];
        if ID.is_match(attrs) && !SRC.is_match(attrs) {
            let hash = base64::engine::general_purpose::STANDARD.encode(Sha256::digest(cap[2].as_bytes()));
            return csp(&hash);
        }
    }
    csp("")
}

/// Whether a path belongs to the API (never served by the web UI).
pub(crate) fn is_api_path(path: &str) -> bool {
    path == "/api" || path.starts_with("/api/") || path.starts_with("/auth/") || path == "/openapi.json"
}

/// Middleware: serve non-API paths from the web UI before auth/readiness.
pub(crate) async fn web_ui(State(app): State<Arc<App>>, req: Request, next: Next) -> Response {
    match &app.web_ui {
        Some(ui) if !is_api_path(req.uri().path()) => ui.serve(req.method(), req.uri().path(), req.headers()),
        _ => next.run(req).await,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn brotli_acceptance() {
        let mut h = HeaderMap::new();
        h.insert(header::ACCEPT_ENCODING, HeaderValue::from_static("gzip, br"));
        assert!(accepts_brotli(&h));
        h.insert(header::ACCEPT_ENCODING, HeaderValue::from_static("br;q=0"));
        assert!(!accepts_brotli(&h));
        h.insert(header::ACCEPT_ENCODING, HeaderValue::from_static("br;q=0.5"));
        assert!(accepts_brotli(&h));
    }

    #[test]
    fn csp_hash_of_theme_script() {
        let html = r#"<html><script id="oc-theme-preload-script">abc</script></html>"#;
        let expected = base64::engine::general_purpose::STANDARD.encode(Sha256::digest(b"abc"));
        assert!(csp_for_html(html).contains(&format!("'sha256-{expected}'")));
        assert!(!csp_for_html("<script src=x id=\"oc-theme-preload-script\"></script>").contains("sha256"));
    }
}

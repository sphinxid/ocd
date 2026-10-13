//! Static web UI serving (spec 06 §10.5, upstream `packages/cli/src/services/web-ui.ts`): SPA
//! fallback, CSP, brotli passthrough.
//!
//! Assets come from either an archive (`packages/cli/src/app-archive.ts`: `[u32 LE indexLen][JSON
//! {"path":[offset,len]}][brotli bodies]`, offsets relative to the body) or a plain directory (an
//! unpacked `packages/app/dist`, like upstream's local builds).

use std::collections::HashMap;
use std::fmt;
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

/// Where [`crate::ServerOptions::web_ui`] comes from.
#[derive(Debug, Clone)]
pub enum WebUiSource {
    /// An archive file or a directory, loaded when the server starts.
    Path(PathBuf),
    /// Assets already loaded (e.g. an archive embedded in the binary).
    Loaded(Arc<WebUi>),
}

impl From<PathBuf> for WebUiSource {
    fn from(path: PathBuf) -> Self {
        Self::Path(path)
    }
}

impl From<WebUi> for WebUiSource {
    fn from(ui: WebUi) -> Self {
        Self::Loaded(Arc::new(ui))
    }
}

enum Source {
    /// Brotli-compressed bytes from an archive.
    Brotli(Bytes),
    /// A file on disk.
    File(PathBuf),
}

/// Loaded web UI assets.
pub struct WebUi {
    assets: HashMap<String, Source>,
    mime: fn(&str) -> String,
}

impl fmt::Debug for WebUi {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("WebUi").field("assets", &self.assets.len()).finish()
    }
}

/// `FSUtil.mimeType` stand-in: extension lookup, `application/octet-stream` when unknown. Callers
/// with upstream's exact `mime-types` table install it with [`WebUi::with_mime`].
fn default_mime(name: &str) -> String {
    mime_guess::from_path(name).first_raw().unwrap_or("application/octet-stream").to_string()
}

impl WebUi {
    /// No assets: every non-API path answers `404` (upstream with an empty embedded archive,
    /// i.e. a build made with `--skip-web-ui`).
    pub fn empty() -> Self {
        Self { assets: HashMap::new(), mime: default_mime }
    }

    /// Load from an archive file or a directory.
    pub fn load(path: &Path) -> anyhow::Result<Self> {
        if path.is_dir() {
            let mut assets = HashMap::new();
            walk(path, path, &mut assets)?;
            return Ok(Self { assets, mime: default_mime });
        }
        let data = std::fs::read(path).with_context(|| format!("reading web UI archive {}", path.display()))?;
        Self::from_archive(Bytes::from(data))
    }

    /// Decode an in-memory archive (`Bytes::from_static` keeps an embedded one zero-copy).
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
        Ok(Self { assets, mime: default_mime })
    }

    /// Use `lookup` (asset path → MIME type) for `content-type`.
    pub fn with_mime(mut self, lookup: fn(&str) -> String) -> Self {
        self.mime = lookup;
        self
    }

    /// Number of assets.
    pub fn len(&self) -> usize {
        self.assets.len()
    }

    /// Whether there are no assets.
    pub fn is_empty(&self) -> bool {
        self.assets.is_empty()
    }

    /// Whether the asset `key` (path without leading `/`) exists.
    pub fn contains(&self, key: &str) -> bool {
        self.assets.contains_key(key)
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
            let mut res = empty(StatusCode::NOT_FOUND);
            res.headers_mut().insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
            return res;
        }
        let name = if exists { key } else { "index.html" };
        if !self.has(name) {
            return empty(StatusCode::NOT_FOUND);
        }
        if method != Method::GET && method != Method::HEAD {
            return empty(StatusCode::METHOD_NOT_ALLOWED);
        }
        let html = name == "index.html";
        let body = match &encoded {
            Some(b) => b.clone(),
            // HEAD needs the bytes only for the HTML's CSP hash.
            None if method == Method::HEAD && !html => Bytes::new(),
            None => match self.plain(name) {
                Some(b) => b,
                None => return empty(StatusCode::NOT_FOUND),
            },
        };
        let revalidate = html || name == "sw.js" || name == "registerSW.js";
        // Upstream writes the asset without a content-length, so Node's compression applies to
        // every size (no 1 KiB threshold) and the response is chunked; a body without a size hint
        // does the same here.
        let mut res = if method == Method::HEAD { empty(StatusCode::OK) } else { (StatusCode::OK, unsized_body(body.clone())).into_response() };
        let h = res.headers_mut();
        let ct = (self.mime)(name);
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

/// A body without a size hint: hyper frames it chunked (or, for `HEAD`, sends no length), as
/// upstream's Node server does for web UI responses.
fn unsized_body(bytes: Bytes) -> axum::body::Body {
    let chunks = if bytes.is_empty() { Vec::new() } else { vec![Ok::<_, std::convert::Infallible>(bytes)] };
    axum::body::Body::from_stream(futures::stream::iter(chunks))
}

/// Response marker: a web UI answer without a body (its unsized body hides that from
/// `size_hint`).
#[derive(Debug, Clone, Copy)]
pub(crate) struct Bodiless;

fn empty(status: StatusCode) -> Response {
    let mut res = (status, unsized_body(Bytes::new())).into_response();
    res.extensions_mut().insert(Bodiless);
    res
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

/// `new URL(request.url, "http://localhost").pathname`: dot segments resolved, `\\` read as `/`.
fn pathname(path_and_query: &str) -> String {
    static BASE: LazyLock<url::Url> = LazyLock::new(|| url::Url::parse("http://localhost").expect("base url"));
    BASE.join(path_and_query).map(|u| u.path().to_string()).unwrap_or_else(|_| path_and_query.to_string())
}

/// Middleware: serve non-API paths from the web UI before auth/readiness.
pub(crate) async fn web_ui(State(app): State<Arc<App>>, req: Request, next: Next) -> Response {
    let Some(ui) = &app.web_ui else { return next.run(req).await };
    let path = pathname(req.uri().path_and_query().map(|p| p.as_str()).unwrap_or("/"));
    if is_api_path(&path) {
        return next.run(req).await;
    }
    ui.serve(req.method(), &path, req.headers())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pathname_like_whatwg_url() {
        assert_eq!(pathname("/"), "/");
        assert_eq!(pathname("/a/../_assets/x.js?v=1"), "/_assets/x.js");
        assert_eq!(pathname("/x/%2e%2e/api/info"), "/api/info");
        assert_eq!(pathname("/a b"), "/a%20b");
        assert_eq!(pathname("/settings#frag"), "/settings");
    }

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

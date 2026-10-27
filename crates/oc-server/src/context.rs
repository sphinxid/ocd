//! Per-request context handed to every [`Backend`](crate::Backend) method.

use std::sync::Arc;

use axum::http::HeaderMap;
use parking_lot::RwLock;

use crate::generated::{Scope, ServerInfo, ServerInfoCapabilities, ServerInfoPaths};

/// A requested location (`Location.Ref`): the directory string exactly as the client sent it
/// (no normalisation, spec 06 §3 / Q11).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct LocationRef {
    pub directory: String,
    pub workspace_id: Option<String>,
}

impl LocationRef {
    pub fn new(directory: impl Into<String>) -> Self {
        Self { directory: directory.into(), workspace_id: None }
    }
}

/// Context for one API call.
#[derive(Clone)]
pub struct RequestContext {
    /// The location resolved from the request (`?location[directory]`, then the
    /// `x-opencode-directory` header, then the server cwd).
    ///
    /// * Scope `L`: verified to be an existing directory; the success envelope echoes it.
    /// * Scope `S`/`F`/`H`: this is still the *request* location — the backend must use the
    ///   session's own location (and answer `SessionNotFoundError`/`LocationNotFoundError`).
    /// * Scope `G`: informational (e.g. `debug.location.evict` evicts exactly this).
    pub location: LocationRef,
    /// The operation's scope.
    pub scope: Scope,
    /// The OpenAPI operationId being served.
    pub operation: &'static str,
    /// Raw request headers.
    pub headers: HeaderMap,
    /// Server identity (version, pid, urls, tmp path).
    pub server: Arc<ServerMeta>,
}

impl std::fmt::Debug for RequestContext {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RequestContext")
            .field("location", &self.location)
            .field("scope", &self.scope)
            .field("operation", &self.operation)
            .finish_non_exhaustive()
    }
}

impl RequestContext {
    /// A context for tests / direct backend calls.
    pub fn for_test(directory: &str) -> Self {
        Self {
            location: LocationRef::new(directory),
            scope: Scope::G,
            operation: "test",
            headers: HeaderMap::new(),
            server: Arc::new(ServerMeta::new("test", "/tmp")),
        }
    }
}

/// Server identity shared by every request.
#[derive(Debug)]
pub struct ServerMeta {
    /// App version (`"unknown"` when not configured).
    pub version: String,
    /// OS process id.
    pub pid: u32,
    /// Global tmp directory (`paths.tmp`).
    pub tmp: String,
    /// Directory used when the request names no location (`process.cwd()` upstream).
    pub cwd: String,
    urls: RwLock<Vec<String>>,
    extra_urls: RwLock<Vec<String>>,
}

impl ServerMeta {
    pub fn new(version: impl Into<String>, tmp: impl Into<String>) -> Self {
        Self {
            version: version.into(),
            pid: std::process::id(),
            tmp: tmp.into(),
            cwd: std::env::current_dir().map(|p| p.to_string_lossy().into_owned()).unwrap_or_default(),
            urls: RwLock::new(Vec::new()),
            extra_urls: RwLock::new(Vec::new()),
        }
    }

    pub(crate) fn with_cwd(mut self, cwd: impl Into<String>) -> Self {
        self.cwd = cwd.into();
        self
    }

    /// Connection URLs: bound URLs followed by any extra (e.g. remote tunnel) URLs.
    pub fn urls(&self) -> Vec<String> {
        let mut out = self.urls.read().clone();
        for u in self.extra_urls.read().iter() {
            if !out.contains(u) {
                out.push(u.clone());
            }
        }
        out
    }

    pub(crate) fn set_urls(&self, urls: Vec<String>) {
        *self.urls.write() = urls;
    }

    /// Replace the extra URLs appended to `urls` (remote tunnel URLs while connected).
    pub fn set_extra_urls(&self, urls: Vec<String>) {
        *self.extra_urls.write() = urls;
    }

    /// `ServerInfo` as served by `GET /api/info`. `persistent_pty` = `None` omits
    /// `capabilities` (the pre-ready shape).
    pub fn info(&self, persistent_pty: Option<bool>) -> ServerInfo {
        ServerInfo {
            version: self.version.clone(),
            pid: i64::from(self.pid),
            urls: self.urls(),
            paths: ServerInfoPaths { tmp: self.tmp.clone() },
            capabilities: persistent_pty.map(|p| ServerInfoCapabilities { persistent_pty: Some(p) }),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `process.test.ts`: server info lists remote URLs alongside local addresses while they are available.
    #[test]
    fn info_lists_remote_urls_after_local_ones_while_available() {
        let meta = ServerMeta::new("2.1.4", "/tmp");
        meta.set_urls(vec!["http://127.0.0.1:4096".into()]);
        meta.set_extra_urls(vec!["https://example.opentunnel.xyz".into()]);
        assert_eq!(meta.info(None).urls, vec!["http://127.0.0.1:4096", "https://example.opentunnel.xyz"]);
        meta.set_extra_urls(vec![]);
        assert_eq!(meta.info(None).urls, vec!["http://127.0.0.1:4096"]);
    }
}

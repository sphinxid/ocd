#![allow(dead_code)]

use std::path::{Path, PathBuf};
use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use oc_core::app::App;
use serde_json::Value;
use tower::ServiceExt;

pub struct Harness {
    pub app: Arc<App>,
    /// The engine behind `router`.
    pub engine: Arc<oc_backend::Engine>,
    pub router: axum::Router,
    pub root: tempfile::TempDir,
    /// Default location (server cwd), a plain directory inside `root`.
    pub cwd: PathBuf,
}

pub struct Res {
    pub status: StatusCode,
    pub body: bytes::Bytes,
    pub headers: axum::http::HeaderMap,
}

impl Res {
    pub fn json(&self) -> Value {
        serde_json::from_slice(&self.body)
            .unwrap_or_else(|e| panic!("json body ({e}) status {}: {:?}", self.status, String::from_utf8_lossy(&self.body)))
    }
}

pub fn global(root: &Path) -> oc_util::Global {
    let g = |p: &str| {
        let d = root.join(p);
        std::fs::create_dir_all(&d).unwrap();
        d
    };
    oc_util::Global {
        home: g("home"),
        data: g("data"),
        cache: g("cache"),
        config: g("config"),
        state: g("state"),
        tmp: g("tmp"),
        bin: g("bin"),
        log: g("log"),
        repos: g("repos"),
    }
}

pub fn harness() -> Harness {
    // SAFETY: tests only ever set this to the same value.
    unsafe { std::env::set_var("OPENCODE_DISABLE_MODELS_FETCH", "1") };
    let root = tempfile::tempdir().unwrap();
    let root_path = std::fs::canonicalize(root.path()).unwrap();
    let cwd = root_path.join("work");
    std::fs::create_dir_all(&cwd).unwrap();
    let db = oc_storage::Database::memory().unwrap();
    let app = App::new(global(&root_path), db, "1.2.3");
    let opts = oc_server::ServerOptions {
        cwd: Some(cwd.display().to_string()),
        version: "1.2.3".into(),
        ready: true,
        ..Default::default()
    };
    let engine = oc_backend::Engine::new(app.clone());
    let (router, _handle) = oc_server::router(engine.clone() as Arc<dyn oc_server::Backend>, &opts).unwrap();
    Harness { app, engine, router, root, cwd }
}

pub fn enc(s: &str) -> String {
    let mut out = String::new();
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || b"-_.~".contains(&b) {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

impl Harness {
    pub fn dir(&self) -> String {
        self.cwd.display().to_string()
    }

    pub async fn req(&self, method: &str, uri: &str, body: Option<Value>) -> Res {
        self.req_in(None, method, uri, body).await
    }

    /// A request at location `directory` (`x-opencode-directory`), or the server cwd.
    pub async fn req_in(&self, directory: Option<&str>, method: &str, uri: &str, body: Option<Value>) -> Res {
        let mut b = Request::builder().method(method).uri(uri);
        if let Some(directory) = directory {
            b = b.header("x-opencode-directory", directory);
        }
        let body = match body {
            Some(v) => {
                b = b.header("content-type", "application/json");
                Body::from(serde_json::to_vec(&v).unwrap())
            }
            None => Body::empty(),
        };
        let res = self.router.clone().oneshot(b.body(body).unwrap()).await.unwrap();
        let status = res.status();
        let headers = res.headers().clone();
        let body = axum::body::to_bytes(res.into_body(), usize::MAX).await.unwrap();
        Res { status, body, headers }
    }

    pub async fn get(&self, uri: &str) -> Res {
        self.req("GET", uri, None).await
    }

    pub async fn post(&self, uri: &str, body: Value) -> Res {
        self.req("POST", uri, Some(body)).await
    }

    /// Create a session at the default location; returns its id.
    pub async fn session(&self) -> String {
        let r = self.post("/api/session", serde_json::json!({})).await;
        assert_eq!(r.status, 200, "{:?}", String::from_utf8_lossy(&r.body));
        r.json()["data"]["id"].as_str().unwrap().to_string()
    }

    /// Deliver every pending inbox item (what the runner does at a step boundary).
    pub async fn promote(&self, session: &str) {
        oc_core::inbox::promote(&self.app.bus, &self.app.sessions.locks, session, oc_core::inbox::Scope::Input)
            .await
            .unwrap();
    }
}

pub fn git_available() -> bool {
    std::process::Command::new("git").arg("--version").output().is_ok_and(|o| o.status.success())
}

pub fn git(dir: &Path, args: &[&str]) {
    let out = std::process::Command::new("git")
        .args(["-c", "user.email=t@example.com", "-c", "user.name=t", "-c", "init.defaultBranch=main"])
        .args(args)
        .current_dir(dir)
        .output()
        .unwrap();
    assert!(out.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&out.stderr));
}

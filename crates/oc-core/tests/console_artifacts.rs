//! The `console` artifact provider (2.3.1 `packages/core/test/plugin/provider-opencode-artifacts.test.ts`, #63929),
//! against a fake Console in-process that stores blobs by digest and answers `prepare` with the ones it lacks.
//!
//! The provider is offered the way the runner offers it once the Console config carries the header
//! (`console_artifacts::sync`); the config fetch and its periodic check are covered side by side in
//! scripts/parity/console231.py.

use std::path::Path;
use std::sync::Arc;

use axum::extract::State;
use axum::http::{Method, StatusCode, Uri};
use axum::response::{IntoResponse, Response};
use oc_ai::credentials::integration::Integrations;
use oc_ai::credentials::store::{CreateCredential, CredentialStore, MemoryCredentialStore};
use oc_ai::credentials::{CredentialValue, map_env};
use oc_core::app::App;
use oc_core::artifact::{self, CreateInput, UpdateInput};
use oc_core::console_artifacts::{self, Current};
use oc_core::location::Location;
use parking_lot::Mutex;
use serde_json::{Value, json};

#[derive(Default)]
struct ConsoleState {
    blobs: Vec<String>,
    uploads: Vec<String>,
    prepared: Vec<Value>,
    published: Vec<(String, Value)>,
    deleted: Vec<String>,
    headers: Vec<(String, String)>,
    /// The status and body Console answers the next publish with instead, when set.
    refuse: Option<(u16, Value)>,
}

#[derive(Clone)]
struct FakeConsole {
    state: Arc<Mutex<ConsoleState>>,
    server: String,
}

impl FakeConsole {
    async fn start() -> FakeConsole {
        let state = Arc::new(Mutex::new(ConsoleState::default()));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let server = format!("http://{}", listener.local_addr().unwrap());
        let console = FakeConsole { state, server };
        let app = axum::Router::new().fallback(routes).with_state(console.clone());
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        console
    }
}

async fn routes(State(console): State<FakeConsole>, method: Method, uri: Uri, headers: axum::http::HeaderMap, body: axum::body::Bytes) -> Response {
    let path = uri.path().to_string();
    let slug = path.rsplit('/').next().unwrap_or_default().to_string();
    let mut state = console.state.lock();
    state.headers = headers.iter().map(|(k, v)| (k.to_string(), v.to_str().unwrap_or_default().to_string())).collect();
    if path == "/api/artifacts/prepare" {
        let body: Value = serde_json::from_slice(&body).unwrap();
        let missing: Vec<Value> =
            body["files"].as_array().unwrap().iter().map(|f| f["sha256"].clone()).filter(|sha| !state.blobs.contains(&sha.as_str().unwrap().to_string())).collect();
        state.prepared.push(body);
        return axum::Json(json!({ "missing": missing })).into_response();
    }
    if path.starts_with("/api/artifacts/blobs/") {
        state.uploads.push(slug.clone());
        state.blobs.push(slug.clone());
        return axum::Json(json!({ "sha256": slug, "bytes": body.len() })).into_response();
    }
    if path.starts_with("/api/artifacts/") && method == Method::PUT {
        if let Some((status, answer)) = state.refuse.take() {
            return (StatusCode::from_u16(status).unwrap(), axum::Json(answer)).into_response();
        }
        let body: Value = serde_json::from_slice(&body).unwrap();
        state.published.push((slug.clone(), body));
        return axum::Json(json!({ "url": format!("https://opencode.ai/console/org_1/artifacts/{slug}"), "watching": false })).into_response();
    }
    if path.starts_with("/api/artifacts/") && method == Method::DELETE {
        state.deleted.push(slug);
        return StatusCode::NO_CONTENT.into_response();
    }
    (StatusCode::NOT_FOUND, "Not found").into_response()
}

fn global(root: &Path) -> oc_util::Global {
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

struct H {
    app: Arc<App>,
    location: Location,
    console: FakeConsole,
    integrations: Arc<Integrations>,
    _tmp: tempfile::TempDir,
}

impl Drop for H {
    fn drop(&mut self) {
        console_artifacts::sync(&self.location.key, None);
    }
}

/// Signs in to the fake Console with an API key, in a project where artifacts are on.
async fn start() -> H {
    unsafe { std::env::set_var("OPENCODE_DISABLE_MODELS_FETCH", "1") };
    let tmp = tempfile::tempdir().unwrap();
    let root = oc_util::fs::canonicalize(tmp.path()).unwrap();
    let project = root.join("project");
    std::fs::create_dir_all(&project).unwrap();
    std::fs::write(project.join("opencode.json"), json!({"experimental": {"artifacts": true}}).to_string()).unwrap();
    let app = App::new(global(&root.join("g")), oc_storage::Database::memory().unwrap(), "2.3.1");
    let location = Location::load(&app.global, &app.bus, &project).await.unwrap();
    let console = FakeConsole::start().await;
    let store = MemoryCredentialStore::shared();
    let key = CredentialValue::Key {
        key: "secret".into(),
        metadata: Some(json!({"server": console.server}).as_object().unwrap().clone()),
    };
    store.create(CreateCredential::new("opencode", key)).await.unwrap();
    let integrations = Integrations::with(store, map_env(std::iter::empty::<(String, String)>()), Arc::new(|| 1_000));
    H { app, location, console, integrations, _tmp: tmp }
}

impl H {
    /// What the runner does once the Console config carries the header: offer the provider through the active
    /// connection, promising comment wakes while watches run.
    async fn offer(&self, wakes: bool) -> bool {
        let connection = self.integrations.active("opencode").await.unwrap().unwrap();
        console_artifacts::sync(
            &self.location.key,
            Some(Current { connection, wakes, integrations: self.integrations.clone(), user_agent: "opencode/test".into() }),
        )
    }

    fn service(&self) -> artifact::Service<'_> {
        artifact::Service::new(&self.app, &self.location)
    }

    async fn session(&self) -> String {
        let at = oc_schema::types::LocationRef { directory: self.location.key.clone(), workspace_id: None };
        self.app.sessions.create(oc_core::session::CreateInput { location: Some(at), ..Default::default() }).await.unwrap().id
    }
}

fn files() -> Vec<(String, String)> {
    vec![("index.html".into(), "<h1>Hi</h1>".into()), ("app.js".into(), "console.log(1)".into())]
}

#[tokio::test]
async fn adds_no_console_provider_while_the_console_config_doesnt_advertise_artifacts() {
    let h = start().await;
    let providers = h.service().providers().await.unwrap();
    assert_eq!(providers, [json!({"id": "local", "name": "Local", "capabilities": ["static"], "default": true})]);
}

#[tokio::test]
async fn makes_console_the_default_provider_while_advertised_and_removes_it_when_the_header_goes() {
    let h = start().await;
    assert!(h.offer(true).await);
    assert!(!h.offer(true).await);
    let providers = h.service().providers().await.unwrap();
    assert_eq!(
        providers,
        [
            json!({"id": "console", "name": "OpenCode Console", "capabilities": ["static"], "default": true}),
            json!({"id": "local", "name": "Local", "capabilities": ["static"], "default": false}),
        ]
    );
    let registry = h.service().registry().await;
    let (default, _) = registry.prompts();
    assert!(default.prompt.contains("Comments wake this session."));
    assert!(artifact::create_description(&registry).contains(
        "Default provider (console [static]): Artifacts are private pages the workspace's members open and comment on inside OpenCode Console. Each needs an index.html. Comments wake this session.\nOther active providers (pass provider to override): local [static]."
    ));
    // `artifact.updated` compares the providers with their selection.
    assert_eq!(registry.snapshot()[0], json!({"id": "console", "name": "OpenCode Console", "capabilities": ["static"], "selection": "console"}));

    assert!(console_artifacts::sync(&h.location.key, None));
    let providers = h.service().providers().await.unwrap();
    assert_eq!(providers.iter().map(|p| (p["id"].clone(), p["default"].clone())).collect::<Vec<_>>(), [(json!("local"), json!(true))]);
}

#[tokio::test]
async fn publishes_with_the_session_and_returns_consoles_url_an_update_keeps_the_slug_and_uploads_only_new_files() {
    let h = start().await;
    h.offer(true).await;
    let session = h.session().await;
    let created = h
        .service()
        .create(CreateInput { title: "Q3 Report!".into(), files: files(), session_id: Some(session.clone()), ..Default::default() })
        .await
        .unwrap();
    let slug = h.console.state.lock().published[0].0.clone();
    assert!(regex::Regex::new(r"^q3-report-[a-z0-9]{6}$").unwrap().is_match(&slug), "{slug}");
    assert_eq!(created.provider, "console");
    assert_eq!(created.url, format!("https://opencode.ai/console/org_1/artifacts/{slug}"));
    {
        let state = h.console.state.lock();
        assert_eq!(state.uploads.len(), 2);
        assert!(state.prepared[0].get("replace").is_none());
        let sha = |text: &str| hex::encode(<sha2::Sha256 as sha2::Digest>::digest(text.as_bytes()));
        assert_eq!(
            state.prepared[0],
            json!({"slug": slug, "files": [
                {"path": "index.html", "sha256": sha("<h1>Hi</h1>"), "bytes": 11},
                {"path": "app.js", "sha256": sha("console.log(1)"), "bytes": 14},
            ]})
        );
        assert_eq!(
            state.published[0].1,
            json!({"title": "Q3 Report!", "files": [
                {"path": "index.html", "sha256": sha("<h1>Hi</h1>")},
                {"path": "app.js", "sha256": sha("console.log(1)")},
            ], "sessionID": session})
        );
        let header = |name: &str| state.headers.iter().find(|(k, _)| k == name).map(|(_, v)| v.clone());
        assert_eq!(header("authorization").as_deref(), Some("Bearer secret"));
        assert_eq!(header("user-agent").as_deref(), Some("opencode/test"));
        assert_eq!(header("accept").as_deref(), Some("application/json"));
    }

    let mut next = files();
    next[0].1 = "<h1>Bye</h1>".into();
    let updated = h.service().update(&created.id, UpdateInput { title: Some("Q4 Report".into()), files: next, ..Default::default() }).await.unwrap();
    assert_eq!(updated.url, format!("https://opencode.ai/console/org_1/artifacts/{slug}"));
    {
        let state = h.console.state.lock();
        assert_eq!(state.uploads.len(), 3);
        assert_eq!((&state.prepared[1]["slug"], &state.prepared[1]["replace"]), (&json!(slug), &json!(true)));
        assert_eq!(state.published[1].0, slug);
        assert_eq!((&state.published[1].1["title"], &state.published[1].1["replace"]), (&json!("Q4 Report"), &json!(true)));
        // The artifact keeps its session, which still wakes.
        assert_eq!(state.published[1].1["sessionID"], json!(session));
    }

    // Console has no API that returns an artifact's files.
    let error = h.service().get(&created.id).await.unwrap_err();
    assert_eq!(error.message(), "Artifact provider console failed: OpenCode Console can't return an artifact's files yet");

    h.service().remove(&created.id).await.unwrap();
    assert_eq!(h.console.state.lock().deleted, [slug]);
}

#[tokio::test]
async fn gives_two_artifacts_with_one_title_their_own_console_artifacts() {
    let h = start().await;
    h.offer(true).await;
    for _ in 0..2 {
        h.service().create(CreateInput { title: "Report".into(), files: files(), ..Default::default() }).await.unwrap();
    }
    let slugs: Vec<String> = h.console.state.lock().published.iter().map(|(slug, _)| slug.clone()).collect();
    assert!(regex::Regex::new(r"^report-[a-z0-9]{6}$").unwrap().is_match(&slugs[0]), "{slugs:?}");
    assert_ne!(slugs[0], slugs[1]);
    // The same bytes were stored by the first: the second uploads nothing.
    assert_eq!(h.console.state.lock().uploads.len(), 2);
}

#[tokio::test]
async fn without_event_watches_publishes_without_the_session_and_promises_no_wake() {
    let h = start().await;
    h.offer(false).await;
    let session = h.session().await;
    let (default, _) = h.service().registry().await.prompts();
    assert!(!default.prompt.contains("wake"));
    h.service().create(CreateInput { title: "Demo".into(), files: files(), session_id: Some(session), ..Default::default() }).await.unwrap();
    assert!(h.console.state.lock().published[0].1.get("sessionID").is_none());
}

#[tokio::test]
async fn names_whoever_shared_an_artifact_of_the_same_name_and_keeps_local_publishing_available() {
    let h = start().await;
    h.offer(true).await;
    h.console.state.lock().refuse = Some((409, json!({"message": "taken", "owner": "Ada"})));
    let error = h.service().create(CreateInput { title: "Report".into(), files: files(), ..Default::default() }).await.unwrap_err();
    let message = error.message();
    assert!(message.starts_with("Artifact provider console failed: Ada already shared an OpenCode Console artifact named report-"), "{message}");
    assert!(message.ends_with("; give this one another title"), "{message}");
    h.console.state.lock().refuse = Some((404, json!({})));
    let error = h.service().create(CreateInput { title: "Report".into(), files: files(), ..Default::default() }).await.unwrap_err();
    assert_eq!(error.message(), "Artifact provider console failed: Artifacts are off for this OpenCode Console workspace");
    h.console.state.lock().refuse = Some((500, json!({"message": "Console is down"})));
    let error = h.service().create(CreateInput { title: "Report".into(), files: files(), ..Default::default() }).await.unwrap_err();
    assert_eq!(error.message(), "Artifact provider console failed: Console is down");
    let local = h
        .service()
        .create(CreateInput { title: "Report".into(), files: files(), provider: Some("local".into()), ..Default::default() })
        .await
        .unwrap();
    assert_eq!(local.provider, "local");
}

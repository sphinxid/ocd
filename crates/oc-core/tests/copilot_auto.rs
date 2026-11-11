//! GitHub Copilot on 2.2.7 (`packages/core/test/plugin/provider-github-copilot.test.ts`): the account's API
//! endpoint from `/copilot_internal/user` (the stored one when the lookup fails), the native packages of the synced
//! catalog, and Copilot Auto (the `auto` model, the `/auto` router with the session's prompt, the pick kept per
//! session with its `Copilot-Session-Token`, a new route after compaction, the chat fallback, and
//! `experimental.model.resolve` choosing a model hidden from the picker). Every request goes to a local mock.

use std::sync::Arc;

use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::routing::{get, post};
use axum::{Json, Router};
use oc_ai::credentials::store::CreateCredential;
use oc_ai::credentials::{CredentialValue, OAuthValue};
use oc_core::models::Models;
use oc_core::plugin_request::{self, Kind};
use oc_core::plugins::Plugins;
use oc_schema::types::ModelRef;
use parking_lot::Mutex;
use serde_json::{json, Value};

const KEY: &str = "/loc";

#[derive(Default)]
struct Api {
    /// `(method path, authorization, body)` of every request.
    requests: Mutex<Vec<(String, Option<String>, Value)>>,
    picks: Mutex<Vec<(u16, Value)>>,
    user_fails: Mutex<bool>,
    base: Mutex<String>,
}

fn remote(id: &str, extra: Value) -> Value {
    let mut model = json!({
        "model_picker_enabled": false, "id": id, "name": id, "version": id, "supported_endpoints": ["/chat/completions"],
        "capabilities": {"family": id, "limits": {"max_prompt_tokens": 128000, "max_output_tokens": 64000}, "supports": {"tool_calls": true}},
    });
    for (k, v) in extra.as_object().unwrap() {
        model[k] = v.clone();
    }
    model
}

async fn serve(api: Arc<Api>) -> String {
    async fn user(State(api): State<Arc<Api>>, headers: HeaderMap) -> (StatusCode, Json<Value>) {
        let auth = headers.get("authorization").and_then(|v| v.to_str().ok()).map(String::from);
        api.requests.lock().push(("GET /copilot_internal/user".into(), auth, Value::Null));
        if *api.user_fails.lock() {
            return (StatusCode::NOT_FOUND, Json(json!({})));
        }
        let base = api.base.lock().clone();
        (StatusCode::OK, Json(json!({"chat_enabled": true, "endpoints": {"api": format!("{base}/live//")}})))
    }
    async fn models(State(api): State<Arc<Api>>, headers: HeaderMap) -> Json<Value> {
        let auth = headers.get("authorization").and_then(|v| v.to_str().ok()).map(String::from);
        api.requests.lock().push(("GET /models".into(), auth, Value::Null));
        Json(json!({"data": [
            remote("routed", json!({"billing": {"auto_discount": 0.1}})),
            remote("other", json!({"billing": {"auto_discount": 0.1}, "supported_endpoints": ["/v1/messages"]})),
            remote("fallback", json!({"is_chat_fallback": true})),
        ]}))
    }
    async fn auto(State(api): State<Arc<Api>>, headers: HeaderMap, Json(body): Json<Value>) -> (StatusCode, Json<Value>) {
        let auth = headers.get("authorization").and_then(|v| v.to_str().ok()).map(String::from);
        assert_eq!(headers.get("x-github-api-version").and_then(|v| v.to_str().ok()), Some("2026-08-01"));
        api.requests.lock().push(("POST /auto".into(), auth, body));
        let (status, answer) = api.picks.lock().remove(0);
        (StatusCode::from_u16(status).unwrap(), Json(answer))
    }
    let app = Router::new()
        .route("/copilot_internal/user", get(user))
        .route("/live/models", get(models))
        .route("/stored/models", get(models))
        .route("/live/auto", post(auto))
        .with_state(api.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    *api.base.lock() = base.clone();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    base
}

struct Env {
    _home: tempfile::TempDir,
    models: Arc<Models>,
    plugins: Arc<Plugins>,
}

async fn setup(api: &Arc<Api>, base: &str) -> Env {
    let home = tempfile::tempdir().unwrap();
    // SAFETY: every test of this binary sets the same values, before any client reads them.
    unsafe {
        std::env::set_var("OPENCODE_DISABLE_MODELS_FETCH", "1");
        std::env::set_var("OPENCODE_TEST_HOME", home.path());
        std::env::remove_var("GITHUB_TOKEN");
    }
    let root = home.path();
    let global = Arc::new(oc_util::Global {
        home: root.to_path_buf(),
        data: root.join("data"),
        cache: root.join("cache"),
        config: root.join("config"),
        state: root.join("state"),
        tmp: root.join("tmp"),
        bin: root.join("cache/bin"),
        log: root.join("data/log"),
        repos: root.join("data/repos"),
    });
    let db = oc_storage::Database::memory().unwrap();
    let bus = oc_core::bus::Bus::new(db.clone());
    let models = Arc::new(Models::new(db.clone(), bus.clone(), "latest", "1.2.3"));
    let plugins = Plugins::new(global, db, bus, "1.2.3");
    models.set_plugins(plugins.clone());
    models.integrations_for(Some(KEY));
    // The account lookup goes to the mock (`OCD_TEST_GITHUB_API` for the binary).
    plugins.copilot.set_github_api(Some(base.to_string()));
    let _ = api;
    Env { _home: home, models, plugins }
}

async fn sign_in(env: &Env, token: &str, base: &str) {
    let store = env.models.integrations.store();
    let value: CredentialValue = OAuthValue {
        method_id: "device".into(),
        refresh: token.into(),
        access: token.into(),
        expires: 0,
        metadata: Some(json!({"apiEndpoint": format!("{base}/stored")}).as_object().unwrap().clone()),
    }
    .into_value();
    store.create(CreateCredential { id: None, integration_id: "github-copilot".into(), value, label: None, activate: Some(true) }).await.unwrap();
}

fn auto() -> ModelRef {
    ModelRef { id: "auto".into(), provider_id: "github-copilot".into(), variant: None }
}

async fn resolve(env: &Env, kind: Kind, session: &str) -> String {
    plugin_request::resolve_model(&env.plugins, KEY, session, "build", kind, &auto()).await.unwrap().id
}

async fn session_token(env: &Env, model: &str) -> Option<String> {
    let resolved = env.models.catalog_at(&[], Some(KEY)).await.unwrap().resolve("github-copilot", model, None, None).unwrap();
    let mut req = oc_ai::LLMRequest::new(resolved.language_model.clone());
    let reference = ModelRef { id: model.into(), provider_id: "github-copilot".into(), variant: None };
    plugin_request::model_request(&env.plugins, KEY, Kind::Primary, "ses_1", "build", &reference, &mut req).await.unwrap();
    req.http.and_then(|h| h.headers).and_then(|h| h.get("Copilot-Session-Token").cloned())
}

#[tokio::test]
async fn copilot_auto_routes_each_session_and_keeps_the_pick_until_compaction() {
    let api = Arc::new(Api::default());
    let base = serve(api.clone()).await;
    let env = setup(&api, &base).await;
    sign_in(&env, "gho_first", &base).await;
    *api.picks.lock() = vec![
        (200, json!({"session_token": "token-routed", "expires_at": 4_000_000_000u64, "selected_model": {"id": "routed"}})),
        (200, json!({"session_token": "token-other", "expires_at": 4_000_000_000u64, "selected_model": {"id": "other"}})),
        (503, json!({})),
    ];

    // The account's endpoint (trailing slashes removed) serves the list, and the catalog runs on native packages.
    let catalog = env.models.catalog_at(&[], Some(KEY)).await.unwrap();
    let copilot = catalog.record("github-copilot").expect("copilot");
    let live = format!("{base}/live");
    let auto_model = &copilot.models["auto"];
    assert_eq!(auto_model.package.as_deref(), Some("@opencode/ai/providers/openai-compatible"));
    assert_eq!(auto_model.settings.as_ref().unwrap()["baseURL"], json!(live));
    assert_eq!(copilot.models["other"].package.as_deref(), Some("@opencode/ai/providers/anthropic"));
    assert_eq!(copilot.models["other"].settings.as_ref().unwrap()["baseURL"], json!(format!("{live}/v1")));
    // Free plans hide every model from the picker; Auto is the one they can select.
    assert!(!copilot.models["routed"].enabled);
    let paths: Vec<String> = api.requests.lock().iter().map(|r| r.0.clone()).collect();
    assert_eq!(paths, ["GET /copilot_internal/user", "GET /models"]);
    assert_eq!(api.requests.lock()[0].1.as_deref(), Some("Bearer gho_first"));

    // Titles use Copilot's utility model, so they never route.
    assert_eq!(resolve(&env, Kind::Title, "ses_1").await, "auto");
    assert_eq!(resolve(&env, Kind::Primary, "ses_1").await, "routed");
    assert_eq!(resolve(&env, Kind::Primary, "ses_1").await, "routed");
    assert_eq!(resolve(&env, Kind::Generate, "ses_1").await, "routed");
    let autos: Vec<(Option<String>, Value)> = api.requests.lock().iter().filter(|r| r.0 == "POST /auto").map(|r| (r.1.clone(), r.2.clone())).collect();
    assert_eq!(autos, [(Some("Bearer gho_first".to_string()), json!({"prompt": ""}))]);
    assert_eq!(session_token(&env, "routed").await.as_deref(), Some("token-routed"));
    // Copilot rejects the token for any model but the one it was issued for.
    assert_eq!(session_token(&env, "other").await, None);

    // The request after a compaction routes again; the compaction request itself keeps the token.
    env.plugins.copilot.compacted(KEY, "ses_1");
    assert_eq!(session_token(&env, "routed").await.as_deref(), Some("token-routed"));
    assert_eq!(resolve(&env, Kind::Primary, "ses_1").await, "other");
    assert_eq!(session_token(&env, "other").await.as_deref(), Some("token-other"));

    // A router failure serves the chat fallback, without a token.
    assert_eq!(resolve(&env, Kind::Primary, "ses_2").await, "fallback");
    assert_eq!(api.requests.lock().iter().filter(|r| r.0 == "POST /auto").count(), 3);

    // `experimental.model.resolve` serves the session with the routed model, though the picker hides it.
    let catalog = env.models.catalog_at(&[], Some(KEY)).await.unwrap();
    let resolved = env.models.resolve_for_session(Some(KEY), &catalog, "ses_1", Some(&auto()), None, Kind::Primary, "build").await.unwrap();
    assert_eq!((resolved.model.provider_id.as_str(), resolved.model.id.as_str()), ("github-copilot", "other"));
}

#[tokio::test]
async fn the_stored_endpoint_serves_when_the_account_lookup_fails() {
    let api = Arc::new(Api::default());
    let base = serve(api.clone()).await;
    *api.user_fails.lock() = true;
    let env = setup(&api, &base).await;
    sign_in(&env, "gho_stored", &base).await;
    let catalog = env.models.catalog_at(&[], Some(KEY)).await.unwrap();
    let copilot = catalog.record("github-copilot").expect("copilot");
    assert_eq!(copilot.models["auto"].settings.as_ref().unwrap()["baseURL"], json!(format!("{base}/stored")));
    let paths: Vec<String> = api.requests.lock().iter().map(|r| r.0.clone()).collect();
    assert_eq!(paths, ["GET /copilot_internal/user", "GET /models"]);
}

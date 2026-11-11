//! OpenCode Console workspace config and usage limits (2.2 `packages/core/test/plugin/provider-opencode.test.ts`,
//! `usage-limits.test.ts`), against fake Console answers.

use async_trait::async_trait;
use oc_ai::credentials::integration::{ConnectionStatus, Integrations};
use oc_ai::credentials::store::{CreateCredential, CredentialStore, MemoryCredentialStore};
use oc_ai::credentials::{map_env, CredentialValue, OAuthValue};
use oc_core::console_config::{self, LoadError, Snapshot};
use oc_core::shared_cache::SharedCache;
use std::time::Duration;
use oc_core::usage_limits::{self, Http, Response};
use parking_lot::Mutex;
use serde_json::{json, Value};
use std::sync::Arc;

/// Answers by URL suffix; records requests.
struct Fake {
    routes: Vec<(&'static str, u16, Value)>,
    requests: Mutex<Vec<(String, Vec<(String, String)>)>>,
}

impl Fake {
    fn new(routes: Vec<(&'static str, u16, Value)>) -> Self {
        Self { routes, requests: Mutex::new(vec![]) }
    }
}

#[async_trait]
impl Http for Fake {
    async fn get(&self, url: &str, headers: &[(String, String)]) -> Result<Response, String> {
        self.requests.lock().push((url.to_string(), headers.to_vec()));
        let (_, status, body) = self.routes.iter().find(|(suffix, _, _)| url.ends_with(suffix)).ok_or("no route")?;
        Ok(Response { status: *status, body: if body.is_string() { body.as_str().unwrap().to_string() } else { body.to_string() } })
    }
}

/// `console_config::load` without sharing fetches between calls (a fresh cache each time).
async fn load(integrations: &Integrations, http: &Fake, ua: &str, previous: &Snapshot) -> Snapshot {
    let shared = Arc::new(Fake::new(http.routes.clone()));
    let out = console_config::load_with(&Arc::new(SharedCache::default()), Duration::ZERO, integrations, shared.clone(), ua, previous).await;
    http.requests.lock().extend(shared.requests.lock().drain(..));
    out
}

fn signed_in() -> CredentialValue {
    OAuthValue {
        method_id: "device".into(),
        refresh: "console-refresh".into(),
        access: "console-access".into(),
        expires: u64::MAX / 2,
        metadata: Some(
            json!({"server": "https://dev.opencode.ai/console", "orgID": "wrk_anomaly", "orgName": "Anomaly"})
                .as_object()
                .unwrap()
                .clone(),
        ),
    }
    .into_value()
}

async fn location(credential: Option<CredentialValue>) -> Arc<Integrations> {
    let store = MemoryCredentialStore::shared();
    if let Some(value) = credential {
        store.create(CreateCredential::new("opencode", value)).await.unwrap();
    }
    let integrations = Integrations::with(store, map_env(std::iter::empty::<(String, String)>()), Arc::new(|| 1_000));
    integrations.update("opencode", |r| r.name = "OpenCode Console".into());
    integrations
}

fn header<'a>(headers: &'a [(String, String)], name: &str) -> Option<&'a str> {
    headers.iter().find(|(k, _)| k.eq_ignore_ascii_case(name)).map(|(_, v)| v.as_str())
}

// ---------------------------------------------------------------------------------------------
// Usage limits
// ---------------------------------------------------------------------------------------------

fn budget() -> Value {
    json!({
        "policyID": "policy_engineering", "ruleID": "rule_astra", "limitDollars": 20, "spentDollars": 15,
        "scope": "each", "per": "model", "modelCount": 1, "coverage": "models",
        "models": [{"providerID": "opencode", "connectionID": "conn_managed", "modelID": "gpt-6-astra", "name": "GPT-6 Astra", "spentDollars": 15}],
        "vendors": [{"id": "openai", "name": "OpenAI"}],
        "inputPriceAtLeast": 3, "picked": false, "shared": false, "resetsAt": "2026-11-01T00:00:00.000Z"
    })
}

fn meter(status: &str, percent: f64) -> Value {
    json!({"status": status, "percent": percent, "resetsAt": "2026-10-24T17:00:00.000Z"})
}

#[tokio::test]
async fn reads_the_members_go_allowance_and_budgets_from_the_console_they_signed_in_to() {
    let integrations = location(Some(signed_in())).await;
    let http = Fake::new(vec![
        ("/zen/go/v1/usage", 200, json!({"usage": {"rolling": meter("ok", 42.0), "weekly": meter("ok", 18.0), "monthly": meter("rate-limited", 100.0)}})),
        ("/api/policies/current-budgets", 200, json!([budget()])),
    ]);
    let limits = usage_limits::get(&integrations, &http, "ua").await.unwrap().unwrap();
    let reset = 1792861200000i64;
    assert_eq!(
        limits,
        json!({
            "workspace": {"id": "wrk_anomaly", "name": "Anomaly", "usageURL": "https://dev.opencode.ai/console/wrk_anomaly/activity/overview"},
            "go": {
                "rolling": {"percent": 42, "limited": false, "resetsAt": reset},
                "weekly": {"percent": 18, "limited": false, "resetsAt": reset},
                "monthly": {"percent": 100, "limited": true, "resetsAt": reset}
            },
            "budgets": [{
                "id": "rule_astra", "limit": 20, "spent": 15, "scope": "each", "per": "model", "coverage": "models",
                "models": [{"providerID": "opencode", "modelID": "gpt-6-astra", "name": "GPT-6 Astra", "spent": 15}],
                "vendors": ["OpenAI"], "inputPriceAtLeast": 3, "picked": false, "shared": false, "resetsAt": 1793491200000i64
            }]
        })
    );
    // Console reads the workspace from x-org-id; inference, which serves Go, from x-opencode-org-id.
    let mut requests = http.requests.lock().clone();
    requests.sort_by(|a, b| a.0.cmp(&b.0));
    assert_eq!(requests[0].0, "https://dev.opencode.ai/console/api/policies/current-budgets");
    assert_eq!(header(&requests[0].1, "authorization"), Some("Bearer console-access"));
    assert_eq!(header(&requests[0].1, "x-org-id"), Some("wrk_anomaly"));
    assert_eq!(requests[1].0, "https://dev.opencode.ai/zen/go/v1/usage");
    assert_eq!(header(&requests[1].1, "authorization"), Some("Bearer console-access"));
    assert_eq!(header(&requests[1].1, "x-opencode-org-id"), Some("wrk_anomaly"));
}

#[tokio::test]
async fn has_no_go_allowance_or_budgets_to_show_where_console_refuses_them() {
    let integrations = location(Some(signed_in())).await;
    let http = Fake::new(vec![("", 403, json!({"type": "error"}))]);
    let limits = usage_limits::get(&integrations, &http, "ua").await.unwrap().unwrap();
    assert_eq!(
        limits,
        json!({
            "workspace": {"id": "wrk_anomaly", "name": "Anomaly", "usageURL": "https://dev.opencode.ai/console/wrk_anomaly/activity/overview"},
            "budgets": []
        })
    );
}

#[tokio::test]
async fn has_nothing_to_show_for_a_location_that_isnt_signed_in_to_opencode_console() {
    let http = Fake::new(vec![("", 200, json!([budget()]))]);
    assert_eq!(usage_limits::get(&*location(None).await, &http, "ua").await.unwrap(), None);
    // A credential without a workspace has nothing to show either.
    let key = CredentialValue::key("sk-console");
    assert_eq!(usage_limits::get(&*location(Some(key)).await, &http, "ua").await.unwrap(), None);
    assert!(http.requests.lock().is_empty());
}

#[tokio::test]
async fn reports_a_console_that_cant_answer_as_unavailable() {
    let integrations = location(Some(signed_in())).await;
    let http = Fake::new(vec![
        ("/current-budgets", 502, json!("upstream down")),
        ("/zen/go/v1/usage", 403, json!({"type": "error"})),
    ]);
    let error = usage_limits::get(&integrations, &http, "ua").await.unwrap_err();
    assert_eq!(
        error.message,
        "OpenCode Console budgets are unavailable: StatusCode: non 2xx status code (502 GET https://dev.opencode.ai/console/api/policies/current-budgets)"
    );
}

// ---------------------------------------------------------------------------------------------
// Console workspace config
// ---------------------------------------------------------------------------------------------

#[tokio::test(start_paused = true)]
async fn loads_the_workspace_config_as_an_integration_document() {
    let integrations = location(Some(signed_in())).await;
    let http = Fake::new(vec![(
        "/api/v2/config",
        200,
        json!({
            "providers": {"anthropic": {"env": ["X"], "settings": {"apiKey": "leak", "baseURL": "https://gateway.test"}}},
            "mcp": {"servers": {"docs": {"type": "remote", "url": "https://mcp.test"}, "local": {"type": "local", "command": ["sh"]}}},
            "policies": [{"action": "provider.use", "resource": "openai", "effect": "deny"}],
            "websearch": {"provider": "opencode"},
            "plugins": ["ignored"]
        }),
    )]);
    let snapshot = load(&integrations, &http, "opencode/latest/1.0.0/opencode", &Snapshot::default()).await;
    assert_eq!(snapshot.organization.as_deref(), Some("Anomaly"));
    assert!(snapshot.offers_websearch());
    let (url, headers) = http.requests.lock()[0].clone();
    assert_eq!(url, "https://dev.opencode.ai/console/api/v2/config");
    assert_eq!(header(&headers, "authorization"), Some("Bearer console-access"));
    assert_eq!(header(&headers, "x-org-id"), Some("wrk_anomaly"));
    assert_eq!(header(&headers, "user-agent"), Some("opencode/latest/1.0.0/opencode"));
    let document = snapshot.document().unwrap();
    let encoded = serde_json::to_value(&document).unwrap();
    assert_eq!(encoded["integrationID"], "opencode");
    assert_eq!(encoded["organization"], "Anomaly");
    assert_eq!(encoded["connection"]["type"], "credential");
    assert_eq!(encoded["info"]["providers"], json!({"anthropic": {"settings": {"baseURL": "https://gateway.test"}}}));
    assert_eq!(encoded["info"]["mcp"], json!({"servers": {"docs": {"type": "remote", "url": "https://mcp.test"}}}));
    assert!(encoded["info"].get("plugins").is_none());
    // Its policies are the organization's.
    let statements = oc_config::policy::statements(&[document]);
    assert_eq!(statements[0].message, "Blocked by Anomaly's policy");
    // A location stores its document for its config loads.
    assert!(console_config::publish("/tmp/console-test-a", snapshot.clone()));
    assert!(!console_config::publish("/tmp/console-test-a", snapshot));
    assert_eq!(console_config::documents("/tmp/console-test-a").len(), 1);
    console_config::forget("/tmp/console-test-a");
    assert!(console_config::documents("/tmp/console-test-a").is_empty());
}

#[tokio::test(start_paused = true)]
async fn a_console_without_config_contributes_nothing() {
    let integrations = location(Some(signed_in())).await;
    let http = Fake::new(vec![("/api/v2/config", 404, json!({}))]);
    let snapshot = load(&integrations, &http, "ua", &Snapshot::default()).await;
    assert!(snapshot.connection.is_some());
    assert!(snapshot.document().is_none());
    assert!(!snapshot.offers_websearch());
    let signed_out = load(&*location(None).await, &http, "ua", &Snapshot::default()).await;
    assert_eq!(signed_out, Snapshot::default());
}

#[tokio::test(start_paused = true)]
async fn marks_the_connection_for_sign_in_when_console_reports_it_and_keeps_the_last_config() {
    let integrations = location(Some(signed_in())).await;
    let ok = Fake::new(vec![("/api/v2/config", 200, json!({"policies": [{"action": "tool.use", "resource": "shell:*", "effect": "deny"}]}))]);
    let first = load(&integrations, &ok, "ua", &Snapshot::default()).await;
    assert!(first.document().is_some());

    let sso = Fake::new(vec![("/api/v2/config", 403, json!({"_tag": "SsoRequired", "orgId": "wrk_anomaly", "connectionId": "conn_okta"}))]);
    let next = load(&integrations, &sso, "ua", &first).await;
    // A failure for the same connection keeps its last config, so organization policy stays in force.
    assert_eq!(next.remote, first.remote);
    assert_eq!(
        status(&integrations).await,
        Some(serde_json::to_value(ConnectionStatus::needs_auth(
            "Sign in with SSO again to use Anomaly",
            Some("https://dev.opencode.ai/console/auth/sso/conn_okta/start?redirectTo=%2Fconsole%2F".into())
        ))
        .unwrap())
    );

    let unauthorized = Fake::new(vec![("/api/v2/config", 401, json!({"_tag": "Unauthorized"}))]);
    load(&integrations, &unauthorized, "ua", &first).await;
    assert_eq!(
        status(&integrations).await,
        Some(serde_json::to_value(ConnectionStatus::needs_auth("Reconnect OpenCode Console to continue", None)).unwrap())
    );

    // A later success clears the status.
    load(&integrations, &ok, "ua", &first).await;
    assert_eq!(status(&integrations).await, None);

    // Other failures change nothing but the config they could not load.
    let down = Fake::new(vec![("/api/v2/config", 500, json!("down"))]);
    let kept = load(&integrations, &down, "ua", &first).await;
    assert_eq!(kept.remote, first.remote);
    assert_eq!(status(&integrations).await, None);
}

#[test]
fn load_errors_read_like_upstream() {
    assert_eq!(LoadError::SignedOut.to_string(), "Reconnect OpenCode Console to continue.");
    assert_eq!(
        LoadError::SsoRequired { organization: None, url: "x".into() }.to_string(),
        "Sign in with SSO again to use your OpenCode Console organization."
    );
}

async fn status(integrations: &Integrations) -> Option<Value> {
    let info = integrations.get("opencode").await.unwrap();
    serde_json::to_value(info.unwrap().connections).unwrap()[0].get("status").cloned()
}

// ---------------------------------------------------------------------------------------------
// Workspace instructions and skills (2.2.7)
// ---------------------------------------------------------------------------------------------

/// Answers by URL suffix from routes that a test changes between loads.
struct Live {
    routes: Mutex<Vec<(&'static str, u16, Value)>>,
}

#[async_trait]
impl Http for Live {
    async fn get(&self, url: &str, _headers: &[(String, String)]) -> Result<Response, String> {
        let routes = self.routes.lock();
        let (_, status, body) = routes.iter().find(|(suffix, _, _)| url.ends_with(suffix)).ok_or("no route")?;
        Ok(Response { status: *status, body: if body.is_string() { body.as_str().unwrap().to_string() } else { body.to_string() } })
    }
}

fn console_key() -> CredentialValue {
    CredentialValue::Key {
        key: "secret".into(),
        metadata: Some(json!({"server": "https://console.test", "orgID": "org_test"}).as_object().unwrap().clone()),
    }
}

/// provider-opencode.test.ts "keeps Console instructions and skills through an edit or a Console error, and clears
/// them on 404" and "applies Console policies while its instructions and skills fail, and keeps a list over a
/// malformed one".
#[tokio::test(start_paused = true)]
async fn workspace_lists_keep_their_last_copy_and_never_hold_back_policies() {
    let integrations = location(Some(console_key())).await;
    let sudo = json!({"action": "tool.use", "resource": "shell:sudo *", "effect": "deny"});
    let http = Arc::new(Live {
        routes: Mutex::new(vec![
            ("/api/v2/config", 200, json!({"providers": {}, "policies": [sudo]})),
            ("/api/v2/instructions", 503, json!("Unavailable")),
            ("/api/v2/skills", 503, json!("Unavailable")),
        ]),
    });
    let cache = Arc::new(SharedCache::default());
    let load = |previous: Snapshot| {
        let (cache, http, integrations) = (cache.clone(), http.clone(), integrations.clone());
        async move { console_config::load_with(&cache, Duration::ZERO, &integrations, http, "ua", &previous).await }
    };
    // Once the lists' retries run out, the config's policies still apply, just without the lists.
    let first = load(Snapshot::default()).await;
    let document = first.document().expect("the Console document");
    assert_eq!(oc_config::policy::statements(&[document]).len(), 1);
    assert!(first.instructions.is_empty());

    let set = |path: &'static str, status: u16, body: Value| {
        let mut routes = http.routes.lock();
        routes.retain(|(p, _, _)| *p != path);
        routes.push((path, status, body));
    };
    set("/api/v2/instructions", 200, json!({"instructions": [{"id": "style", "content": "Use tabs."}]}));
    set("/api/v2/skills", 200, json!({"skills": [{"slug": "deploy", "version": "1"}]}));
    let ok = load(first).await;
    let files: Vec<(String, String)> = ok.instruction_files().into_iter().map(|f| (f.path, f.content)).collect();
    assert_eq!(files, [("OpenCode Console instruction style".to_string(), "Use tabs.".to_string())]);
    assert_eq!(ok.skills, [console_config::SkillRef { slug: "deploy".into(), version: "1".into() }]);

    // A malformed list or a Console error keeps the last one.
    set("/api/v2/instructions", 200, json!({"instructions": [{"id": "style"}]}));
    set("/api/v2/skills", 503, json!("down"));
    let kept = load(ok.clone()).await;
    assert_eq!((kept.instructions.clone(), kept.skills.clone()), (ok.instructions.clone(), ok.skills.clone()));
    // A 404 means the workspace has none; a 403 that is not SSO too.
    set("/api/v2/instructions", 404, json!({}));
    set("/api/v2/skills", 403, json!({"type": "forbidden"}));
    let cleared = load(kept).await;
    assert!(cleared.instructions.is_empty() && cleared.skills.is_empty());
    assert!(cleared.document().is_some());

    // The lists apply only while the location holds the Console document unchanged.
    let document = cleared.document().unwrap();
    assert!(cleared.allowed(std::slice::from_ref(&document)));
    assert!(!cleared.allowed(&[]));
}

type Seen = Arc<Mutex<Vec<(String, Option<String>, Option<String>)>>>;

/// A mock Console that answers the skill download with a redirect (`/archive/...` on `archive_host`).
async fn console_server(archive_host: &'static str) -> (String, Seen) {
    use axum::extract::{OriginalUri, State};
    use axum::http::{HeaderMap, StatusCode};
    use axum::response::{IntoResponse, Redirect};
    let seen: Seen = Arc::new(Mutex::new(vec![]));
    async fn handle(State((seen, port, host)): State<(Seen, u16, &'static str)>, uri: OriginalUri, headers: HeaderMap) -> axum::response::Response {
        let header = |name: &str| headers.get(name).and_then(|v| v.to_str().ok()).map(String::from);
        seen.lock().push((uri.0.to_string(), header("authorization"), header("x-org-id")));
        match uri.0.path() {
            "/api/v2/skills/deploy" => Redirect::to(&format!("http://{host}:{port}/archive/deploy.json?expires=1&sig=a%20b~c")).into_response(),
            path if path.starts_with("/archive/") => axum::Json(json!({
                "slug": "deploy",
                "files": [
                    {"path": "SKILL.md", "content": "---\nname: Deploy\ndescription: Deploys the service.\n---\n\n# Deploy\n"},
                    {"path": "scripts/run.sh", "content": "echo run\n"},
                ],
            }))
            .into_response(),
            _ => StatusCode::NOT_FOUND.into_response(),
        }
    }
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let app = axum::Router::new().fallback(handle).with_state((seen.clone(), port, archive_host));
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (format!("http://localhost:{port}"), seen)
}

async fn download(server: &str) -> Vec<console_config::ConsoleSkill> {
    let credential =
        CredentialValue::Key { key: "secret".into(), metadata: Some(json!({"server": server, "orgID": "org_test"}).as_object().unwrap().clone()) };
    let integrations = location(Some(credential)).await;
    let connection = integrations.active("opencode").await.unwrap().unwrap();
    let listed = [console_config::SkillRef { slug: "deploy".into(), version: "1".into() }];
    console_config::load_skills_with(&Arc::new(SharedCache::default()), &integrations, &connection, &listed).await.unwrap()
}

/// provider-opencode.test.ts "downloads a skill through a redirect without sending credentials to another origin".
#[tokio::test]
async fn downloads_a_skill_through_a_redirect_without_credentials_crossing_origins() {
    let (server, seen) = console_server("127.0.0.1").await;
    let skills = download(&server).await;
    assert_eq!(skills.len(), 1);
    let skill = &skills[0];
    assert_eq!((skill.skill.id.as_str(), skill.skill.name.as_str()), ("deploy", "Deploy"));
    assert_eq!(skill.skill.description.as_deref(), Some("Deploys the service."));
    assert_eq!(skill.skill.path, std::path::PathBuf::from("skill://deploy/SKILL.md"));
    assert_eq!(skill.files.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect::<Vec<_>>(), [("scripts/run.sh", "echo run\n")]);
    let seen = seen.lock().clone();
    assert_eq!(seen[0], ("/api/v2/skills/deploy".to_string(), Some("Bearer secret".to_string()), Some("org_test".to_string())));
    // The presigned query is requested exactly as sent, and the other origin gets no credentials.
    assert_eq!(seen.last().unwrap(), &("/archive/deploy.json?expires=1&sig=a%20b~c".to_string(), None, None));
}

/// provider-opencode.test.ts "keeps credentials on a same-origin skill redirect".
#[tokio::test]
async fn keeps_credentials_on_a_same_origin_skill_redirect() {
    let (server, seen) = console_server("localhost").await;
    assert_eq!(download(&server).await.len(), 1);
    assert_eq!(seen.lock().last().unwrap().1.as_deref(), Some("Bearer secret"));
}

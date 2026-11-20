//! The LM Studio, Ollama and vLLM discovery plugins (`core/test/plugin/provider-{lmstudio,ollama,vllm}.test.ts`)
//! against mock servers on ephemeral ports. The default origins are always replaced: nothing here may reach a real
//! server on 1234, 11434 or 8000.

use std::sync::Arc;
use std::time::Duration;

use axum::body::Bytes;
use axum::http::{HeaderMap, Method, StatusCode, Uri};
use axum::response::{IntoResponse, Response};
use futures::StreamExt;
use oc_ai::catalog::local::Kind;
use oc_config::schema::{Entry, Info};
use oc_core::models::Models;
use oc_core::plugins::Plugins;
use parking_lot::Mutex;
use serde_json::{json, Value};

#[derive(Debug, Clone, PartialEq)]
struct Seen {
    method: String,
    path: String,
    authorization: Option<String>,
    body: Value,
}

type Handler = Arc<dyn Fn(&Seen) -> (u16, Option<Value>) + Send + Sync>;

/// A mock server: every request is recorded and answered by `handler` (status, JSON body).
struct Server {
    origin: String,
    seen: Arc<Mutex<Vec<Seen>>>,
    /// How long every answer waits.
    delay: Arc<Mutex<Duration>>,
    task: tokio::task::JoinHandle<()>,
}

impl Server {
    async fn start(handler: impl Fn(&Seen) -> (u16, Option<Value>) + Send + Sync + 'static) -> Server {
        let handler: Handler = Arc::new(handler);
        let seen = Arc::new(Mutex::new(Vec::new()));
        let recorded = seen.clone();
        let delay = Arc::new(Mutex::new(Duration::ZERO));
        let waits = delay.clone();
        let app = axum::Router::new().fallback(move |method: Method, uri: Uri, headers: HeaderMap, body: Bytes| {
            let (handler, recorded, waits) = (handler.clone(), recorded.clone(), waits.clone());
            async move {
                let request = Seen {
                    method: method.to_string(),
                    path: uri.path().to_string(),
                    authorization: headers.get("authorization").and_then(|v| v.to_str().ok()).map(String::from),
                    body: serde_json::from_slice(&body).unwrap_or(Value::Null),
                };
                recorded.lock().push(request.clone());
                let wait = *waits.lock();
                tokio::time::sleep(wait).await;
                let (status, body) = handler(&request);
                let status = StatusCode::from_u16(status).unwrap();
                match body {
                    Some(Value::String(text)) => Response::builder()
                        .status(status)
                        .header("content-type", "text/event-stream")
                        .body(axum::body::Body::from(text))
                        .unwrap(),
                    Some(body) => (status, axum::Json(body)).into_response(),
                    None => status.into_response(),
                }
            }
        });
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let origin = format!("http://127.0.0.1:{}", listener.local_addr().unwrap().port());
        let task = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        Server { origin, seen, delay, task }
    }

    fn set_delay(&self, delay: Duration) {
        *self.delay.lock() = delay;
    }

    fn seen(&self) -> Vec<Seen> {
        self.seen.lock().clone()
    }

    fn clear(&self) {
        self.seen.lock().clear();
    }

    fn count(&self, path: &str) -> usize {
        self.seen.lock().iter().filter(|s| s.path == path).count()
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        self.task.abort();
    }
}

struct Ctx {
    plugins: Arc<Plugins>,
    models: Arc<Models>,
    bus: oc_core::bus::Bus,
    _home: tempfile::TempDir,
}

/// The app with discovery enabled: every default origin off, `kind` at `origin` (`None`: off too).
fn ctx(kind: Kind, origin: Option<&str>, interval: Duration) -> Ctx {
    let home = tempfile::tempdir().unwrap();
    // SAFETY: every test sets the same values.
    unsafe {
        std::env::set_var("OPENCODE_DISABLE_MODELS_FETCH", "1");
        std::env::set_var(oc_core::local_providers::ORIGIN_OVERRIDE, "off");
        std::env::remove_var("LMSTUDIO_API_KEY");
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
    let plugins = Plugins::new(global, db, bus.clone(), "1.2.3");
    models.set_plugins(plugins.clone());
    let local = &plugins.local;
    for k in Kind::ALL {
        local.set_default_origin(k, None);
    }
    local.set_default_origin(kind, origin.map(String::from));
    local.set_interval(interval);
    local.enable();
    Ctx { plugins, models, bus, _home: home }
}

fn config(providers: Value) -> Entry {
    Entry::document(None, Info::from_value(&json!({ "providers": providers })).unwrap())
}

impl Ctx {
    /// A location boots with `entries` (its plugins start).
    fn boot(&self, key: &str, entries: &[Entry]) {
        self.plugins.local.observe(key, entries);
        self.plugins.local.start(key);
    }

    /// The location's configuration changed (`ensure_reload`, then `config.updated`).
    fn reconfigure(&self, key: &str, entries: &[Entry]) {
        self.plugins.local.observe(key, entries);
        let at = oc_schema::types::LocationRef { directory: key.to_string(), workspace_id: None };
        self.bus.publish("config.updated", json!({}), Some(at));
    }

    async fn catalog(&self, key: &str, entries: &[Entry]) -> oc_ai::catalog::Catalog {
        self.models.catalog_at(entries, Some(key)).await.unwrap()
    }

    async fn model(&self, key: &str, entries: &[Entry], provider: &str, id: &str) -> Option<oc_ai::catalog::ModelInfo> {
        self.catalog(key, entries).await.model(provider, id).cloned()
    }

    fn discovered(&self, key: &str, kind: Kind) -> Option<oc_ai::catalog::local::Discovered> {
        self.plugins.local.discovered(key).into_iter().find(|d| d.kind == kind)
    }
}

/// Polls `f` until `pred` holds (15 s: the first catalog parses models.dev, slow in a loaded debug build).
async fn eventually<T, F, Fut>(mut f: F, pred: impl Fn(&T) -> bool) -> T
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = T>,
{
    let deadline = tokio::time::Instant::now() + Duration::from_secs(15);
    loop {
        let value = f().await;
        if pred(&value) || tokio::time::Instant::now() > deadline {
            assert!(pred(&value), "condition not reached in time");
            return value;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

const HOUR: Duration = Duration::from_secs(3600);
const FAST: Duration = Duration::from_millis(5);

// ------------------------------------------------------------------------------------------------ LM Studio

fn lm(key: &str, name: &str, max: i64) -> Value {
    json!({"type": "llm", "key": key, "display_name": name, "loaded_instances": [], "max_context_length": max})
}

#[test]
fn registered_as_built_in_provider_plugins() {
    for id in ["opencode.provider.lmstudio", "opencode.provider.ollama", "opencode.provider.vllm"] {
        assert!(oc_core::plugins::BUILTIN_PLUGINS.contains(&id));
    }
    assert_eq!(Kind::LmStudio.plugin_id(), "opencode.provider.lmstudio");
    assert_eq!(Kind::Vllm.plugin_id(), "opencode.provider.vllm");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn lmstudio_discovers_language_models_with_capabilities_and_effective_context() {
    let server = Server::start(|_| {
        (200, Some(json!({"models": [
            {"type": "llm", "key": "google/gemma-4-26b-a4b", "display_name": "Gemma 4 26B A4B", "architecture": "gemma4",
             "loaded_instances": [{"config": {"context_length": 32768}}, {"config": {"context_length": 16384}}],
             "max_context_length": 262144, "capabilities": {"vision": true, "trained_for_tool_use": true}},
            {"type": "llm", "key": "deepseek-r1", "display_name": "DeepSeek R1", "architecture": "deepseek", "loaded_instances": [],
             "max_context_length": 131072, "capabilities": {"vision": false, "trained_for_tool_use": false}},
            {"type": "llm", "key": "unknown-context", "display_name": "Unknown Context", "loaded_instances": [], "max_context_length": 0},
            {"type": "embedding", "key": "nomic-embed", "display_name": "Nomic Embed", "loaded_instances": [], "max_context_length": 2048}
        ]})))
    })
    .await;
    let c = ctx(Kind::LmStudio, Some(&server.origin), HOUR);
    let entries = vec![];
    c.boot("/loc", &entries);
    let gemma = eventually(|| c.model("/loc", &entries, "lmstudio", "google/gemma-4-26b-a4b"), Option::is_some).await.unwrap();

    let catalog = c.catalog("/loc", &entries).await;
    let provider = catalog.provider("lmstudio").unwrap();
    assert_eq!(provider.name, "LM Studio");
    assert_eq!(provider.activation, oc_ai::catalog::Activation::Enabled);
    assert_eq!(provider.package, "@opencode/ai/providers/openai-compatible");
    assert_eq!(provider.integration_id, None);
    assert_eq!(
        provider.settings.clone().map(Value::Object),
        Some(json!({"baseURL": format!("{}/v1", server.origin), "provider": "lmstudio", "apiKey": ""}))
    );
    assert!(catalog.is_available("lmstudio"));
    assert_eq!((gemma.family.as_deref(), gemma.name.as_str()), (Some("gemma4"), "Gemma 4 26B A4B"));
    assert!(gemma.capabilities.tools);
    assert_eq!((gemma.capabilities.input.clone(), gemma.capabilities.output.clone()), (vec!["text".to_string(), "image".into()], vec!["text".to_string()]));
    assert_eq!((gemma.limit.context, gemma.limit.output), (16_384, 32_000));
    let deepseek = catalog.model("lmstudio", "deepseek-r1").unwrap();
    assert!(!deepseek.capabilities.tools);
    assert_eq!(deepseek.capabilities.input, ["text"]);
    assert_eq!((deepseek.limit.context, deepseek.limit.output), (131_072, 32_000));
    assert_eq!(catalog.model("lmstudio", "unknown-context").unwrap().limit.context, 200_000);
    assert!(catalog.model("lmstudio", "nomic-embed").is_none());
    // The models.dev models of the same provider are gone.
    assert_eq!(catalog.provider_models("lmstudio").len(), 3);
    assert_eq!(server.seen()[0].path, "/api/v1/models");
    assert_eq!(server.seen()[0].authorization, None);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn lmstudio_refreshes_the_catalog_when_models_change() {
    let models = Arc::new(Mutex::new(Vec::<Value>::new()));
    let listed = models.clone();
    let server = Server::start(move |_| (200, Some(json!({ "models": listed.lock().clone() })))).await;
    let c = ctx(Kind::LmStudio, Some(&server.origin), FAST);
    let entries = vec![];
    c.boot("/loc", &entries);
    eventually(|| async { server.count("/api/v1/models") }, |n| *n >= 2).await;
    // Nothing discovered: the models.dev definition stays (it needs a credential).
    let catalog = c.catalog("/loc", &entries).await;
    assert!(!catalog.is_available("lmstudio"));
    assert_eq!(catalog.provider("lmstudio").unwrap().name, "LMStudio");

    models.lock().push(json!({"type": "llm", "key": "qwen/qwen3-coder", "display_name": "Qwen 3 Coder", "architecture": "qwen3",
        "loaded_instances": [], "max_context_length": 65536, "capabilities": {"vision": false, "trained_for_tool_use": true}}));
    let model = eventually(|| c.model("/loc", &entries, "lmstudio", "qwen/qwen3-coder"), Option::is_some).await.unwrap();
    assert_eq!(model.name, "Qwen 3 Coder");

    models.lock().clear();
    eventually(|| async { c.catalog("/loc", &entries).await.is_available("lmstudio") }, |a| !a).await;
    assert!(c.model("/loc", &entries, "lmstudio", "qwen/qwen3-coder").await.is_none());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn lmstudio_discovers_from_configured_endpoints_with_bearer_authentication() {
    let initial = Server::start(|_| (200, Some(json!({"models": [lm("initial-model", "initial-model", 32768)]})))).await;
    let configured = Server::start(|_| (200, Some(json!({"models": [lm("configured-model", "configured-model", 32768)]})))).await;
    let c = ctx(Kind::LmStudio, Some(&initial.origin), HOUR);
    c.boot("/loc", &[]);
    eventually(|| c.model("/loc", &[], "lmstudio", "initial-model"), Option::is_some).await;

    let base = format!("{}/proxy/v1", configured.origin);
    let entries = vec![config(json!({"lmstudio": {"settings": {"baseURL": base, "apiKey": "secret"}}}))];
    c.reconfigure("/loc", &entries);
    eventually(|| c.model("/loc", &entries, "lmstudio", "configured-model"), Option::is_some).await;
    assert!(configured.seen().iter().any(|s| s.authorization.as_deref() == Some("Bearer secret") && s.path == "/proxy/api/v1/models"));
    assert!(c.model("/loc", &entries, "lmstudio", "initial-model").await.is_none());
    let discovered = c.discovered("/loc", Kind::LmStudio).unwrap();
    assert_eq!((discovered.base_url.as_str(), discovered.api_key.as_deref()), (base.as_str(), Some("secret")));
    let provider = c.catalog("/loc", &entries).await.provider("lmstudio").cloned().unwrap();
    assert_eq!(provider.settings.map(Value::Object), Some(json!({"baseURL": base, "provider": "lmstudio", "apiKey": "secret"})));

    // A later document removes the key: discovery asks without authorization.
    configured.clear();
    let entries = vec![
        config(json!({"lmstudio": {"settings": {"baseURL": base, "apiKey": "secret"}}})),
        config(json!({"lmstudio": {"settings": {"baseURL": base, "apiKey": null}}})),
    ];
    c.reconfigure("/loc", &entries);
    eventually(|| async { c.discovered("/loc", Kind::LmStudio).map(|d| d.api_key) }, |k| *k == Some(None)).await;
    assert!(configured.seen().iter().any(|s| s.authorization.is_none() && s.path == "/proxy/api/v1/models"));

    // `/proxy/api/v1` is the same server root.
    configured.clear();
    let api_base = format!("{}/proxy/api/v1", configured.origin);
    let entries = vec![config(json!({"lmstudio": {"settings": {"baseURL": api_base, "apiKey": "api-secret"}}}))];
    c.reconfigure("/loc", &entries);
    eventually(|| async { c.discovered("/loc", Kind::LmStudio).map(|d| d.base_url) }, |b| b.as_deref() == Some(api_base.as_str())).await;
    assert!(configured.seen().iter().any(|s| s.authorization.as_deref() == Some("Bearer api-secret") && s.path == "/proxy/api/v1/models"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn lmstudio_shares_discovery_requests_across_locations() {
    let server = Server::start(|_| (200, Some(json!({"models": [lm("shared-model", "Shared Model", 32768)]})))).await;
    let c = ctx(Kind::LmStudio, Some(&server.origin), HOUR);
    c.boot("/a", &[]);
    c.boot("/b", &[]);
    eventually(|| c.model("/a", &[], "lmstudio", "shared-model"), Option::is_some).await;
    eventually(|| c.model("/b", &[], "lmstudio", "shared-model"), Option::is_some).await;
    assert_eq!(server.count("/api/v1/models"), 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn lmstudio_replaces_the_credential_gated_models_dev_catalog_when_discovery_succeeds() {
    let models = Arc::new(Mutex::new(vec![lm("discovered-model", "Discovered Model", 32768)]));
    let listed = models.clone();
    let server = Server::start(move |_| (200, Some(json!({ "models": listed.lock().clone() })))).await;
    let c = ctx(Kind::LmStudio, None, FAST);
    // Before discovery: the models.dev provider, gated by the `lmstudio` integration (LMSTUDIO_API_KEY).
    let entries = vec![];
    let before = c.catalog("/loc", &entries).await;
    let record = before.record("lmstudio").expect("models.dev lists LM Studio");
    assert_eq!(record.provider.integration_id.as_deref(), Some("lmstudio"));
    let static_models: Vec<String> = record.models.keys().cloned().collect();
    assert!(!static_models.is_empty());
    assert!(!before.is_available("lmstudio"));
    let integrations = c.models.integrations_for(Some("/loc"));
    assert!(integrations.get("lmstudio").await.unwrap().is_some());

    c.plugins.local.set_default_origin(Kind::LmStudio, Some(server.origin.clone()));
    c.boot("/loc", &entries);
    eventually(|| c.model("/loc", &entries, "lmstudio", "discovered-model"), Option::is_some).await;
    let catalog = c.catalog("/loc", &entries).await;
    assert!(integrations.get("lmstudio").await.unwrap().is_none(), "the integration is removed");
    assert_eq!(catalog.provider("lmstudio").unwrap().integration_id, None);
    assert!(static_models.iter().all(|m| catalog.model("lmstudio", m).is_none()));
    assert!(catalog.is_available("lmstudio"));

    // An empty answer restores the models.dev definition and its integration.
    models.lock().clear();
    let restored = eventually(|| c.catalog("/loc", &entries), |c| c.record("lmstudio").is_some_and(|r| r.models.contains_key(&static_models[0]))).await;
    assert!(restored.model("lmstudio", "discovered-model").is_none());
    assert!(!restored.is_available("lmstudio"));
    assert!(integrations.get("lmstudio").await.unwrap().is_some());
    assert_eq!(restored.provider("lmstudio").unwrap().integration_id.as_deref(), Some("lmstudio"));
}

// ------------------------------------------------------------------------------------------------ vLLM

fn remote(id: &str, max: Value, owned_by: &str) -> Value {
    json!({"id": id, "object": "model", "created": 1, "owned_by": owned_by, "root": id, "parent": null, "max_model_len": max, "permission": []})
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn vllm_waits_for_readiness_and_discovers_model_metadata() {
    let healthy = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let ready = healthy.clone();
    let server = Server::start(move |seen| {
        if seen.path == "/health" {
            return (if ready.load(std::sync::atomic::Ordering::SeqCst) { 200 } else { 503 }, None);
        }
        (200, Some(json!({"object": "list", "data": [remote("Qwen/Qwen3-Coder", json!(65536), "vllm"), remote("unknown-limit", json!(0), "vllm"), remote("foreign-model", json!(4096), "other")]})))
    })
    .await;
    let c = ctx(Kind::Vllm, Some(&server.origin), FAST);
    c.boot("/loc", &[]);
    tokio::time::sleep(Duration::from_millis(30)).await;
    assert!(c.catalog("/loc", &[]).await.provider("vllm").is_none());
    assert_eq!(server.count("/v1/models"), 0);

    healthy.store(true, std::sync::atomic::Ordering::SeqCst);
    let model = eventually(|| c.model("/loc", &[], "vllm", "Qwen/Qwen3-Coder"), Option::is_some).await.unwrap();
    let catalog = c.catalog("/loc", &[]).await;
    let provider = catalog.provider("vllm").unwrap();
    assert_eq!((provider.name.as_str(), provider.package.as_str()), ("vLLM", "@opencode/ai/providers/openai-compatible"));
    assert_eq!(provider.activation, oc_ai::catalog::Activation::Enabled);
    assert_eq!(provider.settings.clone().map(Value::Object), Some(json!({"baseURL": format!("{}/v1", server.origin), "provider": "vllm", "apiKey": ""})));
    assert!(catalog.is_available("vllm"));
    assert_eq!((model.model_id.as_str(), model.name.as_str()), ("Qwen/Qwen3-Coder", "Qwen/Qwen3-Coder"));
    assert!(!model.capabilities.tools);
    assert_eq!(model.capabilities.input, ["text"]);
    assert_eq!((model.limit.context, model.limit.output), (65_536, 32_000));
    assert_eq!(catalog.model("vllm", "unknown-limit").unwrap().limit.context, 200_000);
    assert!(catalog.model("vllm", "foreign-model").is_none());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn vllm_retains_the_last_success_through_transient_failures() {
    let state = Arc::new(Mutex::new((false, vec![remote("first-model", json!(32768), "vllm")])));
    let current = state.clone();
    let server = Server::start(move |seen| {
        let state = current.lock();
        if state.0 {
            return (503, None);
        }
        if seen.path == "/health" {
            return (200, None);
        }
        (200, Some(json!({"object": "list", "data": state.1})))
    })
    .await;
    let c = ctx(Kind::Vllm, Some(&server.origin), FAST);
    c.boot("/loc", &[]);
    eventually(|| c.model("/loc", &[], "vllm", "first-model"), Option::is_some).await;

    *state.lock() = (true, vec![remote("second-model", json!(32768), "vllm")]);
    let failing = server.count("/health");
    eventually(|| async { server.count("/health") }, |n| *n >= failing + 3).await;
    assert!(c.model("/loc", &[], "vllm", "first-model").await.is_some());
    assert!(c.model("/loc", &[], "vllm", "second-model").await.is_none());

    state.lock().0 = false;
    eventually(|| c.model("/loc", &[], "vllm", "second-model"), Option::is_some).await;
    assert!(c.model("/loc", &[], "vllm", "first-model").await.is_none());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn vllm_removes_the_provider_after_an_empty_success() {
    let models = Arc::new(Mutex::new(vec![remote("discovered-model", json!(32768), "vllm")]));
    let listed = models.clone();
    let server = Server::start(move |seen| {
        if seen.path == "/health" {
            return (200, None);
        }
        (200, Some(json!({"object": "list", "data": listed.lock().clone()})))
    })
    .await;
    let c = ctx(Kind::Vllm, Some(&server.origin), FAST);
    c.boot("/loc", &[]);
    eventually(|| c.model("/loc", &[], "vllm", "discovered-model"), Option::is_some).await;
    models.lock().clear();
    // vLLM has no models.dev definition to restore.
    eventually(|| async { c.catalog("/loc", &[]).await.provider("vllm").is_none() }, |gone| *gone).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn vllm_reloads_layered_endpoint_and_bearer_settings() {
    let initial = Server::start(|seen| {
        if seen.path == "/health" {
            return (200, None);
        }
        (200, Some(json!({"object": "list", "data": [remote("initial-model", json!(32768), "vllm")]})))
    })
    .await;
    let configured = Server::start(|seen| {
        if seen.path == "/proxy/health" {
            return (200, None);
        }
        (200, Some(json!({"object": "list", "data": [remote("configured-model", json!(32768), "vllm")]})))
    })
    .await;
    let c = ctx(Kind::Vllm, Some(&initial.origin), HOUR);
    c.boot("/loc", &[]);
    eventually(|| c.model("/loc", &[], "vllm", "initial-model"), Option::is_some).await;

    let base = format!("{}/proxy/v1", configured.origin);
    let entries = vec![config(json!({"vllm": {"settings": {"baseURL": base}}})), config(json!({"vllm": {"settings": {"apiKey": "secret"}}}))];
    c.reconfigure("/loc", &entries);
    eventually(|| c.model("/loc", &entries, "vllm", "configured-model"), Option::is_some).await;
    let seen = configured.seen();
    assert!(seen.iter().any(|s| s.authorization.as_deref() == Some("Bearer secret") && s.path == "/proxy/health"));
    assert!(seen.iter().any(|s| s.authorization.as_deref() == Some("Bearer secret") && s.path == "/proxy/v1/models"));
    assert!(c.model("/loc", &entries, "vllm", "initial-model").await.is_none());
    let provider = c.catalog("/loc", &entries).await.provider("vllm").cloned().unwrap();
    assert_eq!(provider.settings.map(Value::Object), Some(json!({"baseURL": base, "provider": "vllm", "apiKey": "secret"})));

    configured.clear();
    let entries = vec![config(json!({"vllm": {"settings": {"baseURL": base}}})), config(json!({"vllm": {"settings": {"apiKey": "next-secret"}}}))];
    c.reconfigure("/loc", &entries);
    eventually(|| async { c.discovered("/loc", Kind::Vllm).and_then(|d| d.api_key) }, |k| k.as_deref() == Some("next-secret")).await;
    let seen = configured.seen();
    assert!(seen.iter().any(|s| s.authorization.as_deref() == Some("Bearer next-secret") && s.path == "/proxy/health"));
    assert!(seen.iter().any(|s| s.authorization.as_deref() == Some("Bearer next-secret") && s.path == "/proxy/v1/models"));
}

/// A discovered model answers through the OpenAI-compatible package at the discovered base URL with the key.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn vllm_routes_requests_to_the_discovered_server() {
    let server = Server::start(|seen| match seen.path.as_str() {
        "/health" => (200, None),
        "/v1/models" => (200, Some(json!({"object": "list", "data": [remote("served", json!(8192), "vllm")]}))),
        _ => {
            let chunks = [
                json!({"id": "c", "choices": [{"index": 0, "delta": {"role": "assistant", "content": "local answer"}}]}),
                json!({"id": "c", "choices": [{"index": 0, "delta": {}, "finish_reason": "stop"}], "usage": {"prompt_tokens": 1, "completion_tokens": 2, "total_tokens": 3}}),
            ];
            let mut body = String::new();
            for chunk in chunks {
                body.push_str(&format!("data: {chunk}\n\n"));
            }
            body.push_str("data: [DONE]\n\n");
            (200, Some(Value::String(body)))
        }
    })
    .await;
    let c = ctx(Kind::Vllm, None, HOUR);
    let entries = vec![config(json!({"vllm": {"settings": {"baseURL": format!("{}/v1", server.origin), "apiKey": "local-key"}}}))];
    c.boot("/loc", &entries);
    eventually(|| c.model("/loc", &entries, "vllm", "served"), Option::is_some).await;
    let catalog = c.catalog("/loc", &entries).await;
    let resolved = c.models.resolve_model(Some("/loc"), &catalog, "vllm", "served", None, None).await.unwrap();
    let request = oc_ai::LLMRequest::new(resolved.language_model).prompt("hi");
    let mut stream = oc_ai::Client::new().stream(request, oc_ai::CancellationToken::new()).await.unwrap();
    let mut text = String::new();
    while let Some(event) = stream.next().await {
        if let oc_ai::LLMEvent::TextDelta { text: delta, .. } = event.unwrap() {
            text.push_str(&delta);
        }
    }
    assert_eq!(text, "local answer");
    let chat = server.seen().into_iter().find(|s| s.path == "/v1/chat/completions").expect("chat request");
    assert_eq!(chat.method, "POST");
    assert_eq!(chat.authorization.as_deref(), Some("Bearer local-key"));
    assert_eq!(chat.body["model"], "served");
}

// ------------------------------------------------------------------------------------------------ Ollama

fn summary(model: &str, digest: &str, family: &str) -> Value {
    json!({"name": model, "model": model, "modified_at": "2026-01-01T00:00:00Z", "size": 1_000_000, "digest": digest,
        "details": {"format": "gguf", "family": family, "families": [family], "parameter_size": "8B", "quantization_level": "Q4_K_M"}})
}

fn show(family: &str, capabilities: &[&str], context: i64) -> Value {
    json!({"parameters": "temperature 0.7",
        "details": {"parent_model": "", "format": "gguf", "family": family, "families": [family], "parameter_size": "8B", "quantization_level": "Q4_K_M"},
        "capabilities": capabilities, "model_info": {"general.architecture": family, format!("{family}.context_length"): context}})
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn ollama_skips_default_discovery_without_an_origin() {
    let c = ctx(Kind::Ollama, None, FAST);
    c.boot("/loc", &[]);
    tokio::time::sleep(Duration::from_millis(30)).await;
    assert!(c.catalog("/loc", &[]).await.provider("ollama").is_none());
    assert!(c.plugins.local.discovered("/loc").is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn ollama_discovers_local_completion_models_and_native_metadata() {
    let server = Server::start(|seen| {
        if seen.method == "GET" {
            return (200, Some(json!({"models": [summary("gemma3:4b", "gemma-digest", "gemma3"), summary("unknown-context", "unknown-digest", "llama"), summary("nomic-embed", "embed-digest", "llama"), summary("removed-model", "removed-digest", "llama")]})));
        }
        match seen.body["model"].as_str().unwrap() {
            "removed-model" => (404, None),
            "gemma3:4b" => (200, Some(json!({"capabilities": ["completion", "tools", "vision"], "model_info": {"gemma3.context_length": 131072}}))),
            "unknown-context" => (200, Some(show("unknown", &["completion"], 0))),
            _ => (200, Some(show("nomic-bert", &["embedding"], 8192))),
        }
    })
    .await;
    let c = ctx(Kind::Ollama, Some(&server.origin), HOUR);
    c.boot("/loc", &[]);
    let model = eventually(|| c.model("/loc", &[], "ollama", "gemma3:4b"), Option::is_some).await.unwrap();
    let catalog = c.catalog("/loc", &[]).await;
    let provider = catalog.provider("ollama").unwrap();
    assert_eq!((provider.name.as_str(), provider.package.as_str()), ("Ollama", "@opencode/ai/providers/openai-compatible"));
    assert_eq!(provider.settings.clone().map(Value::Object), Some(json!({"baseURL": format!("{}/v1", server.origin), "provider": "ollama", "apiKey": ""})));
    assert_eq!((model.model_id.as_str(), model.name.as_str(), model.family.as_deref()), ("gemma3:4b", "gemma3:4b", Some("gemma3")));
    assert!(model.capabilities.tools);
    assert_eq!(model.capabilities.input, ["text", "image"]);
    assert_eq!((model.limit.context, model.limit.output), (131_072, 32_000));
    assert_eq!(catalog.model("ollama", "unknown-context").unwrap().limit.context, 200_000);
    assert!(catalog.model("ollama", "nomic-embed").is_none());
    assert!(catalog.model("ollama", "removed-model").is_none());
    let seen = server.seen();
    assert!(seen.iter().any(|s| s.method == "GET" && s.path == "/api/tags"));
    for name in ["gemma3:4b", "nomic-embed", "removed-model"] {
        assert!(seen.iter().any(|s| s.method == "POST" && s.path == "/api/show" && s.body == json!({"model": name})), "{name}");
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn ollama_refreshes_changed_digests_and_retains_inventory_through_failures() {
    let state = Arc::new(Mutex::new(("digest-1".to_string(), 32_768i64, false)));
    let current = state.clone();
    let shows = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let counted = shows.clone();
    let server = Server::start(move |seen| {
        let state = current.lock();
        if seen.method == "GET" {
            if state.2 {
                return (503, None);
            }
            return (200, Some(json!({"models": [summary("qwen3:8b", &state.0, "qwen3")]})));
        }
        counted.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        (200, Some(show("qwen3", &["completion", "tools"], state.1)))
    })
    .await;
    let c = ctx(Kind::Ollama, Some(&server.origin), FAST);
    c.boot("/loc", &[]);
    eventually(|| c.model("/loc", &[], "ollama", "qwen3:8b"), |m| m.as_ref().is_some_and(|m| m.limit.context == 32_768)).await;
    eventually(|| async { server.count("/api/tags") }, |n| *n >= 2).await;
    assert_eq!(shows.load(std::sync::atomic::Ordering::SeqCst), 1, "an unchanged digest is not asked again");

    {
        let mut state = state.lock();
        state.0 = "digest-2".into();
        state.1 = 65_536;
    }
    eventually(|| c.model("/loc", &[], "ollama", "qwen3:8b"), |m| m.as_ref().is_some_and(|m| m.limit.context == 65_536)).await;
    assert_eq!(shows.load(std::sync::atomic::Ordering::SeqCst), 2);

    state.lock().2 = true;
    let tags = server.count("/api/tags");
    eventually(|| async { server.count("/api/tags") }, |n| *n >= tags + 3).await;
    assert_eq!(c.model("/loc", &[], "ollama", "qwen3:8b").await.unwrap().limit.context, 65_536);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn ollama_reloads_layered_endpoint_and_bearer_settings() {
    let initial = Server::start(|seen| {
        if seen.method == "GET" {
            return (200, Some(json!({"models": [summary("initial-model", "initial-digest", "llama")]})));
        }
        (200, Some(show("llama", &["completion"], 4096)))
    })
    .await;
    let configured = Server::start(|seen| {
        if seen.method == "GET" {
            return (200, Some(json!({"models": [summary("configured-model", "configured-digest", "llama")]})));
        }
        (200, Some(show("llama", &["completion", "vision"], 65536)))
    })
    .await;
    let c = ctx(Kind::Ollama, Some(&initial.origin), HOUR);
    c.boot("/loc", &[]);
    eventually(|| c.model("/loc", &[], "ollama", "initial-model"), Option::is_some).await;

    let base = format!("{}/proxy/v1", configured.origin);
    let entries = vec![config(json!({"ollama": {"settings": {"baseURL": base, "apiKey": "old"}}})), config(json!({"ollama": {"settings": {"apiKey": "secret"}}}))];
    c.reconfigure("/loc", &entries);
    eventually(|| c.model("/loc", &entries, "ollama", "configured-model"), Option::is_some).await;
    let seen = configured.seen();
    assert!(seen.iter().any(|s| s.authorization.as_deref() == Some("Bearer secret") && s.method == "GET" && s.path == "/proxy/api/tags"));
    assert!(seen.iter().any(|s| s.authorization.as_deref() == Some("Bearer secret") && s.method == "POST" && s.path == "/proxy/api/show"));
    assert!(c.model("/loc", &entries, "ollama", "initial-model").await.is_none());
    let discovered = c.discovered("/loc", Kind::Ollama).unwrap();
    assert_eq!((discovered.base_url.as_str(), discovered.api_key.as_deref()), (base.as_str(), Some("secret")));

    configured.clear();
    let entries = vec![config(json!({"ollama": {"settings": {"baseURL": base, "apiKey": "secret"}}})), config(json!({"ollama": {"settings": {"apiKey": null}}}))];
    c.reconfigure("/loc", &entries);
    eventually(|| async { c.discovered("/loc", Kind::Ollama).map(|d| d.api_key) }, |k| *k == Some(None)).await;
    // The discovered state can change before the mock has logged every request of that round: wait for the log.
    let unauthorized = |method: &'static str, path: &'static str| {
        move |seen: &Vec<_>| seen.iter().any(|s: &Seen| s.authorization.is_none() && s.method == method && s.path == path)
    };
    let seen = eventually(|| async { configured.seen() }, |seen| unauthorized("GET", "/proxy/api/tags")(seen) && unauthorized("POST", "/proxy/api/show")(seen)).await;
    assert!(seen.iter().any(|s| s.authorization.is_none() && s.method == "GET" && s.path == "/proxy/api/tags"));
    // The digest is unchanged but the key changed: the details are asked again.
    assert!(seen.iter().any(|s| s.authorization.is_none() && s.method == "POST" && s.path == "/proxy/api/show"));

    configured.clear();
    let api_base = format!("{}/proxy/api", configured.origin);
    let entries = vec![config(json!({"ollama": {"settings": {"baseURL": api_base, "apiKey": "api-secret"}}}))];
    c.reconfigure("/loc", &entries);
    eventually(|| async { c.discovered("/loc", Kind::Ollama).map(|d| d.base_url) }, |b| b.as_deref() == Some(api_base.as_str())).await;
    let bearer = |method: &'static str, path: &'static str| {
        move |seen: &Vec<_>| seen.iter().any(|s: &Seen| s.authorization.as_deref() == Some("Bearer api-secret") && s.method == method && s.path == path)
    };
    let seen = eventually(|| async { configured.seen() }, |seen| bearer("GET", "/proxy/api/tags")(seen) && bearer("POST", "/proxy/api/show")(seen)).await;
    assert!(seen.iter().any(|s| s.authorization.as_deref() == Some("Bearer api-secret") && s.method == "GET" && s.path == "/proxy/api/tags"));
    assert!(seen.iter().any(|s| s.authorization.as_deref() == Some("Bearer api-secret") && s.method == "POST" && s.path == "/proxy/api/show"));
}

/// An inventory change reloads the integrations and providers once; unchanged answers reload nothing.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn reload_events_follow_inventory_changes() {
    let server = Server::start(|seen| match seen.path.as_str() {
        "/health" => (200, None),
        _ => (200, Some(json!({"object": "list", "data": [remote("m", json!(1024), "vllm")]}))),
    })
    .await;
    let c = ctx(Kind::Vllm, Some(&server.origin), FAST);
    let mut events = c.bus.subscribe();
    c.boot("/loc", &[]);
    eventually(|| c.model("/loc", &[], "vllm", "m"), Option::is_some).await;
    eventually(|| async { server.count("/v1/models") }, |n| *n >= 3).await;
    let mut kinds = vec![];
    while let Ok(event) = events.try_recv() {
        if event.kind.ends_with(".updated") {
            kinds.push(event.kind.clone());
        }
    }
    // One reload for the one change (no projector here: the integration notify is not gated).
    assert_eq!(kinds, ["integration.updated", "provider.updated", "model.updated"]);
}

// ------------------------------------------------------------------------------------------------ 2.3.0

/// provider-lmstudio.test.ts (#56214, #63198, #63200): decision models are listed with an unknown (`null`) context
/// and a vision adapter, as decision models on the OpenAI-compatible System One route, and answer without a key.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn lmstudio_discovers_decision_models_and_decides_through_system_one() {
    let server = Server::start(|seen| match seen.path.as_str() {
        "/api/v1/models" => (200, Some(json!({"models": [
            lm("chat-model", "Chat", 8192),
            {"type": "decision", "key": "clef-flash", "display_name": "Clef Flash", "loaded_instances": [], "max_context_length": null, "has_vision_adapter": true}
        ]}))),
        _ => (200, Some(json!({"model": "clef-flash", "answers": {"refund": {"type": "noul", "noul": 0.9}}, "usage": {"input_tokens": 3, "output_tokens": 1}}))),
    })
    .await;
    let c = ctx(Kind::LmStudio, Some(&server.origin), HOUR);
    c.boot("/loc", &[]);
    eventually(|| c.model("/loc", &[], "lmstudio", "chat-model"), Option::is_some).await;
    let catalog = c.catalog("/loc", &[]).await;
    // Only chat models are offered as session models.
    assert!(catalog.model("lmstudio", "clef-flash").is_none());
    let decision = catalog
        .available_models_of(&["decision".to_string()])
        .into_iter()
        .find(|m| m.provider_id == "lmstudio")
        .cloned()
        .expect("the decision model is available");
    assert_eq!((decision.id.as_str(), decision.kind.as_deref(), decision.name.as_str()), ("clef-flash", Some("decision"), "Clef Flash"));
    assert_eq!(decision.package.as_deref(), Some("@opencode/ai/providers/openai-compatible/decisions"));
    assert_eq!(
        (decision.capabilities.tools, decision.capabilities.input.clone(), decision.capabilities.output.clone()),
        (false, vec!["text".to_string(), "image".into()], vec!["decision".to_string()])
    );
    assert_eq!(decision.limit.context, 200_000);

    // The provider has no key and no credential: the decision model is built without auth.
    let credential = c.models.credential(Some("/loc"), &catalog, "lmstudio").await;
    assert!(credential.is_none());
    let built = oc_ai::catalog::resolve::resolve_media(
        catalog.provider("lmstudio"),
        &decision,
        oc_ai::catalog::resolve::MediaType::Decision,
        None,
        None,
        &c.models.resolve_options(Some("/loc")),
    )
    .unwrap();
    let oc_ai::catalog::resolve::MediaModel::Decision(model) = built else { panic!("a decision model") };
    let questions = vec![json!({"name": "refund", "type": "predicate", "instructions": "Refund?"}).as_object().unwrap().clone()];
    let request = oc_ai::decision::DecisionRequest {
        model,
        input: vec![oc_ai::decision::DecisionPart::Text("Please refund.".into()), oc_ai::decision::DecisionPart::Media { media: oc_ai::MediaAsset::base64("AQID", "image/png"), filename: None }],
        questions,
        provider_options: None,
        http: None,
    };
    let response = oc_ai::decision::generate(&reqwest::Client::new(), request).await.unwrap();
    assert_eq!(response.answers.len(), 1);
    let sent = server.seen().into_iter().find(|s| s.path == "/v1/systemone").expect("a System One request");
    assert_eq!(sent.authorization, None);
    assert_eq!(sent.body["model"], "clef-flash");
    // Compatible hosts take inline media in root `images`.
    assert!(sent.body.get("images").is_some(), "{}", sent.body);
    assert!(sent.body.get("assets").is_none());
}

/// provider-lmstudio.test.ts (#63266): a key saved with /connect is sent as soon as it is saved
/// (`credential.switched`), and the integration stays so /connect can still manage it.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn lmstudio_discovers_with_a_key_saved_through_connect() {
    let server = Server::start(|seen| {
        if seen.authorization.as_deref() != Some("Bearer saved-key") {
            return (401, Some(json!({"error": {"code": "invalid_api_key"}})));
        }
        (200, Some(json!({"models": [lm("secured-model", "Secured", 8192)]})))
    })
    .await;
    let c = ctx(Kind::LmStudio, Some(&server.origin), HOUR);
    // The location's registry knows the integration from its first catalog.
    c.catalog("/loc", &[]).await;
    c.boot("/loc", &[]);
    eventually(|| async { server.seen().len() }, |n| *n > 0).await;
    assert_eq!(server.seen()[0].authorization, None);

    let integrations = c.models.integrations_for(Some("/loc"));
    integrations
        .store()
        .create(oc_ai::credentials::CreateCredential::new("lmstudio", oc_ai::credentials::CredentialValue::key("saved-key")))
        .await
        .unwrap();
    eventually(|| c.model("/loc", &[], "lmstudio", "secured-model"), Option::is_some).await;
    assert_eq!(server.seen().last().unwrap().authorization.as_deref(), Some("Bearer saved-key"));
    assert!(c.discovered("/loc", Kind::LmStudio).unwrap().connected);
    let catalog = c.catalog("/loc", &[]).await;
    assert!(integrations.get("lmstudio").await.unwrap().is_some(), "the integration stays");
    assert_eq!(catalog.provider("lmstudio").unwrap().integration_id.as_deref(), Some("lmstudio"));
    // The provider settings keep the configured key; requests use the credential through the integration.
    assert_eq!(catalog.provider("lmstudio").unwrap().settings.clone().map(Value::Object).unwrap()["apiKey"], "");
}

/// provider-vllm.test.ts (#63700): a request that takes longer than a second still lists the models.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn vllm_waits_for_a_slow_endpoint_instead_of_dropping_its_models() {
    let server = Server::start(|seen| match seen.path.as_str() {
        "/health" => (200, None),
        _ => (200, Some(json!({"object": "list", "data": [remote("slow-model", json!(4096), "vllm")]}))),
    })
    .await;
    server.set_delay(Duration::from_millis(1500));
    let c = ctx(Kind::Vllm, Some(&server.origin), HOUR);
    c.boot("/loc", &[]);
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    while c.model("/loc", &[], "vllm", "slow-model").await.is_none() {
        assert!(tokio::time::Instant::now() < deadline, "the slow endpoint's models never appeared");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// provider-vllm.test.ts (#63700): a configured endpoint's failure is reported once per outage; a default origin's
/// is not; a replaced endpoint's failure is never reported against the new one, and a slow host doesn't delay
/// discovery at another one.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn failures_are_reported_once_per_outage_for_configured_endpoints() {
    let down = Arc::new(std::sync::atomic::AtomicBool::new(true));
    let state = down.clone();
    let server = Server::start(move |seen| {
        if state.load(std::sync::atomic::Ordering::SeqCst) {
            return (503, None);
        }
        match seen.path.as_str() {
            "/health" => (200, None),
            _ => (200, Some(json!({"object": "list", "data": [remote("back", json!(4096), "vllm")]}))),
        }
    })
    .await;
    // The default origin is not expected to answer.
    let c = ctx(Kind::Vllm, Some(&server.origin), FAST);
    c.boot("/default", &[]);
    eventually(|| async { server.count("/health") }, |n| *n >= 3).await;
    assert!(!c.plugins.local.failing("/default", Kind::Vllm));

    let entries = vec![config(json!({"vllm": {"settings": {"baseURL": format!("{}/v1", server.origin)}}}))];
    c.boot("/configured", &entries);
    eventually(|| async { c.plugins.local.failing("/configured", Kind::Vllm) }, |f| *f).await;
    // The endpoint comes back: the outage is over.
    down.store(false, std::sync::atomic::Ordering::SeqCst);
    eventually(|| c.model("/configured", &entries, "vllm", "back"), Option::is_some).await;
    assert!(!c.plugins.local.failing("/configured", Kind::Vllm));

    // A request to a slow endpoint is replaced by a healthy one before it fails.
    let slow = Server::start(|_| (503, None)).await;
    slow.set_delay(Duration::from_secs(2));
    let healthy = Server::start(|seen| match seen.path.as_str() {
        "/health" => (200, None),
        _ => (200, Some(json!({"object": "list", "data": [remote("healthy-model", json!(4096), "vllm")]}))),
    })
    .await;
    let c = ctx(Kind::Vllm, None, HOUR);
    let entries = vec![config(json!({"vllm": {"settings": {"baseURL": format!("{}/v1", slow.origin)}}}))];
    c.boot("/loc", &entries);
    eventually(|| async { slow.count("/health") }, |n| *n >= 1).await;
    let entries = vec![config(json!({"vllm": {"settings": {"baseURL": format!("{}/v1", healthy.origin)}}}))];
    c.reconfigure("/loc", &entries);
    eventually(|| c.model("/loc", &entries, "vllm", "healthy-model"), Option::is_some).await;
    // The replaced endpoint's request fails afterwards: nothing is reported against the new one.
    tokio::time::sleep(Duration::from_millis(2500)).await;
    assert!(!c.plugins.local.failing("/loc", Kind::Vllm));
    assert!(c.model("/loc", &entries, "vllm", "healthy-model").await.is_some());
}

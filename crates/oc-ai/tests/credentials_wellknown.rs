use oc_ai::credentials::callback::{self, Request, Response, sync_handler};
use oc_ai::credentials::integration::{Integrations, Method};
use oc_ai::credentials::kv::{KvStore, MemoryKv};
use oc_ai::credentials::store::MemoryCredentialStore;
use oc_ai::credentials::wellknown::{self, Entry, Manifest, WellKnown};
use oc_ai::credentials::{map_env, system_clock};
use parking_lot::Mutex;
use serde_json::{Value, json};
use std::collections::HashMap;
use std::sync::Arc;
use tokio_util::sync::{CancellationToken, DropGuard};

struct Mock {
    base: String,
    requests: Arc<Mutex<Vec<Request>>>,
    up: Arc<std::sync::atomic::AtomicBool>,
    _guard: DropGuard,
}

async fn mock(route: impl Fn(&Request) -> Response + Send + Sync + 'static) -> Mock {
    let requests = Arc::new(Mutex::new(Vec::<Request>::new()));
    let up = Arc::new(std::sync::atomic::AtomicBool::new(true));
    let recorded = requests.clone();
    let alive = up.clone();
    let handler = sync_handler(move |request| {
        recorded.lock().push(request.clone());
        if !alive.load(std::sync::atomic::Ordering::SeqCst) {
            return Response::new(503, "down");
        }
        route(&request)
    });
    let bound = callback::bind("127.0.0.1", 0).await.unwrap();
    let base = format!("http://127.0.0.1:{}", bound.port);
    let scope = CancellationToken::new();
    bound.serve(handler, scope.clone());
    Mock { base, requests, up, _guard: scope.drop_guard() }
}

fn http() -> reqwest::Client {
    reqwest::Client::builder().no_proxy().build().unwrap()
}

#[test]
fn env_substitution() {
    let mut variables = HashMap::new();
    variables.insert("TOKEN".to_string(), "secret".to_string());
    let env = map_env([("REGION", "eu"), ("TOKEN", "from-env")]);
    assert_eq!(
        wellknown::substitute("https://{env:REGION}.x/{env:TOKEN}/{env:MISSING}", &variables, &env),
        "https://eu.x/secret/"
    );
    assert_eq!(wellknown::substitute("plain", &variables, &env), "plain");
    assert_eq!(wellknown::manifest_url("https://corp.example.com///"), "https://corp.example.com/.well-known/opencode");
}

#[test]
fn manifest_decoding() {
    let manifest: Manifest = serde_json::from_value(json!({"auth":{"command":["corp","login"],"env":"CORP_TOKEN"},"config":null})).unwrap();
    assert_eq!(manifest.config, Some(None));
    assert_eq!(manifest.auth.unwrap().env, "CORP_TOKEN");
    let manifest: Manifest = serde_json::from_value(json!({})).unwrap();
    assert_eq!(manifest.config, None);
    assert!(serde_json::from_value::<Manifest>(json!({"auth":{"command":"x"}})).is_err());
}

#[tokio::test]
async fn resolve_inline_and_remote_config() {
    let server = mock(|request| match request.path.as_str() {
        "/.well-known/opencode" => Response::json(
            200,
            json!({
                "auth": {"command": ["corp", "login"], "env": "CORP_TOKEN"},
                "config": {"model": "corp/fast"},
                "remote_config": {"url": "REPLACE/config?region={env:REGION}", "headers": {"Authorization": "Bearer {env:CORP_TOKEN}"}}
            })
            .to_string(),
        ),
        "/config" => Response::json(200, json!({"config": {"provider": {"corp": {}}}}).to_string()),
        _ => Response::not_found("nope"),
    })
    .await;
    let mut manifest = wellknown::inspect(&http(), &format!("{}/", server.base)).await.unwrap();
    manifest.remote_config.as_mut().unwrap().url = manifest.remote_config.as_ref().unwrap().url.replace("REPLACE", &server.base);
    let entry = Entry { origin: server.base.clone(), integration_id: server.base.clone(), manifest };
    let variables = WellKnown::variables(&entry, Some("tok-1"));
    let env = map_env([("REGION", "eu")]);
    let configs = wellknown::resolve_entry(&http(), &entry, &variables, &env).await.unwrap();
    assert_eq!(serde_json::to_value(&configs).unwrap(), json!([{"model":"corp/fast"},{"provider":{"corp":{}}}]));
    let remote = server.requests.lock().iter().find(|r| r.path == "/config").cloned().unwrap();
    assert_eq!(remote.query, "region=eu");
    assert!(remote.headers.contains(&("authorization".to_string(), "Bearer tok-1".to_string())));
    assert!(remote.headers.contains(&("accept".to_string(), "application/json".to_string())));

    // Registration on the integration registry.
    let integrations = Integrations::with(MemoryCredentialStore::shared(), map_env::<[(&str, &str); 0], &str, &str>([]), system_clock());
    wellknown::register(&integrations, &[entry.clone()]);
    let info = integrations.get(&entry.origin).await.unwrap().unwrap();
    assert_eq!(info.name, "127.0.0.1");
    assert_eq!(info.methods, vec![Method::Command { id: "login".into(), label: "Log in".into(), command: vec!["corp".into(), "login".into()] }]);
}

#[tokio::test]
async fn bare_remote_config_and_errors() {
    let server = mock(|request| match request.path.as_str() {
        "/bare" => Response::json(200, json!({"theme": "dark"}).to_string()),
        "/.well-known/opencode" => Response::json(200, json!({"config": {"a": 1}}).to_string()),
        _ => Response::not_found("nope"),
    })
    .await;
    let manifest: Manifest = serde_json::from_value(json!({"remote_config": {"url": format!("{}/bare", server.base)}})).unwrap();
    let entry = Entry { origin: server.base.clone(), integration_id: server.base.clone(), manifest };
    let env = map_env::<[(&str, &str); 0], &str, &str>([]);
    let configs = wellknown::resolve_entry(&http(), &entry, &HashMap::new(), &env).await.unwrap();
    assert_eq!(serde_json::to_value(&configs).unwrap(), json!([{"theme":"dark"}]));

    let error = wellknown::inspect(&http(), &format!("{}/missing", server.base)).await.unwrap_err();
    assert_eq!(error.message, format!("Failed to load wellknown manifest from {}/missing/.well-known/opencode", server.base));

    let kv: Arc<MemoryKv> = Arc::new(MemoryKv::new());
    let service = WellKnown::new(http(), kv.clone(), env);
    let error = service.add(&server.base).await.unwrap_err();
    assert_eq!(error.message, format!("No authentication method found at {}", server.base));
    assert_eq!(kv.get(wellknown::SOURCES_KEY).await.unwrap(), None);
}

#[tokio::test]
async fn add_cache_and_remove_sources() {
    let server = mock(|request| match request.path.as_str() {
        "/.well-known/opencode" => {
            Response::json(200, json!({"auth": {"command": ["corp"], "env": "CORP"}, "config": {"k": "v"}}).to_string())
        }
        _ => Response::not_found("nope"),
    })
    .await;
    let kv: Arc<MemoryKv> = Arc::new(MemoryKv::new());
    let env = map_env::<[(&str, &str); 0], &str, &str>([]);
    let service = WellKnown::new(http(), kv.clone(), env.clone());
    let mut updates = service.subscribe();
    let entry = service.add(&format!("{}/", server.base)).await.unwrap();
    assert_eq!(entry.origin, server.base);
    assert!(updates.try_recv().is_ok());
    assert_eq!(kv.get(wellknown::SOURCES_KEY).await.unwrap(), Some(json!([server.base])));
    assert!(kv.get(&wellknown::manifest_key(&server.base)).await.unwrap().is_some());
    let configs = service.resolve(&entry, &HashMap::new()).await.unwrap();
    assert_eq!(serde_json::to_value(&configs).unwrap(), json!([{"k":"v"}]));
    assert_eq!(service.cached(&server.base).await, configs);

    // A restarted service falls back to the cached manifest when the origin is down.
    server.up.store(false, std::sync::atomic::Ordering::SeqCst);
    let restarted = WellKnown::new(http(), kv.clone(), env);
    let entries = restarted.entries().await.unwrap();
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].manifest.auth.as_ref().unwrap().env, "CORP");
    assert_eq!(restarted.refresh().await.unwrap(), false);
    server.up.store(true, std::sync::atomic::Ordering::SeqCst);

    service.remove(&server.base).await.unwrap();
    assert_eq!(kv.get(wellknown::SOURCES_KEY).await.unwrap(), Some(Value::Array(vec![])));
    assert_eq!(kv.get(&wellknown::config_key(&server.base)).await.unwrap(), None);
    assert!(service.snapshot().is_empty());
    assert!(service.cached(&server.base).await.is_empty());
}

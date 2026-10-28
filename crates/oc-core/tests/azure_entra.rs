//! The Azure plugin's Entra ID hooks (`packages/core/src/plugin/provider/azure.ts`): with the Azure CLI sign-in
//! active, the `experimental.ws.handshake` hook and the `http.request` hook replace the placeholder `api-key` with
//! a bearer token minted for the target URL's scope; key sign-ins and other providers are left alone.

use std::sync::Arc;

use oc_ai::credentials::CredentialValue;
use oc_ai::credentials::azure::AzCli;
use oc_ai::credentials::store::CreateCredential;
use oc_ai::route::transport::WireRequest;
use oc_core::models::Models;
use oc_core::plugin_request::{self, Kind};
use oc_core::plugins::Plugins;
use oc_schema::types::ModelRef;
use parking_lot::Mutex;

const KEY: &str = "/loc";

struct FakeAz {
    calls: Mutex<Vec<String>>,
}

#[async_trait::async_trait]
impl AzCli for FakeAz {
    async fn run(&self, args: &[&str]) -> Result<String, String> {
        self.calls.lock().push(args.join(" "));
        let scope = args.iter().skip_while(|a| **a != "--scope").nth(1).copied().unwrap_or_default();
        let token = if scope.contains("ai.azure.com") { "foundry-token" } else { "cognitive-token" };
        Ok(format!("{{\"accessToken\":\"{token}\",\"expires_on\":4102444800}}"))
    }
}

struct Env {
    _home: tempfile::TempDir,
    models: Arc<Models>,
    plugins: Arc<Plugins>,
    az: Arc<FakeAz>,
}

fn setup() -> Env {
    let home = tempfile::tempdir().unwrap();
    // SAFETY: every test of this binary sets the same values.
    unsafe {
        std::env::set_var("OPENCODE_DISABLE_MODELS_FETCH", "1");
        std::env::set_var("OPENCODE_TEST_HOME", home.path());
        for name in ["AZURE_API_KEY", "AZURE_COGNITIVE_SERVICES_API_KEY"] {
            std::env::remove_var(name);
        }
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
    let az = Arc::new(FakeAz { calls: Mutex::new(Vec::new()) });
    plugins.azure.set_cli(az.clone());
    // The location's integration registry (where the hooks look up the active `azure` connection).
    models.integrations_for(Some(KEY));
    Env { _home: home, models, plugins, az }
}

async fn sign_in(env: &Env, value: CredentialValue) {
    let store = env.models.integrations.store();
    store.create(CreateCredential { id: None, integration_id: "azure".into(), value, label: None, activate: Some(true) }).await.unwrap();
}

fn azure_cli() -> CredentialValue {
    CredentialValue::External { method_id: "azure-cli".into(), metadata: None }
}

fn model(provider: &str) -> ModelRef {
    ModelRef { id: "gpt-5".into(), provider_id: provider.into(), variant: None }
}

fn handshake_headers() -> oc_ai::route::Headers {
    [("api-key", "azure-cli"), ("openai-beta", "x"), ("user-agent", "route")].iter().map(|(k, v)| (k.to_string(), v.to_string())).collect()
}

#[tokio::test]
async fn entra_token_on_the_websocket_handshake() {
    let env = setup();
    sign_in(&env, azure_cli()).await;
    let ws = plugin_request::ws_hooks(&env.plugins, KEY, Kind::Primary, "ses_1", "build", &model("azure")).expect("azure handshake hook");
    let url = "wss://res.openai.azure.com/openai/v1/responses";
    let (out_url, headers) = ws.handshake(url.into(), handshake_headers()).await.unwrap();
    assert_eq!(out_url, url);
    assert_eq!(headers.get("authorization").map(String::as_str), Some("Bearer cognitive-token"));
    assert_eq!(headers.get("api-key"), None);
    assert_eq!(headers.get("user-agent").map(String::as_str), Some(oc_core::copilot::user_agent("1.2.3").as_str()));
    assert_eq!(headers.get("openai-beta").map(String::as_str), Some("x"));
    // Foundry hosts use the AI scope; the token is cached per scope.
    let (_, headers) = ws.handshake("wss://res.services.ai.azure.com/openai/v1/responses".into(), handshake_headers()).await.unwrap();
    assert_eq!(headers.get("authorization").map(String::as_str), Some("Bearer foundry-token"));
    ws.handshake(url.into(), handshake_headers()).await.unwrap();
    assert_eq!(
        *env.az.calls.lock(),
        [
            "account get-access-token --scope https://cognitiveservices.azure.com/.default --output json",
            "account get-access-token --scope https://ai.azure.com/.default --output json",
        ]
    );
}

#[tokio::test]
async fn entra_token_on_http_requests() {
    let env = setup();
    sign_in(&env, azure_cli()).await;
    let http = plugin_request::http_hooks(&env.plugins, KEY, Kind::Primary, "ses_1", "build", &model("azure")).expect("azure http hook");
    let request = WireRequest {
        url: "https://res.openai.azure.com/openai/v1/responses?api-version=v1".into(),
        method: "POST".into(),
        headers: [("api-key", "azure-cli"), ("x-api-key", "y"), ("content-type", "application/json")]
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect(),
        body: Some("{}".into()),
    };
    let out = http.request(request).await.unwrap();
    assert_eq!(out.headers.keys().map(String::as_str).collect::<Vec<_>>(), ["content-type", "authorization", "user-agent"]);
    assert_eq!(out.headers["authorization"], "Bearer cognitive-token");
}

#[tokio::test]
async fn key_sign_in_and_other_providers_are_untouched() {
    let env = setup();
    sign_in(&env, CredentialValue::key("secret")).await;
    let ws = plugin_request::ws_hooks(&env.plugins, KEY, Kind::Primary, "ses_1", "build", &model("azure")).expect("azure handshake hook");
    let (_, headers) = ws.handshake("wss://res.openai.azure.com/openai/v1/responses".into(), handshake_headers()).await.unwrap();
    assert_eq!(headers, handshake_headers());
    assert!(env.az.calls.lock().is_empty());
    // No hook at all for another provider (without plugin hooks).
    assert!(plugin_request::ws_hooks(&env.plugins, KEY, Kind::Primary, "ses_1", "build", &model("openai")).is_none());
    assert!(plugin_request::http_hooks(&env.plugins, KEY, Kind::Primary, "ses_1", "build", &model("openai")).is_none());
}

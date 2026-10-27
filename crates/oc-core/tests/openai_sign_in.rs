//! The OpenAI sign-in plugins at a location (`packages/core/test/plugin/provider-openai.test.ts`,
//! `provider-chatgpt.test.ts`): the loaded sign-in state, the Codex and token-sharing catalogs built
//! from the models.dev snapshot, their `session.model.request` headers and base URL, the output limit
//! the OpenAI plugin drops, the token-sharing retry stop, credential switches, and the move of
//! token-sharing credentials saved under the browser method.

use std::sync::Arc;

use oc_ai::LLMRequest;
use oc_ai::catalog::openai::{CODEX_BASE_URL, SignIn, TOKEN_SHARING_BASE_URL};
use oc_ai::credentials::CredentialValue;
use oc_ai::credentials::store::CreateCredential;
use oc_ai::types::GenerationOptions;
use oc_config::schema::{Entry, Info};
use oc_core::models::Models;
use oc_core::plugin_request::{self, Kind};
use oc_core::plugins::Plugins;
use oc_schema::types::{ErrorResponse, SessionError};
use serde_json::{Value, json};

const KEY: &str = "/loc";
/// 2100-01-01: never refreshed.
const FAR: u64 = 4_102_444_800_000;

struct Env {
    _home: tempfile::TempDir,
    db: oc_storage::Database,
    models: Arc<Models>,
    plugins: Arc<Plugins>,
}

fn setup() -> Env {
    let home = tempfile::tempdir().unwrap();
    // SAFETY: every test of this binary sets the same values.
    unsafe {
        std::env::set_var("OPENCODE_DISABLE_MODELS_FETCH", "1");
        std::env::set_var("OPENCODE_TEST_HOME", home.path());
        for name in ["OPENAI_API_KEY", "OPENCODE_API_KEY", "GITHUB_TOKEN"] {
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
    let plugins = Plugins::new(global, db.clone(), bus, "1.2.3");
    models.set_plugins(plugins.clone());
    Env { _home: home, db, models, plugins }
}

fn entries(config: Value) -> Vec<Entry> {
    vec![Entry::document(None, Info::from_value(&config).expect("config document"))]
}

fn oauth(method: &str, metadata: Value) -> CredentialValue {
    CredentialValue::Oauth {
        method_id: method.into(),
        refresh: "fake-refresh".into(),
        access: "fake-access".into(),
        expires: FAR,
        metadata: metadata.as_object().cloned(),
    }
}

async fn create(env: &Env, value: CredentialValue) -> String {
    let store = env.models.integrations.store();
    let input = CreateCredential { id: None, integration_id: "openai".into(), value, label: None, activate: Some(true) };
    store.create(input).await.unwrap().id
}

/// A primary request for `openai/<model>` as the runner assembles it, shaped by the plugins.
async fn request(env: &Env, config: &[Entry], provider: &str, model: &str, kind: Kind) -> LLMRequest {
    let catalog = env.models.catalog_at(config, Some(KEY)).await.unwrap();
    let credential = env.models.credential(Some(KEY), &catalog, provider).await;
    let resolved = env.models.resolve_model(Some(KEY), &catalog, provider, model, None, credential.as_ref()).await.unwrap();
    let mut req = LLMRequest::new(resolved.language_model.clone());
    let mut headers = indexmap::IndexMap::new();
    headers.insert("x-opencode-session-id".to_string(), "ses_child".to_string());
    headers.insert("x-session-affinity".to_string(), "ses_parent".to_string());
    req.http = Some(oc_ai::types::HttpOptions { headers: Some(headers), ..Default::default() });
    req.generation = Some(GenerationOptions { max_tokens: Some(128_000.0), ..Default::default() });
    plugin_request::shape(&env.plugins, KEY, kind, "ses_child", "build", &resolved.model_ref(), &mut req, None).await.unwrap();
    req
}

fn header<'a>(req: &'a LLMRequest, name: &str) -> Option<&'a str> {
    req.http.as_ref()?.headers.as_ref()?.get(name).map(String::as_str)
}

#[tokio::test]
async fn codex_sign_in_routes_to_codex_with_its_headers_and_eligible_models() {
    let env = setup();
    let config = entries(json!({}));
    create(&env, oauth("chatgpt-browser", json!({"accountID": "acct_123"}))).await;
    let catalog = env.models.catalog_at(&config, Some(KEY)).await.unwrap();
    assert_eq!(env.plugins.openai.get(KEY), SignIn::Codex { account_id: Some("acct_123".into()) });

    let provider = catalog.provider("openai").unwrap();
    let settings = provider.settings.as_ref().unwrap();
    assert_eq!(settings["baseURL"], CODEX_BASE_URL);
    assert_eq!(settings["encoding"], "zstd");
    assert_eq!(settings["transport"], "websocket");
    let headers = provider.headers.as_ref().unwrap();
    assert_eq!(headers["originator"], "opencode");
    assert_eq!(headers["chatgpt-account-id"], "acct_123");
    assert_eq!(headers["x-codex-beta-features"], "remote_compaction_v2");
    let enabled: Vec<&str> = catalog.available_models().into_iter().filter(|m| m.provider_id == "openai").map(|m| m.id.as_str()).collect();
    assert!(enabled.contains(&"gpt-6.1-sol") && enabled.contains(&"gpt-5.6-sol"));
    for id in ["gpt-5.5", "gpt-5.6", "gpt-5.4", "gpt-4.1", "gpt-5.5-pro", "gpt-5.3-codex-spark"] {
        assert!(!enabled.contains(&id), "{id}");
    }
    // Costs are zeroed and the plan's input caps apply.
    let sol = catalog.model("openai", "gpt-6.1-sol").unwrap();
    assert!(sol.cost.is_empty());
    assert_eq!((sol.limit.input, sol.limit.context), (Some(500_000), 628_000));
    let older = catalog.model("openai", "gpt-5.6-sol").unwrap();
    assert_eq!((older.limit.input, older.limit.context), (Some(272_000), 400_000));

    let req = request(&env, &config, "openai", "gpt-6.1-sol", Kind::Primary).await;
    assert_eq!(req.model.route.endpoint.base_url.as_deref(), Some(CODEX_BASE_URL));
    assert_eq!(header(&req, "originator"), Some("opencode"));
    // ChatGPT routes its prompt cache on `session-id`, so children share the parent's.
    assert_eq!(header(&req, "session-id"), Some("ses_parent"));
    assert_eq!(header(&req, "thread-id"), None);
    assert!(req.generation.is_none(), "no output limit: {:?}", req.generation);
    let compiled = oc_ai::Client::new().compile(&req).await.unwrap();
    assert_eq!(compiled.prepared.url, format!("{CODEX_BASE_URL}/responses"));
    let sent = |name: &str| compiled.prepared.headers.iter().find(|(k, _)| k.eq_ignore_ascii_case(name)).map(|(_, v)| v.clone());
    assert_eq!(sent("chatgpt-account-id").as_deref(), Some("acct_123"));
    assert_eq!(sent("originator").as_deref(), Some("opencode"));
    assert_eq!(sent("authorization").as_deref(), Some("Bearer fake-access"));
    assert!(compiled.prepared.json_body.get("max_output_tokens").is_none());
}

#[tokio::test]
async fn codex_moves_a_direct_openai_base_url_but_keeps_a_proxy() {
    let env = setup();
    create(&env, oauth("chatgpt-headless", json!({}))).await;
    for (base, expected) in
        [("https://api.openai.com/v1", CODEX_BASE_URL), ("https://proxy.example/v1?region=us", "https://proxy.example/v1?region=us")]
    {
        let config = entries(json!({"providers": {"openai": {"models": {"gpt-6.1-sol": {"settings": {"baseURL": base}}}}}}));
        let req = request(&env, &config, "openai", "gpt-6.1-sol", Kind::Primary).await;
        assert_eq!(req.model.route.endpoint.base_url.as_deref(), Some(expected));
        assert_eq!(header(&req, "originator"), Some("opencode"));
        assert_eq!(header(&req, "session-id"), Some("ses_parent"));
    }
}

#[tokio::test]
async fn token_sharing_routes_over_http_with_fallback_models_and_session_headers() {
    let env = setup();
    let config = entries(json!({}));
    let id = create(&env, oauth("chatgpt-token-sharing", json!({"clientID": "oaiapp_issued"}))).await;
    let catalog = env.models.catalog_at(&config, Some(KEY)).await.unwrap();
    assert_eq!(env.plugins.openai.get(KEY), SignIn::TokenSharing { connection: format!("credential:{id}") });
    let provider = catalog.provider("openai").unwrap();
    let settings = provider.settings.as_ref().unwrap();
    assert_eq!(settings["baseURL"], TOKEN_SHARING_BASE_URL);
    assert_eq!(settings["transport"], "http");
    assert_eq!(settings["encoding"], "zstd");
    assert!(provider.headers.as_ref().is_none_or(|h| !h.contains_key("originator")));
    let mut ids: Vec<&str> = catalog.provider_models("openai").into_iter().map(|m| m.id.as_str()).collect();
    ids.sort();
    let mut fallback: Vec<&str> = oc_ai::catalog::openai::FALLBACK_MODELS.iter().copied().filter(|id| ids.contains(id)).collect();
    fallback.sort();
    assert_eq!(ids, fallback, "only fallback models remain");
    let model = catalog.model("openai", "gpt-6.1-sol").unwrap();
    assert!(model.cost.is_empty());
    assert_eq!(model.compatibility.as_ref().unwrap().supports_effort_updates, Some(false));

    let req = request(&env, &config, "openai", "gpt-6.1-sol", Kind::Primary).await;
    assert_eq!(header(&req, "session-id"), Some("ses_parent"));
    assert_eq!(header(&req, "thread-id"), Some("ses_child"));
    assert_eq!(header(&req, "x-client-request-id"), Some("ses_child"));
    assert_eq!(header(&req, "originator"), None);
    let resolved = env
        .models
        .resolve_for_session(
            Some(KEY),
            &catalog,
            "ses_child",
            None,
            Some(&oc_schema::types::ModelRef { id: "gpt-6.1-sol".into(), provider_id: "openai".into(), variant: None }),
        )
        .await
        .unwrap();
    assert_eq!(resolved.compaction.as_deref(), Some("summary"));
    assert_eq!(resolved.transport.as_deref(), Some("http"));
    let compiled = oc_ai::Client::new().compile(&req).await.unwrap();
    assert_eq!(compiled.prepared.url, format!("{TOKEN_SHARING_BASE_URL}/responses"));

    // Sharing failures that will not clear stop retrying; other failures keep the decision.
    let model = resolved.model_ref();
    assert!(plugin_request::has_retry_hook(&env.plugins, KEY, "openai"));
    let error = |body: &str| SessionError {
        kind: "provider.rate-limit".into(),
        message: "Rate limit exceeded".into(),
        status: Some(429),
        response: Some(ErrorResponse { body: body.into() }),
        retry_after_ms: None,
    };
    let decision = json!({"retry": true, "delay": 1000});
    let limit = error(r#"{"error":{"code":"subscription_sharing_usage_limit_exceeded"}}"#);
    let out = plugin_request::retry(&env.plugins, KEY, "ses_child", "build", &model, &limit, 2, decision.clone()).await;
    assert_eq!(out, json!({"retry": false}));
    for (code, retry) in [
        ("subscription_sharing_user_not_eligible", false),
        ("subscription_sharing_unsupported_capability", false),
        ("subscription_sharing_route_not_supported", false),
        ("subscription_sharing_invalid_user", false),
        ("chatpass_v2_scope_not_authorized", false),
        ("chatpass_v2_invalid_authorization_context", false),
        ("subscription_sharing_usage_unavailable", true),
        ("subscription_sharing_user_unavailable", true),
    ] {
        let body = json!({"type": "response.failed", "response": {"error": {"code": code}}}).to_string();
        let out = plugin_request::retry(&env.plugins, KEY, "ses_child", "build", &model, &error(&body), 1, decision.clone()).await;
        assert_eq!(out["retry"], json!(retry), "{code}");
    }
    let other = plugin_request::retry(&env.plugins, KEY, "ses_child", "build", &model, &error("{}"), 1, decision.clone()).await;
    assert_eq!(other, decision);
}

#[tokio::test]
async fn api_keys_keep_the_full_catalog_and_retries_but_drop_the_openai_output_limit() {
    let env = setup();
    let config = entries(json!({
        "providers": {"mock": {
            "package": "@opencode/ai/providers/openai-compatible",
            "settings": {"baseURL": "http://127.0.0.1:9/v1", "apiKey": "test"},
            "models": {"m1": {}}
        }}
    }));
    create(&env, CredentialValue::key("sk-fake")).await;
    let catalog = env.models.catalog_at(&config, Some(KEY)).await.unwrap();
    assert_eq!(env.plugins.openai.get(KEY), SignIn::None);
    let provider = catalog.provider("openai").unwrap();
    let settings = provider.settings.as_ref().unwrap();
    assert_eq!(settings["transport"], "websocket");
    assert!(settings.get("encoding").is_none() && settings.get("baseURL").is_none());
    assert!(catalog.model("openai", "gpt-4.1").unwrap().enabled);
    assert!(!catalog.model("openai", "gpt-5.5").unwrap().cost.is_empty());
    assert_eq!(catalog.model("openai", "gpt-6.1-sol").unwrap().limit.input, Some(922_000));

    for kind in [Kind::Primary, Kind::Compaction] {
        let req = request(&env, &config, "openai", "gpt-6.1-sol", kind).await;
        assert!(req.generation.is_none(), "{kind:?}");
        assert_eq!(header(&req, "originator"), None);
        assert_eq!(header(&req, "session-id"), None);
        assert_eq!(req.model.route.endpoint.base_url.as_deref(), Some("https://api.openai.com/v1"));
        let mock = request(&env, &config, "mock", "m1", kind).await;
        assert_eq!(mock.generation.and_then(|g| g.max_tokens), Some(128_000.0), "{kind:?}");
    }
    assert!(!plugin_request::has_retry_hook(&env.plugins, KEY, "openai"));
}

/// The verbosity plugin (`verbosity.ts`) on requests: known OpenAI Responses models answer with low
/// verbosity on every request kind; other models are left alone.
#[tokio::test]
async fn known_openai_responses_models_default_to_low_text_verbosity() {
    let env = setup();
    let config = entries(json!({
        "providers": {
            "mock": {
                "package": "@opencode/ai/providers/openai-compatible",
                "settings": {"baseURL": "http://127.0.0.1:9/v1", "apiKey": "test"},
                "models": {"m1": {}}
            },
            "openai": {"models": {"gpt-5.5": {"settings": {"textVerbosity": "medium"}}}}
        }
    }));
    create(&env, CredentialValue::key("sk-fake")).await;
    for kind in [Kind::Primary, Kind::Compaction, Kind::Generate, Kind::Title] {
        let req = request(&env, &config, "openai", "gpt-6.1-sol", kind).await;
        assert_eq!(req.provider_options.as_ref().and_then(|o| o.get("textVerbosity")), Some(&json!("low")), "{kind:?}");
        let body = oc_ai::testing::compile_body(&req).await.unwrap();
        assert_eq!(body["text"]["verbosity"], "low", "{kind:?}");
    }
    for (provider, model) in [("openai", "gpt-4.1"), ("openai", "gpt-5.5"), ("mock", "m1")] {
        let req = request(&env, &config, provider, model, Kind::Primary).await;
        assert!(req.provider_options.as_ref().is_none_or(|o| !o.contains_key("textVerbosity")), "{provider}/{model}");
    }
    let configured = request(&env, &config, "openai", "gpt-5.5", Kind::Primary).await;
    assert_eq!(oc_ai::testing::compile_body(&configured).await.unwrap()["text"]["verbosity"], "medium");
}

#[tokio::test]
async fn switches_between_token_sharing_and_codex_credentials_without_mixing_their_routes() {
    let env = setup();
    let config = entries(json!({}));
    let legacy = create(&env, oauth("chatgpt-browser", json!({"accountID": "acct_123"}))).await;
    let sharing = create(&env, oauth("chatgpt-token-sharing", json!({"clientID": "oaiapp_issued"}))).await;
    let base = |catalog: &oc_ai::catalog::Catalog| catalog.provider("openai").unwrap().settings.as_ref().unwrap()["baseURL"].clone();
    let catalog = env.models.catalog_at(&config, Some(KEY)).await.unwrap();
    assert_eq!(base(&catalog), TOKEN_SHARING_BASE_URL);

    let store = env.models.integrations.store();
    store.activate(&legacy).await.unwrap();
    let catalog = env.models.catalog_at(&config, Some(KEY)).await.unwrap();
    assert_eq!(base(&catalog), CODEX_BASE_URL);
    assert_eq!(catalog.provider("openai").unwrap().headers.as_ref().unwrap()["chatgpt-account-id"], "acct_123");

    store.activate(&sharing).await.unwrap();
    let catalog = env.models.catalog_at(&config, Some(KEY)).await.unwrap();
    assert_eq!(base(&catalog), TOKEN_SHARING_BASE_URL);
    let provider = catalog.provider("openai").unwrap();
    assert_eq!(provider.settings.as_ref().unwrap()["transport"], "http");
    assert!(provider.headers.as_ref().is_none_or(|h| !h.contains_key("chatgpt-account-id")));
}

#[tokio::test]
async fn moves_only_token_sharing_credentials_to_the_new_method_id_and_caches_their_saved_models() {
    let env = setup();
    let snapshot = json!([{
        "slug": "gpt-5.5", "display_name": "GPT-5.5", "visibility": "list", "supported_in_api": true,
        "context_window": 272_000, "input_modalities": ["text"],
    }]);
    let legacy = create(&env, oauth("chatgpt-browser", json!({"accountID": "acct_123"}))).await;
    let sharing = create(&env, oauth("chatgpt-browser", json!({"clientID": "oaiapp_issued", "models": snapshot}))).await;
    env.models.catalog_at(&entries(json!({})), Some(KEY)).await.unwrap();
    let store = env.models.integrations.store();
    let method = |value: &CredentialValue| match value {
        CredentialValue::Oauth { method_id, metadata, .. } => (method_id.clone(), metadata.clone().map(Value::Object)),
        _ => unreachable!(),
    };
    let (legacy_method, _) = method(&store.get(&legacy).await.unwrap().unwrap().value);
    assert_eq!(legacy_method, "chatgpt-browser");
    let (sharing_method, metadata) = method(&store.get(&sharing).await.unwrap().unwrap().value);
    assert_eq!(sharing_method, "chatgpt-token-sharing");
    assert_eq!(metadata, Some(json!({"clientID": "oaiapp_issued"})));
    let namespace: String = "opencode.provider.chatgpt".encode_utf16().map(|u| format!("{u:04x}")).collect();
    assert_eq!(env.db.kv_get(&format!("plugin:{namespace}:models:oaiapp_issued")).unwrap(), Some(snapshot));
    // The active connection is now a token-sharing one.
    assert!(matches!(env.plugins.openai.get(KEY), SignIn::TokenSharing { .. }));
}

#[test]
fn registers_the_context_caps_plugin_after_the_openai_plugin() {
    let ids = oc_core::plugins::BUILTIN_PLUGINS;
    assert_eq!(ids.len(), 90);
    let openai = ids.iter().position(|id| *id == "opencode.provider.openai").unwrap();
    assert_eq!(ids[openai + 1], "opencode.provider.openai-context-caps");
    assert!(ids.iter().position(|id| *id == "opencode.provider.chatgpt").unwrap() < openai);
}

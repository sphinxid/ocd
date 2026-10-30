//! Provider transforms of the Google Vertex, Amazon Bedrock, Cloudflare, Databricks and GitLab plugins (2.2
//! `packages/core/test/plugin/provider-{google-vertex,amazon-bedrock,databricks,cloudflare-ai-gateway,
//! cloudflare-workers-ai,gitlab}.test.ts`), plus the resolver's `${AWS_REGION}` rule.

use oc_ai::catalog::registry::{Access, Catalog, CatalogPlugin, ModelEditor, ProviderEditor};
use oc_ai::catalog::transforms::{metadata_settings, CredentialLogins, Env, ProviderTransforms};
use oc_ai::catalog::types::{Activation, ModelInfo, ProviderInfo};
use oc_ai::credentials::CredentialValue;
use serde_json::{json, Value};
use std::sync::Arc;

fn env(vars: &[(&str, &str)]) -> Env {
    let vars: Vec<(String, String)> = vars.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect();
    Arc::new(move |name| vars.iter().find(|(k, _)| k == name).map(|(_, v)| v.clone()))
}

/// Seeds providers (and models) the way the catalog would before the provider plugins run.
struct Seed(Vec<(ProviderInfo, Vec<ModelInfo>)>);

impl CatalogPlugin for Seed {
    fn providers(&mut self, editor: &mut ProviderEditor) {
        for (info, models) in &self.0 {
            editor.add(info.clone(), models.clone(), None);
        }
    }
}

struct Enable;
impl CatalogPlugin for Enable {
    fn providers(&mut self, _: &mut ProviderEditor) {}
    fn models(&mut self, _: &mut ModelEditor<'_>) {}
}

fn provider(id: &str, package: &str, settings: Value) -> ProviderInfo {
    ProviderInfo {
        package: package.into(),
        settings: settings.as_object().cloned().filter(|o| !o.is_empty()),
        ..ProviderInfo::empty(id)
    }
}

fn build(seed: Vec<(ProviderInfo, Vec<ModelInfo>)>, vars: &[(&str, &str)], logins: CredentialLogins) -> Catalog {
    let mut plugins: Vec<Box<dyn CatalogPlugin>> =
        vec![Box::new(Seed(seed)), Box::new(ProviderTransforms { env: env(vars), logins }), Box::new(Enable)];
    Catalog::build(&mut plugins, &Access::new())
}

fn settings(catalog: &Catalog, id: &str) -> Value {
    Value::Object(catalog.provider(id).unwrap().settings.clone().unwrap_or_default())
}

const TEMPLATE: &str = "https://${GOOGLE_VERTEX_ENDPOINT}/v1/projects/${GOOGLE_VERTEX_PROJECT}/locations/${GOOGLE_VERTEX_LOCATION}";

// ---------------------------------------------------------------------------------------------
// Google Vertex
// ---------------------------------------------------------------------------------------------

#[test]
fn vertex_ignores_openai_compatible_providers_that_are_not_google_vertex() {
    let c = build(
        vec![(provider("opencode", "@opencode/ai/providers/openai-compatible", json!({"baseURL": "https://opencode.ai/zen/v1"})), vec![])],
        &[("GOOGLE_CLOUD_PROJECT", "p")],
        Default::default(),
    );
    assert_eq!(settings(&c, "opencode"), json!({"baseURL": "https://opencode.ai/zen/v1"}));
}

#[test]
fn vertex_resolves_project_and_location_from_env_using_legacy_precedence() {
    let c = build(
        vec![(provider("google-vertex", "@opencode/ai/providers/google-vertex/chat", json!({"baseURL": TEMPLATE})), vec![])],
        &[
            ("GOOGLE_CLOUD_PROJECT", "google-cloud-project"),
            ("GCP_PROJECT", "gcp-project"),
            ("GCLOUD_PROJECT", "gcloud-project"),
            ("GOOGLE_VERTEX_LOCATION", "google-vertex-location"),
            ("GOOGLE_CLOUD_LOCATION", "google-cloud-location"),
            ("VERTEX_LOCATION", "vertex-location"),
        ],
        Default::default(),
    );
    assert_eq!(
        settings(&c, "google-vertex"),
        json!({
            "baseURL": "https://google-vertex-location-aiplatform.googleapis.com/v1/projects/google-cloud-project/locations/google-vertex-location",
            "project": "google-cloud-project",
            "location": "google-vertex-location"
        })
    );
    assert_eq!(c.provider("google-vertex").unwrap().activation, Activation::Enabled);
}

#[test]
fn vertex_stays_automatic_without_a_project_and_defaults_to_global() {
    let c = build(vec![(provider("google-vertex", "@opencode/ai/providers/google-vertex", json!({})), vec![])], &[], Default::default());
    assert_eq!(c.provider("google-vertex").unwrap().activation, Activation::Auto);
    assert_eq!(settings(&c, "google-vertex"), json!({"location": "global"}));
    let c = build(
        vec![(provider("google-vertex", "@opencode/ai/providers/google-vertex", json!({"project": "config-project"})), vec![])],
        &[],
        Default::default(),
    );
    assert_eq!(settings(&c, "google-vertex"), json!({"project": "config-project", "location": "global"}));
}

#[test]
fn vertex_keeps_configured_project_and_location_and_expands_multi_regions() {
    for (location, host) in [("global", "aiplatform.googleapis.com"), ("eu", "aiplatform.eu.rep.googleapis.com"), ("us", "aiplatform.us.rep.googleapis.com")] {
        let c = build(
            vec![(
                provider(
                    "google-vertex",
                    "@opencode/ai/providers/google-vertex/chat",
                    json!({"baseURL": TEMPLATE, "project": "config-project", "location": location}),
                ),
                vec![],
            )],
            &[("GOOGLE_CLOUD_PROJECT", "env-project"), ("GOOGLE_VERTEX_LOCATION", "env-location")],
            Default::default(),
        );
        assert_eq!(
            settings(&c, "google-vertex")["baseURL"],
            json!(format!("https://{host}/v1/projects/config-project/locations/{location}"))
        );
    }
}

#[test]
fn vertex_expands_endpoint_templates_on_chat_models_and_uses_the_adc_login() {
    let mut model = ModelInfo::default_for("google-vertex", "meta/llama-maas");
    model.package = Some("@opencode/ai/providers/google-vertex/chat".into());
    model.settings = json!({"baseURL": format!("{TEMPLATE}/endpoints/openapi")}).as_object().cloned();
    let logins = CredentialLogins { vertex: vec![("project".into(), "adc-project".into()), ("location".into(), "eu".into())], ..Default::default() };
    let c = build(vec![(provider("google-vertex", "@opencode/ai/providers/google-vertex", json!({})), vec![model])], &[], logins);
    let model = c.model("google-vertex", "meta/llama-maas").unwrap();
    assert_eq!(
        Value::Object(model.settings.clone().unwrap()),
        json!({
            "project": "adc-project",
            "location": "eu",
            "baseURL": "https://aiplatform.eu.rep.googleapis.com/v1/projects/adc-project/locations/eu/endpoints/openapi"
        })
    );
    assert_eq!(c.provider("google-vertex").unwrap().activation, Activation::Enabled);
}

// ---------------------------------------------------------------------------------------------
// Amazon Bedrock
// ---------------------------------------------------------------------------------------------

#[test]
fn bedrock_moves_endpoint_to_base_url_unless_one_is_set() {
    let c = build(
        vec![(provider("amazon-bedrock", "@opencode/ai/providers/amazon-bedrock", json!({"endpoint": "https://vpce.test"})), vec![])],
        &[],
        Default::default(),
    );
    assert_eq!(settings(&c, "amazon-bedrock"), json!({"region": "us-east-1", "baseURL": "https://vpce.test"}));
    let c = build(
        vec![(
            provider("amazon-bedrock", "@opencode/ai/providers/amazon-bedrock", json!({"baseURL": "https://explicit.test", "endpoint": "https://vpce.test"})),
            vec![],
        )],
        &[],
        Default::default(),
    );
    assert_eq!(settings(&c, "amazon-bedrock"), json!({"baseURL": "https://explicit.test", "region": "us-east-1"}));
}

#[test]
fn bedrock_enables_on_ambient_aws_configuration_only() {
    let seed = || vec![(provider("amazon-bedrock", "@opencode/ai/providers/amazon-bedrock", json!({})), vec![])];
    assert_eq!(build(seed(), &[], Default::default()).provider("amazon-bedrock").unwrap().activation, Activation::Auto);
    for name in ["AWS_PROFILE", "AWS_ACCESS_KEY_ID", "AWS_WEB_IDENTITY_TOKEN_FILE", "AWS_CONTAINER_CREDENTIALS_RELATIVE_URI", "AWS_CONTAINER_CREDENTIALS_FULL_URI"] {
        let c = build(seed(), &[(name, "x")], Default::default());
        assert_eq!(c.provider("amazon-bedrock").unwrap().activation, Activation::Enabled, "{name}");
    }
    let mut disabled = provider("amazon-bedrock", "@opencode/ai/providers/amazon-bedrock", json!({}));
    disabled.activation = Activation::Disabled;
    let c = build(vec![(disabled, vec![])], &[("AWS_PROFILE", "dev")], Default::default());
    assert_eq!(c.provider("amazon-bedrock").unwrap().activation, Activation::Disabled);
}

#[test]
fn bedrock_takes_the_profile_of_a_stored_aws_profile_login_and_fills_the_region() {
    let mut logins = CredentialLogins::default();
    logins.set(
        "amazon-bedrock",
        Some(&CredentialValue::External {
            method_id: "aws-profile".into(),
            metadata: json!({"profile": "sso-dev", "other": "x"}).as_object().cloned(),
        }),
    );
    let c = build(
        vec![(provider("amazon-bedrock", "@opencode/ai/providers/amazon-bedrock/mantle", json!({})), vec![])],
        &[("AWS_DEFAULT_REGION", "eu-west-1")],
        logins,
    );
    assert_eq!(settings(&c, "amazon-bedrock"), json!({"profile": "sso-dev", "region": "eu-west-1"}));
    assert_eq!(c.provider("amazon-bedrock").unwrap().activation, Activation::Enabled);
    let c = build(
        vec![(provider("amazon-bedrock", "@opencode/ai/providers/amazon-bedrock", json!({"region": "ap-south-1"})), vec![])],
        &[("AWS_REGION", "us-west-2")],
        Default::default(),
    );
    assert_eq!(settings(&c, "amazon-bedrock"), json!({"region": "ap-south-1"}));
}

// ---------------------------------------------------------------------------------------------
// Databricks, Cloudflare, GitLab
// ---------------------------------------------------------------------------------------------

#[test]
fn databricks_fills_the_base_url_from_the_host() {
    let template = "https://${DATABRICKS_HOST}/ai-gateway/mlflow/v1";
    for host in ["dbc-1.cloud.databricks.com", "HTTPS://dbc-1.cloud.databricks.com//"] {
        let c = build(
            vec![(provider("databricks", "@opencode/ai/providers/openai-compatible", json!({"baseURL": template})), vec![])],
            &[("DATABRICKS_HOST", host)],
            Default::default(),
        );
        assert_eq!(settings(&c, "databricks")["baseURL"], "https://dbc-1.cloud.databricks.com/ai-gateway/mlflow/v1", "{host}");
    }
}

#[test]
fn cloudflare_gateway_takes_env_and_login_ids_and_keeps_a_configured_endpoint() {
    let seed = |settings: Value| vec![(provider("cloudflare-ai-gateway", "@opencode/ai/providers/cloudflare-ai-gateway", settings), vec![])];
    let c = build(seed(json!({})), &[("CLOUDFLARE_ACCOUNT_ID", "acct"), ("CLOUDFLARE_GATEWAY_ID", "gw")], Default::default());
    assert_eq!(settings(&c, "cloudflare-ai-gateway"), json!({"accountId": "acct", "gatewayId": "gw"}));
    let c = build(seed(json!({"accountId": "config-acct"})), &[("CLOUDFLARE_ACCOUNT_ID", "acct")], Default::default());
    assert_eq!(settings(&c, "cloudflare-ai-gateway"), json!({"accountId": "config-acct"}));
    let mut logins = CredentialLogins::default();
    logins.set(
        "cloudflare-ai-gateway",
        Some(&CredentialValue::Key { key: "k".into(), metadata: json!({"accountId": "login-acct", "gatewayId": "login-gw"}).as_object().cloned() }),
    );
    let c = build(seed(json!({"accountId": "config-acct"})), &[], logins.clone());
    assert_eq!(settings(&c, "cloudflare-ai-gateway"), json!({"accountId": "login-acct", "gatewayId": "login-gw"}));
    let c = build(seed(json!({"baseURL": "https://gateway.test"})), &[("CLOUDFLARE_ACCOUNT_ID", "acct")], logins);
    assert_eq!(settings(&c, "cloudflare-ai-gateway"), json!({"baseURL": "https://gateway.test"}));
}

#[test]
fn cloudflare_workers_ai_gives_the_package_its_account() {
    let native = |settings: Value| vec![(provider("cloudflare-workers-ai", "@opencode/ai/providers/cloudflare-workers-ai", settings), vec![])];
    let c = build(native(json!({})), &[("CLOUDFLARE_ACCOUNT_ID", "acct")], Default::default());
    assert_eq!(settings(&c, "cloudflare-workers-ai"), json!({"accountId": "acct"}));
    let c = build(native(json!({"accountId": "config"})), &[("CLOUDFLARE_ACCOUNT_ID", "acct")], Default::default());
    assert_eq!(settings(&c, "cloudflare-workers-ai"), json!({"accountId": "config"}));
    let logins = CredentialLogins { cloudflare_workers: vec![("accountId".into(), "login".into())], ..Default::default() };
    let c = build(native(json!({})), &[], logins);
    assert_eq!(settings(&c, "cloudflare-workers-ai"), json!({"accountId": "login"}));
    let compatible = vec![(provider("cloudflare-workers-ai", "aisdk:workers-ai-provider", json!({})), vec![])];
    let c = build(compatible, &[("CLOUDFLARE_ACCOUNT_ID", "acct")], Default::default());
    assert_eq!(settings(&c, "cloudflare-workers-ai"), json!({"baseURL": "https://api.cloudflare.com/client/v4/accounts/acct/ai/v1"}));
    let c = build(native(json!({"baseURL": "https://custom.test"})), &[("CLOUDFLARE_ACCOUNT_ID", "acct")], Default::default());
    assert_eq!(settings(&c, "cloudflare-workers-ai"), json!({"baseURL": "https://custom.test"}));
}

#[test]
fn gitlab_sends_requests_to_the_instance_an_oauth_login_was_issued_against() {
    let mut logins = CredentialLogins::default();
    logins.set(
        "gitlab",
        Some(&CredentialValue::Oauth {
            method_id: "oauth".into(),
            refresh: "r".into(),
            access: "a".into(),
            expires: 0,
            metadata: json!({"instanceUrl": "https://gitlab.example.test"}).as_object().cloned(),
        }),
    );
    let c = build(vec![(provider("gitlab", "aisdk:gitlab-ai-provider", json!({"instanceUrl": "https://gitlab.com"})), vec![])], &[], logins);
    assert_eq!(settings(&c, "gitlab"), json!({"instanceUrl": "https://gitlab.example.test"}));
}

// Integration documents (`ConfigProviderPlugin.integrate`).
#[test]
fn integrates_a_console_documents_providers_bound_to_its_connection() {
    use oc_ai::catalog::config::{parse_providers, IntegrationDocument, IntegrationProviders};
    let mut base = ModelInfo::default_for("anthropic", "claude-sonnet-5");
    base.name = "Claude Sonnet 5".into();
    base.package = Some("@opencode/ai/providers/anthropic".into());
    let seed = || vec![(provider("anthropic", "@opencode/ai/providers/anthropic", json!({})), vec![base.clone()])];
    let document = IntegrationDocument {
        integration_id: "opencode".into(),
        connection: "credential:crd_console".into(),
        providers: parse_providers(&json!({
            "console-anthropic": {
                "canonical": "anthropic",
                "name": "Anthropic (Console)",
                "settings": {"baseURL": "https://console.test/anthropic"},
                "models": {"sonnet": {"modelID": "claude-sonnet-5", "type": "chat"}}
            }
        })),
    };
    let build = |access: Access| {
        let mut plugins: Vec<Box<dyn CatalogPlugin>> =
            vec![Box::new(Seed(seed())), Box::new(IntegrationProviders { documents: vec![document.clone()] })];
        Catalog::build(&mut plugins, &access)
    };
    let connected = build(Access::new().integration("opencode", vec!["credential:crd_console".into()]));
    let provider = connected.provider("console-anthropic").unwrap();
    assert_eq!(provider.integration_id.as_deref(), Some("opencode"));
    assert_eq!(provider.canonical.as_deref(), Some("anthropic"));
    assert_eq!(provider.name, "Anthropic (Console)");
    assert_eq!(provider.activation, Activation::Enabled);
    assert_eq!(connected.record("console-anthropic").unwrap().source_connection.as_deref(), Some("credential:crd_console"));
    let model = connected.model("console-anthropic", "sonnet").unwrap();
    assert_eq!(model.name, "Claude Sonnet 5");
    assert_eq!(model.model_id, "claude-sonnet-5");
    assert_eq!(model.kind.as_deref(), Some("chat"));
    // The providers disappear as soon as the account changes or signs out.
    let switched = build(Access::new().integration("opencode", vec!["credential:other".into()]));
    assert!(!switched.is_available("console-anthropic"));
    let signed_out = build(Access::new());
    assert!(!signed_out.is_available("console-anthropic"));
}

#[test]
fn metadata_settings_pick_non_empty_strings() {
    let metadata = json!({"a": "x", "b": "", "c": 1}).as_object().cloned();
    assert_eq!(metadata_settings(metadata.as_ref(), &["a", "b", "c", "d"]), vec![("a".to_string(), "x".to_string())]);
}

// ---------------------------------------------------------------------------------------------
// Resolver: `${AWS_REGION}` takes the region setting first
// ---------------------------------------------------------------------------------------------

#[test]
fn aws_region_placeholders_take_the_region_setting() {
    use oc_ai::catalog::resolve::{from_catalog_model, ResolveOptions};
    let mut model = ModelInfo::default_for("amazon-bedrock", "openai.gpt-oss-120b");
    model.package = Some("@opencode/ai/providers/openai-compatible".into());
    model.settings = json!({"baseURL": "https://bedrock-mantle.${AWS_REGION}.api.aws/v1", "region": "eu-north-1", "apiKey": "k"})
        .as_object()
        .cloned();
    let options = ResolveOptions { env: Some(Arc::new(|name: &str| (name == "AWS_REGION").then(|| "us-east-1".to_string()))), ..Default::default() };
    let runtime = from_catalog_model(&model, None, &options).unwrap();
    assert_eq!(runtime.route.endpoint.base_url.as_deref(), Some("https://bedrock-mantle.eu-north-1.api.aws/v1"));
    model.settings.as_mut().unwrap().shift_remove("region");
    let runtime = from_catalog_model(&model, None, &options).unwrap();
    assert_eq!(runtime.route.endpoint.base_url.as_deref(), Some("https://bedrock-mantle.us-east-1.api.aws/v1"));
}

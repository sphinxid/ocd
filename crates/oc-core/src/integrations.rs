//! Integration registrations of the built-in provider and web-search plugins
//! (`core/src/plugin/provider/*.ts`, `core/src/plugin/websearch/*.ts`).
//!
//! Upstream these plugins edit the integration State after the models.dev plugin registered a `key` and an `env`
//! method per provider, in plugin order (`core/src/plugin/internal.ts`). Methods occupy slots keyed by
//! `type` (+ `id`), so an update keeps the slot's position: `key` and `env` stay first, OAuth and external
//! methods follow in registration order. [`register`] replays the data part on every catalog build (right after
//! the models.dev specs); the OAuth implementations are registered once, through [`register_oauth`].

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use serde_json::{json, Map, Value};

use oc_ai::credentials::flows::chatgpt::SavedClientId;
use oc_ai::credentials::flows::{
    ChatGptTokenSharing, CodexBrowser, CodexDevice, ConsoleDevice, CopilotDevice, DigitalOceanImplicit, GitLabPkce,
    PoeBrowser, SnowflakeBrowser, XaiDevice,
};
use oc_ai::credentials::http::FlowContext;
use oc_ai::credentials::{EnvSource, Integrations, KvStore, Method};

/// Inputs the plugins read when they build their methods.
pub struct Environment {
    /// Process environment.
    pub env: EnvSource,
    /// `Global.Path.home` (honours `OPENCODE_TEST_HOME`).
    pub home: PathBuf,
    /// Provider settings as the plugins see them (`configuredSettings`): models.dev settings overlaid with every
    /// configuration document's `providers[id].settings`.
    pub settings: HashMap<String, Map<String, Value>>,
    /// Whether the Azure CLI (`az`) is on `PATH` (`which("az")`).
    pub azure_cli: bool,
}

impl Environment {
    fn var(&self, name: &str) -> Option<String> {
        (self.env)(name)
    }

    fn setting(&self, provider: &str, key: &str) -> Option<&Value> {
        self.settings.get(provider).and_then(|s| s.get(key))
    }

    fn setting_str(&self, provider: &str, key: &str) -> Option<&str> {
        self.setting(provider, key).and_then(Value::as_str)
    }
}

/// `Global.Path.home`: `OPENCODE_TEST_HOME`, else the user's home directory.
pub fn global_home() -> PathBuf {
    std::env::var_os("OPENCODE_TEST_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(PathBuf::from))
        .unwrap_or_default()
}

/// `configuredSettings(id)` for every provider: `initial` settings overlaid with each configuration document's
/// `providers[id].settings` (later documents win per key; `Provider.mergeOverlay`).
pub fn settings(initial: impl IntoIterator<Item = (String, Map<String, Value>)>, sections: &[Value]) -> HashMap<String, Map<String, Value>> {
    let mut out: HashMap<String, Map<String, Value>> = initial.into_iter().collect();
    for section in sections {
        let Some(providers) = section.as_object() else { continue };
        for (id, provider) in providers {
            let Some(overlay) = provider.get("settings").and_then(Value::as_object) else { continue };
            let current = out.entry(id.clone()).or_default();
            for (key, value) in overlay {
                if !value.is_null() {
                    current.insert(key.clone(), value.clone());
                }
            }
        }
    }
    out
}

fn key(label: Option<&str>, form: Option<Vec<Value>>) -> Method {
    Method::Key { label: label.map(str::to_string), form }
}

fn env(names: &[&str]) -> Method {
    Method::Env { names: names.iter().map(|n| n.to_string()).collect() }
}

/// Data registrations of the provider and web-search plugins, in plugin order. Idempotent.
pub fn register(integrations: &Integrations, environment: &Environment) {
    // opencode.provider.amazon.bedrock (amazon-bedrock.ts:48-83)
    let profiles = aws_profiles(environment);
    integrations.register_method("amazon-bedrock", env(&["AWS_BEARER_TOKEN_BEDROCK"]));
    integrations.register_method("amazon-bedrock", key(Some("Bedrock API key"), None));
    let count = profiles.len();
    integrations.register_method(
        "amazon-bedrock",
        Method::External {
            id: "aws-profile".into(),
            label: "AWS profile (SSO or named profile)".into(),
            form: Some(vec![json!({
                "key": "profile",
                "type": "string",
                "title": "AWS profile",
                "description": if count > 0 {
                    format!("Found {count} profile{} in your AWS configuration.", if count == 1 { "" } else { "s" })
                } else {
                    "No profiles found in your AWS configuration.".into()
                },
                "required": true,
                "minLength": 1,
                "pattern": "\\S",
                "placeholder": "Profile name",
                "custom": true,
                "options": profiles.iter().map(|p| json!({"value": p, "label": p})).collect::<Vec<_>>(),
            })]),
        },
    );

    // opencode.provider.azure (azure.ts:139-188)
    let azure_resource = azure_resource_configured(environment);
    integrations.register_method("azure", env(&["AZURE_API_KEY", "AZURE_COGNITIVE_SERVICES_API_KEY"]));
    integrations.register_method(
        "azure",
        key(
            Some("API key"),
            (!azure_resource).then(|| {
                vec![json!({
                    "type": "string",
                    "key": "resourceName",
                    "title": "Enter Azure Resource Name",
                    "placeholder": "e.g. my-models",
                    "required": true,
                })]
            }),
        ),
    );
    if environment.azure_cli {
        integrations.register_method("azure", azure_cli_method(azure_resource));
    }

    // opencode.provider.cloudflare.ai.gateway (cloudflare-ai-gateway.ts:29-48)
    let gateway = "cloudflare-ai-gateway";
    let mut fields = vec![];
    if !environment.setting(gateway, "baseURL").is_some_and(Value::is_string) {
        if environment.var("CLOUDFLARE_ACCOUNT_ID").is_none_or(|v| v.is_empty())
            && !environment.setting(gateway, "accountId").is_some_and(Value::is_string)
        {
            fields.push(json!({
                "type": "string",
                "key": "accountId",
                "title": "Enter your Cloudflare Account ID",
                "placeholder": "e.g. 1234567890abcdef1234567890abcdef",
                "required": true,
            }));
        }
        if environment.var("CLOUDFLARE_GATEWAY_ID").is_none_or(|v| v.is_empty())
            && !environment.setting(gateway, "gatewayId").is_some_and(Value::is_string)
        {
            fields.push(json!({
                "type": "string",
                "key": "gatewayId",
                "title": "Enter your Cloudflare AI Gateway ID",
                "placeholder": "e.g. my-gateway",
                "required": true,
            }));
        }
    }
    integrations.register_method(gateway, key(Some("Gateway API token"), (!fields.is_empty()).then_some(fields)));

    // opencode.provider.cloudflare.workers.ai (cloudflare-workers-ai.ts:13-38)
    let workers = "cloudflare-workers-ai";
    let account = environment.setting_str(workers, "accountId").map(str::to_string).or_else(|| environment.var("CLOUDFLARE_ACCOUNT_ID"));
    let workers_form = (!environment.setting(workers, "baseURL").is_some_and(Value::is_string) && account.is_none_or(|a| a.is_empty()))
        .then(|| {
            vec![json!({
                "type": "string",
                "key": "accountId",
                "title": "Enter your Cloudflare Account ID",
                "placeholder": "e.g. 1234567890abcdef1234567890abcdef",
                "required": true,
            })]
        });
    integrations.register_method(workers, env(&["CLOUDFLARE_API_KEY", "CLOUDFLARE_WORKERS_AI_TOKEN", "CLOUDFLARE_API_TOKEN"]));
    integrations.register_method(workers, key(Some("API key"), workers_form));

    // opencode.provider.databricks (databricks.ts:7-10)
    integrations.register_method("databricks", env(&["DATABRICKS_TOKEN"]));

    // opencode.provider.github.copilot (github-copilot.ts:212-215): no API key method.
    integrations.remove_method("github-copilot", &key(None, None));

    // opencode.provider.google.vertex (google-vertex.ts:116-160)
    let (projects, locations) = vertex_choices(environment);
    let mut project = json!({
        "key": "project",
        "type": "string",
        "title": "Google Cloud project",
        "description": if projects.is_empty() {
            "No projects found in your gcloud configuration.".to_string()
        } else {
            format!("Found {} project{} in your gcloud configuration.", projects.len(), if projects.len() == 1 { "" } else { "s" })
        },
        "required": true,
        "minLength": 1,
        "pattern": "\\S",
        "placeholder": "Project ID",
        "custom": true,
    });
    if let Some(first) = projects.first() {
        project["default"] = json!(first);
    }
    project["options"] = json!(projects.iter().map(|p| json!({"value": p, "label": p})).collect::<Vec<_>>());
    let mut location = json!({
        "key": "location",
        "type": "string",
        "title": "Location",
        "description": "global serves the most models. Use us, eu, or a single region for data residency or regional quota.",
        "required": true,
        "minLength": 1,
        "pattern": "\\S",
        "placeholder": "Location",
        "custom": true,
    });
    location["default"] = json!(locations[0]);
    location["options"] = json!(locations.iter().map(|l| json!({"value": l, "label": l})).collect::<Vec<_>>());
    integrations.register_method("google-vertex", env(&["GOOGLE_VERTEX_API_KEY"]));
    integrations.register_method(
        "google-vertex",
        Method::External {
            id: "google-adc".into(),
            label: "Google Cloud credentials (gcloud auth or environment)".into(),
            form: Some(vec![project, location]),
        },
    );

    // opencode.provider.neon (neon.ts:7-10)
    integrations.register_method("neon", env(&["NEON_AI_GATEWAY_TOKEN"]));

    // opencode.provider.opencode (opencode.ts:248-254)
    integrations.update("opencode", |r| r.name = "OpenCode Console".into());
    integrations.register_method("opencode", key(Some("API key (service account)"), None));

    // opencode.provider.snowflake.cortex (snowflake-cortex.ts:82-91)
    integrations.register_method(
        "snowflake-cortex",
        key(
            Some("Paste PAT or bearer token"),
            Some(vec![json!({
                "type": "string",
                "key": "account",
                "title": "Snowflake account",
                "placeholder": "myorg-myaccount",
                "required": true,
            })]),
        ),
    );
    integrations.register_method("snowflake-cortex", env(&["SNOWFLAKE_CORTEX_TOKEN", "SNOWFLAKE_CORTEX_PAT"]));

    // opencode.provider.xai (xai.ts:91-97)
    integrations.update("xai", |r| r.name = "xAI".into());
    integrations.register_method("xai", key(Some("Manually enter API Key"), None));

    // opencode.websearch.* (websearch/{exa,firecrawl,parallel,tavily,tinyfish}.ts), `WebSearchPlugins` order.
    for (id, name, var) in [
        ("exa", "Exa", "EXA_API_KEY"),
        ("firecrawl", "Firecrawl", "FIRECRAWL_API_KEY"),
        ("parallel", "Parallel", "PARALLEL_API_KEY"),
        ("tavily", "Tavily", "TAVILY_API_KEY"),
        ("tinyfish", "TinyFish", "TINYFISH_API_KEY"),
    ] {
        integrations.update(id, |r| r.name = name.into());
        integrations.register_method(id, key(None, None));
        integrations.register_method(id, env(&[var]));
    }
}

/// `configuredResource` (azure.ts:135, `resolveResourceName` at :532): a non-blank configured
/// `resourceName`, else `AZURE_RESOURCE_NAME ?? AZURE_COGNITIVE_SERVICES_RESOURCE_NAME` (an empty
/// `AZURE_RESOURCE_NAME` wins and counts as none), or a configured `baseURL` string.
pub fn azure_resource_configured(environment: &Environment) -> bool {
    let setting = environment.setting_str("azure", "resourceName").is_some_and(|r| !r.trim().is_empty());
    let env = environment.var("AZURE_RESOURCE_NAME").or_else(|| environment.var("AZURE_COGNITIVE_SERVICES_RESOURCE_NAME"));
    setting || env.is_some_and(|v| !v.is_empty()) || environment.setting("azure", "baseURL").is_some_and(Value::is_string)
}

/// The `azure-cli` external method (azure.ts:146-166). The resource field is free text.
pub fn azure_cli_method(configured_resource: bool) -> Method {
    Method::External {
        id: "azure-cli".into(),
        label: "Microsoft Entra ID (Azure CLI)".into(),
        form: (!configured_resource).then(|| {
            vec![json!({
                "type": "string",
                "key": "resourceName",
                "title": "Azure resource",
                "placeholder": "e.g. my-models",
                "required": true,
                "pattern": AZURE_RESOURCE_PATTERN,
                "description": "Enter the name of your Azure OpenAI or AI Services resource.",
            })]
        }),
    }
}

/// `resourcePattern` (azure.ts).
pub const AZURE_RESOURCE_PATTERN: &str = "^[a-zA-Z0-9][a-zA-Z0-9-]*$";

/// What the OAuth flows need.
pub struct OAuthDeps {
    pub ctx: FlowContext,
    pub env: EnvSource,
    /// Plugin storage of `opencode.provider.chatgpt` (agent host id).
    pub chatgpt_storage: Arc<dyn KvStore>,
    /// `metadata.clientID` of the active ChatGPT token-sharing credential.
    pub chatgpt_client_id: SavedClientId,
}

/// OAuth method implementations of the provider plugins, in plugin order (internal.ts `ProviderPlugins`). Run once,
/// after the first [`register`], so OAuth slots follow `key`/`env`.
pub fn register_oauth(integrations: &Integrations, deps: OAuthDeps) {
    let ctx = deps.ctx;
    // opencode.provider.chatgpt (chatgpt.ts:233-237)
    integrations.register_oauth(Arc::new(ChatGptTokenSharing::new(ctx.clone(), deps.chatgpt_storage, deps.chatgpt_client_id)));
    // opencode.provider.digitalocean (digitalocean.ts:155-157)
    integrations.register_oauth(Arc::new(DigitalOceanImplicit::default()));
    // opencode.provider.github.copilot (github-copilot.ts:214)
    integrations.register_oauth(Arc::new(CopilotDevice::new(ctx.clone())));
    // opencode.provider.gitlab (gitlab.ts:125-140)
    integrations.register_oauth(Arc::new(GitLabPkce::new(ctx.clone(), deps.env.clone())));
    // opencode.provider.opencode (opencode.ts:252)
    integrations.register_oauth(Arc::new(ConsoleDevice::new(ctx.clone())));
    // opencode.provider.snowflake.cortex (snowflake-cortex.ts:92-100)
    integrations.register_oauth(Arc::new(SnowflakeBrowser::new(ctx.clone(), deps.env)));
    // opencode.provider.openai (openai.ts:251-254)
    integrations.register_oauth(Arc::new(CodexBrowser::new(ctx.clone())));
    integrations.register_oauth(Arc::new(CodexDevice::new(ctx.clone())));
    // opencode.provider.poe (poe.ts:28-32)
    integrations.register_oauth(Arc::new(PoeBrowser::new(ctx.clone())));
    // opencode.provider.xai (xai.ts:95)
    integrations.register_oauth(Arc::new(XaiDevice::new(ctx)));
}

/// Profile names from the AWS config and credentials files (amazon-bedrock.ts:26-47): sorted, deduplicated.
fn aws_profiles(environment: &Environment) -> Vec<String> {
    let files = [
        environment.var("AWS_CONFIG_FILE").map(PathBuf::from).unwrap_or_else(|| environment.home.join(".aws").join("config")),
        environment
            .var("AWS_SHARED_CREDENTIALS_FILE")
            .map(PathBuf::from)
            .unwrap_or_else(|| environment.home.join(".aws").join("credentials")),
    ];
    let mut out: Vec<String> = vec![];
    for (index, file) in files.iter().enumerate() {
        let content = std::fs::read_to_string(file).unwrap_or_default();
        for line in content.lines() {
            let trimmed = line.trim_start();
            let Some(rest) = trimmed.strip_prefix('[') else { continue };
            let Some(end) = rest.find(']') else { continue };
            let section = rest[..end].trim();
            if section.is_empty() {
                continue;
            }
            let name = if index == 1 || section == "default" {
                Some(section.to_string())
            } else {
                section.strip_prefix("profile ").map(|p| p.trim().to_string())
            };
            if let Some(name) = name.filter(|n| !n.is_empty())
                && !out.contains(&name)
            {
                out.push(name);
            }
        }
    }
    out.sort();
    out
}

/// Projects and locations offered by the Google Cloud credentials form (google-vertex.ts:58-100).
fn vertex_choices(environment: &Environment) -> (Vec<String>, Vec<String>) {
    let home = &environment.home;
    let gcloud = match environment.var("CLOUDSDK_CONFIG").filter(|v| !v.is_empty()) {
        Some(v) if v == "~" => home.clone(),
        Some(v) if v.starts_with("~/") => home.join(&v[2..]),
        Some(v) => PathBuf::from(v),
        None => home.join(".config").join("gcloud"),
    };
    let read = |p: &Path| std::fs::read_to_string(p).ok();
    let active = format!(
        "config_{}",
        read(&gcloud.join("active_config")).map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).unwrap_or_else(|| "default".into())
    );
    let mut configs: Vec<String> = std::fs::read_dir(gcloud.join("configurations"))
        .map(|d| d.filter_map(|e| e.ok()?.file_name().into_string().ok()).filter(|n| n.starts_with("config_")).collect())
        .unwrap_or_default();
    configs.sort_by(|a, b| (b == &active).cmp(&(a == &active)).then_with(|| oc_schema::collate::locale_compare(a, b)));
    let contents: Vec<String> = configs.iter().filter_map(|n| read(&gcloud.join("configurations").join(n))).collect();
    let adc = read(&gcloud.join("application_default_credentials.json"))
        .and_then(|s| serde_json::from_str::<Value>(&s).ok())
        .and_then(|v| v.get("quota_project_id").and_then(Value::as_str).map(str::to_string));
    let configured_project = environment
        .setting_str("google-vertex", "project")
        .map(str::to_string)
        .or_else(|| ["GOOGLE_VERTEX_PROJECT", "GOOGLE_CLOUD_PROJECT", "GCP_PROJECT", "GCLOUD_PROJECT"].iter().find_map(|n| environment.var(n)));
    let configured_location = environment
        .setting_str("google-vertex", "location")
        .map(str::to_string)
        .or_else(|| ["GOOGLE_VERTEX_LOCATION", "GOOGLE_CLOUD_LOCATION", "VERTEX_LOCATION"].iter().find_map(|n| environment.var(n)));
    let first_match = |content: &str, key: &str| -> Vec<String> {
        content
            .lines()
            .filter_map(|line| {
                let rest = line.strip_prefix(key)?;
                let value = rest.trim_start().strip_prefix('=')?.trim_start();
                value.split_whitespace().next().map(str::to_string)
            })
            .collect()
    };
    let mut projects: Vec<String> = vec![];
    let push = |list: &mut Vec<String>, value: Option<String>| {
        if let Some(v) = value.filter(|v| !v.is_empty())
            && !list.contains(&v)
        {
            list.push(v);
        }
    };
    push(&mut projects, configured_project);
    for content in &contents {
        push(&mut projects, first_match(content, "project").into_iter().next());
    }
    push(&mut projects, adc);
    let mut locations: Vec<String> = vec![];
    push(&mut locations, Some(configured_location.unwrap_or_else(|| "global".into())));
    for l in ["global", "us", "eu"] {
        push(&mut locations, Some(l.into()));
    }
    for content in &contents {
        for region in first_match(content, "region") {
            push(&mut locations, Some(region));
        }
    }
    (projects, locations)
}

#[cfg(test)]
mod tests {
    use super::*;
    use oc_ai::credentials::{MemoryCredentialStore, MemoryKv};

    fn methods(integrations: &Integrations, id: &str) -> Value {
        Value::Array(integrations.methods(id).iter().map(Method::to_json).collect())
    }

    /// Expected values are upstream's `GET /api/integration` entries for an empty home and environment.
    #[test]
    fn matches_upstream_registrations() {
        let integrations = Integrations::new(Arc::new(MemoryCredentialStore::default()));
        // What the models.dev plugin registers first.
        for (id, names) in [
            ("amazon-bedrock", vec!["AWS_ACCESS_KEY_ID", "AWS_SECRET_ACCESS_KEY", "AWS_REGION", "AWS_BEARER_TOKEN_BEDROCK"]),
            ("github-copilot", vec!["GITHUB_TOKEN"]),
            ("openai", vec!["OPENAI_API_KEY"]),
        ] {
            integrations.register_method(id, Method::Key { label: None, form: None });
            integrations.register_method(id, Method::Env { names: names.into_iter().map(String::from).collect() });
        }
        let home = std::env::temp_dir().join(format!("oc-integrations-test-{}", std::process::id()));
        let environment = Environment {
            env: Arc::new(|_: &str| None),
            home: home.clone(),
            settings: HashMap::new(),
            azure_cli: false,
        };
        register(&integrations, &environment);
        register_oauth(
            &integrations,
            OAuthDeps {
                ctx: FlowContext::new("opencode/latest/test/opencode"),
                env: Arc::new(|_: &str| None),
                chatgpt_storage: Arc::new(MemoryKv::default()),
                chatgpt_client_id: oc_ai::credentials::flows::chatgpt::no_saved_client_id(),
            },
        );
        // A second catalog build replays the data registrations after the models.dev ones.
        integrations.register_method("github-copilot", Method::Key { label: None, form: None });
        integrations.register_method("openai", Method::Key { label: None, form: None });
        register(&integrations, &environment);

        assert_eq!(
            methods(&integrations, "github-copilot"),
            json!([
                {"type": "env", "names": ["GITHUB_TOKEN"]},
                {"id": "device", "type": "oauth", "label": "Login with GitHub Copilot", "form": [
                    {"key": "deploymentType", "options": [
                        {"description": "Public", "label": "GitHub.com", "value": "github.com"},
                        {"description": "Data residency or self-hosted", "label": "GitHub Enterprise", "value": "enterprise"}
                    ], "required": true, "title": "Select GitHub deployment type", "type": "string"},
                    {"key": "enterpriseUrl", "placeholder": "company.ghe.com or https://company.ghe.com", "required": true,
                     "title": "Enter your GitHub Enterprise URL or domain", "type": "string",
                     "when": [{"key": "deploymentType", "op": "eq", "value": "enterprise"}]}
                ]}
            ])
        );
        assert_eq!(
            methods(&integrations, "openai"),
            json!([
                {"type": "key"},
                {"type": "env", "names": ["OPENAI_API_KEY"]},
                {"id": "chatgpt-token-sharing", "type": "oauth", "label": "Sign in with ChatGPT"},
                {"id": "chatgpt-browser", "type": "oauth", "label": "Codex browser (legacy)"},
                {"id": "chatgpt-headless", "type": "oauth", "label": "Codex device code (legacy)"}
            ])
        );
        assert_eq!(
            methods(&integrations, "amazon-bedrock"),
            json!([
                {"type": "key", "label": "Bedrock API key"},
                {"type": "env", "names": ["AWS_BEARER_TOKEN_BEDROCK"]},
                {"id": "aws-profile", "type": "external", "label": "AWS profile (SSO or named profile)", "form": [
                    {"custom": true, "description": "No profiles found in your AWS configuration.", "key": "profile",
                     "minLength": 1, "options": [], "pattern": "\\S", "placeholder": "Profile name", "required": true,
                     "title": "AWS profile", "type": "string"}
                ]}
            ])
        );
        assert_eq!(
            methods(&integrations, "exa"),
            json!([{"type": "key"}, {"type": "env", "names": ["EXA_API_KEY"]}])
        );
    }

    #[test]
    fn aws_profiles_from_files() {
        let home = std::env::temp_dir().join(format!("oc-integrations-aws-{}", std::process::id()));
        std::fs::create_dir_all(home.join(".aws")).unwrap();
        std::fs::write(home.join(".aws/config"), "[default]\n[profile work]\n[sso-session x]\n").unwrap();
        std::fs::write(home.join(".aws/credentials"), "[personal]\n[default]\n").unwrap();
        let environment = Environment { env: Arc::new(|_: &str| None), home: home.clone(), settings: HashMap::new(), azure_cli: false };
        assert_eq!(aws_profiles(&environment), vec!["default", "personal", "work"]);
        let _ = std::fs::remove_dir_all(home);
    }
}

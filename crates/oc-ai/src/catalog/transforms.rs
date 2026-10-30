//! Provider transforms of the Amazon Bedrock, Google Vertex, Cloudflare AI Gateway, Cloudflare Workers AI,
//! Databricks and GitLab provider plugins (`packages/core/src/plugin/provider/{amazon-bedrock,google-vertex,
//! cloudflare-ai-gateway,cloudflare-workers-ai,databricks,gitlab}.ts`): ambient credentials that enable a
//! provider, catalog URL templates filled from settings and the environment, and the settings a login keeps
//! as credential metadata (`credential-settings.ts`).

use super::registry::{CatalogPlugin, ModelEditor, ProviderEditor};
use super::types::{Activation, ProviderInfo};
use crate::credentials::CredentialValue;
use crate::json::Object;
use serde_json::Value;
use std::sync::Arc;

/// Environment lookup (`process.env[name]`).
pub type Env = Arc<dyn Fn(&str) -> Option<String> + Send + Sync>;

/// The process environment.
pub fn process_env() -> Env {
    Arc::new(|name| std::env::var(name).ok())
}

const BEDROCK: &str = "amazon-bedrock";
const VERTEX: &str = "google-vertex";
const CLOUDFLARE_GATEWAY: &str = "cloudflare-ai-gateway";
const CLOUDFLARE_WORKERS: &str = "cloudflare-workers-ai";
const DATABRICKS: &str = "databricks";
const GITLAB: &str = "gitlab";

/// Settings the provider plugins derive from each integration's active connection's **stored** credential
/// (login form answers kept as credential metadata). The model resolver never passes credential metadata to
/// packages, so these are applied by the provider transforms instead.
#[derive(Debug, Clone, Default, PartialEq, Eq, Hash)]
pub struct CredentialLogins {
    /// `{project, location}` of a Google Cloud credentials (`google-adc`) connection.
    pub vertex: Vec<(String, String)>,
    /// `{profile}` of an AWS profile connection.
    pub bedrock: Vec<(String, String)>,
    /// `{accountId, gatewayId}` of a Cloudflare AI Gateway key.
    pub cloudflare_gateway: Vec<(String, String)>,
    /// `{accountId}` of a Cloudflare Workers AI key.
    pub cloudflare_workers: Vec<(String, String)>,
    /// `{instanceUrl}` of a GitLab OAuth login.
    pub gitlab: Vec<(String, String)>,
}

/// The integrations whose stored credential feeds [`CredentialLogins`].
pub const LOGIN_INTEGRATIONS: &[&str] = &[VERTEX, BEDROCK, CLOUDFLARE_GATEWAY, CLOUDFLARE_WORKERS, GITLAB];

/// `metadataSettings(metadata, keys)`: the non-empty string metadata entries a package accepts as settings.
pub fn metadata_settings(metadata: Option<&Object>, keys: &[&str]) -> Vec<(String, String)> {
    keys.iter()
        .filter_map(|key| match metadata.and_then(|m| m.get(*key)) {
            Some(Value::String(s)) if !s.is_empty() => Some((key.to_string(), s.clone())),
            _ => None,
        })
        .collect()
}

impl CredentialLogins {
    /// Set the projection of `integration_id` from its active connection's stored credential.
    pub fn set(&mut self, integration_id: &str, credential: Option<&CredentialValue>) {
        let Some(value) = credential else { return };
        match (integration_id, value) {
            // The Vertex plugin keeps both keys, whatever their value (`google-vertex.ts` `load`).
            (VERTEX, CredentialValue::External { method_id, metadata, .. }) if method_id == "google-adc" => {
                self.vertex = ["project", "location"]
                    .iter()
                    .filter_map(|k| metadata.as_ref().and_then(|m| m.get(*k)).and_then(Value::as_str).map(|v| (k.to_string(), v.to_string())))
                    .collect();
            }
            (BEDROCK, CredentialValue::External { method_id, metadata, .. }) if method_id == "aws-profile" => {
                self.bedrock = metadata_settings(metadata.as_ref(), &["profile"]);
            }
            (CLOUDFLARE_GATEWAY, CredentialValue::Key { metadata, .. }) => {
                self.cloudflare_gateway = metadata_settings(metadata.as_ref(), &["accountId", "gatewayId"]);
            }
            (CLOUDFLARE_WORKERS, CredentialValue::Key { metadata, .. }) => {
                self.cloudflare_workers = metadata_settings(metadata.as_ref(), &["accountId"]);
            }
            (GITLAB, CredentialValue::Oauth { metadata, .. }) => {
                self.gitlab = metadata_settings(metadata.as_ref(), &["instanceUrl"]);
            }
            _ => {}
        }
    }
}

fn spread(settings: Option<&Object>, extra: &[(String, String)]) -> Object {
    let mut out = settings.cloned().unwrap_or_default();
    for (k, v) in extra {
        out.insert(k.clone(), Value::String(v.clone()));
    }
    out
}

/// The provider transforms (and the Vertex model transform) of the plugins listed in the module docs.
pub struct ProviderTransforms {
    pub env: Env,
    pub logins: CredentialLogins,
}

impl ProviderTransforms {
    fn var(&self, name: &str) -> Option<String> {
        (self.env)(name)
    }

    /// JS truthiness of `process.env[name]`.
    fn set(&self, name: &str) -> bool {
        self.var(name).is_some_and(|v| !v.is_empty())
    }

    fn vertex_project(&self, settings: &Object) -> Option<String> {
        match settings.get("project") {
            Some(Value::Null) | None => ["GOOGLE_VERTEX_PROJECT", "GOOGLE_CLOUD_PROJECT", "GCP_PROJECT", "GCLOUD_PROJECT"]
                .iter()
                .find_map(|name| self.var(name)),
            Some(Value::String(s)) => Some(s.clone()),
            Some(_) => None,
        }
    }

    fn vertex_location(&self, settings: &Object) -> Option<String> {
        match settings.get("location") {
            Some(Value::Null) | None => {
                ["GOOGLE_VERTEX_LOCATION", "GOOGLE_CLOUD_LOCATION", "VERTEX_LOCATION"].iter().find_map(|name| self.var(name))
            }
            Some(Value::String(s)) => Some(s.clone()),
            Some(_) => None,
        }
    }

    fn vertex_settings(&self, provider: &ProviderInfo) -> Object {
        let own = provider.integration_id.as_deref().unwrap_or(&provider.id) == VERTEX;
        spread(provider.settings.as_ref(), if own { &self.logins.vertex } else { &[] })
    }

    fn amazon_bedrock(&self, editor: &mut ProviderEditor) {
        const CHAIN_ENV: &[&str] = &[
            "AWS_PROFILE",
            "AWS_ACCESS_KEY_ID",
            "AWS_WEB_IDENTITY_TOKEN_FILE",
            "AWS_CONTAINER_CREDENTIALS_RELATIVE_URI",
            "AWS_CONTAINER_CREDENTIALS_FULL_URI",
        ];
        let ids: Vec<String> = editor
            .list()
            .filter(|r| r.provider.package.starts_with("@opencode/ai/providers/amazon-bedrock"))
            .map(|r| r.provider.id.clone())
            .collect();
        for id in ids {
            editor.update(&id, |provider| {
                let own = provider.integration_id.as_deref().unwrap_or(&provider.id) == BEDROCK;
                let mut settings = spread(provider.settings.as_ref(), if own { &self.logins.bedrock } else { &[] });
                let chain = settings.get("profile").is_some_and(Value::is_string) || CHAIN_ENV.iter().any(|name| self.set(name));
                // SigV4 authenticates through the AWS default chain rather than a key credential, so ambient AWS
                // configuration is what makes Bedrock usable.
                if chain && provider.activation == Activation::Auto {
                    provider.activation = Activation::Enabled;
                }
                // Same default the native package uses, made explicit so catalog `${AWS_REGION}` URLs resolve.
                if !settings.get("region").is_some_and(Value::is_string) {
                    let region = self.var("AWS_REGION").or_else(|| self.var("AWS_DEFAULT_REGION")).unwrap_or_else(|| "us-east-1".into());
                    settings.insert("region".into(), Value::String(region));
                }
                // Bedrock private/VPC endpoints are configured as `endpoint`; it becomes the base URL once.
                if !settings.get("baseURL").is_some_and(Value::is_string)
                    && let Some(endpoint) = settings.get("endpoint").filter(|e| e.is_string()).cloned()
                {
                    settings.insert("baseURL".into(), endpoint);
                }
                settings.shift_remove("endpoint");
                provider.settings = Some(settings);
            });
        }
    }

    fn cloudflare_ai_gateway(&self, editor: &mut ProviderEditor) {
        let account = self.var("CLOUDFLARE_ACCOUNT_ID").filter(|v| !v.is_empty());
        let gateway = self.var("CLOUDFLARE_GATEWAY_ID").filter(|v| !v.is_empty());
        if editor.get(CLOUDFLARE_GATEWAY).is_none()
            || (account.is_none() && gateway.is_none() && self.logins.cloudflare_gateway.is_empty())
        {
            return;
        }
        editor.update(CLOUDFLARE_GATEWAY, |provider| {
            if provider.base_url().is_some() {
                return;
            }
            let mut settings = Object::new();
            if let Some(account) = &account {
                settings.insert("accountId".into(), Value::String(account.clone()));
            }
            if let Some(gateway) = &gateway {
                settings.insert("gatewayId".into(), Value::String(gateway.clone()));
            }
            for (k, v) in provider.settings.iter().flatten() {
                settings.insert(k.clone(), v.clone());
            }
            provider.settings = Some(spread(Some(&settings), &self.logins.cloudflare_gateway));
        });
    }

    fn cloudflare_workers_ai(&self, editor: &mut ProviderEditor) {
        if editor.get(CLOUDFLARE_WORKERS).is_none() {
            return;
        }
        editor.update(CLOUDFLARE_WORKERS, |provider| {
            if provider.base_url().is_some() {
                return;
            }
            let merged = spread(provider.settings.as_ref(), &self.logins.cloudflare_workers);
            let account = match merged.get("accountId") {
                Some(Value::String(s)) => Some(s.clone()),
                _ => self.var("CLOUDFLARE_ACCOUNT_ID"),
            };
            let Some(account) = account.filter(|a| !a.is_empty()) else { return };
            let mut settings = provider.settings.clone().unwrap_or_default();
            if provider.package == "@opencode/ai/providers/cloudflare-workers-ai" {
                settings.insert("accountId".into(), Value::String(account));
            } else {
                settings.insert("baseURL".into(), Value::String(format!("https://api.cloudflare.com/client/v4/accounts/{account}/ai/v1")));
            }
            provider.settings = Some(settings);
        });
    }

    fn databricks(&self, editor: &mut ProviderEditor) {
        // The template adds its own scheme, but Databricks tooling documents DATABRICKS_HOST with one.
        let Some(host) = self.var("DATABRICKS_HOST").map(|h| {
            let lower = h.to_ascii_lowercase();
            let stripped = if lower.starts_with("https://") {
                &h[8..]
            } else if lower.starts_with("http://") {
                &h[7..]
            } else {
                &h[..]
            };
            stripped.trim_end_matches('/').to_string()
        }) else {
            return;
        };
        if host.is_empty() {
            return;
        }
        let Some(base_url) = editor.get(DATABRICKS).and_then(|r| r.provider.base_url()).map(String::from) else { return };
        if !base_url.contains("${DATABRICKS_HOST}") {
            return;
        }
        editor.update(DATABRICKS, |provider| {
            let mut settings = provider.settings.clone().unwrap_or_default();
            settings.insert("baseURL".into(), Value::String(base_url.replace("${DATABRICKS_HOST}", &host)));
            provider.settings = Some(settings);
        });
    }

    fn gitlab(&self, editor: &mut ProviderEditor) {
        // An OAuth login pins the instance it was issued against, so requests go there too.
        if !self.logins.gitlab.iter().any(|(k, _)| k == "instanceUrl") || editor.get(GITLAB).is_none() {
            return;
        }
        editor.update(GITLAB, |provider| provider.settings = Some(spread(provider.settings.as_ref(), &self.logins.gitlab)));
    }

    fn is_vertex(provider: &ProviderInfo) -> bool {
        provider.package.starts_with("@opencode/ai/providers/google-vertex")
            || (provider.id == VERTEX && provider.package == "@opencode/ai/providers/openai-compatible")
    }

    fn google_vertex(&self, editor: &mut ProviderEditor) {
        let ids: Vec<String> = editor.list().filter(|r| Self::is_vertex(&r.provider)).map(|r| r.provider.id.clone()).collect();
        for id in ids {
            editor.update(&id, |provider| {
                let settings = self.vertex_settings(provider);
                let project = self.vertex_project(&settings);
                let location = self.vertex_location(&settings).unwrap_or_else(|| "global".into());
                // Vertex authenticates through ADC rather than a key credential, so a resolvable project is what
                // makes the provider usable.
                if project.as_deref().is_some_and(|p| !p.is_empty()) && provider.activation == Activation::Auto {
                    provider.activation = Activation::Enabled;
                }
                let mut next = settings;
                if let Some(project) = project.as_ref().filter(|p| !p.is_empty()) {
                    next.insert("project".into(), Value::String(project.clone()));
                }
                next.insert("location".into(), Value::String(location.clone()));
                if let Some(base_url) = provider.base_url() {
                    next.insert("baseURL".into(), Value::String(replace_vertex_vars(base_url, project.as_deref(), &location)));
                }
                provider.settings = Some(next);
            });
        }
    }
}

/// `vertexEndpoint(location)`.
pub fn vertex_endpoint(location: &str) -> String {
    match location {
        "global" => "aiplatform.googleapis.com".into(),
        // Jurisdictional multi-regions use Regional Endpoint Platform domains.
        "eu" | "us" => format!("aiplatform.{location}.rep.googleapis.com"),
        _ => format!("{location}-aiplatform.googleapis.com"),
    }
}

/// `replaceVertexVars`: Vertex endpoints are stored as templates in the catalog; they are expanded once the
/// project and location are resolved.
pub fn replace_vertex_vars(value: &str, project: Option<&str>, location: &str) -> String {
    value
        .replace("${GOOGLE_VERTEX_PROJECT}", project.unwrap_or("${GOOGLE_VERTEX_PROJECT}"))
        .replace("${GOOGLE_VERTEX_LOCATION}", location)
        .replace("${GOOGLE_VERTEX_ENDPOINT}", &vertex_endpoint(location))
}

impl CatalogPlugin for ProviderTransforms {
    fn providers(&mut self, editor: &mut ProviderEditor) {
        // ProviderPlugins order: Amazon Bedrock, Cloudflare AI Gateway, Cloudflare Workers AI, Databricks,
        // GitLab, Google Vertex.
        self.amazon_bedrock(editor);
        self.cloudflare_ai_gateway(editor);
        self.cloudflare_workers_ai(editor);
        self.databricks(editor);
        self.gitlab(editor);
        self.google_vertex(editor);
    }

    fn models(&mut self, editor: &mut ModelEditor<'_>) {
        let providers: Vec<(String, Option<String>, String)> = editor
            .providers()
            .filter(|r| Self::is_vertex(&r.provider))
            .map(|r| {
                let settings = self.vertex_settings(&r.provider);
                let project = self.vertex_project(&settings);
                let location = self.vertex_location(&settings).unwrap_or_else(|| "global".into());
                (r.provider.id.clone(), project, location)
            })
            .collect();
        for (provider_id, project, location) in providers {
            for (pid, mid) in editor.list(Some(&provider_id)) {
                let Some(base_url) = editor.get(&pid, &mid).and_then(|m| m.settings.as_ref()?.get("baseURL")?.as_str().map(String::from))
                else {
                    continue;
                };
                let base_url = replace_vertex_vars(&base_url, project.as_deref(), &location);
                editor.update(&pid, &mid, |model| {
                    let mut settings = model.settings.clone().unwrap_or_default();
                    settings.insert("baseURL".into(), Value::String(base_url));
                    model.settings = Some(settings);
                });
            }
        }
    }
}

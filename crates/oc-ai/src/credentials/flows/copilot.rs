//! GitHub Copilot device flow (`packages/core/src/plugin/provider/github-copilot.ts:17-157`, spec §6.4.3).

use super::{PollTiming, sleep_ms};
use crate::credentials::form::FormAnswer;
use crate::credentials::http::{FlowContext, request_json, str_field};
use crate::credentials::integration::{AuthorizeResult, FlowError, FlowResult, OAuthMethod, OAuthMethodInfo};
use crate::credentials::{Metadata, OAuthValue};
use async_trait::async_trait;
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

pub const CLIENT_ID: &str = "Ov23li8tweQw6odWQebz";
pub const API_VERSION: &str = "2026-08-01";
pub const USER_API_VERSION: &str = "2025-04-01";
pub const INTEGRATION_ID: &str = "github-copilot";
pub const METHOD_ID: &str = "device";
pub const DEVICE_GRANT: &str = "urn:ietf:params:oauth:grant-type:device_code";

pub struct CopilotDevice {
    pub ctx: FlowContext,
    pub timing: PollTiming,
    /// Test override replacing `https://<domain>` and the API host.
    pub base_override: Option<String>,
}

impl CopilotDevice {
    pub fn new(ctx: FlowContext) -> Self {
        Self { ctx, timing: PollTiming::default(), base_override: None }
    }

    fn urls(&self, domain: &str) -> (String, String, String) {
        if let Some(base) = &self.base_override {
            return (
                format!("{base}/login/device/code"),
                format!("{base}/login/oauth/access_token"),
                format!("{base}/copilot_internal/user"),
            );
        }
        let api = if domain == "github.com" { "https://api.github.com".to_string() } else { format!("https://api.{domain}") };
        (
            format!("https://{domain}/login/device/code"),
            format!("https://{domain}/login/oauth/access_token"),
            format!("{api}/copilot_internal/user"),
        )
    }
}

/// The method's form (deployment type + enterprise URL).
pub fn form() -> Vec<Value> {
    vec![
        json!({
            "type": "string",
            "key": "deploymentType",
            "title": "Select GitHub deployment type",
            "required": true,
            "options": [
                { "label": "GitHub.com", "value": "github.com", "description": "Public" },
                { "label": "GitHub Enterprise", "value": "enterprise", "description": "Data residency or self-hosted" }
            ]
        }),
        json!({
            "type": "string",
            "key": "enterpriseUrl",
            "title": "Enter your GitHub Enterprise URL or domain",
            "placeholder": "company.ghe.com or https://company.ghe.com",
            "required": true,
            "when": [{ "key": "deploymentType", "op": "eq", "value": "enterprise" }]
        }),
    ]
}

/// Strips `http(s)://` and one trailing `/`.
pub fn normalize_domain(input: &str) -> String {
    let stripped = input.strip_prefix("https://").or_else(|| input.strip_prefix("http://")).unwrap_or(input);
    stripped.strip_suffix('/').unwrap_or(stripped).to_string()
}

/// Copilot API base: `metadata.apiEndpoint`, else `https://copilot-api.<enterprise>` / `https://api.githubcopilot.com`.
pub fn copilot_base_url(metadata: Option<&Metadata>) -> String {
    if let Some(endpoint) = metadata.and_then(|m| m.get("apiEndpoint")).and_then(Value::as_str).filter(|e| !e.is_empty())
    {
        return endpoint.to_string();
    }
    match metadata.and_then(|m| m.get("enterpriseUrl")).and_then(Value::as_str) {
        Some(enterprise) if !enterprise.is_empty() => format!("https://copilot-api.{}", normalize_domain(enterprise)),
        _ => "https://api.githubcopilot.com".to_string(),
    }
}

/// `copilotEntitlementError`: only an explicit `chat_enabled: false` blocks login.
pub fn entitlement_error(user: &Value) -> Option<&'static str> {
    if user.get("chat_enabled").and_then(Value::as_bool) != Some(false) {
        return None;
    }
    if user.get("can_signup_for_limited").and_then(Value::as_bool) == Some(true) {
        return Some(
            "This GitHub account is not signed up for GitHub Copilot. Sign up for Copilot Free at https://github.com/features/copilot/plans and connect again.",
        );
    }
    Some(
        "This GitHub account does not have GitHub Copilot access. It needs an active Copilot subscription or a seat assigned by an organization.",
    )
}

/// Lenient decode of the `/copilot_internal/user` body (`User` schema); mismatched types → `None`.
fn decode_user(value: &Value) -> Option<Value> {
    let obj = value.as_object()?;
    let bool_ok = |k: &str| obj.get(k).is_none_or(Value::is_boolean);
    if !bool_ok("chat_enabled") || !bool_ok("can_signup_for_limited") {
        return None;
    }
    match obj.get("endpoints") {
        None => {}
        Some(Value::Object(e)) if e.get("api").is_none_or(Value::is_string) => {}
        Some(_) => return None,
    }
    Some(value.clone())
}

fn headers(ctx: &FlowContext, request: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
    request
        .header("Accept", "application/json")
        .header("Content-Type", "application/json")
        .header("User-Agent", ctx.user_agent.as_str())
}

#[async_trait]
impl OAuthMethod for CopilotDevice {
    fn integration_id(&self) -> String {
        INTEGRATION_ID.into()
    }

    fn info(&self) -> OAuthMethodInfo {
        OAuthMethodInfo::new(METHOD_ID, "Login with GitHub Copilot").with_form(form())
    }

    async fn authorize(&self, answer: FormAnswer, _scope: CancellationToken) -> FlowResult<AuthorizeResult> {
        let enterprise = answer.get("deploymentType").and_then(Value::as_str) == Some("enterprise");
        let enterprise_url = answer.get("enterpriseUrl").and_then(Value::as_str).map(str::to_string);
        if enterprise && enterprise_url.as_deref().is_none_or(str::is_empty) {
            return Err(FlowError::new("Enterprise URL is required"));
        }
        let domain = if enterprise { normalize_domain(enterprise_url.as_deref().unwrap_or("")) } else { "github.com".into() };
        let (device_url, token_url, user_url) = self.urls(&domain);
        let device = request_json(
            headers(&self.ctx, self.ctx.http.post(&device_url))
                .body(json!({ "client_id": CLIENT_ID, "scope": "read:user" }).to_string()),
        )
        .await?;
        let field = |k: &str| str_field(&device, k).ok_or_else(|| FlowError::new(format!("Invalid device response: missing {k}")));
        let verification_uri = field("verification_uri")?;
        let user_code = field("user_code")?;
        let device_code = field("device_code")?;
        let interval = device
            .get("interval")
            .and_then(Value::as_f64)
            .ok_or_else(|| FlowError::new("Invalid device response: missing interval"))?;
        let interval_ms = (interval.max(1.0) * self.timing.second_ms as f64) as u64;
        let ctx = self.ctx.clone();
        let timing = self.timing;
        let callback = async move {
            let mut wait = interval_ms;
            loop {
                let token = request_json(headers(&ctx, ctx.http.post(&token_url)).body(
                    json!({ "client_id": CLIENT_ID, "device_code": device_code, "grant_type": DEVICE_GRANT }).to_string(),
                ))
                .await?;
                if let Some(access) = str_field(&token, "access_token").filter(|a| !a.is_empty()) {
                    let user = request_json(
                        ctx.http
                            .get(&user_url)
                            .header("Accept", "application/json")
                            .header("Authorization", format!("Bearer {access}"))
                            .header("User-Agent", ctx.user_agent.as_str())
                            .header("X-GitHub-Api-Version", USER_API_VERSION),
                    )
                    .await
                    .ok()
                    .and_then(|u| decode_user(&u));
                    if let Some(denied) = user.as_ref().and_then(entitlement_error) {
                        return Err(FlowError::new(denied));
                    }
                    let api_endpoint = user
                        .as_ref()
                        .and_then(|u| u.get("endpoints"))
                        .and_then(|e| e.get("api"))
                        .and_then(Value::as_str)
                        .map(|a| a.trim_end_matches('/').to_string())
                        .filter(|a| !a.is_empty());
                    let metadata = (enterprise || api_endpoint.is_some()).then(|| {
                        let mut metadata = Metadata::new();
                        if enterprise {
                            metadata.insert("enterpriseUrl".into(), json!(domain));
                        }
                        if let Some(api) = &api_endpoint {
                            metadata.insert("apiEndpoint".into(), json!(api));
                        }
                        metadata
                    });
                    return Ok(OAuthValue {
                        method_id: METHOD_ID.into(),
                        refresh: access.clone(),
                        access,
                        expires: 0,
                        metadata,
                    });
                }
                let error = str_field(&token, "error");
                match error.as_deref() {
                    Some("authorization_pending") => sleep_ms(wait + timing.margin_ms).await,
                    Some("slow_down") => {
                        let next = match token.get("interval").and_then(Value::as_f64) {
                            Some(i) if i > 0.0 => (i * timing.second_ms as f64) as u64,
                            _ => wait + 5 * timing.second_ms,
                        };
                        wait = next;
                        sleep_ms(next + timing.margin_ms).await;
                    }
                    other => {
                        return Err(FlowError::new(match other {
                            Some(e) if !e.is_empty() => format!("Device authorization failed: {e}"),
                            _ => "Device authorization failed".to_string(),
                        }));
                    }
                }
            }
        };
        Ok(AuthorizeResult::auto(verification_uri, format!("Enter code: {user_code}"), callback))
    }
}

//! OpenCode Console device flow (`packages/core/src/plugin/provider/opencode.ts:101-178, 540-633`,
//! spec §6.4.4).

use super::{PollTiming, sleep_ms};
use crate::credentials::form::FormAnswer;
use crate::credentials::http::{FlowContext, str_field, transport};
use crate::credentials::integration::{AuthorizeResult, FlowError, FlowResult, OAuthMethod, OAuthMethodInfo};
use crate::credentials::{CredentialValue, Metadata, OAuthValue, now_ms};
use async_trait::async_trait;
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

pub const DEFAULT_SERVER: &str = "https://opencode.ai/console";
pub const CLIENT_ID: &str = "opencode-cli";
pub const INTEGRATION_ID: &str = "opencode";
pub const METHOD_ID: &str = "device";
pub const DEVICE_GRANT: &str = "urn:ietf:params:oauth:grant-type:device_code";
/// `SignedOutError.message`.
pub const SIGNED_OUT_MESSAGE: &str = "Reconnect OpenCode Console to continue.";

pub struct ConsoleDevice {
    pub ctx: FlowContext,
    pub timing: PollTiming,
}

impl ConsoleDevice {
    pub fn new(ctx: FlowContext) -> Self {
        Self { ctx, timing: PollTiming::default() }
    }
}

pub fn form() -> Vec<Value> {
    vec![json!({ "key": "server", "type": "string", "format": "uri", "hidden": true, "default": DEFAULT_SERVER })]
}

/// `normalizeServer`: `origin + pathname` without trailing slashes; HTTP(S) only.
pub fn normalize_server(input: Option<&Value>) -> FlowResult<String> {
    let fail = |m: &str| FlowError::new(format!("Invalid OpenCode server URL: {m}"));
    let text = match input {
        None => DEFAULT_SERVER,
        Some(Value::String(s)) => s.as_str(),
        Some(_) => return Err(fail("expected string")),
    };
    let url = url::Url::parse(text).map_err(|_| fail("Invalid URL"))?;
    if url.scheme() != "http" && url.scheme() != "https" {
        return Err(fail("expected HTTP(S)"));
    }
    Ok(format!("{}{}", url.origin().ascii_serialization(), url.path().trim_end_matches('/')))
}

/// `metadata.server` or the default server.
pub fn server_url(value: &CredentialValue) -> String {
    value
        .metadata()
        .and_then(|m| m.get("server"))
        .and_then(Value::as_str)
        .unwrap_or(DEFAULT_SERVER)
        .to_string()
}

/// Request headers for Console calls: `authorization: Bearer <token>` and `x-org-id` when known.
pub fn credential_headers(value: &CredentialValue) -> Vec<(String, String)> {
    let token = match value {
        CredentialValue::Oauth { access, .. } => access.clone(),
        CredentialValue::Key { key, .. } => key.clone(),
        CredentialValue::External { .. } => String::new(),
    };
    let mut out = vec![("authorization".to_string(), format!("Bearer {token}"))];
    if let Some(org) = value.metadata().and_then(|m| m.get("orgID")).and_then(Value::as_str) {
        out.push(("x-org-id".into(), org.into()));
    }
    out
}

fn status_error(status: u16, method: &str, url: &str) -> FlowError {
    FlowError::new(format!("StatusCode: non 2xx status code ({status} {method} {url})"))
}

fn decode_error(what: &str) -> FlowError {
    FlowError::new(format!("Invalid {what} response"))
}

async fn send(ctx: &FlowContext, url: &str, body: Value) -> FlowResult<reqwest::Response> {
    ctx.http
        .post(url)
        .header("Accept", "application/json")
        .header("Content-Type", "application/json")
        .header("User-Agent", ctx.user_agent.as_str())
        .body(body.to_string())
        .send()
        .await
        .map_err(transport)
}

async fn get(ctx: &FlowContext, url: &str, token: &str) -> FlowResult<Value> {
    let response = ctx
        .http
        .get(url)
        .header("Accept", "application/json")
        .header("Authorization", format!("Bearer {token}"))
        .header("User-Agent", ctx.user_agent.as_str())
        .send()
        .await
        .map_err(transport)?;
    let status = response.status().as_u16();
    if !response.status().is_success() {
        return Err(status_error(status, "GET", url));
    }
    response.json().await.map_err(transport)
}

#[derive(Debug, Clone)]
struct Token {
    access_token: String,
    refresh_token: String,
    expires_in: f64,
    org_id: Option<String>,
}

fn decode_token(value: &Value) -> Option<Token> {
    let org_id = match value.get("org_id") {
        None | Some(Value::Null) => None,
        Some(Value::String(s)) => Some(s.clone()),
        Some(_) => return None,
    };
    Some(Token {
        access_token: str_field(value, "access_token")?,
        refresh_token: str_field(value, "refresh_token")?,
        expires_in: value.get("expires_in").and_then(Value::as_f64)?,
        org_id,
    })
}

#[async_trait]
impl OAuthMethod for ConsoleDevice {
    fn integration_id(&self) -> String {
        INTEGRATION_ID.into()
    }

    fn info(&self) -> OAuthMethodInfo {
        OAuthMethodInfo::new(METHOD_ID, "OpenCode Console account").with_form(form())
    }

    async fn authorize(&self, answer: FormAnswer, _scope: CancellationToken) -> FlowResult<AuthorizeResult> {
        let server = normalize_server(answer.get("server"))?;
        let url = format!("{server}/auth/device/code");
        let response = send(&self.ctx, &url, json!({ "client_id": CLIENT_ID, "supports_org_scope": true })).await?;
        if !response.status().is_success() {
            return Err(status_error(response.status().as_u16(), "POST", &url));
        }
        let device: Value = response.json().await.map_err(transport)?;
        let device_code = str_field(&device, "device_code").ok_or_else(|| decode_error("device"))?;
        let user_code = str_field(&device, "user_code").ok_or_else(|| decode_error("device"))?;
        let complete = str_field(&device, "verification_uri_complete").ok_or_else(|| decode_error("device"))?;
        let interval = device.get("interval").and_then(Value::as_f64).ok_or_else(|| decode_error("device"))?;
        device.get("expires_in").and_then(Value::as_f64).ok_or_else(|| decode_error("device"))?;
        let verification = url::Url::parse(&format!("{server}/"))
            .and_then(|base| base.join(&complete))
            .map_err(|_| "Invalid URL".to_string())
            .and_then(|url| {
                if url.scheme() == "http" || url.scheme() == "https" {
                    Ok(url)
                } else {
                    Err("expected HTTP(S)".to_string())
                }
            })
            .map_err(|m| FlowError::new(format!("Invalid device verification URL: {m}")))?;
        let ctx = self.ctx.clone();
        let timing = self.timing;
        let callback = async move {
            let mut wait = (interval * timing.second_ms as f64).max(0.0) as u64;
            loop {
                sleep_ms(wait).await;
                let response = send(
                    &ctx,
                    &format!("{server}/auth/device/token"),
                    json!({ "grant_type": DEVICE_GRANT, "device_code": device_code, "client_id": CLIENT_ID }),
                )
                .await?;
                let body: Value = response.json().await.map_err(|_| decode_error("token"))?;
                if let Some(token) = decode_token(&body) {
                    return credential(&ctx, &server, token).await;
                }
                let Some(error) = str_field(&body, "error") else { return Err(decode_error("token")) };
                match error.as_str() {
                    "authorization_pending" => {}
                    "slow_down" => wait += 5 * timing.second_ms,
                    _ => return Err(FlowError::new(format!("Device authorization failed: {error}"))),
                }
            }
        };
        Ok(AuthorizeResult::auto(verification.to_string(), format!("Enter code: {user_code}"), callback))
    }

    fn can_refresh(&self) -> bool {
        true
    }

    async fn refresh(&self, value: OAuthValue) -> FlowResult<OAuthValue> {
        refresh(&self.ctx, value).await
    }

    fn label(&self, value: &OAuthValue) -> Option<String> {
        value.meta_str("orgName").map(String::from)
    }
}

async fn credential(ctx: &FlowContext, server: &str, token: Token) -> FlowResult<OAuthValue> {
    let (user_url, orgs_url) = (format!("{server}/api/user"), format!("{server}/api/orgs"));
    let (user, orgs) =
        tokio::try_join!(get(ctx, &user_url, &token.access_token), get(ctx, &orgs_url, &token.access_token))?;
    let user_id = str_field(&user, "id").ok_or_else(|| decode_error("user"))?;
    let email = str_field(&user, "email").ok_or_else(|| decode_error("user"))?;
    let orgs: Vec<(String, String)> = orgs
        .as_array()
        .ok_or_else(|| decode_error("orgs"))?
        .iter()
        .map(|o| Some((str_field(o, "id")?, str_field(o, "name")?)))
        .collect::<Option<_>>()
        .ok_or_else(|| decode_error("orgs"))?;
    let org = match &token.org_id {
        None => {
            let mut sorted = orgs.clone();
            sorted.sort_by(|a, b| {
                crate::credentials::integration::locale_compare(&a.1, &b.1)
                    .then_with(|| crate::credentials::integration::locale_compare(&a.0, &b.0))
            });
            sorted.into_iter().next()
        }
        Some(id) => orgs.iter().find(|o| &o.0 == id).cloned(),
    };
    if let (Some(id), None) = (&token.org_id, &org) {
        return Err(FlowError::new(format!("OpenCode organization not found: {id}")));
    }
    let mut metadata = Metadata::new();
    metadata.insert("server".into(), json!(server));
    metadata.insert("accountID".into(), json!(user_id));
    metadata.insert("email".into(), json!(email));
    if let Some((id, name)) = org {
        metadata.insert("orgID".into(), json!(id));
        metadata.insert("orgName".into(), json!(name));
    }
    Ok(OAuthValue {
        method_id: METHOD_ID.into(),
        access: token.access_token,
        refresh: token.refresh_token,
        expires: (now_ms() as f64 + token.expires_in * 1000.0).max(0.0) as u64,
        metadata: Some(metadata),
    })
}

/// Refresh: 400 `invalid_grant` → [`SIGNED_OUT_MESSAGE`]; org changes update `orgID`/`orgName`.
pub async fn refresh(ctx: &FlowContext, value: OAuthValue) -> FlowResult<OAuthValue> {
    let server = server_url(&value.clone().into_value());
    let url = format!("{server}/auth/device/token");
    let response = send(
        ctx,
        &url,
        json!({ "grant_type": "refresh_token", "refresh_token": value.refresh, "client_id": CLIENT_ID }),
    )
    .await?;
    let status = response.status().as_u16();
    let text = response.text().await.map_err(transport)?;
    let body: Option<Value> = serde_json::from_str(&text).ok();
    if status == 400 && body.as_ref().and_then(|b| b.get("error")).and_then(Value::as_str) == Some("invalid_grant") {
        return Err(FlowError::new(SIGNED_OUT_MESSAGE));
    }
    if !(200..300).contains(&status) {
        return Err(status_error(status, "POST", &url));
    }
    let token = body.as_ref().and_then(decode_token).ok_or_else(|| decode_error("token"))?;
    let metadata = match &token.org_id {
        None => value.metadata.clone(),
        Some(org_id) => {
            let mut metadata = value.metadata.clone().unwrap_or_default();
            let same = metadata.get("orgID").and_then(Value::as_str) == Some(org_id.as_str());
            let org_name = match metadata.get("orgName").and_then(Value::as_str) {
                Some(name) if same => name.to_string(),
                _ => org_id.clone(),
            };
            metadata.insert("orgID".into(), json!(org_id));
            metadata.insert("orgName".into(), json!(org_name));
            Some(metadata)
        }
    };
    Ok(OAuthValue {
        method_id: value.method_id,
        access: token.access_token,
        refresh: token.refresh_token,
        expires: (now_ms() as f64 + token.expires_in * 1000.0).max(0.0) as u64,
        metadata,
    })
}

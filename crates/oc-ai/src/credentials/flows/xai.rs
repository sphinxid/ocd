//! xAI device flow (`packages/core/src/plugin/provider/xai.ts`, spec §6.4.5).

use super::{PollTiming, sleep_ms};
use crate::credentials::form::FormAnswer;
use crate::credentials::http::{FlowContext, str_field, transport};
use crate::credentials::integration::{AuthorizeResult, FlowError, FlowResult, OAuthMethod, OAuthMethodInfo};
use crate::credentials::pkce;
use crate::credentials::{CredentialValue, Metadata, OAuthValue, now_ms};
use async_trait::async_trait;
use serde_json::Value;
use tokio_util::sync::CancellationToken;

pub const CLIENT_ID: &str = "b1a00492-073a-47ea-816f-4c329264a828";
pub const ISSUER: &str = "https://auth.x.ai/oauth2";
pub const DEVICE_GRANT: &str = "urn:ietf:params:oauth:grant-type:device_code";
pub const SCOPE: &str = "openid profile email offline_access grok-cli:access api:access";
pub const INTEGRATION_ID: &str = "xai";
pub const BROWSER_METHOD_ID: &str = "browser";
pub const DEVICE_METHOD_ID: &str = "device";

pub struct XaiDevice {
    pub ctx: FlowContext,
    pub issuer: String,
    pub timing: PollTiming,
}

impl XaiDevice {
    pub fn new(ctx: FlowContext) -> Self {
        Self { ctx, issuer: ISSUER.into(), timing: PollTiming::default() }
    }
}

/// Legacy `browser` OAuth credentials are migrated to `device`; returns the new value when changed.
pub fn migrate_legacy(value: &CredentialValue) -> Option<CredentialValue> {
    match value {
        CredentialValue::Oauth { method_id, refresh, access, expires, metadata } if method_id == BROWSER_METHOD_ID => {
            Some(CredentialValue::Oauth {
                method_id: DEVICE_METHOD_ID.into(),
                refresh: refresh.clone(),
                access: access.clone(),
                expires: *expires,
                metadata: metadata.clone(),
            })
        }
        _ => None,
    }
}

/// `positiveSeconds(value, fallback)`.
fn positive_seconds(value: Option<&Value>, fallback: f64) -> f64 {
    let seconds = match value {
        Some(Value::Number(n)) => n.as_f64().unwrap_or(f64::NAN),
        Some(Value::String(s)) => s.trim().parse::<f64>().unwrap_or(if s.trim().is_empty() { 0.0 } else { f64::NAN }),
        Some(Value::Bool(b)) => f64::from(u8::from(*b)),
        Some(Value::Null) => 0.0,
        _ => f64::NAN,
    };
    if seconds.is_finite() && seconds > 0.0 { seconds } else { fallback }
}

fn request(ctx: &FlowContext, url: &str, pairs: &[(&str, &str)]) -> reqwest::RequestBuilder {
    ctx.http
        .post(url)
        .header("Content-Type", "application/x-www-form-urlencoded")
        .header("Accept", "application/json")
        .header("User-Agent", ctx.user_agent.as_str())
        .body(pkce::form_encode(pairs))
}

/// `request(url, init, schema)`: non-OK → `xAI request failed (<status>)[: <body>]`.
async fn request_ok(builder: reqwest::RequestBuilder) -> FlowResult<Value> {
    let response = builder.send().await.map_err(transport)?;
    let status = response.status();
    if status.is_success() {
        return response.json().await.map_err(transport);
    }
    let detail = response.text().await.unwrap_or_default();
    Err(FlowError::new(format!(
        "xAI request failed ({}){}",
        status.as_u16(),
        if detail.is_empty() { String::new() } else { format!(": {detail}") }
    )))
}

fn decode_token(value: &Value) -> FlowResult<(String, Option<String>, Option<f64>)> {
    let invalid = || FlowError::new("Invalid xAI token response");
    let access = str_field(value, "access_token").ok_or_else(invalid)?;
    let refresh = match value.get("refresh_token") {
        None => None,
        Some(Value::String(s)) => Some(s.clone()),
        Some(_) => return Err(invalid()),
    };
    let expires_in = match value.get("expires_in") {
        None => None,
        Some(v) => Some(v.as_f64().ok_or_else(invalid)?),
    };
    Ok((access, refresh, expires_in))
}

#[async_trait]
impl OAuthMethod for XaiDevice {
    fn integration_id(&self) -> String {
        INTEGRATION_ID.into()
    }

    fn info(&self) -> OAuthMethodInfo {
        OAuthMethodInfo::new(DEVICE_METHOD_ID, "SuperGrok Subscription")
    }

    async fn authorize(&self, _answer: FormAnswer, _scope: CancellationToken) -> FlowResult<AuthorizeResult> {
        let device = request_ok(request(
            &self.ctx,
            &format!("{}/device/code", self.issuer),
            &[("client_id", CLIENT_ID), ("scope", SCOPE), ("referrer", "opencode")],
        ))
        .await?;
        let invalid = || FlowError::new("Invalid xAI device response");
        let device_code = str_field(&device, "device_code").ok_or_else(invalid)?;
        let user_code = str_field(&device, "user_code").ok_or_else(invalid)?;
        let verification_uri = str_field(&device, "verification_uri").ok_or_else(invalid)?;
        let complete = str_field(&device, "verification_uri_complete");
        let created = now_ms();
        let lifetime = positive_seconds(device.get("expires_in"), 0.0);
        let ctx = self.ctx.clone();
        let issuer = self.issuer.clone();
        let timing = self.timing;
        let expires_in = device.get("expires_in").cloned();
        let interval = device.get("interval").cloned();
        let callback = async move {
            let tokens = poll(&ctx, &issuer, &device_code, expires_in.as_ref(), interval.as_ref(), timing).await?;
            credential(DEVICE_METHOD_ID, &tokens, None, None)
        };
        let mut result = AuthorizeResult::auto(
            complete.unwrap_or_else(|| verification_uri.clone()),
            format!("Open {verification_uri} on any device and enter code: {user_code}"),
            callback,
        );
        if lifetime > 0.0 {
            result.expires_at = Some(created + (lifetime * 1000.0) as u64);
        }
        Ok(result)
    }

    fn can_refresh(&self) -> bool {
        true
    }

    async fn refresh(&self, value: OAuthValue) -> FlowResult<OAuthValue> {
        let tokens = request_ok(request(
            &self.ctx,
            &format!("{}/token", self.issuer),
            &[("grant_type", "refresh_token"), ("refresh_token", &value.refresh), ("client_id", CLIENT_ID)],
        ))
        .await?;
        credential(DEVICE_METHOD_ID, &tokens, Some(&value.refresh), value.metadata.clone())
    }
}

async fn poll(
    ctx: &FlowContext,
    issuer: &str,
    device_code: &str,
    expires_in: Option<&Value>,
    interval: Option<&Value>,
    timing: PollTiming,
) -> FlowResult<Value> {
    let started = now_ms();
    let expires = started + (positive_seconds(expires_in, 300.0) * timing.second_ms as f64) as u64;
    let mut interval = ((positive_seconds(interval, 5.0) * timing.second_ms as f64) as u64).max(timing.second_ms);
    loop {
        if now_ms() >= expires {
            return Err(FlowError::new("xAI device authorization timed out"));
        }
        let response = request(
            ctx,
            &format!("{issuer}/token"),
            &[("grant_type", DEVICE_GRANT), ("client_id", CLIENT_ID), ("device_code", device_code)],
        )
        .send()
        .await
        .map_err(transport)?;
        let status = response.status();
        if status.is_success() {
            let body: Value = response.json().await.map_err(transport)?;
            decode_token(&body)?;
            return Ok(body);
        }
        let error: Option<Value> = response.text().await.ok().and_then(|t| serde_json::from_str(&t).ok());
        let field = |k: &str| error.as_ref().and_then(|e| e.get(k)).and_then(Value::as_str).map(String::from);
        let code = field("error");
        match code.as_deref() {
            Some("authorization_pending") => sleep_ms(interval + timing.margin_ms).await,
            Some("slow_down") => {
                interval += 5 * timing.second_ms;
                sleep_ms(interval + timing.margin_ms).await;
            }
            Some("access_denied") | Some("authorization_denied") => {
                return Err(FlowError::new("xAI device authorization was denied"));
            }
            Some("expired_token") => return Err(FlowError::new("xAI device code expired - please re-run login")),
            _ => {
                let detail = field("error_description").or(code);
                return Err(FlowError::new(format!(
                    "xAI device token exchange failed ({}){}",
                    status.as_u16(),
                    detail.map(|d| format!(": {d}")).unwrap_or_default()
                )));
            }
        }
    }
}

/// Builds the credential; the refresh token falls back to the current one.
pub fn credential(
    method_id: &str,
    tokens: &Value,
    current_refresh: Option<&str>,
    metadata: Option<Metadata>,
) -> FlowResult<OAuthValue> {
    let (access, refresh, expires_in) = decode_token(tokens)?;
    let refresh = refresh
        .filter(|r| !r.is_empty())
        .or_else(|| current_refresh.filter(|r| !r.is_empty()).map(String::from))
        .ok_or_else(|| FlowError::new("xAI token response is missing refresh_token"))?;
    Ok(OAuthValue {
        method_id: method_id.into(),
        refresh,
        expires: token_expiration(&access, expires_in),
        access,
        metadata,
    })
}

/// `expires_in` (seconds) or the JWT `exp` or now + 1 h.
pub fn token_expiration(access: &str, expires_in: Option<f64>) -> u64 {
    let now = now_ms();
    if let Some(seconds) = expires_in.filter(|s| *s != 0.0 && !s.is_nan()) {
        let seconds = if seconds.is_finite() && seconds > 0.0 { seconds } else { 3600.0 };
        return now + (seconds * 1000.0) as u64;
    }
    match pkce::jwt_claims(access).and_then(|c| c.get("exp").and_then(Value::as_f64)) {
        Some(exp) if exp != 0.0 => (exp * 1000.0).max(0.0) as u64,
        _ => now + 3600 * 1000,
    }
}

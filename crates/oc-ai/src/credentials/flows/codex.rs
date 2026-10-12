//! Codex (legacy) ChatGPT login: browser + device code (`packages/core/src/plugin/provider/openai.ts`,
//! spec §6.4.2).

use super::{PollTiming, await_deferred, sleep_ms};
use crate::credentials::callback::{Bound, Once, Response, sync_handler};
use crate::credentials::form::FormAnswer;
use crate::credentials::http::{FlowContext, request_json, str_field, transport};
use crate::credentials::integration::{AuthorizeResult, FlowError, FlowResult, OAuthMethod, OAuthMethodInfo};
use crate::credentials::page::{self, CallbackPageOptions};
use crate::credentials::pkce::{self, Pkce};
use crate::credentials::{Metadata, OAuthValue, now_ms};
use async_trait::async_trait;
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

pub const CLIENT_ID: &str = "app_EMoamEEZ73f0CkXaXp7hrann";
pub const ISSUER: &str = "https://auth.openai.com";
pub const CALLBACK_PORT: u16 = 1455;
pub const CALLBACK_FALLBACK_PORT: u16 = 1457;
pub const CALLBACK_BIND_ATTEMPTS: u32 = 10;
pub const CALLBACK_BIND_RETRY_DELAY_MS: u64 = 200;
pub const CODEX_BASE_URL: &str = "https://chatgpt.com/backend-api/codex";
pub const INTEGRATION_ID: &str = "openai";
pub const BROWSER_METHOD_ID: &str = "chatgpt-browser";
pub const HEADLESS_METHOD_ID: &str = "chatgpt-headless";
pub const INSTRUCTIONS: &str = "Complete authorization in your browser. This window will close automatically.";

/// Browser login on `localhost:1455` (fallback 1457).
pub struct CodexBrowser {
    pub ctx: FlowContext,
    pub issuer: String,
    pub port: u16,
    pub fallback_port: u16,
}

impl CodexBrowser {
    pub fn new(ctx: FlowContext) -> Self {
        Self { ctx, issuer: ISSUER.into(), port: CALLBACK_PORT, fallback_port: CALLBACK_FALLBACK_PORT }
    }
}

/// Authorize URL (`authorizeURL`).
pub fn authorize_url(issuer: &str, redirect: &str, pkce: &Pkce, state: &str) -> String {
    format!(
        "{issuer}/oauth/authorize?{}",
        pkce::form_encode(&[
            ("response_type", "code"),
            ("client_id", CLIENT_ID),
            ("redirect_uri", redirect),
            ("scope", "openid profile email offline_access"),
            ("code_challenge", pkce.challenge.as_str()),
            ("code_challenge_method", "S256"),
            ("id_token_add_organizations", "true"),
            ("codex_cli_simplified_flow", "true"),
            ("state", state),
            ("originator", "opencode"),
        ])
    )
}

fn options() -> CallbackPageOptions<'static> {
    CallbackPageOptions::provider("ChatGPT")
}

async fn bind_once(port: u16) -> std::io::Result<Bound> {
    crate::credentials::callback::bind("localhost", port).await
}

async fn bind_with_retry(port: u16, attempts: u32) -> std::io::Result<Bound> {
    let mut remaining = attempts;
    loop {
        match bind_once(port).await {
            Err(error) if error.kind() == std::io::ErrorKind::AddrInUse && remaining > 1 => {
                remaining -= 1;
                sleep_ms(CALLBACK_BIND_RETRY_DELAY_MS).await;
            }
            other => return other,
        }
    }
}

/// `listen`: 1455; on EADDRINUSE ask the previous login to `/cancel` (2 s), retry 10× every 200 ms, then 1457.
async fn listen(http: &reqwest::Client, port: u16, fallback: u16) -> FlowResult<Bound> {
    let in_use = |e: &std::io::Error| e.kind() == std::io::ErrorKind::AddrInUse;
    match bind_once(port).await {
        Ok(bound) => return Ok(bound),
        Err(error) if !in_use(&error) => return Err(FlowError::new(error.to_string())),
        Err(_) => {}
    }
    let _ = http
        .get(format!("http://localhost:{port}/cancel"))
        .timeout(std::time::Duration::from_millis(2000))
        .send()
        .await;
    sleep_ms(CALLBACK_BIND_RETRY_DELAY_MS).await;
    match bind_with_retry(port, CALLBACK_BIND_ATTEMPTS - 1).await {
        Ok(bound) => return Ok(bound),
        Err(error) if !in_use(&error) => return Err(FlowError::new(error.to_string())),
        Err(_) => {}
    }
    match bind_with_retry(fallback, CALLBACK_BIND_ATTEMPTS).await {
        Ok(bound) => Ok(bound),
        Err(error) if in_use(&error) => Err(FlowError::new(format!(
            "OpenAI browser login needs local port {port} or {fallback}, but both are already in use. Stop the processes using those ports or choose Codex device code (legacy), then try again."
        ))),
        Err(error) => Err(FlowError::new(error.to_string())),
    }
}

#[async_trait]
impl OAuthMethod for CodexBrowser {
    fn integration_id(&self) -> String {
        INTEGRATION_ID.into()
    }

    fn info(&self) -> OAuthMethodInfo {
        OAuthMethodInfo::new(BROWSER_METHOD_ID, "Codex browser (legacy)")
    }

    async fn authorize(&self, _answer: FormAnswer, scope: CancellationToken) -> FlowResult<AuthorizeResult> {
        let pkce = pkce::openai_pkce();
        let state = pkce::random_state();
        let (once, rx) = Once::<FlowResult<String>>::new();
        let expected = state.clone();
        let handler = sync_handler(move |request| {
            if request.path != "/auth/callback" {
                return Response::not_found("Not found");
            }
            let error = request.param("error_description").or_else(|| request.param("error"));
            let value = request.param("code").filter(|c| !c.is_empty());
            if let Some(error) = error {
                once.complete(Err(FlowError::new(error.clone())));
                return Response::html(400, page::error(&error, &options()));
            }
            match value {
                Some(value) if request.param("state").as_deref() == Some(expected.as_str()) => {
                    once.complete(Ok(value));
                    Response::html(200, page::success(&options()))
                }
                value => {
                    let message = if value.is_some() { "Invalid OAuth state" } else { "Missing authorization code" };
                    once.complete(Err(FlowError::new(message)));
                    Response::html(400, page::error(message, &options()))
                }
            }
        });
        let bound = listen(&self.ctx.http, self.port, self.fallback_port).await?;
        let port = bound.port;
        bound.serve(handler, scope);
        let redirect = format!("http://localhost:{port}/auth/callback");
        let url = authorize_url(&self.issuer, &redirect, &pkce, &state);
        let ctx = self.ctx.clone();
        let issuer = self.issuer.clone();
        let callback = async move {
            let code = await_deferred(rx).await?;
            let tokens = exchange(&ctx, &issuer, &code, &redirect, &pkce.verifier).await?;
            credential(BROWSER_METHOD_ID, &tokens)
        };
        Ok(AuthorizeResult::auto(url, INSTRUCTIONS, callback))
    }

    fn can_refresh(&self) -> bool {
        true
    }

    async fn refresh(&self, value: OAuthValue) -> FlowResult<OAuthValue> {
        refresh(&self.ctx, &self.issuer, BROWSER_METHOD_ID, &value).await
    }
}

/// Device-code login (`chatgpt-headless`).
pub struct CodexDevice {
    pub ctx: FlowContext,
    pub issuer: String,
    pub timing: PollTiming,
}

impl CodexDevice {
    pub fn new(ctx: FlowContext) -> Self {
        Self { ctx, issuer: ISSUER.into(), timing: PollTiming::default() }
    }
}

fn json_headers(ctx: &FlowContext, request: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
    request.header("Content-Type", "application/json").header("User-Agent", ctx.user_agent.as_str())
}

#[async_trait]
impl OAuthMethod for CodexDevice {
    fn integration_id(&self) -> String {
        INTEGRATION_ID.into()
    }

    fn info(&self) -> OAuthMethodInfo {
        OAuthMethodInfo::new(HEADLESS_METHOD_ID, "Codex device code (legacy)")
    }

    async fn authorize(&self, _answer: FormAnswer, scope: CancellationToken) -> FlowResult<AuthorizeResult> {
        let issuer = self.issuer.clone();
        let device = request_json(
            json_headers(&self.ctx, self.ctx.http.post(format!("{issuer}/api/accounts/deviceauth/usercode")))
                .body(json!({ "client_id": CLIENT_ID }).to_string()),
        )
        .await?;
        let device_auth_id = str_field(&device, "device_auth_id").unwrap_or_default();
        let user_code = str_field(&device, "user_code").unwrap_or_default();
        let interval_seconds = match device.get("interval") {
            Some(Value::String(s)) => super::parse_int(s),
            Some(Value::Number(n)) => n.as_f64().map(|f| f.trunc() as i64),
            _ => None,
        }
        .filter(|n| *n != 0)
        .unwrap_or(5)
        .max(1) as u64;
        let interval = interval_seconds * self.timing.second_ms;
        let margin = self.timing.margin_ms;
        let ctx = self.ctx.clone();
        let instructions = format!("Enter code: {user_code}");
        let poll_issuer = issuer.clone();
        let callback = async move {
            loop {
                if scope.is_cancelled() {
                    return Err(FlowError::new("Authorization cancelled"));
                }
                let response = json_headers(&ctx, ctx.http.post(format!("{poll_issuer}/api/accounts/deviceauth/token")))
                    .body(json!({ "device_auth_id": device_auth_id, "user_code": user_code }).to_string())
                    .send()
                    .await
                    .map_err(transport)?;
                let status = response.status().as_u16();
                if response.status().is_success() {
                    let data: Value = response.json().await.map_err(transport)?;
                    let code = str_field(&data, "authorization_code").unwrap_or_default();
                    let verifier = str_field(&data, "code_verifier").unwrap_or_default();
                    let redirect = format!("{poll_issuer}/deviceauth/callback");
                    let tokens = exchange(&ctx, &poll_issuer, &code, &redirect, &verifier).await?;
                    return credential(HEADLESS_METHOD_ID, &tokens);
                }
                if status != 403 && status != 404 {
                    return Err(FlowError::new(format!("Device authorization failed: {status}")));
                }
                sleep_ms(interval + margin).await;
            }
        };
        Ok(AuthorizeResult::auto(format!("{issuer}/codex/device"), instructions, callback))
    }

    fn can_refresh(&self) -> bool {
        true
    }

    async fn refresh(&self, value: OAuthValue) -> FlowResult<OAuthValue> {
        refresh(&self.ctx, &self.issuer, HEADLESS_METHOD_ID, &value).await
    }
}

async fn post_form(ctx: &FlowContext, url: &str, pairs: &[(&str, &str)]) -> FlowResult<Value> {
    request_json(
        ctx.http
            .post(url)
            .header("Content-Type", "application/x-www-form-urlencoded")
            .header("User-Agent", ctx.user_agent.as_str())
            .body(pkce::form_encode(pairs)),
    )
    .await
}

/// Token exchange: form `grant_type=authorization_code, code, redirect_uri, client_id, code_verifier`.
pub async fn exchange(ctx: &FlowContext, issuer: &str, code: &str, redirect: &str, verifier: &str) -> FlowResult<Value> {
    post_form(
        ctx,
        &format!("{issuer}/oauth/token"),
        &[
            ("grant_type", "authorization_code"),
            ("code", code),
            ("redirect_uri", redirect),
            ("client_id", CLIENT_ID),
            ("code_verifier", verifier),
        ],
    )
    .await
}

/// Refresh: form `grant_type=refresh_token, refresh_token, client_id`; metadata preserved when not re-derived.
pub async fn refresh(ctx: &FlowContext, issuer: &str, method_id: &str, value: &OAuthValue) -> FlowResult<OAuthValue> {
    let tokens = post_form(
        ctx,
        &format!("{issuer}/oauth/token"),
        &[("grant_type", "refresh_token"), ("refresh_token", &value.refresh), ("client_id", CLIENT_ID)],
    )
    .await?;
    let mut next = credential(method_id, &tokens)?;
    if next.metadata.is_none() {
        next.metadata = value.metadata.clone();
    }
    Ok(next)
}

/// Credential with `metadata.accountID` from JWT claims (id_token, then access_token).
pub fn credential(method_id: &str, tokens: &Value) -> FlowResult<OAuthValue> {
    let access = str_field(tokens, "access_token").ok_or_else(|| FlowError::new("Invalid token response"))?;
    let refresh = str_field(tokens, "refresh_token").ok_or_else(|| FlowError::new("Invalid token response"))?;
    let id_token = str_field(tokens, "id_token");
    let account = pkce::extract_account_id(id_token.as_deref(), &access).filter(|a| !a.is_empty());
    let expires_in = tokens.get("expires_in").and_then(Value::as_f64).unwrap_or(3600.0);
    Ok(OAuthValue {
        method_id: method_id.into(),
        refresh,
        access,
        expires: (now_ms() as f64 + expires_in * 1000.0).max(0.0) as u64,
        metadata: account.map(|account| {
            let mut metadata = Metadata::new();
            metadata.insert("accountID".into(), json!(account));
            metadata
        }),
    })
}

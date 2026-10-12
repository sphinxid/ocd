//! Poe browser login (`packages/core/src/plugin/provider/poe.ts`, spec §6.4.9).

use super::{Received, await_deferred, bind};
use crate::credentials::callback::{Handler, Once, Request, Response};
use crate::credentials::form::FormAnswer;
use crate::credentials::http::FlowContext;
use crate::credentials::integration::{AuthorizeResult, FlowError, FlowResult, OAuthMethod, OAuthMethodInfo};
use crate::credentials::page::{self, CallbackPageOptions};
use crate::credentials::pkce;
use crate::credentials::{OAuthValue, now_ms};
use async_trait::async_trait;
use serde_json::Value;
use std::sync::Arc;
use tokio_util::sync::CancellationToken;

pub const INTEGRATION_ID: &str = "poe";
pub const METHOD_ID: &str = "browser";
pub const CLIENT_ID: &str = "client_728290227fc048cc9262091a1ea197ea";
pub const ISSUER: &str = "https://poe.com";
pub const TOKEN_URL: &str = "https://api.poe.com/token";
pub const MAX_EXPIRY: u64 = 8_640_000_000_000_000;
pub const EXPIRED_MESSAGE: &str = "Poe API key expired. Log in with Poe again.";
pub const INSTRUCTIONS: &str = "Complete authorization in your browser. This window will close automatically.";

pub struct PoeBrowser {
    pub ctx: FlowContext,
    pub issuer: String,
    pub token_url: String,
}

impl PoeBrowser {
    pub fn new(ctx: FlowContext) -> Self {
        Self { ctx, issuer: ISSUER.into(), token_url: TOKEN_URL.into() }
    }
}

/// `callbackError` (the issuer check always compares with `https://poe.com`).
pub fn callback_error(request: &Request, state: &str) -> Option<String> {
    if request.param("state").as_deref() != Some(state) {
        return Some("Invalid OAuth state".into());
    }
    if request.has_param("iss") && request.param("iss").as_deref() != Some(ISSUER) {
        return Some("Invalid OAuth issuer".into());
    }
    if request.has_param("error") {
        let detail = request
            .param("error_description")
            .filter(|v| !v.is_empty())
            .or_else(|| request.param("error").filter(|v| !v.is_empty()))
            .unwrap_or_else(|| "Authorization denied".into());
        return Some(if detail.contains(state) { "Poe authorization failed".into() } else { detail });
    }
    if request.param("code").is_some_and(|c| !c.trim().is_empty()) { None } else { Some("Missing authorization code".into()) }
}

fn options() -> CallbackPageOptions<'static> {
    CallbackPageOptions::provider("Poe")
}

/// `exchangeCode`.
pub async fn exchange_code(
    ctx: &FlowContext,
    token_url: &str,
    code: &str,
    redirect: &str,
    verifier: &str,
) -> FlowResult<OAuthValue> {
    let response = ctx
        .http
        .post(token_url)
        .header("Content-Type", "application/x-www-form-urlencoded")
        .body(pkce::form_encode(&[
            ("grant_type", "authorization_code"),
            ("client_id", CLIENT_ID),
            ("code", code),
            ("redirect_uri", redirect),
            ("code_verifier", verifier),
        ]))
        .send()
        .await
        .map_err(|_| FlowError::new("Poe token exchange request failed"))?;
    let status = response.status().as_u16();
    if !(200..300).contains(&status) {
        let text = response.text().await.unwrap_or_default();
        let parsed: Option<Value> = serde_json::from_str(&text).ok();
        let field = |k: &str| {
            parsed.as_ref().and_then(|p| p.get(k)).and_then(Value::as_str).filter(|v| !v.is_empty()).map(String::from)
        };
        let detail = field("error_description").or_else(|| field("error"));
        return Err(FlowError::new(match detail {
            Some(detail) if !detail.contains(code) && !detail.contains(verifier) => {
                format!("Poe token exchange failed: {detail}")
            }
            _ => format!("Poe token exchange failed ({status})"),
        }));
    }
    let invalid = || FlowError::new("Invalid Poe token response");
    let body: Value = response.json().await.map_err(|_| invalid())?;
    let api_key = body.get("api_key").and_then(Value::as_str).map(str::trim).ok_or_else(invalid)?;
    if api_key.is_empty() || api_key.chars().any(char::is_whitespace) {
        return Err(invalid());
    }
    let expires = match body.get("api_key_expires_in") {
        None | Some(Value::Null) => MAX_EXPIRY as f64,
        Some(v) => {
            let n = v.as_f64().filter(|n| n.fract() == 0.0 && *n >= 0.0).ok_or_else(invalid)?;
            now_ms() as f64 + n * 1000.0
        }
    };
    if !(expires <= 9_007_199_254_740_991.0) || expires > MAX_EXPIRY as f64 {
        return Err(FlowError::new("Invalid Poe API key expiry"));
    }
    Ok(OAuthValue {
        method_id: METHOD_ID.into(),
        access: api_key.to_string(),
        refresh: String::new(),
        expires: expires as u64,
        metadata: None,
    })
}

#[async_trait]
impl OAuthMethod for PoeBrowser {
    fn integration_id(&self) -> String {
        INTEGRATION_ID.into()
    }

    fn info(&self) -> OAuthMethodInfo {
        OAuthMethodInfo::new(METHOD_ID, "Login with Poe (browser)")
    }

    async fn authorize(&self, _answer: FormAnswer, scope: CancellationToken) -> FlowResult<AuthorizeResult> {
        let pkce = pkce::base64_pkce(32);
        let state = pkce::random_state();
        let (once, rx) = Once::<FlowResult<Received<String>>>::new();
        let expected = state.clone();
        let handler: Handler = Arc::new(move |request: Request| {
            let once = once.clone();
            let state = expected.clone();
            Box::pin(async move {
                if request.method != "GET" || request.path != "/callback" {
                    return Some(Response::new(404, ""));
                }
                if let Some(error) = callback_error(&request, &state) {
                    once.complete(Err(FlowError::new(error.clone())));
                    return Some(Response::html(400, page::error(&error, &options())));
                }
                let (responder, response) = tokio::sync::oneshot::channel();
                if !once.complete(Ok(Received { value: request.param("code").unwrap_or_default(), responder })) {
                    return Some(Response::new(409, "OAuth callback already received"));
                }
                response.await.ok()
            })
        });
        let bound = bind("127.0.0.1", 0, None).await?;
        let port = bound.port;
        bound.serve(handler, scope);
        let redirect = format!("http://127.0.0.1:{port}/callback");
        let url = format!(
            "{}/oauth/authorize?{}",
            self.issuer,
            pkce::form_encode(&[
                ("response_type", "code"),
                ("client_id", CLIENT_ID),
                ("redirect_uri", redirect.as_str()),
                ("scope", "apikey:create"),
                ("code_challenge", pkce.challenge.as_str()),
                ("code_challenge_method", "S256"),
                ("state", state.as_str()),
            ])
        );
        let ctx = self.ctx.clone();
        let token_url = self.token_url.clone();
        let callback = async move {
            let Received { value: code, responder } = await_deferred(rx).await?;
            let result = exchange_code(&ctx, &token_url, &code, &redirect, &pkce.verifier).await;
            let page = match &result {
                Ok(_) => Response::html(200, page::success(&options())),
                Err(error) => Response::html(400, page::error(&error.message, &options())),
            };
            let _ = responder.send(page);
            result
        };
        Ok(AuthorizeResult::auto(url, INSTRUCTIONS, callback))
    }

    fn can_refresh(&self) -> bool {
        true
    }

    /// Poe-issued API keys remain usable until expiry, then require another login.
    async fn refresh(&self, value: OAuthValue) -> FlowResult<OAuthValue> {
        if value.expires > now_ms() { Ok(value) } else { Err(FlowError::new(EXPIRED_MESSAGE)) }
    }
}

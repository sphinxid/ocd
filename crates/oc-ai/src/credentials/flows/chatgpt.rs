//! ChatGPT token sharing (`packages/core/src/plugin/provider/chatgpt.ts`, spec §6.4.1).

use super::{Received, await_deferred, bind};
use crate::credentials::callback::{Handler, Once, Request, Response};
use crate::credentials::form::FormAnswer;
use crate::credentials::http::{FlowContext, str_field};
use crate::credentials::integration::{AuthorizeResult, FlowError, FlowResult, OAuthMethod, OAuthMethodInfo};
use crate::credentials::kv::KvStore;
use crate::credentials::page::{self, CallbackPageOptions};
use crate::credentials::pkce::{self, Pkce};
use crate::credentials::{CredentialValue, Metadata, OAuthValue, now_ms};
use async_trait::async_trait;
use serde_json::{Value, json};
use std::sync::Arc;
use tokio_util::sync::CancellationToken;

/// First-time sign-in registers a user-owned client; OpenAI returns its issued client ID on the callback.
pub const REGISTRATION_CLIENT_ID: &str = "dynamic_agent_client";
pub const AGENT_NAME: &str = "OpenCode";
pub const ISSUER: &str = "https://auth.openai.com";
pub const RESOURCE: &str = "https://api.openai.com/v1";
pub const TOKEN_SHARING_SCOPE: &str = "chatgpt.tokens.use.direct";
pub const INTEGRATION_ID: &str = "openai";
pub const METHOD_ID: &str = "chatgpt-token-sharing";
pub const LABEL: &str = "Sign in with ChatGPT";
/// Plugin storage key of the persistent `urn:uuid:` agent host id.
pub const HOST_ID_KEY: &str = "chatgpt-agent-host-id";
pub const INSTRUCTIONS: &str = "Complete authorization in your browser. This window will close automatically.";

/// Error codes in a response body that forbid session retries (`nonRetryableSharingCodes`).
pub const NON_RETRYABLE_SHARING_CODES: &[&str] = &[
    "subscription_sharing_usage_limit_exceeded",
    "subscription_sharing_user_not_eligible",
    "subscription_sharing_unsupported_capability",
    "subscription_sharing_route_not_supported",
    "subscription_sharing_invalid_user",
    "chatpass_v2_scope_not_authorized",
    "chatpass_v2_invalid_authorization_context",
];

/// Saved client id of the active ChatGPT credential (`metadata.clientID`), if any.
pub type SavedClientId = Arc<dyn Fn() -> futures::future::BoxFuture<'static, Option<String>> + Send + Sync>;

/// A [`SavedClientId`] that never has a saved id.
pub fn no_saved_client_id() -> SavedClientId {
    Arc::new(|| Box::pin(async { None }))
}

/// `decodeMetadata` (chatgpt.ts:84-90): `{clientID: string, scopes?: string[], models?: RemoteModel[]}`.
fn metadata_client_id(metadata: Option<&serde_json::Map<String, serde_json::Value>>) -> Option<String> {
    let metadata = metadata?;
    if let Some(scopes) = metadata.get("scopes")
        && !scopes.as_array().is_some_and(|a| a.iter().all(serde_json::Value::is_string))
    {
        return None;
    }
    if let Some(models) = metadata.get("models")
        && !models.as_array().is_some_and(|a| a.iter().all(serde_json::Value::is_object))
    {
        return None;
    }
    metadata.get("clientID")?.as_str().map(str::to_string)
}

/// The client id of the `openai` integration's **active connection** (chatgpt.ts:218-237): the
/// connection `Integration.connection.active` picks (stored credentials first, then env-var
/// connections), resolved (an expiring OAuth token is refreshed; a failed refresh means none). Only
/// a ChatGPT token-sharing OAuth credential has one; an env-var or API-key connection has none.
pub fn active_connection_client_id(integrations: std::sync::Weak<crate::credentials::integration::Integrations>) -> SavedClientId {
    Arc::new(move || {
        let integrations = integrations.clone();
        Box::pin(async move {
            let integrations = integrations.upgrade()?;
            let connection = integrations.active(INTEGRATION_ID).await.ok().flatten()?;
            match integrations.resolve(&connection).await.ok().flatten()? {
                CredentialValue::Oauth { method_id, metadata, .. } if method_id == METHOD_ID => metadata_client_id(metadata.as_ref()),
                _ => None,
            }
        })
    })
}

pub struct ChatGptTokenSharing {
    pub ctx: FlowContext,
    pub storage: Arc<dyn KvStore>,
    pub saved_client_id: SavedClientId,
    pub issuer: String,
    pub jwks_url: String,
}

impl ChatGptTokenSharing {
    pub fn new(ctx: FlowContext, storage: Arc<dyn KvStore>, saved_client_id: SavedClientId) -> Self {
        Self {
            ctx,
            storage,
            saved_client_id,
            issuer: ISSUER.into(),
            jwks_url: format!("{ISSUER}/.well-known/jwks.json"),
        }
    }

    fn token_url(&self) -> String {
        format!("{}/api/accounts/oauth/token", self.issuer)
    }

    async fn host_id(&self) -> FlowResult<String> {
        let stored = self.storage.get(HOST_ID_KEY).await.map_err(|e| FlowError::new(e.to_string()))?;
        if let Some(Value::String(id)) = stored {
            return Ok(id);
        }
        let id = format!("urn:uuid:{}", uuid::Uuid::new_v4());
        self.storage.set(HOST_ID_KEY, json!(id)).await.map_err(|e| FlowError::new(e.to_string()))?;
        Ok(id)
    }
}

/// Authorize URL (`authorizeURL`), parameters in upstream order.
pub fn authorize_url(
    issuer: &str,
    redirect: &str,
    pkce: &Pkce,
    state: &str,
    nonce: &str,
    saved_id: Option<&str>,
    host_id: &str,
) -> String {
    let scope = format!("openid profile email offline_access resource.invoke {TOKEN_SHARING_SCOPE}");
    let mut pairs: Vec<(&str, &str)> = vec![("client_id", saved_id.unwrap_or(REGISTRATION_CLIENT_ID))];
    if saved_id.is_none() {
        pairs.push(("agent_name_hint", AGENT_NAME));
    }
    pairs.extend([
        ("ext_agent_host_id", host_id),
        ("response_type", "code"),
        ("redirect_uri", redirect),
        ("scope", scope.as_str()),
        ("resource", RESOURCE),
        ("state", state),
        ("nonce", nonce),
        ("code_challenge_method", "S256"),
        ("code_challenge", pkce.challenge.as_str()),
    ]);
    format!("{issuer}/api/accounts/authorize?{}", pkce::form_encode(&pairs))
}

struct Callback {
    code: String,
    client_id: Option<String>,
}

fn options() -> CallbackPageOptions<'static> {
    CallbackPageOptions::provider("ChatGPT")
}

#[async_trait]
impl OAuthMethod for ChatGptTokenSharing {
    fn integration_id(&self) -> String {
        INTEGRATION_ID.into()
    }

    fn info(&self) -> OAuthMethodInfo {
        OAuthMethodInfo::new(METHOD_ID, LABEL)
    }

    async fn authorize(&self, _answer: FormAnswer, scope: CancellationToken) -> FlowResult<AuthorizeResult> {
        let host_id = self.host_id().await?;
        let pkce = pkce::openai_pkce();
        let state = pkce::random_state();
        let nonce = pkce::random_state();
        let saved_id = (self.saved_client_id)().await;
        let (once, rx) = Once::<FlowResult<Received<Callback>>>::new();
        let expected_state = state.clone();
        let handler: Handler = Arc::new(move |request: Request| {
            let once = once.clone();
            let state = expected_state.clone();
            Box::pin(async move {
                if request.path != "/auth/callback" {
                    return Some(Response::not_found("Not found"));
                }
                let error = request.param("error_description").or_else(|| request.param("error"));
                let code = request.param("code");
                if let Some(error) = error {
                    once.complete(Err(FlowError::new(error.clone())));
                    return Some(Response::html(400, page::error(&error, &options())));
                }
                let code = match code {
                    Some(code) if !code.is_empty() && request.param("state").as_deref() == Some(state.as_str()) => code,
                    code => {
                        let message = if code.is_some_and(|c| !c.is_empty()) {
                            "Invalid OAuth state"
                        } else {
                            "Missing authorization code"
                        };
                        once.complete(Err(FlowError::new(message)));
                        return Some(Response::html(400, page::error(message, &options())));
                    }
                };
                let (responder, response) = tokio::sync::oneshot::channel();
                let callback = Callback { code, client_id: request.param("client_id") };
                if !once.complete(Ok(Received { value: callback, responder })) {
                    return Some(Response::new(409, "OAuth callback already received"));
                }
                response.await.ok()
            })
        });
        let bound = bind("127.0.0.1", 0, None).await?;
        let port = bound.port;
        bound.serve(handler, scope);
        let redirect = format!("http://127.0.0.1:{port}/auth/callback");
        let url = authorize_url(&self.issuer, &redirect, &pkce, &state, &nonce, saved_id.as_deref(), &host_id);

        let ctx = self.ctx.clone();
        let token_url = self.token_url();
        let issuer = self.issuer.clone();
        let jwks_url = self.jwks_url.clone();
        let callback = async move {
            let received = await_deferred(rx).await?;
            let Received { value, responder } = received;
            let result = async {
                // Reauthorization callbacks may omit the client ID; reuse the one this attempt started with.
                if let (Some(saved), Some(returned)) = (&saved_id, &value.client_id)
                    && saved != returned
                {
                    return Err(FlowError::new("ChatGPT returned a different client than this connection. Connect again."));
                }
                let Some(client_id) = saved_id.clone().or(value.client_id.clone()) else {
                    return Err(FlowError::new("ChatGPT sign-in did not return a client ID. Connect again."));
                };
                let tokens = exchange(&ctx, &token_url, &value.code, &client_id, &redirect, &pkce).await?;
                let scopes = str_field(&tokens, "scope");
                if !scopes.as_deref().unwrap_or("").split(' ').any(|s| s == TOKEN_SHARING_SCOPE) {
                    return Err(FlowError::new(
                        "ChatGPT sign-in finished without token sharing. Sign in again and allow token sharing, or connect OpenAI with an API key.",
                    ));
                }
                let Some(id_token) = str_field(&tokens, "id_token").filter(|t| !t.is_empty()) else {
                    return Err(FlowError::new("ChatGPT sign-in did not return an ID token."));
                };
                verify_id_token(&ctx, &id_token, &client_id, &nonce, &issuer, &jwks_url).await?;
                credential(&tokens, &client_id, None)
            }
            .await;
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

    async fn refresh(&self, value: OAuthValue) -> FlowResult<OAuthValue> {
        refresh(&self.ctx, &self.token_url(), &value).await
    }
}

fn form_headers(ctx: &FlowContext, request: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
    request
        .header("Content-Type", "application/x-www-form-urlencoded")
        .header("User-Agent", ctx.user_agent.as_str())
}

async fn post_form(ctx: &FlowContext, url: &str, pairs: &[(&str, &str)]) -> FlowResult<Value> {
    let body = pkce::form_encode(pairs);
    crate::credentials::http::request_json(form_headers(ctx, ctx.http.post(url)).body(body)).await
}

async fn exchange(
    ctx: &FlowContext,
    token_url: &str,
    code: &str,
    client_id: &str,
    redirect: &str,
    pkce: &Pkce,
) -> FlowResult<Value> {
    post_form(
        ctx,
        token_url,
        &[
            ("grant_type", "authorization_code"),
            ("client_id", client_id),
            ("code", code),
            ("code_verifier", &pkce.verifier),
            ("redirect_uri", redirect),
            ("resource", RESOURCE),
        ],
    )
    .await
}

/// `refresh(value)`: requires `metadata.clientID`.
pub async fn refresh(ctx: &FlowContext, token_url: &str, value: &OAuthValue) -> FlowResult<OAuthValue> {
    let metadata = value.metadata.as_ref();
    let client_id = metadata.and_then(|m| m.get("clientID")).and_then(Value::as_str);
    let scopes = metadata.and_then(|m| m.get("scopes"));
    let scopes_ok = match scopes {
        None => true,
        Some(Value::Array(items)) => items.iter().all(Value::is_string),
        Some(_) => false,
    };
    let (Some(client_id), true) = (client_id, scopes_ok) else {
        return Err(FlowError::new("This ChatGPT connection has no registered client ID. Connect again."));
    };
    let tokens = post_form(
        ctx,
        token_url,
        &[
            ("grant_type", "refresh_token"),
            ("client_id", client_id),
            ("refresh_token", &value.refresh),
            ("resource", RESOURCE),
        ],
    )
    .await?;
    let scopes: Option<Vec<String>> =
        scopes.and_then(Value::as_array).map(|a| a.iter().filter_map(|s| s.as_str().map(String::from)).collect());
    credential(&tokens, client_id, scopes)
}

/// Builds the stored credential: `{access, refresh, expires: now + (expires_in ?? 3600) s, metadata: {clientID, scopes}}`.
pub fn credential(tokens: &Value, client_id: &str, scopes: Option<Vec<String>>) -> FlowResult<OAuthValue> {
    let access = str_field(tokens, "access_token").ok_or_else(|| FlowError::new("Invalid token response"))?;
    let refresh = str_field(tokens, "refresh_token").ok_or_else(|| FlowError::new("Invalid token response"))?;
    let expires_in = tokens.get("expires_in").and_then(Value::as_f64).unwrap_or(3600.0);
    let scopes: Vec<String> = match str_field(tokens, "scope") {
        Some(scope) => scope.split(' ').filter(|s| !s.is_empty()).map(String::from).collect(),
        None => scopes.unwrap_or_default(),
    };
    let mut metadata = Metadata::new();
    metadata.insert("clientID".into(), json!(client_id));
    metadata.insert("scopes".into(), json!(scopes));
    Ok(OAuthValue {
        method_id: METHOD_ID.into(),
        refresh,
        access,
        expires: (now_ms() as f64 + expires_in * 1000.0).max(0.0) as u64,
        metadata: Some(metadata),
    })
}

/// Verifies the ID token against the issuer JWKS (RS256, `iss`, `aud`, required `exp`/`nonce`/`sub`, nonce
/// match). Any failure → `"ChatGPT sign-in returned an invalid ID token."`.
pub async fn verify_id_token(
    ctx: &FlowContext,
    token: &str,
    client_id: &str,
    nonce: &str,
    issuer: &str,
    jwks_url: &str,
) -> FlowResult<()> {
    let invalid = || FlowError::new("ChatGPT sign-in returned an invalid ID token.");
    let response = ctx.http.get(jwks_url).send().await.map_err(|_| invalid())?;
    if !response.status().is_success() {
        return Err(invalid());
    }
    let jwks: Value = response.json().await.map_err(|_| invalid())?;
    verify_id_token_with_jwks(token, &jwks, client_id, nonce, issuer, now_ms() / 1000).map(|_| ()).map_err(|_| invalid())
}

/// Offline part of [`verify_id_token`]; `now` in epoch seconds. Returns the failure reason.
pub fn verify_id_token_with_jwks(
    token: &str,
    jwks: &Value,
    client_id: &str,
    nonce: &str,
    issuer: &str,
    now: u64,
) -> Result<Value, String> {
    let parts: Vec<&str> = token.split('.').collect();
    if parts.len() != 3 {
        return Err("malformed token".into());
    }
    let header: Value = pkce::base64url_decode(parts[0])
        .and_then(|b| serde_json::from_slice(&b).ok())
        .ok_or("invalid header")?;
    if header.get("alg").and_then(Value::as_str) != Some("RS256") {
        return Err("unexpected alg".into());
    }
    let kid = header.get("kid").and_then(Value::as_str);
    let signature = pkce::base64url_decode(parts[2]).ok_or("invalid signature encoding")?;
    let message = format!("{}.{}", parts[0], parts[1]);
    let keys = jwks.get("keys").and_then(Value::as_array).ok_or("invalid JWKS")?;
    let candidates: Vec<&Value> = keys
        .iter()
        .filter(|k| k.get("kty").and_then(Value::as_str) == Some("RSA"))
        .filter(|k| k.get("alg").and_then(Value::as_str).is_none_or(|a| a == "RS256"))
        .filter(|k| k.get("use").and_then(Value::as_str).is_none_or(|u| u == "sig"))
        .filter(|k| kid.is_none_or(|kid| k.get("kid").and_then(Value::as_str) == Some(kid)))
        .collect();
    if candidates.is_empty() {
        return Err("no matching key".into());
    }
    let verified = candidates.iter().any(|key| {
        let (Some(n), Some(e)) = (
            key.get("n").and_then(Value::as_str).and_then(pkce::base64url_decode),
            key.get("e").and_then(Value::as_str).and_then(pkce::base64url_decode),
        ) else {
            return false;
        };
        ring::signature::RsaPublicKeyComponents { n: &n, e: &e }
            .verify(&ring::signature::RSA_PKCS1_2048_8192_SHA256, message.as_bytes(), &signature)
            .is_ok()
    });
    if !verified {
        return Err("signature verification failed".into());
    }
    let claims = pkce::jwt_claims(token).ok_or("invalid claims")?;
    if claims.get("iss").and_then(Value::as_str) != Some(issuer) {
        return Err("unexpected iss".into());
    }
    let audience_ok = match claims.get("aud") {
        Some(Value::String(aud)) => aud == client_id,
        Some(Value::Array(items)) => items.iter().any(|a| a.as_str() == Some(client_id)),
        _ => false,
    };
    if !audience_ok {
        return Err("unexpected aud".into());
    }
    let exp = claims.get("exp").and_then(Value::as_f64).ok_or("missing exp")?;
    if exp <= now as f64 {
        return Err("expired".into());
    }
    if let Some(nbf) = claims.get("nbf") {
        if nbf.as_f64().is_none_or(|nbf| nbf > now as f64) {
            return Err("not yet valid".into());
        }
    }
    let sub = claims.get("sub").and_then(Value::as_str).ok_or("missing sub")?;
    if sub.trim().is_empty() {
        return Err("ID token subject is missing".into());
    }
    if claims.get("nonce").and_then(Value::as_str) != Some(nonce) {
        return Err("ID token nonce does not match this sign-in attempt".into());
    }
    Ok(claims)
}

/// Whether a session retry must be forbidden for a ChatGPT token-sharing failure body.
pub fn forbids_retry(body: Option<&str>) -> bool {
    body.is_some_and(|body| NON_RETRYABLE_SHARING_CODES.iter().any(|code| body.contains(code)))
}

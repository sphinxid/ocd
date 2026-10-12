//! GitLab PKCE login (`packages/core/src/plugin/provider/gitlab.ts`, spec §6.4.6).

use super::{Received, await_deferred, bind};
use crate::credentials::callback::{Handler, Once, Request, Response};
use crate::credentials::form::FormAnswer;
use crate::credentials::http::{FlowContext, str_field};
use crate::credentials::integration::{
    AuthorizeResult, BoxFuture, FlowError, FlowResult, OAuthMethod, OAuthMethodInfo,
};
use crate::credentials::page::{self, CallbackPageOptions};
use crate::credentials::pkce;
use crate::credentials::{EnvSource, Metadata, OAuthValue, now_ms};
use async_trait::async_trait;
use futures::FutureExt;
use futures::future::Shared;
use once_cell::sync::Lazy;
use parking_lot::Mutex;
use serde_json::{Value, json};
use std::collections::HashMap;
use std::sync::Arc;
use tokio_util::sync::CancellationToken;

pub const INTEGRATION_ID: &str = "gitlab";
pub const METHOD_ID: &str = "pkce";
/// Instance-owned GitLab OAuth application for OpenCode (gitlab.com only).
pub const BUNDLED_CLIENT_ID: &str = "fd180700a8f9c5d5557aca231632dd0611a1135a3bb510a741d5a988a3394fa7";
/// Application used by every opencode-gitlab-auth release (refreshes old credentials).
pub const LEGACY_CLIENT_ID: &str = "1d89f9fdb23ee96d4e603201f6861dab6e143c5c3c00469a018a2d94bdc03d4e";
pub const GITLAB_COM_HOST: &str = "gitlab.com";
pub const OAUTH_SCOPE: &str = "api";
pub const CALLBACK_HOST: &str = "127.0.0.1";
pub const CALLBACK_PORT: u16 = 8080;
pub const REFRESH_RETENTION_MS: u64 = 60_000;
pub const INSTRUCTIONS: &str = "Complete authorization in your browser. This window will close automatically.";

type SharedRefresh = Shared<BoxFuture<Result<OAuthValue, FlowError>>>;

/// Refreshes of the same refresh token share one exchange; successes are retained 60 s.
static REFRESHING: Lazy<Mutex<HashMap<String, (SharedRefresh, u64)>>> = Lazy::new(|| Mutex::new(HashMap::new()));

pub struct GitLabPkce {
    pub ctx: FlowContext,
    pub env: EnvSource,
    pub callback_port: u16,
}

impl GitLabPkce {
    pub fn new(ctx: FlowContext, env: EnvSource) -> Self {
        Self { ctx, env, callback_port: CALLBACK_PORT }
    }

    pub fn redirect_uri(&self) -> String {
        format!("http://{CALLBACK_HOST}:{}/callback", self.callback_port)
    }

    /// `GITLAB_INSTANCE_URL ?? "https://gitlab.com"`.
    pub fn default_instance_url(&self) -> String {
        (self.env)("GITLAB_INSTANCE_URL").unwrap_or_else(|| "https://gitlab.com".into())
    }

    fn override_client_id(&self) -> Option<String> {
        (self.env)("GITLAB_OAUTH_CLIENT_ID").map(|v| v.trim().to_string()).filter(|v| !v.is_empty())
    }

    /// `metadata.instanceUrl` or the default instance.
    pub fn credential_instance_url(&self, metadata: Option<&Metadata>) -> String {
        metadata
            .and_then(|m| m.get("instanceUrl"))
            .and_then(Value::as_str)
            .filter(|v| !v.is_empty())
            .map(String::from)
            .unwrap_or_else(|| self.default_instance_url())
    }

    pub fn form(&self) -> Vec<Value> {
        let default = self.default_instance_url();
        vec![json!({
            "type": "string",
            "key": "instanceUrl",
            "title": "GitLab instance URL",
            "description": "Leave the default to use gitlab.com, or enter your self-managed GitLab URL.",
            "placeholder": default,
            "default": default,
            "pattern": "^https?://\\S+$"
        })]
    }
}

fn describe_client(client_id: &str) -> String {
    let source = if client_id == BUNDLED_CLIENT_ID {
        "bundled default"
    } else if client_id == LEGACY_CLIENT_ID {
        "opencode-gitlab-auth application"
    } else {
        "from GITLAB_OAUTH_CLIENT_ID"
    };
    let prefix: String = client_id.chars().take(12).collect();
    format!("client_id used: {prefix}... ({source})")
}

/// `describeGrantFailure`.
pub fn describe_grant_failure(detail: &str, client_id: &str, redirect_uri: &str) -> String {
    let mut hints = vec![
        "the authorization code was already used, or came from an older login attempt. Codes are single-use and bound to one PKCE verifier, so a stale browser tab or a reloaded callback page fails here. Start a completely fresh login.".to_string(),
        format!("redirect_uri sent: {redirect_uri} — the application must register this exactly"),
        describe_client(client_id),
    ];
    if client_id != BUNDLED_CLIENT_ID {
        hints.push(
            "confirm that application registers the redirect URI above, grants the `api` scope, and is NOT marked \"Confidential\" (PKCE requires a public client). Unset GITLAB_OAUTH_CLIENT_ID to fall back to the bundled application.".to_string(),
        );
    }
    format!("{detail}\n\nLikely causes:\n- {}", hints.join("\n- "))
}

/// `describeRefreshFailure`.
pub fn describe_refresh_failure(detail: &str, client_id: &str) -> String {
    format!(
        "{detail}\n\nThe GitLab refresh token was revoked, expired, or issued to a different OAuth application ({}). Sign in to GitLab again.",
        describe_client(client_id)
    )
}

/// `callbackError`.
pub fn callback_error(request: &Request, state: &str) -> Option<String> {
    if request.param("state").as_deref() != Some(state) {
        return Some("Invalid OAuth state".into());
    }
    if request.has_param("error") {
        return Some(
            request
                .param("error_description")
                .filter(|v| !v.is_empty())
                .or_else(|| request.param("error").filter(|v| !v.is_empty()))
                .unwrap_or_else(|| "Authorization denied".into()),
        );
    }
    if request.param("code").is_some_and(|c| !c.trim().is_empty()) {
        None
    } else {
        Some("Missing authorization code".into())
    }
}

enum Grant {
    Authorization,
    Refresh,
}

async fn exchange(
    ctx: &FlowContext,
    instance_url: &str,
    client_id: &str,
    body: &[(&str, &str)],
    grant: Grant,
    redirect_uri: &str,
) -> FlowResult<(String, String, Option<f64>)> {
    let mut pairs: Vec<(&str, &str)> = body.to_vec();
    pairs.push(("client_id", client_id));
    let response = ctx
        .http
        .post(format!("{instance_url}/oauth/token"))
        .header("Content-Type", "application/x-www-form-urlencoded")
        .body(pkce::form_encode(&pairs))
        .send()
        .await
        .map_err(|_| FlowError::new("GitLab token exchange request failed"))?;
    let status = response.status().as_u16();
    if !(200..300).contains(&status) {
        let text = response.text().await.unwrap_or_default();
        let parsed: Option<Value> = serde_json::from_str(&text).ok();
        let field = |k: &str| {
            parsed.as_ref().and_then(|p| p.get(k)).and_then(Value::as_str).filter(|v| !v.is_empty()).map(String::from)
        };
        let error = field("error");
        let detail = field("error_description").or(error.clone()).unwrap_or_else(|| format!("HTTP {status}"));
        let rejected = matches!(error.as_deref(), Some("invalid_grant") | Some("invalid_client"));
        return Err(FlowError::new(if rejected {
            match grant {
                Grant::Authorization => describe_grant_failure(&detail, client_id, redirect_uri),
                Grant::Refresh => describe_refresh_failure(&detail, client_id),
            }
        } else {
            detail
        }));
    }
    let invalid = || FlowError::new("Invalid GitLab token response");
    let body: Value = response.json().await.map_err(|_| invalid())?;
    let access = str_field(&body, "access_token").filter(|v| !v.is_empty()).ok_or_else(invalid)?;
    let refresh = str_field(&body, "refresh_token").filter(|v| !v.is_empty()).ok_or_else(invalid)?;
    let expires_in = match body.get("expires_in") {
        None => None,
        Some(v) => Some(v.as_f64().ok_or_else(invalid)?),
    };
    Ok((access, refresh, expires_in))
}

fn credential(instance_url: &str, client_id: &str, tokens: (String, String, Option<f64>)) -> OAuthValue {
    let (access, refresh, expires_in) = tokens;
    let mut metadata = Metadata::new();
    metadata.insert("instanceUrl".into(), json!(instance_url));
    metadata.insert("clientID".into(), json!(client_id));
    OAuthValue {
        method_id: METHOD_ID.into(),
        access,
        refresh,
        expires: (now_ms() as f64 + expires_in.unwrap_or(7200.0) * 1000.0).max(0.0) as u64,
        metadata: Some(metadata),
    }
}

fn options() -> CallbackPageOptions<'static> {
    CallbackPageOptions::provider("GitLab")
}

/// `new URL(x)` → `origin + pathname` without trailing slashes, and the host.
fn instance(input: &str) -> FlowResult<(String, String)> {
    let url = url::Url::parse(input).map_err(|_| FlowError::new("Invalid URL"))?;
    let host = match (url.host_str(), url.port()) {
        (Some(h), Some(p)) => format!("{h}:{p}"),
        (Some(h), None) => h.to_string(),
        _ => String::new(),
    };
    Ok((format!("{}{}", url.origin().ascii_serialization(), url.path().trim_end_matches('/')), host))
}

#[async_trait]
impl OAuthMethod for GitLabPkce {
    fn integration_id(&self) -> String {
        INTEGRATION_ID.into()
    }

    fn info(&self) -> OAuthMethodInfo {
        OAuthMethodInfo::new(METHOD_ID, "Login with GitLab (OAuth)").with_form(self.form())
    }

    async fn authorize(&self, answer: FormAnswer, scope: CancellationToken) -> FlowResult<AuthorizeResult> {
        // Captured once so the authorize request and the token exchange agree.
        let override_id = self.override_client_id();
        let client_id = override_id.clone().unwrap_or_else(|| BUNDLED_CLIENT_ID.into());
        let answered = answer.get("instanceUrl").and_then(Value::as_str).map(str::trim).unwrap_or("");
        let input = if answered.is_empty() { self.default_instance_url() } else { answered.to_string() };
        let (instance_url, host) = instance(&input)?;
        let redirect_uri = self.redirect_uri();
        if override_id.is_none() && host != GITLAB_COM_HOST {
            return Err(FlowError::new(format!(
                "The bundled GitLab OAuth application only exists on {GITLAB_COM_HOST}. To sign in to {host}, register an OAuth application there with redirect URI {redirect_uri} and the `{OAUTH_SCOPE}` scope (not marked \"Confidential\"), then set GITLAB_OAUTH_CLIENT_ID to its application ID."
            )));
        }
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
                let code = request.param("code").unwrap_or_default();
                if !once.complete(Ok(Received { value: code, responder })) {
                    return Some(Response::new(409, "OAuth callback already received"));
                }
                response.await.ok()
            })
        });
        let bound = bind(
            CALLBACK_HOST,
            self.callback_port,
            Some(format!(
                "GitLab login needs local port {}, but it is already in use. Stop the process using that port and try again.",
                self.callback_port
            )),
        )
        .await?;
        bound.serve(handler, scope);
        let url = format!(
            "{instance_url}/oauth/authorize?{}",
            pkce::form_encode(&[
                ("client_id", client_id.as_str()),
                ("redirect_uri", redirect_uri.as_str()),
                ("response_type", "code"),
                ("state", state.as_str()),
                ("scope", OAUTH_SCOPE),
                ("code_challenge", pkce.challenge.as_str()),
                ("code_challenge_method", "S256"),
            ])
        );
        let ctx = self.ctx.clone();
        let callback = async move {
            let Received { value: code, responder } = await_deferred(rx).await?;
            let result = exchange(
                &ctx,
                &instance_url,
                &client_id,
                &[
                    ("grant_type", "authorization_code"),
                    ("code", code.as_str()),
                    ("redirect_uri", redirect_uri.as_str()),
                    ("code_verifier", pkce.verifier.as_str()),
                ],
                Grant::Authorization,
                &redirect_uri,
            )
            .await
            .map(|tokens| credential(&instance_url, &client_id, tokens));
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
        let instance_url = self.credential_instance_url(value.metadata.as_ref());
        // Refresh with the application that issued the token.
        let client_id = value
            .meta_str("clientID")
            .filter(|v| !v.is_empty())
            .map(String::from)
            .or_else(|| self.override_client_id())
            .unwrap_or_else(|| LEGACY_CLIENT_ID.into());
        let key = format!("{instance_url}\0{}", value.refresh);
        let ctx = self.ctx.clone();
        let redirect_uri = self.redirect_uri();
        let refresh_token = value.refresh.clone();
        let effect: BoxFuture<FlowResult<OAuthValue>> = Box::pin(async move {
            exchange(
                &ctx,
                &instance_url,
                &client_id,
                &[
                    ("grant_type", "refresh_token"),
                    ("refresh_token", refresh_token.as_str()),
                    ("redirect_uri", redirect_uri.as_str()),
                ],
                Grant::Refresh,
                &redirect_uri,
            )
            .await
            .map(|tokens| credential(&instance_url, &client_id, tokens))
        });
        single_flight(key, effect).await
    }

    fn label(&self, value: &OAuthValue) -> Option<String> {
        let url = self.credential_instance_url(value.metadata.as_ref());
        instance(&url).ok().map(|(_, host)| host)
    }
}

/// Joins refreshes of the same key onto one exchange.
async fn single_flight(key: String, effect: BoxFuture<FlowResult<OAuthValue>>) -> FlowResult<OAuthValue> {
    let (shared, owner) = {
        let now = now_ms();
        let mut map = REFRESHING.lock();
        map.retain(|_, (_, until)| *until >= now);
        match map.get(&key) {
            Some((existing, _)) => (existing.clone(), false),
            None => {
                let shared = effect.shared();
                map.insert(key.clone(), (shared.clone(), u64::MAX));
                (shared, true)
            }
        }
    };
    let result = shared.clone().await;
    if owner {
        let mut map = REFRESHING.lock();
        match &result {
            Ok(_) => {
                map.insert(key, (shared, now_ms() + REFRESH_RETENTION_MS));
            }
            Err(_) => {
                map.remove(&key);
            }
        }
    }
    result
}

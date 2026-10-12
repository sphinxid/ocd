//! Snowflake Cortex external-browser login (`packages/core/src/plugin/provider/snowflake-cortex.ts`,
//! spec §6.4.7).

use super::{await_deferred, bind};
use crate::credentials::callback::{Once, Response, sync_handler};
use crate::credentials::form::FormAnswer;
use crate::credentials::http::{FlowContext, str_field, transport};
use crate::credentials::integration::{AuthorizeResult, FlowError, FlowResult, OAuthMethod, OAuthMethodInfo};
use crate::credentials::page::{self, CallbackPageOptions};
use crate::credentials::pkce;
use crate::credentials::{EnvSource, Metadata, OAuthValue, now_ms};
use async_trait::async_trait;
use base64::Engine;
use once_cell::sync::Lazy;
use regex::Regex;
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

pub const INTEGRATION_ID: &str = "snowflake-cortex";
pub const METHOD_ID: &str = "browser";
pub const CLIENT_ID: &str = "LOCAL_APPLICATION";
/// Env-method variables (`SNOWFLAKE_ACCOUNT` configures the endpoint; it is not a token).
pub const ENV_NAMES: &[&str] = &["SNOWFLAKE_CORTEX_TOKEN", "SNOWFLAKE_CORTEX_PAT"];

pub struct SnowflakeBrowser {
    pub ctx: FlowContext,
    pub env: EnvSource,
    /// Test override for `https://<account>.snowflakecomputing.com`.
    pub issuer_override: Option<String>,
}

impl SnowflakeBrowser {
    pub fn new(ctx: FlowContext, env: EnvSource) -> Self {
        Self { ctx, env, issuer_override: None }
    }

    fn issuer(&self, account: &str) -> String {
        self.issuer_override.clone().unwrap_or_else(|| issuer(account))
    }

    /// `SNOWFLAKE_CORTEX_TOKEN ?? SNOWFLAKE_CORTEX_PAT`.
    pub fn env_token(&self) -> Option<String> {
        (self.env)("SNOWFLAKE_CORTEX_TOKEN").or_else(|| (self.env)("SNOWFLAKE_CORTEX_PAT"))
    }

    async fn token(&self, account: &str, form: &[(&str, &str)]) -> FlowResult<OAuthValue> {
        if account.is_empty() {
            return Err(FlowError::new("Snowflake account is required"));
        }
        let mut pairs = form.to_vec();
        pairs.push(("client_id", CLIENT_ID));
        let basic = base64::engine::general_purpose::STANDARD.encode(format!("{CLIENT_ID}:{CLIENT_ID}"));
        let response = self
            .ctx
            .http
            .post(format!("{}/oauth/token-request", self.issuer(account)))
            .header("Accept", "application/json")
            .header("User-Agent", self.ctx.user_agent.as_str())
            .header("Authorization", format!("Basic {basic}"))
            .header("Content-Type", "application/x-www-form-urlencoded")
            .body(pkce::form_encode(&pairs))
            .send()
            .await
            .map_err(transport)?;
        let status = response.status().as_u16();
        let text = response.text().await.map_err(transport)?;
        if !(200..300).contains(&status) {
            return Err(FlowError::new(format!("Snowflake token request failed ({status}): {text}")));
        }
        let invalid = || FlowError::new("Invalid Snowflake token response");
        let tokens: Value = serde_json::from_str(&text).map_err(|_| invalid())?;
        let access = str_field(&tokens, "access_token").filter(|a| !a.is_empty()).ok_or_else(invalid)?;
        let form_refresh = form.iter().find(|(k, _)| *k == "refresh_token").map(|(_, v)| v.to_string());
        let refresh = str_field(&tokens, "refresh_token")
            .filter(|r| !r.is_empty())
            .or(form_refresh.filter(|r| !r.is_empty()))
            .ok_or_else(|| FlowError::new("Snowflake token response did not include refresh_token"))?;
        let expires_in = tokens.get("expires_in").and_then(Value::as_f64).unwrap_or(600.0);
        let mut metadata = Metadata::new();
        metadata.insert("account".into(), json!(account));
        Ok(OAuthValue {
            method_id: METHOD_ID.into(),
            access,
            refresh,
            expires: (now_ms() as f64 + expires_in * 1000.0).max(0.0) as u64,
            metadata: Some(metadata),
        })
    }
}

/// Account form shared by the key and OAuth methods.
pub fn account_form() -> Vec<Value> {
    vec![json!({ "type": "string", "key": "account", "title": "Snowflake account", "placeholder": "myorg-myaccount", "required": true })]
}

pub fn oauth_form() -> Vec<Value> {
    let mut form = account_form();
    form.push(json!({ "type": "string", "key": "role", "title": "Snowflake role (optional)" }));
    form
}

/// `normalizeAccount`: trims, strips scheme, trailing slashes and `.snowflakecomputing.com`.
pub fn normalize_account(value: Option<&Value>) -> String {
    static SCHEME: Lazy<Regex> = Lazy::new(|| Regex::new(r"(?i)^https?://").expect("valid regex"));
    static SUFFIX: Lazy<Regex> = Lazy::new(|| Regex::new(r"(?i)\.snowflakecomputing\.com$").expect("valid regex"));
    let Some(Value::String(text)) = value else { return String::new() };
    let text = SCHEME.replace(text.trim(), "").to_string();
    let text = text.trim_end_matches('/');
    SUFFIX.replace(text, "").to_string()
}

pub fn issuer(account: &str) -> String {
    format!("https://{account}.snowflakecomputing.com")
}

/// `endpoint(baseURL, account)`: substitutes `${SNOWFLAKE_ACCOUNT}` in the configured or default base URL.
pub fn endpoint(value: Option<&str>, account: &str) -> String {
    let base = value
        .map(String::from)
        .unwrap_or_else(|| format!("{}/api/v2/cortex/v1", issuer("${SNOWFLAKE_ACCOUNT}")));
    if account.is_empty() { base } else { base.replace("${SNOWFLAKE_ACCOUNT}", account) }
}

/// OAuth scope for an optional role.
pub fn scope_for_role(role: &str) -> String {
    static PLAIN: Lazy<Regex> = Lazy::new(|| Regex::new(r"^[-_A-Za-z0-9]+$").expect("valid regex"));
    if role.is_empty() {
        "refresh_token".into()
    } else if PLAIN.is_match(role) {
        format!("refresh_token session:role:{role}")
    } else {
        format!("refresh_token session:role-encoded:{}", pkce::encode_uri_component(role))
    }
}

fn options() -> CallbackPageOptions<'static> {
    CallbackPageOptions::provider("Snowflake")
}

#[async_trait]
impl OAuthMethod for SnowflakeBrowser {
    fn integration_id(&self) -> String {
        INTEGRATION_ID.into()
    }

    fn info(&self) -> OAuthMethodInfo {
        OAuthMethodInfo::new(METHOD_ID, "Login with Snowflake (External Browser)").with_form(oauth_form())
    }

    async fn authorize(&self, answer: FormAnswer, scope: CancellationToken) -> FlowResult<AuthorizeResult> {
        let account = normalize_account(answer.get("account"));
        if account.is_empty() {
            return Err(FlowError::new("Snowflake account is required"));
        }
        let role = answer.get("role").and_then(Value::as_str).map(str::trim).unwrap_or("").to_string();
        let pkce = pkce::base64_pkce(48);
        let state = pkce::random_state();
        let (once, rx) = Once::<FlowResult<String>>::new();
        let expected = state.clone();
        let handler = sync_handler(move |request| {
            if request.path != "/" {
                return Response::new(404, "");
            }
            let value = request.param("code");
            let error = if request.param("state").as_deref() != Some(expected.as_str()) {
                Some("Invalid OAuth state".to_string())
            } else {
                request
                    .param("error_description")
                    .filter(|v| !v.is_empty())
                    .or_else(|| request.param("error").filter(|v| !v.is_empty()))
                    .or_else(|| value.as_deref().is_none_or(str::is_empty).then(|| "Missing authorization code".into()))
            };
            match error {
                Some(error) => {
                    once.complete(Err(FlowError::new(error.clone())));
                    Response::html(400, page::error(&error, &options()))
                }
                None => {
                    once.complete(Ok(value.unwrap_or_default()));
                    Response::html(200, page::success(&options()))
                }
            }
        });
        let bound = bind("127.0.0.1", 0, None).await?;
        let port = bound.port;
        bound.serve(handler, scope);
        let redirect = format!("http://127.0.0.1:{port}/");
        let scope_param = scope_for_role(&role);
        let url = format!(
            "{}/oauth/authorize?{}",
            self.issuer(&account),
            pkce::form_encode(&[
                ("client_id", CLIENT_ID),
                ("response_type", "code"),
                ("redirect_uri", redirect.as_str()),
                ("state", state.as_str()),
                ("scope", scope_param.as_str()),
                ("code_challenge", pkce.challenge.as_str()),
                ("code_challenge_method", "S256"),
            ])
        );
        let this = SnowflakeBrowser {
            ctx: self.ctx.clone(),
            env: self.env.clone(),
            issuer_override: self.issuer_override.clone(),
        };
        let callback = async move {
            let code = await_deferred(rx).await?;
            this.token(
                &account,
                &[
                    ("grant_type", "authorization_code"),
                    ("code", code.as_str()),
                    ("redirect_uri", redirect.as_str()),
                    ("code_verifier", pkce.verifier.as_str()),
                ],
            )
            .await
        };
        Ok(AuthorizeResult::auto(url, "Complete Snowflake sign-in in your browser.", callback))
    }

    fn can_refresh(&self) -> bool {
        true
    }

    /// An environment-token override must work even when the saved OAuth login has expired.
    async fn refresh(&self, value: OAuthValue) -> FlowResult<OAuthValue> {
        if self.env_token().is_some() {
            return Ok(value);
        }
        let account = normalize_account(value.metadata.as_ref().and_then(|m| m.get("account")));
        self.token(&account, &[("grant_type", "refresh_token"), ("refresh_token", value.refresh.as_str())]).await
    }

    fn label(&self, value: &OAuthValue) -> Option<String> {
        Some(normalize_account(value.metadata.as_ref().and_then(|m| m.get("account"))))
    }
}

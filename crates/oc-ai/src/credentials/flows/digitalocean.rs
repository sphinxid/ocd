//! DigitalOcean implicit grant (`packages/core/src/plugin/provider/digitalocean.ts`, spec §6.4.8).

use super::{await_deferred, bind, parse_int};
use crate::credentials::callback::{Once, Response, sync_handler};
use crate::credentials::form::FormAnswer;
use crate::credentials::integration::{AuthorizeResult, FlowError, FlowResult, OAuthMethod, OAuthMethodInfo};
use crate::credentials::page;
use crate::credentials::pkce;
use crate::credentials::{OAuthValue, now_ms};
use async_trait::async_trait;
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

pub const INTEGRATION_ID: &str = "digitalocean";
pub const METHOD_ID: &str = "browser";
pub const CLIENT_ID: &str = "b1a6c5158156caac821fd1b30253ca8acb52454a48fa744420e41889cb589f82";
pub const AUTHORIZE_ENDPOINT: &str = "https://cloud.digitalocean.com/v1/oauth/authorize";
pub const ROUTERS_ENDPOINT: &str = "https://api.digitalocean.com/v2/gen-ai/models/routers";
pub const CALLBACK_PORT: u16 = 1456;
pub const CALLBACK_PATH: &str = "/auth/callback";
pub const TOKEN_PATH: &str = "/auth/token";
const DEFAULT_EXPIRES_IN: u64 = 60 * 60 * 24 * 30;
pub const INSTRUCTIONS: &str = "Complete authorization in your browser. This window will close automatically.";

pub struct DigitalOceanImplicit {
    pub callback_port: u16,
}

impl Default for DigitalOceanImplicit {
    fn default() -> Self {
        Self { callback_port: CALLBACK_PORT }
    }
}

/// Decodes the bootstrap page's POST body; every field must be an optional string.
fn decode_callback(body: &[u8]) -> Option<serde_json::Map<String, Value>> {
    let value: Value = serde_json::from_slice(body).ok()?;
    let object = value.as_object()?;
    for key in ["access_token", "expires_in", "state", "error", "error_description"] {
        if object.get(key).is_some_and(|v| !v.is_string()) {
            return None;
        }
    }
    Some(object.clone())
}

#[async_trait]
impl OAuthMethod for DigitalOceanImplicit {
    fn integration_id(&self) -> String {
        INTEGRATION_ID.into()
    }

    fn info(&self) -> OAuthMethodInfo {
        OAuthMethodInfo::new(METHOD_ID, "Login with DigitalOcean")
    }

    async fn authorize(&self, _answer: FormAnswer, scope: CancellationToken) -> FlowResult<AuthorizeResult> {
        let state = pkce::random_state();
        let (once, rx) = Once::<FlowResult<(String, u64)>>::new();
        let expected = state.clone();
        let handler = sync_handler(move |request| {
            if request.method == "GET" && request.path == CALLBACK_PATH {
                return Response::html(200, page::bootstrap(TOKEN_PATH, Some("DigitalOcean")));
            }
            if request.method == "POST" && request.path == TOKEN_PATH {
                let body = decode_callback(&request.body);
                let field = |k: &str| {
                    body.as_ref().and_then(|b| b.get(k)).and_then(Value::as_str).map(String::from)
                };
                let error = field("error_description").filter(|v| !v.is_empty()).or_else(|| field("error").filter(|v| !v.is_empty()));
                let access = field("access_token").filter(|v| !v.is_empty());
                let state_ok = field("state").as_deref() == Some(expected.as_str());
                match (error, access) {
                    (None, Some(access)) if state_ok => {
                        let expires = field("expires_in").as_deref().map_or(Some(0), parse_int).unwrap_or(0);
                        let expires_in = if expires > 0 { expires as u64 } else { DEFAULT_EXPIRES_IN };
                        once.complete(Ok((access, expires_in)));
                        Response::json(200, json!({ "ok": true }).to_string())
                    }
                    (error, access) => {
                        let message = error.unwrap_or_else(|| {
                            if access.is_some() { "Invalid OAuth state".into() } else { "Missing access token".into() }
                        });
                        once.complete(Err(FlowError::new(message.clone())));
                        Response::json(400, json!({ "error": message }).to_string())
                    }
                }
            } else {
                Response::not_found("Not found")
            }
        });
        let bound = bind(
            "localhost",
            self.callback_port,
            Some(format!(
                "DigitalOcean login needs local port {}, but it is already in use. Stop the process using that port and try again.",
                self.callback_port
            )),
        )
        .await?;
        let port = bound.port;
        bound.serve(handler, scope);
        let redirect = format!("http://localhost:{port}{CALLBACK_PATH}");
        let url = format!(
            "{AUTHORIZE_ENDPOINT}?{}",
            pkce::form_encode(&[
                ("response_type", "token"),
                ("client_id", CLIENT_ID),
                ("redirect_uri", redirect.as_str()),
                ("scope", "genai:read inference:query"),
                ("state", state.as_str()),
            ])
        );
        let callback = async move {
            let (access, expires_in) = await_deferred(rx).await?;
            Ok(OAuthValue {
                method_id: METHOD_ID.into(),
                access,
                refresh: String::new(),
                expires: now_ms() + expires_in * 1000,
                metadata: None,
            })
        };
        Ok(AuthorizeResult::auto(url, INSTRUCTIONS, callback))
    }
}

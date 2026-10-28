//! IAM Identity Center: `fromSSO` (`@aws-sdk/credential-provider-sso`) for `sso_session` and legacy
//! profiles, the `fromSso` token provider (`@aws-sdk/token-providers`: the `~/.aws/sso/cache`
//! token, refreshed through SSO-OIDC `CreateToken` inside the 5-minute window, at most every 30 s),
//! and SSO `GetRoleCredentials`.

use super::chain::{Identity, ProviderError, is_sso};
use super::ini;
use super::service::{self, Service};
use crate::protocols::cloud_env::Env;
use indexmap::IndexMap;
use serde_json::Value;
use std::path::PathBuf;
use std::time::{Duration, Instant};

const REFRESH: &str = "To refresh this SSO session run aws sso login with the corresponding profile.";
const TOKEN_REFRESH: &str = "To refresh this SSO session run 'aws sso login' with the corresponding profile.";
const EXPIRE_WINDOW_MS: f64 = 5.0 * 60.0 * 1000.0;

fn now_ms() -> f64 {
    chrono::Utc::now().timestamp_millis() as f64
}

/// `getSSOTokenFilepath`: `~/.aws/sso/cache/<sha1(id)>.json`.
pub fn token_path(env: &Env, id: &str) -> PathBuf {
    let digest = ring::digest::digest(&ring::digest::SHA1_FOR_LEGACY_USE_ONLY, id.as_bytes());
    env.aws_home().join(".aws").join("sso").join("cache").join(format!("{}.json", hex::encode(digest.as_ref())))
}

fn read_token(env: &Env, id: &str) -> Option<IndexMap<String, Value>> {
    let text = std::fs::read_to_string(token_path(env, id)).ok()?;
    serde_json::from_str(&text).ok()
}

/// `fromSSO({ profile })` as `fromIni` calls it.
pub async fn from_sso_profile(env: &Env, name: &str) -> Result<Identity, ProviderError> {
    let profiles = ini::known_profiles(env);
    let Some(profile) = profiles.get(name) else {
        return Err(ProviderError::next(format!("Profile {name} was not found.")));
    };
    if !is_sso(Some(profile)) {
        return Err(ProviderError::next(format!("Profile {name} is not configured with SSO credentials.")));
    }
    // Keys in order, with `undefined` for values the sso-session copied over as missing.
    let mut fields: IndexMap<String, Option<String>> = profile.iter().map(|(k, v)| (k.clone(), Some(v.clone()))).collect();
    if let Some(session_name) = profile.get("sso_session") {
        let sessions = ini::sso_sessions(env);
        let Some(session) = sessions.get(session_name) else {
            // The TypeError of the bundled SDK in upstream's binary.
            return Err(ProviderError::stop("undefined is not an object (evaluating 'h.sso_region')"));
        };
        fields.insert("sso_region".into(), session.get("sso_region").cloned());
        fields.insert("sso_start_url".into(), session.get("sso_start_url").cloned());
    }
    let get = |k: &str| fields.get(k).cloned().flatten().filter(|v| !v.is_empty());
    let (Some(start_url), Some(account_id), Some(region), Some(role_name)) =
        (get("sso_start_url"), get("sso_account_id"), get("sso_region"), get("sso_role_name"))
    else {
        return Err(ProviderError::stop(format!(
            "Profile is configured with invalid SSO credentials. Required parameters \"sso_account_id\", \"sso_region\", \"sso_role_name\", \"sso_start_url\". Got {}\nReference: https://docs.aws.amazon.com/cli/latest/userguide/cli-configure-sso.html",
            fields.keys().cloned().collect::<Vec<_>>().join(", ")
        )));
    };
    let session = get("sso_session");
    let (access_token, expires_at) = match &session {
        Some(_) => {
            let token = token_from_sso(env, name).await.map_err(|e| ProviderError::stop(e.message))?;
            (token.token, Some(token.expiration))
        }
        None => {
            let Some(token) = read_token(env, &start_url) else {
                return Err(ProviderError::stop(format!("The SSO session associated with this profile is invalid. {REFRESH}")));
            };
            let expires = token.get("expiresAt").map(js_date_value).unwrap_or(f64::NAN);
            (token.get("accessToken").map(js_string).unwrap_or_default(), Some(expires))
        }
    };
    if expires_at.is_some_and(|at| at - now_ms() <= 0.0) {
        return Err(ProviderError::stop(format!("The SSO session associated with this profile has expired. {REFRESH}")));
    }
    let credentials = get_role_credentials(env, &region, &account_id, &role_name, &access_token).await.map_err(ProviderError::stop)?;
    let role = credentials.get("roleCredentials").cloned().unwrap_or(Value::Null);
    let s = |k: &str| role.get(k).filter(|v| truthy(v)).map(js_string);
    match (s("accessKeyId"), s("secretAccessKey"), s("sessionToken"), role.get("expiration").filter(|v| truthy(v))) {
        (Some(access_key_id), Some(secret_access_key), Some(session_token), Some(expiration)) => {
            Ok(Identity { access_key_id, secret_access_key, session_token: Some(session_token), expiration: expiration.as_f64() })
        }
        _ => Err(ProviderError::stop("SSO returns an invalid temporary credential.")),
    }
}

fn truthy(value: &Value) -> bool {
    match value {
        Value::Null => false,
        Value::Bool(b) => *b,
        Value::Number(n) => n.as_f64().is_some_and(|f| f != 0.0),
        Value::String(s) => !s.is_empty(),
        _ => true,
    }
}

fn js_string(value: &Value) -> String {
    match value {
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

/// `new Date(value).getTime()` (`NaN` when it does not parse).
fn js_date_value(value: &Value) -> f64 {
    match value {
        Value::String(s) => crate::js_date::parse(s).unwrap_or(f64::NAN),
        Value::Number(n) => n.as_f64().unwrap_or(f64::NAN),
        _ => f64::NAN,
    }
}

/// SSO `GetRoleCredentials`; `Err` is `String(error)` of the thrown error.
async fn get_role_credentials(env: &Env, region: &str, account_id: &str, role_name: &str, token: &str) -> Result<Value, String> {
    let endpoint = service::endpoint(env, Service::Sso, region).map_err(|e| format!("Error: {e}"))?;
    let url = format!(
        "{}/federation/credentials?account_id={}&role_name={}",
        endpoint.url,
        service::encode_component(account_id),
        service::encode_component(role_name)
    );
    let client = service::http_client();
    let response = service::send(env, || client.get(&url).header("x-amz-sso_bearer_token", token), |r| Some(service::json_exception(r).0))
        .await
        .map_err(|e| format!("Error: {e}"))?;
    if response.status >= 300 {
        let (name, message) = service::json_exception(&response);
        return Err(if message.is_empty() {
            name
        } else {
            format!("{name}: {message}")
        });
    }
    serde_json::from_str(&response.body).map_err(|_| "SyntaxError: JSON Parse error: Unrecognized token".to_string())
}

/// A token from `fromSso`.
pub struct SsoToken {
    pub token: String,
    pub expiration: f64,
}

/// `TokenProviderError`.
pub struct TokenError {
    pub message: String,
}

fn token_error(message: impl Into<String>) -> TokenError {
    TokenError { message: message.into() }
}

/// `fromSso({ profile })()` of `@aws-sdk/token-providers`.
pub async fn token_from_sso(env: &Env, name: &str) -> Result<SsoToken, TokenError> {
    let profiles = ini::known_profiles(env);
    let Some(profile) = profiles.get(name) else {
        return Err(token_error(format!("Profile '{name}' could not be found in shared credentials file.")));
    };
    let Some(session_name) = profile.get("sso_session").filter(|v| !v.is_empty()) else {
        return Err(token_error(format!("Profile '{name}' is missing required property 'sso_session'.")));
    };
    let sessions = ini::sso_sessions(env);
    let Some(session) = sessions.get(session_name) else {
        return Err(token_error(format!("Sso session '{session_name}' could not be found in shared credentials file.")));
    };
    for key in ["sso_start_url", "sso_region"] {
        if session.get(key).is_none_or(|v| v.is_empty()) {
            return Err(token_error(format!("Sso session '{session_name}' is missing required property '{key}'.")));
        }
    }
    let region = session["sso_region"].clone();
    let Some(cached) = read_token(env, session_name) else {
        return Err(token_error(format!("The SSO session token associated with profile={name} was not found or is invalid. {TOKEN_REFRESH}")));
    };
    let require = |key: &str, refresh: bool| -> Result<(), TokenError> {
        if cached.contains_key(key) {
            Ok(())
        } else {
            Err(token_error(format!("Value not present for '{key}' in SSO Token{}. {TOKEN_REFRESH}", if refresh { ". Cannot refresh" } else { "" })))
        }
    };
    require("accessToken", false)?;
    require("expiresAt", false)?;
    let existing = SsoToken { token: js_string(&cached["accessToken"]), expiration: js_date_value(&cached["expiresAt"]) };
    if existing.expiration - now_ms() > EXPIRE_WINDOW_MS {
        return Ok(existing);
    }
    let expired = |token: SsoToken| -> Result<SsoToken, TokenError> {
        if token.expiration < now_ms() {
            Err(token_error(format!("Token is expired. {TOKEN_REFRESH}")))
        } else {
            Ok(token)
        }
    };
    {
        let last = env.state().sso_last_refresh.lock();
        if last.is_some_and(|at| at.elapsed() < Duration::from_secs(30)) {
            drop(last);
            return expired(existing);
        }
    }
    require("clientId", true)?;
    require("clientSecret", true)?;
    require("refreshToken", true)?;
    *env.state().sso_last_refresh.lock() = Some(Instant::now());
    match create_token(env, &region, &cached).await {
        Some((access_token, expires_in, refresh_token)) => {
            let expiration = now_ms() + expires_in * 1000.0;
            let mut updated = cached.clone();
            updated.insert("accessToken".into(), Value::String(access_token.clone()));
            updated.insert("expiresAt".into(), Value::String(iso(expiration)));
            match refresh_token {
                Some(token) => {
                    updated.insert("refreshToken".into(), token);
                }
                None => {
                    updated.shift_remove("refreshToken");
                }
            }
            if let Ok(text) = serde_json::to_string_pretty(&updated) {
                let _ = std::fs::write(token_path(env, session_name), text);
            }
            Ok(SsoToken { token: access_token, expiration })
        }
        None => expired(existing),
    }
}

/// `Date.prototype.toISOString`.
fn iso(ms: f64) -> String {
    chrono::DateTime::from_timestamp_millis(ms as i64).map(|d| d.format("%Y-%m-%dT%H:%M:%S%.3fZ").to_string()).unwrap_or_default()
}

/// SSO-OIDC `CreateToken` with the refresh token; `None` on any failure (the caller falls back).
async fn create_token(env: &Env, region: &str, cached: &IndexMap<String, Value>) -> Option<(String, f64, Option<Value>)> {
    let endpoint = service::endpoint(env, Service::SsoOidc, region).ok()?;
    let url = format!("{}/token", endpoint.url);
    let mut body = serde_json::Map::new();
    for (key, source) in [("clientId", "clientId"), ("clientSecret", "clientSecret"), ("grantType", ""), ("refreshToken", "refreshToken")] {
        if source.is_empty() {
            body.insert(key.into(), Value::String("refresh_token".into()));
        } else if let Some(value) = cached.get(source).filter(|v| !v.is_null()) {
            body.insert(key.into(), value.clone());
        }
    }
    let body = Value::Object(body).to_string();
    let client = service::http_client();
    let response =
        service::send(env, || client.post(&url).header("content-type", "application/json").body(body.clone()), |r| Some(service::json_exception(r).0))
            .await
            .ok()?;
    if response.status >= 300 {
        return None;
    }
    let parsed: Value = serde_json::from_str(&response.body).ok()?;
    let access_token = parsed.get("accessToken").filter(|v| !v.is_null())?;
    let expires_in = parsed.get("expiresIn").filter(|v| !v.is_null())?.as_f64()?;
    Some((js_string(access_token), expires_in, parsed.get("refreshToken").filter(|v| !v.is_null()).cloned()))
}

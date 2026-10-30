//! OpenCode Console usage limits (`packages/core/src/usage-limits.ts`, `packages/schema/src/usage-limits.ts`):
//! the Go allowance and the budgets of the Console workspace a location is signed in to. Backs
//! `GET /api/experimental/usage/limits`.

use async_trait::async_trait;
use oc_ai::credentials::flows::console::{credential_headers, server_url};
use oc_ai::credentials::integration::Integrations;
use oc_ai::credentials::CredentialValue;
use serde_json::{json, Map, Value};
use std::time::Duration;

/// The integration of the OpenCode Console sign-in.
pub const INTEGRATION_ID: &str = "opencode";
/// The overall limit on both Console calls.
pub const TIMEOUT: Duration = Duration::from_secs(15);

/// `UsageLimits.UnavailableError`: answered as 503 `{service: "opencode"}`.
#[derive(Debug, Clone, PartialEq, thiserror::Error)]
#[error("{message}")]
pub struct UnavailableError {
    pub message: String,
}

impl UnavailableError {
    fn new(message: impl Into<String>) -> Self {
        Self { message: message.into() }
    }
}

/// One HTTP answer.
#[derive(Debug, Clone)]
pub struct Response {
    pub status: u16,
    pub body: String,
}

/// `GET url` with headers (a transport failure is an error message).
#[async_trait]
pub trait Http: Send + Sync {
    async fn get(&self, url: &str, headers: &[(String, String)]) -> Result<Response, String>;
}

/// reqwest-backed [`Http`].
pub struct ReqwestHttp(pub reqwest::Client);

impl Default for ReqwestHttp {
    fn default() -> Self {
        Self(oc_util::http::client())
    }
}

#[async_trait]
impl Http for ReqwestHttp {
    async fn get(&self, url: &str, headers: &[(String, String)]) -> Result<Response, String> {
        let mut request = self.0.get(url);
        for (k, v) in headers {
            request = request.header(k, v);
        }
        let response = request.send().await.map_err(|e| e.to_string())?;
        let status = response.status().as_u16();
        let body = response.text().await.map_err(|e| e.to_string())?;
        Ok(Response { status, body })
    }
}

/// `UsageLimits.get()`: `None` when the location has no active OpenCode Console connection, the connection is
/// external, or its credential names no workspace (`metadata.orgID`).
/// Requests carry the app's `User-Agent`.
pub async fn get(integrations: &Integrations, http: &dyn Http, user_agent: &str) -> Result<Option<Value>, UnavailableError> {
    let Some(connection) = integrations.active(INTEGRATION_ID).await.map_err(|e| UnavailableError::new(e.to_string()))? else {
        return Ok(None);
    };
    let credential = integrations.resolve(&connection).await.map_err(|e| UnavailableError::new(e.to_string()))?;
    let Some(credential) = credential else { return Ok(None) };
    if matches!(credential, CredentialValue::External { .. }) {
        return Ok(None);
    }
    let metadata = credential.metadata();
    let Some(workspace_id) = metadata.and_then(|m| m.get("orgID")).and_then(Value::as_str).map(String::from) else {
        return Ok(None);
    };
    let server = server_url(&credential);
    let both = async {
        tokio::try_join!(read_budgets(http, user_agent, &server, &credential), read_go(http, user_agent, &server, &credential, &workspace_id))
    };
    let (budgets, go) = tokio::time::timeout(TIMEOUT, both)
        .await
        .map_err(|_| UnavailableError::new("OpenCode Console usage limits timed out"))??;
    let mut workspace = Map::new();
    workspace.insert("id".into(), json!(workspace_id));
    if let Some(name) = metadata.and_then(|m| m.get("orgName")).and_then(Value::as_str) {
        workspace.insert("name".into(), json!(name));
    }
    workspace.insert("usageURL".into(), json!(format!("{server}/{workspace_id}/activity/overview")));
    let mut info = Map::new();
    info.insert("workspace".into(), Value::Object(workspace));
    if let Some(go) = go {
        info.insert("go".into(), go);
    }
    info.insert("budgets".into(), Value::Array(budgets));
    Ok(Some(Value::Object(info)))
}

fn status_error(status: u16, url: &str) -> String {
    format!("StatusCode: non 2xx status code ({status} GET {url})")
}

/// `Schema.DateFromString` as epoch milliseconds.
fn instant(value: Option<&Value>) -> Result<Value, String> {
    let text = value.and_then(Value::as_str).ok_or("Expected string")?;
    let time = oc_ai::js_date::parse(text).ok_or_else(|| format!("Invalid date: {text}"))?;
    Ok(oc_ai::json::num(time))
}

fn finite(value: Option<&Value>) -> Result<f64, String> {
    value.and_then(Value::as_f64).ok_or_else(|| "Expected a finite number".to_string())
}

fn literal(value: Option<&Value>, allowed: &[&str]) -> Result<Value, String> {
    match value.and_then(Value::as_str) {
        Some(s) if allowed.contains(&s) => Ok(json!(s)),
        _ => Err(format!("Expected {}", allowed.iter().map(|a| format!("\"{a}\"")).collect::<Vec<_>>().join(" | "))),
    }
}

fn string(value: Option<&Value>) -> Result<Value, String> {
    value.filter(|v| v.is_string()).cloned().ok_or_else(|| "Expected string".to_string())
}

fn boolean(value: Option<&Value>) -> Result<Value, String> {
    value.filter(|v| v.is_boolean()).cloned().ok_or_else(|| "Expected boolean".to_string())
}

/// One budget as `GET /api/policies/current-budgets` describes it, as a `UsageLimits.Budget`.
fn budget(item: &Value) -> Result<Value, String> {
    let o = item.as_object().ok_or("Expected object")?;
    let models = o
        .get("models")
        .and_then(Value::as_array)
        .ok_or("Expected array")?
        .iter()
        .map(|m| {
            let m = m.as_object().ok_or("Expected object")?;
            let mut out = Map::new();
            out.insert("providerID".into(), string(m.get("providerID"))?);
            out.insert("modelID".into(), string(m.get("modelID"))?);
            out.insert("name".into(), string(m.get("name"))?);
            if let Some(spent) = m.get("spentDollars") {
                out.insert("spent".into(), oc_ai::json::num(finite(Some(spent))?));
            }
            Ok(Value::Object(out))
        })
        .collect::<Result<Vec<_>, String>>()?;
    let vendors = o
        .get("vendors")
        .and_then(Value::as_array)
        .ok_or("Expected array")?
        .iter()
        .map(|v| {
            let v = v.as_object().ok_or("Expected object")?;
            string(v.get("id"))?;
            string(v.get("name"))
        })
        .collect::<Result<Vec<_>, String>>()?;
    let mut out = Map::new();
    out.insert("id".into(), string(o.get("ruleID"))?);
    out.insert("limit".into(), oc_ai::json::num(finite(o.get("limitDollars"))?));
    out.insert("spent".into(), oc_ai::json::num(finite(o.get("spentDollars"))?));
    out.insert("scope".into(), literal(o.get("scope"), &["each", "combined"])?);
    out.insert("per".into(), literal(o.get("per"), &["set", "model"])?);
    out.insert("coverage".into(), literal(o.get("coverage"), &["all", "models"])?);
    out.insert("models".into(), Value::Array(models));
    out.insert("vendors".into(), Value::Array(vendors));
    if let Some(price) = o.get("inputPriceAtLeast") {
        out.insert("inputPriceAtLeast".into(), oc_ai::json::num(finite(Some(price))?));
    }
    out.insert("picked".into(), boolean(o.get("picked"))?);
    out.insert("shared".into(), boolean(o.get("shared"))?);
    out.insert("resetsAt".into(), instant(o.get("resetsAt"))?);
    Ok(Value::Object(out))
}

/// Every budget covering the member. Console refuses a service account's key with 403, and it has no budgets.
async fn read_budgets(http: &dyn Http, user_agent: &str, server: &str, credential: &CredentialValue) -> Result<Vec<Value>, UnavailableError> {
    let url = format!("{server}/api/policies/current-budgets");
    let mut headers = vec![("user-agent".to_string(), user_agent.to_string()), ("accept".to_string(), "application/json".to_string())];
    headers.extend(credential_headers(credential));
    let result = async {
        let response = http.get(&url, &headers).await?;
        if response.status == 403 {
            return Ok(vec![]);
        }
        if !(200..300).contains(&response.status) {
            return Err(status_error(response.status, &url));
        }
        let body: Value = serde_json::from_str(&response.body).map_err(|e| e.to_string())?;
        body.as_array().ok_or("Expected array")?.iter().map(budget).collect()
    }
    .await;
    result.map_err(|e: String| UnavailableError::new(format!("OpenCode Console budgets are unavailable: {e}")))
}

fn meter(value: Option<&Value>) -> Result<Value, String> {
    let o = value.and_then(Value::as_object).ok_or("Expected object")?;
    let status = literal(o.get("status"), &["ok", "rate-limited"])?;
    let percent = finite(o.get("percent"))?;
    Ok(json!({"percent": oc_ai::json::num(percent), "limited": status == "rate-limited", "resetsAt": instant(o.get("resetsAt"))?}))
}

/// The Go allowance, from the inference API that shares the Console's origin. Go refuses (403) a workspace the
/// member doesn't subscribe to Go in, which has no allowance to show.
async fn read_go(
    http: &dyn Http,
    user_agent: &str,
    server: &str,
    credential: &CredentialValue,
    workspace_id: &str,
) -> Result<Option<Value>, UnavailableError> {
    let token = match credential {
        CredentialValue::Oauth { access, .. } => access.clone(),
        CredentialValue::Key { key, .. } => key.clone(),
        CredentialValue::External { .. } => String::new(),
    };
    let result = async {
        let origin = url::Url::parse(server).map_err(|e| e.to_string())?.origin().ascii_serialization();
        let url = format!("{origin}/zen/go/v1/usage");
        let headers = vec![
            ("user-agent".to_string(), user_agent.to_string()),
            ("accept".to_string(), "application/json".to_string()),
            ("authorization".to_string(), format!("Bearer {token}")),
            ("x-opencode-org-id".to_string(), workspace_id.to_string()),
        ];
        let response = http.get(&url, &headers).await?;
        if response.status == 403 {
            return Ok(None);
        }
        if !(200..300).contains(&response.status) {
            return Err(status_error(response.status, &url));
        }
        let body: Value = serde_json::from_str(&response.body).map_err(|e| e.to_string())?;
        let usage = body.get("usage");
        let field = |key: &str| meter(usage.and_then(|u| u.get(key)));
        Ok(Some(json!({"rolling": field("rolling")?, "weekly": field("weekly")?, "monthly": field("monthly")?})))
    }
    .await;
    result.map_err(|e: String| UnavailableError::new(format!("OpenCode Go usage is unavailable: {e}")))
}

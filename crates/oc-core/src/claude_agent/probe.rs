//! The login check (`claude/probe.ts`): start `claude` without a prompt, read its version, account and
//! models from `initialize`, and on new enough versions ask each model's context window. No message is
//! sent, so no model call is made.

use std::time::Duration;

use serde_json::{Value, json};

use super::process::{Options, Query};

/// Older binaries silently ignore `verbatimPrompts` and can interpret user text as Claude commands.
pub const MINIMUM_VERSION: &str = "2.1.248";
/// Older binaries do not support summary-only context usage. Their models keep the conservative fallback.
pub const CONTEXT_DISCOVERY_VERSION: &str = "2.1.257";
pub const START_TIMEOUT: Duration = Duration::from_secs(30);

/// `ClaudeProbe.Result`.
#[derive(Debug, Clone, PartialEq)]
pub struct ProbeResult {
    pub version: Option<String>,
    pub account: Value,
    /// `ModelInfo`s, with `contextWindow` when discovered.
    pub models: Vec<Value>,
}

fn parse(value: &str) -> Vec<f64> {
    let release = value.split(['-', '+']).next().unwrap_or_default();
    release.split('.').map(|p| if p.trim().is_empty() { 0.0 } else { p.trim().parse::<f64>().unwrap_or(f64::NAN) }).collect()
}

/// Compares release numbers like 2.1.248. A pre-release or build suffix is ignored.
pub fn at_least(version: &str, minimum: &str) -> bool {
    let current = parse(version);
    let required = parse(minimum);
    let index = required.iter().enumerate().position(|(i, part)| current.get(i).copied().unwrap_or(0.0) != *part);
    match index {
        None => true,
        Some(i) => current.get(i).copied().unwrap_or(0.0) > required[i],
    }
}

/// `ClaudeProbe.versionError`.
pub fn version_error(version: Option<&str>) -> Option<String> {
    if let Some(v) = version.filter(|v| !v.is_empty())
        && at_least(v, MINIMUM_VERSION)
    {
        return None;
    }
    let message = match version.filter(|v| !v.is_empty()) {
        Some(v) => format!("Claude Code {v} is too old; {MINIMUM_VERSION} or later is required."),
        None => "Claude Code did not report its version, so compatibility could not be checked.".into(),
    };
    Some(format!("{message} Run `claude update`."))
}

/// `ClaudeProbe.isLoggedIn`.
pub fn is_logged_in(account: &Value) -> bool {
    account.get("tokenSource").and_then(Value::as_str) != Some("none") || account.get("apiKeySource").is_some()
}

/// `ClaudeProbe.run(options)`.
pub async fn run(mut options: Options) -> Result<ProbeResult, String> {
    options.persist = false;
    let query = Query::start(options);
    let result = probe(&query).await;
    // The never-yielding prompt ends, then the query closes.
    query.end_input();
    query.close();
    result
}

async fn probe(query: &std::sync::Arc<Query>) -> Result<ProbeResult, String> {
    let init = match tokio::time::timeout(START_TIMEOUT, query.initialization()).await {
        Ok(init) => init?,
        Err(_) => return Err("claude did not start within 30 seconds".into()),
    };
    // No prompt is sent here, so an old Claude Code can still list its models.
    // The version is checked when a chat starts, and that chat's error tells the user to update.
    let version = init.get("claude_code_version").and_then(Value::as_str).map(String::from);
    let mut models = init.get("models").and_then(Value::as_array).cloned().unwrap_or_default();
    if version.as_deref().is_some_and(|v| !v.is_empty() && at_least(v, CONTEXT_DISCOVERY_VERSION)) {
        let discovery = discover(query, &mut models);
        if tokio::time::timeout(START_TIMEOUT, discovery).await.is_err() {
            tracing::warn!("Claude Code context discovery timed out; undiscovered models use conservative limits");
        }
    }
    Ok(ProbeResult { version, account: init.get("account").cloned().unwrap_or(json!({})), models })
}

/// Sets each model's context window in place, so models discovered before a timeout keep theirs.
async fn discover(query: &std::sync::Arc<Query>, models: &mut [Value]) {
    for model in models.iter_mut() {
        let value = model.get("value").and_then(Value::as_str).unwrap_or_default().to_string();
        if value == "default" {
            continue;
        }
        let attempt = async {
            query.request(json!({ "subtype": "set_model", "model": value })).await?;
            let usage = query.request(json!({ "subtype": "get_context_usage", "detail": "summary" })).await?;
            // rawMaxTokens is capacity; maxTokens can reflect Claude's own compaction window.
            let raw = usage.get("rawMaxTokens").and_then(Value::as_f64).filter(|v| v.fract() == 0.0 && *v > 0.0 && *v <= 9_007_199_254_740_991.0);
            raw.ok_or_else(|| "Claude Code did not report a valid context window".to_string())
        };
        match attempt.await {
            Ok(window) => {
                if let Some(o) = model.as_object_mut() {
                    o.insert("contextWindow".into(), oc_ai::json::num(window));
                }
            }
            Err(cause) => tracing::warn!(model = %value, %cause, "Claude Code context discovery failed"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn versions() {
        assert!(at_least("2.1.248", MINIMUM_VERSION));
        assert!(at_least("2.1.300-beta.1", MINIMUM_VERSION));
        assert!(at_least("3.0.0", MINIMUM_VERSION));
        assert!(!at_least("2.1.247", MINIMUM_VERSION));
        assert!(!at_least("2.0.999", MINIMUM_VERSION));
        assert_eq!(version_error(Some("2.1.200")).unwrap(), "Claude Code 2.1.200 is too old; 2.1.248 or later is required. Run `claude update`.");
        assert_eq!(
            version_error(None).unwrap(),
            "Claude Code did not report its version, so compatibility could not be checked. Run `claude update`."
        );
        assert!(version_error(Some("2.1.302")).is_none());
    }

    #[test]
    fn login() {
        assert!(!is_logged_in(&json!({ "tokenSource": "none", "apiProvider": "firstParty" })));
        assert!(is_logged_in(&json!({ "tokenSource": "none", "apiKeySource": "ANTHROPIC_API_KEY" })));
        assert!(is_logged_in(&json!({ "email": "user@example.com", "apiProvider": "firstParty" })));
    }
}

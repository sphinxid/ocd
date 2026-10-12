//! `opencode debug config` redaction (`cli/src/commands/handlers/debug/redact.ts`).

use std::sync::LazyLock;

use regex::Regex;
use serde_json::{Map, Value};

/// `/(?:api.?key|secret|password|token$|authorization$|cookie$|credential|private.?key)/i`
pub static SECRET_NAME: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)(?:api.?key|secret|password|token$|authorization$|cookie$|credential|private.?key)").unwrap()
});

static HTTP: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?i)^https?://").unwrap());

fn secret_name(name: &str) -> bool {
    // JS `.` does not match line terminators; names with newlines are vanishingly rare.
    SECRET_NAME.is_match(name)
}

fn redact_url(item: &str) -> bool {
    let Ok(url) = url::Url::parse(item) else { return true };
    !url.username().is_empty() || url.password().is_some() || url.query_pairs().any(|(name, _)| secret_name(&name))
}

/// Redact secrets for display. Order is unchanged.
pub fn redact_config(value: &Value) -> Value {
    redact(value, false)
}

fn redact(value: &Value, headers: bool) -> Value {
    match value {
        Value::Array(items) => Value::Array(items.iter().map(|i| redact(i, headers)).collect()),
        Value::Object(map) => {
            let mut out = Map::new();
            for (key, item) in map {
                if let Value::String(s) = item {
                    if headers || secret_name(key) {
                        out.insert(key.clone(), Value::String("***".into()));
                        continue;
                    }
                    if HTTP.is_match(s) && redact_url(s) {
                        out.insert(key.clone(), Value::String("***".into()));
                        continue;
                    }
                }
                out.insert(key.clone(), redact(item, headers || key.to_lowercase() == "headers"));
            }
            Value::Object(out)
        }
        other => other.clone(),
    }
}

/// `JSON.stringify(redactConfig(entries), null, 2) + EOL`.
pub fn render_debug_config(entries: &Value) -> String {
    let mut out = serde_json::to_string_pretty(&redact_config(entries)).unwrap_or_default();
    out.push('\n');
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn redacts_secret_names_headers_and_urls() {
        let input = json!([{
            "type": "document",
            "info": {
                "providers": { "x": {
                    "settings": { "apiKey": "sk-1", "baseURL": "https://api.example.com/v1", "accessToken": "t", "tokenizer": "keep" },
                    "headers": { "X-Custom": "v", "nested": { "a": "b" } },
                }},
                "mcp": { "servers": { "r": { "type": "remote", "url": "https://user:pw@example.com" } } },
                "references": { "a": "https://example.com/?api_key=1", "b": "https://example.com/?q=1", "c": "http://[bad" },
                "count": 3,
                "list": ["https://u@x.com"],
            },
        }]);
        let out = redact_config(&input);
        let info = &out[0]["info"];
        assert_eq!(info["providers"]["x"]["settings"]["apiKey"], "***");
        assert_eq!(info["providers"]["x"]["settings"]["baseURL"], "https://api.example.com/v1");
        assert_eq!(info["providers"]["x"]["settings"]["accessToken"], "***");
        assert_eq!(info["providers"]["x"]["settings"]["tokenizer"], "keep");
        assert_eq!(info["providers"]["x"]["headers"]["X-Custom"], "***");
        assert_eq!(info["providers"]["x"]["headers"]["nested"]["a"], "***");
        assert_eq!(info["mcp"]["servers"]["r"]["url"], "***");
        assert_eq!(info["references"]["a"], "***");
        assert_eq!(info["references"]["b"], "https://example.com/?q=1");
        assert_eq!(info["references"]["c"], "***");
        assert_eq!(info["count"], 3);
        // array items have no key: only nested objects are inspected
        assert_eq!(info["list"][0], "https://u@x.com");
    }
}

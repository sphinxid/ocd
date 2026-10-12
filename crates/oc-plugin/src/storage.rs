//! Plugin storage namespacing (`PluginHost.storage`, `packages/core/src/plugin/host.ts:609-636`).

use serde_json::{Value, json};

use crate::api::{ApiError, PluginApi, StorageScan};

/// `plugin:<each UTF-16 code unit as 4 hex digits>:`.
pub fn namespace(plugin_id: &str) -> String {
    let hex: String = plugin_id.encode_utf16().map(|unit| format!("{unit:04x}")).collect();
    format!("plugin:{hex}:")
}

/// Serves one `storage.*` call for `plugin_id` against the core's KV.
pub async fn handle(api: &dyn PluginApi, plugin_id: &str, method: &str, input: &Value) -> Result<Value, ApiError> {
    let namespace = namespace(plugin_id);
    let key = |field: &str| -> Result<String, ApiError> {
        input
            .get(field)
            .and_then(Value::as_str)
            .map(|k| format!("{namespace}{k}"))
            .ok_or_else(|| ApiError::new(format!("storage: missing {field}")))
    };
    match method {
        "storage.get" => Ok(api.storage_get(&key("key")?).await?.unwrap_or(Value::Null)),
        "storage.set" => {
            api.storage_set(&key("key")?, input.get("value").cloned().unwrap_or(Value::Null)).await?;
            Ok(Value::Null)
        }
        "storage.remove" => {
            api.storage_remove(&key("key")?).await?;
            Ok(Value::Null)
        }
        "storage.scan" => {
            let prefix = format!("{namespace}{}", input.get("prefix").and_then(Value::as_str).unwrap_or(""));
            let after = input.get("after").and_then(Value::as_str).map(|a| format!("{namespace}{a}"));
            let limit = input.get("limit").and_then(Value::as_u64);
            let StorageScan { entries, next } = api.storage_scan(&prefix, after.as_deref(), limit).await?;
            let entries: Vec<Value> = entries
                .into_iter()
                .map(|e| json!({ "key": e.key.get(namespace.len()..).unwrap_or(""), "value": e.value }))
                .collect();
            Ok(match next {
                Some(next) => json!({ "entries": entries, "next": next.get(namespace.len()..).unwrap_or("") }),
                None => json!({ "entries": entries }),
            })
        }
        other => Err(ApiError::unsupported(other)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn namespace_matches_upstream() {
        // "a.b" → 0061 002e 0062
        assert_eq!(namespace("a.b"), "plugin:0061002e0062:");
        // non-BMP characters encode as two UTF-16 code units
        assert_eq!(namespace("😀"), "plugin:d83dde00:");
    }
}

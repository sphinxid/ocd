//! Catalog shaping (`src/mini/catalog.shared.ts`): agents, commands, references and the provider/model
//! footer catalog.

use super::types::{RunAgent, RunCommand, RunProvider, RunProviderModel, RunReference};
use serde_json::Value;

fn s(v: &Value, k: &str) -> String {
    v.get(k).and_then(|x| x.as_str()).unwrap_or_default().to_string()
}

fn opt(v: &Value, k: &str) -> Option<String> {
    v.get(k).and_then(|x| x.as_str()).map(|x| x.to_string())
}

pub fn run_agent(v: &Value) -> RunAgent {
    RunAgent { id: s(v, "id"), name: s(v, "name"), description: opt(v, "description"), mode: s(v, "mode"), hidden: v.get("hidden").and_then(|h| h.as_bool()).unwrap_or(false) }
}

pub fn run_command(v: &Value) -> RunCommand {
    RunCommand { name: s(v, "name"), description: opt(v, "description"), source: None }
}

/// Visible references (`loadRunReferences` drops `hidden`).
pub fn run_references(list: &[Value]) -> Vec<RunReference> {
    list.iter()
        .filter(|r| r.get("hidden").and_then(|h| h.as_bool()) != Some(true))
        .map(|r| {
            let source = r.get("source").cloned().unwrap_or(Value::Null);
            let source_type = s(&source, "type");
            RunReference {
                name: s(r, "name"),
                path: s(r, "path"),
                description: opt(r, "description"),
                source_detail: if source_type == "git" { s(&source, "repository") } else { s(&source, "path") },
                source_type,
            }
        })
        .collect()
}

/// `defaultCost(model)`.
fn default_cost(model: &Value) -> Option<f64> {
    let costs = model.get("cost").and_then(|c| c.as_array())?;
    let picked = costs.iter().find(|c| c.get("tier").is_none_or(|t| t.is_null())).or(costs.first())?;
    if costs.iter().all(|c| c.get("input").and_then(|i| i.as_f64()) == Some(0.0)) {
        return Some(0.0);
    }
    picked.get("input").and_then(|i| i.as_f64())
}

/// `runProviders(providers, models)`.
pub fn run_providers(providers: &[Value], models: &[Value]) -> Vec<RunProvider> {
    let mut grouped: indexmap::IndexMap<String, RunProvider> = indexmap::IndexMap::new();
    for p in providers {
        let id = s(p, "id");
        grouped.insert(id.clone(), RunProvider { id, name: s(p, "name"), models: Default::default() });
    }
    for m in models {
        let provider_id = s(m, "providerID");
        let entry = grouped.entry(provider_id.clone()).or_insert_with(|| RunProvider { id: provider_id.clone(), name: provider_id.clone(), models: Default::default() });
        entry.models.insert(
            s(m, "id"),
            RunProviderModel {
                name: opt(m, "name"),
                cost_input: default_cost(m),
                context: m.pointer("/limit/context").and_then(|c| c.as_f64()),
                status: opt(m, "status"),
                variants: m.get("variants").and_then(|v| v.as_array()).map(|a| a.iter().map(|v| s(v, "id")).collect()).unwrap_or_default(),
            },
        );
    }
    grouped.into_values().collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn references_drop_hidden() {
        let refs = run_references(&[
            json!({"name": "effect", "path": "/repos/effect", "description": "Effect v4 sources", "source": {"type": "local", "path": "/repos/effect"}}),
            json!({"name": "secret", "path": "/repos/secret", "hidden": true, "source": {"type": "local", "path": "/repos/secret"}}),
        ]);
        assert_eq!(refs.len(), 1);
        assert_eq!((refs[0].name.as_str(), refs[0].path.as_str(), refs[0].description.as_deref()), ("effect", "/repos/effect", Some("Effect v4 sources")));
    }

    #[test]
    fn merges_providers_and_models() {
        let providers = run_providers(
            &[json!({"id": "openai", "name": "OpenAI", "activation": "auto", "package": ""})],
            &[json!({
                "id": "gpt-5", "modelID": "openai", "providerID": "openai", "name": "Little Frank",
                "capabilities": {"tools": true, "input": ["text"], "output": ["text"]},
                "variants": [{"id": "high"}], "time": {"released": 1},
                "cost": [{"input": 0, "output": 0, "cache": {"read": 0, "write": 0}}],
                "status": "active", "enabled": true, "limit": {"context": 128000, "output": 8192}
            })],
        );
        assert_eq!(providers.len(), 1);
        assert_eq!((providers[0].id.as_str(), providers[0].name.as_str()), ("openai", "OpenAI"));
        let m = &providers[0].models["gpt-5"];
        assert_eq!(
            m,
            &RunProviderModel { name: Some("Little Frank".into()), cost_input: Some(0.0), context: Some(128000.0), status: Some("active".into()), variants: vec!["high".into()] }
        );
    }
}

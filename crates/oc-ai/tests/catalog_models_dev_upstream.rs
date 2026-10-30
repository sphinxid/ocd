//! Cross-checks ocd's models.dev normalization against upstream's `ModelsDev.decodeSnapshot`, run with
//! Bun on the upstream checkout. Needs `OCD_SCRATCH` (with `src/opencode-2.2.2`, dependencies installed)
//! and `OCD_BUN_PATH`; skipped otherwise.

use oc_ai::catalog::models_dev::{decode_snapshot, Snapshot, SNAPSHOT};
use serde_json::{json, Value};
use std::path::PathBuf;

fn upstream_core() -> Option<(PathBuf, PathBuf)> {
    let scratch = std::env::var("OCD_SCRATCH").ok()?;
    let bun = PathBuf::from(std::env::var("OCD_BUN_PATH").ok()?);
    let core = PathBuf::from(scratch).join("src/opencode-2.2.2/packages/core/src");
    (core.join("models-dev.ts").is_file() && bun.is_file()).then_some((core, bun))
}

fn upstream_normalize(core: &PathBuf, bun: &PathBuf, catalog: &str) -> Value {
    let dir = tempfile::tempdir().unwrap();
    let script = dir.path().join("normalize.ts");
    std::fs::write(
        &script,
        format!(
            r#"import {{ Effect }} from "effect"
import {{ readFileSync, writeFileSync }} from "node:fs"
const {{ ModelsDev }} = await import({core:?} + "/models-dev.ts")
const result = await Effect.runPromise(ModelsDev.decodeSnapshot(readFileSync(process.argv[2], "utf8")))
writeFileSync(process.argv[3], JSON.stringify(result))
"#,
            core = core.display().to_string()
        ),
    )
    .unwrap();
    let input = dir.path().join("catalog.json");
    std::fs::write(&input, catalog).unwrap();
    // `effect` resolves from the upstream core package.
    let output = std::process::Command::new(bun)
        .arg("run")
        .arg(&script)
        .arg(&input)
        .arg(dir.path().join("out.json"))
        .current_dir(core.parent().unwrap())
        .env("NODE_PATH", core.parent().unwrap().join("node_modules"))
        .output()
        .unwrap();
    assert!(output.status.success(), "bun failed: {}", String::from_utf8_lossy(&output.stderr));
    serde_json::from_slice(&std::fs::read(dir.path().join("out.json")).unwrap()).unwrap()
}

/// Numbers compared as f64 (JSON `1` and `1.0` are the same value in JavaScript).
fn numbers(value: Value) -> Value {
    match value {
        Value::Number(n) => json!(n.as_f64().unwrap()),
        Value::Array(items) => Value::Array(items.into_iter().map(numbers).collect()),
        Value::Object(o) => Value::Object(o.into_iter().map(|(k, v)| (k, numbers(v))).collect()),
        other => other,
    }
}

fn ours(data: &[Snapshot]) -> Value {
    Value::Array(data.iter().map(|s| json!({"info": s.info, "models": s.models, "environment": s.environment})).collect())
}

fn compare(catalog: &str) {
    let Some((core, bun)) = upstream_core() else {
        eprintln!("upstream 2.2.2 checkout or bun not found (set OCD_SCRATCH and OCD_BUN_PATH); skipping");
        return;
    };
    let expected = numbers(upstream_normalize(&core, &bun, catalog));
    let actual = numbers(ours(&decode_snapshot(catalog).unwrap()));
    let (expected, actual) = (expected.as_array().unwrap(), actual.as_array().unwrap());
    assert_eq!(
        actual.iter().map(|p| p["info"]["id"].clone()).collect::<Vec<_>>(),
        expected.iter().map(|p| p["info"]["id"].clone()).collect::<Vec<_>>()
    );
    for (a, e) in actual.iter().zip(expected) {
        assert_eq!(a["info"], e["info"], "provider {}", e["info"]["id"]);
        assert_eq!(a["environment"], e["environment"], "provider {}", e["info"]["id"]);
        let (am, em) = (a["models"].as_array().unwrap(), e["models"].as_array().unwrap());
        assert_eq!(
            am.iter().map(|m| m["id"].clone()).collect::<Vec<_>>(),
            em.iter().map(|m| m["id"].clone()).collect::<Vec<_>>(),
            "models of {}",
            e["info"]["id"]
        );
        for (m, u) in am.iter().zip(em) {
            assert_eq!(m, u, "model {}/{}", e["info"]["id"], u["id"]);
        }
    }
}

#[test]
fn bundled_snapshot_normalizes_like_upstream() {
    compare(SNAPSHOT);
}

#[test]
fn reasoning_fixture_normalizes_like_upstream() {
    compare(include_str!("fixtures/catalog/models-dev-reasoning.json"));
}

#[test]
fn v1_catalog_normalizes_like_upstream() {
    compare(include_str!("fixtures/catalog/models-dev-reasoning-v1.json"));
}

#[test]
fn edge_cases_normalize_like_upstream() {
    let model = |id: &str, extra: Value| {
        let mut m = json!({
            "id": id, "type": "chat", "name": id, "release_date": "2026-01-01",
            "modalities": {"input": ["text"], "output": ["text"]},
            "capabilities": {"tools": {"supported": true}, "reasoning": {"supported": false}},
            "api": {"chat-completions": {"base_url": "https://api.acme.test/v1", "sdk": {"aisdk": "@ai-sdk/openai-compatible"}}}
        });
        for (k, v) in extra.as_object().unwrap() {
            m[k] = v.clone();
        }
        m
    };
    let catalog = json!({
        "acme": {"id": "acme", "name": "Acme", "env": ["ACME_KEY"], "models": {
            "plain": model("plain", json!({})),
            "family-empty": model("family-empty", json!({"family": "", "status": "beta"})),
            "family-null": model("family-null", json!({"family": null})),
            "status-other": model("status-other", json!({"status": "preview", "release_date": "2025-02"})),
            "reasoning": model("reasoning", json!({"capabilities": {"tools": {"supported": true}, "reasoning": {"supported": true, "toggle": true, "effort": ["default", "low", "high"], "budget": {"min": 1024, "max": 32000}}, "temperature": false}, "limit": {"context": 200000, "input": 180000, "output": 64000}})),
            "bad-budget": model("bad-budget", json!({"capabilities": {"tools": {"supported": true}, "reasoning": {"supported": true, "budget": {"min": 10.5}}}})),
            "media": model("media", json!({"modalities": {"input": ["text/plain", "image/png", "image/jpeg", "application/pdf", "audio/wav", "video/mp4", "application/json"], "output": ["text", "image/webp"]}})),
            "responses": model("responses", json!({"api": {
                "responses": {"base_url": "https://api.acme.test/v1", "sdk": {"aisdk": "@ai-sdk/openai"}, "compatibility": {"reasoning_field": "rc", "max_tokens_field": "max_tokens", "media_input": "assets"}},
                "chat-completions": {"base_url": "https://x.acme.test", "sdk": {"aisdk": "@ai-sdk/openai"}}
            }})),
            "bad-compat": model("bad-compat", json!({"api": {"chat-completions": {"base_url": "", "sdk": {"opencodeai": "@opencode/ai/providers/openai-compatible"}, "compatibility": {"max_tokens_field": "tokens", "media_input": "elsewhere"}}}})),
            "costly": model("costly", json!({"cost": {
                "text": {
                    "input": {"price": 3, "cache": {"read": 0.3, "explicit": {"write": {"1h": 6, "5m": 3.75}}}, "overrides": [
                        {"when": {"context": {"gte": 200000}}, "price": 6},
                        {"when": {"context": {"gt": 1000000}}, "cache": {"write": 9}},
                        {"when": {"context": {"gt": 5}, "service_tier": {"equals": "x"}}, "price": 1}
                    ]},
                    "output": {"price": 15, "overrides": [{"when": {"context": {"gte": 200000}}, "price": 22.5}, {"when": {"context": {"gt": 300000}}}]}
                },
                "multipliers": [
                    {"when": {"service_tier": {"equals": "flex"}}, "factor": 0.5},
                    {"when": {"service_tier": {"equals": "priority"}}, "factor": {"text": {"input": {"price": 2, "cache": {"read": 2, "write": {"5m": 1.5}}}, "output": 1.7}}}
                ]
            }, "experimental": {"modes": {
                "flex": {},
                "fast": {"service_tier": "priority", "headers": {"x-fast": "1"}},
                "custom": {"cost": {"text": {"input": {"price": 1}, "output": {"price": 2}}}, "body": {"speed": "fast"}},
                "broken": {"headers": {"x": 1}}
            }}})),
            "per-image": model("per-image", json!({"cost": {"text": {"input": {"unit": "image", "price": 1}, "output": {"price": 2}}}})),
            "missing-output": model("missing-output", json!({"cost": {"text": {"input": {"price": 1}}}})),
            "broken": {"id": "broken", "type": "chat"},
            "video": model("video", json!({"type": "video"})),
            "image-no-entry": model("image-no-entry", json!({"type": "image"})),
            "image": model("image", json!({"type": "image", "api": {"images": {"base_url": "https://api.acme.test/v1", "sdk": {"opencodeai": "@opencode/ai/providers/openai-compatible/images"}}}})),
            "unknown-entry": model("unknown-entry", json!({"api": {"x": {"sdk": {"opencodeai": "@opencode/ai/providers/acme/next"}}, "y": {"base_url": "https://y.test", "sdk": {"aisdk": "@ai-sdk/anthropic"}}}})),
        }},
        "vertex": {"id": "google-vertex", "name": "Vertex", "env": ["GOOGLE_VERTEX_PROJECT"], "models": {
            "gemini": model("gemini", json!({"api": {"gemini": {"base_url": "https://aiplatform.googleapis.com", "sdk": {"opencodeai": "@opencode/ai/providers/google-vertex/gemini"}}}})),
            "glm": model("glm", json!({"api": {"chat-completions": {"base_url": "https://${GOOGLE_VERTEX_ENDPOINT}/v1/projects/${GOOGLE_VERTEX_PROJECT}/locations/${GOOGLE_VERTEX_LOCATION}/endpoints/openapi", "sdk": {"opencodeai": "@opencode/ai/providers/google-vertex/chat"}}}})),
        }},
        "empty": {"id": "empty", "name": "Empty", "env": [], "models": {"broken": {"id": 1}}},
        "invalid": {"id": "invalid", "name": 1, "env": [], "models": {}},
    });
    compare(&catalog.to_string());
}

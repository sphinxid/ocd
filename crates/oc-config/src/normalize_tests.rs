//! Port of `packages/core/test/config/normalization.test.ts`.

use serde_json::{Value, json};

use crate::decode::Decode;
use crate::normalize::{Diagnostic, DiagnosticKind, NormalizeResult, normalize};
use crate::schema::Info;

fn normalized(input: Value) -> (Value, Vec<Diagnostic>) {
    match normalize(&input) {
        NormalizeResult::Normalized { encoded, diagnostics } => (encoded, diagnostics),
        NormalizeResult::Rejected { .. } => panic!("expected normalized config"),
    }
}

fn paths(d: &[Diagnostic], kind: DiagnosticKind) -> Vec<Vec<String>> {
    d.iter().filter(|x| x.kind == kind).map(|x| x.path.clone()).collect()
}

fn pv(items: &[&[&str]]) -> Vec<Vec<String>> {
    items.iter().map(|p| p.iter().map(|s| s.to_string()).collect()).collect()
}

#[test]
fn accepts_tool_use_without_losing_statement_order() {
    let policies = json!([
        { "action": "tool.use", "effect": "deny", "resource": "shell:*" },
        { "action": "tool.use", "effect": "allow", "resource": "shell:git *" },
        { "action": "tool.use", "effect": "deny", "resource": "shell:git push *" },
    ]);
    let (encoded, d) = normalized(json!({ "experimental": { "policies": policies } }));
    assert!(d.is_empty());
    let info = Info::decode(&encoded).unwrap();
    assert_eq!(serde_json::to_value(info.policies.unwrap()).unwrap(), policies);
}

#[test]
fn drops_unsupported_policy_actions() {
    let (encoded, d) = normalized(json!({ "experimental": { "policies": [
        { "action": "permission", "effect": "allow", "resource": "*" },
        { "action": "tool.use", "effect": "deny", "resource": "shell:*" },
        { "action": "future.use", "effect": "deny", "resource": "*" },
    ]}}));
    assert_eq!(encoded["policies"], json!([{ "action": "tool.use", "resource": "shell:*", "effect": "deny" }]));
    assert_eq!(
        d.iter().map(|x| x.path.clone()).collect::<Vec<_>>(),
        pv(&[&["experimental", "policies", "0"], &["experimental", "policies", "2"]])
    );
}

#[test]
fn rejects_every_non_object_root() {
    for input in [json!(null), json!([]), json!("config"), json!(true), json!(1)] {
        assert_eq!(
            normalize(&input),
            NormalizeResult::Rejected {
                diagnostics: vec![Diagnostic {
                    kind: DiagnosticKind::Invalid,
                    path: vec!["$".into()],
                    message: "rejected configuration because its root is not an object",
                }]
            }
        );
    }
}

#[test]
fn keeps_unrelated_native_fields_with_legacy_field() {
    let (encoded, _) = normalized(json!({ "snapshot": false, "agents": { "reviewer": { "system": "Use V2" } } }));
    let info = Info::decode(&encoded).unwrap();
    assert_eq!(info.snapshots, Some(false));
    assert_eq!(info.agents.unwrap()["reviewer"].system.as_deref(), Some("Use V2"));
}

#[test]
fn canonicalizes_warming_durations() {
    let (encoded, _) = normalized(json!({ "warming": { "interval": "4 minutes", "duration": "30 minutes" } }));
    assert_eq!(encoded["warming"], json!({ "interval": "240000 millis", "duration": "1800000 millis" }));
    let info = Info::decode(&encoded).unwrap();
    let Some(crate::schema::Warming::Settings(w)) = info.warming else { panic!() };
    assert_eq!(w.interval.unwrap().to_millis(), 240_000.0);
    assert_eq!(w.duration.unwrap().to_millis(), 1_800_000.0);
}

#[test]
fn merges_named_maps_native_precedence() {
    let (encoded, d) = normalized(json!({
        "reference": { "legacy": { "path": "../legacy" }, "duplicate": { "path": "../old" } },
        "references": { "native": { "path": "../native" }, "duplicate": { "path": "../new" } },
        "command": { "legacy": { "template": "legacy" }, "duplicate": { "template": "old" } },
        "commands": { "native": { "template": "native" }, "duplicate": { "template": "new" } },
    }));
    assert_eq!(
        serde_json::to_string(&encoded["references"]).unwrap(),
        r#"{"legacy":{"path":"../legacy"},"duplicate":{"path":"../new"},"native":{"path":"../native"}}"#
    );
    assert_eq!(
        encoded["commands"],
        json!({ "legacy": { "template": "legacy" }, "native": { "template": "native" }, "duplicate": { "template": "new" } })
    );
    assert_eq!(paths(&d, DiagnosticKind::Conflict), pv(&[&["references", "duplicate"], &["commands", "duplicate"]]));
}

#[test]
fn canonical_equal_duplicates_are_not_conflicts() {
    let (_, d) = normalized(json!({
        "snapshot": false, "snapshots": false,
        "reference": { "docs": { "path": "../docs" } }, "references": { "docs": { "path": "../docs" } },
        "agent": { "reviewer": { "prompt": "same" } }, "agents": { "reviewer": { "system": "same" } },
        "provider": { "custom": { "name": "same" } }, "providers": { "custom": { "name": "same" } },
        "compaction": { "preserve_recent_tokens": 1000, "keep": { "tokens": 1000 } },
    }));
    assert!(paths(&d, DiagnosticKind::Conflict).is_empty(), "{d:?}");
}

#[test]
fn agent_then_mode_then_native_precedence() {
    let (encoded, d) = normalized(json!({
        "agent": { "reviewer": { "prompt": "agent" }, "agentOnly": { "prompt": "agent-only" } },
        "mode": { "reviewer": { "prompt": "mode" }, "modeOnly": { "prompt": "mode-only" } },
        "agents": { "reviewer": { "system": "native" }, "nativeOnly": { "system": "native-only" } },
    }));
    assert_eq!(
        encoded["agents"],
        json!({
            "reviewer": { "system": "native" },
            "agentOnly": { "system": "agent-only" },
            "modeOnly": { "system": "mode-only", "mode": "primary" },
            "nativeOnly": { "system": "native-only" },
        })
    );
    assert_eq!(paths(&d, DiagnosticKind::Conflict), pv(&[&["agents", "reviewer"], &["agents", "reviewer"]]));
    assert!(Info::decode(&encoded).is_some());
}

#[test]
fn renames_legacy_provider_ids_in_model() {
    assert_eq!(
        normalized(json!({ "model": "google-vertex-anthropic/claude#high" })).0["model"],
        json!({ "providerID": "google-vertex", "model": "claude", "variant": "high" })
    );
    assert_eq!(
        normalized(json!({ "model": { "providerID": "azure-cognitive-services", "model": "deployment" } })).0["model"],
        json!({ "providerID": "azure", "model": "deployment" })
    );
    assert_eq!(
        normalized(json!({ "model": "anthropic/claude" })).0["model"],
        json!({ "providerID": "anthropic", "model": "claude" })
    );
}

#[test]
fn small_model_migrates_to_title_agent() {
    let (encoded, d) = normalized(json!({ "small_model": "anthropic/claude-haiku-4-5" }));
    assert_eq!(encoded["agents"], json!({ "title": { "model": { "providerID": "anthropic", "model": "claude-haiku-4-5" } } }));
    assert!(d.is_empty());
    let (encoded, d) = normalized(json!({
        "small_model": "anthropic/claude-haiku-4-5",
        "agent": { "title": { "prompt": "Custom title prompt" } },
    }));
    assert_eq!(
        encoded["agents"],
        json!({ "title": { "model": { "providerID": "anthropic", "model": "claude-haiku-4-5" }, "system": "Custom title prompt" } })
    );
    assert!(d.is_empty());
}

#[test]
fn invalid_small_model_is_omitted_without_value() {
    let secret = "do-not-log-this-value";
    let (encoded, d) = normalized(json!({ "small_model": secret }));
    assert!(encoded.get("agents").is_none());
    assert_eq!(paths(&d, DiagnosticKind::Unsupported), pv(&[&["small_model"]]));
    assert!(!serde_json::to_string(&d).unwrap().contains(secret));
}

#[test]
fn recovers_malformed_named_entries() {
    let (encoded, d) = normalized(json!({
        "command": { "fallback": { "template": "legacy" } },
        "commands": { "fallback": { "template": 1 }, "valid": { "template": "native" }, "invalid": { "template": false } },
        "providers": { "valid": { "name": "Valid" }, "invalid": { "env": [1] } },
    }));
    assert_eq!(encoded["commands"], json!({ "fallback": { "template": "legacy" }, "valid": { "template": "native" } }));
    assert_eq!(encoded["providers"], json!({ "valid": { "name": "Valid" } }));
    assert_eq!(
        paths(&d, DiagnosticKind::Invalid),
        pv(&[&["commands", "fallback"], &["commands", "invalid"], &["providers", "invalid"]])
    );
}

#[test]
fn partial_capabilities_accepted() {
    let input = json!({ "providers": { "demo": { "name": "Demo", "models": { "demo": { "name": "Demo", "capabilities": { "input": ["text"], "output": ["text"] } } } } } });
    let (encoded, d) = normalized(input.clone());
    assert!(d.is_empty());
    assert_eq!(encoded["providers"], input["providers"]);
}

#[test]
fn retired_alias_used_when_canonical_malformed() {
    let (encoded, d) = normalized(json!({
        "provider": { "azure-cognitive-services": { "models": { "deployment": {} } }, "azure": { "env": [1] } },
    }));
    assert!(encoded["providers"]["azure"]["models"].get("deployment").is_some());
    assert!(paths(&d, DiagnosticKind::Invalid).contains(&pv(&[&["provider", "azure"]])[0]));
}

#[test]
fn permission_source_order_and_native_append() {
    let (encoded, _) = normalized(json!({
        "tools": { "bash": true, "write": false },
        "permission": { "read": "allow", "custom": { "first": "deny", "second": "ask" }, "task": "allow" },
        "permissions": [{ "action": "native", "resource": "*", "effect": "deny" }],
    }));
    assert_eq!(
        encoded["permissions"],
        json!([
            { "action": "shell", "resource": "*", "effect": "allow" },
            { "action": "edit", "resource": "*", "effect": "deny" },
            { "action": "read", "resource": "*", "effect": "allow" },
            { "action": "custom", "resource": "first", "effect": "deny" },
            { "action": "custom", "resource": "second", "effect": "ask" },
            { "action": "subagent", "resource": "*", "effect": "allow" },
            { "action": "native", "resource": "*", "effect": "deny" },
        ])
    );
}

#[test]
fn legacy_agent_and_mode_permission_order() {
    for key in ["agent", "mode"] {
        let build = json!({
            "tools": { "write": false, "read": true },
            "permission": { "*": "allow", "bash": "ask", "custom": "deny", "edit": "allow" },
        });
        let (encoded, _) = normalized(json!({ key: { "build": build } }));
        assert_eq!(
            encoded["agents"]["build"]["permissions"],
            json!([
                { "action": "edit", "resource": "*", "effect": "allow" },
                { "action": "read", "resource": "*", "effect": "allow" },
                { "action": "*", "resource": "*", "effect": "allow" },
                { "action": "shell", "resource": "*", "effect": "ask" },
                { "action": "custom", "resource": "*", "effect": "deny" },
            ])
        );
    }
}

#[test]
fn permission_resource_keys_redacted_from_diagnostics() {
    let (_, d) = normalized(json!({ "permission": { "bash": { "curl -H Authorization:Bearer TOPSECRET *": "bogus" } } }));
    assert_eq!(
        d,
        vec![Diagnostic {
            kind: DiagnosticKind::Invalid,
            path: vec!["permission".into(), "bash".into(), "0".into()],
            message: "skipped malformed recognized value",
        }]
    );
    assert!(!serde_json::to_string(&d).unwrap().contains("TOPSECRET"));
}

#[test]
fn recovers_list_items() {
    let (encoded, d) = normalized(json!({
        "skills": { "paths": ["./skills", 1], "urls": [false, "https://example.com/skills"] },
        "instructions": ["one", 2, "three"],
        "permissions": [
            { "action": "read", "resource": "*", "effect": "allow" },
            { "action": "read", "resource": "*", "effect": "invalid" },
        ],
    }));
    assert_eq!(encoded["skills"], json!(["./skills", "https://example.com/skills"]));
    assert_eq!(encoded["instructions"], json!(["one", "three"]));
    assert_eq!(encoded["permissions"], json!([{ "action": "read", "resource": "*", "effect": "allow" }]));
    assert_eq!(paths(&d, DiagnosticKind::Invalid).len(), 4);
}

#[test]
fn omits_malformed_collection_roots() {
    let (encoded, d) = normalized(json!({
        "commands": [], "providers": "invalid", "references": false, "agents": 1, "permissions": {}, "instructions": {},
    }));
    assert_eq!(encoded, json!({}));
    assert_eq!(
        paths(&d, DiagnosticKind::Invalid),
        pv(&[&["references"], &["commands"], &["agents"], &["providers"], &["permissions"], &["instructions"]])
    );
}

#[test]
fn formatter_and_lsp_maps() {
    let (encoded, d) = normalized(json!({
        "formatter": { "prettier": { "command": [1] } },
        "lsp": { "typescript": { "command": [1] } },
    }));
    assert!(encoded.get("formatter").is_none());
    assert!(encoded.get("lsp").is_none());
    assert_eq!(paths(&d, DiagnosticKind::Invalid), pv(&[&["formatter", "prettier"], &["lsp", "typescript"]]));
    let (encoded, _) = normalized(json!({ "formatter": {}, "lsp": {} }));
    assert_eq!(encoded["formatter"], json!({}));
    assert_eq!(encoded["lsp"], json!({}));
    let (encoded, _) = normalized(json!({ "lsp": { "ts": { "disabled": true, "command": ["x"] } }, "formatter": true }));
    assert_eq!(encoded["lsp"], json!({ "ts": { "disabled": true } }));
    assert_eq!(encoded["formatter"], json!(true));
}

#[test]
fn combines_legacy_and_native_mcp() {
    let (encoded, d) = normalized(json!({
        "experimental": { "mcp_timeout": 5000 },
        "mcp": {
            "legacy": { "type": "local", "command": ["legacy"] },
            "duplicate": { "type": "remote", "url": "https://legacy.example.com" },
            "servers": {
                "native": { "type": "local", "command": ["native"] },
                "duplicate": { "type": "remote", "url": "https://native.example.com" },
                "invalid": { "type": "local", "command": [1] },
            },
            "timeout": { "startup": 1000, "catalog": 6000 },
        },
    }));
    assert_eq!(
        encoded["mcp"],
        json!({
            "timeout": { "catalog": 6000, "execution": 5000, "startup": 1000 },
            "servers": {
                "legacy": { "type": "local", "command": ["legacy"] },
                "duplicate": { "type": "remote", "url": "https://native.example.com" },
                "native": { "type": "local", "command": ["native"] },
            },
        })
    );
    let has = |kind, path: &str| d.iter().any(|x| x.kind == kind && x.path.join(".") == path);
    assert!(has(DiagnosticKind::Conflict, "mcp.servers.duplicate"));
    assert!(has(DiagnosticKind::Conflict, "mcp.timeout.catalog"));
    assert!(has(DiagnosticKind::Invalid, "mcp.servers.invalid"));
}

#[test]
fn raw_mcp_discriminators_for_reserved_names() {
    let (encoded, _) = normalized(json!({ "mcp": {
        "servers": { "type": "local", "command": ["reserved-servers"] },
        "timeout": { "type": "remote", "url": "https://reserved.example.com" },
    }}));
    assert_eq!(
        encoded["mcp"]["servers"],
        json!({
            "servers": { "type": "local", "command": ["reserved-servers"] },
            "timeout": { "type": "remote", "url": "https://reserved.example.com" },
        })
    );
    let (encoded, d) = normalized(json!({ "mcp": { "servers": { "enabled": true }, "timeout": { "enabled": false } } }));
    assert!(encoded.get("mcp").is_none());
    assert_eq!(paths(&d, DiagnosticKind::Unsupported), pv(&[&["mcp", "servers"], &["mcp", "timeout"]]));
}

#[test]
fn legacy_mcp_mapping() {
    let (encoded, _) = normalized(json!({ "mcp": {
        "remote": { "type": "remote", "url": "https://x", "enabled": false, "timeout": 10,
                    "oauth": { "clientId": "id", "callbackPort": 1234 } },
        "nooauth": { "type": "remote", "url": "https://y", "oauth": false },
    }}));
    assert_eq!(
        serde_json::to_string(&encoded["mcp"]["servers"]).unwrap(),
        r#"{"remote":{"type":"remote","url":"https://x","oauth":{"client_id":"id","callback_port":1234},"disabled":true,"timeout":{"startup":10,"catalog":10,"execution":10}},"nooauth":{"type":"remote","url":"https://y","oauth":false}}"#
    );
}

#[test]
fn mcp_timeout_schema_order_and_recovery() {
    let (encoded, d) = normalized(json!({ "mcp": { "timeout": { "execution": 3000, "startup": "invalid", "catalog": 2000 } } }));
    assert_eq!(serde_json::to_string(&encoded["mcp"]).unwrap(), r#"{"timeout":{"catalog":2000,"execution":3000}}"#);
    assert_eq!(d.iter().map(|x| (x.kind, x.path.clone())).collect::<Vec<_>>(), vec![(
        DiagnosticKind::Invalid,
        pv(&[&["mcp", "timeout", "startup"]])[0].clone()
    )]);
    assert!(normalized(json!({ "mcp": { "timeout": {} } })).0.get("mcp").is_none());
    let (encoded, d) = normalized(json!({ "mcp": { "timeout": { "unknown": 1000 } } }));
    assert!(encoded.get("mcp").is_none());
    assert_eq!(paths(&d, DiagnosticKind::Invalid), pv(&[&["mcp", "timeout"]]));
    assert_eq!(normalized(json!({ "mcp": {} })).0["mcp"], json!({}));
}

#[test]
fn compaction_leaves() {
    let (encoded, d) = normalized(json!({ "compaction": {
        "auto": false, "preserve_recent_tokens": 1000, "keep": { "tokens": 2000 },
        "reserved": 3000, "buffer": 4000, "tail_turns": 2, "prune": true,
    }}));
    assert_eq!(encoded["compaction"], json!({ "auto": false, "keep": { "tokens": 2000 }, "buffer": 4000 }));
    assert_eq!(
        d.iter().map(|x| (x.kind, x.path.clone())).collect::<Vec<_>>(),
        vec![
            (DiagnosticKind::Unsupported, pv(&[&["compaction", "tail_turns"]])[0].clone()),
            (DiagnosticKind::Unsupported, pv(&[&["compaction", "prune"]])[0].clone()),
            (DiagnosticKind::Conflict, pv(&[&["compaction", "keep", "tokens"]])[0].clone()),
            (DiagnosticKind::Conflict, pv(&[&["compaction", "buffer"]])[0].clone()),
        ]
    );
}

#[test]
fn enabled_provider_lists() {
    assert_eq!(
        normalized(json!({ "enabled_providers": [] })).0["policies"],
        json!([{ "action": "provider.use", "resource": "*", "effect": "deny" }])
    );
    assert_eq!(
        normalized(json!({ "enabled_providers": [1, "anthropic", false] })).0["policies"],
        json!([
            { "action": "provider.use", "resource": "*", "effect": "deny" },
            { "action": "provider.use", "resource": "anthropic", "effect": "allow" },
        ])
    );
    assert!(normalized(json!({ "enabled_providers": [1, false] })).0.get("policies").is_none());
    assert!(normalized(json!({ "enabled_providers": "anthropic" })).0.get("policies").is_none());
}

// "merges migrated, experimental, and native policies in order"
#[test]
fn native_policies_after_migrated() {
    let encoded = normalized(json!({
        "enabled_providers": ["anthropic"],
        "disabled_providers": ["openai"],
        "policies": [{ "action": "provider.use", "resource": "native", "effect": "deny" }],
        "experimental": {
            "portable_shell_scanner": true, "todo": true, "background_compaction": true, "subagent_depth": 0,
            "location_idle_timeout": "2 hours",
            "policies": [{ "action": "provider.use", "resource": "custom", "effect": "allow" }],
        },
    }))
    .0;
    assert_eq!(
        encoded["experimental"],
        json!({
            "portable_shell_scanner": true,
            "todo": true,
            "background_compaction": true,
            "subagent_depth": 0,
            "location_idle_timeout": "7200000 millis",
        })
    );
    assert_eq!(
        encoded["policies"],
        json!([
            { "action": "provider.use", "resource": "*", "effect": "deny" },
            { "action": "provider.use", "resource": "anthropic", "effect": "allow" },
            { "action": "provider.use", "resource": "openai", "effect": "deny" },
            { "action": "provider.use", "resource": "custom", "effect": "allow" },
            { "action": "provider.use", "resource": "native", "effect": "deny" },
        ])
    );
    let info = Info::decode(&encoded).unwrap();
    assert_eq!(serde_json::to_value(&info.experimental).unwrap()["location_idle_timeout"], json!("7200000 millis"));
}

#[test]
fn never_emits_experimental_policies() {
    assert_eq!(
        normalized(json!({ "experimental": { "policies": [{ "action": "provider.use", "resource": "*", "effect": "deny" }] } })).0,
        json!({ "policies": [{ "action": "provider.use", "resource": "*", "effect": "deny" }] })
    );
}

#[test]
fn expands_numeric_mcp_timeouts_to_every_phase() {
    let (encoded, d) = normalized(json!({ "mcp": {
        "timeout": 4000,
        "servers": {
            "local": { "type": "local", "command": ["local"], "timeout": 6000 },
            "remote": { "type": "remote", "url": "https://remote.example.com", "timeout": 8000 },
            "structured": { "type": "local", "command": ["structured"], "timeout": { "catalog": 9000 } },
            "invalid": { "type": "local", "command": ["invalid"], "timeout": -1 },
        },
    }}));
    assert_eq!(
        encoded["mcp"],
        json!({
            "timeout": { "startup": 4000, "catalog": 4000, "execution": 4000 },
            "servers": {
                "local": { "type": "local", "command": ["local"], "timeout": { "startup": 6000, "catalog": 6000, "execution": 6000 } },
                "remote": { "type": "remote", "url": "https://remote.example.com", "timeout": { "startup": 8000, "catalog": 8000, "execution": 8000 } },
                "structured": { "type": "local", "command": ["structured"], "timeout": { "catalog": 9000 } },
            },
        })
    );
    assert_eq!(
        d.iter().map(|x| (x.kind, x.path.clone())).collect::<Vec<_>>(),
        vec![(DiagnosticKind::Invalid, pv(&[&["mcp", "servers", "invalid"]])[0].clone())]
    );
}

#[test]
fn unsupported_legacy_settings_without_values() {
    let secret = "do-not-log-this-value";
    let (_, d) = normalized(json!({
        "logLevel": "DEBUG",
        "agent": { "reviewer": { "name": secret, "prompt": "review" } },
        "provider": { "custom": {
            "id": secret, "whitelist": ["model"],
            "models": { "model": { "release_date": secret, "status": "active", "interleaved": true } },
        }},
        "experimental": { "openTelemetry": true },
    }));
    assert_eq!(
        paths(&d, DiagnosticKind::Unsupported),
        pv(&[
            &["logLevel"],
            &["agent", "reviewer", "name"],
            &["provider", "custom", "id"],
            &["provider", "custom", "whitelist"],
            &["provider", "custom", "models", "model", "release_date"],
            &["provider", "custom", "models", "model", "status"],
            &["provider", "custom", "models", "model", "interleaved"],
            &["experimental", "openTelemetry"],
        ])
    );
    assert!(!serde_json::to_string(&d).unwrap().contains(secret));
}

#[test]
fn unsupported_legacy_model_selections() {
    let (encoded, d) = normalized(json!({
        "command": {
            "invalidModel": { "template": "one", "model": "invalid" },
            "invalidVariant": { "template": "two", "model": "anthropic/model", "variant": "bad#variant" },
            "missingModel": { "template": "three", "variant": "high" },
        },
        "agent": { "invalid": { "prompt": "agent", "model": "invalid", "variant": "" } },
    }));
    let keys: Vec<_> = encoded["commands"].as_object().unwrap().keys().cloned().collect();
    assert_eq!(keys, vec!["invalidModel", "invalidVariant", "missingModel"]);
    assert_eq!(
        paths(&d, DiagnosticKind::Unsupported),
        pv(&[
            &["command", "invalidModel", "model"],
            &["command", "invalidVariant", "variant"],
            &["command", "missingModel", "variant"],
            &["agent", "invalid", "model"],
            &["agent", "invalid", "variant"],
        ])
    );
}

#[test]
fn thinking_block_binding_opt_out() {
    let (encoded, _) = normalized(json!({ "provider": { "gateway": { "models": {
        "anthropic": { "options": { "thinking": { "type": "adaptive", "blockBinding": false }, "effort": "high" } },
        "bedrock": { "options": { "reasoningConfig": { "blockBinding": false } } },
        "both": { "options": {
            "thinking": { "type": "adaptive", "blockBinding": false },
            "reasoningConfig": { "type": "adaptive", "blockBinding": false },
        }},
        "untouched": { "options": { "thinking": { "type": "adaptive" } } },
    }}}}));
    let models = &encoded["providers"]["gateway"]["models"];
    assert_eq!(models["anthropic"]["compatibility"], json!({ "supportsThinkingBlockBinding": false }));
    assert_eq!(models["anthropic"]["settings"], json!({ "thinking": { "type": "adaptive" }, "effort": "high" }));
    assert_eq!(models["bedrock"]["settings"], json!({}));
    assert_eq!(
        models["both"]["settings"],
        json!({ "thinking": { "type": "adaptive" }, "reasoningConfig": { "type": "adaptive" } })
    );
    assert_eq!(models["untouched"]["settings"], json!({ "thinking": { "type": "adaptive" } }));
    assert!(models["untouched"].get("compatibility").is_none());
}

#[test]
fn invalid_provider_overlays_skip_only_that_provider() {
    let (encoded, d) = normalized(json!({ "provider": {
        "headers": { "options": { "headers": { "valid": "yes", "invalid": 1 } } },
        "body": { "options": { "body": "not-an-object" } },
        "valid": { "options": { "headers": { "valid": "yes" }, "body": { "trace": true } } },
    }}));
    assert_eq!(
        encoded["providers"],
        json!({ "valid": { "settings": {}, "headers": { "valid": "yes" }, "body": { "trace": true } } })
    );
    assert_eq!(
        paths(&d, DiagnosticKind::Invalid),
        pv(&[&["provider", "headers", "options", "headers"], &["provider", "body", "options", "body"]])
    );
}

#[test]
fn standard_provider_migration() {
    let (encoded, _) = normalized(json!({ "provider": {
        "custom": {
            "npm": "@ai-sdk/openai-compatible", "api": "https://api.example.com", "name": "Custom",
            "options": { "baseURL": "x", "apiKey": "k", "extra": 1 },
            "models": { "m": {
                "id": "real", "tool_call": false, "interleaved": { "field": "reasoning_content" },
                "cost": { "input": 1, "output": 2, "cache_read": 0.5, "context_over_200k": { "input": 3, "output": 4 } },
                "limit": { "context": 1000.7, "output": 10 },
                "status": "deprecated",
                "variants": { "high": { "effort": "high" } },
                "provider": { "npm": "@ai-sdk/anthropic" },
            }},
        },
        "azure-cognitive-services": { "npm": "@ai-sdk/openai-compatible", "env": ["AZURE_COGNITIVE_SERVICES_RESOURCE_NAME", "KEY"] },
        "google-vertex-anthropic": { "models": { "claude": {} } },
    }}));
    let p = &encoded["providers"];
    assert_eq!(p["custom"]["package"], json!("aisdk:@ai-sdk/openai-compatible"));
    assert_eq!(
        serde_json::to_string(&p["custom"]["settings"]).unwrap(),
        r#"{"apiKey":"k","baseURL":"https://api.example.com","extra":1}"#
    );
    let m = &p["custom"]["models"]["m"];
    assert_eq!(m["modelID"], json!("real"));
    assert_eq!(m["compatibility"], json!({ "reasoningField": "reasoning_content" }));
    assert_eq!(m["package"], json!("aisdk:@ai-sdk/anthropic"));
    assert_eq!(m["capabilities"], json!({ "tools": false, "input": ["text", "image"], "output": ["text"] }));
    assert_eq!(m["variants"], json!([{ "id": "high", "settings": { "effort": "high" } }]));
    assert_eq!(
        m["cost"],
        json!([
            { "input": 1, "output": 2, "cache": { "read": 0.5 } },
            { "tier": { "type": "context", "size": 200000 }, "input": 3, "output": 4, "cache": {} },
        ])
    );
    assert_eq!(m["disabled"], json!(true));
    assert_eq!(m["limit"], json!({ "context": 1000, "output": 10 }));
    assert_eq!(p["azure"]["env"], json!(["KEY"]));
    assert_eq!(
        p["azure"]["settings"]["baseURL"],
        json!("https://${AZURE_COGNITIVE_SERVICES_RESOURCE_NAME}.cognitiveservices.azure.com/openai")
    );
    assert!(p["google-vertex"].get("package").is_none());
    assert_eq!(p["google-vertex"]["models"]["claude"]["package"], json!("aisdk:@ai-sdk/google-vertex/anthropic"));
}

#[test]
fn legacy_agent_migration_fields() {
    let (encoded, _) = normalized(json!({ "agent": { "a": {
        "model": "anthropic/claude", "variant": "high", "temperature": 0.2, "top_p": 1,
        "prompt": "p", "color": "primary", "maxSteps": 3, "disable": true, "custom": "x",
        "options": { "o": 1 },
    }}}));
    assert_eq!(
        serde_json::to_string(&encoded["agents"]["a"]).unwrap(),
        r##"{"model":{"providerID":"anthropic","model":"claude","variant":"high"},"request":{"body":{"o":1,"custom":"x","temperature":0.2,"top_p":1}},"system":"p","color":"#aaaaaa","steps":3,"disabled":true}"##
    );
}

#[test]
fn explicit_empty_values_preserved() {
    let (encoded, _) = normalized(json!({
        "snapshot": false, "autoshare": false, "references": {}, "commands": {}, "agents": {}, "providers": {},
        "instructions": [], "experimental": { "subagent_depth": 0, "location_idle_timeout": false },
    }));
    assert_eq!(encoded["snapshots"], json!(false));
    for key in ["references", "commands", "agents", "providers"] {
        assert_eq!(encoded[key], json!({}));
    }
    assert_eq!(encoded["instructions"], json!([]));
    assert_eq!(encoded["experimental"], json!({ "subagent_depth": 0, "location_idle_timeout": false }));
    assert!(encoded.get("share").is_none());
}

#[test]
fn autoupdate_and_plugins() {
    let (encoded, d) = normalized(json!({ "autoupdate": "notify", "autoshare": true, "plugin": ["a", ["b", { "x": 1 }]], "plugins": ["-c"] }));
    assert_eq!(encoded["update"], json!("notify"));
    assert_eq!(encoded["share"], json!("auto"));
    assert_eq!(encoded["plugins"], json!(["a", { "package": "b", "options": { "x": 1 } }, "-c"]));
    assert!(d.is_empty());
    let (encoded, d) = normalized(json!({ "autoupdate": false, "update": "auto" }));
    assert_eq!(encoded["update"], json!("auto"));
    assert_eq!(paths(&d, DiagnosticKind::Conflict), pv(&[&["update"]]));
}

#[test]
fn schema_is_metadata_and_worktree_trimmed() {
    let (encoded, _) = normalized(json!({ "$schema": "https://opencode.ai/config.json", "worktree": { "directory": "  ../wt " } }));
    assert_eq!(encoded["worktree"], json!({ "directory": "../wt" }));
    assert!(normalized(json!({ "worktree": { "directory": "   " } })).1.len() == 1);
}

// --- config.test.ts migration cases ---------------------------------------------------------------

fn migrate_v1(input: Value) -> Info {
    Info::decode(&normalized(input).0).expect("valid v2 configuration")
}

#[test]
fn decodes_configuration_maps_in_source_order() {
    let keys = ["zeta", "build", "alpha", "plan", "mid"];
    let map = |v: Value| Value::Object(keys.iter().map(|k| (k.to_string(), v.clone())).collect());
    let info = migrate_v1(json!({
        "agents": map(json!({})),
        "formatter": map(json!({ "command": ["fmt"], "extensions": [".ts"] })),
        "providers": map(json!({})),
        "commands": map(json!({ "template": "run" })),
    }));
    assert_eq!(info.agents.unwrap().keys().cloned().collect::<Vec<_>>(), keys);
    let Some(crate::schema::Formatter::Entries(f)) = info.formatter else { panic!() };
    assert_eq!(f.keys().cloned().collect::<Vec<_>>(), keys);
    assert_eq!(info.providers.unwrap().keys().cloned().collect::<Vec<_>>(), keys);
    assert_eq!(info.commands.unwrap().keys().cloned().collect::<Vec<_>>(), keys);
}

#[test]
fn decodes_v1_permissions_in_source_order() {
    use crate::legacy::{PermissionRuleV1::*, decode_permission};
    assert_eq!(
        decode_permission(&json!({ "*": "allow", "custom": "deny", "bash": { "git *": "ask", "*": "deny" }, "edit": "deny" })).unwrap(),
        vec![
            ("*".to_string(), Action("allow".into())),
            ("custom".to_string(), Action("deny".into())),
            ("bash".to_string(), Entries(vec![("git *".into(), "ask".into()), ("*".into(), "deny".into())])),
            ("edit".to_string(), Action("deny".into())),
        ]
    );
    assert_eq!(decode_permission(&json!("ask")).unwrap(), vec![("*".to_string(), Action("ask".into()))]);
    for invalid in [json!(["allow"]), json!({ "bash": ["allow"] }), json!({ "question": { "*": "allow" } }), json!({ "read": null })] {
        assert!(decode_permission(&invalid).is_none(), "{invalid}");
    }
}

#[test]
fn migrates_v1_scalars() {
    assert_eq!(migrate_v1(json!({ "experimental": { "subagent_depth": 2 } })).experimental.unwrap().subagent_depth, Some(2));
    use crate::schema::UpdatePolicy::*;
    assert_eq!(migrate_v1(json!({ "autoupdate": false })).update, Some(Disable));
    assert_eq!(migrate_v1(json!({ "autoupdate": "notify" })).update, Some(Notify));
    assert_eq!(migrate_v1(json!({ "autoupdate": true })).update, Some(Auto));
    assert_eq!(migrate_v1(json!({})).update, None);
    assert_eq!(
        normalize(&json!({ "update": "auto" })),
        NormalizeResult::Normalized { encoded: json!({ "update": "auto" }), diagnostics: vec![] }
    );
}

#[test]
fn migrates_v1_provider_lists_to_policies() {
    let e = normalized(json!({ "enabled_providers": ["anthropic", "openai"], "disabled_providers": ["openai"] })).0;
    assert_eq!(e["policies"], json!([
        { "action": "provider.use", "resource": "*", "effect": "deny" },
        { "action": "provider.use", "resource": "anthropic", "effect": "allow" },
        { "action": "provider.use", "resource": "openai", "effect": "allow" },
        { "action": "provider.use", "resource": "openai", "effect": "deny" },
    ]));
}

#[test]
fn migrates_v1_provider_setup_options_into_aisdk_settings() {
    let e = normalized(json!({ "provider": { "bedrock": {
        "npm": "@ai-sdk/amazon-bedrock",
        "models": { "claude": { "provider": { "npm": "@ai-sdk/anthropic" } } },
        "options": { "headers": { "x-test": "1" }, "body": { "trace": true }, "region": "us-east-1", "profile": "dev" },
    }}}))
    .0;
    let b = &e["providers"]["bedrock"];
    assert_eq!(b["package"], json!("aisdk:@ai-sdk/amazon-bedrock"));
    assert_eq!(b["models"]["claude"]["package"], json!("aisdk:@ai-sdk/anthropic"));
    assert_eq!(b["settings"], json!({ "region": "us-east-1", "profile": "dev" }));
    assert_eq!(b["headers"], json!({ "x-test": "1" }));
    assert_eq!(b["body"], json!({ "trace": true }));
}

#[test]
fn renames_old_provider_ids_while_migrating() {
    let e = normalized(json!({
        "model": "azure-cognitive-services/deployment",
        "enabled_providers": ["google-vertex-anthropic"],
        "disabled_providers": ["azure-cognitive-services"],
        "agent": { "reviewer": { "model": "google-vertex-anthropic/claude-sonnet" } },
        "command": { "review": { "template": "Review", "model": "azure-cognitive-services/deployment" } },
        "provider": {
            "azure-cognitive-services": { "npm": "@ai-sdk/azure", "env": ["AZURE_COGNITIVE_SERVICES_RESOURCE_NAME", "AZURE_COGNITIVE_SERVICES_API_KEY"], "models": { "deployment": {} } },
            "google-vertex-anthropic": { "npm": "@ai-sdk/google-vertex/anthropic", "options": { "project": "test-project", "location": "us-central1" }, "models": { "claude-sonnet": {} } },
        },
    }))
    .0;
    assert_eq!(e["model"], json!({ "providerID": "azure", "model": "deployment" }));
    assert_eq!(e["agents"]["reviewer"]["model"], json!({ "providerID": "google-vertex", "model": "claude-sonnet" }));
    assert_eq!(e["commands"]["review"]["model"], json!({ "providerID": "azure", "model": "deployment" }));
    assert_eq!(e["policies"], json!([
        { "action": "provider.use", "resource": "*", "effect": "deny" },
        { "action": "provider.use", "resource": "google-vertex", "effect": "allow" },
        { "action": "provider.use", "resource": "azure", "effect": "deny" },
    ]));
    assert_eq!(e["providers"]["azure"]["env"], json!(["AZURE_COGNITIVE_SERVICES_API_KEY"]));
    assert_eq!(e["providers"]["azure"]["package"], json!("aisdk:@ai-sdk/azure"));
    assert!(e["providers"].get("azure-cognitive-services").is_none());
    let gv = &e["providers"]["google-vertex"];
    assert_eq!(gv["settings"], json!({ "project": "test-project", "location": "us-central1" }));
    assert_eq!(gv["models"]["claude-sonnet"]["package"], json!("aisdk:@ai-sdk/google-vertex/anthropic"));
    assert!(gv.get("package").is_none());
    assert!(e["providers"].get("google-vertex-anthropic").is_none());
}

#[test]
fn ignores_old_provider_ids_when_current_configured() {
    let e = normalized(json!({ "provider": {
        "azure": { "models": { "current": {} } },
        "azure-cognitive-services": { "models": { "legacy": {} } },
        "google-vertex": { "models": { "gemini": {} } },
        "google-vertex-anthropic": { "models": { "claude": {} } },
    }}))
    .0;
    assert_eq!(e["providers"]["azure"]["models"].as_object().unwrap().keys().collect::<Vec<_>>(), vec!["current"]);
    assert_eq!(e["providers"]["google-vertex"]["models"].as_object().unwrap().keys().collect::<Vec<_>>(), vec!["gemini"]);
}

#[test]
fn migrates_v1_interleaved_fields_to_compatibility() {
    let e = normalized(json!({ "provider": { "custom": { "models": {
        "object": { "interleaved": { "field": "vendor_reasoning" } },
        "string": { "interleaved": "reasoning_text" },
        "boolean": { "interleaved": true },
    }}}}))
    .0;
    let m = &e["providers"]["custom"]["models"];
    assert_eq!(m["object"]["compatibility"], json!({ "reasoningField": "vendor_reasoning" }));
    assert_eq!(m["string"]["compatibility"], json!({ "reasoningField": "reasoning_text" }));
    assert!(m["boolean"].get("compatibility").is_none());
}

#[test]
fn migrates_v1_command_configuration() {
    for subtask in [true, false] {
        let e = normalized(json!({ "command": { "review": {
            "template": "Review changes", "description": "Review code", "agent": "reviewer",
            "model": "anthropic/claude", "variant": "high", "subtask": subtask,
        }}}))
        .0;
        assert_eq!(e["commands"], json!({ "review": {
            "template": "Review changes", "description": "Review code", "agent": "reviewer",
            "model": { "providerID": "anthropic", "model": "claude", "variant": "high" }, "subagent": subtask,
        }}));
    }
}

#[test]
fn normalizes_renamed_permission_actions() {
    let e = normalized(json!({ "permission": { "task": "ask", "bash": { "git status": "allow", "*": "deny" }, "write": "deny", "apply_patch": "deny", "read": "allow" } })).0;
    assert_eq!(e["permissions"], json!([
        { "action": "subagent", "resource": "*", "effect": "ask" },
        { "action": "shell", "resource": "git status", "effect": "allow" },
        { "action": "shell", "resource": "*", "effect": "deny" },
        { "action": "edit", "resource": "*", "effect": "deny" },
        { "action": "edit", "resource": "*", "effect": "deny" },
        { "action": "read", "resource": "*", "effect": "allow" },
    ]));
}

#[test]
fn legacy_agent_options_exclude_schema_fields_and_name() {
    let agent = crate::legacy::decode_agent(&json!({
        "name": "reviewer", "model": "test/model", "variant": "high", "temperature": 0.5, "top_p": 0.9,
        "prompt": "Review carefully.", "tools": { "edit": false }, "disable": false, "description": "Reviews changes",
        "mode": "subagent", "hidden": true, "options": { "existing": true }, "color": "#112233", "steps": 10,
        "maxSteps": 20, "permission": { "read": "allow" }, "custom": "preserved",
    }))
    .unwrap();
    assert_eq!(Value::Object(agent.options), json!({ "existing": true, "custom": "preserved" }));
    assert_eq!(agent.steps, Some(10));
}

#[test]
fn migrates_the_legacy_set_cache_key_provider_option_into_provider_compatibility() {
    let (encoded, _) = normalized(json!({
        "provider": {
            "proxy": { "npm": "@ai-sdk/openai-compatible", "options": { "baseURL": "https://proxy.test/v1", "setCacheKey": true } },
            "openai": { "options": { "setCacheKey": false } },
            "untouched": { "options": { "baseURL": "https://untouched.test/v1" } }
        }
    }));
    let providers = &encoded["providers"];
    assert_eq!(providers["proxy"]["compatibility"], json!({ "supportsPromptCacheKey": true }));
    assert_eq!(providers["proxy"]["settings"], json!({ "baseURL": "https://proxy.test/v1" }));
    assert_eq!(providers["openai"]["compatibility"], json!({ "supportsPromptCacheKey": false }));
    assert!(providers["openai"]["settings"].get("setCacheKey").is_none());
    assert!(providers["untouched"].get("compatibility").is_none());
    // Schema order: compatibility follows package.
    let keys: Vec<&str> = providers["proxy"].as_object().unwrap().keys().map(String::as_str).collect();
    assert_eq!(keys, ["package", "compatibility", "settings"]);
}

#[test]
fn moves_openai_compatible_options_the_package_does_not_read_into_the_body() {
    use crate::legacy::compatible_model;
    let options = json!({
        "reasoningEffort": "high",
        "timeout": 1000,
        "allowed_openai_params": ["reasoning_effort"],
        "chat_template_kwargs": { "enable_thinking": true }
    });
    let (settings, body) = compatible_model(options.as_object().unwrap());
    assert_eq!(Value::Object(settings), json!({ "reasoningEffort": "high", "timeout": 1000 }));
    assert_eq!(
        body.map(Value::Object),
        Some(json!({ "allowed_openai_params": ["reasoning_effort"], "chat_template_kwargs": { "enable_thinking": true } }))
    );
    let (settings, body) = compatible_model(json!({ "reasoningEffort": "low" }).as_object().unwrap());
    assert_eq!(Value::Object(settings), json!({ "reasoningEffort": "low" }));
    assert_eq!(body, None);

    let (encoded, _) = normalized(json!({
        "provider": {
            "local": {
                "npm": "@ai-sdk/openai-compatible",
                "models": {
                    "qwen": {
                        "options": { "reasoningEffort": "high", "chat_template_kwargs": { "enable_thinking": true } },
                        "variants": { "fast": { "chat_template_kwargs": { "enable_thinking": false } } }
                    }
                }
            },
            "other": { "npm": "@ai-sdk/openai", "models": { "gpt": { "options": { "custom": 1 } } } }
        }
    }));
    let qwen = &encoded["providers"]["local"]["models"]["qwen"];
    assert_eq!(qwen["settings"], json!({ "reasoningEffort": "high" }));
    assert_eq!(qwen["body"], json!({ "chat_template_kwargs": { "enable_thinking": true } }));
    assert_eq!(qwen["variants"], json!([{ "id": "fast", "settings": {}, "body": { "chat_template_kwargs": { "enable_thinking": false } } }]));
    assert_eq!(encoded["providers"]["other"]["models"]["gpt"]["settings"], json!({ "custom": 1 }));
}

#[test]
fn keeps_built_in_provider_routes_native_when_migrating() {
    let (encoded, _) = normalized(json!({
        "provider": {
            "google-vertex": {
                "npm": "@opencode/ai/providers/google-vertex",
                "options": { "project": "my-project", "location": "europe-west4" },
                "models": { "chat": { "provider": { "npm": "@opencode-ai/ai/providers/google-vertex/mistral" } } }
            },
            "sdk": { "npm": "@ai-sdk/openai" }
        }
    }));
    assert_eq!(
        encoded["providers"]["google-vertex"],
        json!({
            "package": "@opencode/ai/providers/google-vertex",
            "settings": { "project": "my-project", "location": "europe-west4" },
            "models": { "chat": { "package": "@opencode/ai/providers/google-vertex/mistral" } }
        })
    );
    assert_eq!(encoded["providers"]["sdk"]["package"], "aisdk:@ai-sdk/openai");
}

#[test]
fn accepts_the_2_1_provider_and_model_fields() {
    let input = json!({
        "providers": {
            "custom": {
                "package": "@opencode/ai/providers/openai-compatible",
                "compatibility": { "streamUsage": false, "cacheFormat": "anthropic", "supportsEffortUpdates": false },
                "settings": { "baseURL": "https://custom.test/v1", "encoding": "zstd" },
                "models": {
                    "m": {
                        "name": "M",
                        "releaseDate": "2026-02",
                        "settings": { "encoding": "gzip" },
                        "capabilities": { "temperature": false, "reasoning": { "supported": true, "effort": ["low"], "budget": { "min": 1024 } } }
                    },
                    "n": { "capabilities": { "reasoning": { "supported": false, "effort": ["ignored"] } } }
                }
            }
        }
    });
    let (encoded, d) = normalized(input);
    assert!(d.is_empty(), "{d:?}");
    let custom = &encoded["providers"]["custom"];
    // Model-version features are model-only and not accepted on the provider.
    assert_eq!(custom["compatibility"], json!({ "streamUsage": false, "cacheFormat": "anthropic" }));
    assert_eq!(custom["settings"], json!({ "encoding": "zstd", "baseURL": "https://custom.test/v1" }));
    assert_eq!(custom["models"]["m"]["releaseDate"], "2026-02");
    assert_eq!(custom["models"]["m"]["settings"], json!({ "encoding": "gzip" }));
    assert_eq!(
        custom["models"]["m"]["capabilities"],
        json!({ "temperature": false, "reasoning": { "supported": true, "effort": ["low"], "budget": { "min": 1024 } } })
    );
    assert_eq!(custom["models"]["n"]["capabilities"], json!({ "reasoning": { "supported": false } }));
    for invalid in [
        json!({ "providers": { "bad": { "models": { "m": { "releaseDate": "Feb 2026" } } } } }),
        json!({ "providers": { "bad": { "settings": { "encoding": "brotli" } } } }),
        json!({ "providers": { "bad": { "compatibility": { "cacheFormat": "openai" } } } }),
    ] {
        let (encoded, d) = normalized(invalid);
        assert!(encoded["providers"].get("bad").is_none());
        assert_eq!(paths(&d, DiagnosticKind::Invalid), pv(&[&["providers", "bad"]]));
    }
}

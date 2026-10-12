//! Signature rendering, catalogs, and `toolExpression` against upstream recordings (signature.test.ts,
//! tool-paths.test.ts, catalog reads, and every tool tree seen by any recorded execution).

mod common;

use std::collections::HashSet;
use std::sync::Arc;

use common::*;
use oc_codemode::codemode::{CodeMode, ExecutionLimits};
use oc_codemode::host::NoTools;
use oc_codemode::tool_runtime::{signature_of, tool_expression, ToolDef, SEARCH_SIGNATURE};
use oc_codemode::tool_schema::{
    input_properties, is_empty_input, json_schema_to_typescript, output_typescript, schema_to_typescript, ToolSchema,
};
use serde_json::{json, Value as Json};

fn tool_def(tool: &Json, path: &str) -> ToolDef {
    ToolDef {
        path: path.to_string(),
        description: tool["description"].as_str().unwrap_or("").to_string(),
        input: schema_of(&tool["input"]),
        output: if tool["output"].is_null() { None } else { Some(schema_of(&tool["output"])) },
    }
}

fn properties_json(def: &ToolDef) -> Json {
    Json::Array(
        input_properties(&def.input)
            .into_iter()
            .map(|p| {
                let mut m = serde_json::Map::new();
                m.insert("name".into(), json!(p.name));
                if let Some(d) = p.description {
                    m.insert("description".into(), json!(d));
                }
                m.insert("required".into(), json!(p.required));
                Json::Object(m)
            })
            .collect(),
    )
}

fn check_tree(node: &Json, path: &mut Vec<String>, failures: &mut Vec<String>, seen: &mut HashSet<String>) {
    if let Some(tool) = node.get("tool") {
        let key = format!("{}|{}", path.join("."), tool);
        if seen.insert(key) {
            let p = path.join(".");
            let def = tool_def(tool, &p);
            let checks: Vec<(&str, Json, Json)> = vec![
                ("signature", json!(signature_of(&def)), tool["signature"].clone()),
                ("inputTs", json!(schema_to_typescript(&def.input, false)), tool["inputTs"].clone()),
                ("outputTs", json!(output_typescript(def.output.as_ref(), false)), tool["outputTs"].clone()),
                ("emptyInput", json!(is_empty_input(&def.input)), tool["emptyInput"].clone()),
                ("inputProperties", properties_json(&def), decode(&tool["inputProperties"]).unwrap_or(Json::Null)),
            ];
            for (what, ours, expected) in checks {
                if expected.get("$throws").is_some() {
                    continue;
                }
                if ours != expected {
                    failures.push(format!("{p} {what}:\n  expected {expected}\n  got      {ours}"));
                }
            }
        }
    }
    if let Some(Json::Array(children)) = node.get("children") {
        for child in children {
            path.push(child[0].as_str().unwrap().to_string());
            check_tree(&child[1], path, failures, seen);
            path.pop();
        }
    }
}

#[test]
fn recorded_tool_signatures_render_identically() {
    let mut failures = Vec::new();
    let mut seen = HashSet::new();
    let mut files: Vec<_> = std::fs::read_dir(fixtures()).unwrap().filter_map(|e| e.ok().map(|e| e.path())).collect();
    files.sort();
    for path in files {
        let name = path.to_string_lossy().to_string();
        if !(name.ends_with(".test.jsonl") || name.ends_with("catalog.jsonl")) {
            continue;
        }
        for record in read_jsonl(&path) {
            if record.get("openapi").is_some() {
                continue;
            }
            check_tree(&record["tree"], &mut Vec::new(), &mut failures, &mut seen);
        }
    }
    assert!(failures.is_empty(), "{} signature mismatches:\n{}", failures.len(), failures.join("\n"));
    assert!(seen.len() > 50, "too few tools checked: {}", seen.len());
}

#[test]
fn recorded_catalog_reads_match() {
    let mut failures = Vec::new();
    for record in read_jsonl(&fixtures().join("catalog.jsonl")) {
        let runtime =
            CodeMode::new(tree_entries(&record["tree"]), Arc::new(NoTools), Vec::new(), ExecutionLimits::default()).unwrap();
        let ours = serde_json::to_value(runtime.catalog()).unwrap();
        if ours != record["catalog"] {
            failures.push(format!("{}:\n  expected {}\n  got      {ours}", record["test"], record["catalog"]));
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

#[test]
fn recorded_schema_calls_match() {
    let mut failures = Vec::new();
    let mut count = 0;
    for record in read_jsonl(&fixtures().join("schema-calls.jsonl")) {
        let args = record["args"].as_array().unwrap();
        // A BigInt default cannot exist in JSON; upstream skips it as unserializable.
        if record["args"].to_string().contains("$bigint") {
            continue;
        }
        let expected = decode(&record["result"]).unwrap_or(Json::Null);
        let ours = match record["fn"].as_str().unwrap() {
            "jsonSchemaToTypeScript" => {
                let pretty = args.get(1).and_then(|p| p.as_bool()).unwrap_or(false);
                let schema = match schema_of(&args[0]) {
                    ToolSchema::Json(s) => s,
                    other => {
                        json!(schema_to_typescript(&other, pretty));
                        continue;
                    }
                };
                json!(json_schema_to_typescript(&schema, pretty))
            }
            "inputTypeScript" => {
                let def = tool_def(&args[0]["tool"], "t");
                let pretty = args.get(1).and_then(|p| p.as_bool()).unwrap_or(false);
                json!(schema_to_typescript(&def.input, pretty))
            }
            "outputTypeScript" => {
                let def = tool_def(&args[0]["tool"], "t");
                let pretty = args.get(1).and_then(|p| p.as_bool()).unwrap_or(false);
                json!(output_typescript(def.output.as_ref(), pretty))
            }
            "inputProperties" => properties_json(&tool_def(&args[0]["tool"], "t")),
            _ => continue,
        };
        count += 1;
        if ours != expected {
            failures.push(format!("{} {}:\n  expected {expected}\n  got      {ours}", record["fn"], record["test"]));
        }
    }
    assert!(failures.is_empty(), "{} of {count} mismatched:\n{}", failures.len(), failures.join("\n"));
}

#[test]
fn search_signature_and_tool_expressions() {
    let d: Json =
        serde_json::from_str(&std::fs::read_to_string(fixtures().join("search-signature.json")).unwrap()).unwrap();
    assert_eq!(d["searchSignature"].as_str().unwrap(), SEARCH_SIGNATURE);
    for pair in d["toolExpression"].as_array().unwrap() {
        assert_eq!(tool_expression(pair[0].as_str().unwrap()), pair[1].as_str().unwrap(), "{pair}");
    }
}

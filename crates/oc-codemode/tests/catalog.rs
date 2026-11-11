//! Catalog summarization and instruction texts against goldens produced by upstream core 2.2.7
//! (`packages/core/src/codemode/{catalog,instructions}.ts`), regenerated with
//! `bun tests/recorder/gen-catalog-golden.ts <opencode-2.2.7> tests/fixtures/catalog-golden.json <out>`.

use oc_codemode::catalog::{lookup, render, summarize, update, CatalogEntry, Summary};
use serde_json::Value as Json;

fn golden() -> Json {
    let path = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/catalog-golden.json");
    serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap()
}

#[test]
fn summaries_and_renders_match_upstream() {
    let g = golden();
    let mut checked = 0;
    for (name, case) in g["summaries"].as_object().unwrap() {
        let inventory: Vec<CatalogEntry> = serde_json::from_value(case["inventory"].clone()).unwrap();
        let budget = case["budget"].as_u64().map(|b| b as usize);
        let summary = summarize(&inventory, budget);
        // The stored summary is hashed: its JSON, key order included, must be upstream's.
        assert_eq!(serde_json::to_value(&summary).unwrap(), case["summary"], "summary {name}");
        assert_eq!(render(&summary), case["render"].as_str().unwrap(), "render {name}");
        checked += 1;
    }
    assert!(checked >= 28);
}

/// `update(previous, current, lookup)` (#62176): deltas when tools or namespaces come, go or change, the whole catalog
/// when that is shorter, and nothing when no listing the model saw changed.
#[test]
fn updates_match_upstream() {
    let g = golden();
    let inventory = |key: &Json| -> Vec<CatalogEntry> { serde_json::from_value(g["inventories"][key.as_str().unwrap()].clone()).unwrap() };
    let mut deltas = 0;
    for case in g["updates"].as_array().unwrap() {
        let budget = case["budget"].as_u64().map(|b| b as usize);
        let (previous, current) = (inventory(&case["previous"]), inventory(&case["current"]));
        let text = update(&summarize(&previous, budget), &summarize(&current, budget), &lookup(&current));
        assert_eq!(text.as_deref(), case["text"].as_str(), "update {} -> {} ({budget:?})", case["previous"], case["current"]);
        deltas += usize::from(text.is_some_and(|t| !t.contains("This catalog supersedes")));
    }
    assert!(deltas > 50, "{deltas}");
}

/// A summary stored by 2.2.5 (`count`/`shown`, no namespace `tools`) does not decode, so the session is sent the whole
/// catalog again; unknown keys alone do not fail it.
#[test]
fn old_summaries_do_not_decode() {
    let old = serde_json::json!({"total": 1, "shown": 1, "namespaces": [{"name": "a", "count": 1, "entries": []}]});
    assert!(serde_json::from_value::<Summary>(old).is_err());
    let extra = serde_json::json!({"total": 1, "shown": 1, "namespaces": [{"name": "a", "tools": ["a.b"], "entries": [], "count": 1}]});
    assert!(serde_json::from_value::<Summary>(extra).is_ok());
}

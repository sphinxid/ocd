//! Catalog summarization and instruction texts against goldens produced by upstream core
//! (`packages/core/src/codemode/{catalog,instructions}.ts`, generator script kept in the scratch build as
//! `gen-catalog-golden.ts`).

use oc_codemode::catalog::{render, summarize, update, CatalogEntry, Summary};
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
        let expected: Summary = serde_json::from_value(case["summary"].clone()).unwrap();
        assert_eq!(summary, expected, "summary {name}");
        assert_eq!(render(&summary), case["render"].as_str().unwrap(), "render {name}");
        checked += 1;
    }
    assert!(checked >= 28);
}

#[test]
fn updates_match_upstream() {
    let g = golden();
    for case in g["updates"].as_array().unwrap() {
        let previous: Summary = serde_json::from_value(case["previous"].clone()).unwrap();
        let current: Summary = serde_json::from_value(case["current"].clone()).unwrap();
        assert_eq!(update(&previous, &current), case["text"].as_str().unwrap());
    }
}

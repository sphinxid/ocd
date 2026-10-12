//! Replays every program execution recorded from the upstream test suite and compares results exactly.
//!
//! Records that cannot be replayed generically (extension host functions, OpenAPI tools, interrupted runs) are
//! covered by dedicated tests. Known, documented divergences are listed in `tests/fixtures/replay-skips.txt`
//! (`file<TAB>seq<TAB>reason`).

mod common;

use std::collections::HashMap;
use std::sync::Arc;

use common::*;
use oc_codemode::codemode::CodeMode;
use serde_json::Value as Json;

fn skips() -> HashMap<(String, u64), String> {
    let path = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/replay-skips.txt");
    let text = std::fs::read_to_string(path).unwrap_or_default();
    text.lines()
        .filter(|l| !l.trim().is_empty() && !l.starts_with('#'))
        .filter_map(|l| {
            let mut parts = l.splitn(3, '\t');
            Some(((parts.next()?.to_string(), parts.next()?.parse().ok()?), parts.next().unwrap_or("").to_string()))
        })
        .collect()
}

pub fn replayable(record: &Json) -> bool {
    record.get("result").is_some()
        && record["via"] == "codemode"
        && record.get("openapi").is_none()
        && record.get("truncated").is_none()
        && record["extensions"].as_array().map(|a| a.is_empty()).unwrap_or(true)
        && record["hooks"].as_array().map(|a| a.is_empty()).unwrap_or(true)
}

/// Runs one record; returns the mismatch description if any.
pub async fn replay(record: &Json) -> Option<String> {
    let entries = tree_entries(&record["tree"]);
    let host = host_arc(record);
    let runtime = match CodeMode::new(entries, host, Vec::new(), limits_of(record)) {
        Ok(r) => r,
        Err(e) => return Some(format!("make failed: {e}")),
    };
    let code = record["code"].as_str().unwrap_or("");
    let ours = runtime.execute(code).await.to_json();
    let expected = decode(&record["result"]).unwrap_or(Json::Null);
    if json_eq(&ours, &expected) {
        None
    } else {
        Some(format!("expected {expected}\n     got {ours}"))
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn upstream_executions_replay_exactly() {
    let skips = skips();
    let mut files: Vec<_> = std::fs::read_dir(fixtures())
        .unwrap()
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.to_string_lossy().ends_with(".test.jsonl"))
        .collect();
    files.sort();
    let mut total = 0;
    let mut passed = 0;
    let mut skipped = 0;
    let mut failures: Vec<String> = Vec::new();
    let only = std::env::var("REPLAY_ONLY").ok();
    for path in files {
        let name = path.file_name().unwrap().to_string_lossy().to_string();
        if let Some(only) = &only
            && !name.contains(only.as_str())
        {
            continue;
        }
        for record in read_jsonl(&path) {
            if !replayable(&record) {
                continue;
            }
            total += 1;
            let file = record["file"].as_str().unwrap_or("").to_string();
            let seq = record["seq"].as_u64().unwrap_or(0);
            let outcome = replay(&record).await;
            match outcome {
                None => passed += 1,
                Some(_) if skips.contains_key(&(file.clone(), seq)) => skipped += 1,
                Some(diff) => failures.push(format!(
                    "{file}\t{seq}\t{}\n  code: {}\n  {diff}",
                    record["test"].as_str().unwrap_or(""),
                    record["code"].as_str().unwrap_or("").replace('\n', "\n        ")
                )),
            }
        }
    }
    eprintln!("replayed {total}: {passed} passed, {skipped} known divergences, {} failed", failures.len());
    if !failures.is_empty() {
        let report = failures.join("\n\n");
        let out = std::env::temp_dir().join("oc-codemode-replay-failures.txt");
        std::fs::write(&out, &report).ok();
        let shown: Vec<&String> = failures.iter().take(20).collect();
        panic!(
            "{} replay mismatches (full list in {}):\n\n{}",
            failures.len(),
            out.display(),
            shown.iter().map(|s| s.as_str()).collect::<Vec<_>>().join("\n\n")
        );
    }
    let _ = Arc::new(());
}

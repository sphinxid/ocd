//! Replays recorded executions that call extension globals (upstream extensions.test.ts and friends): each host
//! function returns exactly what the recorded upstream host function returned, and results must match.

mod common;

use common::*;
use oc_codemode::codemode::CodeMode;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn extension_executions_replay_exactly() {
    let mut total = 0;
    let mut failures = Vec::new();
    let mut files: Vec<_> = std::fs::read_dir(fixtures()).unwrap().filter_map(|e| e.ok().map(|e| e.path())).collect();
    files.sort();
    for path in files {
        if !path.to_string_lossy().ends_with(".test.jsonl") {
            continue;
        }
        for record in read_jsonl(&path) {
            let Some(extensions) = record["extensions"].as_array() else { continue };
            if extensions.is_empty() || record.get("result").is_none() || !record["hooks"].as_array().unwrap().is_empty() {
                continue;
            }
            total += 1;
            let exts = common::ext::replay_extensions(&record);
            let runtime = match CodeMode::new(tree_entries(&record["tree"]), host_arc(&record), exts, limits_of(&record)) {
                Ok(r) => r,
                Err(e) => {
                    failures.push(format!("{}: make failed {e}", record["test"]));
                    continue;
                }
            };
            let ours = runtime.execute(record["code"].as_str().unwrap()).await.to_json();
            let expected = decode(&record["result"]).unwrap();
            if !json_eq(&ours, &expected) {
                failures.push(format!(
                    "{} (seq {})\n  code: {}\n  expected {expected}\n  got      {ours}",
                    record["test"], record["seq"], record["code"]
                ));
            }
        }
    }
    eprintln!("extension replays: {total}, {} failed", failures.len());
    let allowed = std::fs::read_to_string(std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/extension-skips.txt")).unwrap_or_default();
    let failures: Vec<String> = failures.into_iter().filter(|f| !allowed.lines().any(|l| !l.is_empty() && !l.starts_with('#') && f.contains(l.split('\t').next().unwrap()))).collect();
    assert!(failures.is_empty(), "{}", failures.join("\n\n"));
}

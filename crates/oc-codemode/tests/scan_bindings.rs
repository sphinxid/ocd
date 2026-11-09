//! `CodeMode.scanTools` and execution bindings (`packages/codemode/test/scan.test.ts`, 2.2.5).

use std::sync::Arc;

use oc_codemode::codemode::{CodeMode, ExecResult, ExecutionLimits};
use oc_codemode::host::NoTools;
use serde_json::json;

fn runtime(bindings: Vec<(&str, serde_json::Value)>) -> CodeMode {
    CodeMode::new(Vec::new(), Arc::new(NoTools), Vec::new(), ExecutionLimits::default())
        .unwrap()
        .with_bindings(bindings.into_iter().map(|(k, v)| (k.to_string(), v)).collect())
}

#[tokio::test]
async fn binds_outer_globals_that_programs_can_read_or_shadow() {
    let read = runtime(vec![("state", json!({"count": 4})), ("runs", json!(2)), ("input", json!({"state": {"count": 4}}))])
        .execute("return { prev: state.count, runs, same: input.state.count }")
        .await;
    match read {
        ExecResult::Success { value, .. } => assert_eq!(value, json!({"prev": 4, "runs": 2, "same": 4})),
        other => panic!("{other:?}"),
    }
    let shadowed = runtime(vec![("state", json!({"count": 4}))]).execute("const state = 9\nreturn { wake: true, state }").await;
    match shadowed {
        ExecResult::Success { value, .. } => assert_eq!(value, json!({"wake": true, "state": 9})),
        other => panic!("{other:?}"),
    }
}

/// Every case of `fixtures/scan_tools.jsonl` (captured from upstream 2.2.5's `CodeMode.scanTools`) gives the same
/// paths or diagnostic. Classes are outside the interpreter's subset, and their bodies are not scanned.
#[test]
fn scan_tools_matches_upstream() {
    for line in include_str!("fixtures/scan_tools.jsonl").lines() {
        let case: serde_json::Value = serde_json::from_str(line).unwrap();
        let code = case["code"].as_str().unwrap();
        if code.starts_with("class ") {
            assert_eq!(oc_codemode::scan::scan_tools(code).unwrap(), Vec::<String>::new());
            continue;
        }
        let got = match oc_codemode::scan::scan_tools(code) {
            Ok(paths) => json!({"_id": "Result", "_tag": "Success", "value": paths}),
            Err(d) => json!({"_id": "Result", "_tag": "Failure", "failure": serde_json::to_value(&d).unwrap()}),
        };
        assert_eq!(got, case["result"], "{code}");
    }
}

#[test]
fn scan_tools_results() {
    use oc_codemode::scan::scan_tools;
    assert_eq!(scan_tools("return await tools.a.b({}) && tools.c({})").unwrap(), vec!["a.b", "c"]);
    assert!(scan_tools("return tools").is_err());
    assert!(scan_tools("var tools = 1; return tools").unwrap().is_empty());
}

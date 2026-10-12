//! Golden tests: help, version, completions and parse errors captured from the real Effect CLI
//! (`effect@4.0.1`) running upstream's `commands.ts` (see tests/golden/README in the crate README).

mod common;

use serde_json::Value;

fn cases(file: &str) -> Vec<Value> {
    let path = format!("{}/tests/golden/{file}", env!("CARGO_MANIFEST_DIR"));
    serde_json::from_str::<Value>(&std::fs::read_to_string(path).unwrap()).unwrap().as_array().unwrap().clone()
}

fn args(c: &Value) -> Vec<String> {
    c["args"].as_array().unwrap().iter().map(|a| a.as_str().unwrap().to_string()).collect()
}

#[test]
fn help_version_completions_and_errors_match_effect_cli() {
    let env = common::Env::new();
    let mut failures = vec![];
    for c in cases("cli.json") {
        let a = args(&c);
        let expected_out = c["stdout"].as_str().unwrap();
        if expected_out.starts_with("HANDLER") {
            continue; // handler cases are covered by parse.rs
        }
        let refs: Vec<&str> = a.iter().map(String::as_str).collect();
        let out = env.run(&refs);
        let (stdout, stderr, code) = (common::text(&out.stdout), common::text(&out.stderr), out.status.code().unwrap_or(-1));
        let expected_err = c["stderr"].as_str().unwrap();
        let expected_code = c["code"].as_i64().unwrap() as i32;
        let err_ok = if let Some(pretty) = expected_err.strip_prefix("PRETTY:") {
            stderr.lines().next() == pretty.lines().next()
        } else {
            stderr == expected_err
        };
        if stdout != expected_out || !err_ok || code != expected_code {
            let first_diff = |a: &str, b: &str| {
                let (al, bl): (Vec<&str>, Vec<&str>) = (a.lines().collect(), b.lines().collect());
                (0..al.len().max(bl.len()))
                    .find(|i| al.get(*i) != bl.get(*i))
                    .map(|i| format!("line {i}: expected {:?} got {:?}", al.get(i), bl.get(i)))
                    .unwrap_or_else(|| format!("trailing difference ({} vs {} bytes)", a.len(), b.len()))
            };
            failures.push(format!(
                "args {a:?}: stdout {} | stderr {} | code {expected_code} vs {code}",
                if stdout == expected_out { "ok".into() } else { first_diff(expected_out, &stdout) },
                if err_ok { "ok".into() } else { first_diff(expected_err, &stderr) },
            ));
        }
    }
    assert!(failures.is_empty(), "{} golden mismatches:\n{}", failures.len(), failures.join("\n=====\n"));
}

#[test]
fn decoded_inputs_match_effect_cli() {
    let env = common::Env::new();
    let mut failures = vec![];
    for c in cases("parse.json").into_iter().chain(cases("cli.json")) {
        let expected = c["stdout"].as_str().unwrap();
        if !expected.starts_with("HANDLER") {
            continue;
        }
        let a = args(&c);
        let refs: Vec<&str> = a.iter().map(String::as_str).collect();
        let out = env.command(&refs).env("OPENCODE_CLI_PARSE_DUMP", "1").output().unwrap();
        let got = common::text(&out.stdout);
        if got != expected {
            failures.push(format!("args {a:?}\nexpected {expected}got      {got}"));
        }
    }
    assert!(failures.is_empty(), "{} parse mismatches:\n{}", failures.len(), failures.join("\n"));
}

//! Call hooks (upstream codemode.test.ts "CodeMode call hooks" and friends): replays recorded executions whose
//! runtimes configured hooks, failing `before` hooks where upstream's did, and checks both the program result and
//! the sequence of hook observations (`tool.after`/`extension.after` statuses).

mod common;

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use common::*;
use futures::future::BoxFuture;
use oc_codemode::codemode::CodeMode;
use oc_codemode::host::{CallResult, ExtensionInvocation, Host, HostValue, ToolCallError, ToolInvocation};
use serde_json::Value as Json;

struct HookHost {
    inner: ReplayHost,
    tool_denials: Mutex<VecDeque<(String, Option<String>)>>,
    extension_denials: Mutex<VecDeque<(String, Option<String>)>>,
    log: Mutex<Vec<String>>,
}

impl Host for HookHost {
    fn call(&self, path: &str, input: Option<Json>) -> BoxFuture<'static, Result<Option<Json>, ToolCallError>> {
        self.inner.call(path, input)
    }
    fn decode_input(&self, path: &str, input: Option<Json>) -> Result<Option<Json>, String> {
        self.inner.decode_input(path, input)
    }
    fn tool_before(&self, call: &ToolInvocation) -> BoxFuture<'static, Result<(), ToolCallError>> {
        self.log.lock().unwrap().push(format!("tool.before {}", call.name));
        let denial = {
            let mut d = self.tool_denials.lock().unwrap();
            match d.iter().position(|(n, _)| *n == call.name) {
                Some(i) => d.remove(i).unwrap().1,
                None => None,
            }
        };
        Box::pin(async move {
            match denial {
                Some(m) => Err(ToolCallError::Failure(m)),
                None => Ok(()),
            }
        })
    }
    fn tool_after(&self, call: &ToolInvocation, result: &CallResult) {
        self.log.lock().unwrap().push(format!("tool.after {} {}", call.name, result.status()));
    }
    fn extension_before(&self, call: &ExtensionInvocation) -> BoxFuture<'static, Result<(), HostValue>> {
        self.log.lock().unwrap().push(format!("extension.before {}", call.name));
        let denial = {
            let mut d = self.extension_denials.lock().unwrap();
            match d.iter().position(|(n, _)| *n == call.name) {
                Some(i) => d.remove(i).unwrap().1,
                None => None,
            }
        };
        Box::pin(async move {
            match denial {
                Some(m) => Err(HostValue::error("Error", m)),
                None => Ok(()),
            }
        })
    }
    fn extension_after(&self, call: &ExtensionInvocation, result: &CallResult) {
        self.log.lock().unwrap().push(format!("extension.after {} {}", call.name, result.status()));
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn hook_executions_replay_exactly() {
    let mut total = 0;
    let mut failures = Vec::new();
    for record in read_jsonl(&fixtures().join("codemode.test.jsonl")) {
        if record["hooks"].as_array().map(|h| h.is_empty()).unwrap_or(true) || record.get("result").is_none() {
            continue;
        }
        total += 1;
        let hooks: Vec<&Json> = record["events"].as_array().unwrap().iter().filter(|e| e["type"] == "hook").collect();
        let configured: Vec<&str> = record["hooks"].as_array().unwrap().iter().filter_map(|h| h.as_str()).collect();
        let denial = |kind: &str| -> VecDeque<(String, Option<String>)> {
            hooks
                .iter()
                .filter(|e| e["hook"] == kind)
                .map(|e| (e["call"]["name"].as_str().unwrap_or("").to_string(), e["failed"].as_str().map(String::from)))
                .collect()
        };
        let host = Arc::new(HookHost {
            inner: ReplayHost::new(&record),
            tool_denials: Mutex::new(denial("tool.before")),
            extension_denials: Mutex::new(denial("extension.before")),
            log: Mutex::new(Vec::new()),
        });
        let runtime = CodeMode::new(
            tree_entries(&record["tree"]),
            host.clone(),
            common::ext::replay_extensions(&record),
            limits_of(&record),
        )
        .unwrap();
        let ours = runtime.execute(record["code"].as_str().unwrap()).await.to_json();
        let expected = decode(&record["result"]).unwrap();
        if !json_eq(&ours, &expected) {
            failures.push(format!("{}: result\n  expected {expected}\n  got      {ours}", record["test"]));
            continue;
        }
        let mut expected_log: Vec<(i64, String)> = Vec::new();
        for e in &hooks {
            let kind = e["hook"].as_str().unwrap();
            if !configured.contains(&kind) {
                continue;
            }
            let name = e["call"]["name"].as_str().unwrap_or("").to_string();
            let line = match e.get("result") {
                Some(r) => format!("{kind} {name} {}", r["status"].as_str().unwrap_or("")),
                None => format!("{kind} {name}"),
            };
            expected_log.push((e["t"].as_i64().unwrap_or(0), line));
        }
        expected_log.sort_by_key(|(t, _)| *t);
        let expected_log: Vec<String> = expected_log.into_iter().map(|(_, l)| l).collect();
        let ours_log: Vec<String> = host
            .log
            .lock()
            .unwrap()
            .iter()
            .filter(|l| configured.iter().any(|c| l.starts_with(c)))
            .cloned()
            .collect();
        if ours_log != expected_log {
            failures.push(format!("{}: hooks\n  expected {expected_log:?}\n  got      {ours_log:?}", record["test"]));
        }
    }
    assert!(total >= 9, "expected the recorded hook executions, found {total}");
    assert!(failures.is_empty(), "{}", failures.join("\n\n"));
}

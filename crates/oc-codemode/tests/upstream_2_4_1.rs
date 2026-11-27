//! What opencode 2.4.1 changed in the Code Mode language, which it now runs as the `psychoscript` 0.2.1 package
//! (#65011, #65207): the import errors (#64587), Buffer and base64 options (#64930, #64933, #64931), V8-format error
//! stacks (#64939), getters and setters (#64908), classes (#64965) and Intl (#64935, #64938), and what the move
//! changed (the supported-syntax text, `Symbol(psychoscript.iterator)`).
//!
//! `tests/fixtures/cases-2.4.1.json` holds the programs with the results upstream 2.4.1 gives
//! (`tests/recorder/cases-2.4.1.ts` regenerates them), in the runtime of `tests/upstream_2_3_1.rs`: the tools `echo`
//! (returns its input) and `shop.show` (returns nothing), and the extension `host` with `echo` (returns its
//! arguments), `echoAsync` (the same, asynchronously), `bytes` (a host Buffer of "host") and `open` (throws a host
//! Error with a cause). Every result must be
//! identical, except the cases with a `gap` note, which must still differ: when one starts to match, delete its note.
//! Programs from psychoscript's tests are MIT-licensed, Copyright (c) 2026 Anomaly.

mod common;

use std::sync::Arc;

use futures::future::BoxFuture;
use common::json_eq;
use oc_codemode::codemode::{CodeMode, ExecutionLimits};
use oc_codemode::host::{Host, HostCall, HostFunction, HostValue, SimpleExtension, ToolCallError};
use oc_codemode::tool_runtime::{ToolDef, ToolEntry};
use serde_json::{json, Value as Json};

struct Echo;

impl Host for Echo {
    fn call(&self, _path: &str, input: Option<Json>) -> BoxFuture<'static, Result<Option<Json>, ToolCallError>> {
        Box::pin(async move { Ok(input) })
    }
}

struct Function {
    name: &'static str,
    asynchronous: bool,
    reply: fn(Vec<HostValue>) -> Result<HostValue, HostValue>,
}

impl HostFunction for Function {
    fn name(&self) -> &str {
        self.name
    }
    fn length(&self) -> usize {
        0
    }
    fn asynchronous(&self) -> bool {
        self.asynchronous
    }
    fn call(&self, args: Vec<HostValue>) -> HostCall {
        let value = (self.reply)(args);
        if self.asynchronous {
            HostCall::Pending(Box::pin(async move { value }))
        } else {
            HostCall::Ready(value)
        }
    }
}

/// `open()` throws a host Error whose cause carries Node's file-system fields, as `extensions.test.ts` does.
fn open_failed() -> HostValue {
    let cause = HostValue::Error {
        name: "Error".into(),
        message: "ENOENT: no such file or directory, open 'x'".into(),
        cause: None,
        fields: vec![
            ("code".into(), HostValue::String("ENOENT".into())),
            ("errno".into(), HostValue::Number(-2.0)),
            ("path".into(), HostValue::String("x".into())),
            ("detail".into(), HostValue::Object(vec![("retried".into(), HostValue::Bool(true))])),
        ],
    };
    HostValue::Error { name: "Error".into(), message: "open failed".into(), cause: Some(Box::new(cause)), fields: vec![] }
}

fn runtime() -> CodeMode {
    let globals: Vec<(String, Arc<dyn HostFunction>)> = vec![
        ("echo".into(), Arc::new(Function { name: "echo", asynchronous: false, reply: |args| Ok(HostValue::Array(args)) })),
        ("echoAsync".into(), Arc::new(Function { name: "echoAsync", asynchronous: true, reply: |args| Ok(HostValue::Array(args)) })),
        ("bytes".into(), Arc::new(Function { name: "bytes", asynchronous: false, reply: |_| Ok(HostValue::Buffer(b"host".to_vec())) })),
        ("open".into(), Arc::new(Function { name: "open", asynchronous: false, reply: |_| Err(open_failed()) })),
    ];
    CodeMode::new(
        vec![
            ToolEntry::Tool(ToolDef::new("echo", "Echo the input", json!({})).with_output(json!({}))),
            ToolEntry::Tool(ToolDef::new("shop.show", "Show raw input", json!({ "type": "object" }))),
        ],
        Arc::new(Echo),
        vec![Arc::new(SimpleExtension { name: "host".into(), globals })],
        ExecutionLimits { timeout_ms: Some(5000.0), ..Default::default() },
    )
    .unwrap()
}

/// Runs every case of `file` (under `tests/fixtures`); a case's `gap` note says it must still differ.
async fn check(file: &str) {
    // The recordings run with `TZ=UTC`, and the host time zone is process-wide: every test here pins the same one.
    unsafe { std::env::set_var("TZ", "UTC") };
    let path = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures").join(file);
    let cases: Vec<Json> = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
    let runtime = runtime();
    let only = std::env::var("CASES_ONLY").ok();
    let (mut failures, mut stale) = (Vec::new(), Vec::new());
    for case in &cases {
        let code = case["code"].as_str().unwrap();
        if let Some(only) = &only
            && !code.contains(only.as_str())
        {
            continue;
        }
        let ours = runtime.execute(code).await.to_json();
        let same = json_eq(&ours, &case["result"]);
        match (same, case.get("gap")) {
            (false, None) => failures.push(format!("{code}\n  expected {}\n  got      {ours}", case["result"])),
            (true, Some(_)) => stale.push(code.to_string()),
            _ => {}
        }
    }
    let report = failures.join("\n\n");
    let name = file.replace('/', "-");
    std::fs::write(std::env::temp_dir().join(format!("oc-codemode-{name}.txt")), &report).ok();
    assert!(stale.is_empty(), "{file}: these cases now match; delete their gap notes:\n{}", stale.join("\n\n"));
    assert!(failures.is_empty(), "{file}: {} of {} differ:\n\n{report}", failures.len(), cases.len());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn results_match_upstream_2_4_1() {
    check("cases-2.4.1.json").await;
}

/// psychoscript's own language tests (upstream `packages/codemode/test/*.test.ts` before the move, #65011): every
/// program they run, with the result 2.4.1's Code Mode gives (`tests/fixtures/language-2.4.1/<test>.json`, collected
/// by `tests/recorder/extract-programs.ts` and recorded by `tests/recorder/cases-2.4.1.ts`).
macro_rules! language_tests {
    ($($name:ident => $file:literal),* $(,)?) => {
        $(
            #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
            async fn $name() {
                check(concat!("language-2.4.1/", $file, ".json")).await;
            }
        )*
    };
}

language_tests! {
    accessors => "accessors",
    descriptors => "descriptors",
    errors => "errors",
    buffer => "buffer",
    classes => "classes",
    intl => "intl",
}

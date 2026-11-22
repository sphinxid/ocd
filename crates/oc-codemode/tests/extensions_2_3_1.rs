//! The extension host API opencode 2.3.1 added (upstream `extensions.test.ts`, ported to this crate's `host` API):
//! object-valued globals whose functions fire the hooks under their path (#61151), constructible globals and host
//! functions passed back as themselves (#64472), a synchronous global blocking on host work (#64363), and per-run
//! extension selection (#64482). The recorded executions of the same tests replay in `extensions_replay.rs`; these
//! check what a replay can't: what the host receives.

use std::sync::{Arc, Mutex};

use futures::future::BoxFuture;
use oc_codemode::codemode::{CodeMode, CodeModeError, ExecResult, ExecutionLimits, RunOptions};
use oc_codemode::host::{
    CallResult, Extension, ExtensionInvocation, GlobalsExtension, Host, HostCall, HostFn, HostFunction, HostValue,
    Json, NoTools, SimpleExtension, ToolCallError,
};
use oc_codemode::tool_runtime::{ToolDef, ToolEntry};
use serde_json::json;

fn runtime(extensions: Vec<Arc<dyn Extension>>) -> CodeMode {
    CodeMode::new(Vec::new(), Arc::new(NoTools), extensions, ExecutionLimits { timeout_ms: Some(10_000.0), ..Default::default() })
        .unwrap()
}

fn globals(name: &str, globals: Vec<(&str, HostValue)>) -> Arc<dyn Extension> {
    Arc::new(GlobalsExtension { name: name.into(), globals: globals.into_iter().map(|(k, v)| (k.to_string(), v)).collect(), modules: Vec::new() })
}

fn value(result: ExecResult) -> Json {
    match result {
        ExecResult::Success { value, .. } => value,
        ExecResult::Failure { error, .. } => panic!("expected success, got {:?}: {}", error.kind, error.message),
    }
}

fn failure(result: ExecResult) -> String {
    match result {
        ExecResult::Success { value, .. } => panic!("expected failure, got {value}"),
        ExecResult::Failure { error, .. } => error.message,
    }
}

fn ready(value: HostValue) -> HostCall {
    HostCall::Ready(Ok(value))
}

// A host function that blocks its caller on host work, as an Effect returned by a synchronous global does.
fn blocking(result: Result<HostValue, HostValue>) -> HostCall {
    HostCall::Block(Box::pin(async move {
        tokio::task::yield_now().await;
        result
    }))
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_host_function_passed_back_arrives_as_itself_wherever_it_sits() {
    // `file(name)` returns `{ name, text }`; `copy(dest, source)` tells which file's `text` it was given.
    let made: Arc<Mutex<Vec<(String, Arc<dyn HostFunction>)>>> = Arc::default();
    let file = {
        let made = made.clone();
        HostFn::new("file", 1, move |args| {
            let name = args[0].as_str().unwrap_or("").to_string();
            let content = format!("text of {name}");
            let text = HostFn::new("text", 0, move |_| {
                let content = content.clone();
                HostCall::Pending(Box::pin(async move { Ok(HostValue::String(content)) }))
            })
            .asynchronous()
            .into_arc();
            made.lock().unwrap().push((name.clone(), text.clone()));
            ready(HostValue::Object(vec![("name".into(), HostValue::String(name)), ("text".into(), HostValue::Function(text))]))
        })
        .into_value()
    };
    let copy = {
        let made = made.clone();
        HostFn::new("copy", 2, move |args| {
            let source = args[1].field("text").cloned();
            let found = match source {
                Some(HostValue::Function(f)) => {
                    made.lock().unwrap().iter().find(|(_, g)| Arc::ptr_eq(g, &f)).map(|(name, _)| name.clone())
                }
                _ => None,
            };
            ready(HostValue::Array(vec![args[0].clone(), found.map(HostValue::String).unwrap_or(HostValue::Undefined)]))
        })
        .into_value()
    };
    let target = runtime(vec![globals("files", vec![("file", file), ("copy", copy)])]);
    assert_eq!(value(target.execute(r#"return copy("b", file("a"))"#).await), json!(["b", "a"]));
    assert_eq!(value(target.execute(r#"const f = file("a"); return copy("b", { text: f.text })"#).await), json!(["b", "a"]));
    // The program still calls it as a program function.
    assert_eq!(value(target.execute(r#"return await file("c").text()"#).await), json!("text of c"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_program_function_or_a_global_passed_to_the_host_is_left_out() {
    let taken: Arc<Mutex<Vec<HostValue>>> = Arc::default();
    let take = {
        let taken = taken.clone();
        HostFn::new("take", 1, move |args| {
            taken.lock().unwrap().push(args.into_iter().next().unwrap_or(HostValue::Undefined));
            ready(HostValue::Undefined)
        })
        .into_value()
    };
    let fs = HostValue::Object(vec![("rm".into(), HostFn::new("rm", 0, |_| ready(HostValue::Undefined)).into_value())]);
    let target = runtime(vec![globals("gated", vec![("fs", fs), ("take", take)])]);
    value(target.execute(r#"take({ text: async () => "x" }); take({ text: take }); take(fs.rm); take({ a: fs.rm }); take(fs)"#).await);
    let taken = format!("{:?}", taken.lock().unwrap());
    assert_eq!(taken, "[{}, {}, undefined, {}, {}]");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_function_from_a_result_never_reaches_a_tool() {
    struct Tools(Arc<Mutex<Vec<Option<Json>>>>);
    impl Host for Tools {
        fn call(&self, _path: &str, input: Option<Json>) -> BoxFuture<'static, Result<Option<Json>, ToolCallError>> {
            self.0.lock().unwrap().push(input.clone());
            Box::pin(async move { Ok(input) })
        }
    }
    let seen: Arc<Mutex<Vec<Option<Json>>>> = Arc::default();
    let make = HostFn::new("make", 0, |_| {
        ready(HostValue::Object(vec![
            ("n".into(), HostValue::Number(1.0)),
            ("f".into(), HostFn::new("f", 0, |_| ready(HostValue::Null)).into_value()),
        ]))
    })
    .into_value();
    let target = CodeMode::new(
        vec![ToolEntry::Tool(ToolDef::new("echo", "Echo", json!({})).with_output(json!({})))],
        Arc::new(Tools(seen.clone())),
        vec![globals("maker", vec![("make", make)])],
        ExecutionLimits::default(),
    )
    .unwrap();
    assert_eq!(value(target.execute("const m = make(); return await tools.echo({ m, f: m.f })").await), json!({ "m": { "n": 1 } }));
    assert_eq!(*seen.lock().unwrap(), vec![Some(json!({ "m": { "n": 1 } }))]);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_constructible_global_works_with_new_and_returns_what_the_host_function_returns() {
    let glob = HostFn::new("Glob", 1, |args| {
        let pattern = args[0].as_str().unwrap_or("").to_string();
        let matcher = HostFn::new("match", 1, move |args| ready(HostValue::Bool(args[0].as_str().unwrap_or("").ends_with(&pattern))));
        ready(HostValue::Object(vec![("match".into(), matcher.into_value())]))
    })
    .constructible()
    .into_value();
    let target = runtime(vec![globals("glob", vec![("Glob", glob)])]);
    assert_eq!(value(target.execute(r#"const g = new Glob(".ts"); return [g.match("a.ts"), g.match("a.js")]"#).await), json!([true, false]));
    assert_eq!(value(target.execute(r#"return Glob(".ts").match("a.ts")"#).await), json!(true));
    assert_eq!(value(target.execute(r#"return new Glob(...[".ts"]).match("a.ts")"#).await), json!(true));
    assert!(failure(target.execute(r#"new (new Glob(".ts").match)("a")"#).await).contains("is not supported"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_constructible_global_that_blocks_on_the_host_gives_new_its_value() {
    let sum = HostFn::new("Sum", 0, |args| {
        let total: f64 = args.iter().map(|v| if let HostValue::Number(n) = v { *n } else { 0.0 }).sum();
        blocking(Ok(HostValue::Object(vec![("total".into(), HostValue::Number(total))])))
    })
    .constructible()
    .into_value();
    let target = runtime(vec![globals("sum", vec![("Sum", sum)])]);
    assert_eq!(value(target.execute("return new Sum(1, 2).total + 1").await), json!(4));
    assert_eq!(value(target.execute("return new Sum(...[1, 2]).total + 1").await), json!(4));
    // An argument that is still a promise is awaited first, as in a plain call.
    assert_eq!(value(target.execute("return new Sum((async () => { await null; return 1 })(), 2).total + 1").await), json!(4));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_global_is_callable_awaitable_and_not_constructible() {
    let later = HostFn::new("later", 1, |args| {
        let n = if let Some(HostValue::Number(n)) = args.first() { *n } else { 0.0 };
        HostCall::Pending(Box::pin(async move { Ok(HostValue::Number(n + 1.0)) }))
    })
    .asynchronous()
    .into_value();
    let target = runtime(vec![globals("web", vec![("later", later)])]);
    assert_eq!(value(target.execute("return await later(1)").await), json!(2));
    assert_eq!(value(target.execute("return [typeof later, later.name, later.length]").await), json!(["function", "later", 1]));
    assert!(failure(target.execute("new later()").await).contains("new later(...) is not supported"));
}

// A host recording the extension hooks.
struct Hooks {
    calls: Mutex<Vec<String>>,
    deny: Option<&'static str>,
}

impl Host for Hooks {
    fn call(&self, path: &str, _input: Option<Json>) -> BoxFuture<'static, Result<Option<Json>, ToolCallError>> {
        let message = format!("Tool '{path}' is unavailable.");
        Box::pin(async move { Err(ToolCallError::Failure(message)) })
    }
    fn extension_before(&self, call: &ExtensionInvocation) -> BoxFuture<'static, Result<(), HostValue>> {
        self.calls.lock().unwrap().push(format!("before {}:{}", call.extension, call.name));
        let denied = self.deny.filter(|name| *name == call.name).map(|name| HostValue::error("Error", format!("{name} is not allowed")));
        Box::pin(async move { denied.map_or(Ok(()), Err) })
    }
    fn extension_after(&self, call: &ExtensionInvocation, result: &CallResult) {
        self.calls.lock().unwrap().push(format!("after {}:{} {}", call.extension, call.name, result.status()));
    }
}

fn effect_globals(sent: Arc<Mutex<Vec<String>>>) -> Arc<dyn Extension> {
    let fetch = HostFn::new("fetch", 1, move |args| {
        let url = args[0].as_str().unwrap_or("").to_string();
        let sent = sent.clone();
        HostCall::Block(Box::pin(async move {
            sent.lock().unwrap().push(url.clone());
            Ok(HostValue::Object(vec![("url".into(), HostValue::String(url)), ("session".into(), HostValue::str("ses_1"))]))
        }))
    })
    .asynchronous()
    .into_value();
    let fail = HostFn::new("fail", 0, |_| blocking(Err(HostValue::error("RangeError", "denied")))).asynchronous().into_value();
    let fs = HostValue::Object(vec![
        (
            "readSync".into(),
            HostFn::new("readSync", 1, |args| blocking(Ok(HostValue::String(format!("ses_1:{}", args[0].as_str().unwrap_or(""))))))
                .into_value(),
        ),
        ("failSync".into(), HostFn::new("failSync", 0, |_| blocking(Err(HostValue::error("RangeError", "missing")))).into_value()),
    ]);
    globals("web", vec![("fetch", fetch), ("fail", fail), ("fs", fs)])
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_asynchronous_global_s_host_work_is_a_promise() {
    let sent: Arc<Mutex<Vec<String>>> = Arc::default();
    let target = runtime(vec![effect_globals(sent.clone())]);
    assert_eq!(
        value(target.execute(r#"const pending = fetch("https://a.test/"); return [pending instanceof Promise, await pending]"#).await),
        json!([true, { "url": "https://a.test/", "session": "ses_1" }])
    );
    assert_eq!(*sent.lock().unwrap(), vec!["https://a.test/".to_string()]);
    assert_eq!(
        value(target.execute("try { await fail() } catch (e) { return [e instanceof RangeError, e.message] }").await),
        json!([true, "denied"])
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_synchronous_global_blocking_on_host_work_returns_its_value_or_throws_before_the_next_statement() {
    let target = runtime(vec![effect_globals(Arc::default())]);
    assert_eq!(
        value(
            target
                .execute(
                    r#"const text = fs.readSync("a")
                       try { fs.failSync() } catch (e) { return [text, e instanceof RangeError, e.message] }"#,
                )
                .await
        ),
        json!(["ses_1:a", true, "missing"])
    );
    // Nothing else in the program runs while it waits: a queued reaction runs after the statement.
    assert_eq!(
        value(
            target
                .execute(r#"const order = []; Promise.resolve().then(() => order.push("job")); order.push(fs.readSync("b")); await null; return order"#)
                .await
        ),
        json!(["ses_1:b", "job"])
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_denying_before_hook_stops_the_host_work_before_it_runs() {
    let sent: Arc<Mutex<Vec<String>>> = Arc::default();
    let host = Arc::new(Hooks { calls: Mutex::default(), deny: Some("fetch") });
    let target = CodeMode::new(Vec::new(), host.clone(), vec![effect_globals(sent.clone())], ExecutionLimits::default()).unwrap();
    assert_eq!(
        value(target.execute(r#"try { await fetch("https://a.test/") } catch (e) { return e.message }"#).await),
        json!("fetch is not allowed")
    );
    assert!(sent.lock().unwrap().is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn import_require_and_import_load_object_globals_whose_functions_fire_the_hooks_by_path() {
    let fs = HostValue::Object(vec![
        (
            "readFile".into(),
            HostFn::new("readFile", 1, |args| ready(HostValue::String(format!("text of {}", args[0].as_str().unwrap_or(""))))).into_value(),
        ),
        (
            "promises".into(),
            HostValue::Object(vec![(
                "readFile".into(),
                HostFn::new("readFile", 1, |args| {
                    let path = args[0].clone();
                    HostCall::Pending(Box::pin(async move { Ok(path) }))
                })
                .asynchronous()
                .into_value(),
            )]),
        ),
    ]);
    let host = Arc::new(Hooks { calls: Mutex::default(), deny: None });
    let target = CodeMode::new(Vec::new(), host.clone(), vec![globals("files", vec![("fs", fs)])], ExecutionLimits::default()).unwrap();
    assert_eq!(
        value(
            target
                .execute(
                    r#"import fs, { readFile } from "node:fs"
                       import * as ns from "fs"
                       import { readFile as readAsync } from "fs/promises"
                       import { URL } from "url"
                       const dynamic = await import("fs")
                       return [readFile("a"), fs === require("fs"), ns.default === fs, dynamic === ns,
                         await readAsync("b"), new URL("https://a.test/x").pathname, typeof require("crypto").randomUUID]"#,
                )
                .await
        ),
        json!(["text of a", true, true, true, "b", "/x", "function"])
    );
    assert_eq!(
        *host.calls.lock().unwrap(),
        vec![
            "before files:fs.readFile",
            "after files:fs.readFile success",
            "before files:fs.promises.readFile",
            "after files:fs.promises.readFile success",
        ]
    );
    // A global object is a plain object of the run, and its functions are named by their own names.
    assert_eq!(
        value(target.execute("return [Object.keys(fs), Object.keys(fs.promises), fs.readFile.name, JSON.stringify(fs)]").await),
        json!([["readFile", "promises"], ["readFile"], "readFile", "{\"promises\":{}}"])
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_run_can_get_only_some_of_the_runtime_s_extensions() {
    let later = HostFn::new("later", 1, |args| {
        let n = if let Some(HostValue::Number(n)) = args.first() { *n } else { 0.0 };
        HostCall::Pending(Box::pin(async move { Ok(HostValue::Number(n + 1.0)) }))
    })
    .asynchronous()
    .into_value();
    let web = globals("web", vec![("later", later)]);
    let files = globals("files", vec![("read", HostFn::new("read", 0, |_| ready(HostValue::str("text"))).into_value())]);
    let target = runtime(vec![web.clone(), files.clone()]);
    assert_eq!(value(target.execute("return [typeof read, typeof later]").await), json!(["function", "function"]));
    let only = target.select_extensions(&[web.clone()]).unwrap();
    assert_eq!(value(only.execute("return [typeof read, typeof later, await later(1)]").await), json!(["undefined", "function", 2]));
    let none = target.select_extensions(&[]).unwrap();
    assert_eq!(value(none.execute("return [typeof read, typeof later]").await), json!(["undefined", "undefined"]));
    // In the runtime's order, whatever order they are picked in; by name too.
    let both = target.select_extensions_named(&["files", "web"]).unwrap();
    assert_eq!(value(both.execute("return [typeof read, typeof later]").await), json!(["function", "function"]));
    let options = RunOptions { extensions: Some(vec![files.clone()]) };
    assert_eq!(value(target.execute_with("return [typeof read, typeof later]", &options).await.unwrap()), json!(["function", "undefined"]));
    // The selection doesn't change the runtime it came from.
    assert_eq!(value(target.execute("return typeof read").await), json!("function"));
    let other = globals("other", vec![("f", HostFn::new("f", 0, |_| ready(HostValue::Number(1.0))).into_value())]);
    let refused = target.select_extensions(&[other.clone()]).err();
    assert_eq!(refused, Some(CodeModeError::Type("Extension \"other\" isn't one this runtime was made with.".into())));
    assert!(target.execute_with("1", &RunOptions { extensions: Some(vec![other]) }).await.is_err());
    // The same name isn't the same extension.
    let impostor = globals("web", vec![("later", HostFn::new("later", 0, |_| ready(HostValue::Null)).into_value())]);
    assert!(target.select_extensions(&[impostor]).is_err());
    assert!(target.select_extensions_named(&["nope"]).is_err());
}

#[test]
fn a_global_must_be_a_function_or_an_object() {
    let bad = globals("bad", vec![("n", HostValue::Number(1.0))]);
    let made = CodeMode::new(Vec::new(), Arc::new(NoTools), vec![bad], ExecutionLimits::default()).err();
    assert_eq!(made, Some(CodeModeError::Type("Extension \"bad\" global \"n\" must be a function or an object.".into())));
}

#[test]
fn a_global_may_not_shadow_a_built_in_or_another_extension() {
    let one = |name: &str, global: &str| -> Arc<dyn Extension> {
        Arc::new(SimpleExtension {
            name: name.into(),
            globals: vec![(global.into(), HostFn::new(global, 0, |_| ready(HostValue::Null)).into_arc())],
        })
    };
    let made = CodeMode::new(Vec::new(), Arc::new(NoTools), vec![one("web", "URL")], ExecutionLimits::default()).err();
    assert_eq!(made, Some(CodeModeError::Type("Extension \"web\" global \"URL\" is already defined.".into())));
    let objects = globals("files", vec![("fs", HostValue::Object(Vec::new()))]);
    let made = CodeMode::new(Vec::new(), Arc::new(NoTools), vec![objects, one("again", "fs")], ExecutionLimits::default()).err();
    assert_eq!(made, Some(CodeModeError::Type("Extension \"again\" global \"fs\" is already defined.".into())));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_blocking_call_from_an_asynchronous_function_is_a_promise() {
    let slow = HostFn::new("slow", 0, |_| blocking(Ok(HostValue::str("done")))).asynchronous().into_value();
    let target = runtime(vec![globals("s", vec![("slow", slow)])]);
    assert_eq!(value(target.execute("const p = slow(); return [p instanceof Promise, await p]").await), json!([true, "done"]));
}

//! The cases upstream added or changed in 2.2 (`packages/codemode/test`, v2.2.2): promise jobs in V8 order, stalled
//! awaits, host results settling like I/O, located parse errors, the nesting-depth failure, timeouts inside built-in
//! work, promise-like `search` results, `for...in` over inherited keys, generator receiver checks, and extension
//! `before` hooks that run before the host function.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use futures::future::BoxFuture;
use oc_codemode::codemode::{CodeMode, ExecutionLimits};
use oc_codemode::host::{
    host_fn, host_fn_async, ExtensionInvocation, Extension, Host, HostCall, HostFunction, HostValue, NoTools,
    SimpleExtension, ToolCallError,
};
use oc_codemode::tool_runtime::{ToolDef, ToolEntry};
use serde_json::{json, Value as Json};

fn result_of(runtime: &CodeMode, code: &str) -> Json {
    runtime.execute_blocking(code).to_json()
}

fn plain() -> CodeMode {
    CodeMode::new(Vec::new(), Arc::new(NoTools), Vec::new(), ExecutionLimits::default()).unwrap()
}

fn value(runtime: &CodeMode, code: &str) -> Json {
    let result = result_of(runtime, code);
    assert_eq!(result["ok"], json!(true), "{code}\n{result}");
    result["value"].clone()
}

fn error(runtime: &CodeMode, code: &str) -> Json {
    let result = result_of(runtime, code);
    assert_eq!(result["ok"], json!(false), "{code}\n{result}");
    result["error"].clone()
}

// promise.test.ts: each case logs from several jobs; expected orders are what Node 25 (V8) logs for the same code.
const V8_ORDER: &[(&str, &str, &str)] = &[
    ("then vs await", r#"Promise.resolve().then(() => log.push("then")); (async () => { await null; log.push("await") })(); log.push("sync")"#, r#"["sync","then","await"]"#),
    ("adopt promise from async fn", r#"async function f() { return Promise.resolve(1) } f().then(() => log.push("f")); Promise.resolve().then(() => log.push("a")).then(() => log.push("b")).then(() => log.push("c")).then(() => log.push("d"))"#, r#"["a","b","f","c","d"]"#),
    ("resolve with promise", r#"new Promise((r) => r(Promise.resolve())).then(() => log.push("adopted")); Promise.resolve().then(() => log.push(1)).then(() => log.push(2)).then(() => log.push(3))"#, r#"[1,2,"adopted",3]"#),
    ("await thenable", r#"(async () => { await { then(r) { log.push("then called"); r(1) } }; log.push("after thenable") })(); Promise.resolve().then(() => log.push(1)).then(() => log.push(2)).then(() => log.push(3))"#, r#"["then called",1,"after thenable",2,3]"#),
    ("finally", r#"Promise.resolve(1).finally(() => log.push("fin")).then(() => log.push("after finally")); Promise.resolve().then(() => log.push(1)).then(() => log.push(2)).then(() => log.push(3)).then(() => log.push(4))"#, r#"["fin",1,2,3,"after finally",4]"#),
    ("finally rejected", r#"Promise.reject(1).finally(() => log.push("fin")).catch(() => log.push("caught")); Promise.resolve().then(() => log.push(1)).then(() => log.push(2)).then(() => log.push(3)).then(() => log.push(4))"#, r#"["fin",1,2,3,"caught",4]"#),
    ("Promise.all", r#"Promise.all([1, Promise.resolve(2)]).then(() => log.push("all")); Promise.resolve().then(() => log.push(1)).then(() => log.push(2)).then(() => log.push(3))"#, r#"[1,"all",2,3]"#),
    ("Promise.all empty", r#"Promise.all([]).then(() => log.push("all")); Promise.resolve().then(() => log.push(1)).then(() => log.push(2))"#, r#"["all",1,2]"#),
    ("Promise.race", r#"Promise.race([Promise.resolve(1), 2]).then((v) => log.push("race " + v)); Promise.resolve().then(() => log.push(1)).then(() => log.push(2)).then(() => log.push(3))"#, r#"[1,"race 1",2,3]"#),
    ("Promise.allSettled", r#"Promise.allSettled([Promise.reject(1), 2]).then(() => log.push("settled")); Promise.resolve().then(() => log.push(1)).then(() => log.push(2)).then(() => log.push(3))"#, r#"[1,"settled",2,3]"#),
    ("Promise.any", r#"Promise.any([Promise.reject(1), Promise.resolve(2)]).then(() => log.push("any")); Promise.resolve().then(() => log.push(1)).then(() => log.push(2)).then(() => log.push(3))"#, r#"[1,"any",2,3]"#),
    ("async generator", r#"async function* g() { log.push("start"); yield 1; log.push("mid"); yield 2; log.push("end") } const it = g(); it.next().then((r) => log.push("n1 " + r.value)); it.next().then((r) => log.push("n2 " + r.value)); it.next().then((r) => log.push("n3 " + r.done)); Promise.resolve().then(() => log.push(1)).then(() => log.push(2)).then(() => log.push(3)).then(() => log.push(4))"#, r#"["start","mid",1,"n1 1","end",2,"n2 2","n3 true",3,4]"#),
    ("async generator return", r#"async function* g() { try { yield 1 } finally { log.push("cleanup") } } const it = g(); it.next().then(() => it.return(5)).then((r) => log.push("ret " + r.value)); Promise.resolve().then(() => log.push(1)).then(() => log.push(2)).then(() => log.push(3)).then(() => log.push(4)).then(() => log.push(5))"#, r#"[1,2,"cleanup",3,4,"ret 5",5]"#),
    ("for await sync iterable", r#"(async () => { for await (const x of [1, Promise.resolve(2), 3]) log.push("x" + x) })(); Promise.resolve().then(() => log.push(1)).then(() => log.push(2)).then(() => log.push(3)).then(() => log.push(4)).then(() => log.push(5)).then(() => log.push(6))"#, r#"[1,"x1",2,3,"x2",4,5,"x3",6]"#),
    ("for await async gen", r#"async function* g() { yield 1; yield 2 } (async () => { for await (const x of g()) log.push("x" + x) })(); Promise.resolve().then(() => log.push(1)).then(() => log.push(2)).then(() => log.push(3)).then(() => log.push(4)).then(() => log.push(5)).then(() => log.push(6))"#, r#"[1,"x1",2,3,"x2",4,5,6]"#),
    ("nested async", r#"async function a() { log.push("a1"); await b(); log.push("a2") } async function b() { log.push("b1"); await null; log.push("b2") } a(); Promise.resolve().then(() => log.push(1)).then(() => log.push(2)).then(() => log.push(3))"#, r#"["a1","b1","b2",1,"a2",2,3]"#),
    ("await rejected", r#"(async () => { try { await Promise.reject(1) } catch { log.push("caught") } })(); Promise.resolve().then(() => log.push(1)).then(() => log.push(2))"#, r#"["caught",1,2]"#),
    ("then handler returns promise", r#"Promise.resolve().then(() => Promise.resolve("x")).then((v) => log.push(v)); Promise.resolve().then(() => log.push(1)).then(() => log.push(2)).then(() => log.push(3)).then(() => log.push(4))"#, r#"[1,2,3,"x",4]"#),
    ("withResolvers", r#"const { promise, resolve } = Promise.withResolvers(); promise.then(() => log.push("resolved")); Promise.resolve().then(() => { log.push(1); resolve() }).then(() => log.push(2)).then(() => log.push(3))"#, r#"[1,"resolved",2,3]"#),
    ("sync generator in async", r#"function* g() { yield 1; yield 2 } (async () => { for (const x of g()) { await null; log.push("g" + x) } })(); Promise.resolve().then(() => log.push(1)).then(() => log.push(2)).then(() => log.push(3))"#, r#"["g1",1,"g2",2,3]"#),
    ("async return await", r#"async function f() { return await Promise.resolve(1) } f().then(() => log.push("f")); Promise.resolve().then(() => log.push(1)).then(() => log.push(2)).then(() => log.push(3))"#, r#"[1,"f",2,3]"#),
    ("yield* sync in async gen", r#"async function* g() { yield* [1, Promise.resolve(2)] } (async () => { for await (const x of g()) log.push("x" + x) })(); Promise.resolve().then(() => log.push(1)).then(() => log.push(2)).then(() => log.push(3)).then(() => log.push(4)).then(() => log.push(5)).then(() => log.push(6)).then(() => log.push(7))"#, r#"[1,2,"x1",3,4,5,"x2",6,7]"#),
    ("break in for await sync", r#"(async () => { for await (const x of [1, 2]) { log.push("x" + x); break } log.push("after") })(); Promise.resolve().then(() => log.push(1)).then(() => log.push(2)).then(() => log.push(3)).then(() => log.push(4))"#, r#"[1,"x1",2,"after",3,4]"#),
    ("for await rejected value", r#"(async () => { try { for await (const x of [Promise.reject("r")]) log.push(x) } catch (e) { log.push("caught " + e) } })(); Promise.resolve().then(() => log.push(1)).then(() => log.push(2)).then(() => log.push(3)).then(() => log.push(4))"#, r#"[1,"caught r",2,3,4]"#),
    ("unhandled then later handled", r#"const p = Promise.reject(1); Promise.resolve().then(() => p.catch(() => log.push("late catch"))); Promise.resolve().then(() => log.push(1)).then(() => log.push(2)).then(() => log.push(3))"#, r#"[1,"late catch",2,3]"#),
    ("generator return in finally", r#"function* g() { try { yield 1 } finally { yield 2 } } const it = g(); log.push(JSON.stringify(it.next())); log.push(JSON.stringify(it.return(9))); log.push(JSON.stringify(it.next()))"#, r#"["{\"value\":1,\"done\":false}","{\"value\":2,\"done\":false}","{\"value\":9,\"done\":true}"]"#),
    ("async gen await in finally", r#"async function* g() { try { yield 1 } finally { await null; log.push("fin") } } const it = g(); it.next().then(() => it.return(3)).then((r) => log.push("r" + r.value)); Promise.resolve().then(() => log.push(1)).then(() => log.push(2)).then(() => log.push(3)).then(() => log.push(4)).then(() => log.push(5))"#, r#"[1,2,3,"fin",4,5,"r3"]"#),
    ("thenable job then throws", r#"Promise.resolve({ then() { throw new Error("boom") } }).catch((e) => log.push(e.message)); Promise.resolve().then(() => log.push(1)).then(() => log.push(2))"#, r#"[1,"boom",2]"#),
    ("Promise.try", r#"Promise.try(() => log.push("ran")).then(() => log.push("try")); Promise.resolve().then(() => log.push(1)).then(() => log.push(2))"#, r#"["ran","try",1,2]"#),
];

#[test]
fn job_order_matches_v8() {
    let runtime = plain();
    let mut failures = Vec::new();
    for (name, code, expected) in V8_ORDER {
        let program = format!("const log = []; {code}; for (let i = 0; i < 20; i++) await null; return log");
        let got = result_of(&runtime, &program);
        let expected: Json = serde_json::from_str(expected).unwrap();
        if got["value"] != expected {
            failures.push(format!("{name}: expected {expected}, got {got}"));
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

/// The deterministic tools of promise.test.ts: `echo` settles at once, `gated` waits until `open` releases its id.
#[derive(Default)]
struct Gates {
    gates: Mutex<HashMap<i64, Arc<tokio::sync::Notify>>>,
}

impl Gates {
    fn gate(&self, id: i64) -> Arc<tokio::sync::Notify> {
        self.gates.lock().unwrap().entry(id).or_insert_with(|| Arc::new(tokio::sync::Notify::new())).clone()
    }
}

impl Host for Gates {
    fn call(&self, path: &str, input: Option<Json>) -> BoxFuture<'static, Result<Option<Json>, ToolCallError>> {
        let id = input.as_ref().and_then(|i| i["id"].as_i64()).unwrap_or(0);
        match path {
            "host.gated" => {
                let gate = self.gate(id);
                Box::pin(async move {
                    gate.notified().await;
                    Ok(Some(json!(id)))
                })
            }
            "host.open" => {
                self.gate(id).notify_one();
                Box::pin(async { Ok(Some(json!(true))) })
            }
            _ => Box::pin(async move { Ok(Some(json!(id))) }),
        }
    }
}

fn gated_runtime() -> CodeMode {
    let input = json!({ "type": "object", "properties": { "id": { "type": "number" } }, "required": ["id"] });
    let tools = vec![
        ToolEntry::Tool(ToolDef::new("host.echo", "Echo an id immediately", input.clone()).with_output(json!({ "type": "number" }))),
        ToolEntry::Tool(
            ToolDef::new("host.gated", "Echo an id once its gate opens", input.clone()).with_output(json!({ "type": "number" })),
        ),
        ToolEntry::Tool(ToolDef::new("host.open", "Open the gate for an id", input).with_output(json!({ "type": "boolean" }))),
    ];
    CodeMode::new(tools, Arc::new(Gates::default()), Vec::new(), ExecutionLimits::default()).unwrap()
}

const STALLED: &str = "Error: The program is waiting on a promise that can never settle: nothing else is running, so nothing can resolve it. Code Mode has no timers or sleep, so don't wait or poll inside a program.";

#[test]
fn a_program_that_can_never_finish_fails_at_the_stuck_await() {
    let runtime = plain();
    let cases: [(&str, u32, u32); 4] = [
        ("await new Promise((r) => r)", 1, 1),
        ("async function wait() {\n  await new Promise(() => {})\n}\nreturn wait()", 2, 3),
        ("async function wait() {\n  await new Promise(() => {})\n}\nawait wait()", 2, 3),
        (
            "const { promise } = Promise.withResolvers()\nconst p = (async () => { await null; await promise })()\nawait p",
            2,
            38,
        ),
    ];
    for (code, line, column) in cases {
        assert_eq!(
            error(&runtime, code),
            json!({ "kind": "ExecutionFailure", "message": format!("{STALLED} (line {line}, col {column})"), "location": { "line": line, "column": column } }),
            "{code}"
        );
    }
    let deep = error(
        &runtime,
        "const never = async () => { await null; await new Promise(() => {}) }\nreturn await Promise.all([never(), Promise.resolve(1)])",
    );
    assert!(deep["message"].as_str().unwrap().starts_with(STALLED));
    assert_eq!(
        error(&runtime, r#"try { await new Promise(() => {}) } catch { return "caught" }"#),
        json!({ "kind": "ExecutionFailure", "message": format!("{STALLED} (line 1, col 7)"), "location": { "line": 1, "column": 7 } })
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn running_tool_calls_are_not_a_stall() {
    let runtime = gated_runtime();
    let settle = runtime
        .execute(
            "const { promise, resolve } = Promise.withResolvers()\ntools.host.gated({ id: 1 }).then(resolve)\ntools.host.open({ id: 1 })\nreturn await promise",
        )
        .await
        .to_json();
    assert_eq!(settle["value"], json!(1), "{settle}");
    let waited = runtime
        .execute("const call = tools.host.gated({ id: 1 })\nawait tools.host.open({ id: 1 })\nreturn await call")
        .await
        .to_json();
    assert_eq!(waited["value"], json!(1), "{waited}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_tool_result_settles_after_the_jobs_already_queued() {
    let result = gated_runtime()
        .execute(
            r#"const log = []
            const call = tools.host.echo({ id: 1 }).then(() => log.push("tool"))
            Promise.resolve().then(() => log.push(1)).then(() => log.push(2)).then(() => log.push(3))
            await call
            return log"#,
        )
        .await
        .to_json();
    assert_eq!(result["value"], json!([1, 2, 3, "tool"]), "{result}");
}

#[test]
fn parse_errors_carry_a_location_and_a_hint() {
    let runtime = plain();
    assert_eq!(
        error(&runtime, "const = 1"),
        json!({ "kind": "ParseError", "message": "SyntaxError: Unexpected token. Code must be plain JavaScript written as a program body. (line 1, col 7)", "location": { "line": 1, "column": 7 } })
    );
    assert_eq!(
        error(&runtime, r#"import x from "y""#),
        json!({ "kind": "ParseError", "message": "SyntaxError: Imports and exports aren't available. Call tools through `tools` and use the built-in globals. (line 1, col 1)", "location": { "line": 1, "column": 1 } })
    );
    assert!(error(&runtime, "export const x = 1")["message"]
        .as_str()
        .unwrap()
        .starts_with("SyntaxError: Imports and exports aren't available."));
    let typescript = error(&runtime, "const value: number = 1; return value");
    assert!(typescript["message"].as_str().unwrap().ends_with("Code must be plain JavaScript written as a program body. (line 1, col 12)"));
    assert_eq!(typescript["location"], json!({ "line": 1, "column": 12 }));
    assert_eq!(
        error(&runtime, "const a = 1\nconst b = 2\nreturn a +* b")["message"],
        json!("SyntaxError: Unexpected token. Code must be plain JavaScript written as a program body. (line 3, col 11)")
    );
    assert_eq!(error(&runtime, " \n\t"), json!({ "kind": "ParseError", "message": "Code cannot be empty." }));
}

#[test]
fn nesting_deeper_than_the_host_stack_fails_cleanly() {
    let runtime = plain();
    let deep = format!("const f = () => f; f{}; return 1", "()".repeat(100_000));
    assert_eq!(error(&runtime, &deep), json!({ "kind": "ExecutionFailure", "message": "Execution exceeded the maximum nesting depth." }));
    let shallow = format!("const f = () => f; f{}; return 1", "()".repeat(500));
    assert_eq!(value(&runtime, &shallow), json!(1));
}

#[test]
fn long_built_in_work_still_times_out() {
    let runtime = CodeMode::new(
        Vec::new(),
        Arc::new(NoTools),
        Vec::new(),
        ExecutionLimits { timeout_ms: Some(100.0), ..Default::default() },
    )
    .unwrap();
    for code in [
        "return Iterator.from({ next: Object }).drop(Infinity).next()",
        "let a = [0]; for (let i = 0; i < 24; i++) a = [a, a]; return JSON.stringify(a).length",
        "let a = [0]; for (let i = 0; i < 24; i++) a = [a, a]; return a",
        "const a = Array(10_000).fill(0); for (let i = 0; i < 20_000; i++) Promise.reject(a); return 1",
    ] {
        let started = std::time::Instant::now();
        assert_eq!(error(&runtime, code)["kind"], json!("TimeoutExceeded"), "{code}");
        assert!(started.elapsed() < std::time::Duration::from_secs(5), "{code} took {:?}", started.elapsed());
    }
}

struct Echo;

impl Host for Echo {
    fn call(&self, _path: &str, _input: Option<Json>) -> BoxFuture<'static, Result<Option<Json>, ToolCallError>> {
        Box::pin(async { Ok(Some(json!("listed"))) })
    }
}

#[test]
fn search_results_can_be_read_awaited_or_chained() {
    let tools = vec![ToolEntry::Tool(
        ToolDef::new("api.list", "List things", json!({ "type": "object", "properties": {} })).with_output(json!({ "type": "string" })),
    )];
    let runtime = CodeMode::new(tools, Arc::new(Echo), Vec::new(), ExecutionLimits::default()).unwrap();
    assert_eq!(
        value(
            &runtime,
            r#"const query = { query: "list" }
            return [
              search(query).items[0].path,
              (await search(query)).items[0].path,
              await search(query).then((result) => result.items[0].path),
              (await search(query).catch(() => "unused")).items[0].path,
              (await search(query).finally(() => "ignored")).items[0].path,
              (await Promise.all([search(query), search({})])).map((result) => result.items.length),
            ]"#
        ),
        json!(["tools.api.list", "tools.api.list", "tools.api.list", "tools.api.list", "tools.api.list", [1, 1]])
    );
    let views = value(
        &runtime,
        r#"const result = search({ query: "list" })
        const forIn = []
        for (const key in result) forIn.push(key)
        return { json: JSON.parse(JSON.stringify(result)), keys: Object.keys(result), spread: Object.keys({ ...result }), forIn, returned: result }"#,
    );
    let listed = json!({ "items": [{ "path": "tools.api.list" }], "remaining": 0, "next": null });
    assert_eq!(views["json"]["items"][0]["path"], listed["items"][0]["path"]);
    assert_eq!(views["keys"], json!(["items", "remaining", "next"]));
    assert_eq!(views["spread"], json!(["items", "remaining", "next"]));
    assert_eq!(views["forIn"], json!(["items", "remaining", "next"]));
    assert_eq!(views["returned"]["remaining"], json!(0));
    assert!(error(&runtime, r#"return search({ limit: "ten" }).items"#)["message"]
        .as_str()
        .unwrap()
        .contains("Invalid input for tool 'search'"));
    assert!(value(&runtime, r#"try { await search({ limit: "ten" }) } catch (error) { return error.message }"#)
        .as_str()
        .unwrap()
        .contains("Invalid input for tool 'search'"));
    let missing = error(
        &runtime,
        "// Try search with different namespaces\nfor (const ns of [\"github\", \"tools.github\", \"tools\", \"\"]) {\n  const s = await search({query: \"star\", namespace: ns, limit: 100}).filter((item) => item.path);\n  return s\n}",
    );
    assert_eq!(missing["message"], json!("TypeError: search(...).filter is not a function. (line 3, col 19)"));
    assert_eq!(missing["location"], json!({ "line": 3, "column": 19 }));
}

#[test]
fn for_in_visits_inherited_enumerable_keys() {
    let runtime = plain();
    let for_in = |target: &str, setup: &str| {
        value(&runtime, &format!("{setup}; const out = []; for (const key in {target}) out.push(key); return out"))
    };
    assert_eq!(for_in("o", "const p = { inh: 1 }; const o = Object.create(p); o.own = 2"), json!(["own", "inh"]));
    assert_eq!(
        for_in(
            "o",
            "const top = { a: 1, top: 1 }\nconst mid = Object.create(top); mid.a = 2; mid.mid = 2\nconst o = Object.create(mid); o.own = 3; o.mid = 3"
        ),
        json!(["own", "mid", "a", "top"])
    );
    assert_eq!(
        for_in("o", "const p = { b: 1, 10: 1, 2: 1 }; const o = Object.create(p); o.z = 1; o[5] = 1; o[1] = 1"),
        json!(["1", "5", "z", "2", "10", "b"])
    );
    let setup = "Object.prototype.length = 1; Object.prototype.push = 2; Object.prototype.extra = 3";
    assert_eq!(for_in("[5]", setup), json!(["0", "extra"]));
    assert_eq!(for_in("Object.create([5])", setup), json!(["0", "extra"]));
    assert_eq!(for_in("o", "const o = Object.create(null); o.a = 1"), json!(["a"]));
    assert_eq!(for_in("Object.create(null)", ""), json!([]));
    assert_eq!(for_in("[7, 8]", ""), json!(["0", "1"]));
    assert_eq!(for_in(r#""ab""#, ""), json!(["0", "1"]));
    assert_eq!(for_in("[7, 8]", "Array.prototype.extra = 1"), json!(["0", "1", "extra"]));
    assert_eq!(for_in(r#""ab""#, "Array.prototype.extra = 1"), json!(["0", "1"]));
    assert_eq!(for_in(r#""ab""#, "String.prototype.extra = 1"), json!(["0", "1", "extra"]));
    assert_eq!(for_in("5", "Number.prototype.extra = 1"), json!(["extra"]));
}

#[test]
fn generator_protocol_misuse() {
    assert_eq!(
        value(
            &plain(),
            r#"const next = (function* () {})().next
            const generator = (async function* () {})()
            try {
              next.call(generator)
              return "no error"
            } catch (e) {
              return [e.name, e.message]
            }"#
        ),
        json!(["TypeError", "Generator.prototype.next called on incompatible receiver an async generator."])
    );
    assert_eq!(
        value(
            &plain(),
            r#"const next = (async function* () {})().next
            const p = next.call((function* () {})())
            try { await p; return "no error" } catch (e) { return [e.name, e.message] }"#
        ),
        json!(["TypeError", "AsyncGenerator.prototype.next called on incompatible receiver."])
    );
}

/// Denies every extension call, like upstream's `extension.before: (call) => Effect.fail(...)`.
struct Deny;

impl Host for Deny {
    fn call(&self, _path: &str, _input: Option<Json>) -> BoxFuture<'static, Result<Option<Json>, ToolCallError>> {
        Box::pin(async { Ok(None) })
    }
    fn extension_before(&self, call: &ExtensionInvocation) -> BoxFuture<'static, Result<(), HostValue>> {
        let message = format!("{} is not allowed", call.name);
        Box::pin(async move { Err(HostValue::error("Error", message)) })
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_before_hook_runs_before_the_host_function() {
    let called: Arc<Mutex<Vec<&'static str>>> = Arc::new(Mutex::new(Vec::new()));
    let record = |name: &'static str, asynchronous: bool, pending: bool| {
        let called = called.clone();
        let f = move |_| {
            called.lock().unwrap().push(name);
            if pending { HostCall::Pending(Box::pin(async { Ok(HostValue::Number(1.0)) })) } else { HostCall::Ready(Ok(HostValue::Number(1.0))) }
        };
        let HostValue::Function(f) = (if asynchronous { host_fn_async(name, 0, f) } else { host_fn(name, 0, f) }) else {
            unreachable!()
        };
        (name.to_string(), f as Arc<dyn HostFunction>)
    };
    let extension: Arc<dyn Extension> = Arc::new(SimpleExtension {
        name: "host".into(),
        globals: vec![record("value", false, false), record("asyncFn", true, false), record("promise", true, true), record("effect", true, true)],
    });
    let runtime = CodeMode::new(Vec::new(), Arc::new(Deny), vec![extension], ExecutionLimits::default()).unwrap();
    let settled = runtime
        .execute("return (await Promise.allSettled([asyncFn(), promise(), effect()])).map((r) => r.reason.message)")
        .await
        .to_json();
    assert_eq!(settled["value"], json!(["asyncFn is not allowed", "promise is not allowed", "effect is not allowed"]), "{settled}");
    let thrown = runtime.execute("try { value() } catch (e) { return e.message }").await.to_json();
    assert_eq!(thrown["value"], json!("value is not allowed"), "{thrown}");
    assert!(called.lock().unwrap().is_empty());
}

//! Cases upstream added between 2.0.26 and 2.1.4 (`packages/codemode/test/{promise,stdlib,
//! array-callbacks-test262}.test.ts` and core's `tool-execute.test.ts`): jobs run to completion, the
//! `Uint8Array.from` mapper, non-coercing byte searches, `flatMap` holes, the host's `Number#toString` radix check, and
//! `fetch` refusing `file://` URLs.

use std::sync::{Arc, Mutex};

use futures::future::BoxFuture;
use oc_codemode::codemode::{CodeMode, ExecResult, ExecutionLimits};
use oc_codemode::host::{HostValue, HttpClient, HttpRequest, HttpResponse, NoTools};
use serde_json::{Value as Json, json};

fn run(code: &str) -> ExecResult {
    let runtime = CodeMode::new(
        Vec::new(),
        Arc::new(NoTools),
        Vec::new(),
        ExecutionLimits::default(),
    )
    .unwrap();
    runtime.execute_blocking(code)
}

/// JSON equality with numbers compared as doubles.
fn same(a: &Json, b: &Json) -> bool {
    match (a, b) {
        (Json::Number(x), Json::Number(y)) => x.as_f64() == y.as_f64(),
        (Json::Array(x), Json::Array(y)) => {
            x.len() == y.len() && x.iter().zip(y).all(|(p, q)| same(p, q))
        }
        (Json::Object(x), Json::Object(y)) => {
            x.len() == y.len()
                && x.iter()
                    .zip(y.iter())
                    .all(|((k, v), (l, w))| k == l && same(v, w))
        }
        _ => a == b,
    }
}

fn value(code: &str, expected: Json) {
    let out = run(code).to_json();
    assert_eq!(out["ok"], json!(true), "{code}\n{out}");
    assert!(
        same(&out["value"], &expected),
        "{code}\nexpected {expected}\nactual   {}",
        out["value"]
    );
}

fn error_message(code: &str) -> String {
    let out = run(code).to_json();
    assert_eq!(out["ok"], json!(false), "{code}\n{out}");
    out["error"]["message"]
        .as_str()
        .unwrap_or_default()
        .to_string()
}

// ---- promise.test.ts: jobs run to completion ----------------------------------------------------------------------

#[test]
fn updates_to_shared_state_between_awaits_are_not_lost() {
    value(
        r#"
        let total = 0
        const work = async () => {
          await null
          for (let i = 0; i < 1000; i++) total = total + 1
        }
        await Promise.all([work(), work()])
        return total
        "#,
        json!(2000),
    );
}

#[test]
fn an_async_function_runs_to_its_first_await_before_its_caller_continues() {
    value(
        r#"
        let count = 0
        const work = async () => {
          for (let i = 0; i < 1000; i++) count = count + 1
          await null
        }
        work()
        return count
        "#,
        json!(1000),
    );
}

#[test]
fn a_combinators_reaction_runs_after_the_continuations_a_preempted_job_queued_first() {
    for op in ["race", "any", "all", "allSettled"] {
        value(
            &format!(
                r#"
                const burn = () => {{ for (let i = 0; i < 400; i++) {{}} }}
                const log = []
                const settled = Promise.{op}([{{ then(resolve) {{ burn(); resolve(1) }} }}]).then(() => log.push("result"))
                const job = (async () => {{ await null; burn(); log.push("b0"); await null; log.push("b1") }})()
                await Promise.all([settled, job])
                return log
                "#
            ),
            json!(["b0", "b1", "result"]),
        );
    }
}

#[test]
fn a_job_that_never_awaits_still_times_out_while_other_jobs_wait_for_it() {
    let runtime = CodeMode::new(
        Vec::new(),
        Arc::new(NoTools),
        Vec::new(),
        ExecutionLimits {
            timeout_ms: Some(100.0),
            ..Default::default()
        },
    )
    .unwrap();
    let out = runtime
        .execute_blocking(
            r#"
            const spin = async () => { await null; while (true) {} }
            const quick = async () => { await null; return 1 }
            return await Promise.all([spin(), quick(), quick()])
            "#,
        )
        .to_json();
    assert_eq!(out["ok"], json!(false), "{out}");
    assert_eq!(out["error"]["kind"], json!("TimeoutExceeded"), "{out}");
}

// ---- stdlib.test.ts ----------------------------------------------------------------------------------------------

#[test]
fn number_to_string_truncates_the_radix_before_checking_its_range() {
    value(
        r#"return [36.1, 36.9, "36.9", { valueOf() { return 36.9 } }].map(radix => (35).toString(radix))"#,
        json!(["z", "z", "z", "z"]),
    );
}

#[test]
fn number_to_string_keeps_invalid_radixes_catchable_as_range_error() {
    value(
        r#"return [0, 1.9, 37, NaN, Infinity, -Infinity].map(radix => {
        try { return (35).toString(radix) }
        catch (error) { return error instanceof RangeError }
      })"#,
        json!([true, true, true, true, true, true]),
    );
    value(
        r#"return [(35).toString(), (35).toString(undefined), (35).toString(2.9)]"#,
        json!(["35", "35", "100011"]),
    );
    // The host's message, as JavaScriptCore words it.
    value(
        r#"try { (1).toString(37) } catch (e) { return e.name + ": " + e.message }"#,
        json!("RangeError: toString() radix argument must be between 2 and 36"),
    );
}

#[test]
fn uint8array_from_maps_the_original_values_with_this_arg_after_consuming_the_source() {
    value(
        r#"
        const events = []
        function* source() {
          events.push("start")
          yield 300
          events.push("end")
        }
        return [
          [...Uint8Array.from([300, 2], (value, index) => value / 2 + index)],
          [...Uint8Array.from(new Uint8Array([2, 3]), (value, index) => value + 255 + index)],
          [...Uint8Array.from([1, 2], function (value, index) { return value + index + this.offset }, { offset: 5 })],
          [...Uint8Array.from(atob("aGk="), (c) => c.charCodeAt(0))],
          [...Uint8Array.from([300, 2], undefined)],
          [...Uint8Array.from(source(), (value) => { events.push("map"); return value })],
          events,
        ]
        "#,
        json!([
            [150, 2],
            [1, 3],
            [6, 8],
            [104, 105],
            [44, 2],
            [44],
            ["start", "end", "map"]
        ]),
    );
}

#[test]
fn uint8array_from_rejects_a_non_callable_mapper_even_for_an_empty_source() {
    for mapper in ["42", "null", "{}"] {
        let message = error_message(&format!("return Uint8Array.from([], {mapper}).length"));
        assert!(
            message.contains("TypeError: Uint8Array.from expects a function callback"),
            "{mapper}: {message}"
        );
    }
}

#[test]
fn typed_array_searches_do_not_coerce_non_numbers() {
    for search in ["'1'", "true", "false", "null", "{}", "undefined", "NaN"] {
        value(
            &format!(
                r#"
                const bytes = new Uint8Array([0, 1, 2, 1])
                return [bytes.indexOf({search}), bytes.lastIndexOf({search}), bytes.includes({search})]
                "#
            ),
            json!([-1, -1, false]),
        );
    }
}

#[test]
fn typed_array_searches_keep_numeric_matching_and_start_index_coercion() {
    value(
        r#"
        const bytes = new Uint8Array([0, 1, 2, 1])
        return [
          bytes.indexOf(1, "2"), bytes.lastIndexOf(1, "2"), bytes.includes(2, "3"),
          bytes.indexOf(-0), bytes.includes(257), bytes.indexOf(1, -1)
        ]
        "#,
        json!([3, 1, false, 0, false, 3]),
    );
}

// ---- array-callbacks-test262.test.ts -------------------------------------------------------------------------------

#[test]
fn flat_map_skips_holes_in_callback_results_while_retaining_explicit_undefined() {
    value(
        r#"
        return [
          [1].flatMap(() => [, 2]),
          [1, 2].flatMap(() => Array(2)),
          [1].flatMap(() => [undefined, 2]),
          [1].flatMap(() => [[, 2]])[0].length,
          0 in [1].flatMap(() => [[, 2]])[0]
        ]
        "#,
        json!([[2], [], [null, 2], 2, false]),
    );
}

// ---- core tool-execute.test.ts / codemode web.ts -------------------------------------------------------------------

struct Recorder(Mutex<Vec<HttpRequest>>);

impl HttpClient for Recorder {
    fn send(&self, request: HttpRequest) -> BoxFuture<'static, Result<HttpResponse, String>> {
        self.0.lock().unwrap().push(request.clone());
        Box::pin(async move {
            Ok(HttpResponse {
                url: request.url,
                status: 200,
                status_text: "OK".into(),
                redirected: false,
                headers: Vec::new(),
                body: Vec::new(),
            })
        })
    }
}

#[tokio::test]
async fn fetch_rejects_filesystem_urls() {
    let client = Arc::new(Recorder(Mutex::new(Vec::new())));
    let runtime = CodeMode::new(
        Vec::new(),
        Arc::new(NoTools),
        vec![oc_codemode::web::extension(client.clone())],
        ExecutionLimits::default(),
    )
    .unwrap();
    let result = runtime
        .execute(r#"return await fetch("file:///blocked.txt")"#)
        .await;
    assert!(!result.ok());
    let output = oc_codemode::execute_tool::format_result(&result);
    assert!(
        output.contains("fetch cannot be used for filesystem access."),
        "{output}"
    );
    // Any spelling the URL parser reads as file:, and a URL object; the request never reaches the client.
    let result = runtime
        .execute(
            r#"
            const out = []
            for (const input of ["FILE:///etc/passwd", " file:relative", new URL("file:///x")]) {
              try { await fetch(input) } catch (e) { out.push(e instanceof Error && e.message) }
            }
            out.push((await fetch("https://example.test/")).status)
            return out
            "#,
        )
        .await;
    let msg = "fetch cannot be used for filesystem access.";
    assert_eq!(result.to_json()["value"], json!([msg, msg, msg, 200]));
    let seen = client.0.lock().unwrap();
    assert_eq!(seen.len(), 1);
    assert_eq!(seen[0].url, "https://example.test/");
}

#[test]
fn fetch_display_falls_back_to_get_when_the_method_is_not_a_string() {
    use oc_codemode::execute_tool::fetch_display;
    let url = HostValue::String("https://a.test".into());
    for init in [
        HostValue::Object(vec![("method".into(), HostValue::Number(1.0))]),
        HostValue::Array(vec![HostValue::String("post".into())]),
        HostValue::Null,
        HostValue::String("post".into()),
    ] {
        assert_eq!(
            Json::Object(fetch_display(&[url.clone(), init])),
            json!({"method": "GET", "url": "https://a.test"})
        );
    }
    assert_eq!(
        Json::Object(fetch_display(&[
            url,
            HostValue::Object(vec![("method".into(), HostValue::String("patch".into()))])
        ])),
        json!({"method": "PATCH", "url": "https://a.test"})
    );
}

// ---- recursion depth under run-to-completion jobs (2.1.4 executions, Bun 1.4) --------------------------------------

#[test]
fn async_and_generator_recursion_hits_the_located_call_depth_limit() {
    let located = |column: u32| {
        json!({"ok": false, "error": {"kind": "ExecutionFailure",
            "message": format!("RangeError: Maximum call stack size exceeded (line 1, col {column})"),
            "location": {"line": 1, "column": column}}, "toolCalls": []})
    };
    let cases = [
        (
            "const f = async (n) => f(n + 1); return await f(0)",
            located(24),
        ),
        (
            "const f = async (n) => await f(n + 1); return await f(0)",
            located(30),
        ),
        (
            "async function f(n) { return f(n + 1) } return await f(0)",
            located(30),
        ),
        (
            "const f = async (n) => { if (n > 20000) return n; return f(n + 1) }; return await f(0)",
            located(58),
        ),
        (
            "const g = (n) => (async () => g(n + 1))(); return await g(0)",
            located(31),
        ),
        (
            "function* g(n) { yield* g(n + 1) } return [...g(0)]",
            located(25),
        ),
        ("const f = (n) => f(n + 1); return f(0)", located(18)),
    ];
    let mut failures = Vec::new();
    for (code, expected) in cases {
        let actual = run(code).to_json();
        if actual != expected {
            failures.push(format!(
                "{code}\n  expected {expected}\n  actual   {actual}"
            ));
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
    value(
        "const f = async (n) => { await null; return f(n + 1) }; try { return await f(0) } catch (e) { return String(e) }",
        json!(
            "RangeError: Too many pending promises (limit 10000); await promises before creating more."
        ),
    );
}

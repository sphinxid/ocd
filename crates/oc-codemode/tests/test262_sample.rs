//! A sampled subset of the vendored test262 conformance runs, replayed with a port of upstream's host-provided
//! harness (`test/test262/run.ts`): every recorded program must produce the identical `Result`.
//! Known divergences are listed in `tests/fixtures/test262-skips.txt` (one vendored path per line, `#` reason).

mod common;

use std::collections::HashMap;
use std::rc::Rc;
use std::sync::Arc;

use common::*;
use oc_codemode::codemode::{CodeMode, ExecutionLimits};
use oc_codemode::host::NoTools;
use oc_codemode::interp::errors::Thrown;
use oc_codemode::interp::eval::{call_value, materialize};
use oc_codemode::interp::function::{arg, constructor, fn_, method, methods, NResult, NativeFn};
use oc_codemode::interp::globals::create_error_value;
use oc_codemode::interp::Rt;
use oc_codemode::object::{define, get, Obj, HIDDEN};
use oc_codemode::value::{PropKey, Value};
use serde_json::Value as Json;

fn show(value: &Value) -> String {
    match value {
        Value::String(s) => oc_codemode::data::quote(&s.to_rust()),
        Value::Number(n) if *n == 0.0 && n.is_sign_negative() => "-0".into(),
        Value::Object(o) if o.is_array() => {
            format!("[{}]", o.array_values().iter().map(show).collect::<Vec<_>>().join(", "))
        }
        Value::Object(o) if o.closure().is_some() => "program function".into(),
        Value::Object(o) if o.is_callable() => {
            oc_codemode::interp::coerce::coerce_to_string(&get(o, &PropKey::str("name")).unwrap_or_default()).to_rust()
        }
        Value::Object(o) => match get(o, &PropKey::str("message")).unwrap_or_default() {
            Value::String(m) => {
                let name = get(o, &PropKey::str("name")).unwrap_or_default();
                let name = if name.is_nullish() { "object".to_string() } else { oc_codemode::interp::coerce::coerce_to_string(&name).to_rust() };
                format!("{name}: {m}")
            }
            _ => "object".into(),
        },
        other => oc_codemode::interp::coerce::coerce_to_string(other).to_rust(),
    }
}

fn harness(rt: &Rt) -> Vec<(String, Value)> {
    let b = &rt.builtins;
    let proto = Obj::ordinary(&b.object);
    define(&proto, "name", Value::str("Test262Error"), HIDDEN);
    let make = {
        let proto = proto.clone();
        Rc::new(move |args: &[Value]| -> Value {
            let message = match arg(args, 0) {
                Value::Undefined => String::new(),
                v => oc_codemode::interp::coerce::coerce_to_string(&v).to_rust(),
            };
            Value::Object(create_error_value(&proto, Some(&message)))
        })
    };
    let fail = {
        let make = make.clone();
        Rc::new(move |message: String| NResult::err(Thrown::Value(make(&[Value::str(&message)]))))
    };
    let prefix = |m: &Value| if m.is_undefined() { String::new() } else { format!("{} ", oc_codemode::interp::coerce::coerce_to_string(m)) };
    let compare = |a: &Value, b: &Value| match (a, b) {
        (Value::Object(x), Value::Object(y)) if x.is_array() && y.is_array() => {
            let (xa, ya) = (x.array_values(), y.array_values());
            xa.len() == ya.len() && xa.iter().zip(ya.iter()).all(|(p, q)| p.same_value(q))
        }
        _ => false,
    };
    let (m1, m2) = (make.clone(), make.clone());
    let test262_error = constructor(
        &b.function,
        &proto,
        "Test262Error",
        0,
        NativeFn {
            call: Rc::new(move |_, _, _, args| NResult::ok(m1(&args))),
            construct: Some(Rc::new(move |_, _, args, _| NResult::ok(m2(&args)))),
            callback: true,
        },
    );
    let f0 = fail.clone();
    methods(&b.function, &test262_error, vec![method("thrower", 1, move |_, _, _, args| {
        f0(oc_codemode::interp::coerce::coerce_to_string(&arg(&args, 0)).to_rust())
    })]);
    let compare_array = fn_(&b.function, "compareArray", 2, move |_, _, _, args| NResult::ok(compare(&arg(&args, 0), &arg(&args, 1))));
    methods(&b.function, &compare_array, vec![method("format", 1, |_, _, _, args| NResult::ok(show(&arg(&args, 0))))]);
    let f1 = fail.clone();
    let assert = fn_(&b.function, "assert", 2, move |_, _, _, args| {
        if matches!(arg(&args, 0), Value::Bool(true)) {
            return NResult::ok(Value::Undefined);
        }
        let message = match arg(&args, 1) {
            Value::Undefined => format!("Expected true but got {}", show(&arg(&args, 0))),
            m => oc_codemode::interp::coerce::coerce_to_string(&m).to_rust(),
        };
        f1(message)
    });
    let (f2, f3, f4, f5) = (fail.clone(), fail.clone(), fail.clone(), fail.clone());
    methods(&b.function, &assert, vec![
        method("sameValue", 3, move |_, _, _, args| {
            if arg(&args, 0).same_value(&arg(&args, 1)) {
                return NResult::ok(Value::Undefined);
            }
            f2(format!("{}Expected SameValue(«{}», «{}») to be true", prefix(&arg(&args, 2)), show(&arg(&args, 0)), show(&arg(&args, 1))))
        }),
        method("notSameValue", 3, move |_, _, _, args| {
            if !arg(&args, 0).same_value(&arg(&args, 1)) {
                return NResult::ok(Value::Undefined);
            }
            f3(format!("{}Expected SameValue(«{}», «{}») to be false", prefix(&arg(&args, 2)), show(&arg(&args, 0)), show(&arg(&args, 1))))
        }),
        method("compareArray", 3, move |_, _, _, args| {
            if compare(&arg(&args, 0), &arg(&args, 1)) {
                return NResult::ok(Value::Undefined);
            }
            f4(format!("Actual {} and expected {} should have the same contents. {}", show(&arg(&args, 0)), show(&arg(&args, 1)), prefix(&arg(&args, 2))))
        }),
        method("throws", 3, move |rt, site, _, args| {
            let expected_ctor = arg(&args, 0);
            let expected = match &expected_ctor {
                Value::Object(o) if o.is_callable() => oc_codemode::interp::coerce::coerce_to_string(&get(o, &PropKey::str("name")).unwrap_or_default()).to_rust(),
                other => show(other),
            };
            let (rt, f5, label) = (rt.clone(), f5.clone(), prefix(&arg(&args, 2)));
            NResult::future(async move {
                match call_value(&rt, &arg(&args, 1), Value::Undefined, Vec::new(), site).await {
                    Err(Thrown::Interrupt) => Err(Thrown::Interrupt),
                    Err(e) => {
                        let thrown = materialize(&rt, &e);
                        let Value::Object(o) = &thrown else {
                            return f5(format!("{label}Thrown value was not an object!")).resolve().await;
                        };
                        let actual = get(o, &PropKey::str("constructor"))?;
                        if actual.strict_equals(&expected_ctor) {
                            return Ok(Value::Undefined);
                        }
                        f5(format!("{label}Expected a {expected} but got a {}", show(&actual))).resolve().await
                    }
                    Ok(_) => f5(format!("{label}Expected a {expected} to be thrown but no exception was thrown at all")).resolve().await,
                }
            })
        }),
    ]);
    vec![
        ("assert".into(), Value::Object(assert)),
        ("compareArray".into(), Value::Object(compare_array)),
        ("Test262Error".into(), Value::Object(test262_error)),
        ("$DONE".into(), Value::Object(fn_(&b.function, "$DONE", 1, |_, _, _, _| NResult::ok(Value::Undefined)))),
        (
            "$DONOTEVALUATE".into(),
            Value::Object(fn_(&b.function, "$DONOTEVALUATE", 0, |_, _, _, _| {
                NResult::err(Thrown::Value(Value::str("Test262: This statement should not be evaluated.")))
            })),
        ),
    ]
}

fn skips() -> HashMap<String, String> {
    let path = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/test262-skips.txt");
    std::fs::read_to_string(path)
        .unwrap_or_default()
        .lines()
        .filter(|l| !l.trim().is_empty() && !l.starts_with('#'))
        .map(|l| match l.split_once('#') {
            Some((p, r)) => (p.trim().to_string(), r.trim().to_string()),
            None => (l.trim().to_string(), String::new()),
        })
        .collect()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test262_sample_matches_upstream() {
    let dir = fixtures().join("test262");
    let mut files = Vec::new();
    for group in ["built-ins", "language"] {
        if let Ok(entries) = std::fs::read_dir(dir.join(group)) {
            files.extend(entries.filter_map(|e| e.ok().map(|e| e.path())));
        }
    }
    files.sort();
    let skips = skips();
    let runtime = CodeMode::new(
        Vec::new(),
        Arc::new(NoTools),
        Vec::new(),
        ExecutionLimits { timeout_ms: Some(5000.0), ..Default::default() },
    )
    .unwrap()
    .with_program_globals(Arc::new(harness));
    let (mut total, mut passed, mut skipped, mut failures) = (0, 0, 0, Vec::new());
    let only = std::env::var("TEST262_ONLY").ok();
    for path in files {
        for record in read_jsonl(&path) {
            let name = record["test"].as_str().unwrap_or("").to_string();
            if let Some(only) = &only
                && !name.contains(only.as_str())
            {
                continue;
            }
            let Some(expected) = record.get("result").and_then(decode) else { continue };
            total += 1;
            let ours = runtime.execute(record["code"].as_str().unwrap_or("")).await.to_json();
            if json_eq(&ours, &expected) || timed_out_both(&ours, &expected) {
                passed += 1;
            } else if skips.contains_key(&name) {
                skipped += 1;
            } else {
                failures.push(format!("{name}\n  expected {expected}\n  got      {ours}"));
            }
        }
    }
    eprintln!("test262 sample: {total} files, {passed} identical, {skipped} known divergences, {} failed", failures.len());
    if !failures.is_empty() {
        let out = std::env::temp_dir().join("oc-codemode-test262-failures.txt");
        std::fs::write(&out, failures.join("\n\n")).ok();
        panic!("{} test262 mismatches (see {}):\n{}", failures.len(), out.display(), failures.iter().take(15).cloned().collect::<Vec<_>>().join("\n"));
    }
}

fn timed_out_both(a: &Json, b: &Json) -> bool {
    a["error"]["kind"] == "TimeoutExceeded" && b["error"]["kind"] == "TimeoutExceeded"
}

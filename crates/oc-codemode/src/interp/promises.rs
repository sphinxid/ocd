//! Promise built-ins: capabilities, reactions that call program code, combinators, `then`/`catch`/`finally`, the
//! constructor, and the promise-like `search` results. Port of `interpreter/promises.ts`; the abstract operations
//! (settle, resolve, then) live in [`super::runner`].

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use super::callback::is_supported_callback;
use super::errors::*;
use super::eval::{call_value, materialize};
use super::function::{arg, constructor, fn_, method, methods, native, receiver, requires_new, NResult, NativeFn};
use super::globals::create_aggregate_error_value;
use super::iteration;
use super::runner::{Exit, Reaction, Resolving};
use super::{describe_value, Rt, Site};
use crate::ast::Loc;
use crate::object::{define, record, Obj, ObjKind, HIDDEN};
use crate::value::Value;

/// NewPromiseCapability(%Promise%): a pending promise with the host functions that settle it once. A built-in's own
/// failure that rejects it is located at `site`, the call that created it.
pub fn capability(rt: &Rt, site: Option<Loc>) -> JsResult<Resolving> {
    let promise = rt.runner.new_promise(rt)?;
    Ok(rt.runner.resolving(&promise, site))
}

// A `resolve`/`reject` handed to an executor or thenable: calling it settles the capability.
fn resolving_functions(rt: &Rt, resolving: &Resolving) -> (Value, Value) {
    let (r1, r2) = (resolving.clone(), resolving.clone());
    let resolve = fn_(&rt.builtins.function, "resolve", 1, move |rt, _, _, args| {
        r1.resolve(rt, arg(&args, 0));
        NResult::ok(Value::Undefined)
    });
    let reject = fn_(&rt.builtins.function, "reject", 1, move |rt, _, _, args| {
        r2.reject(rt, Thrown::Value(arg(&args, 0)));
        NResult::ok(Value::Undefined)
    });
    (Value::Object(resolve), Value::Object(reject))
}

/// Runs program code as a job of its own: `body` is the task.
fn start(rt: &Rt, body: impl std::future::Future<Output = ()> + 'static) {
    rt.runner.spawn(Box::pin(body));
}

/// A promise reaction that calls program code: the handler runs as a job, and its result or failure settles the
/// derived promise. A missing handler passes the outcome through.
fn handler_reaction(on_fulfilled: Option<Value>, on_rejected: Option<Value>, derived: Option<Resolving>) -> Rc<Reaction> {
    Reaction::new(move |settled: Exit| {
        let rt = super::rt();
        let handler = if settled.is_ok() { on_fulfilled } else { on_rejected };
        let handler = match handler {
            Some(h) if !matches!(settled, Err(Thrown::Interrupt)) => h,
            _ => {
                if let Some(d) = derived {
                    d.settle_with(&rt, settled);
                }
                return;
            }
        };
        let argument = match &settled {
            Ok(v) => v.clone(),
            Err(e) => materialize(&rt, e),
        };
        let rt2 = rt.clone();
        start(&rt, async move {
            match call_value(&rt2, &handler, Value::Undefined, vec![argument], Site::default()).await {
                Ok(result) => {
                    if let Some(d) = derived {
                        d.resolve(&rt2, result)
                    }
                }
                Err(error) => {
                    if let Some(d) = derived {
                        d.reject(&rt2, error)
                    }
                }
            }
        });
    })
}

const STATICS: [&str; 8] = ["all", "allSettled", "race", "any", "resolve", "reject", "withResolvers", "try"];

fn invoke_static(rt: &Rt, site: Site, name: &'static str, args: Vec<Value>) -> NResult {
    let node = site.node;
    match name {
        "resolve" => NResult::Ready(rt.runner.promise_resolve(rt, arg(&args, 0)).map(Value::Object)),
        "reject" => NResult::Ready(capability(rt, node).map(|made| {
            made.reject(rt, Thrown::Value(arg(&args, 0)));
            Value::Object(made.promise)
        })),
        "withResolvers" => NResult::Ready(capability(rt, node).map(|made| {
            let (resolve, reject) = resolving_functions(rt, &made);
            Value::Object(record(
                &rt.builtins.object,
                vec![("promise", Value::Object(made.promise)), ("resolve", resolve), ("reject", reject)],
            ))
        })),
        "try" => {
            let f = arg(&args, 0);
            if f.js_typeof() != "function" {
                return NResult::err(type_error(format!("Promise.try expects a function, received {}.", describe_value(&f))));
            }
            let made = match capability(rt, node) {
                Ok(m) => m,
                Err(e) => return NResult::err(e),
            };
            let rt = rt.clone();
            NResult::future(async move {
                let rest = args.get(1..).map(|s| s.to_vec()).unwrap_or_default();
                match call_value(&rt, &f, Value::Undefined, rest, Site::default()).await {
                    Ok(v) => made.resolve(&rt, v),
                    Err(e) => made.reject(&rt, e),
                }
                Ok(Value::Object(made.promise))
            })
        }
        _ => {
            let rt = rt.clone();
            let iterable = arg(&args, 0);
            NResult::future(async move { combinator(&rt, name, iterable, site).await })
        }
    }
}

// Promise.all, allSettled, race, and any: each element is resolved to a promise and observed through a reaction, so
// the result settles in the job where the deciding element's reaction runs, or at once for an empty input. A failure
// while iterating rejects the result.
async fn combinator(rt: &Rt, name: &'static str, iterable: Value, site: Site) -> JsResult<Value> {
    let made = capability(rt, site.node)?;
    struct State {
        values: Vec<Value>,
        reasons: Vec<Value>,
        remaining: usize,
        attached: Vec<(Obj, Rc<Reaction>)>,
    }
    let state = Rc::new(RefCell::new(State { values: Vec::new(), reasons: Vec::new(), remaining: 1, attached: Vec::new() }));
    // Once the result settles early, the remaining members' reactions can do nothing: detach them so a member that
    // stays pending for the rest of the program does not keep them.
    let decided = {
        let state = state.clone();
        move |rt: &Rt, settle: &dyn Fn()| {
            settle();
            let attached = std::mem::take(&mut state.borrow_mut().attached);
            for (item, reaction) in &attached {
                rt.runner.detach(item, reaction);
            }
        }
    };
    let finish = {
        let (state, made) = (state.clone(), made.clone());
        move |rt: &Rt| {
            let done = {
                let mut s = state.borrow_mut();
                s.remaining -= 1;
                s.remaining == 0
            };
            if !done {
                return;
            }
            if name == "any" {
                let reasons = state.borrow().reasons.clone();
                let error = create_aggregate_error_value(rt, reasons, "All promises were rejected", None);
                made.reject(rt, Thrown::Value(Value::Object(error)));
                return;
            }
            let values = state.borrow().values.clone();
            made.resolve(rt, Value::Object(Obj::array(&rt.builtins.array, values)));
        }
    };
    let finish = Rc::new(finish);
    let decided = Rc::new(decided);
    let outcome: JsResult<()> = async {
        let cursor = iteration::iterate(rt, &iterable, None, site).await?;
        let Some(cursor) = cursor else {
            return Err(type_error(format!(
                "Promise.{name} expects a synchronous iterable, received {}.",
                describe_value(&iterable)
            )));
        };
        let mut index = 0usize;
        loop {
            let step = cursor.next().await?;
            if step.done {
                break;
            }
            let item = rt.runner.promise_resolve(rt, step.value)?;
            let at = index;
            index += 1;
            {
                let mut s = state.borrow_mut();
                s.values.push(Value::Undefined);
                s.reasons.push(Value::Undefined);
                s.remaining += 1;
            }
            let called = Rc::new(Cell::new(false));
            let (state2, made2, finish2, decided2) = (state.clone(), made.clone(), finish.clone(), decided.clone());
            let reaction = Reaction::new(move |settled: Exit| {
                if called.replace(true) {
                    return;
                }
                let rt = super::rt();
                if name == "race" || matches!(settled, Err(Thrown::Interrupt)) {
                    return decided2(&rt, &|| made2.settle_with(&rt, settled.clone()));
                }
                match name {
                    "all" => match settled {
                        Err(e) => decided2(&rt, &|| made2.reject(&rt, e.clone())),
                        Ok(v) => {
                            state2.borrow_mut().values[at] = v;
                            finish2(&rt)
                        }
                    },
                    "any" => match settled {
                        Ok(v) => decided2(&rt, &|| made2.resolve(&rt, v.clone())),
                        Err(e) => {
                            let reason = materialize(&rt, &e);
                            state2.borrow_mut().reasons[at] = reason;
                            finish2(&rt)
                        }
                    },
                    _ => {
                        let entry = match settled {
                            Ok(v) => record(&rt.builtins.object, vec![("status", Value::str("fulfilled")), ("value", v)]),
                            Err(e) => record(
                                &rt.builtins.object,
                                vec![("status", Value::str("rejected")), ("reason", materialize(&rt, &e))],
                            ),
                        };
                        state2.borrow_mut().values[at] = Value::Object(entry);
                        finish2(&rt)
                    }
                }
            });
            state.borrow_mut().attached.push((item.clone(), reaction.clone()));
            rt.runner.then(&item, reaction);
        }
        if name == "race" && index == 0 {
            return Err(type_error("Promise.race([]) would never settle; provide at least one promise or value."));
        }
        finish(rt);
        Ok(())
    }
    .await;
    if let Err(error) = outcome {
        made.reject(rt, error);
    }
    Ok(Value::Object(made.promise))
}

fn reaction_handler(value: &Value, method: &str) -> JsResult<Option<Value>> {
    if is_supported_callback(value) {
        return Ok(Some(value.clone()));
    }
    if value.js_typeof() == "function" {
        return Err(type_error(format!(
            "{method} cannot use this callable as a handler; wrap it in an arrow function, e.g. (value) => tools.ns.tool(value)."
        )));
    }
    Ok(None)
}

fn instance_method(rt: &Rt, site: Site, name: &'static str, this: Value, args: Vec<Value>) -> JsResult<Value> {
    let method = format!("Promise.prototype.{name}");
    let promise = receiver(&this, &method, |o| o.is_promise())?;
    then_on(rt, site, name, &promise, args)
}

fn then_on(rt: &Rt, site: Site, name: &'static str, promise: &Obj, args: Vec<Value>) -> JsResult<Value> {
    let method = format!("Promise.prototype.{name}");
    if name == "finally" {
        let cleanup = reaction_handler(&arg(&args, 0), &method)?;
        return chain_finally(rt, promise, cleanup, site.node).map(Value::Object);
    }
    let on_fulfilled = if name == "then" { reaction_handler(&arg(&args, 0), &method)? } else { None };
    let on_rejected = reaction_handler(&arg(&args, if name == "then" { 1 } else { 0 }), &method)?;
    let made = capability(rt, site.node)?;
    let derived = made.promise.clone();
    rt.runner.then(promise, handler_reaction(on_fulfilled, on_rejected, Some(made)));
    Ok(Value::Object(derived))
}

// Promise.prototype.finally: the cleanup runs as a reaction, then the original outcome passes through once the
// cleanup's own result settles; a failing cleanup wins.
fn chain_finally(rt: &Rt, source: &Obj, cleanup: Option<Value>, site: Option<Loc>) -> JsResult<Obj> {
    let made = capability(rt, site)?;
    let promise = made.promise.clone();
    let Some(cleanup) = cleanup else {
        rt.runner.then(
            source,
            Reaction::new(move |settled| {
                let rt = super::rt();
                made.settle_with(&rt, settled);
            }),
        );
        return Ok(promise);
    };
    rt.runner.then(
        source,
        Reaction::new(move |settled: Exit| {
            let rt = super::rt();
            if matches!(settled, Err(Thrown::Interrupt)) {
                return made.settle_with(&rt, settled);
            }
            let rt2 = rt.clone();
            start(&rt, async move {
                let rt = rt2;
                let result = match call_value(&rt, &cleanup, Value::Undefined, Vec::new(), Site::default()).await {
                    Ok(r) => r,
                    Err(error) => return made.reject(&rt, error),
                };
                let after = match rt.runner.promise_resolve(&rt, result) {
                    Ok(p) => p,
                    Err(error) => return made.reject(&rt, error),
                };
                // The spec chains `then(valueThunk)` onto the cleanup's promise: one more job before the outcome passes.
                let derived = match capability(&rt, site) {
                    Ok(d) => d,
                    Err(error) => return made.reject(&rt, error),
                };
                let d2 = derived.clone();
                rt.runner.then(
                    &after,
                    Reaction::new(move |cleaned: Exit| {
                        let rt = super::rt();
                        match cleaned {
                            Ok(_) => d2.resolve(&rt, Value::Undefined),
                            Err(e) => d2.reject(&rt, e),
                        }
                    }),
                );
                rt.runner.then(
                    &derived.promise,
                    Reaction::new(move |cleaned: Exit| {
                        let rt = super::rt();
                        made.settle_with(&rt, if cleaned.is_ok() { settled } else { cleaned });
                    }),
                );
            });
        }),
    );
    Ok(promise)
}

fn construct_promise(rt: &Rt, site: Site, executor: Value) -> NResult {
    if !matches!(&executor, Value::Object(o) if o.closure().is_some()) {
        return NResult::err(type_error(
            "new Promise(...) expects an executor function (e.g. new Promise((resolve, reject) => { ... })).",
        ));
    }
    let made = match capability(rt, site.node) {
        Ok(m) => m,
        Err(e) => return NResult::err(e),
    };
    let (resolve, reject) = resolving_functions(rt, &made);
    let rt = rt.clone();
    NResult::future(async move {
        if let Err(error) = call_value(&rt, &executor, Value::Undefined, vec![resolve, reject], Site::default()).await {
            made.reject(&rt, error);
        }
        Ok(Value::Object(made.promise))
    })
}

/// Lets a synchronous object result also be awaited or chained like a Promise. It inherits non-enumerable
/// then/catch/finally that settle with a plain copy, so data views never show them and awaiting does not recurse.
pub fn promise_like(rt: &Rt, value: Value) -> Value {
    if let Value::Object(o) = &value
        && matches!(o.kind(), ObjKind::Ordinary)
        && o.proto().is_some_and(|p| Obj::ptr_eq(&p, &rt.builtins.object))
    {
        o.set_proto(Some(promise_like_prototype(rt)));
    }
    value
}

thread_local! {
    static PROMISE_LIKE: RefCell<Option<(usize, Obj)>> = const { RefCell::new(None) };
}

fn promise_like_prototype(rt: &Rt) -> Obj {
    let key = Rc::as_ptr(rt) as usize;
    if let Some(cached) = PROMISE_LIKE.with(|c| c.borrow().as_ref().filter(|(k, _)| *k == key).map(|(_, p)| p.clone())) {
        return cached;
    }
    let proto = Obj::ordinary(&rt.builtins.object);
    let settled = |rt: &Rt, this: &Value, name: &str| -> JsResult<Obj> {
        let source = receiver(this, name, |_| true)?;
        let copy = Obj::ordinary(&rt.builtins.object);
        {
            let props = source.0.props.borrow();
            let mut out = copy.0.props.borrow_mut();
            for (key, slot) in props.iter() {
                out.insert(key.clone(), slot.clone());
            }
        }
        rt.runner.promise_resolve(rt, Value::Object(copy))
    };
    let table = ["then", "catch", "finally"]
        .into_iter()
        .map(|name| {
            method(name, if name == "then" { 2 } else { 1 }, move |rt, site, this, args| {
                NResult::Ready(settled(rt, &this, name).and_then(|p| then_on(rt, site, name, &p, args)))
            })
        })
        .collect();
    methods(&rt.builtins.function, &proto, table);
    PROMISE_LIKE.with(|c| *c.borrow_mut() = Some((key, proto.clone())));
    proto
}

pub fn promise_global(rt: &Rt) -> Value {
    PROMISE_LIKE.with(|c| c.borrow_mut().take());
    let b = &rt.builtins;
    let proto = &b.promise;
    let promise = constructor(
        &b.function,
        proto,
        "Promise",
        1,
        NativeFn {
            call: requires_new("Promise"),
            construct: Some(Rc::new(|rt, site, args, _| construct_promise(rt, site, arg(&args, 0)))),
            callback: true,
        },
    );
    // Combinators are not callbacks: `[p].map(Promise.resolve)` must ask for an arrow function.
    for name in STATICS {
        let f = native(
            &b.function,
            name,
            1,
            NativeFn { call: Rc::new(move |rt, site, _, args| invoke_static(rt, site, name, args)), construct: None, callback: false },
        );
        define(&promise, name, Value::Object(f), HIDDEN);
    }
    methods(
        &b.function,
        proto,
        vec![
            method("then", 2, |rt, site, this, args| NResult::Ready(instance_method(rt, site, "then", this, args))),
            method("catch", 1, |rt, site, this, args| NResult::Ready(instance_method(rt, site, "catch", this, args))),
            method("finally", 1, |rt, site, this, args| NResult::Ready(instance_method(rt, site, "finally", this, args))),
        ],
    );
    if let Ok(Value::Object(then)) = crate::object::get(proto, &crate::value::PropKey::str("then")) {
        *rt.runner.intrinsic_then.borrow_mut() = Some(then);
    }
    Value::Object(promise)
}

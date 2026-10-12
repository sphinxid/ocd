//! Promises: supervised fibers, the pending-work registry, resolution, combinators, and reactions.
//! Port of `interpreter/promises.ts`.

use std::cell::{Cell, RefCell};
use std::collections::BTreeMap;
use std::rc::Rc;

use futures::future::LocalBoxFuture;
use indexmap::IndexMap;

use super::callback::{apply_collection_callback, is_supported_callback};
use super::errors::*;
use super::eval::{await_promise, call_value, materialize};
use super::exec::{executor, yield_now, Deferred, Exit, FiberCell, WaitAny};
use super::function::{arg, constructor, fn_, native, requires_new, receiver, method, methods, NResult, NativeFn};
use super::globals::{create_aggregate_error_value, normalize_error};
use super::iteration;
use super::limits::MAX_PENDING_PROMISES;
use super::{describe_value, Rt, Site};
use crate::object::{define, get, record, Obj, ObjKind, HIDDEN};
use crate::value::{PropKey, Value};

pub type SelfSlot = Rc<RefCell<Option<Obj>>>;

/// Observation only controls rejection reporting; program completion interrupts all promise work.
#[derive(Default)]
pub struct Pending {
    active: RefCell<IndexMap<usize, Obj>>,
    failures: RefCell<BTreeMap<u64, Diagnostic>>,
    next_id: Cell<u64>,
}

fn cell_of(promise: &Obj) -> Option<&Rc<FiberCell>> {
    match promise.kind() {
        ObjKind::Promise(c) => Some(c),
        _ => None,
    }
}

impl Pending {
    pub fn active_count(&self) -> usize {
        self.active.borrow().len()
    }

    /// Forks `body` as a promise that starts immediately.
    pub fn create(&self, rt: &Rt, site: Site, body: LocalBoxFuture<'static, JsResult<Value>>) -> JsResult<Obj> {
        if self.active.borrow().len() >= MAX_PENDING_PROMISES {
            return Err(range_error(format!(
                "Too many pending promises (limit {MAX_PENDING_PROMISES}); await promises before creating more."
            )));
        }
        // Allocate before forking so reruns get distinct IDs and diagnostics retain creation order.
        let id = self.next_id.get();
        self.next_id.set(id + 1);
        let node = site.node;
        let cell = executor().spawn(async move { body.await.map_err(|e| locate(e, node)) });
        let promise = Obj::new(Some(rt.builtins.promise.clone()), ObjKind::Promise(cell.clone()));
        self.active.borrow_mut().insert(cell.id, promise.clone());
        cell.promise_id.set(Some(id));
        let weak = Rc::downgrade(&cell);
        cell.add_observer(Box::new(move |exit: &Exit| {
            let rt = super::rt();
            let Some(cell) = weak.upgrade() else { return };
            rt.pending.active.borrow_mut().shift_remove(&cell.id);
            let interrupted = matches!(exit, Err(Thrown::Interrupt));
            if exit.is_ok() || interrupted || cell.observed.get() {
                cell.promise_id.set(None);
                return;
            }
            let Err(error) = exit else { return };
            let failure = normalize_error(error);
            rt.pending.failures.borrow_mut().insert(
                id,
                Diagnostic {
                    message: format!("Unhandled rejection from an un-awaited promise: {}", failure.message),
                    ..failure
                },
            );
        }));
        Ok(promise)
    }

    /// Resolution bodies need the promise's own identity to reject `resolve(promise)` self-resolution.
    pub fn create_with_self(
        &self,
        rt: &Rt,
        site: Site,
        body: impl FnOnce(SelfSlot) -> LocalBoxFuture<'static, JsResult<Value>>,
    ) -> JsResult<Obj> {
        let slot: SelfSlot = Rc::new(RefCell::new(None));
        let promise = self.create(rt, site, body(slot.clone()))?;
        *slot.borrow_mut() = Some(promise.clone());
        Ok(promise)
    }

    /// Observation must be recorded when responsibility transfers, before the consumer runs.
    pub fn mark_observed(&self, promise: &Obj) {
        if let Some(cell) = cell_of(promise) {
            cell.observed.set(true);
            if let Some(id) = cell.promise_id.take() {
                self.failures.borrow_mut().remove(&id);
            }
        }
    }

    /// Starts supervised background work that is not a promise (generator bodies).
    pub fn fork(&self, body: LocalBoxFuture<'static, JsResult<Value>>) {
        executor().spawn(body);
    }

    pub fn diagnostics(&self) -> Vec<Diagnostic> {
        self.failures.borrow().values().cloned().collect()
    }

    /// Interrupts every fiber still running and returns the unhandled-rejection warnings.
    pub fn interrupt(&self) -> Vec<Diagnostic> {
        let exec = executor();
        loop {
            let alive = exec.alive();
            if alive.is_empty() {
                break;
            }
            for id in alive {
                exec.interrupt(id);
            }
        }
        self.active.borrow_mut().clear();
        self.diagnostics()
    }
}

/// Await a promise's fiber without marking or yielding (`pending.await`).
async fn fiber_exit(promise: &Obj) -> Exit {
    match cell_of(promise) {
        Some(cell) => cell.wait().await,
        None => Ok(Value::Object(promise.clone())),
    }
}

fn capability(rt: &Rt, name: &str, settle: impl Fn(Value) + 'static) -> Value {
    Value::Object(fn_(&rt.builtins.function, name, 1, move |_, _, _, args| {
        settle(arg(&args, 0));
        NResult::ok(Value::Undefined)
    }))
}

/// Promise resolution: adopt promises and callable thenables recursively.
pub fn resolve_promise_value(rt: &Rt, value: Value, own: Option<SelfSlot>) -> LocalBoxFuture<'static, JsResult<Value>> {
    let rt = rt.clone();
    Box::pin(async move {
        if let (Some(own), Value::Object(o)) = (&own, &value)
            && let Some(p) = own.borrow().as_ref()
            && Obj::ptr_eq(p, o)
        {
            return Err(type_error("Chaining cycle detected: a promise cannot resolve with itself."));
        }
        let Value::Object(o) = &value else { return Ok(value) };
        if o.is_promise() {
            return await_promise(&rt, o).await;
        }
        let then = get(o, &PropKey::str("then"))?;
        if then.js_typeof() != "function" {
            return Ok(value);
        }
        // Promise resolution invokes a thenable's method in a later job.
        yield_now().await;
        let deferred = Deferred::new();
        let (d1, d2) = (deferred.clone(), deferred.clone());
        let resolve = capability(&rt, "resolve", move |v| {
            d1.complete(Ok(v));
        });
        let reject = capability(&rt, "reject", move |v| {
            d2.complete(Err(Thrown::Value(v)));
        });
        if let Err(e) = call_value(&rt, &then, value.clone(), vec![resolve, reject], Site::default()).await {
            if matches!(e, Thrown::Interrupt) {
                return Err(e);
            }
            deferred.complete(Err(e));
        }
        let settled = deferred.wait().await?;
        resolve_promise_value(&rt, settled, own).await
    })
}

pub fn resolve_promise(rt: &Rt, value: Value, site: Site) -> JsResult<Obj> {
    if let Value::Object(o) = &value
        && o.is_promise()
    {
        return Ok(o.clone());
    }
    let rt2 = rt.clone();
    rt.pending.create_with_self(rt, site, move |own| resolve_promise_value(&rt2, value, Some(own)))
}

/// NewPromiseCapability: a pending promise with the resolve/reject callables that settle it exactly once.
fn promise_capability(rt: &Rt, site: Site) -> JsResult<(Obj, Value, Value, Rc<Deferred>)> {
    let deferred = Deferred::new();
    let (d0, rt0) = (deferred.clone(), rt.clone());
    let promise = rt.pending.create_with_self(rt, site, move |own| {
        Box::pin(async move {
            let value = d0.wait().await?;
            resolve_promise_value(&rt0, value, Some(own)).await
        })
    })?;
    let (d1, d2) = (deferred.clone(), deferred.clone());
    let resolve = capability(rt, "resolve", move |v| {
        d1.complete(Ok(v));
    });
    let reject = capability(rt, "reject", move |v| {
        d2.complete(Err(Thrown::Value(v)));
    });
    Ok((promise, resolve, reject, deferred))
}

async fn settle_after_turn<T>(result: T) -> T {
    yield_now().await;
    result
}

const STATICS: [&str; 8] = ["all", "allSettled", "race", "any", "resolve", "reject", "withResolvers", "try"];

fn invoke_static(rt: &Rt, site: Site, name: &'static str, args: Vec<Value>) -> NResult {
    match name {
        "resolve" => NResult::Ready(resolve_promise(rt, arg(&args, 0), site).map(Value::Object)),
        "reject" => {
            let reason = arg(&args, 0);
            NResult::Ready(rt.pending.create(rt, site, Box::pin(async move { Err(Thrown::Value(reason)) })).map(Value::Object))
        }
        "withResolvers" => NResult::Ready(promise_capability(rt, site).map(|(promise, resolve, reject, _)| {
            Value::Object(record(&rt.builtins.object, vec![("promise", Value::Object(promise)), ("resolve", resolve), ("reject", reject)]))
        })),
        "try" => {
            let f = arg(&args, 0);
            if f.js_typeof() != "function" {
                return NResult::err(type_error(format!("Promise.try expects a function, received {}.", describe_value(&f))));
            }
            let rt = rt.clone();
            NResult::future(async move {
                let rest = args.get(1..).map(|s| s.to_vec()).unwrap_or_default();
                match call_value(&rt, &f, Value::Undefined, rest, site).await {
                    Ok(v) => resolve_promise(&rt, v, site).map(Value::Object),
                    Err(Thrown::Interrupt) => Err(Thrown::Interrupt),
                    Err(e) => rt.pending.create(&rt, site, Box::pin(async move { Err(e) })).map(Value::Object),
                }
            })
        }
        _ => {
            let rt2 = rt.clone();
            let source = arg(&args, 0);
            NResult::Ready(
                rt.pending
                    .create(rt, site, Box::pin(async move { combinator(&rt2, name, source, site).await }))
                    .map(Value::Object),
            )
        }
    }
}

async fn combinator(rt: &Rt, name: &'static str, source: Value, site: Site) -> JsResult<Value> {
    let cursor = iteration::iterate(rt, &source, None, site).await?;
    let Some(cursor) = cursor else {
        return Err(type_error(format!(
            "Promise.{name} expects a synchronous iterable, received {}.",
            describe_value(&source)
        )));
    };
    let mut items: Vec<Obj> = Vec::new();
    loop {
        let step = cursor.next().await?;
        if step.done {
            break;
        }
        let item = resolve_promise(rt, step.value, site)?;
        rt.pending.mark_observed(&item);
        items.push(item);
    }
    let cells: Vec<Rc<FiberCell>> = items.iter().filter_map(|i| cell_of(i).cloned()).collect();
    match name {
        "all" => {
            let failure: Rc<RefCell<Option<Thrown>>> = Rc::new(RefCell::new(None));
            for cell in &cells {
                let f = failure.clone();
                cell.add_observer(Box::new(move |exit: &Exit| {
                    if let Err(e) = exit
                        && f.borrow().is_none()
                    {
                        *f.borrow_mut() = Some(e.clone());
                    }
                }));
            }
            let (f, cs) = (failure.clone(), cells.clone());
            let outcome: JsResult<Vec<Value>> = WaitAny::new(cells.clone(), move || {
                if let Some(e) = f.borrow().clone() {
                    return Some(Err(e));
                }
                if cs.iter().all(|c| c.is_done()) {
                    return Some(Ok(cs.iter().map(|c| c.exit().unwrap().unwrap_or_default()).collect()));
                }
                None
            })
            .await;
            let outcome = settle_after_turn(outcome).await?;
            Ok(Value::Object(Obj::array(&rt.builtins.array, outcome)))
        }
        "allSettled" => {
            let mut outcomes = Vec::new();
            for item in &items {
                match fiber_exit(item).await {
                    Ok(v) => outcomes.push(Value::Object(record(
                        &rt.builtins.object,
                        vec![("status", Value::str("fulfilled")), ("value", v)],
                    ))),
                    Err(Thrown::Interrupt) => return Err(Thrown::Interrupt),
                    Err(e) => outcomes.push(Value::Object(record(
                        &rt.builtins.object,
                        vec![("status", Value::str("rejected")), ("reason", materialize(rt, &e))],
                    ))),
                }
            }
            yield_now().await;
            Ok(Value::Object(Obj::array(&rt.builtins.array, outcomes)))
        }
        "race" => {
            if items.is_empty() {
                return Err(type_error("Promise.race([]) would never settle; provide at least one promise or value."));
            }
            let winner: Rc<RefCell<Option<Exit>>> = Rc::new(RefCell::new(None));
            for cell in &cells {
                let w = winner.clone();
                cell.add_observer(Box::new(move |exit: &Exit| {
                    if w.borrow().is_none() {
                        *w.borrow_mut() = Some(exit.clone());
                    }
                }));
            }
            let w = winner.clone();
            let exit = WaitAny::new(cells.clone(), move || w.borrow().clone()).await;
            settle_after_turn(exit).await
        }
        _ => {
            // any
            let fulfilled: Rc<RefCell<Option<Exit>>> = Rc::new(RefCell::new(None));
            let reasons: Rc<RefCell<Vec<Option<Value>>>> = Rc::new(RefCell::new(vec![None; cells.len()]));
            for (index, cell) in cells.iter().enumerate() {
                let (f, r) = (fulfilled.clone(), reasons.clone());
                cell.add_observer(Box::new(move |exit: &Exit| {
                    if f.borrow().is_some() {
                        return;
                    }
                    match exit {
                        Ok(v) => *f.borrow_mut() = Some(Ok(v.clone())),
                        Err(Thrown::Interrupt) => *f.borrow_mut() = Some(Err(Thrown::Interrupt)),
                        Err(e) => r.borrow_mut()[index] = Some(materialize(&super::rt(), e)),
                    }
                }));
            }
            let (f, r) = (fulfilled.clone(), reasons.clone());
            let exit: Exit = WaitAny::new(cells.clone(), move || {
                if let Some(x) = f.borrow().clone() {
                    return Some(x);
                }
                if r.borrow().iter().all(|x| x.is_some()) {
                    let errors: Vec<Value> = r.borrow().iter().map(|x| x.clone().unwrap()).collect();
                    let rt = super::rt();
                    let value = create_aggregate_error_value(&rt, errors, "All promises were rejected", None);
                    return Some(Err(Thrown::Value(Value::Object(value))));
                }
                None
            })
            .await;
            settle_after_turn(exit).await
        }
    }
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

// Teardown bypasses handlers; settled reactions yield once so handlers never run inline.
async fn reaction_exit(source: &Obj) -> JsResult<Exit> {
    let exit = fiber_exit(source).await;
    if matches!(exit, Err(Thrown::Interrupt)) {
        return Err(Thrown::Interrupt);
    }
    yield_now().await;
    Ok(exit)
}

fn instance_method(rt: &Rt, site: Site, name: &'static str, this: Value, args: Vec<Value>) -> JsResult<Value> {
    let method = format!("Promise.prototype.{name}");
    let promise = receiver(&this, &method, |o| o.is_promise())?;
    rt.pending.mark_observed(&promise);
    if name == "finally" {
        let cleanup = reaction_handler(&arg(&args, 0), &method)?;
        let rt2 = rt.clone();
        let source = promise.clone();
        return rt
            .pending
            .create(
                rt,
                site,
                Box::pin(async move {
                    let exit = reaction_exit(&source).await?;
                    let Some(cleanup) = cleanup else { return exit };
                    let call = apply_collection_callback(&cleanup, &method)?;
                    let result = call(&rt2, Vec::new(), Value::Undefined, site).await?;
                    let rt3 = rt2.clone();
                    let intermediate = rt2.pending.create(
                        &rt2,
                        site,
                        Box::pin(async move {
                            let p = resolve_promise(&rt3, result, site)?;
                            await_promise(&rt3, &p).await?;
                            exit
                        }),
                    )?;
                    await_promise(&rt2, &intermediate).await
                }),
            )
            .map(Value::Object);
    }
    let on_fulfilled = if name == "then" { reaction_handler(&arg(&args, 0), &method)? } else { None };
    let on_rejected = reaction_handler(&arg(&args, if name == "then" { 1 } else { 0 }), &method)?;
    let rt2 = rt.clone();
    rt.pending
        .create_with_self(rt, site, move |own| {
            Box::pin(async move {
                let exit = reaction_exit(&promise).await?;
                let handler = if exit.is_ok() { on_fulfilled } else { on_rejected };
                let Some(handler) = handler else { return exit };
                let input = match &exit {
                    Ok(v) => v.clone(),
                    Err(e) => materialize(&rt2, e),
                };
                let call = apply_collection_callback(&handler, &method)?;
                let result = call(&rt2, vec![input], Value::Undefined, site).await?;
                resolve_promise_value(&rt2, result, Some(own)).await
            })
        })
        .map(Value::Object)
}

fn construct_promise(rt: &Rt, site: Site, executor: Value) -> NResult {
    if !matches!(&executor, Value::Object(o) if o.closure().is_some()) {
        return NResult::err(type_error(
            "new Promise(...) expects an executor function (e.g. new Promise((resolve, reject) => { ... })).",
        ));
    }
    let rt = rt.clone();
    NResult::future(async move {
        let (promise, resolve, reject, deferred) = promise_capability(&rt, site)?;
        match call_value(&rt, &executor, Value::Undefined, vec![resolve, reject], site).await {
            Ok(_) => {}
            Err(Thrown::Interrupt) => return Err(Thrown::Interrupt),
            Err(e) => {
                deferred.complete(Err(e));
            }
        }
        Ok(Value::Object(promise))
    })
}

pub fn promise_global(rt: &Rt) -> Value {
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
            NativeFn {
                call: Rc::new(move |rt, site, _, args| invoke_static(rt, site, name, args)),
                construct: None,
                callback: false,
            },
        );
        define(&promise, name, Value::Object(f), HIDDEN);
    }
    methods(
        &b.function,
        proto,
        vec![
            method("then", 2, |rt, site, this, args| NResult::Ready(instance_method(rt, site, "then", this, args))),
            method("catch", 1, |rt, site, this, args| NResult::Ready(instance_method(rt, site, "catch", this, args))),
            method("finally", 1, |rt, site, this, args| {
                NResult::Ready(instance_method(rt, site, "finally", this, args))
            }),
        ],
    );
    Value::Object(promise)
}

//! `Iterator`, `Iterator.prototype` helpers, and `IteratorHelper.prototype.return`. Port of `stdlib/iterator.ts`.
//!
//! Every built-in iterator and helper shares the Iterator prototype; JS gives each collection its own, which is only
//! observable through getPrototypeOf.

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use futures::future::LocalBoxFuture;

use crate::interp::callback::apply_collection_callback;
use crate::interp::coerce::coerce_to_number;
use crate::interp::errors::{range_error, type_error, JsResult, Thrown};
use crate::interp::function::{arg, constructor, fn_, method, methods, receiver, requires_new, NResult, NativeFn};
use crate::interp::iteration::{self, iter_result, iterate_direct, Cursor, CursorRef, Step};
use crate::interp::{describe_value, Rt, Site};
use crate::object::{define, has_prototype, Obj, ObjKind, HIDDEN};
use crate::value::Value;

macro_rules! tri {
    ($e:expr) => {
        match $e {
            Ok(v) => v,
            Err(e) => return NResult::err(e),
        }
    };
}

type Pull = Rc<dyn Fn() -> LocalBoxFuture<'static, JsResult<Step>>>;
type Close = Rc<dyn Fn() -> LocalBoxFuture<'static, JsResult<()>>>;

fn method_name(name: &str) -> String {
    format!("Iterator.prototype.{name}")
}

/// A step that ran program code: when it threw, close the source first, as IteratorClose does.
async fn guarded<T>(source: &CursorRef, result: JsResult<T>) -> JsResult<T> {
    match result {
        Err(e) if !matches!(e, Thrown::Interrupt) => {
            let _ = source.close().await;
            Err(e)
        }
        other => other,
    }
}

struct HelperState {
    done: Cell<bool>,
    running: Cell<bool>,
    pull: RefCell<Option<Pull>>,
    close: RefCell<Option<Close>>,
}

impl HelperState {
    fn enter(&self) -> JsResult<()> {
        if self.running.get() {
            return Err(type_error("Iterator helper is already running."));
        }
        self.running.set(true);
        Ok(())
    }
}

/// A lazy helper: `pull` advances it once. Once it reports done, throws, or is closed, it stays done. A callback that
/// re-enters its own helper is a TypeError, as for a running generator.
struct HelperCursor(Rc<HelperState>);

impl Cursor for HelperCursor {
    fn next(&self) -> LocalBoxFuture<'static, JsResult<Step>> {
        let state = self.0.clone();
        Box::pin(async move {
            if state.done.get() {
                return Ok(Step::done());
            }
            state.enter()?;
            let pull = state.pull.borrow().clone();
            let result = match pull {
                Some(pull) => pull().await,
                None => Ok(Step::done()),
            };
            state.running.set(false);
            state.done.set(match &result {
                Ok(step) => step.done,
                Err(_) => true,
            });
            result
        })
    }
    fn close(&self) -> LocalBoxFuture<'static, JsResult<()>> {
        let state = self.0.clone();
        Box::pin(async move {
            if state.done.get() {
                return Ok(());
            }
            state.enter()?;
            state.done.set(true);
            let close = state.close.borrow().clone();
            let result = match close {
                Some(close) => close().await,
                None => Ok(()),
            };
            state.running.set(false);
            result
        })
    }
    fn teardown(&self) -> Box<dyn std::any::Any> {
        Box::new((self.0.pull.borrow_mut().take(), self.0.close.borrow_mut().take()))
    }
}

fn source_close(source: &CursorRef) -> Close {
    let source = source.clone();
    Rc::new(move || source.close())
}

fn helper(rt: &Rt, pull: Pull, close: Close) -> Value {
    let cursor: CursorRef = Rc::new(HelperCursor(Rc::new(HelperState {
        done: Cell::new(false),
        running: Cell::new(false),
        pull: RefCell::new(Some(pull)),
        close: RefCell::new(Some(close)),
    })));
    Value::Object(Obj::new(Some(rt.builtins.iterator_helper.clone()), ObjKind::Iterator(cursor)))
}

/// GetIteratorFlattenable: an iterable by `[Symbol.iterator]`, otherwise the object itself as an iterator.
async fn flattenable(rt: &Rt, value: Value, name: &str, strings: bool, site: Site) -> JsResult<CursorRef> {
    let rejected = match &value {
        Value::String(_) => !strings,
        Value::Object(_) => false,
        _ => true,
    };
    if rejected {
        return Err(type_error(format!(
            "{name} expects an iterable or iterator, received {}.",
            describe_value(&value)
        )));
    }
    if let Some(cursor) = iteration::iterate(rt, &value, None, site).await? {
        return Ok(cursor);
    }
    iterate_direct(rt, &value, None, site)
}

fn limit(value: &Value, name: &str) -> JsResult<f64> {
    let count = coerce_to_number(value).trunc();
    if count.is_nan() || count < 0.0 {
        return Err(range_error(format!(
            "{} expects a non-negative count, received {}.",
            method_name(name),
            crate::number::to_string(count)
        )));
    }
    Ok(count)
}

fn next_index(counter: &Cell<usize>) -> Value {
    let index = counter.get();
    counter.set(index + 1);
    Value::Number(index as f64)
}

pub fn iterator_globals(rt: &Rt) -> Value {
    let b = &rt.builtins;
    let proto = b.iterator.clone();
    methods(
        &b.function,
        &proto,
        vec![
            method("next", 0, |rt, _, this, _| {
                let target = tri!(receiver(&this, &method_name("next"), |o| matches!(o.kind(), ObjKind::Iterator(_))));
                let ObjKind::Iterator(cursor) = target.kind() else { unreachable!() };
                let (rt, cursor) = (rt.clone(), cursor.clone());
                NResult::future(async move {
                    let step = cursor.next().await?;
                    Ok(iter_result(&rt, step.value, step.done))
                })
            }),
            method("map", 1, |rt, site, this, args| {
                let source = tri!(iterate_direct(rt, &this, None, site));
                let apply = Rc::new(tri!(apply_collection_callback(&arg(&args, 0), &method_name("map"))));
                let counter = Rc::new(Cell::new(0usize));
                let (rt2, src) = (rt.clone(), source.clone());
                let pull: Pull = Rc::new(move || {
                    let (rt, source, apply, counter) = (rt2.clone(), src.clone(), apply.clone(), counter.clone());
                    Box::pin(async move {
                        let step = source.next().await?;
                        if step.done {
                            return Ok(Step::done());
                        }
                        let mapped = apply(&rt, vec![step.value, next_index(&counter)], Value::Undefined, site).await;
                        Ok(Step::value(guarded(&source, mapped).await?))
                    })
                });
                NResult::ok(helper(rt, pull, source_close(&source)))
            }),
            method("filter", 1, |rt, site, this, args| {
                let source = tri!(iterate_direct(rt, &this, None, site));
                let apply = Rc::new(tri!(apply_collection_callback(&arg(&args, 0), &method_name("filter"))));
                let counter = Rc::new(Cell::new(0usize));
                let (rt2, src) = (rt.clone(), source.clone());
                let pull: Pull = Rc::new(move || {
                    let (rt, source, apply, counter) = (rt2.clone(), src.clone(), apply.clone(), counter.clone());
                    Box::pin(async move {
                        loop {
                            let step = source.next().await?;
                            if step.done {
                                return Ok(Step::done());
                            }
                            let kept =
                                apply(&rt, vec![step.value.clone(), next_index(&counter)], Value::Undefined, site).await;
                            if guarded(&source, kept).await?.truthy() {
                                return Ok(step);
                            }
                        }
                    })
                });
                NResult::ok(helper(rt, pull, source_close(&source)))
            }),
            method("take", 1, |rt, site, this, args| {
                let source = tri!(iterate_direct(rt, &this, None, site));
                let remaining = Rc::new(Cell::new(tri!(limit(&arg(&args, 0), "take"))));
                let src = source.clone();
                let pull: Pull = Rc::new(move || {
                    let (source, remaining) = (src.clone(), remaining.clone());
                    Box::pin(async move {
                        if remaining.get() == 0.0 {
                            source.close().await?;
                            return Ok(Step::done());
                        }
                        remaining.set(remaining.get() - 1.0);
                        source.next().await
                    })
                });
                NResult::ok(helper(rt, pull, source_close(&source)))
            }),
            method("drop", 1, |rt, site, this, args| {
                let source = tri!(iterate_direct(rt, &this, None, site));
                let remaining = Rc::new(Cell::new(tri!(limit(&arg(&args, 0), "drop"))));
                let src = source.clone();
                let pull: Pull = Rc::new(move || {
                    let (source, remaining) = (src.clone(), remaining.clone());
                    Box::pin(async move {
                        while remaining.get() > 0.0 {
                            remaining.set(remaining.get() - 1.0);
                            let step = source.next().await?;
                            if step.done {
                                return Ok(Step::done());
                            }
                        }
                        source.next().await
                    })
                });
                NResult::ok(helper(rt, pull, source_close(&source)))
            }),
            method("flatMap", 1, |rt, site, this, args| {
                let source = tri!(iterate_direct(rt, &this, None, site));
                let apply = Rc::new(tri!(apply_collection_callback(&arg(&args, 0), &method_name("flatMap"))));
                let counter = Rc::new(Cell::new(0usize));
                let inner: Rc<RefCell<Option<CursorRef>>> = Rc::new(RefCell::new(None));
                let (rt2, src, inner2) = (rt.clone(), source.clone(), inner.clone());
                let pull: Pull = Rc::new(move || {
                    let (rt, source, apply, counter, inner) =
                        (rt2.clone(), src.clone(), apply.clone(), counter.clone(), inner2.clone());
                    Box::pin(async move {
                        loop {
                            let current = inner.borrow().clone();
                            if let Some(current) = current {
                                let step = guarded(&source, current.next().await).await?;
                                if !step.done {
                                    return Ok(step);
                                }
                                *inner.borrow_mut() = None;
                            }
                            let step = source.next().await?;
                            if step.done {
                                return Ok(Step::done());
                            }
                            let flattened = async {
                                let mapped =
                                    apply(&rt, vec![step.value, next_index(&counter)], Value::Undefined, site).await?;
                                flattenable(&rt, mapped, &method_name("flatMap"), false, site).await
                            }
                            .await;
                            let cursor = guarded(&source, flattened).await?;
                            *inner.borrow_mut() = Some(cursor);
                        }
                    })
                });
                let src = source.clone();
                let close: Close = Rc::new(move || {
                    let (source, current) = (src.clone(), inner.borrow().clone());
                    Box::pin(async move {
                        if let Some(current) = current {
                            current.close().await?;
                        }
                        source.close().await
                    })
                });
                NResult::ok(helper(rt, pull, close))
            }),
            method("reduce", 1, |rt, site, this, args| {
                let source = tri!(iterate_direct(rt, &this, None, site));
                let apply = tri!(apply_collection_callback(&arg(&args, 0), &method_name("reduce")));
                let rt = rt.clone();
                NResult::future(async move {
                    let mut counter = 0usize;
                    let mut accumulator = arg(&args, 1);
                    if args.len() < 2 {
                        let first = source.next().await?;
                        if first.done {
                            return Err(type_error(
                                "Iterator.prototype.reduce of an empty iterator with no initial value.",
                            ));
                        }
                        accumulator = first.value;
                        counter = 1;
                    }
                    loop {
                        let step = source.next().await?;
                        if step.done {
                            return Ok(accumulator);
                        }
                        let index = Value::Number(counter as f64);
                        counter += 1;
                        let reduced = apply(&rt, vec![accumulator, step.value, index], Value::Undefined, site).await;
                        accumulator = guarded(&source, reduced).await?;
                    }
                })
            }),
            method("toArray", 0, |rt, site, this, _| {
                let source = tri!(iterate_direct(rt, &this, None, site));
                let rt = rt.clone();
                NResult::future(async move {
                    let mut items = Vec::new();
                    loop {
                        let step = source.next().await?;
                        if step.done {
                            return Ok(Value::Object(Obj::array(&rt.builtins.array, items)));
                        }
                        items.push(step.value);
                    }
                })
            }),
            method("forEach", 1, |rt, site, this, args| {
                let source = tri!(iterate_direct(rt, &this, None, site));
                let apply = tri!(apply_collection_callback(&arg(&args, 0), &method_name("forEach")));
                let rt = rt.clone();
                NResult::future(async move {
                    let counter = Cell::new(0usize);
                    loop {
                        let step = source.next().await?;
                        if step.done {
                            return Ok(Value::Undefined);
                        }
                        let called = apply(&rt, vec![step.value, next_index(&counter)], Value::Undefined, site).await;
                        guarded(&source, called).await?;
                    }
                })
            }),
            method("some", 1, |rt, site, this, args| {
                let source = tri!(iterate_direct(rt, &this, None, site));
                let apply = tri!(apply_collection_callback(&arg(&args, 0), &method_name("some")));
                let rt = rt.clone();
                NResult::future(async move {
                    let counter = Cell::new(0usize);
                    loop {
                        let step = source.next().await?;
                        if step.done {
                            return Ok(Value::Bool(false));
                        }
                        let called = apply(&rt, vec![step.value, next_index(&counter)], Value::Undefined, site).await;
                        if guarded(&source, called).await?.truthy() {
                            source.close().await?;
                            return Ok(Value::Bool(true));
                        }
                    }
                })
            }),
            method("every", 1, |rt, site, this, args| {
                let source = tri!(iterate_direct(rt, &this, None, site));
                let apply = tri!(apply_collection_callback(&arg(&args, 0), &method_name("every")));
                let rt = rt.clone();
                NResult::future(async move {
                    let counter = Cell::new(0usize);
                    loop {
                        let step = source.next().await?;
                        if step.done {
                            return Ok(Value::Bool(true));
                        }
                        let called = apply(&rt, vec![step.value, next_index(&counter)], Value::Undefined, site).await;
                        if !guarded(&source, called).await?.truthy() {
                            source.close().await?;
                            return Ok(Value::Bool(false));
                        }
                    }
                })
            }),
            method("find", 1, |rt, site, this, args| {
                let source = tri!(iterate_direct(rt, &this, None, site));
                let apply = tri!(apply_collection_callback(&arg(&args, 0), &method_name("find")));
                let rt = rt.clone();
                NResult::future(async move {
                    let counter = Cell::new(0usize);
                    loop {
                        let step = source.next().await?;
                        if step.done {
                            return Ok(Value::Undefined);
                        }
                        let called =
                            apply(&rt, vec![step.value.clone(), next_index(&counter)], Value::Undefined, site).await;
                        if guarded(&source, called).await?.truthy() {
                            source.close().await?;
                            return Ok(step.value);
                        }
                    }
                })
            }),
        ],
    );

    // Helpers and `Iterator.from` wrappers can be closed; collection iterators, as in JS, cannot.
    methods(
        &b.function,
        &b.iterator_helper,
        vec![method("return", 0, |rt, _, this, _| {
            let target =
                tri!(receiver(&this, "Iterator.prototype.return", |o| matches!(o.kind(), ObjKind::Iterator(_))));
            let ObjKind::Iterator(cursor) = target.kind() else { unreachable!() };
            let (rt, cursor) = (rt.clone(), cursor.clone());
            NResult::future(async move {
                cursor.close().await?;
                Ok(iter_result(&rt, Value::Undefined, true))
            })
        })],
    );

    let iterator = constructor(
        &b.function,
        &proto,
        "Iterator",
        0,
        NativeFn {
            call: requires_new("Iterator"),
            construct: Some(Rc::new(|_, _, _, _| {
                NResult::err(type_error("Iterator is abstract; use Iterator.from(...) or a built-in iterator."))
            })),
            callback: true,
        },
    );
    let from = fn_(&b.function, "from", 1, |rt, site, _, args| {
        let value = arg(&args, 0);
        if matches!(value, Value::Object(_)) && has_prototype(&value, &rt.builtins.iterator) {
            return NResult::ok(value);
        }
        let rt = rt.clone();
        NResult::future(async move {
            let cursor = flattenable(&rt, value, "Iterator.from", true, site).await?;
            Ok(Value::Object(Obj::new(Some(rt.builtins.iterator_helper.clone()), ObjKind::Iterator(cursor))))
        })
    });
    define(&iterator, "from", Value::Object(from), HIDDEN);
    Value::Object(iterator)
}

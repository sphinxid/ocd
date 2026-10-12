//! Iteration: the `Cursor` the interpreter drives for `for...of`, spread, and every synchronous consumer, over
//! built-in collections (live, like JS iterators) and program-defined iterators.

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use futures::future::LocalBoxFuture;

use super::errors::{type_error, type_error_at, JsResult};
use super::eval::call_value;
use super::exec::yield_now;
use super::{Rt, Site};
use crate::ast::Loc;
use crate::object::{get, record, Obj, ObjKind};
use crate::value::{JsStr, PropKey, Sym, Value};

/// One pull from an iterator, as `for...of` sees it.
#[derive(Clone, Debug)]
pub struct Step {
    pub done: bool,
    pub value: Value,
}

impl Step {
    pub fn done() -> Step {
        Step { done: true, value: Value::Undefined }
    }
    pub fn value(value: Value) -> Step {
        Step { done: false, value }
    }
}

/// How the interpreter drives any iterator: pull the next step, or close it early.
pub trait Cursor {
    fn next(&self) -> LocalBoxFuture<'static, JsResult<Step>>;
    fn close(&self) -> LocalBoxFuture<'static, JsResult<()>>;
    fn teardown(&self) -> Box<dyn std::any::Any> {
        Box::new(())
    }
}

pub type CursorRef = Rc<dyn Cursor>;

fn ready<T: 'static>(value: T) -> LocalBoxFuture<'static, T> {
    Box::pin(std::future::ready(value))
}

/// Which part of an entry a collection iterator yields.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Keys,
    Values,
    Entries,
}

/// A live array iterator (`array.values()`): reads by index and re-checks the length every step.
pub struct ArrayCursor {
    array: RefCell<Option<Obj>>,
    index: Cell<usize>,
    mode: Mode,
    array_proto: Obj,
}

impl ArrayCursor {
    pub fn new(rt: &Rt, array: Obj, mode: Mode) -> Rc<dyn Cursor> {
        Rc::new(ArrayCursor {
            array: RefCell::new(Some(array)),
            index: Cell::new(0),
            mode,
            array_proto: rt.builtins.array.clone(),
        })
    }
}

impl Cursor for ArrayCursor {
    fn next(&self) -> LocalBoxFuture<'static, JsResult<Step>> {
        let Some(array) = self.array.borrow().clone() else { return ready(Ok(Step::done())) };
        let i = self.index.get();
        let item = match array.kind() {
            ObjKind::Array(a) => {
                let data = a.borrow();
                if i >= data.items.len() { None } else { Some(data.items[i].clone().unwrap_or_default()) }
            }
            ObjKind::Bytes(b) => b.get(i).map(|x| Value::Number(x as f64)),
            _ => None,
        };
        let Some(item) = item else {
            *self.array.borrow_mut() = None;
            return ready(Ok(Step::done()));
        };
        self.index.set(i + 1);
        let value = match self.mode {
            Mode::Values => item,
            Mode::Keys => Value::Number(i as f64),
            Mode::Entries => Value::Object(Obj::array(&self.array_proto, vec![Value::Number(i as f64), item])),
        };
        ready(Ok(Step::value(value)))
    }
    fn close(&self) -> LocalBoxFuture<'static, JsResult<()>> {
        ready(Ok(()))
    }
    fn teardown(&self) -> Box<dyn std::any::Any> {
        Box::new(self.array.borrow_mut().take())
    }
}

/// A live Map/Set iterator over insertion order (tombstones keep the position stable).
pub struct MapCursor {
    target: RefCell<Option<Obj>>,
    index: Cell<usize>,
    mode: Mode,
    array_proto: Obj,
}

impl MapCursor {
    pub fn new(rt: &Rt, target: Obj, mode: Mode) -> Rc<dyn Cursor> {
        Rc::new(MapCursor {
            target: RefCell::new(Some(target)),
            index: Cell::new(0),
            mode,
            array_proto: rt.builtins.array.clone(),
        })
    }
}

impl Cursor for MapCursor {
    fn next(&self) -> LocalBoxFuture<'static, JsResult<Step>> {
        let Some(target) = self.target.borrow().clone() else { return ready(Ok(Step::done())) };
        let (data, is_set) = match target.kind() {
            ObjKind::Map(m) => (m, false),
            ObjKind::Set(s) => (s, true),
            _ => return ready(Ok(Step::done())),
        };
        let found = {
            let d = data.borrow();
            let mut i = self.index.get();
            let mut found = None;
            while i < d.entries.len() {
                if let Some((k, v)) = &d.entries[i] {
                    found = Some((i, k.clone(), v.clone()));
                    break;
                }
                i += 1;
            }
            found
        };
        match found {
            None => {
                *self.target.borrow_mut() = None;
                ready(Ok(Step::done()))
            }
            Some((i, k, v)) => {
                self.index.set(i + 1);
                let value = match (self.mode, is_set) {
                    (Mode::Keys, _) | (Mode::Values, true) => k,
                    (Mode::Values, false) => v,
                    (Mode::Entries, true) => Value::Object(Obj::array(&self.array_proto, vec![k.clone(), k])),
                    (Mode::Entries, false) => Value::Object(Obj::array(&self.array_proto, vec![k, v])),
                };
                ready(Ok(Step::value(value)))
            }
        }
    }
    fn close(&self) -> LocalBoxFuture<'static, JsResult<()>> {
        ready(Ok(()))
    }
    fn teardown(&self) -> Box<dyn std::any::Any> {
        Box::new(self.target.borrow_mut().take())
    }
}

/// A cursor over a fixed list (string code points, `arguments`, snapshots).
pub struct ListCursor {
    items: RefCell<std::collections::VecDeque<Value>>,
}

impl ListCursor {
    pub fn new(items: Vec<Value>) -> Rc<dyn Cursor> {
        Rc::new(ListCursor { items: RefCell::new(items.into()) })
    }
}

impl Cursor for ListCursor {
    fn next(&self) -> LocalBoxFuture<'static, JsResult<Step>> {
        let item = self.items.borrow_mut().pop_front();
        ready(Ok(match item {
            Some(v) => Step::value(v),
            None => Step::done(),
        }))
    }
    fn close(&self) -> LocalBoxFuture<'static, JsResult<()>> {
        ready(Ok(()))
    }
    fn teardown(&self) -> Box<dyn std::any::Any> {
        Box::new(std::mem::take(&mut *self.items.borrow_mut()))
    }
}

/// A cursor whose steps are produced by closures (helpers, generated sequences).
pub struct FnCursor {
    pub next: RefCell<Option<Rc<dyn Fn() -> LocalBoxFuture<'static, JsResult<Step>>>>>,
    pub close: RefCell<Option<Rc<dyn Fn() -> LocalBoxFuture<'static, JsResult<()>>>>>,
}

impl FnCursor {
    pub fn new(
        next: impl Fn() -> LocalBoxFuture<'static, JsResult<Step>> + 'static,
        close: impl Fn() -> LocalBoxFuture<'static, JsResult<()>> + 'static,
    ) -> Rc<dyn Cursor> {
        Rc::new(FnCursor { next: RefCell::new(Some(Rc::new(next))), close: RefCell::new(Some(Rc::new(close))) })
    }
}

impl Cursor for FnCursor {
    fn next(&self) -> LocalBoxFuture<'static, JsResult<Step>> {
        match self.next.borrow().clone() {
            Some(f) => f(),
            None => ready(Ok(Step::done())),
        }
    }
    fn close(&self) -> LocalBoxFuture<'static, JsResult<()>> {
        match self.close.borrow().clone() {
            Some(f) => f(),
            None => ready(Ok(())),
        }
    }
    fn teardown(&self) -> Box<dyn std::any::Any> {
        Box::new((self.next.borrow_mut().take(), self.close.borrow_mut().take()))
    }
}

/// Code points of a string, as `for...of` yields them.
pub fn string_code_points(s: &JsStr) -> Vec<Value> {
    let u = s.units();
    let mut out = Vec::new();
    let mut i = 0;
    while i < u.len() {
        let c = u[i];
        if (0xD800..0xDC00).contains(&c) && i + 1 < u.len() && (0xDC00..0xE000).contains(&u[i + 1]) {
            out.push(Value::String(JsStr::from_slice(&u[i..i + 2])));
            i += 2;
        } else {
            out.push(Value::String(JsStr::from_slice(&u[i..i + 1])));
            i += 1;
        }
    }
    out
}

/// The built-in iteration `for...of` and spread use, when this kind of value has one.
pub fn builtin_cursor(rt: &Rt, value: &Value) -> Option<Rc<dyn Cursor>> {
    match value {
        Value::String(s) => Some(ListCursor::new(string_code_points(s))),
        Value::Object(o) => match o.kind() {
            ObjKind::Iterator(c) => Some(c.clone()),
            ObjKind::Array(_) | ObjKind::Bytes(_) => Some(ArrayCursor::new(rt, o.clone(), Mode::Values)),
            ObjKind::Arguments => {
                let keys = crate::object::keys(o);
                let values = keys
                    .into_iter()
                    .map(|k| get(o, &PropKey::Str(k)).unwrap_or_default())
                    .collect();
                Some(ListCursor::new(values))
            }
            ObjKind::Map(_) => Some(MapCursor::new(rt, o.clone(), Mode::Entries)),
            ObjKind::Set(_) => Some(MapCursor::new(rt, o.clone(), Mode::Values)),
            ObjKind::URLSearchParams(p) => Some(crate::stdlib::url::params_cursor(rt, p.clone(), Mode::Entries)),
            ObjKind::Headers(_) => Some(crate::stdlib::headers::headers_cursor(rt, o.clone(), Mode::Entries)),
            _ => None,
        },
        _ => None,
    }
}

/// A program-defined iterator: the object, its `next`, and whether it came from `Symbol.asyncIterator`.
#[derive(Clone)]
pub struct CustomIterator {
    pub iterator: Obj,
    pub next: Value,
    pub asynchronous: bool,
}

pub fn require_iterator_object(value: Value, context: &str, node: Option<Loc>) -> JsResult<Obj> {
    match value {
        Value::Object(o) => Ok(o),
        _ => Err(located(type_error(format!("{context} must be an object.")), node)),
    }
}

pub fn require_iterator_method(value: Value, context: &str, node: Option<Loc>) -> JsResult<Value> {
    if value.js_typeof() == "function" {
        return Ok(value);
    }
    Err(located(type_error(format!("{context} must be a function.")), node))
}

fn located(e: super::errors::Thrown, node: Option<Loc>) -> super::errors::Thrown {
    match node {
        Some(n) => super::errors::locate(e, Some(n)),
        None => e,
    }
}

/// GetIterator via `[Symbol.asyncIterator]` (when allowed) or `[Symbol.iterator]`.
pub async fn custom_iterator(
    rt: &Rt,
    value: &Value,
    node: Option<Loc>,
    allow_async: bool,
    site: Site,
) -> JsResult<Option<CustomIterator>> {
    let Value::Object(o) = value else { return Ok(None) };
    let async_method = if allow_async { get(o, &PropKey::Sym(Sym::AsyncIterator))? } else { Value::Undefined };
    let method = if !async_method.is_nullish() { async_method.clone() } else { get(o, &PropKey::Sym(Sym::Iterator))? };
    if method.is_nullish() {
        return Ok(None);
    }
    let method = require_iterator_method(method, "Iterator method", node)?;
    let site = Site { node: node.or(site.node), depth: site.depth };
    let iterator = call_value(rt, &method, value.clone(), Vec::new(), site).await?;
    let object = require_iterator_object(iterator, "Iterator method result", node)?;
    let next = require_iterator_method(get(&object, &PropKey::str("next"))?, "Iterator next", node)?;
    Ok(Some(CustomIterator { iterator: object, next, asynchronous: !async_method.is_nullish() }))
}

/// One step of a program-defined iterator. `awaiting` is the `for await` adaptation of a sync iterator.
pub async fn next_iterator_result(
    rt: &Rt,
    iterator: &CustomIterator,
    node: Option<Loc>,
    awaiting: bool,
    site: Site,
) -> JsResult<Step> {
    let site = Site { node: node.or(site.node), depth: site.depth };
    if iterator.asynchronous {
        let called = call_value(rt, &iterator.next, Value::Object(iterator.iterator.clone()), Vec::new(), site).await?;
        let settled = super::eval::await_value(rt, called).await?;
        let object = require_iterator_object(settled, "Iterator next() result", node)?;
        let done = get(&object, &PropKey::str("done"))?.truthy();
        return Ok(Step { done, value: get(&object, &PropKey::str("value"))? });
    }
    let called = call_value(rt, &iterator.next, Value::Object(iterator.iterator.clone()), Vec::new(), site).await;
    let called = match called {
        Ok(v) => v,
        Err(e) => {
            if awaiting {
                yield_now().await;
            }
            return Err(e);
        }
    };
    let captured = (|| -> JsResult<Step> {
        let object = require_iterator_object(called, "Iterator next() result", node)?;
        Ok(Step { done: get(&object, &PropKey::str("done"))?.truthy(), value: get(&object, &PropKey::str("value"))? })
    })();
    let captured = match captured {
        Ok(s) => s,
        Err(e) => {
            if awaiting {
                yield_now().await;
            }
            return Err(e);
        }
    };
    if !awaiting {
        return Ok(captured);
    }
    let value = await_async_from_sync_value(rt, iterator, captured.value, node, !captured.done, site).await?;
    Ok(Step { done: captured.done, value })
}

pub async fn await_async_from_sync_value(
    rt: &Rt,
    iterator: &CustomIterator,
    value: Value,
    node: Option<Loc>,
    close_on_rejection: bool,
    site: Site,
) -> JsResult<Value> {
    match super::eval::await_value(rt, value).await {
        Ok(v) => Ok(v),
        Err(e) => {
            if close_on_rejection && !matches!(e, super::errors::Thrown::Interrupt) {
                let _ = close_iterator(rt, iterator, node, false, site).await;
            }
            Err(e)
        }
    }
}

/// IteratorClose (sync) or AsyncIteratorClose.
pub async fn close_iterator(
    rt: &Rt,
    iterator: &CustomIterator,
    node: Option<Loc>,
    awaiting: bool,
    site: Site,
) -> JsResult<()> {
    let close = get(&iterator.iterator, &PropKey::str("return"))?;
    if close.is_nullish() {
        if iterator.asynchronous || !awaiting {
            return Ok(());
        }
        yield_now().await;
        return Ok(());
    }
    let site = Site { node: node.or(site.node), depth: site.depth };
    let method = require_iterator_method(close, "Iterator return", node)?;
    if iterator.asynchronous {
        let called = call_value(rt, &method, Value::Object(iterator.iterator.clone()), Vec::new(), site).await?;
        let settled = super::eval::await_value(rt, called).await?;
        require_iterator_object(settled, "Iterator return() result", node)?;
        return Ok(());
    }
    let called = call_value(rt, &method, Value::Object(iterator.iterator.clone()), Vec::new(), site).await;
    let called = match called {
        Ok(v) => v,
        Err(e) => {
            if awaiting {
                yield_now().await;
            }
            return Err(e);
        }
    };
    let captured = require_iterator_object(called, "Iterator return() result", node)
        .and_then(|o| get(&o, &PropKey::str("value")));
    let captured = match captured {
        Ok(v) => v,
        Err(e) => {
            if awaiting {
                yield_now().await;
            }
            return Err(e);
        }
    };
    if awaiting {
        super::eval::await_value(rt, captured).await?;
    }
    Ok(())
}

/// A cursor over a program-defined iterator.
pub fn custom_cursor(rt: &Rt, iterator: CustomIterator, node: Option<Loc>, site: Site) -> Rc<dyn Cursor> {
    let it = Rc::new(iterator);
    let (rt1, it1) = (rt.clone(), it.clone());
    let (rt2, it2) = (rt.clone(), it);
    FnCursor::new(
        move || {
            let (rt, it) = (rt1.clone(), it1.clone());
            Box::pin(async move { next_iterator_result(&rt, &it, node, false, site).await })
        },
        move || {
            let (rt, it) = (rt2.clone(), it2.clone());
            Box::pin(async move { close_iterator(&rt, &it, node, false, site).await })
        },
    )
}

/// `ctx.iterate`: a built-in cursor, or a program-defined synchronous iterator.
pub async fn iterate(rt: &Rt, value: &Value, node: Option<Loc>, site: Site) -> JsResult<Option<Rc<dyn Cursor>>> {
    if let Some(c) = builtin_cursor(rt, value) {
        return Ok(Some(c));
    }
    Ok(custom_iterator(rt, value, node, false, site).await?.map(|it| custom_cursor(rt, it, node, site)))
}

/// GetIteratorDirect: drive an iterator by its own `next`, without asking for `[Symbol.iterator]`.
pub fn iterate_direct(rt: &Rt, value: &Value, node: Option<Loc>, site: Site) -> JsResult<Rc<dyn Cursor>> {
    if let Value::Object(o) = value
        && let ObjKind::Iterator(c) = o.kind()
    {
        return Ok(c.clone());
    }
    let Value::Object(o) = value else {
        let e = type_error(format!("An iterator must be an object, received {}.", super::describe_value(value)));
        return Err(match node {
            Some(n) => super::errors::locate(e, Some(n)),
            None => e,
        });
    };
    let next = require_iterator_method(get(o, &PropKey::str("next"))?, "Iterator next", node)?;
    Ok(custom_cursor(rt, CustomIterator { iterator: o.clone(), next, asynchronous: false }, node, site))
}

/// IteratorClose: a consumer failure closes the iterator and wins over any close failure, except that a
/// generator's return() is a return completion, so a failing close wins over it.
pub async fn preserve_consumer_error<T>(cursor: &Rc<dyn Cursor>, result: JsResult<T>) -> JsResult<T> {
    match result {
        Ok(v) => Ok(v),
        Err(super::errors::Thrown::Interrupt) => Err(super::errors::Thrown::Interrupt),
        Err(e) => {
            let closed = cursor.close().await;
            if let (Err(close_error), super::errors::Thrown::GeneratorReturn(_)) = (&closed, &e) {
                return Err(close_error.clone());
            }
            Err(e)
        }
    }
}

/// A `{ value, done }` result object.
pub fn iter_result(rt: &Rt, value: Value, done: bool) -> Value {
    Value::Object(record(&rt.builtins.object, vec![("value", value), ("done", Value::Bool(done))]))
}

/// A built-in iterator object around a cursor.
pub fn iterator_object(rt: &Rt, cursor: Rc<dyn Cursor>) -> Value {
    Value::Object(Obj::new(Some(rt.builtins.iterator.clone()), ObjKind::Iterator(cursor)))
}

pub fn type_error_located(message: String, node: Option<Loc>) -> super::errors::Thrown {
    match node {
        Some(n) => type_error_at(message, n),
        None => type_error(message),
    }
}

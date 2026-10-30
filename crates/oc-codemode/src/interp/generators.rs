//! Generator objects and the `next`/`return`/`throw` prototype methods. Port of `interpreter/generators.ts`.
//!
//! A generator's body is a task of its own (see [`super::runner`]): a request resumes it nested in the caller, as
//! upstream pushes the body's saved activations onto the caller's stack. A synchronous generator answers when the
//! body yields or ends; an asynchronous one queues each request with its promise and runs the spec's state machine
//! (AsyncGeneratorCompleteStep, DrainQueue, AwaitReturn), and its body also parks at an `await` like any async body.

use std::cell::{Cell, RefCell};
use std::collections::VecDeque;
use std::rc::Rc;

use futures::future::LocalBoxFuture;

use super::errors::{type_error, JsResult, Thrown};
use super::eval::Frame;
use super::function::{arg, fn_, method, methods, receiver, NResult};
use super::iteration::iter_result;
use super::promises::capability;
use super::runner::{suspend, Exit, Reaction, Resolving, TaskId};
use super::{Rt, Site};
use crate::object::{define, ObjKind, HIDDEN};
use crate::value::{PropKey, Sym, Value};

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ReqKind {
    Next,
    Return,
    Throw,
}

impl ReqKind {
    fn name(self) -> &'static str {
        match self {
            ReqKind::Next => "next",
            ReqKind::Return => "return",
            ReqKind::Throw => "throw",
        }
    }
}

/// What a generator body continues with: the `next`, `return`, or `throw` call that resumed it.
#[derive(Clone)]
pub struct GenRequest {
    pub kind: ReqKind,
    pub value: Value,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum GenState {
    SuspendedStart,
    SuspendedYield,
    Executing,
    AwaitingReturn,
    Draining,
    Completed,
}

/// What a synchronous generator's last step produced.
pub enum GenOutput {
    Yielded(Value),
    Done(Exit),
}

pub struct GeneratorHandle {
    pub asynchronous: bool,
    pub state: Cell<GenState>,
    /// The body, started by the first `next()`.
    pub body: RefCell<Option<LocalBoxFuture<'static, Exit>>>,
    /// The body's task once started.
    pub task: Cell<Option<TaskId>>,
    pub frame: RefCell<Option<Rc<Frame>>>,
    /// An async generator's requests, oldest first, each with the functions that settle its promise.
    pub queue: RefCell<VecDeque<(GenRequest, Resolving)>>,
    /// The request handed to a suspended body.
    pub input: Rc<RefCell<Option<GenRequest>>>,
    pub output: RefCell<Option<GenOutput>>,
}

impl GeneratorHandle {
    pub fn new(asynchronous: bool, body: LocalBoxFuture<'static, Exit>, frame: Rc<Frame>) -> GeneratorHandle {
        GeneratorHandle {
            asynchronous,
            state: Cell::new(GenState::SuspendedStart),
            body: RefCell::new(Some(body)),
            task: Cell::new(None),
            frame: RefCell::new(Some(frame)),
            queue: RefCell::new(VecDeque::new()),
            input: Rc::new(RefCell::new(None)),
            output: RefCell::new(None),
        }
    }

    pub fn teardown(&self) -> Box<dyn std::any::Any> {
        Box::new((
            self.body.borrow_mut().take(),
            self.frame.borrow_mut().take(),
            std::mem::take(&mut *self.queue.borrow_mut()),
            self.input.borrow_mut().take(),
            self.output.borrow_mut().take(),
        ))
    }
}

// Puts the suspended body back to work with `request`; the body answers the caller when it next yields or ends.
fn resume(rt: &Rt, generator: &Rc<GeneratorHandle>, request: GenRequest) {
    let started = generator.state.get() != GenState::SuspendedStart;
    generator.state.set(GenState::Executing);
    if !started {
        let Some(body) = generator.body.borrow_mut().take() else { return };
        let handle = generator.clone();
        let rt2 = rt.clone();
        let id = rt.runner.create_task(Box::pin(async move {
            let completion = body.await;
            finish(&rt2, &handle, completion);
        }));
        generator.task.set(Some(id));
        rt.runner.poll_task(id);
        return;
    }
    *generator.input.borrow_mut() = Some(request);
    if let Some(id) = generator.task.get() {
        rt.runner.poll_task(id);
    }
}

// The body returned or threw.
fn finish(rt: &Rt, generator: &Rc<GeneratorHandle>, completion: Exit) {
    if !generator.asynchronous {
        generator.state.set(GenState::Completed);
        *generator.output.borrow_mut() = Some(GenOutput::Done(completion));
        return;
    }
    complete_step(rt, generator, completion, true);
    drain_queue(rt, generator);
}

/// `yield value` in a generator body: suspends until the next request, which it returns.
pub async fn suspend_body(rt: &Rt, generator: &Rc<GeneratorHandle>, value: Value) -> GenRequest {
    if !generator.asynchronous {
        *generator.output.borrow_mut() = Some(GenOutput::Yielded(value));
        generator.state.set(GenState::SuspendedYield);
        return suspend(generator.input.clone()).await;
    }
    // AsyncGeneratorYield: answer the oldest request, and keep running if another is already waiting.
    complete_step(rt, generator, Ok(value), false);
    let next = generator.queue.borrow().front().map(|(r, _)| r.clone());
    if let Some(next) = next {
        return next;
    }
    generator.state.set(GenState::SuspendedYield);
    suspend(generator.input.clone()).await
}

// A synchronous generator step: the body runs on the caller's stack until it yields, returns, or throws.
fn step(rt: &Rt, generator: &Rc<GeneratorHandle>, kind: ReqKind, value: Value) -> JsResult<Value> {
    if generator.state.get() == GenState::Executing {
        return Err(type_error("Generator is already running."));
    }
    if generator.state.get() == GenState::SuspendedStart && kind != ReqKind::Next {
        generator.state.set(GenState::Completed);
    }
    if generator.state.get() == GenState::Completed {
        if kind == ReqKind::Throw {
            return Err(Thrown::Value(value));
        }
        return Ok(iter_result(rt, if kind == ReqKind::Return { value } else { Value::Undefined }, true));
    }
    resume(rt, generator, GenRequest { kind, value });
    match generator.output.borrow_mut().take() {
        Some(GenOutput::Yielded(v)) => Ok(iter_result(rt, v, false)),
        Some(GenOutput::Done(Ok(v))) => Ok(iter_result(rt, v, true)),
        Some(GenOutput::Done(Err(e))) => Err(e),
        None => Err(Thrown::Interrupt),
    }
}

// AsyncGenerator.prototype.next/return/throw: queue the request with its promise, and run the body now if it is
// waiting for one. A request that arrives while the body runs is answered when the body reaches it.
fn request(rt: &Rt, this: &Value, kind: ReqKind, value: Value) -> JsResult<Value> {
    let resolving = capability(rt, None)?;
    let promise = Value::Object(resolving.promise.clone());
    let generator = match this {
        Value::Object(o) => match o.kind() {
            ObjKind::Generator(g) if g.asynchronous => Some(g.clone()),
            _ => None,
        },
        _ => None,
    };
    let Some(generator) = generator else {
        resolving.reject(rt, type_error(format!("AsyncGenerator.prototype.{} called on incompatible receiver.", kind.name())));
        return Ok(promise);
    };
    let state = generator.state.get();
    if kind == ReqKind::Next && state == GenState::Completed {
        resolving.resolve(rt, iter_result(rt, Value::Undefined, true));
        return Ok(promise);
    }
    if kind == ReqKind::Throw {
        if state == GenState::SuspendedStart {
            generator.state.set(GenState::Completed);
        }
        if generator.state.get() == GenState::Completed {
            resolving.reject(rt, Thrown::Value(value));
            return Ok(promise);
        }
    }
    let queued = GenRequest { kind, value };
    generator.queue.borrow_mut().push_back((queued.clone(), resolving));
    if kind == ReqKind::Return && matches!(state, GenState::SuspendedStart | GenState::Completed) {
        generator.state.set(GenState::AwaitingReturn);
        await_return(rt, &generator);
        return Ok(promise);
    }
    if matches!(state, GenState::SuspendedStart | GenState::SuspendedYield) {
        resume(rt, &generator, queued);
    }
    Ok(promise)
}

// AsyncGeneratorCompleteStep: answer the oldest request.
fn complete_step(rt: &Rt, generator: &Rc<GeneratorHandle>, completion: Exit, done: bool) {
    let next = generator.queue.borrow_mut().pop_front();
    let Some((_, resolving)) = next else { return };
    match completion {
        Ok(v) => resolving.resolve(rt, iter_result(rt, v, done)),
        Err(e) => resolving.reject(rt, e),
    }
}

// AsyncGeneratorDrainQueue: once the body has ended, answer the remaining requests in order.
fn drain_queue(rt: &Rt, generator: &Rc<GeneratorHandle>) {
    generator.state.set(GenState::Draining);
    loop {
        let next = generator.queue.borrow().front().map(|(r, _)| r.clone());
        let Some(next) = next else {
            generator.state.set(GenState::Completed);
            return;
        };
        if next.kind == ReqKind::Return {
            generator.state.set(GenState::AwaitingReturn);
            return await_return(rt, generator);
        }
        let completion = if next.kind == ReqKind::Throw { Err(Thrown::Value(next.value)) } else { Ok(Value::Undefined) };
        complete_step(rt, generator, completion, true);
    }
}

// AsyncGeneratorAwaitReturn: a `return(value)` on an ended or unstarted generator settles once `value` does.
fn await_return(rt: &Rt, generator: &Rc<GeneratorHandle>) {
    let value = generator.queue.borrow().front().map(|(r, _)| r.value.clone()).unwrap_or_default();
    let handle = generator.clone();
    let finish = move |completion: Exit| {
        let rt = super::rt();
        complete_step(&rt, &handle, completion, true);
        drain_queue(&rt, &handle);
    };
    match rt.runner.promise_resolve(rt, value) {
        Ok(promise) => rt.runner.then(&promise, Reaction::new(finish)),
        Err(error) => finish(Err(error)),
    }
}

fn sync_method(rt: &Rt, kind: ReqKind, this: Value, args: Vec<Value>) -> JsResult<Value> {
    let method = format!("Generator.prototype.{}", kind.name());
    let object = receiver(&this, &method, |o| matches!(o.kind(), ObjKind::Generator(_)))?;
    let ObjKind::Generator(generator) = object.kind() else { unreachable!() };
    if generator.asynchronous {
        return Err(type_error(format!("{method} called on incompatible receiver an async generator.")));
    }
    step(rt, generator, kind, arg(&args, 0))
}

/// `next`/`return`/`throw` on the generator prototypes; async generators answer with promises.
pub fn generator_globals(rt: &Rt) {
    let b = &rt.builtins;
    let sync = |name: &'static str, kind: ReqKind| {
        method(name, 1, move |rt, _: Site, this, args| NResult::Ready(sync_method(rt, kind, this, args)))
    };
    methods(&b.function, &b.generator, vec![sync("next", ReqKind::Next), sync("return", ReqKind::Return), sync("throw", ReqKind::Throw)]);
    let asynchronous = |name: &'static str, kind: ReqKind| {
        method(name, 1, move |rt, _: Site, this, args| NResult::Ready(request(rt, &this, kind, arg(&args, 0))))
    };
    methods(
        &b.function,
        &b.async_generator,
        vec![asynchronous("next", ReqKind::Next), asynchronous("return", ReqKind::Return), asynchronous("throw", ReqKind::Throw)],
    );
    let f = fn_(&b.function, "[Symbol.iterator]", 0, |_, _, this, _| NResult::ok(this));
    define(&b.iterator, PropKey::Sym(Sym::Iterator), Value::Object(f), HIDDEN);
    let f = fn_(&b.function, "[Symbol.asyncIterator]", 0, |_, _, this, _| NResult::ok(this));
    define(&b.async_iterator, PropKey::Sym(Sym::AsyncIterator), Value::Object(f), HIDDEN);
}

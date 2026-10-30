//! One execution's job queue. Port of `interpreter/runner.ts`.
//!
//! Program code runs only inside a job, one job at a time and each to completion, as in a JavaScript engine:
//! promise reactions and resumed awaits queue in order, host work runs concurrently and settles its promise as a
//! later job, and the program is stuck exactly when no job is queued and no host work is running.
//!
//! Upstream keeps program activations on an explicit stack and splits it at the innermost async boundary when a body
//! awaits. Here a *task* is a Rust future: an async body (the program, an async function, an async generator body, a
//! tool call's argument crossing) owns one, and every synchronous activation it calls runs nested inside that future.
//! An `await` parks the innermost running task (its future returns `Pending` up to the task boundary) and attaches a
//! reaction that polls it again as a job. Calling an async function polls its new task at once, nested in the caller,
//! so its body runs synchronously until it first awaits, exactly like upstream's `Call` with an `AsyncBoundary`.
//!
//! Host work (tool calls, async extension globals, `fetch`) runs as host futures polled on the interpreter thread and
//! woken through a thread-safe queue; a finished one pushes its settlement into an inbox that is drained only when
//! the job queue is empty. A blocking host call (`search`, synchronous extension globals) holds the current job: the
//! interpreter thread waits for it, polling other host work meanwhile.

use std::cell::{Cell, RefCell};
use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::future::Future;
use std::pin::Pin;
use std::rc::Rc;
use std::sync::atomic::Ordering;
use std::sync::{Arc, Condvar, Mutex};
use std::task::{Context, Poll, Wake, Waker};
use std::time::{Duration, Instant};

use futures::future::LocalBoxFuture;
use indexmap::IndexMap;

use super::errors::{locate, range_error, type_error, Diagnostic, DiagnosticKind, ErrorType, JsResult, PendingThrow, Thrown};
use super::globals::normalize_error;
use super::limits::MAX_PENDING_PROMISES;
use super::Rt;
use crate::ast::Loc;
use crate::object::{get, Obj, ObjKind};
use crate::value::{PropKey, Value};

pub type Exit = Result<Value, Thrown>;
pub type TaskId = usize;
type Job = Box<dyn FnOnce()>;

/// [[PromiseState]].
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum PState {
    Pending,
    Fulfilled,
    Rejected,
}

/// A promise's internal slots. The result is kept as thrown, so a handler receives it materialized.
pub struct PromiseState {
    /// Creation order, for reporting unhandled rejections in the order their promises were made.
    pub id: u64,
    state: Cell<PState>,
    result: RefCell<Option<Exit>>,
    reactions: RefCell<Vec<Rc<Reaction>>>,
    /// [[PromiseIsHandled]]: a reaction was attached, so a rejection is not reported as unhandled.
    handled: Cell<bool>,
}

impl PromiseState {
    pub fn state(&self) -> PState {
        self.state.get()
    }
    pub fn teardown(&self) -> Box<dyn std::any::Any> {
        Box::new((std::mem::take(&mut *self.reactions.borrow_mut()), self.result.borrow_mut().take()))
    }
}

type ReactionFn = Box<dyn FnOnce(Exit)>;

/// A settled promise's reaction, run as a job.
pub struct Reaction(RefCell<Option<ReactionFn>>);

impl Reaction {
    pub fn new(f: impl FnOnce(Exit) + 'static) -> Rc<Reaction> {
        Rc::new(Reaction(RefCell::new(Some(Box::new(f)))))
    }
    fn run(&self, exit: Exit) {
        let f = self.0.borrow_mut().take();
        if let Some(f) = f {
            f(exit)
        }
    }
}

pub fn promise_state(promise: &Obj) -> Option<&Rc<PromiseState>> {
    match promise.kind() {
        ObjKind::Promise(p) => Some(p),
        _ => None,
    }
}

/// The functions that settle one promise, at most once between them (CreateResolvingFunctions, for host callers).
/// A failure without a location is located at `site`, the call that created the promise.
#[derive(Clone)]
pub struct Resolving {
    pub promise: Obj,
    done: Rc<Cell<bool>>,
    site: Option<Loc>,
}

impl Resolving {
    pub fn resolve(&self, rt: &Rt, resolution: Value) {
        if self.done.replace(true) {
            return;
        }
        rt.runner.resolve_with(rt, &self.promise, resolution, self.site);
    }
    pub fn reject(&self, rt: &Rt, reason: Thrown) {
        if self.done.replace(true) {
            return;
        }
        rt.runner.settle(rt, &self.promise, Err(locate(reason, self.site)));
    }
    /// Settles with a completion: a fulfillment value is resolved, so a thenable is adopted.
    pub fn settle_with(&self, rt: &Rt, completion: Exit) {
        match completion {
            Ok(v) => self.resolve(rt, v),
            Err(e) => self.reject(rt, e),
        }
    }
}

/// Wakes the interpreter thread from any thread (host futures).
#[derive(Default)]
pub struct External {
    queue: Mutex<Vec<usize>>,
    cond: Condvar,
}

/// The id a blocking host call's waker reports.
const BLOCKING: usize = usize::MAX;

struct HostWaker {
    id: usize,
    external: Arc<External>,
}

impl Wake for HostWaker {
    fn wake(self: Arc<Self>) {
        self.wake_by_ref()
    }
    fn wake_by_ref(self: &Arc<Self>) {
        let mut queue = self.external.queue.lock().unwrap();
        queue.push(self.id);
        self.external.cond.notify_all();
    }
}

struct Parked {
    promise: Obj,
    node: Option<Loc>,
}

/// How a run ended.
pub enum Outcome {
    /// The program's body completed (its copied-out value, or how it failed).
    Done(Exit),
    /// The deadline passed or the caller cancelled.
    Stopped,
}

pub struct Runner {
    tasks: RefCell<Vec<Option<LocalBoxFuture<'static, ()>>>>,
    running: RefCell<Vec<TaskId>>,
    /// The promise an async task's body settles (its AsyncBoundary result).
    task_results: RefCell<HashMap<TaskId, Obj>>,
    queue: RefCell<VecDeque<Job>>,
    inbox: RefCell<VecDeque<Job>>,
    hosts: RefCell<Vec<Option<LocalBoxFuture<'static, Job>>>>,
    host_live: Cell<usize>,
    pub external: Arc<External>,
    pending: Cell<usize>,
    next_id: Cell<u64>,
    /// Rejections nothing handled yet, described when they happened so reporting them at the end is cheap.
    unhandled: RefCell<BTreeMap<u64, Diagnostic>>,
    /// Bodies suspended at an `await` and the promise each waits on, so a stall can point at the one that is stuck.
    parked: RefCell<IndexMap<TaskId, Parked>>,
    /// The intrinsic `Promise.prototype.then`, so adopting a native promise skips exposing its rejection first.
    pub intrinsic_then: RefCell<Option<Obj>>,
    stopping: Cell<bool>,
}

impl Default for Runner {
    fn default() -> Self {
        Runner {
            tasks: RefCell::new(Vec::new()),
            running: RefCell::new(Vec::new()),
            task_results: RefCell::new(HashMap::new()),
            queue: RefCell::new(VecDeque::new()),
            inbox: RefCell::new(VecDeque::new()),
            hosts: RefCell::new(Vec::new()),
            host_live: Cell::new(0),
            external: Arc::new(External::default()),
            pending: Cell::new(0),
            next_id: Cell::new(0),
            unhandled: RefCell::new(BTreeMap::new()),
            parked: RefCell::new(IndexMap::new()),
            intrinsic_then: RefCell::new(None),
            stopping: Cell::new(false),
        }
    }
}

/// Suspends the running task until `slot` is filled by the job that resumes it.
pub struct Suspend<T> {
    slot: Rc<RefCell<Option<T>>>,
}

impl<T> Future for Suspend<T> {
    type Output = T;
    fn poll(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<T> {
        match self.slot.borrow_mut().take() {
            Some(v) => Poll::Ready(v),
            None => Poll::Pending,
        }
    }
}

pub fn suspend<T>(slot: Rc<RefCell<Option<T>>>) -> Suspend<T> {
    Suspend { slot }
}

impl Runner {
    // --- Tasks ---

    /// The task being polled, innermost first.
    pub fn current(&self) -> Option<TaskId> {
        self.running.borrow().last().copied()
    }

    /// Registers a task without running it.
    pub fn create_task(&self, future: LocalBoxFuture<'static, ()>) -> TaskId {
        let mut tasks = self.tasks.borrow_mut();
        tasks.push(Some(future));
        tasks.len() - 1
    }

    /// Starts a task now: it runs, nested in whatever is running, until it first suspends or ends.
    pub fn spawn(&self, future: LocalBoxFuture<'static, ()>) -> TaskId {
        let id = self.create_task(future);
        self.poll_task(id);
        id
    }

    /// Starts an async body whose completion settles `result` (an AsyncBoundary).
    pub fn spawn_async(&self, future: LocalBoxFuture<'static, ()>, result: &Obj) -> TaskId {
        let id = self.create_task(future);
        self.task_results.borrow_mut().insert(id, result.clone());
        self.poll_task(id);
        id
    }

    /// Continues a task until it suspends again or ends; returns whether it ended.
    pub fn poll_task(&self, id: TaskId) -> bool {
        let future = self.tasks.borrow_mut().get_mut(id).and_then(|slot| slot.take());
        let Some(mut future) = future else { return true };
        self.running.borrow_mut().push(id);
        let waker = futures::task::noop_waker();
        let poll = future.as_mut().poll(&mut Context::from_waker(&waker));
        self.running.borrow_mut().pop();
        match poll {
            Poll::Ready(()) => {
                self.task_results.borrow_mut().remove(&id);
                drop(future);
                true
            }
            Poll::Pending => {
                if let Some(slot) = self.tasks.borrow_mut().get_mut(id) {
                    *slot = Some(future);
                }
                false
            }
        }
    }

    pub fn enqueue(&self, job: impl FnOnce() + 'static) {
        self.queue.borrow_mut().push_back(Box::new(job));
    }

    // --- Host work ---

    /// Starts host work now (so admission and hooks happen at the call site); its completion settles through the
    /// returned job in a later turn, in completion order.
    pub fn host(&self, work: LocalBoxFuture<'static, Job>) {
        let id = {
            let mut hosts = self.hosts.borrow_mut();
            hosts.push(Some(work));
            hosts.len() - 1
        };
        self.host_live.set(self.host_live.get() + 1);
        self.poll_host(id);
    }

    fn poll_host(&self, id: usize) {
        let future = self.hosts.borrow_mut().get_mut(id).and_then(|slot| slot.take());
        let Some(mut future) = future else { return };
        let waker = Waker::from(Arc::new(HostWaker { id, external: self.external.clone() }));
        match future.as_mut().poll(&mut Context::from_waker(&waker)) {
            Poll::Ready(settle) => {
                self.host_live.set(self.host_live.get() - 1);
                drop(future);
                // Interrupted by the program ending: nothing is left to settle.
                if !self.stopping.get() {
                    self.inbox.borrow_mut().push_back(settle);
                }
            }
            Poll::Pending => {
                if let Some(slot) = self.hosts.borrow_mut().get_mut(id) {
                    *slot = Some(future);
                }
            }
        }
    }

    /// Waits until a host future is woken (or `limit` passes), then polls the woken ones. Returns whether the
    /// blocking call's future was among them.
    fn wait_host(&self, limit: Option<Instant>) -> bool {
        let woken: Vec<usize> = {
            let mut queue = self.external.queue.lock().unwrap();
            if queue.is_empty() {
                let wait = match limit {
                    Some(d) => d.saturating_duration_since(Instant::now()).min(Duration::from_millis(50)),
                    None => Duration::from_millis(50),
                };
                queue = self.external.cond.wait_timeout(queue, wait).unwrap().0;
            }
            std::mem::take(&mut *queue)
        };
        let mut blocking = false;
        let mut seen = HashSet::new();
        for id in woken {
            if id == BLOCKING {
                blocking = true;
            } else if seen.insert(id) {
                self.poll_host(id);
            }
        }
        blocking
    }

    /// A blocking host call: no other job runs until it ends, as with a synchronous host call. The deadline and
    /// cancellation still interrupt it.
    pub fn block_on<T>(&self, rt: &Rt, work: impl Future<Output = T>) -> JsResult<T> {
        let mut work = std::pin::pin!(work);
        let waker = Waker::from(Arc::new(HostWaker { id: BLOCKING, external: self.external.clone() }));
        loop {
            if let Poll::Ready(v) = work.as_mut().poll(&mut Context::from_waker(&waker)) {
                return Ok(v);
            }
            loop {
                if rt.interrupt.fired.get() || stop_requested(rt) {
                    rt.interrupt.fired.set(true);
                    return Err(Thrown::Interrupt);
                }
                if self.wait_host(rt.interrupt.deadline) {
                    break;
                }
            }
        }
    }

    /// Wakes the main loop without resuming anything (cancellation).
    pub fn notify(&self) {
        let _guard = self.external.queue.lock().unwrap();
        self.external.cond.notify_all();
    }

    // --- Promise abstract operations ---

    /// A new pending promise; every pending promise counts toward the limit until it settles.
    pub fn new_promise(&self, rt: &Rt) -> JsResult<Obj> {
        if self.pending.get() >= MAX_PENDING_PROMISES {
            return Err(range_error(format!(
                "Too many pending promises (limit {MAX_PENDING_PROMISES}); await promises before creating more."
            )));
        }
        self.pending.set(self.pending.get() + 1);
        let id = self.next_id.get();
        self.next_id.set(id + 1);
        let state = Rc::new(PromiseState {
            id,
            state: Cell::new(PState::Pending),
            result: RefCell::new(None),
            reactions: RefCell::new(Vec::new()),
            handled: Cell::new(false),
        });
        Ok(Obj::new(Some(rt.builtins.promise.clone()), ObjKind::Promise(state)))
    }

    pub fn resolving(&self, promise: &Obj, site: Option<Loc>) -> Resolving {
        Resolving { promise: promise.clone(), done: Rc::new(Cell::new(false)), site }
    }

    /// PerformPromiseThen with one reaction for either outcome; marks the promise handled.
    pub fn then(&self, promise: &Obj, reaction: Rc<Reaction>) {
        let Some(p) = promise_state(promise) else { return };
        if p.state.get() == PState::Pending {
            p.reactions.borrow_mut().push(reaction);
        } else {
            if p.state.get() == PState::Rejected {
                self.unhandled.borrow_mut().remove(&p.id);
            }
            let settled = p.result.borrow().clone().unwrap_or(Ok(Value::Undefined));
            self.enqueue(move || reaction.run(settled));
        }
        p.handled.set(true);
    }

    /// Removes a reaction an internal consumer no longer needs, so a long-pending promise does not keep it.
    pub fn detach(&self, promise: &Obj, reaction: &Rc<Reaction>) {
        if let Some(p) = promise_state(promise) {
            let mut reactions = p.reactions.borrow_mut();
            if let Some(index) = reactions.iter().position(|r| Rc::ptr_eq(r, reaction)) {
                reactions.remove(index);
            }
        }
    }

    /// PromiseResolve(%Promise%, value).
    pub fn promise_resolve(&self, rt: &Rt, value: Value) -> JsResult<Obj> {
        if let Value::Object(o) = &value
            && o.is_promise()
        {
            return Ok(o.clone());
        }
        let promise = self.new_promise(rt)?;
        self.resolve_with(rt, &promise, value, None);
        Ok(promise)
    }

    pub fn settle(&self, rt: &Rt, promise: &Obj, completion: Exit) {
        let Some(p) = promise_state(promise) else { return };
        if p.state.get() != PState::Pending {
            return;
        }
        self.pending.set(self.pending.get().saturating_sub(1));
        p.state.set(if completion.is_ok() { PState::Fulfilled } else { PState::Rejected });
        *p.result.borrow_mut() = Some(completion.clone());
        let reactions = std::mem::take(&mut *p.reactions.borrow_mut());
        if let Err(error) = &completion
            && !p.handled.get()
            && !matches!(error, Thrown::Interrupt)
        {
            let failure = normalize_error(error);
            self.unhandled.borrow_mut().insert(
                p.id,
                Diagnostic { message: format!("Unhandled rejection from an un-awaited promise: {}", failure.message), ..failure },
            );
        }
        let _ = rt;
        for reaction in reactions {
            let settled = completion.clone();
            self.enqueue(move || reaction.run(settled));
        }
    }

    pub fn resolve_with(&self, rt: &Rt, promise: &Obj, resolution: Value, site: Option<Loc>) {
        if let Value::Object(o) = &resolution
            && Obj::ptr_eq(o, promise)
        {
            let cycle = locate(type_error("Chaining cycle detected: a promise cannot resolve with itself."), site);
            return self.settle(rt, promise, Err(cycle));
        }
        let Value::Object(thenable) = &resolution else { return self.settle(rt, promise, Ok(resolution)) };
        let then = match get(thenable, &PropKey::str("then")) {
            Ok(t) => t,
            Err(e) => return self.settle(rt, promise, Err(e)),
        };
        if then.js_typeof() != "function" {
            return self.settle(rt, promise, Ok(resolution));
        }
        let (promise, thenable) = (promise.clone(), thenable.clone());
        self.enqueue(move || thenable_job(promise, thenable, then));
    }

    /// Rejections no handler observed, in the order their promises were created. Host work that finished before the
    /// program did settles first, so its failures count even though no later job ran.
    pub fn diagnostics(&self) -> Vec<Diagnostic> {
        loop {
            let settle = self.inbox.borrow_mut().pop_front();
            match settle {
                Some(settle) => settle(),
                None => break,
            }
        }
        self.unhandled.borrow().values().cloned().collect()
    }

    /// Stops host work still running: later completions settle nothing.
    pub fn interrupt(&self) {
        self.stopping.set(true);
        let hosts = std::mem::take(&mut *self.hosts.borrow_mut());
        self.host_live.set(0);
        drop(hosts);
    }

    /// Drops every task and job so the run's objects can be released.
    pub fn teardown(&self) {
        self.interrupt();
        let tasks = std::mem::take(&mut *self.tasks.borrow_mut());
        drop(tasks);
        let queue = std::mem::take(&mut *self.queue.borrow_mut());
        drop(queue);
        let inbox = std::mem::take(&mut *self.inbox.borrow_mut());
        drop(inbox);
        self.parked.borrow_mut().clear();
        self.task_results.borrow_mut().clear();
        self.intrinsic_then.borrow_mut().take();
    }

    // --- Await ---

    fn park(&self, task: TaskId, promise: &Obj, node: Option<Loc>) {
        let mut parked = self.parked.borrow_mut();
        parked.shift_remove(&task);
        parked.insert(task, Parked { promise: promise.clone(), node });
    }

    // The innermost stuck `await`: an outer one waits on the result of a parked async body, so prefer one that does
    // not. Bodies that wait on each other in a cycle leave no such await; any located one is then as good as another.
    fn stuck_await(&self) -> Option<Loc> {
        let parked = self.parked.borrow();
        let results = self.task_results.borrow();
        let waited: Vec<&Obj> = parked.keys().filter_map(|task| results.get(task)).collect();
        let located: Vec<&Parked> = parked.values().filter(|p| p.node.is_some()).collect();
        located
            .iter()
            .find(|p| !waited.iter().any(|r| Obj::ptr_eq(r, &p.promise)))
            .or(located.first())
            .and_then(|p| p.node)
    }

    // --- Driver ---

    /// Runs the program's body as the first job until it completes, then returns how it ended. Host work it started
    /// may still be running; `interrupt` stops it.
    pub fn run(&self, rt: &Rt, main: LocalBoxFuture<'static, Exit>) -> Outcome {
        let outcome: Rc<RefCell<Option<Exit>>> = Rc::new(RefCell::new(None));
        let slot = outcome.clone();
        self.spawn(Box::pin(async move {
            let exit = main.await;
            *slot.borrow_mut() = Some(exit);
        }));
        let mut steps: u32 = 0;
        loop {
            if let Some(exit) = outcome.borrow_mut().take() {
                return Outcome::Done(exit);
            }
            if rt.interrupt.fired.get() {
                return Outcome::Stopped;
            }
            steps = steps.wrapping_add(1);
            if steps.is_multiple_of(64) && stop_requested(rt) {
                return Outcome::Stopped;
            }
            let job = self.queue.borrow_mut().pop_front();
            if let Some(job) = job {
                job();
                continue;
            }
            let settle = self.inbox.borrow_mut().pop_front();
            if let Some(settle) = settle {
                settle();
                continue;
            }
            if self.host_live.get() > 0 {
                if stop_requested(rt) {
                    return Outcome::Stopped;
                }
                self.wait_host(rt.interrupt.deadline);
                continue;
            }
            let node = self.stuck_await();
            let stalled = Thrown::Pending(Rc::new(PendingThrow {
                error_type: ErrorType::Error,
                message: "The program is waiting on a promise that can never settle: nothing else is running, so nothing can resolve it. Code Mode has no timers or sleep, so don't wait or poll inside a program.".into(),
                node: RefCell::new(node),
                kind: DiagnosticKind::ExecutionFailure,
                suggestions: None,
                value: RefCell::new(None),
                unlocated: false,
            }));
            return Outcome::Done(Err(stalled));
        }
    }
}

fn stop_requested(rt: &Rt) -> bool {
    rt.interrupt.cancelled.load(Ordering::Relaxed) || rt.interrupt.deadline.is_some_and(|d| Instant::now() >= d)
}

// NewPromiseResolveThenableJob. Adopting a native promise through the intrinsic `then` takes the same jobs as calling
// it, and keeps the rejection as thrown so its diagnostic survives.
fn thenable_job(promise: Obj, thenable: Obj, then: Value) {
    let rt = super::rt();
    let resolving = rt.runner.resolving(&promise, None);
    let intrinsic = matches!((&then, &*rt.runner.intrinsic_then.borrow()), (Value::Object(t), Some(i)) if Obj::ptr_eq(t, i));
    if intrinsic && thenable.is_promise() {
        let r = resolving.clone();
        rt.runner.then(
            &thenable,
            Reaction::new(move |settled| {
                let rt = super::rt();
                r.settle_with(&rt, settled);
            }),
        );
        return;
    }
    let function = &rt.builtins.function;
    let (r1, r2) = (resolving.clone(), resolving.clone());
    let resolve = super::function::fn_(function, "", 1, move |rt, _, _, args| {
        r1.resolve(rt, super::function::arg(&args, 0));
        super::function::NResult::ok(Value::Undefined)
    });
    let reject = super::function::fn_(function, "", 1, move |rt, _, _, args| {
        r2.reject(rt, Thrown::Value(super::function::arg(&args, 0)));
        super::function::NResult::ok(Value::Undefined)
    });
    let rt2 = rt.clone();
    rt.runner.spawn(Box::pin(async move {
        let called = super::eval::call_value(
            &rt2,
            &then,
            Value::Object(thenable),
            vec![Value::Object(resolve), Value::Object(reject)],
            super::Site::default(),
        )
        .await;
        if let Err(error) = called {
            resolving.reject(&rt2, error);
        }
    }));
}

/// Await: suspends the innermost async body until `value` settles; a stall while it waits is reported at `node`.
pub async fn await_value(rt: &Rt, value: Value, node: Option<Loc>) -> JsResult<Value> {
    let runner = &rt.runner;
    let promise = runner.promise_resolve(rt, value)?;
    let Some(task) = runner.current() else {
        return Err(type_error("await is only valid in async functions."));
    };
    let slot: Rc<RefCell<Option<Exit>>> = Rc::new(RefCell::new(None));
    runner.park(task, &promise, node);
    let filled = slot.clone();
    runner.then(
        &promise,
        Reaction::new(move |settled| {
            let rt = super::rt();
            rt.runner.parked.borrow_mut().shift_remove(&task);
            *filled.borrow_mut() = Some(settled);
            rt.runner.poll_task(task);
        }),
    );
    suspend(slot).await
}

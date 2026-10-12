//! A single-threaded fiber runtime that reproduces the scheduling the upstream interpreter gets from Effect:
//!
//! - forked fibers start immediately and run synchronously until they first suspend;
//! - completing a fiber or a deferred resumes its waiters synchronously, in registration order (Effect's
//!   observers and `callback` resumption are synchronous);
//! - `yield_now` reschedules the current fiber at the back of a FIFO queue;
//! - host futures (tool calls, extension promises) wake their fiber through a thread-safe queue that the main loop
//!   drains when nothing else is runnable.
//!
//! Each execution runs on its own thread, so the executor is thread-local.

use std::cell::{Cell, RefCell};
use std::collections::VecDeque;
use std::future::Future;
use std::pin::Pin;
use std::rc::Rc;
use std::sync::{Arc, Condvar, Mutex};
use std::task::{Context, Poll, Wake, Waker};
use std::time::Instant;

use futures::future::LocalBoxFuture;

use super::errors::Thrown;
use crate::value::Value;

pub type FiberId = usize;
pub type Exit = Result<Value, Thrown>;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum State {
    Suspended,
    Running,
    Done,
}

struct SlotData {
    future: Option<LocalBoxFuture<'static, ()>>,
    state: State,
    waker: Waker,
}

/// Wakes a fiber from any thread (host futures).
struct FiberWaker {
    id: FiberId,
    external: Arc<External>,
}

impl Wake for FiberWaker {
    fn wake(self: Arc<Self>) {
        self.wake_by_ref()
    }
    fn wake_by_ref(self: &Arc<Self>) {
        let mut queue = self.external.queue.lock().unwrap();
        queue.push(self.id);
        self.external.cond.notify_all();
    }
}

#[derive(Default)]
pub struct External {
    queue: Mutex<Vec<FiberId>>,
    cond: Condvar,
}

impl External {
    /// Wakes the main loop without resuming a fiber (used for cancellation).
    pub fn notify(&self) {
        let _guard = self.queue.lock().unwrap();
        self.cond.notify_all();
    }
}

pub struct Executor {
    slots: RefCell<Vec<Option<SlotData>>>,
    queue: RefCell<VecDeque<FiberId>>,
    running: RefCell<Vec<FiberId>>,
    pub external: Arc<External>,
}

thread_local! {
    static EXEC: RefCell<Option<Rc<Executor>>> = const { RefCell::new(None) };
}

/// The executor of the current interpreter thread.
pub fn executor() -> Rc<Executor> {
    EXEC.with(|e| {
        let mut slot = e.borrow_mut();
        if slot.is_none() {
            *slot = Some(Rc::new(Executor {
                slots: RefCell::new(Vec::new()),
                queue: RefCell::new(VecDeque::new()),
                running: RefCell::new(Vec::new()),
                external: Arc::new(External::default()),
            }));
        }
        slot.as_ref().unwrap().clone()
    })
}

/// Drops the thread's executor and every fiber it still holds.
pub fn reset_executor() {
    let exec = EXEC.with(|e| e.borrow_mut().take());
    if let Some(exec) = exec {
        exec.drop_all();
    }
}

impl Executor {
    /// Starts `future` as a new fiber and runs it synchronously until it first suspends.
    pub fn spawn(&self, future: impl Future<Output = Exit> + 'static) -> Rc<FiberCell> {
        let id = self.slots.borrow().len();
        let cell = Rc::new(FiberCell {
            id,
            exit: RefCell::new(None),
            observers: RefCell::new(Vec::new()),
            promise_id: Cell::new(None),
            observed: Cell::new(false),
        });
        let completion = cell.clone();
        let wrapped: LocalBoxFuture<'static, ()> = Box::pin(async move {
            let exit = future.await;
            completion.complete(exit);
        });
        let waker = Waker::from(Arc::new(FiberWaker { id, external: self.external.clone() }));
        self.slots.borrow_mut().push(Some(SlotData { future: Some(wrapped), state: State::Suspended, waker }));
        self.resume(id);
        cell
    }

    /// The fiber currently being polled.
    pub fn current(&self) -> Option<FiberId> {
        self.running.borrow().last().copied()
    }

    /// Polls a suspended fiber now (nested inside whatever is running). Running or finished fibers are ignored.
    pub fn resume(&self, id: FiberId) {
        let (mut future, waker) = {
            let mut slots = self.slots.borrow_mut();
            let Some(Some(slot)) = slots.get_mut(id) else { return };
            if slot.state != State::Suspended {
                return;
            }
            let Some(future) = slot.future.take() else { return };
            slot.state = State::Running;
            (future, slot.waker.clone())
        };
        self.running.borrow_mut().push(id);
        let mut cx = Context::from_waker(&waker);
        let poll = future.as_mut().poll(&mut cx);
        self.running.borrow_mut().pop();
        let mut slots = self.slots.borrow_mut();
        let finished = matches!(poll, Poll::Ready(()));
        let interrupted = matches!(slots.get(id), Some(Some(slot)) if slot.state == State::Done);
        let leftover = if finished || interrupted || !matches!(slots.get(id), Some(Some(_))) {
            if let Some(entry) = slots.get_mut(id) {
                *entry = None;
            }
            Some(future)
        } else {
            let slot = slots.get_mut(id).unwrap().as_mut().unwrap();
            slot.state = State::Suspended;
            slot.future = Some(future);
            None
        };
        drop(slots);
        drop(leftover);
    }

    /// Schedules a fiber to resume from the main loop (FIFO).
    pub fn schedule(&self, id: FiberId) {
        self.queue.borrow_mut().push_back(id);
    }

    /// Drops a fiber's future without running it further. Returns whether it was still alive.
    pub fn interrupt(&self, id: FiberId) -> bool {
        let future = {
            let mut slots = self.slots.borrow_mut();
            match slots.get_mut(id) {
                Some(Some(slot)) => match slot.state {
                    State::Done => return false,
                    State::Running => {
                        slot.state = State::Done;
                        return true;
                    }
                    State::Suspended => {
                        let f = slot.future.take();
                        *slots.get_mut(id).unwrap() = None;
                        f
                    }
                },
                _ => return false,
            }
        };
        drop(future);
        true
    }

    pub fn is_alive(&self, id: FiberId) -> bool {
        matches!(self.slots.borrow().get(id), Some(Some(_)))
    }

    /// Ids of every fiber that has not finished, in creation order.
    pub fn alive(&self) -> Vec<FiberId> {
        self.slots.borrow().iter().enumerate().filter_map(|(i, s)| s.as_ref().map(|_| i)).collect()
    }

    fn drop_all(&self) {
        let futures: Vec<_> = {
            let mut slots = self.slots.borrow_mut();
            slots.iter_mut().filter_map(|s| s.take().and_then(|s| s.future)).collect()
        };
        drop(futures);
        self.queue.borrow_mut().clear();
    }

    /// Runs queued and externally woken fibers until `done()` holds, the deadline passes, or `cancelled()`.
    /// Returns false when it stopped because of the deadline or cancellation.
    pub fn run_until(
        &self,
        done: &dyn Fn() -> bool,
        deadline: Option<Instant>,
        cancelled: &dyn Fn() -> bool,
    ) -> bool {
        loop {
            if done() {
                return true;
            }
            if cancelled() {
                return false;
            }
            if let Some(d) = deadline
                && Instant::now() >= d
            {
                return false;
            }
            let next = self.queue.borrow_mut().pop_front();
            if let Some(id) = next {
                self.resume(id);
                continue;
            }
            let woken: Vec<FiberId> = std::mem::take(&mut *self.external.queue.lock().unwrap());
            if !woken.is_empty() {
                for id in woken {
                    self.resume(id);
                    if done() {
                        return true;
                    }
                }
                continue;
            }
            // Nothing runnable: wait for a host future.
            let guard = self.external.queue.lock().unwrap();
            if !guard.is_empty() {
                continue;
            }
            let wait = match deadline {
                Some(d) => d.saturating_duration_since(Instant::now()).min(std::time::Duration::from_millis(50)),
                None => std::time::Duration::from_millis(50),
            };
            let _ = self.external.cond.wait_timeout(guard, wait).unwrap();
        }
    }
}

/// The settlement of a fiber: what `Fiber.await` and `addObserver` see.
pub struct FiberCell {
    pub id: FiberId,
    exit: RefCell<Option<Exit>>,
    observers: RefCell<Vec<Box<dyn FnOnce(&Exit)>>>,
    /// Creation order of the promise around this fiber (for warning order).
    pub promise_id: Cell<Option<u64>>,
    /// Whether responsibility for the promise's rejection transferred to an observer.
    pub observed: Cell<bool>,
}

impl FiberCell {
    pub fn exit(&self) -> Option<Exit> {
        self.exit.borrow().clone()
    }
    pub fn is_done(&self) -> bool {
        self.exit.borrow().is_some()
    }
    fn complete(&self, exit: Exit) {
        if self.exit.borrow().is_some() {
            return;
        }
        *self.exit.borrow_mut() = Some(exit.clone());
        let observers = std::mem::take(&mut *self.observers.borrow_mut());
        for observer in observers {
            observer(&exit);
        }
    }
    /// Marks the fiber interrupted (its future was dropped) and notifies observers.
    pub fn complete_interrupted(&self) {
        self.complete(Err(Thrown::Interrupt));
    }
    /// Calls `observer` when the fiber exits (immediately if it already has).
    pub fn add_observer(&self, observer: Box<dyn FnOnce(&Exit)>) {
        let exit = self.exit.borrow().clone();
        match exit {
            Some(exit) => observer(&exit),
            None => self.observers.borrow_mut().push(observer),
        }
    }
    /// `Fiber.await`: resolves with the exit; the waiting fiber resumes synchronously when this one exits.
    pub fn wait(self: &Rc<Self>) -> FiberWait {
        FiberWait { cell: self.clone(), registered: Cell::new(false) }
    }
    pub fn teardown(&self) -> Box<dyn std::any::Any> {
        let observers = std::mem::take(&mut *self.observers.borrow_mut());
        let exit = self.exit.borrow_mut().take();
        Box::new((observers.len(), exit))
    }
}

pub struct FiberWait {
    cell: Rc<FiberCell>,
    registered: Cell<bool>,
}

impl Future for FiberWait {
    type Output = Exit;
    fn poll(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Exit> {
        if let Some(exit) = self.cell.exit() {
            return Poll::Ready(exit);
        }
        if !self.registered.get() {
            self.registered.set(true);
            let exec = executor();
            if let Some(id) = exec.current() {
                self.cell.observers.borrow_mut().push(Box::new(move |_| executor().resume(id)));
            }
        }
        Poll::Pending
    }
}

/// `Effect.yieldNow`: reschedule the current fiber behind everything already queued.
pub fn yield_now() -> YieldNow {
    YieldNow { yielded: false }
}

pub struct YieldNow {
    yielded: bool,
}

impl Future for YieldNow {
    type Output = ();
    fn poll(mut self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<()> {
        if self.yielded {
            return Poll::Ready(());
        }
        self.yielded = true;
        let exec = executor();
        if let Some(id) = exec.current() {
            exec.schedule(id);
            Poll::Pending
        } else {
            Poll::Ready(())
        }
    }
}

/// A one-shot completion with synchronous waiter resumption (Effect's `Deferred`).
pub struct Deferred {
    value: RefCell<Option<Exit>>,
    waiters: RefCell<Vec<FiberId>>,
}

impl Deferred {
    pub fn new() -> Rc<Deferred> {
        Rc::new(Deferred { value: RefCell::new(None), waiters: RefCell::new(Vec::new()) })
    }
    pub fn is_done(&self) -> bool {
        self.value.borrow().is_some()
    }
    /// `Deferred.doneUnsafe`: first completion wins; waiters resume synchronously in registration order.
    pub fn complete(&self, exit: Exit) -> bool {
        if self.value.borrow().is_some() {
            return false;
        }
        *self.value.borrow_mut() = Some(exit);
        let waiters = std::mem::take(&mut *self.waiters.borrow_mut());
        let exec = executor();
        for id in waiters {
            exec.resume(id);
        }
        true
    }
    pub fn wait(self: &Rc<Self>) -> DeferredWait {
        DeferredWait { deferred: self.clone(), registered: false }
    }
    pub fn teardown(&self) {
        self.value.borrow_mut().take();
        self.waiters.borrow_mut().clear();
    }
}

pub struct DeferredWait {
    deferred: Rc<Deferred>,
    registered: bool,
}

impl Future for DeferredWait {
    type Output = Exit;
    fn poll(mut self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Exit> {
        if let Some(exit) = self.deferred.value.borrow().clone() {
            return Poll::Ready(exit);
        }
        if !self.registered {
            self.registered = true;
            if let Some(id) = executor().current() {
                self.deferred.waiters.borrow_mut().push(id);
            }
        }
        Poll::Pending
    }
}

/// Waits for several fibers at once; resolves when `check` returns `Some`. Used by the combinators.
pub struct WaitAny<T> {
    cells: Vec<Rc<FiberCell>>,
    check: Box<dyn FnMut() -> Option<T>>,
    registered: bool,
}

impl<T> WaitAny<T> {
    pub fn new(cells: Vec<Rc<FiberCell>>, check: impl FnMut() -> Option<T> + 'static) -> Self {
        WaitAny { cells, check: Box::new(check), registered: false }
    }
}

impl<T> Unpin for WaitAny<T> {}

impl<T> Future for WaitAny<T> {
    type Output = T;
    fn poll(mut self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<T> {
        if let Some(result) = (self.check)() {
            return Poll::Ready(result);
        }
        if !self.registered {
            self.registered = true;
            if let Some(id) = executor().current() {
                for cell in &self.cells {
                    if !cell.is_done() {
                        cell.observers.borrow_mut().push(Box::new(move |_| executor().resume(id)));
                    }
                }
            }
        }
        Poll::Pending
    }
}

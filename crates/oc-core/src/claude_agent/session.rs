//! One running `claude` with OpenCode's turn bookkeeping (`claude/session.ts`): the only reader of
//! the SDK messages, turns that end at a result or park at a tool boundary, interrupt-and-drain,
//! and an orderly stop.

use std::collections::VecDeque;
use std::sync::Arc;

use futures::FutureExt;
use futures::future::{BoxFuture, Shared};
use parking_lot::Mutex;
use serde_json::{Value, json};
use tokio::sync::{Notify, watch};

use super::convert;
use super::probe::{self, ProbeResult};
use super::process::{Delivered, Options, Query};

struct Turn {
    messages: Mutex<VecDeque<Value>>,
    wake: Notify,
    finished: watch::Sender<bool>,
}

impl Turn {
    fn new() -> Arc<Turn> {
        Arc::new(Turn { messages: Mutex::new(VecDeque::new()), wake: Notify::new(), finished: watch::channel(false).0 })
    }

    fn finished(&self) -> bool {
        *self.finished.borrow()
    }

    fn finish(&self) {
        self.finished.send_replace(true);
    }

    async fn drained(&self) {
        let mut rx = self.finished.subscribe();
        while !*rx.borrow_and_update() {
            if rx.changed().await.is_err() {
                return;
            }
        }
    }
}

#[derive(Default)]
struct State {
    turn: Option<Arc<Turn>>,
    initialized: bool,
    ended: bool,
    failure: Option<String>,
    persisted: bool,
}

/// How a step ended (`readStep`).
pub enum Step {
    Result(Value),
    /// A top-level reply ended with calls to offered tools: the turn waits for their results.
    Tools,
}

pub struct ClaudeSession {
    pub id: String,
    query: Arc<Query>,
    state: Arc<Mutex<State>>,
    ready: Shared<BoxFuture<'static, Result<ProbeResult, String>>>,
    stopping: Mutex<Option<Shared<BoxFuture<'static, ()>>>>,
    pump: Shared<BoxFuture<'static, ()>>,
}

impl ClaudeSession {
    /// `ClaudeSession.start(options)`.
    pub fn start(options: Options) -> Arc<ClaudeSession> {
        let id = options.resume.clone().or_else(|| options.session_id.clone()).unwrap_or_default();
        let query = Query::start(options);
        let state = Arc::new(Mutex::new(State::default()));
        let ready = {
            let (query, state) = (query.clone(), state.clone());
            async move {
                let init = query.initialization().await?;
                let version = init.get("claude_code_version").and_then(Value::as_str).map(String::from);
                if let Some(error) = probe::version_error(version.as_deref()) {
                    return Err(error);
                }
                state.lock().initialized = true;
                Ok(ProbeResult {
                    version,
                    account: init.get("account").cloned().unwrap_or(json!({})),
                    models: init.get("models").and_then(Value::as_array).cloned().unwrap_or_default(),
                })
            }
            .boxed()
            .shared()
        };
        // The only reader of the SDK messages, including while a turn is being cancelled.
        let pump = {
            let (query, state) = (query.clone(), state.clone());
            async move {
                loop {
                    match query.next().await {
                        Some(Delivered::Message(message)) => {
                            let kind = message.get("type").and_then(Value::as_str).unwrap_or_default().to_string();
                            let mut s = state.lock();
                            if let Some(turn) = &s.turn {
                                turn.messages.lock().push_back(message);
                                turn.wake.notify_one();
                            }
                            if kind == "assistant" || kind == "result" {
                                s.persisted = true;
                            }
                            if kind == "result"
                                && let Some(turn) = &s.turn
                            {
                                turn.finish();
                            }
                        }
                        end => {
                            let failure = match end {
                                Some(Delivered::End(failure)) => failure,
                                _ => None,
                            };
                            let mut s = state.lock();
                            s.failure = failure;
                            s.ended = true;
                            if let Some(turn) = &s.turn {
                                turn.finish();
                                turn.wake.notify_one();
                            }
                            return;
                        }
                    }
                }
            }
            .boxed()
            .shared()
        };
        tokio::spawn(pump.clone());
        Arc::new(ClaudeSession { id, query, state, ready, stopping: Mutex::new(None), pump })
    }

    /// Resolves once `claude` answered `initialize` (and its version is new enough).
    pub async fn ready(&self) -> Result<ProbeResult, String> {
        self.ready.clone().await
    }

    pub fn ended(&self) -> bool {
        self.state.lock().ended
    }

    pub fn persisted(&self) -> bool {
        self.state.lock().persisted
    }

    pub fn running(&self) -> bool {
        self.state.lock().turn.as_ref().is_some_and(|t| !t.finished())
    }

    /// Starts a new turn with this user input.
    pub fn begin(&self, content: Vec<Value>, uuid: String) -> Result<(), String> {
        let mut s = self.state.lock();
        if s.ended || self.stopping.lock().is_some() {
            return Err(s.failure.clone().unwrap_or_else(|| "Claude Code session has stopped".into()));
        }
        if s.turn.as_ref().is_some_and(|t| !t.finished()) {
            return Err("Claude Code is still finishing the previous turn".into());
        }
        s.turn = Some(Turn::new());
        drop(s);
        self.query.send_user(json!({ "type": "user", "uuid": uuid, "message": { "role": "user", "content": content }, "parent_tool_use_id": null }));
        Ok(())
    }

    /// Steers the running turn.
    pub fn add_user(&self, content: Vec<Value>, uuid: String) {
        self.query.send_user(json!({
            "type": "user",
            "uuid": uuid,
            "priority": "next",
            "message": { "role": "user", "content": content },
            "parent_tool_use_id": null,
        }));
    }

    /// Reads the turn until its result, or until a reply that called offered tools has ended.
    pub async fn read_step(&self, emit: &mut (dyn FnMut(&Value) -> Result<(), String> + Send), tools: &[Value]) -> Result<Step, String> {
        let Some(active) = self.state.lock().turn.clone() else { return Err("No Claude turn to read".into()) };
        let mut called_tools = false;
        loop {
            let message = active.messages.lock().pop_front();
            let Some(message) = message else {
                {
                    let s = self.state.lock();
                    if s.ended {
                        return Err(s.failure.clone().unwrap_or_else(|| "Claude Code stopped before finishing its reply".into()));
                    }
                }
                let notified = active.wake.notified();
                if !active.messages.lock().is_empty() || self.state.lock().ended {
                    continue;
                }
                notified.await;
                continue;
            };
            emit(&message)?;
            let kind = message.get("type").and_then(Value::as_str).unwrap_or_default();
            let top = message.get("parent_tool_use_id").is_none_or(Value::is_null);
            if kind == "assistant" && top {
                for part in message.get("message").and_then(|m| m.get("content")).and_then(Value::as_array).map(Vec::as_slice).unwrap_or(&[]) {
                    // Calls to tools we did not offer get Claude Code's own error, and the turn continues without OpenCode.
                    if part.get("type").and_then(Value::as_str) == Some("tool_use")
                        && convert::original(part.get("name").and_then(Value::as_str).unwrap_or_default(), tools).is_some()
                    {
                        called_tools = true;
                    }
                }
            }
            let reply_ended =
                kind == "stream_event" && top && message.get("event").and_then(|e| e.get("type")).and_then(Value::as_str) == Some("message_stop");
            if called_tools && reply_ended {
                return Ok(Step::Tools);
            }
            if kind == "result" {
                return Ok(Step::Result(message));
            }
        }
    }

    /// Interrupts a running turn and waits until it drained (its result arrived).
    pub async fn interrupt(&self) -> Result<(), String> {
        let active = {
            let s = self.state.lock();
            match &s.turn {
                Some(turn) if !turn.finished() && !s.ended => turn.clone(),
                _ => return Ok(()),
            }
        };
        self.query.request(json!({ "subtype": "interrupt" })).await?;
        // interrupt() resolves before the result and transcript flush, so wait for the drain before another turn starts.
        active.drained().await;
        Ok(())
    }

    async fn shutdown(query: Arc<Query>, state: Arc<Mutex<State>>, pump: Shared<BoxFuture<'static, ()>>) {
        let (initialized, ended, turn) = {
            let s = state.lock();
            (s.initialized, s.ended, s.turn.clone())
        };
        if initialized && !ended {
            let _ = query.request(json!({ "subtype": "interrupt" })).await;
            if let Some(turn) = turn.filter(|t| !t.finished()) {
                turn.drained().await;
            }
        }
        // The prompt stream ends: once the run ended, stdin closes.
        if query.sent_input() {
            query.wait_run_end().await;
        }
        query.end_input();
        // An uninitialized process has no conversation to flush.
        if !initialized {
            query.close();
        }
        pump.await;
        query.close();
    }

    /// Stops the process (once).
    pub fn stop(&self) -> Shared<BoxFuture<'static, ()>> {
        let mut stopping = self.stopping.lock();
        if let Some(stopping) = stopping.as_ref() {
            return stopping.clone();
        }
        let future = Self::shutdown(self.query.clone(), self.state.clone(), self.pump.clone()).boxed().shared();
        *stopping = Some(future.clone());
        tokio::spawn(future.clone());
        future
    }

    pub async fn set_model(&self, model: &str) -> Result<(), String> {
        self.query.request(json!({ "subtype": "set_model", "model": model })).await.map(|_| ())
    }

    pub async fn set_effort(&self, effort: Option<&str>) -> Result<(), String> {
        self.query.request(json!({ "subtype": "apply_flag_settings", "settings": { "effortLevel": effort } })).await.map(|_| ())
    }
}

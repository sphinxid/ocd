//! Stream → durable events (`core/session/runner/publish-llm-event.ts`).

use std::sync::Arc;
use std::time::Duration;

use futures::future::{BoxFuture, FutureExt, Shared};
use indexmap::IndexMap;
use parking_lot::Mutex;
use serde_json::{json, Map, Value};

use oc_ai::{LLMEvent, ToolResultValue};
use oc_schema::event::SessionEvent;
use oc_schema::types::{FinishReason, Metadata, ModelRef, SessionError, TokenUsage, ToolContent};

use crate::bus::Bus;

const DELTA_BATCH: Duration = Duration::from_millis(100);

#[derive(Debug, Clone)]
pub struct Settlement {
    pub finish: FinishReason,
    pub raw_finish: Option<String>,
    pub provider_state: Option<Metadata>,
    pub tokens: TokenUsage,
}

#[derive(Debug, Clone)]
pub struct StepRecord {
    pub output_started: bool,
    pub provider_failed: bool,
    pub failure: Option<SessionError>,
    pub finish: Option<Settlement>,
    pub needs_continuation: bool,
}

struct ToolSt {
    name: String,
    called: bool,
    settled: bool,
    provider_executed: bool,
    progress: Option<Metadata>,
}

struct Fragment {
    ordinal: u32,
    values: String,
    pending: String,
    timer: bool,
    state: Option<Metadata>,
}

#[derive(Default)]
struct Frags {
    chunks: IndexMap<String, Fragment>,
    next: u32,
}

#[derive(Clone, Copy, PartialEq)]
enum Kind {
    Text,
    Reasoning,
    ToolInput,
}

#[derive(Default)]
struct Inner {
    tools: IndexMap<String, ToolSt>,
    step_started: bool,
    provider_failed: bool,
    output_started: bool,
    failure: Option<SessionError>,
    settlement: Option<Settlement>,
    text: Frags,
    reasoning: Frags,
    tool_input: Frags,
}

impl Inner {
    fn frags(&mut self, k: Kind) -> &mut Frags {
        match k {
            Kind::Text => &mut self.text,
            Kind::Reasoning => &mut self.reasoning,
            Kind::ToolInput => &mut self.tool_input,
        }
    }
}

pub struct Input {
    pub session_id: String,
    pub agent: String,
    pub model: ModelRef,
    pub provider_metadata_key: String,
    pub started: u64,
    pub assistant_message_id: String,
    pub pending_snapshot: Option<BoxFuture<'static, Option<String>>>,
}

pub struct Publisher {
    bus: Bus,
    session_id: String,
    agent: String,
    model: ModelRef,
    key: String,
    started: u64,
    pub assistant_message_id: String,
    snapshot: Option<Shared<BoxFuture<'static, Option<String>>>>,
    inner: Mutex<Inner>,
}

fn die(msg: String) -> anyhow::Error {
    anyhow::anyhow!(msg)
}

fn stringify(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

fn as_record(v: &Value) -> Metadata {
    match v {
        Value::Object(m) => m.clone(),
        other => {
            let mut m = Map::new();
            m.insert("value".into(), other.clone());
            m
        }
    }
}

fn merge(a: Option<Metadata>, b: Option<Metadata>) -> Option<Metadata> {
    match (a, b) {
        (None, b) => b,
        (a, None) => a,
        (Some(mut a), Some(b)) => {
            a.extend(b);
            Some(a)
        }
    }
}

/// `contentFilterError(summary, reason)`
pub fn content_filter_error(summary: &str, category: Option<&str>, explanation: Option<&str>) -> SessionError {
    let head = match category {
        Some(c) => format!("{summary} ({c})"),
        None => summary.to_string(),
    };
    let message = [Some(head), explanation.map(str::to_string)].into_iter().flatten().filter(|s| !s.is_empty()).collect::<Vec<_>>().join(": ");
    SessionError::new("provider.content-filter", message)
}

/// `SessionUsage.tokens`
pub fn tokens(usage: Option<&oc_ai::Usage>) -> TokenUsage {
    let Some(u) = usage else { return TokenUsage::default() };
    TokenUsage {
        input: u.non_cached_input_tokens.unwrap_or(0),
        output: u.visible_output_tokens(),
        reasoning: u.reasoning_tokens.unwrap_or(0),
        cache: oc_schema::types::CacheUsage {
            read: u.cache_read_input_tokens.unwrap_or(0),
            write: u.cache_write_input_tokens.unwrap_or(0),
        },
    }
}

impl Publisher {
    pub fn new(bus: Bus, input: Input) -> Arc<Publisher> {
        Arc::new(Publisher {
            bus,
            session_id: input.session_id,
            agent: input.agent,
            model: input.model,
            key: input.provider_metadata_key,
            started: input.started,
            assistant_message_id: input.assistant_message_id,
            snapshot: input.pending_snapshot.map(|f| f.shared()),
            inner: Mutex::new(Inner::default()),
        })
    }

    fn state(&self, meta: &Option<oc_ai::types::ProviderMetadata>) -> Option<Metadata> {
        meta.as_ref().and_then(|m| m.get(&self.key)).and_then(Value::as_object).cloned()
    }

    fn commit(&self, e: SessionEvent) -> anyhow::Result<()> {
        self.bus.commit_one(e)?;
        Ok(())
    }

    pub fn has_started(&self) -> bool {
        self.inner.lock().step_started
    }

    pub fn has_provider_error(&self) -> bool {
        self.inner.lock().provider_failed
    }

    pub async fn start_assistant(&self) -> anyhow::Result<String> {
        if self.inner.lock().step_started {
            return Ok(self.assistant_message_id.clone());
        }
        let snapshot = match &self.snapshot {
            Some(s) => s.clone().await,
            None => None,
        };
        {
            let mut g = self.inner.lock();
            if g.step_started {
                return Ok(self.assistant_message_id.clone());
            }
            g.step_started = true;
        }
        self.commit(SessionEvent::StepStarted {
            session_id: self.session_id.clone(),
            assistant_message_id: self.assistant_message_id.clone(),
            agent: self.agent.clone(),
            model: self.model.clone(),
            snapshot,
            started: self.started,
        })?;
        Ok(self.assistant_message_id.clone())
    }

    pub async fn streamed(&self) -> anyhow::Result<()> {
        let id = self.start_assistant().await?;
        self.commit(SessionEvent::StepStreamed { session_id: self.session_id.clone(), assistant_message_id: id })
    }

    fn current_id(&self) -> anyhow::Result<String> {
        if self.inner.lock().step_started {
            Ok(self.assistant_message_id.clone())
        } else {
            Err(die("Tool event before assistant step start".into()))
        }
    }

    // ----- fragments -----

    fn frag_name(k: Kind) -> &'static str {
        match k {
            Kind::Text => "text",
            Kind::Reasoning => "reasoning",
            Kind::ToolInput => "tool input",
        }
    }

    fn frag_start(&self, k: Kind, id: &str, state: Option<Metadata>) -> anyhow::Result<u32> {
        let mut g = self.inner.lock();
        let f = g.frags(k);
        if f.chunks.contains_key(id) {
            return Err(die(format!("Duplicate {} start: {id}", Self::frag_name(k))));
        }
        if k != Kind::ToolInput && !f.chunks.is_empty() {
            return Err(die(format!("{} start before end: {id}", Self::frag_name(k))));
        }
        let ordinal = f.next;
        f.next += 1;
        f.chunks.insert(id.into(), Fragment { ordinal, values: String::new(), pending: String::new(), timer: false, state });
        Ok(ordinal)
    }

    fn publish_delta(&self, k: Kind, id: &str) -> anyhow::Result<()> {
        if k == Kind::ToolInput {
            return Ok(());
        }
        let (value, ordinal) = {
            let mut g = self.inner.lock();
            let Some(c) = g.frags(k).chunks.get_mut(id) else {
                return Err(die(format!("{} delta before start: {id}", Self::frag_name(k))));
            };
            if c.pending.is_empty() {
                return Ok(());
            }
            (std::mem::take(&mut c.pending), c.ordinal)
        };
        let kind = if k == Kind::Text { "session.text.delta" } else { "session.reasoning.delta" };
        self.bus.publish_session_ephemeral(
            &self.session_id,
            kind,
            json!({"sessionID": self.session_id, "assistantMessageID": self.current_id()?, "ordinal": ordinal, "delta": value}),
        );
        Ok(())
    }

    fn frag_append(self: &Arc<Self>, k: Kind, id: &str, value: &str, state: Option<Metadata>) -> anyhow::Result<()> {
        let spawn = {
            let mut g = self.inner.lock();
            let Some(c) = g.frags(k).chunks.get_mut(id) else {
                return Err(die(format!("{} delta before start: {id}", Self::frag_name(k))));
            };
            c.values.push_str(value);
            if k != Kind::ToolInput {
                c.pending.push_str(value);
            }
            if state.is_some() {
                c.state = merge(c.state.take(), state);
            }
            let spawn = !c.pending.is_empty() && !c.timer;
            if spawn {
                c.timer = true;
            }
            spawn
        };
        if spawn {
            let this = self.clone();
            let id = id.to_string();
            tokio::spawn(async move {
                loop {
                    tokio::time::sleep(DELTA_BATCH).await;
                    let more = {
                        let mut g = this.inner.lock();
                        match g.frags(k).chunks.get(&id) {
                            Some(c) if c.timer && !c.pending.is_empty() => true,
                            _ => false,
                        }
                    };
                    if !more {
                        break;
                    }
                    let _ = this.publish_delta(k, &id);
                }
                if let Some(c) = this.inner.lock().frags(k).chunks.get_mut(&id) {
                    c.timer = false;
                }
            });
        }
        Ok(())
    }

    fn frag_end(&self, k: Kind, id: &str, state: Option<Metadata>, value: Option<String>) -> anyhow::Result<()> {
        {
            let mut g = self.inner.lock();
            let Some(c) = g.frags(k).chunks.get_mut(id) else {
                return Err(die(format!("{} end before start: {id}", Self::frag_name(k))));
            };
            c.timer = false;
        }
        self.publish_delta(k, id)?;
        let frag = self.inner.lock().frags(k).chunks.shift_remove(id).unwrap();
        let text = value.unwrap_or(frag.values);
        let state = if state.is_none() { frag.state } else { merge(frag.state, state) };
        match k {
            Kind::Text => self.commit(SessionEvent::TextEnded {
                session_id: self.session_id.clone(),
                assistant_message_id: self.current_id()?,
                ordinal: frag.ordinal,
                text,
                state,
            }),
            Kind::Reasoning => self.commit(SessionEvent::ReasoningEnded {
                session_id: self.session_id.clone(),
                assistant_message_id: self.current_id()?,
                ordinal: frag.ordinal,
                text,
                state,
            }),
            Kind::ToolInput => {
                if !self.inner.lock().tools.contains_key(id) {
                    return Err(die(format!("Tool input end before start: {id}")));
                }
                self.commit(SessionEvent::ToolInputEnded {
                    session_id: self.session_id.clone(),
                    assistant_message_id: self.assistant_message_id.clone(),
                    id: id.into(),
                    text,
                })
            }
        }
    }

    fn frag_has(&self, k: Kind, id: &str) -> bool {
        self.inner.lock().frags(k).chunks.contains_key(id)
    }

    fn publish_pending(&self, k: Kind) -> anyhow::Result<()> {
        let ids: Vec<String> = {
            let mut g = self.inner.lock();
            let f = g.frags(k);
            for c in f.chunks.values_mut() {
                c.timer = false;
            }
            f.chunks.keys().cloned().collect()
        };
        for id in ids {
            self.publish_delta(k, &id)?;
        }
        Ok(())
    }

    fn publish_pending_deltas(&self) -> anyhow::Result<()> {
        self.publish_pending(Kind::Text)?;
        self.publish_pending(Kind::Reasoning)
    }

    pub fn flush(&self) -> anyhow::Result<()> {
        for k in [Kind::Text, Kind::Reasoning, Kind::ToolInput] {
            let ids: Vec<String> = self.inner.lock().frags(k).chunks.keys().cloned().collect();
            for id in ids {
                self.frag_end(k, &id, None, None)?;
            }
        }
        Ok(())
    }

    // ----- tools -----

    async fn start_tool_input(&self, id: &str, name: &str, provider_executed: bool) -> anyhow::Result<()> {
        if self.inner.lock().tools.contains_key(id) {
            return Err(die(format!("Duplicate tool input start: {id}")));
        }
        self.start_assistant().await?;
        self.inner.lock().tools.insert(
            id.into(),
            ToolSt { name: name.into(), called: false, settled: false, provider_executed, progress: None },
        );
        self.frag_start(Kind::ToolInput, id, None)?;
        self.publish_pending_deltas()?;
        self.commit(SessionEvent::ToolInputStarted {
            session_id: self.session_id.clone(),
            assistant_message_id: self.assistant_message_id.clone(),
            id: id.into(),
            name: name.into(),
        })
    }

    fn check_name(&self, id: &str, name: &str, what: &str) -> anyhow::Result<()> {
        let g = self.inner.lock();
        let Some(t) = g.tools.get(id) else { return Err(die(format!("{what} before start: {id}"))) };
        if t.name != name {
            return Err(die(format!("{what} name changed for {id}: {} -> {name}", t.name)));
        }
        Ok(())
    }

    fn end_tool_input(&self, id: &str, name: &str, value: Option<String>) -> anyhow::Result<()> {
        self.check_name(id, name, "Tool input")?;
        if !self.frag_has(Kind::ToolInput, id) {
            return Err(die(format!("Duplicate tool input end: {id}")));
        }
        self.frag_end(Kind::ToolInput, id, None, value)
    }

    fn failure_metadata(progress: &Option<Metadata>, metadata: Option<Metadata>) -> Option<Metadata> {
        match (progress.clone(), metadata) {
            (None, m) => m,
            (Some(p), None) => Some(p),
            (Some(mut p), Some(m)) => {
                p.extend(m);
                Some(p)
            }
        }
    }

    pub fn fail_tool(&self, id: &str, error: SessionError, metadata: Option<Metadata>) -> anyhow::Result<bool> {
        let (error, meta, executed) = {
            let mut g = self.inner.lock();
            let Some(t) = g.tools.get_mut(id) else { return Ok(false) };
            if t.settled {
                return Ok(false);
            }
            t.settled = true;
            let child = t.progress.as_ref().and_then(|p| p.get("sessionID")).and_then(Value::as_str).map(str::to_string);
            let error = match child {
                Some(c) if t.name == "subagent" && error.kind == "aborted" => {
                    SessionError { message: format!("{} (sessionID: {c})", error.message), ..error }
                }
                _ => error,
            };
            (error, Self::failure_metadata(&t.progress, metadata), t.provider_executed)
        };
        self.commit(SessionEvent::ToolFailed {
            session_id: self.session_id.clone(),
            assistant_message_id: self.assistant_message_id.clone(),
            id: id.into(),
            error,
            content: None,
            metadata: meta,
            executed,
            result_state: None,
        })?;
        Ok(true)
    }

    /// mode: "all" | "hosted" | "uncalled"
    pub fn fail_unsettled_tools(&self, error: &SessionError, mode: &str) -> anyhow::Result<bool> {
        let ids: Vec<String> = self
            .inner
            .lock()
            .tools
            .iter()
            .filter(|(_, t)| !(t.settled || (mode == "hosted" && !t.provider_executed) || (mode == "uncalled" && t.called)))
            .map(|(k, _)| k.clone())
            .collect();
        let mut failed = false;
        for id in ids {
            failed = self.fail_tool(&id, error.clone(), None)? || failed;
        }
        Ok(failed)
    }

    pub async fn fail_assistant(&self, error: SessionError) -> anyhow::Result<()> {
        self.flush()?;
        self.fail_unsettled_tools(&error, "uncalled")?;
        self.start_assistant().await?;
        let mut g = self.inner.lock();
        if g.failure.is_none() {
            g.failure = Some(error);
        }
        Ok(())
    }

    pub async fn publish_step_failure(
        &self,
        cost: Option<f64>,
        tokens: Option<TokenUsage>,
        snapshot: Option<String>,
        files: Option<Vec<String>>,
    ) -> anyhow::Result<()> {
        let (failure, settlement) = {
            let g = self.inner.lock();
            (g.failure.clone(), g.settlement.clone())
        };
        let Some(error) = failure else { return Ok(()) };
        let id = self.start_assistant().await?;
        self.commit(SessionEvent::StepFailed {
            session_id: self.session_id.clone(),
            assistant_message_id: id,
            error,
            finish: settlement.as_ref().filter(|s| s.finish == FinishReason::ContentFilter).map(|s| s.finish),
            raw_finish: settlement.as_ref().and_then(|s| s.raw_finish.clone()),
            provider_state: settlement.as_ref().and_then(|s| s.provider_state.clone()),
            cost,
            tokens,
            snapshot,
            files,
        })
    }

    pub fn progress(&self, id: &str, update: Metadata) -> anyhow::Result<()> {
        {
            let mut g = self.inner.lock();
            match g.tools.get_mut(id) {
                Some(t) if t.called && !t.settled => t.progress = Some(update.clone()),
                _ => return Err(die(format!("Tool progress outside running call: {id}"))),
            }
        }
        self.bus.publish_session_ephemeral(
            &self.session_id,
            "session.tool.progress",
            json!({"sessionID": self.session_id, "assistantMessageID": self.assistant_message_id, "id": id, "metadata": update}),
        );
        Ok(())
    }

    /// Terminal success for a locally executed tool.
    pub fn tool_execution(&self, id: &str, name: &str, content: Vec<ToolContent>, metadata: Option<Metadata>) -> anyhow::Result<()> {
        let executed = {
            let mut g = self.inner.lock();
            let Some(t) = g.tools.get_mut(id).filter(|t| t.called) else {
                return Err(die(format!("Tool execution before call: {id}")));
            };
            if t.name != name {
                return Err(die(format!("Tool execution name changed for {id}: {} -> {name}", t.name)));
            }
            if t.settled {
                return Err(die(format!("Duplicate tool execution: {id}")));
            }
            t.settled = true;
            t.provider_executed
        };
        if content.is_empty() {
            return Err(die(format!("Tool execution has no content: {id}")));
        }
        self.commit(SessionEvent::ToolSuccess {
            session_id: self.session_id.clone(),
            assistant_message_id: self.assistant_message_id.clone(),
            id: id.into(),
            content,
            metadata,
            executed,
            result_state: None,
        })
    }

    pub fn tool_name(&self, id: &str) -> Option<String> {
        self.inner.lock().tools.get(id).map(|t| t.name.clone())
    }

    pub fn record(&self) -> StepRecord {
        let g = self.inner.lock();
        StepRecord {
            output_started: g.output_started,
            provider_failed: g.provider_failed,
            failure: g.failure.clone(),
            finish: g.settlement.clone(),
            needs_continuation: g.tools.values().any(|t| !t.provider_executed && (t.called || t.settled)),
        }
    }

    /// Fold one provider event into durable events.
    pub async fn publish(self: &Arc<Self>, event: &LLMEvent) -> anyhow::Result<()> {
        match event {
            LLMEvent::StepStart { .. } => {
                self.start_assistant().await?;
            }
            LLMEvent::TextStart { id, provider_metadata } => {
                self.inner.lock().output_started = true;
                let ordinal = self.frag_start(Kind::Text, id, self.state(provider_metadata))?;
                self.publish_pending_deltas()?;
                let aid = self.start_assistant().await?;
                self.commit(SessionEvent::TextStarted { session_id: self.session_id.clone(), assistant_message_id: aid, ordinal })?;
            }
            LLMEvent::TextDelta { id, text, provider_metadata } => {
                self.frag_append(Kind::Text, id, text, self.state(provider_metadata))?
            }
            LLMEvent::TextEnd { id, text, provider_metadata } => {
                self.frag_end(Kind::Text, id, self.state(provider_metadata), text.clone())?
            }
            LLMEvent::ReasoningStart { id, provider_metadata } => {
                self.inner.lock().output_started = true;
                let state = self.state(provider_metadata);
                let ordinal = self.frag_start(Kind::Reasoning, id, state.clone())?;
                self.publish_pending_deltas()?;
                let aid = self.start_assistant().await?;
                self.commit(SessionEvent::ReasoningStarted {
                    session_id: self.session_id.clone(),
                    assistant_message_id: aid,
                    ordinal,
                    state,
                })?;
            }
            LLMEvent::ReasoningDelta { id, text, provider_metadata } => {
                self.frag_append(Kind::Reasoning, id, text, self.state(provider_metadata))?
            }
            LLMEvent::ReasoningEnd { id, text, provider_metadata } => {
                self.frag_end(Kind::Reasoning, id, self.state(provider_metadata), text.clone())?
            }
            LLMEvent::ToolInputStart { id, name, provider_executed, .. } => {
                self.inner.lock().output_started = true;
                self.start_tool_input(id, name, *provider_executed == Some(true)).await?;
            }
            LLMEvent::ToolInputDelta { id, name, text, .. } => {
                self.check_name(id, name, "Tool input delta")?;
                if !self.frag_has(Kind::ToolInput, id) {
                    return Err(die(format!("Tool input delta after end: {id}")));
                }
                self.frag_append(Kind::ToolInput, id, text, None)?;
            }
            LLMEvent::ToolInputEnd { id, name, .. } => self.end_tool_input(id, name, None)?,
            LLMEvent::ToolInputError { id, name, raw, .. } => {
                self.inner.lock().output_started = true;
                if !self.inner.lock().tools.contains_key(id) {
                    self.start_tool_input(id, name, false).await?;
                }
                {
                    let g = self.inner.lock();
                    let t = &g.tools[id];
                    if t.called || t.settled {
                        return Err(die(format!("Malformed tool input after call settlement: {id}")));
                    }
                    if &t.name != name {
                        return Err(die(format!("Tool input name changed for {id}: {} -> {name}", t.name)));
                    }
                }
                if self.frag_has(Kind::ToolInput, id) {
                    self.end_tool_input(id, name, Some(raw.clone()))?;
                }
                let progress = {
                    let mut g = self.inner.lock();
                    let t = g.tools.get_mut(id).unwrap();
                    t.settled = true;
                    t.progress.clone()
                };
                self.commit(SessionEvent::ToolFailed {
                    session_id: self.session_id.clone(),
                    assistant_message_id: self.assistant_message_id.clone(),
                    id: id.clone(),
                    error: SessionError::new(
                        "tool.input-json",
                        "Tool call arguments were malformed JSON and were not executed. Retry with valid JSON.",
                    ),
                    content: None,
                    metadata: progress,
                    executed: false,
                    result_state: None,
                })?;
            }
            LLMEvent::ToolCall { id, name, input, provider_executed, provider_metadata, .. } => {
                self.inner.lock().output_started = true;
                if !self.inner.lock().tools.contains_key(id) {
                    self.start_tool_input(id, name, *provider_executed == Some(true)).await?;
                }
                if self.frag_has(Kind::ToolInput, id) {
                    self.end_tool_input(id, name, None)?;
                }
                let executed = {
                    let mut g = self.inner.lock();
                    let t = g.tools.get_mut(id).unwrap();
                    if &t.name != name {
                        return Err(die(format!("Tool call name changed for {id}: {} -> {name}", t.name)));
                    }
                    if t.called {
                        return Err(die(format!("Duplicate tool call: {id}")));
                    }
                    t.called = true;
                    t.provider_executed = *provider_executed == Some(true);
                    t.provider_executed
                };
                self.commit(SessionEvent::ToolCalled {
                    session_id: self.session_id.clone(),
                    assistant_message_id: self.assistant_message_id.clone(),
                    id: id.clone(),
                    input: as_record(input),
                    executed,
                    state: self.state(provider_metadata),
                })?;
            }
            LLMEvent::ToolResult { id, name, result, provider_executed, provider_metadata, .. } => {
                let (executed, progress) = {
                    let mut g = self.inner.lock();
                    let Some(t) = g.tools.get_mut(id).filter(|t| t.called) else {
                        return Err(die(format!("Tool result before call: {id}")));
                    };
                    if &t.name != name {
                        return Err(die(format!("Tool result name changed for {id}: {} -> {name}", t.name)));
                    }
                    if t.settled {
                        if matches!(result, ToolResultValue::Error(_)) {
                            return Ok(());
                        }
                        return Err(die(format!("Duplicate tool result: {id}")));
                    }
                    t.settled = true;
                    (*provider_executed == Some(true) || t.provider_executed, t.progress.clone())
                };
                let result_state = self.state(provider_metadata);
                match result {
                    ToolResultValue::Error(v) => self.commit(SessionEvent::ToolFailed {
                        session_id: self.session_id.clone(),
                        assistant_message_id: self.assistant_message_id.clone(),
                        id: id.clone(),
                        error: SessionError::new("tool.execution", stringify(v)),
                        content: None,
                        metadata: progress,
                        executed,
                        result_state,
                    })?,
                    other => {
                        let content = match other {
                            ToolResultValue::Content(c) if !c.is_empty() => serde_json::from_value(serde_json::to_value(c)?)
                                .unwrap_or_else(|_| vec![ToolContent::Text { text: stringify(&serde_json::to_value(c).unwrap()) }]),
                            ToolResultValue::Json(v) | ToolResultValue::Text(v) => vec![ToolContent::Text { text: stringify(v) }],
                            ToolResultValue::Content(c) => vec![ToolContent::Text { text: stringify(&serde_json::to_value(c)?) }],
                            ToolResultValue::Error(_) => unreachable!(),
                        };
                        self.commit(SessionEvent::ToolSuccess {
                            session_id: self.session_id.clone(),
                            assistant_message_id: self.assistant_message_id.clone(),
                            id: id.clone(),
                            content,
                            metadata: None,
                            executed,
                            result_state,
                        })?
                    }
                }
            }
            LLMEvent::ToolError { id, name, message, provider_metadata, .. } => {
                let (executed, progress) = {
                    let mut g = self.inner.lock();
                    let Some(t) = g.tools.get_mut(id).filter(|t| t.called) else {
                        return Err(die(format!("Tool error before call: {id}")));
                    };
                    if &t.name != name {
                        return Err(die(format!("Tool error name changed for {id}: {} -> {name}", t.name)));
                    }
                    if t.settled {
                        return Err(die(format!("Duplicate tool error: {id}")));
                    }
                    t.settled = true;
                    (t.provider_executed, t.progress.clone())
                };
                let kind = if *message == format!("Unknown tool: {name}") { "tool.unknown" } else { "tool.execution" };
                self.commit(SessionEvent::ToolFailed {
                    session_id: self.session_id.clone(),
                    assistant_message_id: self.assistant_message_id.clone(),
                    id: id.clone(),
                    error: SessionError::new(kind, message.clone()),
                    content: None,
                    metadata: progress,
                    executed,
                    result_state: self.state(provider_metadata),
                })?;
            }
            LLMEvent::StepFinish { reason, usage, provider_metadata, .. } => {
                self.flush()?;
                {
                    let mut g = self.inner.lock();
                    if g.settlement.is_some() {
                        return Err(die("Duplicate step finish".into()));
                    }
                    g.settlement = Some(Settlement {
                        finish: reason.normalized,
                        raw_finish: reason.raw.clone(),
                        provider_state: self.state(provider_metadata),
                        tokens: tokens(usage.as_ref()),
                    });
                }
                if reason.normalized == FinishReason::ContentFilter {
                    self.inner.lock().provider_failed = true;
                    self.fail_assistant(content_filter_error(
                        "Provider blocked the response",
                        reason.category.as_deref(),
                        reason.explanation.as_deref(),
                    ))
                    .await?;
                }
            }
            LLMEvent::Finish { .. } | LLMEvent::Media { .. } | LLMEvent::Compaction(_) => {}
            LLMEvent::ProviderError { message, .. } => {
                self.inner.lock().provider_failed = true;
                self.fail_assistant(SessionError::new("provider.unknown", message.clone())).await?;
            }
        }
        Ok(())
    }
}

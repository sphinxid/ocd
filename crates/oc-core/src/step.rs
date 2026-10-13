//! One step attempt (`core/session/runner/step.ts`): stream the provider response, publish it,
//! run local tools concurrently, then settle the step.

use std::sync::Arc;

use async_trait::async_trait;
use futures::future::FutureExt;
use futures::StreamExt;
use tokio_util::sync::CancellationToken;

use oc_ai::{AIError, Client, ErrorKind, LLMEvent, LLMRequest};
use oc_schema::event::SessionEvent;
use oc_schema::types::{FinishReason, Metadata, ModelRef, SessionError, ToolContent};

use crate::bus::Bus;
use crate::errors::{from_ai, StepFailed};
use crate::publisher::{Input as PubInput, Publisher};
use crate::retry::{Decision, Policy};
use crate::usage::{calculate_cost, Cost};

pub struct ToolCall {
    pub id: String,
    pub name: String,
    pub input: serde_json::Value,
}

pub enum ToolRun {
    Ok { content: Vec<ToolContent>, metadata: Option<Metadata> },
    Failed { error: SessionError, metadata: Option<Metadata> },
    /// User declined (`Permission.DeclinedError`), question dismissed (message), or location closed.
    Declined { question: Option<String> },
    Interrupted,
    Defect(String),
}

pub type Progress = Arc<dyn Fn(Metadata) + Send + Sync>;

/// `session.retry` hook: receives the error, attempt and proposed decision JSON, returns the decision.
pub type RetryHook = Arc<dyn Fn(SessionError, u32, serde_json::Value) -> futures::future::BoxFuture<'static, serde_json::Value> + Send + Sync>;

#[async_trait]
pub trait ToolRunner: Send + Sync {
    async fn run(&self, call: ToolCall, progress: Progress, cancel: CancellationToken) -> ToolRun;
}

#[async_trait]
pub trait Snapshotter: Send + Sync {
    async fn capture(&self) -> Option<String>;
    async fn files(&self, from: &str, to: &str) -> Option<Vec<String>>;
}

pub enum Outcome {
    Completed { needs_continuation: bool },
    Retry { error: SessionError, decision: Decision },
    Continue { error: SessionError, decision: Decision },
    RecoverFull,
    Compacted,
}

#[derive(Debug, thiserror::Error)]
pub enum StepError {
    #[error("interrupted")]
    Interrupted,
    #[error("{0}")]
    Failed(#[from] StepFailed),
    #[error(transparent)]
    Defect(#[from] anyhow::Error),
}

pub struct Input<'a> {
    pub bus: Bus,
    pub client: Client,
    pub session_id: String,
    pub assistant_message_id: String,
    pub agent: String,
    pub model: ModelRef,
    pub provider_metadata_key: String,
    pub cost: Vec<Cost>,
    pub request: LLMRequest,
    pub tool_choice_none: bool,
    pub tools: Arc<dyn ToolRunner>,
    pub snapshots: Option<Arc<dyn Snapshotter>>,
    pub recover_continuation: bool,
    pub retry: &'a mut Policy,
    pub cancel: CancellationToken,
    pub location_closed: Arc<dyn Fn() -> bool + Send + Sync>,
    /// `session.retry` plugin hook: `(error, attempt, decision)` → decision.
    pub retry_hook: Option<RetryHook>,
    /// `session.http.request` / `session.http.response` middleware.
    pub http_hook: Option<Arc<dyn oc_ai::route::transport::HttpHook>>,
    /// The session's Responses WebSocket (`StreamOptions.webSocket`) for WebSocket-transport models.
    pub web_socket: Option<Arc<dyn oc_ai::route::websocket::ChannelExecutor>>,
    /// Runner-owned overflow recovery (compaction); returns whether it succeeded.
    pub recover_overflow: Box<dyn FnOnce() -> futures::future::BoxFuture<'a, bool> + Send + 'a>,
}

fn aborted(m: &str) -> SessionError {
    SessionError::new("aborted", m)
}

fn is_interrupted_stream(e: &AIError) -> bool {
    e.is_interrupted_stream()
}

fn recovery_full(e: &AIError) -> bool {
    matches!(&e.kind, ErrorKind::Transport { recovery: Some(r), .. } if r == "retry-full" || r == "rotate-and-retry-full")
}

fn overflow_event(e: &LLMEvent) -> bool {
    matches!(e, LLMEvent::ProviderError { classification: Some(c), .. } if c == "context-overflow")
}

struct Run {
    handle: tokio::task::JoinHandle<ToolRun>,
    id: String,
    cancel: CancellationToken,
}

pub async fn attempt(input: Input<'_>) -> Result<Outcome, StepError> {
    let snapshots = input.snapshots.clone();
    let start_snapshot = {
        let s = snapshots.clone();
        async move {
            match s {
                Some(s) => s.capture().await,
                None => None,
            }
        }
        .boxed()
        .shared()
    };
    // Upstream forks the start-snapshot capture so it overlaps the provider request
    // (session/runner/step.ts `pendingStartSnapshot`); drive it now, not only when the step starts. An
    // interruption before any provider output then still records the step when the capture finished
    // (always, without snapshots): `step.started` + `step.failed` "Step interrupted".
    let _snapshot_driver = tokio::spawn(start_snapshot.clone());
    let publisher = Publisher::new(
        input.bus.clone(),
        PubInput {
            session_id: input.session_id.clone(),
            agent: input.agent.clone(),
            model: input.model.clone(),
            provider_metadata_key: input.provider_metadata_key.clone(),
            started: oc_schema::id::now_ms(),
            assistant_message_id: input.assistant_message_id.clone(),
            pending_snapshot: Some(start_snapshot.clone().boxed()),
        },
    );
    let mut runs: Vec<Run> = Vec::new();
    let mut overflow: Option<LLMEvent> = None;
    let stream_cancel = input.cancel.child_token();

    // ---- provider stream ----
    let stream_result: Result<(), AIError> = async {
        let options = oc_ai::StreamOptions { http: input.http_hook.clone(), web_socket: input.web_socket.clone() };
        let mut stream = input.client.stream_opts(input.request.clone(), stream_cancel.clone(), options).await?;
        while let Some(item) = stream.next().await {
            let event = item?;
            if overflow.is_some() || publisher.has_provider_error() {
                continue;
            }
            if overflow_event(&event) && !publisher.record().output_started {
                overflow = Some(event);
                continue;
            }
            publisher.publish(&event).await.map_err(|e| AIError::new(ErrorKind::UnknownProvider, e.to_string()))?;
            if let LLMEvent::ToolCall { id, name, input: args, provider_executed, .. } = &event {
                if *provider_executed == Some(true) {
                    continue;
                }
                let cancel = input.cancel.child_token();
                let call = ToolCall { id: id.clone(), name: name.clone(), input: args.clone() };
                let p = publisher.clone();
                let call_id = id.clone();
                let progress: Progress = Arc::new(move |m| {
                    let _ = p.progress(&call_id, m);
                });
                let tools = input.tools.clone();
                let c = cancel.clone();
                let none = input.tool_choice_none;
                let p = publisher.clone();
                // Each tool publishes its terminal event as soon as it settles, concurrently with the stream.
                let handle = tokio::spawn(async move {
                    let run = if none {
                        ToolRun::Failed {
                            error: SessionError::new("tool.execution", "Tools are disabled after the maximum agent steps"),
                            metadata: None,
                        }
                    } else {
                        tools.run(ToolCall { id: call.id.clone(), name: call.name.clone(), input: call.input }, progress, c).await
                    };
                    match run {
                        ToolRun::Ok { content, metadata } => match p.tool_execution(&call.id, &call.name, content, metadata) {
                            Ok(()) => ToolRun::Ok { content: vec![], metadata: None },
                            Err(e) => ToolRun::Defect(e.to_string()),
                        },
                        ToolRun::Failed { error, metadata } => match p.fail_tool(&call.id, error, metadata) {
                            Ok(_) => ToolRun::Ok { content: vec![], metadata: None },
                            Err(e) => ToolRun::Defect(e.to_string()),
                        },
                        other => other,
                    }
                });
                runs.push(Run { handle, id: id.clone(), cancel });
            }
        }
        Ok(())
    }
    .await;
    let _ = publisher.flush();
    let stream_interrupted = input.cancel.is_cancelled();
    let stream_failure = stream_result.err();

    if stream_interrupted && !publisher.has_started() && start_snapshot.peek().is_none() {
        return Err(StepError::Interrupted);
    }
    if overflow.is_none() && publisher.has_started() {
        publisher.streamed().await?;
    }
    if stream_interrupted {
        for r in &runs {
            r.cancel.cancel();
        }
    }

    // ---- join tools ----
    let mut declines: Vec<(String, Option<String>)> = Vec::new();
    let mut tools_interrupted = false;
    let mut tool_defect: Option<String> = None;
    for r in runs.drain(..) {
        let result = r.handle.await.unwrap_or(ToolRun::Interrupted);
        match result {
            ToolRun::Ok { .. } | ToolRun::Failed { .. } => {}
            ToolRun::Declined { question } => declines.push((r.id, question)),
            ToolRun::Interrupted => tools_interrupted = true,
            ToolRun::Defect(m) => {
                tool_defect.get_or_insert(m);
            }
        }
    }

    // An interrupted stream is not a provider failure: upstream's stream exit is an interruption, not an
    // `AIError`, so the aborted request (`Request aborted`) must not be retried or reported
    // (session/runner/step.ts `llmFailure = streamFailure instanceof AIError ? ...`).
    let provider_failure = if stream_interrupted { None } else { stream_failure.clone() };

    // ---- overflow recovery ----
    let overflow_failure = overflow.is_some() || provider_failure.as_ref().is_some_and(AIError::is_context_overflow);
    if !publisher.record().output_started && overflow_failure && (input.recover_overflow)().await {
        tracing::warn!(sessionID = %input.session_id, "provider rejected the request as too long; compacting");
        return Ok(Outcome::Compacted);
    }
    if let Some(ev) = overflow.take() {
        publisher.publish(&ev).await?;
    }
    let recorded = publisher.record();
    let unknown_finish = stream_failure.is_none()
        && recorded.finish.as_ref().is_some_and(|f| f.finish == FinishReason::Unknown);
    let llm_failure: Option<AIError> = provider_failure.clone().or_else(|| {
        unknown_finish.then(|| {
            AIError::new(
                ErrorKind::InvalidProviderOutput { classification: Some("incomplete-stream".into()), route: None },
                "The provider response ended with an unknown finish reason.",
            )
        })
    });
    let llm_error = llm_failure.as_ref().filter(|_| !recorded.provider_failed).map(from_ai);
    if input.recover_continuation && llm_failure.as_ref().is_some_and(recovery_full) && !recorded.output_started {
        return Ok(Outcome::RecoverFull);
    }
    let retry = match (&llm_failure, &llm_error) {
        (Some(f), Some(_)) if !f.is_context_overflow() => {
            let retryable = f.is_retryable() || (recorded.output_started && is_interrupted_stream(f));
            match (input.retry.decide_raw(f, retryable), &input.retry_hook, &llm_error) {
                (None, _, _) => None,
                (Some((d, ok)), None, _) => ok.then_some(d),
                (Some((d, ok)), Some(hook), Some(error)) => {
                    let proposed = if ok { serde_json::json!({ "retry": true, "delay": d.delay }) } else { serde_json::json!({ "retry": false }) };
                    let decided = hook(error.clone(), d.attempt, proposed).await;
                    if decided.get("retry").and_then(serde_json::Value::as_bool) == Some(true) {
                        let delay = decided
                            .get("delay")
                            .and_then(serde_json::Value::as_f64)
                            .filter(|v| v.is_finite() && *v >= 0.0)
                            .map(|v| v.ceil() as u64)
                            .unwrap_or(d.delay);
                        Some(Decision { attempt: d.attempt, delay })
                    } else {
                        None
                    }
                }
                (Some((d, ok)), Some(_), None) => ok.then_some(d),
            }
        }
        _ => None,
    };
    if let (Some(error), Some(decision)) = (&llm_error, retry) {
        if !recorded.output_started {
            publisher.start_assistant().await?;
            return Ok(Outcome::Retry { error: error.clone(), decision });
        }
    }
    if let Some(e) = &llm_error {
        publisher.fail_assistant(e.clone()).await?;
    }
    for (id, question) in &declines {
        let message = if (input.location_closed)() {
            "Interaction cancelled because the location shut down".to_string()
        } else {
            question.clone().unwrap_or_else(|| "The user declined this tool call".into())
        };
        publisher.fail_tool(id, aborted(&message), None)?;
    }
    let interrupted = !declines.is_empty() || stream_interrupted || tools_interrupted;
    let tool_failure = if interrupted {
        Some(aborted("Tool execution interrupted"))
    } else if let Some(d) = &tool_defect {
        Some(SessionError::new("unknown", d.clone()))
    } else if recorded.provider_failed {
        Some(aborted("Tool execution interrupted"))
    } else {
        None
    };
    if let Some(f) = &tool_failure {
        publisher.fail_unsettled_tools(f, "all")?;
    }
    if interrupted {
        publisher.fail_assistant(aborted("Step interrupted")).await?;
    }
    if llm_error.is_some() || (stream_failure.is_none() && !recorded.provider_failed) {
        let missing = SessionError::new("tool.result-missing", "Provider did not return a tool result");
        let any = publisher.fail_unsettled_tools(&missing, "hosted")?;
        if any && llm_error.is_none() && recorded.finish.is_none() {
            publisher.fail_assistant(missing).await?;
        }
        publisher.fail_unsettled_tools(
            &SessionError::new(
                "tool.input-incomplete",
                "Tool call arguments were not completed and were not executed. Re-issue the tool call with complete arguments.",
            ),
            "uncalled",
        )?;
    }

    let record = publisher.record();
    if record.finish.is_some() || record.failure.is_some() {
        let start = start_snapshot.clone().await;
        let end = match &snapshots {
            Some(s) => s.capture().await,
            None => None,
        };
        let files = match (&start, &end, &snapshots) {
            (Some(a), Some(b), _) if a == b => Some(vec![]),
            (Some(a), Some(b), Some(s)) => s.files(a, b).await,
            _ => None,
        };
        let usage = record.finish.as_ref().map(|f| (calculate_cost(&input.cost, &f.tokens), f.tokens));
        if record.failure.is_some() {
            publisher.publish_step_failure(usage.map(|u| u.0), usage.map(|u| u.1), end.clone(), files.clone()).await?;
        } else if let (Some(f), Some((cost, tokens))) = (&record.finish, usage) {
            let aid = publisher.start_assistant().await?;
            input.bus.commit_one(SessionEvent::StepEnded {
                session_id: input.session_id.clone(),
                assistant_message_id: aid,
                finish: f.finish,
                raw_finish: f.raw_finish.clone(),
                provider_state: f.provider_state.clone(),
                cost,
                tokens,
                snapshot: end,
                files,
            })?;
        }
    }

    if let (Some(error), Some(decision)) = (&llm_error, retry) {
        if record.output_started && declines.is_empty() && !tools_interrupted {
            return Ok(Outcome::Continue { error: error.clone(), decision });
        }
    }
    if let Some(f) = stream_failure {
        if stream_interrupted {
            return Err(StepError::Interrupted);
        }
        return Err(StepError::Defect(f.into()));
    }
    if !declines.is_empty() {
        if (input.location_closed)() {
            return Ok(Outcome::Completed { needs_continuation: true });
        }
        return Err(StepError::Interrupted);
    }
    if tools_interrupted {
        if let Some(d) = tool_defect {
            return Err(StepError::Defect(anyhow::anyhow!(d)));
        }
        return Err(StepError::Interrupted);
    }
    if let Some(f) = record.failure {
        return Err(StepError::Failed(StepFailed(f)));
    }
    if stream_interrupted {
        return Err(StepError::Interrupted);
    }
    Ok(Outcome::Completed { needs_continuation: !input.tool_choice_none && record.needs_continuation })
}


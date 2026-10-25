//! Session compaction (`core/session/compaction.ts`), with the token estimate (`core/util/token.ts`).
//!
//! A summary compaction sends the older part of the history plus the summary prompt to the model and stores
//! the reply as a checkpoint beside the newest `keep` tokens of conversation, referenced by their first
//! message (`metadata.keepID`) and sent as text:
//!
//! ```text
//!   first:  [E1 … E8][E9 E10]                  →  [S1 + "E9 E10"]
//!   again:  [S1 + "E9 E10"][E11 … E18][E19]    →  [S2 + "E19"]      S2 covers S1, E9 E10, and E11–E18
//! ```
//!
//! Native (provider-side) compaction is not available: `oc-ai` has no compact endpoint, so a model whose
//! `compaction` policy is `native` fails with `provider.unsupported-operation`, exactly as upstream does when
//! `LLMClient.canCompact` is false.

use std::sync::Arc;
use std::time::Duration;

use futures::StreamExt;
use oc_ai::catalog::ResolvedModel;
use oc_ai::error::FailureClassification;
use oc_ai::types::{
    ContentPart, GenerationOptions, Message as LlmMessage, Role, SystemPart, ToolContent as LlmToolContent, ToolEntry,
    ToolResultValue,
};
use oc_ai::{AIError, ErrorKind, LLMEvent, LLMRequest};
use oc_schema::event::SessionEvent;
use oc_schema::message::{CompactionReason, CompactionStatus};
use oc_schema::types::{FinishReason, Metadata, ModelRef, SessionError, TokenUsage};
use oc_schema::Message;
use parking_lot::Mutex;
use serde_json::{json, Value};
use tokio_util::sync::CancellationToken;

use crate::bus::Durable1;
use crate::history::is_native_compaction;
use crate::recent_text::{kept, message_to_text, truncate_tool_output};
use crate::runner::{Engine, Loaded};

// ---------------------------------------------------------------------------------------------
// Constants (verbatim)
// ---------------------------------------------------------------------------------------------

/// After each "too long" rejection, the next attempt aims at this share of the first rejected request's size.
pub const SHRINK_STEPS: [f64; 3] = [0.7, 0.5, 0.35];
/// The least of the window kept free for the last reply before compaction and for the summary itself.
pub const RESERVE_MIN: i64 = 16_000;
/// A common window size, assumed for the compaction request when the model's window is unknown.
pub const UNKNOWN_WINDOW: i64 = 200_000;
pub const IMAGE_TOKEN_ESTIMATE: u64 = 1_500;
pub const PDF_TOKEN_ESTIMATE: u64 = 2_000;
/// Default `keep` (tokens of recent conversation kept verbatim beside the summary).
pub const DEFAULT_KEEP: u64 = 15_000;

pub const SUMMARY_TEMPLATE: &str = "You MUST use this format for your response (you may omit sections that aren't applicable). Do not include the <template> tags in your response.
<template>
## Objective
- [one or two brief sentences describing what the user is trying to accomplish]

## Requirements
- [constraints, preferences, requirements, and scope boundaries stated by the user, or \"(none)\"]

## Decisions
- [decisions already made and why, or \"(none)\"]

## Work State
Break the objective into smaller goals and report which are completed, which are being worked on, and which are blocked.
### Completed
- [goals that have been completed; otherwise \"(none)\"]

### Active
- [goals currently being worked on; otherwise \"(none)\"]

### Blocked
- [anything blocking progress, and why; otherwise \"(none)\"]

## Next Move
1. [ordered list of next actions, or \"(none)\"]

## Relevant Files
List the files and directories, other than the current working directory, that another agent would need to open to continue this work. Include at most 15, most important first. Do not list every file that was read or changed. Include paths outside the current working directory when relevant. If none, write \"(none)\".
- `[file or directory path]`: [brief reason it matters]

## Important Context
- [facts the next agent cannot continue without and cannot easily find on its own; or \"(none)\"]
</template>";

pub const SUMMARY_RULES: &str = "Rules:
- Keep each section concise. Use terse, single-line bullets, not prose paragraphs or nested lists.
- Prefer short references over detailed restatement. It is fine to leave out information the next agent can recover from the code or the files listed above.
- Preserve exact file paths, symbols, commands, error strings, URLs, and identifiers.
- Carry forward only user questions or requests that remain unanswered or require further action. Do not repeat ones that newer history has answered or resolved. Preserve exact wording when carrying one forward.
- Preserve consequential workflow state, including whether changes are uncommitted, committed, pushed, under review, or merged.
- Do not mention the summary process or that context was compacted.";

pub const NUDGE: &str = "The previous response did not fill in the required summary template. Do not call tools. Return the summary as text using the exact section headings from the template.";

/// Summaries written with the previous template carry this catch-all heading.
pub const LEGACY_HEADING: &str = "## Additional Context";

/// `buildPrompt(update, legacy)`
pub fn build_prompt(update: bool, legacy: bool) -> String {
    let shared = [
        "Summarize only what the user and the assistant said and did. Leave out instructions and setup the assistant was given rather than told by the user: repository conventions, instruction files such as AGENTS.md, and environment details like the session ID. The next agent receives current versions of all of these separately.",
        SUMMARY_TEMPLATE,
        SUMMARY_RULES,
        "Do not continue the task or call tools.",
        "Return only the structured summary in the requested format. Do not include a preamble, explanation, or other commentary.",
    ];
    let mut parts: Vec<&str> = Vec::new();
    if !update {
        parts.push("You MUST summarize the conversation above into a structured summary that will be given to another agent to resume the work.");
        parts.extend(shared);
        return parts.join("\n\n");
    }
    parts.push("Update the existing checkpoint in the conversation above into one consolidated summary.");
    if legacy {
        parts.push("The existing checkpoint was written with an earlier format that recorded far more detail than this one asks for. Rewrite it at the level of detail described below rather than carrying its detail forward. Keep its requirements, decisions, and open questions; they came from earlier conversation with the user.");
    }
    parts.push("Newer history always takes precedence over the existing checkpoint. Preserve previous information unless newer history clearly contradicts, supersedes, resolves, or makes it stale. If something is no longer relevant to continuing the work, you may remove it.");
    parts.push("Incorporate newer requirements, decisions, progress, and context. Reconcile Work State and Next Move: move completed work out of Active, remove resolved blockers and answered questions, and preserve unresolved or pending work.");
    parts.push("Return only the updated Markdown sections. Do not reproduce the `<conversation-checkpoint>`, `<summary>`, or `<recent-context>` wrapper tags from the previous checkpoint.");
    parts.extend(shared);
    parts.join("\n\n")
}

/// Whether a reply fills the template: some line, trimmed, equals one of the template's heading lines
/// (every template line starting with `##`, which includes the `###` sub-headings).
pub fn filled(text: &str) -> bool {
    let headings: Vec<&str> = SUMMARY_TEMPLATE.split('\n').filter(|l| l.starts_with("##")).collect();
    text.split('\n').any(|line| headings.contains(&line.trim()))
}

// ---------------------------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq)]
pub enum Trigger {
    /// Before a step request; skipped unless due.
    Auto,
    /// The provider just rejected this context as too long.
    Overflow,
    /// `/compact`: `input_id` is the inbox item whose message shows the outcome (the runner already published
    /// `session.compaction.started` for it).
    Manual { input_id: String },
}

impl Trigger {
    fn reason(&self) -> CompactionReason {
        match self {
            Trigger::Manual { .. } => CompactionReason::Manual,
            _ => CompactionReason::Auto,
        }
    }
    fn input_id(&self) -> Option<String> {
        match self {
            Trigger::Manual { input_id } => Some(input_id.clone()),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum CompactOutcome {
    /// Only `auto`/`overflow` skip: the context fits, or automatic compaction is off.
    Skipped,
    Completed,
    Failed(SessionError),
}

/// Returned (as the `anyhow` error) when the cancellation token fires during a compaction. A non-manual
/// compaction has already published `session.compaction.failed{compaction.interrupted}`.
#[derive(Debug, Clone, thiserror::Error)]
#[error("Compaction was interrupted")]
pub struct Interrupted;

/// Compaction settings (`Settings`), folded from config.
#[derive(Debug, Clone, PartialEq)]
pub struct Settings {
    pub auto: bool,
    /// Tokens kept free below the model's limits before compacting. Unset keeps 10% free.
    pub buffer: Option<u64>,
    /// Tokens of recent conversation kept verbatim beside the summary.
    pub keep: u64,
}

impl Default for Settings {
    fn default() -> Self {
        Settings { auto: true, buffer: None, keep: DEFAULT_KEEP }
    }
}

impl Settings {
    pub fn from_entries(entries: &[oc_config::schema::Entry]) -> Settings {
        let c = oc_config::fold::compaction(entries);
        Settings { auto: c.auto.unwrap_or(true), buffer: c.buffer, keep: c.keep.unwrap_or(DEFAULT_KEEP) }
    }
}

// ---------------------------------------------------------------------------------------------
// Token estimates (`util/token.ts` + compaction.ts:925-963)
// ---------------------------------------------------------------------------------------------

fn js_len(s: &str) -> usize {
    s.encode_utf16().count()
}

/// `Token.estimate`: `max(0, round(length / 4))` with the UTF-16 length (JS `Math.round`: halves round up).
pub fn token_estimate(s: &str) -> u64 {
    ((js_len(s) + 2) / 4) as u64
}

fn json_encode(v: &Value) -> String {
    oc_ai::json::encode(v)
}

fn estimate_media(mime: &str) -> u64 {
    let t = mime.to_lowercase();
    if t.starts_with("image/") {
        IMAGE_TOKEN_ESTIMATE
    } else if t == "application/pdf" {
        PDF_TOKEN_ESTIMATE
    } else {
        0
    }
}

pub fn estimate_part(part: &ContentPart) -> u64 {
    match part {
        // An encrypted native compaction has no locally measurable size.
        ContentPart::Compaction(c) => token_estimate(c.text.clone().flatten().as_deref().unwrap_or("")),
        ContentPart::Effort(_) => 0,
        ContentPart::Text(t) => token_estimate(&t.text),
        ContentPart::Reasoning(r) => token_estimate(&r.text),
        ContentPart::Media(m) => estimate_media(&m.media.media_type()),
        ContentPart::ToolCall(c) => token_estimate(&format!("{}{}", c.name, json_encode(&c.input))),
        ContentPart::ToolResult(r) => match &r.result {
            ToolResultValue::Content(items) => items
                .iter()
                .map(|c| match c {
                    LlmToolContent::Text { text } => token_estimate(text),
                    LlmToolContent::File { mime, .. } => estimate_media(mime),
                })
                .sum(),
            ToolResultValue::Json(v) | ToolResultValue::Text(v) | ToolResultValue::Error(v) => match v {
                Value::String(s) => token_estimate(s),
                other => token_estimate(&json_encode(other)),
            },
        },
    }
}

pub fn estimate_message(message: &LlmMessage) -> u64 {
    message.content.iter().map(estimate_part).sum()
}

/// Only what providers receive; `metadata` and `native` stay local.
pub fn estimate_tool(tool: &ToolEntry) -> u64 {
    match tool {
        ToolEntry::Tool(t) => token_estimate(&format!(
            "{}{}{}",
            t.name,
            t.description,
            json_encode(&Value::Object(t.input_schema.clone()))
        )),
        ToolEntry::Namespace(n) => {
            token_estimate(&format!("{}{}", n.name, n.description.as_deref().unwrap_or("")))
                + n.tools.iter().map(estimate_tool).sum::<u64>()
        }
    }
}

/// `estimateRequest({system, tools, messages})`
pub fn estimate_request(system: &[SystemPart], tools: &[ToolEntry], messages: &[LlmMessage]) -> u64 {
    system.iter().map(|p| token_estimate(&p.text)).sum::<u64>()
        + tools.iter().map(estimate_tool).sum::<u64>()
        + messages.iter().map(estimate_message).sum::<u64>()
}

fn estimate_llm_request(r: &LLMRequest) -> u64 {
    estimate_request(&r.system, &r.tools, &r.messages)
}

/// `calculateCeiling(limit, buffer)`: the largest request the model takes while leaving room for its reply.
/// `None` is +∞ (unknown window: auto compaction never triggers).
pub fn calculate_ceiling(context: i64, input: Option<i64>, buffer: Option<u64>) -> Option<i64> {
    // Unknown limits are reported as 0. An unknown input limit falls back to the context window.
    let window = match input {
        Some(i) if i != 0 => i,
        _ => context,
    };
    if window <= 0 {
        return None;
    }
    if let Some(b) = buffer {
        return Some(window - b as i64);
    }
    Some(window - (window / 10).max(if window >= 2 * RESERVE_MIN { RESERVE_MIN } else { 0 }))
}

/// Another provider's count may describe a native window this model cannot replay, so only the current
/// provider's counts are trusted.
pub fn has_measured_prompt(message: &Message, model: &ModelRef) -> bool {
    let Message::Assistant(a) = message else { return false };
    a.model.provider_id == model.provider_id
        && a.error.is_none()
        && a.tokens.as_ref().is_some_and(|t| t.input + t.cache.read + t.cache.write > 0)
}

fn is_completed_compaction(message: &Message) -> bool {
    matches!(message, Message::Compaction(c) if matches!(c.status, CompactionStatus::Completed { .. }))
}

/// History loads from the latest completed compaction, so a previous one is always the first message.
pub fn previous_compaction(messages: &[Message]) -> Option<&Message> {
    messages.first().filter(|m| is_completed_compaction(m))
}

fn previous_summary(m: &Message) -> Option<&str> {
    match m {
        Message::Compaction(c) => match &c.status {
            CompactionStatus::Completed { summary, .. } => Some(summary.as_str()),
            _ => None,
        },
        _ => None,
    }
}

/// What `estimatePrompt` needs from a loaded context: history, the model, the base transcript system parts
/// (`agent.system || system.txt`, then the instruction baseline) and the tool definitions.
pub struct PromptView<'a> {
    pub messages: &'a [Message],
    pub model: ModelRef,
    pub provider_metadata_key: String,
    pub input_modalities: &'a [String],
    pub system: Vec<String>,
    pub tools: &'a [ToolEntry],
}

/// `estimatePrompt`: `measured` is what the provider reported at the latest response, `estimated` is the text since.
pub fn estimate_prompt_view(view: &PromptView) -> (u64, u64) {
    let anchor_index = view.messages.iter().rposition(|m| has_measured_prompt(m, &view.model));
    let anchor = anchor_index.map(|i| &view.messages[i]);
    let slice = &view.messages[anchor_index.unwrap_or(0)..];
    let mut sent = crate::lower::to_llm_messages(slice, &view.model, Some(&view.provider_metadata_key));
    crate::prompt::unsupported_parts(&mut sent, view.input_modalities, "");
    crate::prompt::bound_images(&mut sent);
    // The anchor's usage covers its own output, but not its local tool results, which the provider never saw.
    let anchor_id = anchor.map(|a| a.id().to_string());
    let unmeasured: Vec<LlmMessage> = sent
        .into_iter()
        .filter(|m| m.role != Role::Assistant || m.id.as_deref() != anchor_id.as_deref())
        .collect();
    let tokens = match anchor {
        Some(Message::Assistant(a)) => a.tokens.clone(),
        _ => None,
    };
    let Some(t) = tokens else {
        let system: Vec<SystemPart> = view.system.iter().map(SystemPart::text).collect();
        return (0, estimate_request(&system, view.tools, &unmeasured));
    };
    (
        t.input + t.cache.read + t.cache.write + t.output + t.reasoning,
        unmeasured.iter().map(estimate_message).sum(),
    )
}

// ---------------------------------------------------------------------------------------------
// Split + text rendering (compaction.ts:702-851)
// ---------------------------------------------------------------------------------------------

/// `oldestToDrop`: walk from the newest item accumulating sizes while `total + size <= budget`; returns the
/// index of the first kept item.
pub fn oldest_to_drop<T>(items: &[T], size: impl Fn(&T) -> u64, budget: i64) -> usize {
    let mut total: i64 = 0;
    let mut start = items.len();
    while start > 0 {
        let next = total.saturating_add(size(&items[start - 1]) as i64);
        if next > budget {
            break;
        }
        total = next;
        start -= 1;
    }
    start
}

fn serialize_llm_tool_content(content: &[LlmToolContent]) -> String {
    content
        .iter()
        .map(|item| match item {
            LlmToolContent::Text { text } => text.clone(),
            LlmToolContent::File { mime, name, .. } => match name {
                None => format!("[Attached {mime}]"),
                Some(n) => format!("[Attached {mime}: {n}]"),
            },
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// `splitConversation`: `older` (`messages[..older]`) gets summarized; the newest messages within `keep`
/// tokens, from `keep_id` on, are kept verbatim beside the summary.
#[derive(Debug, Clone, PartialEq)]
pub struct Split {
    pub older: usize,
    pub keep_id: Option<String>,
}

struct Entry {
    index: usize,
    text: String,
    user: bool,
}

/// `None` when there is nothing to compact.
pub fn split_conversation(messages: &[Message], keep: u64) -> Option<Split> {
    let entries: Vec<Entry> = messages
        .iter()
        .enumerate()
        .filter_map(|(index, m)| {
            let text = message_to_text(m);
            (!text.is_empty()).then(|| Entry { index, text, user: matches!(m, Message::User { .. }) })
        })
        .collect();
    if entries.is_empty() {
        return None;
    }
    let previous_recent = previous_compaction(messages).is_some_and(|p| !kept(p).is_empty());
    let first = entries.get(recent_start(&entries, keep, previous_recent));
    Some(Split {
        older: first.map(|e| e.index).unwrap_or(messages.len()),
        keep_id: first.map(|e| messages[e.index].id().to_string()),
    })
}

fn recent_start(entries: &[Entry], keep: u64, previous_recent: bool) -> usize {
    // Drop the oldest entries until the rest fit the allowance, but always keep the newest one.
    let dropped = oldest_to_drop(entries, |e| token_estimate(&e.text), keep as i64).min(entries.len() - 1);
    // Start at a user message so an assistant's tool calls and results stay together.
    let user_boundary = entries.iter().enumerate().rposition(|(i, e)| i <= dropped && e.user);
    if let Some(b) = user_boundary.filter(|b| *b > 0) {
        return b;
    }
    // Everything fits. Keep only the latest exchange so there is an older part left to summarize.
    if let Some(l) = entries.iter().rposition(|e| e.user).filter(|l| *l > 0) {
        return l;
    }
    // One exchange, nothing older. Summarize it all and keep nothing, unless a previous summary already
    // kept recent text, in which case keep everything and summarize only the summary before it.
    if previous_recent { 0 } else { entries.len() }
}

/// `flattenMessage`: one request message as a flattened transcript line. Empty for what the transcript leaves out.
pub fn flatten_message(message: &LlmMessage) -> String {
    if message.role == Role::System {
        return String::new();
    }
    message
        .content
        .iter()
        .filter_map(|part| match part {
            ContentPart::Text(t) => (!t.text.is_empty())
                .then(|| format!("[{}]: {}", if message.role == Role::User { "User" } else { "Assistant" }, t.text)),
            ContentPart::Media(m) => Some(format!("[{} omitted]", m.media.media_type())),
            ContentPart::ToolCall(c) => Some(format!("[Assistant tool call]: {}({})", c.name, json_encode(&c.input))),
            ContentPart::ToolResult(r) => {
                let output = match &r.result {
                    ToolResultValue::Content(items) => serialize_llm_tool_content(items),
                    ToolResultValue::Json(v) | ToolResultValue::Text(v) | ToolResultValue::Error(v) => match v {
                        Value::String(s) => s.clone(),
                        other => json_encode(other),
                    },
                };
                let label = if matches!(r.result, ToolResultValue::Error(_)) { "Tool error" } else { "Tool result" };
                Some(format!("[{label}]: {}", truncate_tool_output(&output)))
            }
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn note(omitted: usize) -> String {
    if omitted == 0 {
        String::new()
    } else {
        format!("[{omitted} older {} omitted]\n\n", if omitted == 1 { "exchange" } else { "exchanges" })
    }
}

/// `flattenAndDropOldest(request, context, target)`: history that has to change is sent as text:
/// `[previous compaction][transcript]`. Whole exchanges are dropped, oldest first, until the request fits
/// `target` (`None` = +∞). `None` when even the newest exchange does not fit.
pub fn flatten_and_drop_oldest(
    request: &LLMRequest,
    messages: &[Message],
    model: &ModelRef,
    target: Option<i64>,
) -> Option<LLMRequest> {
    let previous = previous_compaction(messages);
    let end = request
        .messages
        .iter()
        .rposition(|m| {
            (previous.is_some() && m.id.as_deref() == previous.map(Message::id))
                || m.content.iter().any(|p| matches!(p, ContentPart::Compaction(_)))
        })
        .map(|i| i + 1)
        .unwrap_or(0);
    let summary = previous.filter(|p| !is_native_compaction(p));
    // The previous summary is rebuilt from storage with its recent part removed.
    let lead: Vec<LlmMessage> = match summary {
        Some(Message::Compaction(c)) => {
            let mut c = c.clone();
            c.metadata.get_or_insert_with(Default::default).insert("recent".into(), Value::String(String::new()));
            crate::lower::to_llm_messages(&[Message::Compaction(c)], model, None)
        }
        _ => request.messages[..end].to_vec(),
    };
    let recent = summary.map(kept).unwrap_or("");
    // A user message and everything after it, up to the next one, is one exchange: kept or dropped whole.
    let mut exchanges: Vec<String> = if recent.is_empty() { vec![] } else { vec![recent.to_string()] };
    for message in &request.messages[end..] {
        let text = flatten_message(message);
        if text.is_empty() {
            continue;
        }
        if message.role == Role::User || exchanges.is_empty() {
            exchanges.push(text);
        } else {
            let last = exchanges.last_mut().unwrap();
            last.push('\n');
            last.push_str(&text);
        }
    }
    // Room for the longest possible note is set aside before choosing what to keep.
    let room = match target {
        None => i64::MAX,
        Some(t) => {
            t - estimate_request(&request.system, &request.tools, &lead) as i64
                - token_estimate(&note(exchanges.len())) as i64
        }
    };
    let start = oldest_to_drop(&exchanges, |t| token_estimate(t) + 1, room);
    let kept = &exchanges[start..];
    if kept.is_empty() {
        return None;
    }
    let mut out = request.clone();
    let mut msgs = lead;
    msgs.push(LlmMessage::user_text(format!("{}{}", note(exchanges.len() - kept.len()), kept.join("\n\n"))));
    out.messages = msgs;
    Some(out)
}

// ---------------------------------------------------------------------------------------------
// Engine integration
// ---------------------------------------------------------------------------------------------

fn costs(model: &ResolvedModel) -> Vec<crate::usage::Cost> {
    model
        .cost()
        .iter()
        .map(|c| crate::usage::Cost {
            tier_size: c.tier.as_ref().map(|t| t.size.max(0) as u64),
            input: c.input,
            output: c.output,
            cache_read: c.cache.read,
            cache_write: c.cache.write,
        })
        .collect()
}

fn metadata_key(model: &ResolvedModel) -> String {
    model
        .language_model
        .route
        .provider_metadata_key
        .clone()
        .unwrap_or_else(|| model.language_model.provider.clone())
}

/// `baseTranscript` system parts (no request hooks: no model-family prompt, no identity block).
fn base_system(loaded: &Loaded, tool_names: &[&str]) -> Vec<String> {
    let base = match loaded.sel.agent.system.as_deref() {
        Some(s) if !s.is_empty() => s.to_string(),
        _ => crate::prompt::render(crate::prompt::SYSTEM, tool_names),
    };
    [base, loaded.initial.clone()].into_iter().filter(|p| !p.is_empty()).collect()
}

/// `estimatePrompt` for a loaded context with the given tool definitions.
pub fn estimate_prompt(loaded: &Loaded, tools: &[ToolEntry]) -> (u64, u64) {
    let names: Vec<&str> = tools.iter().map(ToolEntry::name).collect();
    let model = &loaded.resolved;
    estimate_prompt_view(&PromptView {
        messages: &loaded.messages,
        model: model.model_ref(),
        provider_metadata_key: metadata_key(model),
        input_modalities: &model.capabilities().input,
        system: base_system(loaded, &names),
        tools,
    })
}

enum Fail {
    /// A reply or condition that cannot be used; never retried.
    Failure(SessionError),
    /// A provider error, handled by `deliver`.
    Ai(AIError),
    Defect(anyhow::Error),
}

impl From<anyhow::Error> for Fail {
    fn from(e: anyhow::Error) -> Self {
        Fail::Defect(e)
    }
}

fn failure(kind: &str, message: &str) -> Fail {
    Fail::Failure(SessionError::new(kind, message))
}

/// A summary fills `text`, and references the messages it keeps verbatim with `metadata.keepID`; history
/// rebuilds their text from that. A native compaction fills `native.messages` with its replacement window.
struct Res {
    text: String,
    native: Option<Metadata>,
    metadata: Option<Metadata>,
}

struct Streamed {
    text: String,
}

enum Mode {
    Summary { prompt: String },
    Native,
}

struct Run<'a> {
    engine: &'a Engine,
    loaded: &'a Loaded,
    /// The model the compaction runs on (the compaction agent's model, or the session's).
    model: &'a ResolvedModel,
    trigger: &'a Trigger,
    session_id: String,
    cancel: CancellationToken,
    /// Each model call is billed as soon as it finishes; the compaction's message shows the total.
    spent: Mutex<Option<(f64, TokenUsage)>>,
}

impl Run<'_> {
    fn spend(&self, cost: f64, tokens: TokenUsage) -> anyhow::Result<()> {
        {
            let mut g = self.spent.lock();
            match g.as_mut() {
                Some((c, t)) => {
                    *c += cost;
                    t.add(&tokens);
                }
                None => *g = Some((cost, tokens.clone())),
            }
        }
        self.engine.app.bus.commit_one(SessionEvent::UsageRecorded {
            session_id: self.session_id.clone(),
            source: "compaction".into(),
            cost,
            tokens,
        })?;
        Ok(())
    }

    /// The conversation as the runner would send it. The output limit leaves room for `budget`, the most
    /// `deliver` sends.
    fn prepare(&self, messages: &[Message], budget: i64) -> LLMRequest {
        let (mut req, names) = self.engine.request(self.loaded, false);
        let model = self.model;
        let names: Vec<&str> = names.iter().map(String::as_str).collect();
        req.model = model.language_model.clone();
        req.system = crate::prompt::system_parts(
            self.loaded.sel.agent.system.as_deref(),
            &names,
            &crate::prompt::ModelIdentity { id: &model.model.id, name: &model.model.name, provider_id: &model.model.provider_id },
            &self.loaded.initial,
        )
        .into_iter()
        .map(SystemPart::text)
        .collect();
        let key = metadata_key(model);
        let mut lowered = crate::lower::to_llm_messages(messages, &model.model_ref(), Some(&key));
        crate::prompt::unsupported_parts(&mut lowered, &model.capabilities().input, &model.model.provider_id);
        crate::prompt::bound_images(&mut lowered);
        req.messages = lowered;
        req.tool_choice = None;
        let max = crate::prompt::output_limit(
            model.output_limit().max(0) as u64,
            model.context_limit().max(0) as u64,
            true,
            Some((budget.max(0) as u64, 0)),
        );
        req.generation = Some(GenerationOptions { max_tokens: Some(max as f64), ..Default::default() });
        req
    }

    async fn stream(&self, request: LLMRequest) -> Result<Streamed, Fail> {
        // `session.compaction` may shape the request or provide the summary (`result`).
        let mut request = request;
        let shaped = crate::plugin_request::shape(
            &self.engine.app.plugins,
            &self.loaded.sel.location.key,
            crate::plugin_request::Kind::Compaction,
            &self.session_id,
            &self.loaded.sel.agent.id,
            &self.model.model_ref(),
            &mut request,
            None,
        )
        .await
        .map_err(|e| Fail::Failure(SessionError::new("unknown", e.to_string())))?;
        if let Some(text) = shaped.result {
            return Ok(Streamed { text });
        }
        let costs = costs(self.model);
        let mut streamed = Streamed { text: String::new() };
        let mut stream = self.engine.client.stream_with(request, self.cancel.clone(), shaped.http).await.map_err(Fail::Ai)?;
        while let Some(event) = stream.next().await {
            match event.map_err(Fail::Ai)? {
                LLMEvent::ProviderError { message, classification, .. } => {
                    let classification = match classification.as_deref() {
                        None => return Err(failure("provider.error", &message)),
                        Some("context-overflow") => Some(FailureClassification::ContextOverflow),
                        Some("payload-too-large") => Some(FailureClassification::PayloadTooLarge),
                        Some(_) => None,
                    };
                    return Err(Fail::Ai(AIError::new(ErrorKind::InvalidRequest { parameter: None, classification }, message)));
                }
                LLMEvent::TextDelta { text, .. } => {
                    self.engine.app.bus.publish_session_ephemeral(
                        &self.session_id,
                        "session.compaction.delta",
                        json!({"sessionID": self.session_id, "text": text}),
                    );
                    streamed.text.push_str(&text);
                }
                LLMEvent::StepFinish { usage, .. } => {
                    let tokens = crate::publisher::tokens(usage.as_ref());
                    let cost = crate::usage::calculate_cost(&costs, &tokens);
                    self.spend(cost, tokens)?;
                }
                LLMEvent::Finish { reason, .. } => match reason.normalized {
                    FinishReason::Unknown => {
                        return Err(Fail::Ai(AIError::new(
                            ErrorKind::InvalidProviderOutput { classification: Some("incomplete-stream".into()), route: None },
                            "The provider response ended with an unknown finish reason.",
                        )));
                    }
                    FinishReason::Error => {
                        return Err(Fail::Ai(AIError::unknown_provider("Compaction generation failed")));
                    }
                    FinishReason::Length => {
                        return Err(failure("compaction.failed", "Compaction summary reached the output token limit"));
                    }
                    FinishReason::ContentFilter => {
                        return Err(Fail::Failure(crate::publisher::content_filter_error(
                            "Compaction summary was blocked by the provider",
                            reason.category.as_deref(),
                            reason.explanation.as_deref(),
                        )));
                    }
                    _ => {}
                },
                _ => {}
            }
        }
        Ok(streamed)
    }

    async fn send(&self, mode: &Mode, request: LLMRequest) -> Result<Res, Fail> {
        match mode {
            Mode::Native => Err(failure(
                "provider.unsupported-operation",
                &format!("Native compaction is not supported for {}/{}", request.model.provider, request.model.route.id),
            )),
            Mode::Summary { prompt } => {
                // A reply that ignores the template gets one reminder before it counts as a failure.
                let mut prompted = request;
                prompted.messages.push(LlmMessage::user_text(prompt.clone()));
                let reply = self.stream(prompted.clone()).await?;
                if filled(&reply.text) {
                    return Ok(Res { text: reply.text, native: None, metadata: None });
                }
                let mut nudged = prompted;
                nudged.messages.push(LlmMessage::user_text(NUDGE));
                let retry = self.stream(nudged).await?;
                if filled(&retry.text) {
                    return Ok(Res { text: retry.text, native: None, metadata: None });
                }
                Err(failure(
                    "compaction.failed",
                    if retry.text.trim().is_empty() {
                        "Compaction produced no summary"
                    } else {
                        "Compaction summary did not match the required template"
                    },
                ))
            }
        }
    }

    /// Sends the request as-is if it is estimated to fit `target`, and as text otherwise. A "too long"
    /// rejection shrinks the next attempts to 70%, 50%, then 35% of the first rejected request's estimate. A
    /// "payload too large" rejection of the unchanged request resends it as text without counting. Other
    /// provider errors resend under the session's retry policy.
    async fn deliver(&self, prepared: &LLMRequest, mode: Mode, mut target: i64) -> Result<Res, Fail> {
        // The runner opened the manual compaction's message when it delivered the `/compact` item.
        if !matches!(self.trigger, Trigger::Manual { .. }) {
            self.engine.app.bus.commit_one(SessionEvent::CompactionStarted {
                session_id: self.session_id.clone(),
                reason: CompactionReason::Auto,
                input_id: None,
            })?;
        }
        let messages = &self.loaded.messages;
        let model_ref = self.model.model_ref();
        let mut policy = crate::retry::Policy::default();
        let mut rejections = 0usize;
        let mut rejected: Option<u64> = None;
        let mut as_text = false;
        loop {
            let fits = !as_text && estimate_llm_request(prepared) as i64 <= target;
            let (request, unchanged) = if fits {
                (Some(prepared.clone()), true)
            } else {
                match flatten_and_drop_oldest(prepared, messages, &model_ref, Some(target)) {
                    Some(r) => (Some(r), false),
                    None if rejections > 0 => (None, false),
                    None if as_text => (flatten_and_drop_oldest(prepared, messages, &model_ref, None), false),
                    None => (Some(prepared.clone()), true),
                }
            };
            let Some(request) = request else {
                return Err(failure(
                    "compaction.failed",
                    "The compaction request cannot be reduced further without losing the latest exchange or checkpoint",
                ));
            };
            let estimate = estimate_llm_request(&request);
            let cause = match self.send(&mode, request).await {
                Ok(r) => return Ok(r),
                Err(Fail::Ai(cause)) => cause,
                Err(other) => return Err(other),
            };
            let error = crate::errors::from_ai(&cause);
            let too_large = cause.classification() == Some(FailureClassification::PayloadTooLarge);
            if too_large && unchanged {
                as_text = true;
                continue;
            }
            if too_large || cause.is_context_overflow() {
                let Some(step) = SHRINK_STEPS.get(rejections) else { return Err(Fail::Failure(error)) };
                let first = *rejected.get_or_insert(estimate);
                target = (first as f64 * step).floor() as i64;
                rejections += 1;
                continue;
            }
            let Some(decision) = policy.decide(&cause, cause.is_retryable()) else { return Err(Fail::Failure(error)) };
            tokio::time::sleep(Duration::from_millis(decision.delay)).await;
        }
    }

    async fn summarize(&self, budget: i64, keep: u64) -> Result<Res, Fail> {
        let messages = &self.loaded.messages;
        let Some(split) = split_conversation(messages, keep) else {
            return Err(failure("compaction.unavailable", "Nothing to compact yet"));
        };
        let previous = previous_compaction(messages).and_then(previous_summary);
        let prompt = build_prompt(previous.is_some(), previous.is_some_and(|s| s.contains(LEGACY_HEADING)));
        let prepared = self.prepare(&messages[..split.older], budget);
        let overhead = (token_estimate(&prompt) + token_estimate(NUDGE)) as i64;
        let mut result = self.deliver(&prepared, Mode::Summary { prompt }, budget - overhead).await?;
        let Some(keep_id) = split.keep_id else { return Ok(result) };
        // A compaction hook may choose its own kept range.
        let mut metadata = Metadata::new();
        metadata.insert("keepID".into(), Value::String(keep_id));
        metadata.extend(result.metadata.take().unwrap_or_default());
        result.metadata = Some(metadata);
        Ok(result)
    }

    async fn compact_natively(&self, budget: i64) -> Result<Res, Fail> {
        let messages = &self.loaded.messages;
        if !messages.iter().any(|m| !message_to_text(m).is_empty()) {
            return Err(failure("compaction.unavailable", "Nothing to compact yet"));
        }
        let prepared = self.prepare(messages, budget);
        self.deliver(&prepared, Mode::Native, budget).await
    }

    fn publish(&self, outcome: Result<Res, SessionError>) -> anyhow::Result<CompactOutcome> {
        let (cost, tokens) = match self.spent.lock().clone() {
            Some((c, t)) => (Some(c), Some(t)),
            None => (None, None),
        };
        match outcome {
            Err(error) => {
                self.engine.app.bus.commit_one(SessionEvent::CompactionFailed {
                    session_id: self.session_id.clone(),
                    reason: self.trigger.reason(),
                    error: error.clone(),
                    input_id: self.trigger.input_id(),
                    cost,
                    tokens,
                })?;
                Ok(CompactOutcome::Failed(error))
            }
            Ok(res) => {
                self.engine.app.bus.commit(vec![Durable1 {
                    event: SessionEvent::CompactionEnded {
                        session_id: self.session_id.clone(),
                        reason: self.trigger.reason(),
                        model: Some(self.model.model_ref()),
                        native: res.native,
                        text: res.text,
                        cost,
                        tokens,
                    },
                    metadata: res.metadata,
                    id: None,
                }])?;
                Ok(CompactOutcome::Completed)
            }
        }
    }
}

impl Engine {
    /// `estimatePrompt(context)` with the session's tool definitions: `(measured, estimated)`.
    pub fn estimate_prompt(&self, loaded: &Loaded) -> (u64, u64) {
        let (req, _) = self.request(loaded, false);
        estimate_prompt(loaded, &req.tools)
    }

    /// `estimateContext = measured + estimated`
    pub fn estimate_context(&self, loaded: &Loaded) -> u64 {
        let (m, e) = self.estimate_prompt(loaded);
        m + e
    }

    /// `due(context, ceiling)`: false right after a completed compaction, or while the latest measured
    /// response predates the latest native checkpoint; else whether the context estimate reaches the ceiling.
    pub fn compaction_due(&self, loaded: &Loaded, ceiling: Option<i64>) -> bool {
        let messages = &loaded.messages;
        if messages.last().is_some_and(is_completed_compaction) {
            return false;
        }
        let model = loaded.resolved.model_ref();
        let measured = messages.iter().rposition(|m| has_measured_prompt(m, &model)).map(|i| i as i64).unwrap_or(-1);
        let checkpoint = messages.iter().rposition(is_native_compaction).map(|i| i as i64).unwrap_or(-1);
        if measured < checkpoint {
            return false;
        }
        match ceiling {
            None => false,
            Some(c) => self.estimate_context(loaded) as i64 >= c,
        }
    }

    async fn compaction_model(&self, loaded: &Loaded) -> Option<ResolvedModel> {
        let agent = loaded.sel.agents.items.get("compaction")?;
        let m = agent.model.as_ref()?;
        let catalog = self.models.catalog_at(&loaded.sel.location.entries, Some(&loaded.sel.location.key)).await.ok()?;
        let cred = self.models.credential(Some(&loaded.sel.location.key), &catalog, &m.provider_id).await;
        self.models.resolve_model(Some(&loaded.sel.location.key), &catalog, &m.provider_id, &m.id, m.variant.as_deref(), cred.as_ref()).await.ok()
    }

    /// `SessionCompaction.compact(trigger)`.
    pub async fn compact(self: &Arc<Self>, loaded: &Loaded, trigger: Trigger, cancel: CancellationToken) -> anyhow::Result<CompactOutcome> {
        let settings = Settings::from_entries(&loaded.sel.location.entries);
        let session_model = &loaded.resolved;
        // Only the user compacts when automatic compaction is off, overflow included.
        if !matches!(trigger, Trigger::Manual { .. }) && !settings.auto {
            return Ok(CompactOutcome::Skipped);
        }
        if trigger == Trigger::Auto {
            let limit = session_model.limit();
            if !self.compaction_due(loaded, calculate_ceiling(limit.context, limit.input, settings.buffer)) {
                return Ok(CompactOutcome::Skipped);
            }
        }
        let native = session_model.compaction.as_deref() == Some("native");
        let agent_model = if native { None } else { self.compaction_model(loaded).await };
        let model = agent_model.as_ref().unwrap_or(session_model);
        let limit = model.limit();
        // An unknown window never triggers auto compaction, but the compaction request still needs a size to aim for.
        let cap = calculate_ceiling(limit.context, limit.input, settings.buffer)
            .or_else(|| calculate_ceiling(UNKNOWN_WINDOW, limit.input, settings.buffer))
            .unwrap_or(i64::MAX);
        // The provider just rejected this context, so the estimate ran low; the first attempt already aims below it.
        let budget = if trigger == Trigger::Overflow {
            cap.min((self.estimate_context(loaded) as f64 * SHRINK_STEPS[0]).floor() as i64)
        } else {
            cap
        };

        let run = Run {
            engine: self,
            loaded,
            model,
            trigger: &trigger,
            session_id: loaded.sel.session.id.clone(),
            cancel: cancel.clone(),
            spent: Mutex::new(None),
        };
        let work = async {
            if native { run.compact_natively(budget).await } else { run.summarize(budget, settings.keep).await }
        };
        let result = tokio::select! {
            biased;
            _ = cancel.cancelled() => None,
            r = work => Some(r),
        };
        let Some(result) = result else {
            // A manual compaction settles through its `/compact` inbox item, which the runner owns.
            if !matches!(trigger, Trigger::Manual { .. }) {
                run.publish(Err(SessionError::new("compaction.interrupted", "Compaction was interrupted")))?;
            }
            return Err(Interrupted.into());
        };
        match result {
            Ok(res) => run.publish(Ok(res)),
            Err(Fail::Failure(error)) => run.publish(Err(error)),
            Err(Fail::Ai(cause)) => run.publish(Err(crate::errors::from_ai(&cause))),
            Err(Fail::Defect(e)) => Err(e),
        }
    }
}

/// `recentUserMessages`: the newest whole, real user messages within `keep` tokens (no skills, no synthetic
/// guidance), lowered for the model. Used by native trigger-mechanism compaction.
pub fn recent_user_messages(messages: &[Message], model: &ModelRef, input_modalities: &[String], keep: u64) -> Vec<LlmMessage> {
    let users: Vec<Message> = messages
        .iter()
        .filter_map(|m| match m {
            Message::User { .. } => {
                let mut m = m.clone();
                if let Message::User { skills, .. } = &mut m {
                    *skills = None;
                }
                Some(m)
            }
            _ => None,
        })
        .collect();
    let mut sendable = crate::lower::to_llm_messages(&users, model, None);
    crate::prompt::unsupported_parts(&mut sendable, input_modalities, "");
    crate::prompt::bound_images(&mut sendable);
    let start = oldest_to_drop(&sendable, estimate_message, keep as i64);
    sendable.split_off(start)
}

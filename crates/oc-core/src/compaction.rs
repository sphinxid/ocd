//! Session compaction (`core/session/compaction.ts`), with the token estimate (`core/util/token.ts`).
//!
//! A summary compaction sends the whole conversation plus the summary prompt to the model, so the request
//! reuses the prompt cache of the normal one, and stores the reply as a checkpoint beside the newest `keep`
//! tokens of conversation, referenced by their first message (`metadata.keepID`) and sent as text:
//!
//! ```text
//!   first:  [E1 … E8][E9 E10]                  →  [S1 + "E9 E10"]
//!   again:  [S1 + "E9 E10"][E11 … E18][E19]    →  [S2 + "E19"]      S2 covers the whole current conversation
//! ```
//!
//! Background summaries (`experimental.background_compaction`): once the context passes 85% of the ceiling, the
//! prefix the provider just processed is summarized off the critical path and held until automatic compaction is
//! due, when it is swapped in instead of summarizing from scratch ([`Engine::precompute`]).
//!
//! Native (provider-side) compaction is not available: `oc-ai` has no compact endpoint, so a model whose
//! `compaction` policy is `native` fails with `provider.unsupported-operation`, exactly as upstream does when
//! `LLMClient.canCompact` is false.

use std::sync::Arc;
use std::sync::atomic::{AtomicU8, Ordering};
use std::time::Duration;

use futures::StreamExt;
use oc_ai::catalog::ResolvedModel;
use oc_ai::error::FailureClassification;
use oc_ai::types::{
    ContentPart, GenerationOptions, Message as LlmMessage, Role, SystemPart, ToolContent as LlmToolContent, ToolEntry,
    ToolResultValue,
};
use oc_ai::route::transport::HttpHook;
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
use crate::recent_text::{kept, message_to_text};
use crate::runner::{Engine, Loaded};

// ---------------------------------------------------------------------------------------------
// Constants (verbatim)
// ---------------------------------------------------------------------------------------------

/// After each "too long" rejection, the next attempt aims at this share of the first rejected request's size.
pub const SHRINK_STEPS: [f64; 4] = [0.85, 0.7, 0.5, 0.35];
/// The least of the window kept free for the last reply before compaction and for the summary itself.
pub const RESERVE_MIN: i64 = 16_000;
/// Share of the window kept free before automatic compaction. Above `RESERVE_MIN`, the margin between it and
/// `summary_room` absorbs a large step past the trigger, so the whole conversation still goes out unchanged and
/// reuses the cache.
pub const RESERVE_SHARE: f64 = 0.15;
/// A common window size, assumed for the compaction request when the model's window is unknown.
pub const UNKNOWN_WINDOW: i64 = 200_000;
/// A summary gets at least this much output, or the room left if less, even when the estimate margin leaves less.
pub const SUMMARY_OUTPUT_FLOOR: u64 = 32_000;
/// How far one exchange of a summary transcript is shortened, in the order the levels apply: whole; tool outputs
/// cut; tool outputs left out and tool inputs cut; only the user's text, cut; omitted.
pub const OUTPUTS_CLIPPED: usize = 1;
pub const OUTPUTS_REMOVED: usize = 2;
pub const USER_ONLY: usize = 3;
pub const OMITTED: usize = 4;
pub const TRANSCRIPT_LEVELS: [usize; 5] = [0, OUTPUTS_CLIPPED, OUTPUTS_REMOVED, USER_ONLY, OMITTED];
/// Characters kept, from the start and end, of a cut tool input, tool output, or user message.
pub const TRANSCRIPT_CLIP: usize = 2_000;
pub const IMAGE_TOKEN_ESTIMATE: u64 = 1_500;
pub const PDF_TOKEN_ESTIMATE: u64 = 2_000;
/// Default `keep` (tokens of recent conversation kept verbatim beside the summary).
pub const DEFAULT_KEEP: u64 = 15_000;
/// Share of the compaction ceiling at which a background summary starts.
pub const PRECOMPUTE_RATIO: f64 = 0.85;
/// Measured prompt tokens below which a background summary is not worth its cost.
pub const PRECOMPUTE_MIN_TOKENS: u64 = 64_000;
/// The most text a swapped-in summary keeps verbatim beside it: twice `keep`, at least 30k, at most a quarter of the
/// budget. A longer tail would leave the context near the threshold, so compaction summarizes from scratch instead.
pub const PRECOMPUTED_TAIL_MIN: i64 = 30_000;
pub const PRECOMPUTED_TAIL_SHARE: f64 = 0.25;
/// Failed background summaries after which no new one starts until the next compaction.
pub const PRECOMPUTE_MAX_FAILURES: u32 = 2;

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
    /// Tokens kept free below the model's limits before compacting. Unset keeps 15%, and at least 16k, free.
    pub buffer: Option<u64>,
    /// Tokens of recent conversation kept verbatim beside the summary.
    pub keep: u64,
    /// Background summaries (`experimental.background_compaction`).
    pub precompute: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Settings { auto: true, buffer: None, keep: DEFAULT_KEEP, precompute: false }
    }
}

impl Settings {
    pub fn from_entries(entries: &[oc_config::schema::Entry]) -> Settings {
        let c = oc_config::fold::compaction(entries);
        Settings {
            auto: c.auto.unwrap_or(true),
            buffer: c.buffer,
            keep: c.keep.unwrap_or(DEFAULT_KEEP),
            // `Config.latest(entries, "experimental")?.background_compaction === true`: the latest entry that has an
            // `experimental` object decides.
            precompute: oc_config::fold::experimental(entries).and_then(|e| e.background_compaction) == Some(true),
        }
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

fn window(context: i64, input: Option<i64>) -> i64 {
    // Unknown limits are reported as 0. An unknown input limit falls back to the context window.
    match input {
        Some(i) if i != 0 => i,
        _ => context,
    }
}

/// `calculateCeiling(limit, buffer)`: the largest request the model takes while leaving room for its reply:
/// `RESERVE_SHARE` of the window, or `RESERVE_MIN` when that is more. A window too small to give up
/// `RESERVE_MIN` keeps `RESERVE_SHARE`. `None` is +∞ (unknown window: auto compaction never triggers).
pub fn calculate_ceiling(context: i64, input: Option<i64>, buffer: Option<u64>) -> Option<i64> {
    let window = window(context, input);
    if window <= 0 {
        return None;
    }
    // A buffer that leaves no room, such as one set for a larger model, is ignored.
    if let Some(b) = buffer
        && (b as i64) < window
    {
        return Some(window - b as i64);
    }
    let share = (window as f64 * RESERVE_SHARE).floor() as i64;
    Some(window - share.max(if window >= 2 * RESERVE_MIN { RESERVE_MIN } else { 0 }))
}

/// `summaryRoom`: the largest summary request that leaves `RESERVE_MIN` of the window for the summary, or less when
/// the request cannot generate that much (`output`, its output limit). Zero for an unknown window.
pub fn summary_room(context: i64, input: Option<i64>, output: Option<f64>) -> i64 {
    let output = output.unwrap_or(RESERVE_MIN as f64);
    (window(context, input) - (RESERVE_MIN as f64).min(output) as i64).max(0)
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
// Split + text rendering
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

/// `splitConversation`: the newest messages within `keep` tokens, from `keep_id` on, are kept verbatim beside
/// the summary.
#[derive(Debug, Clone, PartialEq)]
pub struct Split {
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
    Some(Split { keep_id: first.map(|e| messages[e.index].id().to_string()) })
}

fn recent_start(entries: &[Entry], keep: u64, previous_recent: bool) -> usize {
    // Drop the oldest entries until the rest fit the allowance, but always keep the newest one.
    let dropped = oldest_to_drop(entries, |e| token_estimate(&e.text), keep as i64).min(entries.len() - 1);
    // Start at a user message so an assistant's tool calls and results stay together.
    let user_boundary = entries.iter().enumerate().rposition(|(i, e)| i <= dropped && e.user);
    if let Some(b) = user_boundary.filter(|b| *b > 0) {
        return b;
    }
    // Everything fits. Keep only the latest exchange rather than duplicating the whole conversation beside the summary.
    if let Some(l) = entries.iter().rposition(|e| e.user).filter(|l| *l > 0) {
        return l;
    }
    // One exchange, nothing older. Summarize it all and keep nothing, unless a previous summary already
    // kept recent text, in which case keep everything and summarize only the summary before it.
    if previous_recent { 0 } else { entries.len() }
}

/// `withoutTail`: the request without the retained tail, which starts at the message with ID `from`.
pub fn without_tail(request: &LLMRequest, from: Option<&str>) -> LLMRequest {
    let index = from.and_then(|from| request.messages.iter().position(|m| m.id.as_deref() == Some(from)));
    let mut out = request.clone();
    if let Some(index) = index {
        out.messages.truncate(index);
    }
    out
}

/// `clip`: `value` cut to `TRANSCRIPT_CLIP` characters from its start and end, never splitting a surrogate pair.
pub fn clip(value: &str) -> String {
    if js_len(value) <= TRANSCRIPT_CLIP {
        return value.to_string();
    }
    let chars: Vec<char> = value.chars().collect();
    if chars.len() <= TRANSCRIPT_CLIP {
        return value.to_string();
    }
    let half = TRANSCRIPT_CLIP / 2;
    let omitted = chars.len() - TRANSCRIPT_CLIP;
    let head: String = chars[..half].iter().collect();
    let tail: String = chars[chars.len() - half..].iter().collect();
    format!("{head}\n[… {omitted} characters omitted …]\n{tail}")
}

/// `flattenMessage(message, level)`: one request message as transcript lines at a shortening level. Empty for
/// what that level leaves out.
pub fn flatten_message(message: &LlmMessage, level: usize) -> String {
    if message.role == Role::System || level == OMITTED {
        return String::new();
    }
    if level >= USER_ONLY && message.role != Role::User {
        return String::new();
    }
    message
        .content
        .iter()
        .filter_map(|part| match part {
            ContentPart::Text(t) => {
                if t.text.is_empty() {
                    return None;
                }
                let text = if level >= USER_ONLY { clip(&t.text) } else { t.text.clone() };
                Some(format!("[{}]: {text}", if message.role == Role::User { "User" } else { "Assistant" }))
            }
            _ if level >= USER_ONLY => None,
            ContentPart::Media(m) => Some(format!("[{} omitted]", m.media.media_type())),
            ContentPart::ToolCall(c) => {
                let input = json_encode(&c.input);
                Some(format!("[Assistant tool call]: {}({})", c.name, if level >= OUTPUTS_REMOVED { clip(&input) } else { input }))
            }
            ContentPart::ToolResult(r) => {
                let label = if matches!(r.result, ToolResultValue::Error(_)) { "Tool error" } else { "Tool result" };
                if level >= OUTPUTS_REMOVED {
                    return Some(format!("[{label} omitted]"));
                }
                let output = match &r.result {
                    ToolResultValue::Content(items) => serialize_llm_tool_content(items),
                    ToolResultValue::Json(v) | ToolResultValue::Text(v) | ToolResultValue::Error(v) => match v {
                        Value::String(s) => s.clone(),
                        other => json_encode(other),
                    },
                };
                Some(format!("[{label}]: {}", if level >= OUTPUTS_CLIPPED { clip(&output) } else { output }))
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
/// `[previous compaction][transcript]`, the conversation since that compaction as one text message. A previous
/// summary always stays in front, while its verbatim recent part becomes the oldest transcript entry; a previous
/// native window stays exactly as the provider made it. The transcript has no tool definitions and leaves out
/// reasoning, media, and instruction updates.
///
/// When the whole transcript does not fit `target` (`None` = +∞), exchanges are shortened oldest first, one level
/// at a time (see [`flatten_message`]): every exchange reaches a level before any reaches the next, and whole
/// exchanges are omitted only as a last resort. The newest exchange is never omitted; `None` when the transcript
/// cannot fit even so.
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

    // A user message and everything after it, up to the next one, is one exchange. Each is rendered at every
    // level once. The previous summary's recent part is already shortened text: whole until it is omitted.
    let mut exchanges: Vec<[String; 5]> = Vec::new();
    if !recent.is_empty() {
        exchanges.push(TRANSCRIPT_LEVELS.map(|level| if level == OMITTED { String::new() } else { recent.to_string() }));
    }
    let mut groups: Vec<Vec<&LlmMessage>> = Vec::new();
    for message in &request.messages[end..] {
        match groups.last_mut() {
            Some(group) if message.role != Role::User => group.push(message),
            _ => groups.push(vec![message]),
        }
    }
    for group in groups {
        let renders = TRANSCRIPT_LEVELS.map(|level| {
            group.iter().map(|m| flatten_message(m, level)).filter(|t| !t.is_empty()).collect::<Vec<_>>().join("\n")
        });
        if !renders[0].is_empty() {
            exchanges.push(renders);
        }
    }
    if exchanges.is_empty() {
        return None;
    }
    let size = |text: &str| -> i64 { if text.is_empty() { 0 } else { token_estimate(text) as i64 + 1 } };
    // Room for the longest possible note is set aside before choosing what to keep.
    let room = target.map(|t| {
        t - estimate_request(&request.system, &[], &lead) as i64 - token_estimate(&note(exchanges.len())) as i64
    });
    let over = |total: i64| room.is_some_and(|room| total > room);

    let mut levels = vec![0usize; exchanges.len()];
    let mut total: i64 = exchanges.iter().map(|r| size(&r[0])).sum();
    let newest = exchanges.len() - 1;
    for &level in &TRANSCRIPT_LEVELS[1..] {
        let mut index = 0;
        while index < exchanges.len() && over(total) {
            // The newest exchange is what the summary most needs; it is shortened but never left out.
            if !(index == newest && exchanges[index][level].is_empty()) {
                total += size(&exchanges[index][level]) - size(&exchanges[index][levels[index]]);
                levels[index] = level;
            }
            index += 1;
        }
    }
    if over(total) {
        return None;
    }
    // Includes exchanges with no user text, which are already gone at `USER_ONLY`.
    let omitted = levels.iter().enumerate().filter(|(i, level)| exchanges[*i][**level].is_empty()).count();
    let text = exchanges
        .iter()
        .zip(&levels)
        .map(|(renders, level)| renders[*level].as_str())
        .filter(|t| !t.is_empty())
        .collect::<Vec<_>>()
        .join("\n\n");
    let mut out = request.clone();
    out.tools = vec![];
    let mut msgs = lead;
    msgs.push(LlmMessage::user_text(format!("{}{text}", note(omitted))));
    out.messages = msgs;
    Some(out)
}

/// `fitOutput`: `prepare` sizes the output limit for a request of `budget` tokens, but `deliver` can send a
/// larger one when it cannot shrink the conversation further. Lower the limit to the room the request being sent
/// leaves in the window. The limit leaves the same margin for a low estimate that primary requests do, but a
/// summary still gets up to `SUMMARY_OUTPUT_FLOOR` of the room left without the margin. `size` is the request's
/// measured size (`(measured, estimated)`), when known; else its character estimate.
pub fn fit_output(mut request: LLMRequest, limit_output: u64, limit_context: u64, size: Option<(u64, u64)>) -> LLMRequest {
    let Some(current) = request.generation.as_ref().and_then(|g| g.max_tokens) else { return request };
    let input = size.unwrap_or_else(|| (0, estimate_llm_request(&request)));
    let margin = crate::prompt::output_limit(limit_output, limit_context, Some(input), 0.0);
    let floor = SUMMARY_OUTPUT_FLOOR.min(crate::prompt::output_limit(limit_output, limit_context, Some((input.0 + input.1, 0)), 0.0));
    let max_tokens = current.min(margin.max(floor) as f64);
    if max_tokens == current {
        return request;
    }
    if let Some(g) = request.generation.as_mut() {
        g.max_tokens = Some(max_tokens);
    }
    request
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

/// A summary run off the critical path: it covers only `messages`, streams no deltas, and bills into `usage`.
struct Background {
    messages: Vec<Message>,
    usage: Arc<Mutex<Vec<(f64, TokenUsage)>>>,
}

/// A background summary of everything before `keep_id`, held until automatic compaction is due.
pub struct Precomputed {
    first_id: Option<String>,
    keep_id: String,
    model: ModelRef,
    initial: String,
    /// The longest tail since `keep_id` that compaction still accepts beside this summary.
    tail_limit: i64,
    /// Background summaries that failed since the last compaction, this one included once it has.
    failures: u32,
    usage: Arc<Mutex<Vec<(f64, TokenUsage)>>>,
    /// 0 running, 1 succeeded, 2 failed.
    state: Arc<AtomicU8>,
    task: tokio::task::JoinHandle<Result<Res, Fail>>,
}

impl Precomputed {
    /// Stops the background work (`Fiber.interrupt`).
    pub fn cancel(&self) {
        self.task.abort();
    }
}

/// Aborts a held summary that is dropped while waited for (an interrupted compaction).
struct AbortOnDrop(tokio::task::JoinHandle<Result<Res, Fail>>);

impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        self.0.abort();
    }
}

/// `heldIndex`: where the messages kept beside a held summary start, or `None` when the summary no longer describes
/// this conversation: a compaction or revert since replaced its start, or the model or instructions changed.
fn held_index(held: &Precomputed, messages: &[Message], initial: &str, model: &ModelRef) -> Option<usize> {
    if held.first_id.as_deref() != messages.first().map(Message::id) || held.initial != initial {
        return None;
    }
    if held.model.provider_id != model.provider_id || held.model.id != model.id || held.model.variant != model.variant {
        return None;
    }
    messages.iter().position(|m| m.id() == held.keep_id)
}

/// `tailTokens`: estimated text a swapped-in summary keeps verbatim, every message from `index` on.
fn tail_tokens(messages: &[Message], index: usize) -> i64 {
    let text: Vec<String> = messages[index..].iter().map(message_to_text).filter(|t| !t.is_empty()).collect();
    token_estimate(&text.join("\n\n")) as i64
}

/// `tailLimit(keep, budget)`
fn tail_limit(keep: u64, budget: i64) -> i64 {
    (keep as i64 * 2).max(PRECOMPUTED_TAIL_MIN).min((budget as f64 * PRECOMPUTED_TAIL_SHARE).floor() as i64)
}

/// The conversation as the runner would send it, after request hooks (`Prepared`).
struct Prepared {
    request: LLMRequest,
    /// The `session.http.*` middleware.
    http: Option<Arc<dyn HttpHook>>,
    /// A compaction hook supplied the summary itself, so no model call happens.
    result: Option<String>,
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
    /// A background summary (`precompute`).
    background: Option<Background>,
}

impl Run<'_> {
    /// The messages the summary covers.
    fn messages(&self) -> &[Message] {
        self.background.as_ref().map(|b| b.messages.as_slice()).unwrap_or(&self.loaded.messages)
    }

    fn add_spent(&self, cost: f64, tokens: &TokenUsage) {
        let mut g = self.spent.lock();
        match g.as_mut() {
            Some((c, t)) => {
                *c += cost;
                t.add(tokens);
            }
            None => *g = Some((cost, tokens.clone())),
        }
    }

    fn spend(&self, cost: f64, tokens: TokenUsage) -> anyhow::Result<()> {
        match &self.background {
            // A background summary keeps its own total until a compaction takes it.
            Some(background) => background.usage.lock().push((cost, tokens.clone())),
            None => self.add_spent(cost, &tokens),
        }
        self.engine.app.bus.commit_one(SessionEvent::UsageRecorded {
            session_id: self.session_id.clone(),
            source: "compaction".into(),
            cost,
            tokens,
        })?;
        Ok(())
    }

    /// The conversation as the runner would send it, after request hooks. The output limit leaves room for
    /// `budget`, the most `deliver` sends; the request prepared here can be larger when the conversation overshot
    /// the threshold, and is only shrunk to fit after hooks have seen it.
    async fn prepare(&self, messages: &[Message], budget: i64) -> Result<Prepared, Fail> {
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
            Some((budget.max(0) as u64, 0)),
            0.0,
        );
        req.generation = Some(GenerationOptions { max_tokens: Some(max as f64), ..Default::default() });
        // `session.compaction` may shape the request or provide the summary (`result`).
        let shaped = crate::plugin_request::shape(
            &self.engine.app.plugins,
            &self.loaded.sel.location.key,
            crate::plugin_request::Kind::Compaction,
            &self.session_id,
            &self.loaded.sel.agent.id,
            &self.model.model_ref(),
            &mut req,
            None,
        )
        .await
        .map_err(|e| Fail::Failure(SessionError::new("unknown", e.to_string())))?;
        Ok(Prepared { request: req, http: shaped.http, result: shaped.result })
    }

    fn fit_output(&self, request: LLMRequest, size: Option<(u64, u64)>) -> LLMRequest {
        fit_output(request, self.model.output_limit().max(0) as u64, self.model.context_limit().max(0) as u64, size)
    }

    /// `measuredPrompt`: the prompt size, only while the model, variant and agent that were measured still match;
    /// they shape the request.
    fn measured_prompt(&self, messages: &[Message]) -> Option<(u64, u64)> {
        let model = self.model.model_ref();
        let Message::Assistant(anchor) = messages.iter().rev().find(|m| has_measured_prompt(m, &model))? else { return None };
        if anchor.model.id != model.id || anchor.model.variant != model.variant || anchor.agent != self.loaded.sel.agent.id {
            return None;
        }
        Some(estimate_prompt_view(&PromptView {
            messages,
            model,
            provider_metadata_key: metadata_key(self.model),
            input_modalities: &self.model.capabilities().input,
            // Unused: there is a measured response.
            system: vec![],
            tools: &[],
        }))
    }

    async fn stream(&self, request: LLMRequest, http: Option<Arc<dyn HttpHook>>) -> Result<Streamed, Fail> {
        let costs = costs(self.model);
        let mut streamed = Streamed { text: String::new() };
        let mut stream = self.engine.client.stream_with(request, self.cancel.clone(), http).await.map_err(Fail::Ai)?;
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
                LLMEvent::TextDelta { text, .. } if self.background.is_some() => streamed.text.push_str(&text),
                LLMEvent::TextDelta { text, .. } => {
                    self.engine.app.bus.publish_session_ephemeral(
                        &self.session_id,
                        "session.compaction.delta",
                        json!({"sessionID": self.session_id, "text": text}),
                    );
                    streamed.text.push_str(&text);
                }
                LLMEvent::StepFinish { usage, .. } => {
                    let (cost, tokens) = crate::usage::record(usage.as_ref(), &costs);
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

    async fn send(&self, mode: &Mode, prepared: &Prepared, request: LLMRequest, size: Option<(u64, u64)>) -> Result<Res, Fail> {
        match mode {
            Mode::Native => Err(failure(
                "provider.unsupported-operation",
                &format!("Native compaction is not supported for {}/{}", request.model.provider, request.model.route.id),
            )),
            Mode::Summary { prompt } => {
                // Hooks saw the request without the summary prompt, so it is appended here. A reply that ignores
                // the template gets one reminder before it counts as a failure.
                let mut prompted = request;
                prompted.messages.push(LlmMessage::user_text(prompt.clone()));
                let prompted = self.fit_output(prompted, size.map(|(m, e)| (m, e + token_estimate(prompt))));
                let reply = self.stream(prompted.clone(), prepared.http.clone()).await?;
                if filled(&reply.text) {
                    return Ok(Res { text: reply.text, native: None, metadata: None });
                }
                let mut nudged = prompted;
                nudged.messages.push(LlmMessage::user_text(NUDGE));
                let nudged = self.fit_output(nudged, size.map(|(m, e)| (m, e + token_estimate(prompt) + token_estimate(NUDGE))));
                let retry = self.stream(nudged, prepared.http.clone()).await?;
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

    /// Sends the request as-is if it is estimated to fit `fit`, else `history` as-is if it fits `target`, and
    /// otherwise `history` as text aimed at `target` (see [`flatten_and_drop_oldest`]). Until the provider
    /// rejects something as too long, a request that cannot be made to fit is sent whole. A "too long" rejection
    /// means the estimate ran low, so the next attempts aim at 85%, 70%, 50%, then 35% of the first rejected
    /// request's estimate, each shortening the transcript again from scratch, and a rejection after those gives
    /// up. A "payload too large" rejection of an unchanged request resends it as text without counting; one on
    /// text counts as "too long". Other provider errors resend under the session's retry policy. `measured`, when
    /// given, sizes the unchanged request until the provider rejects it; after that it is only sent as text.
    /// `history_is_prepared`: `history` is the prepared request itself (no tail to leave out).
    #[allow(clippy::too_many_arguments)]
    async fn deliver(
        &self,
        prepared: &Prepared,
        mode: Mode,
        mut target: i64,
        fit: i64,
        history: &LLMRequest,
        measured_size: Option<(u64, u64)>,
        history_is_prepared: bool,
    ) -> Result<Res, Fail> {
        if let Some(text) = &prepared.result {
            return Ok(Res { text: text.clone(), native: None, metadata: None });
        }
        let messages = &self.loaded.messages;
        let model_ref = self.model.model_ref();
        let mut policy = crate::retry::Policy::default();
        let mut rejections = 0usize;
        let mut rejected: Option<u64> = None;
        let mut as_text = false;
        let mut fit = fit;
        loop {
            let measured = if rejections == 0 { measured_size } else { None };
            // `(request, unchanged, is the prepared request)`
            let unchanged = if as_text {
                None
            } else if measured.map(|(m, e)| m + e).unwrap_or_else(|| estimate_llm_request(&prepared.request)) as i64 <= fit {
                Some((prepared.request.clone(), true))
            } else if estimate_llm_request(history) as i64 <= target {
                Some((history.clone(), history_is_prepared))
            } else {
                None
            };
            let (request, unchanged, is_prepared) = match unchanged {
                Some((r, p)) => (Some(r), true, p),
                None => match flatten_and_drop_oldest(history, messages, &model_ref, Some(target)) {
                    Some(r) => (Some(r), false, false),
                    None if rejections > 0 => (None, false, false),
                    None if as_text => (flatten_and_drop_oldest(history, messages, &model_ref, None), false, false),
                    None => (Some(history.clone()), true, history_is_prepared),
                },
            };
            let Some(request) = request else {
                return Err(failure(
                    "compaction.failed",
                    "The compaction request cannot be reduced further without losing the latest exchange or checkpoint",
                ));
            };
            let estimate = estimate_llm_request(&request);
            let size = if is_prepared { measured } else { None };
            let cause = match self.send(&mode, prepared, request, size).await {
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
                let first = *rejected.get_or_insert(size.map(|(m, e)| m + e).unwrap_or(estimate));
                // The characters can still say the rejected request fits, so it only goes out again as text.
                if size.is_some() {
                    as_text = true;
                }
                target = (first as f64 * step).floor() as i64;
                fit = target;
                rejections += 1;
                continue;
            }
            let Some(decision) = policy.decide(&cause, cause.is_retryable()) else { return Err(Fail::Failure(error)) };
            tokio::time::sleep(Duration::from_millis(decision.delay)).await;
        }
    }

    /// ```text
    ///   first:  [E1 … E8][E9 E10]                  →  [S1 + "E9 E10"]
    ///   again:  [S1 + "E9 E10"][E11 … E18][E19]    →  [S2 + "E19"]      S2 covers the whole current conversation
    /// ```
    ///
    /// The model is sent the whole conversation, including the tail kept beside the summary, then the summary
    /// prompt, so the summary reuses the normal request's prompt cache.
    async fn summarize(&self, budget: i64, keep: u64) -> Result<Res, Fail> {
        let messages = self.messages();
        let Some(split) = split_conversation(messages, keep) else {
            return Err(failure("compaction.unavailable", "Nothing to compact yet"));
        };
        // A background summary covers all of its messages; what follows them is kept when it is swapped in.
        let keep_id = if self.background.is_some() { None } else { split.keep_id };
        let previous = previous_compaction(messages).and_then(previous_summary);
        let prompt = build_prompt(previous.is_some(), previous.is_some_and(|s| s.contains(LEGACY_HEADING)));
        let prepared = self.prepare(messages, budget).await?;
        // Size the unchanged request with the number that decided to compact: the last response's count plus an
        // estimate of what came after it. Characters overstate a conversation whose old reasoning the provider drops.
        // A hook can still make this request larger than that count; the provider then rejects it and it is resent as
        // text. An overflow already proved the count low, so it keeps the character estimate.
        let measured = if *self.trigger == Trigger::Overflow { None } else { self.measured_prompt(messages) };
        let overhead = (token_estimate(&prompt) + token_estimate(NUDGE)) as i64;
        // Automatic compaction starts past the ceiling, so the whole conversation is sent unchanged while the window
        // can still fit the summary. Below that, the history before the tail is sent instead: unchanged when it
        // fits, and as text otherwise.
        let limit = self.model.limit();
        let output = prepared.request.generation.as_ref().and_then(|g| g.max_tokens);
        let fit = if *self.trigger == Trigger::Overflow { budget } else { budget.max(summary_room(limit.context, limit.input, output)) };
        let history = without_tail(&prepared.request, keep_id.as_deref());
        let history_is_prepared = !keep_id.as_deref().is_some_and(|k| prepared.request.messages.iter().any(|m| m.id.as_deref() == Some(k)));
        let mut result = self
            .deliver(&prepared, Mode::Summary { prompt }, budget - overhead, fit - overhead, &history, measured, history_is_prepared)
            .await?;
        let Some(keep_id) = keep_id else { return Ok(result) };
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
        let prepared = self.prepare(messages, budget).await?;
        let history = prepared.request.clone();
        self.deliver(&prepared, Mode::Native, budget, budget, &history, None, true).await
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
        self.compaction_due_with(loaded, ceiling, None)
    }

    /// [`Engine::compaction_due`] with the context's estimate when it is already known.
    pub fn compaction_due_with(&self, loaded: &Loaded, ceiling: Option<i64>, estimate: Option<(u64, u64)>) -> bool {
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
            Some(c) => estimate.map(|(m, e)| m + e).unwrap_or_else(|| self.estimate_context(loaded)) as i64 >= c,
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
        let held = self.precomputed.lock().remove(&loaded.sel.session.id);
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
            background: None,
        };
        // The runner opened the manual compaction's message when it delivered the `/compact` item. An automatic
        // one opens here, before any wait.
        if !matches!(trigger, Trigger::Manual { .. }) {
            self.app.bus.commit_one(SessionEvent::CompactionStarted {
                session_id: run.session_id.clone(),
                reason: CompactionReason::Auto,
                input_id: None,
            })?;
        }
        let work = async {
            // A summary still being written shows as a running compaction while it is waited for.
            if let Some(held) = held
                && let Some(res) = use_precomputed(&run, held, budget, &settings).await
            {
                return Ok(res);
            }
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

/// `usePrecomputed`: the held summary plus everything since it, kept verbatim, when it still describes this
/// conversation and that tail is short. Waits for a summary still being written. `None`, after stopping the
/// background work, when it cannot be used; compaction then summarizes as usual.
async fn use_precomputed(run: &Run<'_>, held: Precomputed, budget: i64, settings: &Settings) -> Option<Res> {
    let messages = &run.loaded.messages;
    let index = held_index(&held, messages, &run.loaded.initial, &run.model.model_ref());
    let usable = settings.precompute
        && *run.trigger != Trigger::Overflow
        && run.model.compaction.as_deref() != Some("native")
        && index.is_some_and(|i| tail_tokens(messages, i) <= tail_limit(settings.keep, budget));
    let Precomputed { keep_id, usage, task, .. } = held;
    let mut task = AbortOnDrop(task);
    if !usable {
        return None;
    }
    let mut res = match (&mut task.0).await {
        Ok(Ok(res)) => res,
        _ => return None,
    };
    // Background calls were billed as they finished; the compaction's message still shows what it cost.
    for (cost, tokens) in usage.lock().iter() {
        run.add_spent(*cost, tokens);
    }
    let mut metadata = Metadata::new();
    metadata.insert("keepID".into(), Value::String(keep_id));
    metadata.extend(res.metadata.take().unwrap_or_default());
    res.metadata = Some(metadata);
    Some(res)
}

impl Engine {
    /// `precompute`: once the context passes `PRECOMPUTE_RATIO` of the ceiling, summarizes everything before the
    /// latest response in the background. That is the prefix the provider just processed, so the summary request
    /// reads it from the prompt cache. The summary is held, not applied: swapping it in now would rewrite the prefix
    /// and throw away the cache for every step until compaction is actually due. One summary is held per session
    /// until compaction takes it, or until it fails or falls too far behind, when a newer response replaces it.
    /// `fresh`: load the session's latest history into the context first (the step before going idle).
    pub async fn precompute(self: &Arc<Self>, loaded: &Loaded, fresh: bool) -> anyhow::Result<()> {
        let settings = Settings::from_entries(&loaded.sel.location.entries);
        if !settings.precompute || !settings.auto {
            return Ok(());
        }
        let reloaded;
        let loaded = if fresh {
            let messages = crate::history::load(&self.app.db, &loaded.sel.session.id, crate::history::Boundary::Latest)?;
            reloaded = Loaded { messages, ..loaded.clone() };
            &reloaded
        } else {
            loaded
        };
        if loaded.resolved.compaction.as_deref() == Some("native") {
            return Ok(());
        }
        let limit = loaded.resolved.limit();
        let Some(ceiling) = calculate_ceiling(limit.context, limit.input, settings.buffer) else { return Ok(()) };
        let estimate = self.estimate_prompt(loaded);
        if self.compaction_due_with(loaded, Some(ceiling), Some(estimate)) {
            return Ok(());
        }
        if ((estimate.0 + estimate.1) as f64) < ceiling as f64 * PRECOMPUTE_RATIO {
            return Ok(());
        }
        let model_ref = loaded.resolved.model_ref();
        let messages = &loaded.messages;
        let Some(anchor_index) = messages.iter().rposition(|m| has_measured_prompt(m, &model_ref)).filter(|i| *i > 0) else {
            return Ok(());
        };
        let Message::Assistant(anchor) = &messages[anchor_index] else { return Ok(()) };
        let Some(tokens) = &anchor.tokens else { return Ok(()) };
        if tokens.input + tokens.cache.read + tokens.cache.write < PRECOMPUTE_MIN_TOKENS {
            return Ok(());
        }
        let session_id = loaded.sel.session.id.clone();
        let existing = self.precomputed.lock().remove(&session_id);
        let failed = existing.as_ref().is_some_and(|e| e.state.load(Ordering::SeqCst) == 2);
        let failures = existing.as_ref().map(|e| e.failures).unwrap_or(0) + failed as u32;
        if let Some(existing) = existing {
            let index = held_index(&existing, messages, &loaded.initial, &model_ref);
            let keep = match index {
                Some(i) if !failed && tail_tokens(messages, i) <= existing.tail_limit => true,
                // Summarizing up to the same response again would repeat the same request.
                Some(_) => existing.keep_id == anchor.id || failures >= PRECOMPUTE_MAX_FAILURES,
                None => false,
            };
            if keep {
                self.precomputed.lock().insert(session_id, existing);
                return Ok(());
            }
            existing.cancel();
        }
        // After a model switch nothing is cached for the new model yet; wait until its first response has written it.
        if anchor.model.id != model_ref.id || anchor.model.variant != model_ref.variant {
            return Ok(());
        }
        let model = self.compaction_model(loaded).await.unwrap_or_else(|| loaded.resolved.clone());
        let model_limit = model.limit();
        let cap = calculate_ceiling(model_limit.context, model_limit.input, settings.buffer)
            .or_else(|| calculate_ceiling(UNKNOWN_WINDOW, model_limit.input, settings.buffer))
            .unwrap_or(i64::MAX);
        let usage = Arc::new(Mutex::new(Vec::new()));
        let state = Arc::new(AtomicU8::new(0));
        let background = Background { messages: messages[..anchor_index].to_vec(), usage: usage.clone() };
        let (engine, context, keep, done) = (self.clone(), loaded.clone(), settings.keep, state.clone());
        let task = tokio::spawn(async move {
            let trigger = Trigger::Auto;
            let run = Run {
                engine: &engine,
                loaded: &context,
                model: &model,
                trigger: &trigger,
                session_id: context.sel.session.id.clone(),
                cancel: CancellationToken::new(),
                spent: Mutex::new(None),
                background: Some(background),
            };
            let result = run.summarize(cap, keep).await;
            if let Err(Fail::Defect(error)) = &result {
                tracing::warn!(%error, "background compaction failed");
            }
            done.store(if result.is_ok() { 1 } else { 2 }, Ordering::SeqCst);
            result
        });
        self.precomputed.lock().insert(
            session_id,
            Precomputed {
                first_id: messages.first().map(|m| m.id().to_string()),
                keep_id: anchor.id.clone(),
                model: model_ref,
                initial: loaded.initial.clone(),
                tail_limit: tail_limit(settings.keep, cap),
                failures,
                usage,
                state,
                task,
            },
        );
        Ok(())
    }

    /// Stops the background summaries of the sessions `matches` selects (their location closed).
    pub fn cancel_precomputed(&self, matches: impl Fn(&str) -> bool) {
        self.precomputed.lock().retain(|session, held| {
            if matches(session) {
                held.cancel();
                false
            } else {
                true
            }
        });
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

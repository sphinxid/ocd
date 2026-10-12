//! Compaction (`core/session/compaction.ts`): algorithm unit tests plus an end-to-end run against an
//! OpenAI-compatible mock provider.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use axum::{body::Body, response::Response, routing::post, Router};
use oc_ai::types::{ContentPart, Message as LlmMessage, ToolDefinition, ToolEntry, ToolNamespace};
use oc_core::compaction::*;
use oc_core::{app::App, inbox::Scope, runner::Engine, session::{CreateInput, PromptInput}};
use oc_schema::event::SessionEvent;
use oc_schema::message::{CompactionReason, CompactionStatus};
use oc_schema::types::{LocationRef, ModelRef};
use oc_schema::Message;
use serde_json::json;
use tokio_util::sync::CancellationToken;

fn msg(v: serde_json::Value) -> Message {
    serde_json::from_value(v).unwrap()
}

fn user(id: &str, text: &str) -> Message {
    msg(json!({"type": "user", "id": id, "time": {"created": 1}, "text": text}))
}

fn assistant(id: &str, text: &str) -> Message {
    msg(json!({
        "type": "assistant", "id": id, "time": {"created": 1}, "agent": "build",
        "model": {"id": "m1", "providerID": "mock"},
        "content": [{"type": "text", "text": text}]
    }))
}

fn compaction(id: &str, summary: &str, recent: &str) -> Message {
    msg(json!({
        "type": "compaction", "id": id, "time": {"created": 1},
        "status": "completed", "reason": "auto", "summary": summary, "recent": recent
    }))
}

fn model_ref() -> ModelRef {
    ModelRef { id: "m1".into(), provider_id: "mock".into(), variant: None }
}

// ------------------------------------------------------------------------------------------
// Token estimate, ceiling
// ------------------------------------------------------------------------------------------

#[test]
fn token_estimate_rounds_like_js() {
    assert_eq!(token_estimate(""), 0);
    assert_eq!(token_estimate("a"), 0);
    assert_eq!(token_estimate("ab"), 1); // 0.5 rounds up
    assert_eq!(token_estimate("abc"), 1);
    assert_eq!(token_estimate("abcdef"), 2); // 1.5 rounds up
    assert_eq!(token_estimate("abcdefghi"), 2);
    // UTF-16 length: one astral char is 2 units.
    assert_eq!(token_estimate("😀"), 1);
    assert_eq!(token_estimate("é"), 0);
}

#[test]
fn ceiling() {
    assert_eq!(calculate_ceiling(0, None, None), None);
    assert_eq!(calculate_ceiling(0, Some(0), Some(10)), None);
    // 10% beats the 16k reserve
    assert_eq!(calculate_ceiling(200_000, None, None), Some(180_000));
    // 16k reserve beats 10%
    assert_eq!(calculate_ceiling(100_000, None, None), Some(84_000));
    // a window too small to give up 16k keeps 10%
    assert_eq!(calculate_ceiling(20_000, None, None), Some(18_000));
    assert_eq!(calculate_ceiling(31_999, None, None), Some(31_999 - 3_199));
    assert_eq!(calculate_ceiling(32_000, None, None), Some(16_000));
    // input limit wins over context; 0 input falls back to context
    assert_eq!(calculate_ceiling(400_000, Some(272_000), None), Some(272_000 - 27_200));
    assert_eq!(calculate_ceiling(100_000, Some(0), None), Some(84_000));
    // explicit buffer
    assert_eq!(calculate_ceiling(100_000, None, Some(5_000)), Some(95_000));
    assert_eq!(calculate_ceiling(100_000, None, Some(150_000)), Some(-50_000));
}

#[test]
fn estimates() {
    let image = ContentPart::Media(oc_ai::types::MediaPart {
        media: oc_ai::MediaAsset::base64("aaaa".to_string(), "IMAGE/PNG".to_string()),
        filename: None,
        cache: None,
        metadata: None,
        provider_metadata: None,
    });
    assert_eq!(estimate_part(&image), 1_500);
    let pdf = ContentPart::Media(oc_ai::types::MediaPart {
        media: oc_ai::MediaAsset::base64("aaaa".to_string(), "application/pdf".to_string()),
        filename: None,
        cache: None,
        metadata: None,
        provider_metadata: None,
    });
    assert_eq!(estimate_part(&pdf), 2_000);
    assert_eq!(estimate_part(&ContentPart::text("12345678")), 2);
    let call = ContentPart::ToolCall(oc_ai::types::ToolCallPart {
        id: "c".into(),
        name: "read".into(),
        namespace: None,
        input: json!({"path": "/a"}),
        provider_executed: None,
        cache: None,
        metadata: None,
        provider_metadata: None,
    });
    // "read" + {"path":"/a"} = 4 + 12 = 16 chars
    assert_eq!(estimate_part(&call), 4);
    let effort = LlmMessage::effort(Some("high".into()), None);
    assert_eq!(estimate_message(&effort), 0);

    let tool = ToolEntry::Tool(ToolDefinition::new("ab", "cd", json!({})));
    // "abcd{}" = 6 chars
    assert_eq!(estimate_tool(&tool), 2);
    let ns = ToolEntry::Namespace(ToolNamespace { name: "ns".into(), description: None, tools: vec![tool.clone(), tool] });
    assert_eq!(estimate_tool(&ns), 1 + 2 + 2);
    let sys = vec![oc_ai::types::SystemPart::text("12345678")];
    assert_eq!(estimate_request(&sys, &[], &[LlmMessage::user_text("1234")]), 3);
}

#[test]
fn estimate_prompt_anchors_on_measured_response() {
    let mut a1 = assistant("a1", "first answer");
    if let Message::Assistant(a) = &mut a1 {
        a.tokens = Some(oc_schema::types::TokenUsage {
            input: 100,
            output: 5,
            reasoning: 2,
            cache: oc_schema::types::CacheUsage { read: 10, write: 1 },
        });
    }
    let messages = vec![user("u1", "first question"), a1, user("u2", "hello world!")];
    let view = PromptView {
        messages: &messages,
        model: model_ref(),
        provider_metadata_key: "mock".into(),
        input_modalities: &[],
        system: vec!["system prompt".into()],
        tools: &[],
    };
    // measured = input + cache.read + cache.write + output + reasoning; estimated = text since (anchor excluded)
    assert_eq!(estimate_prompt_view(&view), (118, 3));

    // Another provider's count is not trusted: estimate everything.
    let other = ModelRef { id: "m1".into(), provider_id: "other".into(), variant: None };
    let view = PromptView { model: other, ..view };
    let expected = token_estimate("system prompt")
        + token_estimate("first question")
        + token_estimate("first answer")
        + token_estimate("hello world!");
    assert_eq!(estimate_prompt_view(&view), (0, expected));
}

// ------------------------------------------------------------------------------------------
// Prompt text
// ------------------------------------------------------------------------------------------

const EXPECTED_TEMPLATE: &str = r#"You MUST use this format for your response (you may omit sections that aren't applicable). Do not include the <template> tags in your response.
<template>
## Objective
- [one or two brief sentences describing what the user is trying to accomplish]

## Requirements
- [constraints, preferences, requirements, and scope boundaries stated by the user, or "(none)"]

## Decisions
- [decisions already made and why, or "(none)"]

## Work State
Break the objective into smaller goals and report which are completed, which are being worked on, and which are blocked.
### Completed
- [goals that have been completed; otherwise "(none)"]

### Active
- [goals currently being worked on; otherwise "(none)"]

### Blocked
- [anything blocking progress, and why; otherwise "(none)"]

## Next Move
1. [ordered list of next actions, or "(none)"]

## Relevant Files
List the files and directories, other than the current working directory, that another agent would need to open to continue this work. Include at most 15, most important first. Do not list every file that was read or changed. Include paths outside the current working directory when relevant. If none, write "(none)".
- `[file or directory path]`: [brief reason it matters]

## Important Context
- [facts the next agent cannot continue without and cannot easily find on its own; or "(none)"]
</template>"#;

const EXPECTED_RULES: &str = "Rules:
- Keep each section concise. Use terse, single-line bullets, not prose paragraphs or nested lists.
- Prefer short references over detailed restatement. It is fine to leave out information the next agent can recover from the code or the files listed above.
- Preserve exact file paths, symbols, commands, error strings, URLs, and identifiers.
- Carry forward only user questions or requests that remain unanswered or require further action. Do not repeat ones that newer history has answered or resolved. Preserve exact wording when carrying one forward.
- Preserve consequential workflow state, including whether changes are uncommitted, committed, pushed, under review, or merged.
- Do not mention the summary process or that context was compacted.";

fn shared() -> String {
    [
        "Summarize only what the user and the assistant said and did. Leave out instructions and setup the assistant was given rather than told by the user: repository conventions, instruction files such as AGENTS.md, and environment details like the session ID. The next agent receives current versions of all of these separately.",
        EXPECTED_TEMPLATE,
        EXPECTED_RULES,
        "Do not continue the task or call tools.",
        "Return only the structured summary in the requested format. Do not include a preamble, explanation, or other commentary.",
    ]
    .join("\n\n")
}

const UPDATE_TAIL: [&str; 3] = [
    "Newer history always takes precedence over the existing checkpoint. Preserve previous information unless newer history clearly contradicts, supersedes, resolves, or makes it stale. If something is no longer relevant to continuing the work, you may remove it.",
    "Incorporate newer requirements, decisions, progress, and context. Reconcile Work State and Next Move: move completed work out of Active, remove resolved blockers and answered questions, and preserve unresolved or pending work.",
    "Return only the updated Markdown sections. Do not reproduce the `<conversation-checkpoint>`, `<summary>`, or `<recent-context>` wrapper tags from the previous checkpoint.",
];

#[test]
fn prompt_verbatim() {
    assert_eq!(SUMMARY_TEMPLATE, EXPECTED_TEMPLATE);
    assert_eq!(SUMMARY_RULES, EXPECTED_RULES);
    assert_eq!(
        NUDGE,
        "The previous response did not fill in the required summary template. Do not call tools. Return the summary as text using the exact section headings from the template."
    );
    assert_eq!(
        build_prompt(false, false),
        format!(
            "You MUST summarize the conversation above into a structured summary that will be given to another agent to resume the work.\n\n{}",
            shared()
        )
    );
    // `legacy` only matters for updates
    assert_eq!(build_prompt(false, true), build_prompt(false, false));
    assert_eq!(
        build_prompt(true, false),
        format!(
            "Update the existing checkpoint in the conversation above into one consolidated summary.\n\n{}\n\n{}",
            UPDATE_TAIL.join("\n\n"),
            shared()
        )
    );
    assert_eq!(
        build_prompt(true, true),
        format!(
            "Update the existing checkpoint in the conversation above into one consolidated summary.\n\nThe existing checkpoint was written with an earlier format that recorded far more detail than this one asks for. Rewrite it at the level of detail described below rather than carrying its detail forward. Keep its requirements, decisions, and open questions; they came from earlier conversation with the user.\n\n{}\n\n{}",
            UPDATE_TAIL.join("\n\n"),
            shared()
        )
    );
}

#[test]
fn filled_template_detection() {
    assert!(filled("## Objective\n- do it"));
    assert!(filled("intro\n  ## Next Move  \n"));
    // `###` sub-headings start with `##` too
    assert!(filled("### Active"));
    assert!(!filled("Objective: do it"));
    assert!(!filled("## Objective: x"));
    assert!(!filled(""));
}

// ------------------------------------------------------------------------------------------
// messageToText, split
// ------------------------------------------------------------------------------------------

#[test]
fn message_text_rendering() {
    let u = msg(json!({
        "type": "user", "id": "u", "time": {"created": 1}, "text": "look",
        "skills": [{"id": "s1", "name": "Skill One", "text": "skill body"}, {"id": "s2", "name": "No Text"}],
        "files": [
            {"data": "", "mime": "image/png", "source": {"type": "inline"}, "name": "pic.png"},
            {"data": "", "mime": "text/plain", "source": {"type": "uri", "uri": "file:///a.txt"}},
            {"data": "", "mime": "image/gif", "source": {"type": "inline"}}
        ]
    }));
    assert_eq!(
        message_to_text(&u),
        "[Skill activated: Skill One]\nskill body\n[User]: look\n[Attached image/png: pic.png]\n[Attached text/plain: file:///a.txt]\n[Attached image/gif: inline attachment]"
    );

    let a = msg(json!({
        "type": "assistant", "id": "a", "time": {"created": 1}, "agent": "build",
        "model": {"id": "m1", "providerID": "mock"},
        "content": [
            {"type": "reasoning", "text": ""},
            {"type": "reasoning", "text": "thinking"},
            {"type": "text", "text": "hi"},
            {"type": "tool", "id": "t1", "name": "read", "time": {"created": 1},
             "state": {"status": "completed", "input": {"path": "/a"}, "content": [
                {"type": "text", "text": "line"}, {"type": "file", "uri": "data:x", "mime": "image/png", "name": "p"},
                {"type": "file", "uri": "data:y", "mime": "image/jpeg"}]}},
            {"type": "tool", "id": "t2", "name": "shell", "time": {"created": 1},
             "state": {"status": "error", "input": {"command": "ls"}, "error": {"type": "unknown", "message": "boom"}}},
            {"type": "tool", "id": "t3", "name": "edit", "time": {"created": 1},
             "state": {"status": "streaming", "input": "{\"partial"}}
        ]
    }));
    assert_eq!(
        message_to_text(&a),
        "[Assistant reasoning]: thinking\n[Assistant]: hi\n[Assistant tool call]: read({\"path\":\"/a\"})\n[Tool result]: line\n[Attached image/png: p]\n[Attached image/jpeg]\n[Assistant tool call]: shell({\"command\":\"ls\"})\n[Tool error]: boom\n[Assistant tool call]: edit({\"partial)"
    );

    let loc = msg(json!({"type": "location-switched", "id": "l", "time": {"created": 1}, "location": {"directory": "/w"}}));
    assert_eq!(message_to_text(&loc), "[User]: The working directory has been changed to /w.");
    let syn = msg(json!({"type": "synthetic", "id": "s", "time": {"created": 1}, "text": "ctx"}));
    assert_eq!(message_to_text(&syn), "[Synthetic context]: ctx");
    let skill = msg(json!({"type": "skill", "id": "k", "time": {"created": 1}, "skill": "x", "name": "X", "text": "body"}));
    assert_eq!(message_to_text(&skill), "[Skill activated: X]\nbody");
    let shell = msg(json!({"type": "shell", "id": "sh", "time": {"created": 1}, "shellID": "s", "command": "ls", "status": "exited",
        "output": {"output": "a\nb", "cursor": 0, "size": 3, "truncated": false}}));
    assert_eq!(message_to_text(&shell), "[Shell]: ls\na\nb");
    let bg = msg(json!({"type": "shell", "id": "sh", "time": {"created": 1}, "shellID": "s", "command": "ls", "status": "running",
        "metadata": {"background": true}}));
    assert_eq!(message_to_text(&bg), "");
    let no_out = msg(json!({"type": "shell", "id": "sh", "time": {"created": 1}, "shellID": "s", "command": "ls", "status": "running"}));
    assert_eq!(message_to_text(&no_out), "[Shell]: ls\n");
    let sys = msg(json!({"type": "system", "id": "y", "time": {"created": 1}, "text": "instr"}));
    assert_eq!(message_to_text(&sys), "");
    assert_eq!(message_to_text(&compaction("c", "S", "R")), "");
    let idle = msg(json!({"type": "idle", "id": "i", "time": {"created": 1}, "outcome": "succeeded"}));
    assert_eq!(message_to_text(&idle), "");
}

#[test]
fn tool_output_truncation_counts_code_points() {
    let exact = "a".repeat(1_250);
    assert_eq!(truncate_tool_output(&exact), exact);
    let over = "a".repeat(1_251);
    assert_eq!(truncate_tool_output(&over), format!("{}\n[truncated]", "a".repeat(1_250)));
    // 700 astral chars: 1,400 UTF-16 units but only 700 code points → kept whole
    let emoji = "😀".repeat(700);
    assert_eq!(truncate_tool_output(&emoji), emoji);
    let emoji = "😀".repeat(1_300);
    assert_eq!(truncate_tool_output(&emoji), format!("{}\n[truncated]", "😀".repeat(1_250)));
}

#[test]
fn oldest_to_drop_walks_from_newest() {
    let sizes = [5u64, 5, 5, 5];
    assert_eq!(oldest_to_drop(&sizes, |s| *s, 100), 0);
    assert_eq!(oldest_to_drop(&sizes, |s| *s, 10), 2);
    assert_eq!(oldest_to_drop(&sizes, |s| *s, 9), 3);
    assert_eq!(oldest_to_drop(&sizes, |s| *s, 0), 4);
    assert_eq!(oldest_to_drop(&sizes, |s| *s, -1), 4);
}

#[test]
fn split_conversation_boundaries() {
    // Nothing to compact.
    assert_eq!(split_conversation(&[], 15_000), None);
    assert_eq!(split_conversation(&[compaction("c", "S", "")], 15_000), None);

    // Everything fits: keep only the latest exchange.
    let msgs = vec![user("u1", "q1"), assistant("a1", "r1"), user("u2", "q2"), assistant("a2", "r2")];
    assert_eq!(
        split_conversation(&msgs, 15_000),
        Some(Split { older: 2, recent: "[User]: q2\n\n[Assistant]: r2".into() })
    );
    // Nothing fits: the newest entry is still kept, from its exchange's user message.
    assert_eq!(split_conversation(&msgs, 0), Some(Split { older: 2, recent: "[User]: q2\n\n[Assistant]: r2".into() }));

    // One exchange, nothing older: summarize it all and keep nothing.
    let one = vec![user("u1", "q1"), assistant("a1", "r1")];
    assert_eq!(split_conversation(&one, 15_000), Some(Split { older: 2, recent: String::new() }));

    // One exchange after a summary that kept recent text: keep everything, summarize only the summary.
    let after = vec![compaction("c", "S", "[User]: old"), user("u1", "q1"), assistant("a1", "r1")];
    assert_eq!(
        split_conversation(&after, 15_000),
        Some(Split { older: 1, recent: "[User]: q1\n\n[Assistant]: r1".into() })
    );
    // ... but a summary without recent text summarizes everything.
    let after = vec![compaction("c", "S", ""), user("u1", "q1"), assistant("a1", "r1")];
    assert_eq!(split_conversation(&after, 15_000), Some(Split { older: 3, recent: String::new() }));

    // Keep budget: the boundary moves back to the user message at or before the first kept entry.
    let big = "x".repeat(4_000); // 1,000 tokens
    let msgs = vec![
        user("u1", &big),
        assistant("a1", &big),
        user("u2", &big),
        assistant("a2", &big),
        assistant("a3", "short"),
        user("u3", "q3"),
        assistant("a4", "r4"),
    ];
    // keep 1,100 tokens: a2 (1,002 tokens with label) does not fit together with the rest → dropped = 4 (a3);
    // the user boundary at or before it is u2 (entry 2).
    let split = split_conversation(&msgs, 1_100).unwrap();
    assert_eq!(split.older, 2);
    assert!(split.recent.starts_with("[User]: xxxx"));
    assert!(split.recent.ends_with("[User]: q3\n\n[Assistant]: r4"));
}

// ------------------------------------------------------------------------------------------
// flatten
// ------------------------------------------------------------------------------------------

fn base_request() -> oc_ai::LLMRequest {
    let settings = json!({"baseURL": "http://127.0.0.1:1/v1", "apiKey": "k"});
    let lm = oc_ai::providers::language_model("openai-compatible", "m1", settings.as_object().unwrap()).unwrap();
    oc_ai::LLMRequest::new(lm)
}

#[test]
fn flatten_messages() {
    let mut a = LlmMessage::assistant(vec![
        ContentPart::text("answer"),
        ContentPart::text(""),
        ContentPart::ToolCall(oc_ai::types::ToolCallPart {
            id: "c".into(),
            name: "read".into(),
            namespace: None,
            input: json!({"path": "/a"}),
            provider_executed: None,
            cache: None,
            metadata: None,
            provider_metadata: None,
        }),
    ]);
    a.id = Some("a".into());
    assert_eq!(flatten_message(&a), "[Assistant]: answer\n[Assistant tool call]: read({\"path\":\"/a\"})");
    let tool = LlmMessage::tool(oc_ai::types::ToolResultPart {
        id: "c".into(),
        name: "read".into(),
        namespace: None,
        result: oc_ai::types::ToolResultValue::Error(json!({"error": "x"})),
        provider_executed: None,
        cache: None,
        metadata: None,
        provider_metadata: None,
    });
    assert_eq!(flatten_message(&tool), "[Tool error]: {\"error\":\"x\"}");
    assert_eq!(flatten_message(&LlmMessage::system("instr")), "");
    let media = LlmMessage::user(vec![
        ContentPart::text("see"),
        ContentPart::Media(oc_ai::types::MediaPart {
            media: oc_ai::MediaAsset::base64("aaaa".to_string(), "image/png".to_string()),
            filename: None,
            cache: None,
            metadata: None,
            provider_metadata: None,
        }),
    ]);
    assert_eq!(flatten_message(&media), "[User]: see\n[image/png omitted]");
}

#[test]
fn flatten_and_drop_oldest_keeps_newest_exchanges() {
    let history = vec![
        compaction("c1", "S", "[User]: kept before"),
        user("u1", "q1"),
        assistant("a1", "r1"),
        user("u2", "q2"),
        assistant("a2", "r2"),
    ];
    let mut request = base_request();
    request.system = vec![oc_ai::types::SystemPart::text("sys")];
    request.messages = oc_core::lower::to_llm_messages(&history, &model_ref(), None);
    request.messages.insert(1, LlmMessage::system("instruction update"));

    let lead = oc_core::lower::to_llm_messages(&[compaction("c1", "S", "")], &model_ref(), None);
    let all = flatten_and_drop_oldest(&request, &history, &model_ref(), None).unwrap();
    assert_eq!(all.messages.len(), 2);
    assert_eq!(all.messages[0], lead[0]);
    assert_eq!(
        all.messages[1],
        LlmMessage::user_text("[User]: kept before\n\n[User]: q1\n[Assistant]: r1\n\n[User]: q2\n[Assistant]: r2")
    );

    // Room for only the newest exchange (plus the note reserve).
    let fixed = estimate_request(&request.system, &request.tools, &lead) as i64;
    let newest = token_estimate("[User]: q2\n[Assistant]: r2") as i64 + 1;
    let reserve = token_estimate("[3 older exchanges omitted]\n\n") as i64;
    let one = flatten_and_drop_oldest(&request, &history, &model_ref(), Some(fixed + reserve + newest)).unwrap();
    assert_eq!(
        one.messages[1],
        LlmMessage::user_text("[2 older exchanges omitted]\n\n[User]: q2\n[Assistant]: r2")
    );
    // Not even the newest exchange fits.
    assert!(flatten_and_drop_oldest(&request, &history, &model_ref(), Some(fixed + reserve)).is_none());

    // Without a previous compaction the lead is empty and everything is transcript.
    let plain = vec![user("u1", "q1"), assistant("a1", "r1")];
    let mut request2 = base_request();
    request2.messages = oc_core::lower::to_llm_messages(&plain, &model_ref(), None);
    let flat = flatten_and_drop_oldest(&request2, &plain, &model_ref(), Some(1_000)).unwrap();
    assert_eq!(flat.messages, vec![LlmMessage::user_text("[User]: q1\n[Assistant]: r1")]);
}

#[test]
fn recent_user_messages_within_keep() {
    let msgs = vec![user("u1", &"x".repeat(400)), assistant("a1", "r"), user("u2", "short")];
    let kept = recent_user_messages(&msgs, &model_ref(), &[], 50);
    assert_eq!(kept.len(), 1);
    assert_eq!(kept[0].id.as_deref(), Some("u2"));
    assert_eq!(recent_user_messages(&msgs, &model_ref(), &[], 1_000).len(), 2);
}

// ------------------------------------------------------------------------------------------
// End to end
// ------------------------------------------------------------------------------------------

fn sse(chunks: &[serde_json::Value]) -> Response {
    let mut body = String::new();
    for c in chunks {
        body.push_str(&format!("data: {c}\n\n"));
    }
    body.push_str("data: [DONE]\n\n");
    Response::builder().header("content-type", "text/event-stream").body(Body::from(body)).unwrap()
}

fn text_reply(id: &str, text: &str, prompt: u64) -> Response {
    sse(&[
        json!({"id": id, "choices": [{"index": 0, "delta": {"role": "assistant", "content": text}}]}),
        json!({"id": id, "choices": [{"index": 0, "delta": {}, "finish_reason": "stop"}],
               "usage": {"prompt_tokens": prompt, "completion_tokens": 7, "total_tokens": prompt + 7}}),
    ])
}

const SUMMARY: &str = "## Objective\n- Answer questions about the project\n\n## Next Move\n1. (none)";

struct NoStore;
#[async_trait::async_trait]
impl oc_tools::websearch::WebSearchStore for NoStore {
    async fn kv_get(&self, _: &str) -> Option<serde_json::Value> {
        None
    }
    async fn kv_set(&self, _: &str, _: serde_json::Value) {}
    async fn kv_remove(&self, _: &str) {}
    async fn api_key(&self, _: &str) -> Option<String> {
        None
    }
}

async fn prompt(app: &Arc<App>, session: &str, text: &str) {
    app.sessions.prompt(session, PromptInput { text: text.into(), ..Default::default() }).await.unwrap();
    app.coordinator().wake(session, Scope::Input);
    tokio::time::sleep(Duration::from_millis(50)).await;
    tokio::time::timeout(Duration::from_secs(20), app.coordinator().await_idle(session)).await.unwrap();
}

fn last_message(app: &Arc<App>, session: &str) -> Message {
    app.db.session_messages(session).unwrap().into_iter().map(|(_, m)| m).last().unwrap()
}

#[tokio::test]
async fn compacts_against_mock_provider() {
    let root = std::env::temp_dir().join(format!("oc-compaction-e2e-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    let project = root.join("project");
    std::fs::create_dir_all(&project).unwrap();
    for (k, v) in [("XDG_DATA_HOME", "data"), ("XDG_CONFIG_HOME", "config"), ("XDG_STATE_HOME", "state"), ("XDG_CACHE_HOME", "cache")] {
        unsafe { std::env::set_var(k, root.join(v)) };
    }
    unsafe {
        std::env::set_var("OPENCODE_DISABLE_MODELS_FETCH", "1");
        std::env::set_var("OPENCODE_TEST_HOME", root.join("home"));
    }

    // mode 0: template summary; 1: ignore the template once, then comply after the nudge; 2: never comply.
    let mode = Arc::new(AtomicUsize::new(0));
    let primary = Arc::new(AtomicUsize::new(0));
    let compactions = Arc::new(parking_lot::Mutex::new(Vec::<serde_json::Value>::new()));
    let primaries = Arc::new(parking_lot::Mutex::new(Vec::<serde_json::Value>::new()));
    let (m2, p2, c2, b2) = (mode.clone(), primary.clone(), compactions.clone(), primaries.clone());
    let router = Router::new().route(
        "/v1/chat/completions",
        post(move |body: axum::Json<serde_json::Value>| {
            let raw = body.0.to_string();
            let reply = if raw.contains("You are a title generator") {
                text_reply("t", "Title", 3)
            } else if raw.contains("You MUST summarize the conversation above") || raw.contains("Update the existing checkpoint") {
                c2.lock().push(body.0.clone());
                let nudged = raw.contains("did not fill in the required summary template");
                match m2.load(Ordering::SeqCst) {
                    0 => text_reply("s", SUMMARY, 50),
                    1 if nudged => text_reply("s", SUMMARY, 60),
                    _ => text_reply("s", "Sure, here is what happened.", 40),
                }
            } else {
                b2.lock().push(body.0.clone());
                let n = p2.fetch_add(1, Ordering::SeqCst) + 1;
                text_reply("c", &format!("answer {n}"), 20)
            };
            async move { reply }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });

    std::fs::write(
        project.join("opencode.json"),
        json!({
            "model": "mock/m1",
            "providers": {"mock": {
                "name": "Mock",
                "package": "@opencode/ai/providers/openai-compatible",
                "settings": {"baseURL": format!("http://127.0.0.1:{port}/v1"), "apiKey": "test"},
                "models": {"m1": {"name": "Mock One", "limit": {"context": 100000, "output": 4096}}}
            }}
        })
        .to_string(),
    )
    .unwrap();

    let global = oc_util::Global::acquire().unwrap();
    let db = oc_storage::Database::open(&global.database("latest"), &global.data).unwrap();
    let app = App::new(global, db, "2.0.26");
    let services = oc_tools::tools::Services {
        formatter: Arc::new(oc_tools::formatter::Formatter::new()),
        ripgrep: Arc::new(oc_tools::ripgrep::Ripgrep::new(app.global.bin.clone(), reqwest::Client::new())),
        shell: oc_tools::shell::service::ShellService::new(app.global.data.clone(), None),
        websearch: Arc::new(oc_tools::websearch::WebSearch::new(Arc::new(NoStore), reqwest::Client::new())),
        http: reqwest::Client::new(),
    };
    let engine = Engine::new(app.clone(), &services);
    engine.install();

    let s = app
        .sessions
        .create(CreateInput { location: Some(LocationRef { directory: project.display().to_string(), workspace_id: None }), ..Default::default() })
        .await
        .unwrap();
    prompt(&app, &s.id, "first question").await;
    prompt(&app, &s.id, "second question").await;

    // Auto: a small conversation in a 100k window is not due.
    let loaded = engine.load(engine.select(&s.id).await.unwrap()).await.unwrap();
    let (measured, estimated) = engine.estimate_prompt(&loaded);
    assert_eq!(measured, 27, "latest response's prompt + output");
    assert_eq!(estimated, 0, "nothing after the anchor");
    let out = engine.compact(&loaded, Trigger::Auto, CancellationToken::new()).await.unwrap();
    assert_eq!(out, CompactOutcome::Skipped);
    assert!(compactions.lock().is_empty());

    // Overflow: summarize the first exchange, keep the second verbatim.
    let out = engine.compact(&loaded, Trigger::Overflow, CancellationToken::new()).await.unwrap();
    assert_eq!(out, CompactOutcome::Completed);
    let Message::Compaction(c) = last_message(&app, &s.id) else { panic!("{:#?}", last_message(&app, &s.id)) };
    match &c.status {
        CompactionStatus::Completed { reason, summary, recent, model, tokens, cost, .. } => {
            assert_eq!(*reason, CompactionReason::Auto);
            assert_eq!(summary, SUMMARY);
            assert_eq!(recent, "[User]: second question\n\n[Assistant]: answer 2");
            assert_eq!(model.as_ref().map(|m| m.id.as_str()), Some("m1"));
            assert_eq!(tokens.as_ref().map(|t| t.input), Some(50));
            assert_eq!(*cost, Some(0.0));
        }
        other => panic!("{other:?}"),
    }
    {
        let bodies = compactions.lock();
        assert_eq!(bodies.len(), 1);
        let messages = bodies[0]["messages"].as_array().unwrap();
        let last = messages.last().unwrap();
        assert_eq!(last["role"], "user");
        assert_eq!(last["content"].as_str().map(str::to_string).unwrap_or_else(|| last["content"][0]["text"].as_str().unwrap().to_string()), build_prompt(false, false));
        let raw = serde_json::to_string(messages).unwrap();
        assert!(raw.contains("first question") && raw.contains("answer 1"));
        assert!(!raw.contains("second question"), "{raw}");
        assert_eq!(bodies[0]["max_completion_tokens"], 4096);
    }

    // History after it starts from the checkpoint.
    let history = oc_core::runner::history_for_runner(&app.bus, &s.id).unwrap();
    assert_eq!(history.len(), 1);
    assert!(matches!(&history[0], Message::Compaction(c) if matches!(c.status, CompactionStatus::Completed { .. })));
    // The completed compaction is the last message: auto is not due right after it.
    let loaded = engine.load(engine.select(&s.id).await.unwrap()).await.unwrap();
    assert_eq!(engine.compact(&loaded, Trigger::Auto, CancellationToken::new()).await.unwrap(), CompactOutcome::Skipped);
    // Nothing to compact yet: only the checkpoint is loaded.
    let out = engine.compact(&loaded, Trigger::Overflow, CancellationToken::new()).await.unwrap();
    assert_eq!(out, CompactOutcome::Failed(oc_schema::types::SessionError::new("compaction.unavailable", "Nothing to compact yet")));

    // The next request opens with the checkpoint.
    prompt(&app, &s.id, "third question").await;
    {
        let bodies = primaries.lock();
        let body = bodies.last().unwrap();
        let raw = body["messages"].to_string();
        assert!(raw.contains("<conversation-checkpoint>"), "{raw}");
        assert!(raw.contains("<recent-context>\\n[User]: second question"), "{raw}");
        assert!(!raw.contains("first question"), "{raw}");
    }

    // Manual compaction with a nudge: the update prompt is used, the reply is retried once.
    mode.store(1, Ordering::SeqCst);
    let input_id = oc_schema::id::message();
    app.bus
        .commit_one(SessionEvent::CompactionStarted {
            session_id: s.id.clone(),
            reason: CompactionReason::Manual,
            recent: String::new(),
            input_id: Some(input_id.clone()),
        })
        .unwrap();
    let loaded = engine.load(engine.select(&s.id).await.unwrap()).await.unwrap();
    let out = engine.compact(&loaded, Trigger::Manual { input_id: input_id.clone() }, CancellationToken::new()).await.unwrap();
    assert_eq!(out, CompactOutcome::Completed);
    {
        let bodies = compactions.lock();
        assert_eq!(bodies.len(), 3, "first reply + nudged retry");
        let raw = bodies[2]["messages"].to_string();
        assert!(raw.contains("Update the existing checkpoint"));
        assert!(raw.contains(NUDGE));
    }
    let Message::Compaction(c) = last_message(&app, &s.id) else { panic!() };
    assert_eq!(c.id, input_id);
    match &c.status {
        CompactionStatus::Completed { reason, summary, recent, tokens, .. } => {
            assert_eq!(*reason, CompactionReason::Manual);
            assert_eq!(summary, SUMMARY);
            // One exchange after a summary that kept recent text: keep it all verbatim.
            assert_eq!(recent, "[User]: third question\n\n[Assistant]: answer 3");
            // both calls are billed on the compaction message
            assert_eq!(tokens.as_ref().map(|t| t.input), Some(100));
        }
        other => panic!("{other:?}"),
    }

    // A reply that never fills the template fails the compaction.
    mode.store(2, Ordering::SeqCst);
    prompt(&app, &s.id, "fourth question").await;
    let loaded = engine.load(engine.select(&s.id).await.unwrap()).await.unwrap();
    let out = engine.compact(&loaded, Trigger::Overflow, CancellationToken::new()).await.unwrap();
    let expected = oc_schema::types::SessionError::new("compaction.failed", "Compaction summary did not match the required template");
    assert_eq!(out, CompactOutcome::Failed(expected.clone()));
    let Message::Compaction(c) = last_message(&app, &s.id) else { panic!() };
    match &c.status {
        CompactionStatus::Failed { error, tokens, .. } => {
            assert_eq!(error, &expected);
            assert_eq!(tokens.as_ref().map(|t| t.input), Some(80));
        }
        other => panic!("{other:?}"),
    }
    // A failed compaction does not move the history boundary.
    let history = oc_core::runner::history_for_runner(&app.bus, &s.id).unwrap();
    assert_eq!(history[0].id(), input_id);

    // Automatic compaction off: overflow skips too.
    std::fs::write(
        project.join("opencode.json"),
        json!({
            "model": "mock/m1",
            "compaction": {"auto": false},
            "providers": {"mock": {
                "name": "Mock",
                "package": "@opencode/ai/providers/openai-compatible",
                "settings": {"baseURL": format!("http://127.0.0.1:{port}/v1"), "apiKey": "test"},
                "models": {"m1": {"name": "Mock One", "limit": {"context": 100000, "output": 4096}}}
            }}
        })
        .to_string(),
    )
    .unwrap();
    engine.invalidate_locations();
    let loaded = engine.load(engine.select(&s.id).await.unwrap()).await.unwrap();
    assert_eq!(engine.compact(&loaded, Trigger::Overflow, CancellationToken::new()).await.unwrap(), CompactOutcome::Skipped);

    std::fs::remove_dir_all(&root).ok();
}

//! The Claude Agent language model (`aisdk.ts` `sdk` hook + `ClaudeAgentLanguageModel.doStream`),
//! served natively in front of the location's sidecar AI SDK bridge: models of provider
//! `claude-agent` with package `@opencode/claude-agent` never reach the sidecar.
//!
//! External plugins' `aisdk.sdk` hooks cannot replace this model (upstream's own hook runs first,
//! but a later external hook could still swap the SDK; ocd does not support that).

use std::sync::Arc;

use futures::future::BoxFuture;
use oc_ai::aisdk::{AiSdkBridge, BridgeError, PartStream, StreamCall};
use oc_ai::error::TransportOperation;
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

use super::process::SystemPrompt;
use super::sessions::{self, Request};
use super::stream::{Converter, error_shape};
use super::{ClaudeAgent, PACKAGE, PROVIDER_ID};

pub const KIND_HEADER: &str = "x-opencode-claude-agent-kind";
pub const SESSION_HEADER: &str = "x-opencode-session-id";
pub const FORK_HEADER: &str = "x-opencode-claude-agent-fork-source";
pub const AGENT_OPTION: &str = "claudeCodeAgent";
pub const INSTRUCTIONS_OPTION: &str = "claudeCodeInstructions";
const RUNTIME: &str = "<runtime_info>You are running in OpenCode through the Claude Code harness. Mention this only when asked.</runtime_info>";
const EFFORTS: [&str; 5] = ["low", "medium", "high", "xhigh", "max"];

/// `ClaudeSystemPrompt.make(role, instructions)`.
pub fn system_prompt(role: Option<&str>, instructions: Option<&str>) -> SystemPrompt {
    let mut blocks = vec![RUNTIME.to_string()];
    if let Some(role) = role.filter(|r| !r.is_empty()) {
        blocks.push(format!("# Agent role (from OpenCode)\n{role}"));
    }
    if let Some(instructions) = instructions.filter(|i| !i.is_empty()) {
        blocks.push(instructions.to_string());
    }
    SystemPrompt::Preset { append: blocks.join("\n\n") }
}

/// `ClaudeSystemPrompt.utility(prompt)`.
pub fn utility(prompt: &[Value]) -> String {
    let blocks: Vec<String> = prompt
        .iter()
        .filter(|m| m.get("role").and_then(Value::as_str) == Some("system"))
        .map(|m| m.get("content").map(|c| c.as_str().map(String::from).unwrap_or_else(|| oc_ai::json::js_string(c))).unwrap_or_default())
        .collect();
    let joined = blocks.join("\n\n");
    if joined.is_empty() { "You are a text generation helper. Return only the requested output.".into() } else { joined }
}

/// Whether a `RuntimeInfo` is a Claude Agent model.
pub fn is_claude(model: &Value) -> bool {
    let package = model.get("package").and_then(Value::as_str).unwrap_or_default();
    model.get("providerID").and_then(Value::as_str) == Some(PROVIDER_ID) && oc_ai::catalog::types::package_name(package) == PACKAGE
}

/// The connection environment from the SDK options upstream's `sdk` hook sees (`prepareOptions`:
/// native settings, the model's headers and its projected body).
fn connection(model: &Value) -> Vec<(String, String)> {
    let settings = model.get("settings").and_then(Value::as_object).map(oc_ai::catalog::types::native_settings);
    let mut body = model.get("body").cloned();
    if body.as_ref().and_then(|b| b.get("reasoning")).and_then(|r| r.get("mode")).and_then(Value::as_str) == Some("pro")
        && let Some(Value::Object(o)) = body.as_mut()
    {
        o.shift_remove("reasoning");
        if o.is_empty() {
            body = None;
        }
    }
    super::connection_env(settings.as_ref(), model.get("headers"), body.as_ref())
}

pub struct ClaudeBridge {
    pub inner: Arc<dyn AiSdkBridge>,
    pub claude: Arc<ClaudeAgent>,
    pub key: String,
}

impl AiSdkBridge for ClaudeBridge {
    fn load(&self, model: Value) -> BoxFuture<'static, Result<(), Option<String>>> {
        if is_claude(&model) {
            return Box::pin(async { Ok(()) });
        }
        self.inner.load(model)
    }

    fn stream(&self, call: StreamCall) -> BoxFuture<'static, Result<PartStream, BridgeError>> {
        if !is_claude(&call.model) {
            return self.inner.stream(call);
        }
        let claude = self.claude.clone();
        let key = self.key.clone();
        Box::pin(async move { do_stream(claude, key, call) })
    }
}

fn request_error(message: String) -> BridgeError {
    BridgeError::Provider { operation: TransportOperation::Request, shape: error_shape(&message) }
}

/// `ClaudeAgentLanguageModel.doStream(options)`.
fn do_stream(claude: Arc<ClaudeAgent>, key: String, call: StreamCall) -> Result<PartStream, BridgeError> {
    let options = &call.options;
    let tools: Vec<Value> = if options.get("toolChoice").and_then(|c| c.get("type")).and_then(Value::as_str) == Some("none") {
        vec![]
    } else {
        options
            .get("tools")
            .and_then(Value::as_array)
            .map(|t| t.iter().filter(|t| t.get("type").and_then(Value::as_str) == Some("function")).cloned().collect())
            .unwrap_or_default()
    };
    let headers = options.get("headers").cloned().unwrap_or(json!({}));
    let header = |name: &str| headers.get(name).and_then(Value::as_str).map(String::from);
    let kind = header(KIND_HEADER);
    let session = header(SESSION_HEADER);
    let settings = options.get("providerOptions").and_then(|p| p.get(PROVIDER_ID)).cloned().unwrap_or(Value::Null);
    let agent = settings.get(AGENT_OPTION).and_then(Value::as_str).map(String::from);
    let instructions = settings.get(INSTRUCTIONS_OPTION).and_then(Value::as_str).map(String::from);
    let effort = match settings.get("effort") {
        None => None,
        Some(Value::String(e)) if EFFORTS.contains(&e.as_str()) => Some(e.clone()),
        Some(_) => return Err(request_error("Claude Agent effort must be low, medium, high, xhigh or max".into())),
    };
    let primary = kind.as_deref() == Some("primary");
    let branch = matches!(kind.as_deref(), Some("generate" | "compaction"));
    let session_id = if primary { session.clone() } else { None };
    // Conversation branches copy their source session and keep the Claude prompt. OpenCode supplies their task as user input.
    let source_id = if primary {
        header(FORK_HEADER)
    } else if branch {
        session
    } else {
        None
    };
    // Branches offer the chat's tools and system prompt so Claude reuses the chat's prompt cache.
    let offered = if primary || branch { tools } else { vec![] };
    let prompt = options.get("prompt").and_then(Value::as_array).cloned().unwrap_or_default();
    // Titles are standalone tasks, so OpenCode's system prompt replaces Claude Code's.
    let system = if primary || branch { system_prompt(agent.as_deref(), instructions.as_deref()) } else { SystemPrompt::Text(utility(&prompt)) };
    let model_id = call.model.get("modelID").or_else(|| call.model.get("id")).and_then(Value::as_str).unwrap_or_default().to_string();
    let Some(ctx) = claude.context(&key) else { return Err(request_error("Claude Agent is unavailable at this location".into())) };

    let (tx, rx) = tokio::sync::mpsc::unbounded_channel::<Result<Value, BridgeError>>();
    let _ = tx.send(Ok(json!({ "type": "stream-start", "warnings": [] })));
    let mut convert = Converter::new(offered.clone());
    let parts = tx.clone();
    let request = Request {
        session_id,
        source_id,
        model: model_id,
        effort,
        max_output_tokens: options.get("maxOutputTokens").and_then(Value::as_f64),
        connection: connection(&call.model),
        system,
        prompt,
        tools: offered,
        emit: Box::new(move |message: &Value| {
            for part in convert.convert(message) {
                let _ = parts.send(Ok(part));
            }
        }),
    };
    let cancel = CancellationToken::new();
    // The consumer going away (or the call's abort) cancels the run; the run still drains and saves its turn.
    {
        let (cancel, outer, watched) = (cancel.clone(), call.cancel.clone(), tx.clone());
        tokio::spawn(async move {
            tokio::select! {
                _ = outer.cancelled() => {}
                _ = watched.closed() => {}
                _ = cancel.cancelled() => return,
            }
            cancel.cancel();
        });
    }
    let registry = claude.registry.clone();
    tokio::spawn(async move {
        let result = registry.run(&ctx, request, cancel.clone()).await;
        if let Err(error) = result
            && !cancel.is_cancelled()
            && !sessions::is_cancelled(&error)
        {
            let _ = tx.send(Err(BridgeError::Provider { operation: TransportOperation::Read, shape: error_shape(&error) }));
        }
        // Ends the watcher: a finished run has nothing to cancel.
        cancel.cancel();
    });
    let stream: PartStream = Box::pin(futures::stream::unfold(rx, |mut rx| async move { rx.recv().await.map(|item| (item, rx)) }));
    Ok(stream)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn system_prompts() {
        assert_eq!(system_prompt(None, None), SystemPrompt::Preset { append: RUNTIME.into() });
        assert_eq!(
            system_prompt(Some("Be terse."), Some("AGENTS")),
            SystemPrompt::Preset { append: format!("{RUNTIME}\n\n# Agent role (from OpenCode)\nBe terse.\n\nAGENTS") }
        );
        let prompt = vec![
            json!({ "role": "system", "content": "Summarize this conversation." }),
            json!({ "role": "system", "content": "Use one sentence." }),
            json!({ "role": "user", "content": [{ "type": "text", "text": "First question" }] }),
        ];
        assert_eq!(utility(&prompt), "Summarize this conversation.\n\nUse one sentence.");
        assert_eq!(utility(&[]), "You are a text generation helper. Return only the requested output.");
    }

    #[test]
    fn detects_the_package() {
        assert!(is_claude(&json!({ "providerID": "claude-agent", "package": "aisdk:@opencode/claude-agent" })));
        assert!(!is_claude(&json!({ "providerID": "claude-agent", "package": "aisdk:other" })));
        assert!(!is_claude(&json!({ "providerID": "x", "package": "aisdk:@opencode/claude-agent" })));
    }
}

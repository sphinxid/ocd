//! Account rotation (`claude/accounts.ts`, 2.4.1): with a list of Claude config folders (`configDir`), a request
//! that a plan or rate limit refuses before Claude showed any output runs again on the next folder, and the folder
//! that worked sticks to the chat (`accounts/<sessionID>`) and to new chats (`last-account`).

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use parking_lot::Mutex;
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

use super::sessions::{Context, Registry, Request};
use super::stream::{Failed, Failure};

/// The folder the last successful request ran on.
pub const LAST: &str = "last-account";

/// The folder a chat last worked on.
pub fn key(session_id: &str) -> String {
    format!("accounts/{session_id}")
}

type Emit = Box<dyn FnMut(&Value) + Send + Sync>;

/// One attempt's view of the messages: everything before the first visible output is held, so a refused attempt
/// can be dropped unseen.
struct Watch {
    failure: Failure,
    held: Option<Vec<Value>>,
    /// Whether messages are still held (the turn's view of `held`).
    holding: Arc<AtomicBool>,
    error: Option<Failed>,
}

impl Watch {
    fn new() -> Watch {
        Watch { failure: Failure::default(), held: Some(vec![]), holding: Arc::new(AtomicBool::new(true)), error: None }
    }

    fn emit(&mut self, message: &Value, out: &mut Emit) {
        if let Some(failed) = self.failure.failed(message) {
            self.error = Some(failed);
        }
        if self.held.is_none() {
            out(message);
            return;
        }
        let kind = message.get("type").and_then(Value::as_str).unwrap_or_default();
        let visible = kind == "stream_event" || (kind == "assistant" && !message.get("error").is_some_and(oc_ai::json::truthy));
        if !visible {
            if let Some(held) = self.held.as_mut() {
                held.push(message.clone());
            }
            return;
        }
        self.flush(out);
        out(message);
    }

    fn flush(&mut self, out: &mut Emit) {
        self.holding.store(false, Ordering::SeqCst);
        for message in self.held.take().unwrap_or_default() {
            out(&message);
        }
    }

    fn shown(&self) -> bool {
        self.held.is_none()
    }
}

async fn read(ctx: &Context, key: &str) -> Option<String> {
    ctx.storage.get(key).await.and_then(|value| value.as_str().map(String::from))
}

/// `ClaudeAccounts.run(request, configDirs)`.
pub async fn run(registry: &Arc<Registry>, ctx: &Context, request: Request, config_dirs: &[String], cancel: CancellationToken) -> Result<(), String> {
    let keyed = request.connection.iter().any(|(k, v)| (k == "ANTHROPIC_API_KEY" || k == "ANTHROPIC_AUTH_TOKEN") && !v.is_empty());
    // An API key or token logs in the same way whatever the folder, so those requests never rotate.
    if config_dirs.len() < 2 || keyed {
        return registry.run(ctx, request, cancel).await;
    }
    let own = match &request.session_id {
        Some(id) => read(ctx, &key(id)).await,
        None => None,
    };
    let last = read(ctx, LAST).await;
    // Side calls and new forks start on their parent chat's account, which has its transcript and prompt cache.
    let parent = match (&own, &request.source_id) {
        (None, Some(source)) => read(ctx, &key(source)).await,
        _ => None,
    };
    let from = own.clone().or(parent).or(last.clone());
    let start = from.filter(|f| !f.is_empty()).and_then(|f| config_dirs.iter().position(|d| *d == f)).unwrap_or(0);
    let dirs: Vec<String> = config_dirs[start..].iter().chain(&config_dirs[..start]).cloned().collect();

    let Request { session_id, source_id, model, effort, max_output_tokens, connection, system, prompt, tools, emit, .. } = request;
    let out = Arc::new(Mutex::new(emit));
    for (i, dir) in dirs.iter().enumerate() {
        let watch = Arc::new(Mutex::new(Watch::new()));
        let holding = watch.lock().holding.clone();
        let mut connection = connection.clone();
        match connection.iter_mut().find(|(k, _)| k == "CLAUDE_CONFIG_DIR") {
            Some(slot) => slot.1 = dir.clone(),
            None => connection.push(("CLAUDE_CONFIG_DIR".into(), dir.clone())),
        }
        let attempt = Request {
            session_id: session_id.clone(),
            source_id: source_id.clone(),
            model: model.clone(),
            effort: effort.clone(),
            max_output_tokens,
            connection,
            system: system.clone(),
            prompt: prompt.clone(),
            tools: tools.clone(),
            emit: {
                let (watch, out) = (watch.clone(), out.clone());
                Box::new(move |message: &Value| watch.lock().emit(message, &mut out.lock()))
            },
            holding: Some(holding),
        };
        registry.run(ctx, attempt, cancel.clone()).await?;
        let (error, shown) = {
            let watch = watch.lock();
            (watch.error.clone(), watch.shown())
        };
        let flush = || watch.lock().flush(&mut out.lock());
        let Some(error) = error else {
            // A side call never moves its chat, because the side call's model may have hit a limit of its own.
            if let Some(id) = &session_id
                && own.as_deref() != Some(dir.as_str())
            {
                ctx.storage.set(&key(id), json!(dir)).await;
            }
            if last.as_deref() != Some(dir.as_str()) {
                ctx.storage.set(LAST, json!(dir)).await;
            }
            flush();
            return Ok(());
        };
        // Only a limit is worth another account. A retry after Claude has shown output would repeat that output.
        if !error.limited || shown || i == dirs.len() - 1 {
            flush();
            return Ok(());
        }
        tracing::info!(from = %dir, to = %dirs[i + 1], "Claude account refused the request, retrying on the next one");
    }
    Ok(())
}

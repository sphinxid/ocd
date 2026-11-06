//! `ClaudeIO.real` (`claude/io.ts`, `claude/fork.ts`): finding `claude`, the environment, and the
//! transcript operations. Transcripts are Claude Code's private JSONL files, so they are read and
//! written by the real `@anthropic-ai/claude-agent-sdk` 0.3.301 file functions in the plugin
//! sidecar (installed into ocd's npm cache on first use, without the optional native `claude`).

use std::collections::{BTreeMap, HashMap};
use std::path::PathBuf;
use std::sync::Weak;

use async_trait::async_trait;
use serde_json::{Value, json};

use super::sessions::{Fork, Io};

/// What upstream reports when its SDK cannot be loaded.
const SDK_MISSING: &str = "Claude Agent SDK isn't installed. Install @anthropic-ai/claude-agent-sdk to use Claude Agent.";

pub struct RealIo {
    plugins: Weak<crate::plugins::Plugins>,
}

impl RealIo {
    pub fn new(plugins: Weak<crate::plugins::Plugins>) -> RealIo {
        RealIo { plugins }
    }

    async fn call(&self, params: Value) -> Result<Value, String> {
        let plugins = self.plugins.upgrade().ok_or_else(|| SDK_MISSING.to_string())?;
        let host = plugins.host_or_create();
        host.claude_transcript(params)
            .await
            .map_err(|error| if error.starts_with("Plugin host is not running") { SDK_MISSING.to_string() } else { error })
    }
}

/// `process.env.OPENCODE_CLAUDE_PATH || which("claude") || undefined`.
pub fn which() -> Option<String> {
    if let Ok(path) = std::env::var("OPENCODE_CLAUDE_PATH")
        && !path.is_empty()
    {
        return Some(path);
    }
    which::which("claude").ok().map(|p| p.to_string_lossy().into_owned())
}

#[async_trait]
impl Io for RealIo {
    fn which(&self) -> Option<String> {
        which()
    }

    fn env(&self) -> BTreeMap<String, String> {
        super::process_env()
    }

    async fn bun(&self) -> Option<PathBuf> {
        if let Some(path) = oc_plugin::bun::env_override() {
            return Some(path);
        }
        let plugins = self.plugins.upgrade()?;
        plugins.host_or_create().bun().await.ok()
    }

    async fn session_exists(&self, session_id: &str) -> Result<bool, String> {
        Ok(self.call(json!({ "op": "exists", "sessionID": session_id })).await? == Value::Bool(true))
    }

    async fn fork(&self, session_id: &str, message_id: &str) -> Result<Option<Fork>, String> {
        let out = self.call(json!({ "op": "fork", "sessionID": session_id, "messageID": message_id })).await?;
        let Some(object) = out.as_object() else { return Ok(None) };
        let messages: HashMap<String, String> = object
            .get("messages")
            .and_then(Value::as_object)
            .map(|m| m.iter().filter_map(|(k, v)| Some((k.clone(), v.as_str()?.to_string()))).collect())
            .unwrap_or_default();
        let session_id = object.get("sessionID").and_then(Value::as_str).unwrap_or_default().to_string();
        Ok(Some(Fork { session_id, messages }))
    }

    async fn remove(&self, session_id: &str) -> Result<(), String> {
        self.call(json!({ "op": "remove", "sessionID": session_id })).await.map(|_| ())
    }
}

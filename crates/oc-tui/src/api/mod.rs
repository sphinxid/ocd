//! HTTP client for the opencode v2 API (`packages/client`, generated promise client subset).

pub mod sse;
pub mod types;

use base64::Engine;
use reqwest::Method;
use serde::de::DeserializeOwned;
use serde_json::{Value, json};
use types::*;

/// An error returned by the server (`{_tag, message, ...}`) or the transport.
#[derive(Debug, Clone)]
pub struct ApiError {
    pub status: u16,
    pub tag: String,
    pub message: String,
    pub body: Value,
}

impl std::fmt::Display for ApiError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for ApiError {}

#[derive(Clone)]
pub struct Client {
    http: reqwest::Client,
    pub base: String,
    auth: Option<String>,
    /// Default location directory (`x-opencode-directory` / `location[directory]`).
    pub directory: String,
}

impl Client {
    pub fn new(base: &str, password: Option<&str>, directory: &str) -> Client {
        let http = reqwest::Client::builder().build().unwrap_or_default();
        let auth = password.filter(|p| !p.is_empty()).map(|p| {
            format!("Basic {}", base64::engine::general_purpose::STANDARD.encode(format!("opencode:{p}")))
        });
        Client { http, base: base.trim_end_matches('/').to_string(), auth, directory: directory.to_string() }
    }

    pub fn http(&self) -> &reqwest::Client {
        &self.http
    }

    pub fn auth_header(&self) -> Option<&str> {
        self.auth.as_deref()
    }

    fn request(&self, method: Method, path: &str) -> reqwest::RequestBuilder {
        let mut r = self.http.request(method, format!("{}{}", self.base, path));
        if let Some(a) = &self.auth {
            r = r.header("authorization", a);
        }
        r
    }

    async fn send(&self, r: reqwest::RequestBuilder) -> anyhow::Result<reqwest::Response> {
        let resp = r.send().await?;
        let status = resp.status();
        if status.is_success() {
            return Ok(resp);
        }
        let text = resp.text().await.unwrap_or_default();
        let body: Value = serde_json::from_str(&text).unwrap_or(Value::Null);
        let tag = body.get("_tag").and_then(|v| v.as_str()).unwrap_or("").to_string();
        let message = body
            .get("message")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string())
            .or_else(|| body.pointer("/data/message").and_then(|v| v.as_str()).map(|s| s.to_string()))
            .unwrap_or_else(|| if text.is_empty() { format!("Request failed with status {}", status.as_u16()) } else { text.clone() });
        Err(ApiError { status: status.as_u16(), tag, message, body }.into())
    }

    async fn json<T: DeserializeOwned>(&self, r: reqwest::RequestBuilder) -> anyhow::Result<T> {
        let resp = self.send(r).await?;
        Ok(resp.json::<T>().await?)
    }

    async fn empty(&self, r: reqwest::RequestBuilder) -> anyhow::Result<()> {
        self.send(r).await?;
        Ok(())
    }

    fn loc(&self, r: reqwest::RequestBuilder, dir: Option<&str>) -> reqwest::RequestBuilder {
        r.query(&[("location[directory]", dir.unwrap_or(&self.directory))])
    }

    // ---- location ----------------------------------------------------------

    pub async fn location_get(&self, dir: Option<&str>) -> anyhow::Result<LocationInfo> {
        self.json(self.loc(self.request(Method::GET, "/api/location"), dir)).await
    }

    pub async fn location_reload(&self) -> anyhow::Result<()> {
        self.empty(self.request(Method::POST, "/api/location/reload")).await
    }

    pub async fn info(&self) -> anyhow::Result<Value> {
        self.json(self.request(Method::GET, "/api/info")).await
    }

    // ---- catalogs ----------------------------------------------------------

    async fn list<T: DeserializeOwned>(&self, path: &str, dir: Option<&str>) -> anyhow::Result<Vec<T>> {
        let env: LocationEnvelope<Vec<T>> = self.json(self.loc(self.request(Method::GET, path), dir)).await?;
        Ok(env.data)
    }

    pub async fn agents(&self, dir: Option<&str>) -> anyhow::Result<Vec<AgentInfo>> {
        self.list("/api/agent", dir).await
    }
    pub async fn models(&self, dir: Option<&str>) -> anyhow::Result<Vec<ModelInfo>> {
        self.list("/api/model", dir).await
    }
    pub async fn default_model(&self, dir: Option<&str>) -> anyhow::Result<Option<ModelInfo>> {
        let env: LocationEnvelope<Option<ModelInfo>> =
            self.json(self.loc(self.request(Method::GET, "/api/model/default"), dir)).await?;
        Ok(env.data)
    }
    pub async fn providers(&self, dir: Option<&str>) -> anyhow::Result<Vec<ProviderInfo>> {
        self.list("/api/provider", dir).await
    }
    pub async fn commands(&self, dir: Option<&str>) -> anyhow::Result<Vec<CommandInfo>> {
        self.list("/api/command", dir).await
    }
    pub async fn skills(&self, dir: Option<&str>) -> anyhow::Result<Vec<SkillInfo>> {
        self.list("/api/skill", dir).await
    }
    pub async fn mcp(&self, dir: Option<&str>) -> anyhow::Result<Vec<McpServer>> {
        self.list("/api/mcp", dir).await
    }
    pub async fn references(&self, dir: Option<&str>) -> anyhow::Result<Vec<ReferenceInfo>> {
        self.list("/api/reference", dir).await
    }
    pub async fn integrations(&self, dir: Option<&str>) -> anyhow::Result<Vec<Value>> {
        self.list("/api/integration", dir).await
    }
    pub async fn permissions(&self, dir: Option<&str>) -> anyhow::Result<Vec<PermissionRequest>> {
        self.list("/api/permission/request", dir).await
    }
    pub async fn forms(&self, dir: Option<&str>) -> anyhow::Result<Vec<FormInfo>> {
        self.list("/api/form", dir).await
    }
    pub async fn vcs(&self, dir: Option<&str>) -> anyhow::Result<Value> {
        let env: LocationEnvelope<Value> = self.json(self.loc(self.request(Method::GET, "/api/vcs"), dir)).await?;
        Ok(env.data)
    }

    pub async fn mcp_connect(&self, name: &str, connect: bool, dir: Option<&str>) -> anyhow::Result<()> {
        let action = if connect { "connect" } else { "disconnect" };
        let path = format!("/api/experimental/mcp/{}/{action}", urlencode(name));
        self.empty(self.loc(self.request(Method::POST, &path), dir)).await
    }

    // ---- filesystem --------------------------------------------------------

    pub async fn file_find(&self, query: &str, limit: u32, dir: Option<&str>) -> anyhow::Result<Vec<FsEntry>> {
        let r = self.loc(self.request(Method::GET, "/api/fs/find"), dir).query(&[("query", query), ("limit", &limit.to_string())]);
        let env: LocationEnvelope<Vec<FsEntry>> = self.json(r).await?;
        Ok(env.data)
    }

    pub async fn file_list(&self, path: Option<&str>, dir: Option<&str>) -> anyhow::Result<(String, Vec<FsEntry>)> {
        let mut r = self.loc(self.request(Method::GET, "/api/fs/list"), dir);
        if let Some(p) = path {
            r = r.query(&[("path", p)]);
        }
        let env: LocationEnvelope<Vec<FsEntry>> = self.json(r).await?;
        Ok((env.location.map(|l| l.directory).unwrap_or_default(), env.data))
    }

    // ---- sessions ----------------------------------------------------------

    pub async fn session_list(&self, query: &[(&str, String)]) -> anyhow::Result<Page<SessionInfo>> {
        self.json(self.request(Method::GET, "/api/session").query(query)).await
    }

    pub async fn session_get(&self, id: &str) -> anyhow::Result<SessionInfo> {
        let env: DataEnvelope<SessionInfo> = self.json(self.request(Method::GET, &format!("/api/session/{id}"))).await?;
        Ok(env.data)
    }

    pub async fn session_active(&self) -> anyhow::Result<Value> {
        let env: DataEnvelope<Value> = self.json(self.request(Method::GET, "/api/session/active")).await?;
        Ok(env.data)
    }

    pub async fn session_create(&self, body: Value) -> anyhow::Result<SessionInfo> {
        let env: DataEnvelope<SessionInfo> = self.json(self.request(Method::POST, "/api/session").json(&body)).await?;
        Ok(env.data)
    }

    pub async fn session_delete(&self, id: &str) -> anyhow::Result<()> {
        self.empty(self.request(Method::DELETE, &format!("/api/session/{id}"))).await
    }

    pub async fn session_update(&self, id: &str, body: Value) -> anyhow::Result<()> {
        self.empty(self.request(Method::PATCH, &format!("/api/session/{id}")).json(&body)).await
    }

    pub async fn session_fork(&self, id: &str, before: Option<&str>) -> anyhow::Result<SessionInfo> {
        let env: DataEnvelope<SessionInfo> = self
            .json(self.request(Method::POST, &format!("/api/session/{id}/fork")).json(&json!({ "before": before })))
            .await?;
        Ok(env.data)
    }

    pub async fn switch_agent(&self, id: &str, agent: &str) -> anyhow::Result<()> {
        self.empty(self.request(Method::POST, &format!("/api/session/{id}/agent")).json(&json!({ "agent": agent }))).await
    }

    pub async fn switch_model(&self, id: &str, model: &ModelRef) -> anyhow::Result<()> {
        self.empty(self.request(Method::POST, &format!("/api/session/{id}/model")).json(&json!({ "model": model }))).await
    }

    pub async fn session_move(&self, id: &str, directory: &str) -> anyhow::Result<()> {
        self.empty(self.request(Method::POST, &format!("/api/session/{id}/move")).json(&json!({ "directory": directory })))
            .await
    }

    pub async fn prompt(&self, id: &str, body: Value) -> anyhow::Result<InboxItem> {
        let env: DataEnvelope<InboxItem> =
            self.json(self.request(Method::POST, &format!("/api/session/{id}/prompt")).json(&body)).await?;
        Ok(env.data)
    }

    pub async fn command(&self, id: &str, body: Value) -> anyhow::Result<()> {
        self.empty(self.request(Method::POST, &format!("/api/session/{id}/command")).json(&body)).await
    }

    pub async fn synthetic(&self, id: &str, body: Value) -> anyhow::Result<()> {
        self.empty(self.request(Method::POST, &format!("/api/session/{id}/synthetic")).json(&body)).await
    }

    pub async fn shell(&self, id: &str, command: &str) -> anyhow::Result<()> {
        self.empty(self.request(Method::POST, &format!("/api/session/{id}/shell")).json(&json!({ "command": command })))
            .await
    }

    pub async fn compact(&self, id: &str, msg_id: &str) -> anyhow::Result<InboxItem> {
        let env: DataEnvelope<InboxItem> = self
            .json(self.request(Method::POST, &format!("/api/session/{id}/compact")).json(&json!({ "id": msg_id })))
            .await?;
        Ok(env.data)
    }

    pub async fn wait(&self, id: &str) -> anyhow::Result<()> {
        self.empty(self.request(Method::POST, &format!("/api/experimental/session/{id}/wait"))).await
    }

    pub async fn revert_stage(&self, id: &str, message_id: &str) -> anyhow::Result<Revert> {
        let env: DataEnvelope<Revert> = self
            .json(
                self.request(Method::POST, &format!("/api/session/{id}/revert/stage"))
                    .json(&json!({ "messageID": message_id })),
            )
            .await?;
        Ok(env.data)
    }

    pub async fn revert_clear(&self, id: &str) -> anyhow::Result<()> {
        self.empty(self.request(Method::DELETE, &format!("/api/session/{id}/revert"))).await
    }

    pub async fn revert_commit(&self, id: &str) -> anyhow::Result<()> {
        self.empty(self.request(Method::POST, &format!("/api/session/{id}/revert/commit"))).await
    }

    pub async fn inbox(&self, id: &str) -> anyhow::Result<Vec<InboxItem>> {
        let env: DataEnvelope<Vec<InboxItem>> = self.json(self.request(Method::GET, &format!("/api/session/{id}/inbox"))).await?;
        Ok(env.data)
    }

    pub async fn inbox_cancel(&self, id: &str, inbox: &str) -> anyhow::Result<()> {
        self.empty(self.request(Method::DELETE, &format!("/api/session/{id}/inbox/{inbox}"))).await
    }

    pub async fn inbox_update(&self, id: &str, inbox: &str, delivery: &str) -> anyhow::Result<()> {
        self.empty(
            self.request(Method::PATCH, &format!("/api/session/{id}/inbox/{inbox}")).json(&json!({ "delivery": delivery })),
        )
        .await
    }

    pub async fn generate(&self, id: &str, prompt: &str) -> anyhow::Result<String> {
        let env: DataEnvelope<Value> = self
            .json(self.request(Method::POST, &format!("/api/session/{id}/generate")).json(&json!({ "prompt": prompt })))
            .await?;
        Ok(env.data.get("text").and_then(|t| t.as_str()).unwrap_or_default().to_string())
    }

    pub async fn interrupt(&self, id: &str, resume: bool) -> anyhow::Result<bool> {
        let v: Value = self
            .json(
                self.request(Method::POST, &format!("/api/session/{id}/interrupt"))
                    .query(&[("resume", if resume { "true" } else { "false" })]),
            )
            .await?;
        Ok(v.get("interrupted").and_then(|v| v.as_bool()).unwrap_or(false))
    }

    pub async fn background(&self, id: &str) -> anyhow::Result<()> {
        self.empty(self.request(Method::POST, &format!("/api/session/{id}/background"))).await
    }

    pub async fn message_get(&self, id: &str, message: &str) -> anyhow::Result<Value> {
        let env: DataEnvelope<Value> =
            self.json(self.request(Method::GET, &format!("/api/session/{id}/message/{message}"))).await?;
        Ok(env.data)
    }

    pub async fn messages(&self, id: &str, limit: u32, cursor: Option<&str>) -> anyhow::Result<Page<Value>> {
        let mut r = self.request(Method::GET, &format!("/api/session/{id}/message")).query(&[("limit", limit.to_string())]);
        r = match cursor {
            Some(c) => r.query(&[("cursor", c)]),
            None => r.query(&[("order", "desc")]),
        };
        self.json(r).await
    }

    pub async fn session_permissions(&self, id: &str) -> anyhow::Result<Vec<PermissionRequest>> {
        let env: DataEnvelope<Vec<PermissionRequest>> =
            self.json(self.request(Method::GET, &format!("/api/session/{id}/permission"))).await?;
        Ok(env.data)
    }

    pub async fn permission_reply(&self, session: &str, request: &str, decision: &str, message: Option<&str>) -> anyhow::Result<()> {
        let body = match message {
            Some(m) => json!({ "decision": decision, "message": m }),
            None => json!({ "decision": decision }),
        };
        self.empty(self.request(Method::POST, &format!("/api/session/{session}/permission/{request}/reply")).json(&body))
            .await
    }

    pub async fn session_forms(&self, id: &str) -> anyhow::Result<Vec<FormInfo>> {
        let env: DataEnvelope<Vec<FormInfo>> = self.json(self.request(Method::GET, &format!("/api/session/{id}/form"))).await?;
        Ok(env.data)
    }

    pub async fn form_reply(&self, session: &str, form: &str, answer: Value, dir: Option<&str>) -> anyhow::Result<()> {
        let mut r = self.request(Method::POST, &format!("/api/session/{session}/form/{form}/reply")).json(&json!({ "answer": answer }));
        if session == "global"
            && let Some(d) = dir
        {
            r = r.header("x-opencode-directory", urlencode(d));
        }
        self.empty(r).await
    }

    pub async fn form_cancel(&self, session: &str, form: &str, dir: Option<&str>) -> anyhow::Result<()> {
        let mut r = self.request(Method::DELETE, &format!("/api/session/{session}/form/{form}"));
        if session == "global"
            && let Some(d) = dir
        {
            r = r.header("x-opencode-directory", urlencode(d));
        }
        self.empty(r).await
    }

    pub async fn environment(&self, id: &str, variables: Value) -> anyhow::Result<()> {
        self.empty(self.request(Method::PUT, &format!("/api/session/{id}/environment")).json(&json!({ "variables": variables })))
            .await
    }

    pub async fn view(&self, id: &str, idle: i64) -> anyhow::Result<()> {
        self.empty(self.request(Method::POST, &format!("/api/session/{id}/view")).json(&json!({ "idle": idle }))).await
    }

    pub async fn export(&self, id: &str, sanitize: bool) -> anyhow::Result<Value> {
        let env: DataEnvelope<Value> = self
            .json(
                self.request(Method::GET, &format!("/api/experimental/session/{id}/export"))
                    .query(&[("sanitize", if sanitize { "true" } else { "false" })]),
            )
            .await?;
        Ok(env.data)
    }

    pub async fn shell_output(&self, shell: &str, cursor: i64, dir: Option<&str>) -> anyhow::Result<ShellOutput> {
        let r = self
            .loc(self.request(Method::GET, &format!("/api/shell/{shell}/output")), dir)
            .query(&[("cursor", cursor.to_string()), ("limit", (1024 * 1024).to_string())]);
        let env: LocationEnvelope<ShellOutput> = self.json(r).await?;
        Ok(env.data)
    }

    pub async fn shell_list(&self, dir: Option<&str>) -> anyhow::Result<Vec<ShellInfo>> {
        self.list("/api/shell", dir).await
    }

    pub async fn shell_remove(&self, shell: &str, dir: Option<&str>) -> anyhow::Result<()> {
        self.empty(self.loc(self.request(Method::DELETE, &format!("/api/shell/{shell}")), dir)).await
    }

    pub async fn vcs_base(&self, dir: Option<&str>) -> anyhow::Result<Option<Value>> {
        let env: LocationEnvelope<Option<Value>> = self.json(self.loc(self.request(Method::GET, "/api/vcs/base"), dir)).await?;
        Ok(env.data)
    }

    pub async fn vcs_diff(&self, mode: &str, base: Option<&str>, context: u32, dir: Option<&str>) -> anyhow::Result<Vec<FileDiff>> {
        let mut r = self.loc(self.request(Method::GET, "/api/vcs/diff"), dir).query(&[("mode", mode), ("context", &context.to_string())]);
        if let Some(b) = base {
            r = r.query(&[("base", b)]);
        }
        let env: LocationEnvelope<Vec<FileDiff>> = self.json(r).await?;
        Ok(env.data)
    }

    pub async fn vcs_branches(&self, search: &str, limit: u32, dir: Option<&str>) -> anyhow::Result<Vec<String>> {
        let r = self.loc(self.request(Method::GET, "/api/vcs/branch"), dir).query(&[("search", search), ("limit", &limit.to_string())]);
        let env: LocationEnvelope<Vec<String>> = self.json(r).await?;
        Ok(env.data)
    }

    pub async fn session_diff(&self, id: &str, context: u32) -> anyhow::Result<Vec<FileDiff>> {
        let env: DataEnvelope<Vec<FileDiff>> = self
            .json(self.request(Method::GET, &format!("/api/session/{id}/diff")).query(&[("context", context.to_string())]))
            .await?;
        Ok(env.data)
    }

    pub async fn session_stats(&self, from: i64, timezone: &str) -> anyhow::Result<Value> {
        let env: DataEnvelope<Value> = self
            .json(self.request(Method::GET, "/api/experimental/session/stats").query(&[("from", from.to_string()), ("timezone", timezone.to_string()), ("tools", "none".to_string())]))
            .await?;
        Ok(env.data)
    }

    pub async fn worktrees(&self, project: &str) -> anyhow::Result<Vec<Value>> {
        self.json(self.request(Method::GET, "/api/worktree").query(&[("projectID", project)])).await
    }

    pub async fn worktree_create(&self, project: &str, name: Option<&str>) -> anyhow::Result<Value> {
        let mut body = json!({ "projectID": project });
        if let Some(n) = name {
            body["name"] = json!(n);
        }
        self.json(self.request(Method::POST, "/api/worktree").json(&body)).await
    }

    pub async fn worktree_remove(&self, project: &str, directory: &str, force: bool) -> anyhow::Result<()> {
        self.empty(self.request(Method::DELETE, "/api/worktree").json(&json!({ "projectID": project, "directory": directory, "force": force }))).await
    }

    pub async fn worktree_refresh(&self, project: &str) -> anyhow::Result<()> {
        self.empty(self.request(Method::POST, "/api/worktree/refresh").json(&json!({ "projectID": project }))).await
    }

    pub async fn projects(&self) -> anyhow::Result<Vec<Value>> {
        let v: Value = self.json(self.request(Method::GET, "/api/project")).await?;
        Ok(match v {
            Value::Array(a) => a,
            other => other.get("data").and_then(|d| d.as_array()).cloned().unwrap_or_default(),
        })
    }

    pub async fn pair(&self) -> anyhow::Result<Value> {
        self.json(self.request(Method::POST, "/api/pair")).await
    }

    pub async fn pty_create(&self, body: Value, dir: Option<&str>) -> anyhow::Result<Value> {
        let env: LocationEnvelope<Value> = self.json(self.loc(self.request(Method::POST, "/api/pty"), dir).json(&body)).await?;
        Ok(env.data)
    }

    pub async fn pty_update(&self, id: &str, body: Value, dir: Option<&str>) -> anyhow::Result<()> {
        self.send(self.loc(self.request(Method::PUT, &format!("/api/pty/{id}")), dir).json(&body)).await?;
        Ok(())
    }

    pub async fn pty_remove(&self, id: &str, dir: Option<&str>) -> anyhow::Result<()> {
        self.empty(self.loc(self.request(Method::DELETE, &format!("/api/pty/{id}")), dir)).await
    }

    pub async fn pty_ticket(&self, id: &str, dir: Option<&str>) -> anyhow::Result<String> {
        let env: LocationEnvelope<Value> = self.json(self.loc(self.request(Method::POST, &format!("/api/pty/{id}/connect-token")), dir)).await?;
        Ok(env.data.get("ticket").and_then(|t| t.as_str()).unwrap_or_default().to_string())
    }

    pub async fn terminals(&self, session: &str) -> anyhow::Result<Vec<Value>> {
        let env: DataEnvelope<Vec<Value>> = self.json(self.request(Method::GET, &format!("/api/experimental/session/{session}/terminal"))).await?;
        Ok(env.data)
    }

    pub async fn terminal_create(&self, session: &str, cwd: Option<&str>) -> anyhow::Result<Value> {
        let mut body = json!({ "args": [], "title": "Terminal", "env": {} });
        if let Some(c) = cwd {
            body["cwd"] = json!(c);
        }
        let env: DataEnvelope<Value> = self.json(self.request(Method::POST, &format!("/api/experimental/session/{session}/terminal")).json(&body)).await?;
        Ok(env.data)
    }

    pub async fn terminal_remove(&self, pty: &str) -> anyhow::Result<()> {
        self.empty(self.request(Method::DELETE, &format!("/api/experimental/persistent-pty/{pty}"))).await
    }

    pub async fn terminal_snapshot(&self, pty: &str) -> anyhow::Result<Value> {
        let env: DataEnvelope<Value> = self.json(self.request(Method::GET, &format!("/api/experimental/persistent-pty/{pty}/snapshot"))).await?;
        Ok(env.data)
    }

    pub async fn terminal_ticket(&self, pty: &str) -> anyhow::Result<String> {
        let v: Value = self
            .json(self.request(Method::POST, &format!("/api/experimental/persistent-pty/{pty}/connect-token")).header("x-opencode-ticket", "1"))
            .await?;
        let data = v.get("data").cloned().unwrap_or(v);
        Ok(data.get("ticket").and_then(|t| t.as_str()).unwrap_or_default().to_string())
    }

    /// WebSocket URL for a persistent PTY attachment.
    pub fn terminal_url(&self, pty: &str, ticket: &str, cursor: i64, attachment: &str) -> String {
        let base = self.base.replacen("https://", "wss://", 1).replacen("http://", "ws://", 1);
        format!(
            "{base}/api/experimental/persistent-pty/{}/connect?ticket={}&cursor={cursor}&attachment_id={}&takeover=true&input_protocol=1",
            urlencode(pty),
            urlencode(ticket),
            urlencode(attachment)
        )
    }

    /// Open the raw SSE response for `GET /api/event`.
    pub async fn event_stream(&self) -> anyhow::Result<reqwest::Response> {
        let r = self.request(Method::GET, "/api/event").header("accept", "text/event-stream");
        self.send(r).await
    }
}

pub fn urlencode(s: &str) -> String {
    let mut out = String::new();
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => out.push(b as char),
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

/// `errorMessage(err)`: the human message of an error.
pub fn error_message(e: &anyhow::Error) -> String {
    if let Some(api) = e.downcast_ref::<ApiError>() {
        return api.message.clone();
    }
    e.to_string()
}

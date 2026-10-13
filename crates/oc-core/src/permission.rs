//! Permissions (`core/src/permission.ts`, `core/src/permission/saved.ts`).

use std::collections::HashMap;
use std::sync::Arc;

use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::sync::oneshot;

use oc_schema::types::{Effect, PermissionRule, Ruleset};

use crate::wildcard;

pub fn evaluate(action: &str, resource: &str, rules: &[PermissionRule]) -> PermissionRule {
    rules
        .iter()
        .rev()
        .find(|r| wildcard::matches(action, &r.action) && wildcard::matches(resource, &r.resource))
        .cloned()
        .unwrap_or(PermissionRule { action: action.into(), resource: "*".into(), effect: Effect::Ask })
}

pub fn merge(sets: &[&[PermissionRule]]) -> Ruleset {
    sets.iter().flat_map(|s| s.iter().cloned()).collect()
}

pub fn missing_agent_permissions() -> Ruleset {
    vec![PermissionRule { action: "*".into(), resource: "*".into(), effect: Effect::Deny }]
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Source {
    #[serde(rename = "type")]
    pub kind: String,
    #[serde(rename = "messageID")]
    pub message_id: String,
    pub id: String,
}

/// `Permission.Request` (ephemeral).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Request {
    pub id: String,
    #[serde(rename = "sessionID")]
    pub session_id: String,
    pub action: String,
    pub resources: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub save: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub metadata: Option<serde_json::Map<String, Value>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source: Option<Source>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Reply {
    Once,
    Always,
    Reject,
}

#[derive(Debug, Clone, thiserror::Error)]
pub enum PermissionError {
    #[error("{}", reason.clone().unwrap_or_else(|| format!("Permission denied: {permission}")))]
    Blocked { rules: Ruleset, permission: String, resources: Vec<String>, reason: Option<String> },
    #[error("{feedback}")]
    Corrected { feedback: String },
    #[error("The user declined this tool call")]
    Declined,
}

#[derive(Debug, Clone)]
pub struct AssertInput {
    pub id: Option<String>,
    pub session_id: String,
    pub action: String,
    pub resources: Vec<String>,
    pub save: Option<Vec<String>>,
    pub metadata: Option<serde_json::Map<String, Value>>,
    pub source: Option<Source>,
    pub agent: Option<String>,
}

/// What the permission service needs from the rest of core.
pub trait PermissionContext: Send + Sync {
    /// Agent + session rules (`configured`), or `None` if the session does not exist.
    fn configured(&self, session_id: &str, agent: Option<&str>) -> Option<Ruleset>;
    /// Saved "always" rules for the project of `session_id`.
    fn saved(&self, session_id: &str) -> Vec<PermissionRule>;
    /// Save "always" rules for the project of `session_id`.
    fn save(&self, session_id: &str, action: &str, resources: &[String]);
    fn publish(&self, kind: &str, data: Value);
    /// `permission.evaluate` plugin hook: returns the (possibly mutated) event.
    fn evaluate_hook(&self, _session_id: &str, event: Value) -> futures::future::BoxFuture<'static, Value> {
        Box::pin(async move { event })
    }
}

struct Pending {
    request: Request,
    agent: Option<String>,
    tx: oneshot::Sender<Result<(), PermissionError>>,
}

pub struct Permissions {
    ctx: Arc<dyn PermissionContext>,
    pending: Mutex<indexmap::IndexMap<String, Pending>>,
    closed: Mutex<bool>,
}

pub enum Evaluated {
    Deny(Ruleset),
    Allow(Ruleset),
    Ask(Ruleset),
}

impl Permissions {
    pub fn new(ctx: Arc<dyn PermissionContext>) -> Self {
        Permissions { ctx, pending: Mutex::new(Default::default()), closed: Mutex::new(false) }
    }

    fn evaluate_input(&self, input: &AssertInput) -> Option<Evaluated> {
        let rules = self.ctx.configured(&input.session_id, input.agent.as_deref())?;
        if input.resources.iter().any(|r| evaluate(&input.action, r, &rules).effect == Effect::Deny) {
            return Some(Evaluated::Deny(rules));
        }
        let mut all = rules;
        all.extend(self.ctx.saved(&input.session_id));
        let ask = input.resources.iter().any(|r| evaluate(&input.action, r, &all).effect == Effect::Ask);
        Some(if ask { Evaluated::Ask(all) } else { Evaluated::Allow(all) })
    }

    /// `evaluateInput` including the `permission.evaluate` hook (not run for config denies).
    async fn evaluate_hooked(&self, input: &AssertInput) -> Option<(Evaluated, Option<String>)> {
        let evaluated = self.evaluate_input(input)?;
        let (effect, rules) = match evaluated {
            Evaluated::Deny(rules) => return Some((Evaluated::Deny(rules), None)),
            Evaluated::Allow(rules) => ("allow", rules),
            Evaluated::Ask(rules) => ("ask", rules),
        };
        let mut event = serde_json::Map::new();
        event.insert("sessionID".into(), Value::String(input.session_id.clone()));
        if let Some(agent) = &input.agent {
            event.insert("agent".into(), Value::String(agent.clone()));
        }
        event.insert("action".into(), Value::String(input.action.clone()));
        event.insert("resources".into(), serde_json::to_value(&input.resources).unwrap_or_default());
        if let Some(metadata) = &input.metadata {
            event.insert("metadata".into(), Value::Object(metadata.clone()));
        }
        if let Some(source) = &input.source {
            event.insert("source".into(), serde_json::to_value(source).unwrap_or_default());
        }
        event.insert("effect".into(), Value::String(effect.into()));
        let out = self.ctx.evaluate_hook(&input.session_id, Value::Object(event)).await;
        let message = out.get("message").and_then(Value::as_str).map(String::from);
        let evaluated = match out.get("effect").and_then(Value::as_str).unwrap_or(effect) {
            "deny" => Evaluated::Deny(rules),
            "allow" => Evaluated::Allow(rules),
            _ => Evaluated::Ask(rules),
        };
        Some((evaluated, message))
    }

    fn request(input: &AssertInput) -> Request {
        Request {
            id: input.id.clone().unwrap_or_else(oc_schema::id::permission),
            session_id: input.session_id.clone(),
            action: input.action.clone(),
            resources: input.resources.clone(),
            save: input.save.clone(),
            metadata: input.metadata.clone(),
            source: input.source.clone(),
            message: None,
        }
    }

    fn create(&self, request: Request, agent: Option<String>) -> oneshot::Receiver<Result<(), PermissionError>> {
        let (tx, rx) = oneshot::channel();
        if *self.closed.lock() {
            let _ = tx.send(Err(PermissionError::Declined));
            return rx;
        }
        self.ctx.publish("permission.asked", serde_json::to_value(&request).unwrap());
        self.pending.lock().insert(request.id.clone(), Pending { request, agent, tx });
        rx
    }

    /// `assert`: Ok when allowed, waits for a reply when `ask`.
    pub async fn assert(&self, input: AssertInput) -> Result<(), PermissionError> {
        if *self.closed.lock() {
            return Err(PermissionError::Declined);
        }
        let Some((result, message)) = self.evaluate_hooked(&input).await else {
            return Err(PermissionError::Blocked {
                rules: vec![],
                permission: input.action.clone(),
                resources: input.resources.clone(),
                reason: Some(format!("Session not found: {}", input.session_id)),
            });
        };
        match result {
            Evaluated::Deny(rules) => Err(PermissionError::Blocked {
                rules: rules.into_iter().filter(|r| wildcard::matches(&input.action, &r.action)).collect(),
                permission: input.action.clone(),
                resources: input.resources.clone(),
                reason: message,
            }),
            Evaluated::Allow(_) => Ok(()),
            Evaluated::Ask(_) => {
                let mut request = Self::request(&input);
                request.message = message;
                let id = request.id.clone();
                let rx = self.create(request, input.agent.clone());
                let guard = PendingGuard { owner: self, id };
                let result = rx.await.unwrap_or(Err(PermissionError::Declined));
                drop(guard);
                result
            }
        }
    }

    /// `ask`: returns the request id and effect without waiting.
    pub async fn ask(&self, input: AssertInput) -> (String, Effect) {
        let mut request = Self::request(&input);
        let id = request.id.clone();
        if *self.closed.lock() {
            return (id, Effect::Deny);
        }
        let evaluated = self.evaluate_hooked(&input).await;
        if let Some((_, message)) = &evaluated {
            request.message = message.clone();
        }
        match evaluated.map(|(e, _)| e) {
            Some(Evaluated::Ask(_)) => {
                let _ = self.create(request, input.agent.clone());
                (id, Effect::Ask)
            }
            Some(Evaluated::Allow(_)) => (id, Effect::Allow),
            _ => (id, Effect::Deny),
        }
    }

    pub fn reply(&self, request_id: &str, reply: Reply, message: Option<String>) -> Result<(), String> {
        let mut pending = self.pending.lock();
        let Some(existing) = pending.shift_remove(request_id) else {
            return Err(request_id.to_string());
        };
        let publish = |sid: &str, rid: &str, r: &str| {
            self.ctx.publish("permission.replied", serde_json::json!({"sessionID": sid, "requestID": rid, "reply": r}))
        };
        publish(&existing.request.session_id, &existing.request.id, reply_str(reply));
        let fail = || match &message {
            Some(m) => PermissionError::Corrected { feedback: m.clone() },
            None => PermissionError::Declined,
        };
        if reply == Reply::Reject {
            let _ = existing.tx.send(Err(fail()));
            let same: Vec<String> = pending
                .iter()
                .filter(|(_, p)| p.request.session_id == existing.request.session_id)
                .map(|(k, _)| k.clone())
                .collect();
            for id in same {
                if let Some(item) = pending.shift_remove(&id) {
                    publish(&item.request.session_id, &item.request.id, "reject");
                    let _ = item.tx.send(Err(fail()));
                }
            }
            return Ok(());
        }
        let save = existing.request.save.clone().unwrap_or_default();
        if reply == Reply::Always && !save.is_empty() {
            self.ctx.save(&existing.request.session_id, &existing.request.action, &save);
        }
        let _ = existing.tx.send(Ok(()));
        if reply != Reply::Always || save.is_empty() {
            return Ok(());
        }
        let ids: Vec<String> = pending.keys().cloned().collect();
        for id in ids {
            let item = &pending[&id];
            let input = AssertInput {
                id: Some(item.request.id.clone()),
                session_id: item.request.session_id.clone(),
                action: item.request.action.clone(),
                resources: item.request.resources.clone(),
                save: item.request.save.clone(),
                metadata: None,
                source: None,
                agent: item.agent.clone(),
            };
            if matches!(self.evaluate_input(&input), Some(Evaluated::Allow(_))) {
                let item = pending.shift_remove(&id).unwrap();
                publish(&item.request.session_id, &item.request.id, "always");
                let _ = item.tx.send(Ok(()));
            }
        }
        Ok(())
    }

    pub fn list(&self) -> Vec<Request> {
        self.pending.lock().values().map(|p| p.request.clone()).collect()
    }

    pub fn for_session(&self, session_id: &str) -> Vec<Request> {
        self.pending.lock().values().filter(|p| p.request.session_id == session_id).map(|p| p.request.clone()).collect()
    }

    pub fn get(&self, id: &str) -> Option<Request> {
        self.pending.lock().get(id).map(|p| p.request.clone())
    }

    pub fn close(&self) {
        *self.closed.lock() = true;
        self.reject_where(|_| true);
    }

    /// Reject pending requests of the matching sessions (a location scope closing).
    pub fn reject_where(&self, mut matches: impl FnMut(&str) -> bool) {
        let drained: Vec<Pending> = {
            let mut pending = self.pending.lock();
            let ids: Vec<String> = pending.iter().filter(|(_, p)| matches(&p.request.session_id)).map(|(k, _)| k.clone()).collect();
            ids.iter().filter_map(|id| pending.shift_remove(id)).collect()
        };
        for item in drained {
            self.ctx.publish(
                "permission.replied",
                serde_json::json!({"sessionID": item.request.session_id, "requestID": item.request.id, "reply": "reject"}),
            );
            let _ = item.tx.send(Err(PermissionError::Declined));
        }
    }
}

struct PendingGuard<'a> {
    owner: &'a Permissions,
    id: String,
}
impl Drop for PendingGuard<'_> {
    fn drop(&mut self) {
        self.owner.pending.lock().shift_remove(&self.id);
    }
}

fn reply_str(r: Reply) -> &'static str {
    match r {
        Reply::Once => "once",
        Reply::Always => "always",
        Reply::Reject => "reject",
    }
}

#[allow(dead_code)]
type _Unused = HashMap<(), ()>;

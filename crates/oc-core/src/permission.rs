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

    /// `reply`. After an "always" reply with rules to save, every other pending request is
    /// re-evaluated like a new ask, including the `permission.evaluate` hook, and the ones now
    /// allowed are approved (`core/src/permission.ts` `reply`).
    pub async fn reply(&self, request_id: &str, reply: Reply, message: Option<String>) -> Result<(), String> {
        let publish = |sid: &str, rid: &str, r: &str| {
            self.ctx.publish("permission.replied", serde_json::json!({"sessionID": sid, "requestID": rid, "reply": r}))
        };
        let candidates: Vec<(String, AssertInput)> = {
            let mut pending = self.pending.lock();
            let Some(existing) = pending.shift_remove(request_id) else {
                return Err(request_id.to_string());
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
            pending
                .iter()
                .map(|(id, item)| {
                    (
                        id.clone(),
                        AssertInput {
                            id: Some(item.request.id.clone()),
                            session_id: item.request.session_id.clone(),
                            action: item.request.action.clone(),
                            resources: item.request.resources.clone(),
                            save: item.request.save.clone(),
                            metadata: item.request.metadata.clone(),
                            source: item.request.source.clone(),
                            agent: item.agent.clone(),
                        },
                    )
                })
                .collect()
        };
        for (id, input) in candidates {
            // A missing session is skipped (upstream catches `Session.NotFoundError`).
            if !matches!(self.evaluate_hooked(&input).await, Some((Evaluated::Allow(_), _))) {
                continue;
            }
            // Still pending? It may have been answered or abandoned while the hook ran.
            let Some(item) = self.pending.lock().shift_remove(&id) else { continue };
            publish(&item.request.session_id, &item.request.id, "always");
            let _ = item.tx.send(Ok(()));
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

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Default)]
    struct Ctx {
        saved: Mutex<Vec<PermissionRule>>,
        hooked: Arc<Mutex<Vec<Value>>>,
        replies: Mutex<Vec<(String, String)>>,
    }

    impl PermissionContext for Ctx {
        fn configured(&self, session_id: &str, _agent: Option<&str>) -> Option<Ruleset> {
            (session_id != "ses_gone").then(|| vec![PermissionRule { action: "*".into(), resource: "*".into(), effect: Effect::Ask }])
        }
        fn saved(&self, _session_id: &str) -> Vec<PermissionRule> {
            self.saved.lock().clone()
        }
        fn save(&self, _session_id: &str, action: &str, resources: &[String]) {
            self.saved.lock().extend(resources.iter().map(|r| PermissionRule { action: action.into(), resource: r.clone(), effect: Effect::Allow }));
        }
        fn publish(&self, kind: &str, data: Value) {
            if kind == "permission.replied" {
                self.replies.lock().push((data["requestID"].as_str().unwrap().into(), data["reply"].as_str().unwrap().into()));
            }
        }
        fn evaluate_hook(&self, _session_id: &str, mut event: Value) -> futures::future::BoxFuture<'static, Value> {
            let hooked = self.hooked.clone();
            Box::pin(async move {
                hooked.lock().push(event.clone());
                // A plugin keeps asking for `veto.env` even when a saved rule allows it.
                if event["resources"].as_array().is_some_and(|r| r.iter().any(|v| v == "veto.env")) {
                    event["effect"] = "ask".into();
                }
                event
            })
        }
    }

    fn input(id: &str, session: &str, resource: &str, save: bool) -> AssertInput {
        let mut metadata = serde_json::Map::new();
        metadata.insert("file".into(), resource.into());
        AssertInput {
            id: Some(id.into()),
            session_id: session.into(),
            action: "read".into(),
            resources: vec![resource.into()],
            save: save.then(|| vec!["*".into()]),
            metadata: Some(metadata),
            source: None,
            agent: Some("build".into()),
        }
    }

    #[tokio::test]
    async fn always_recheck_runs_the_evaluate_hook() {
        let ctx = Arc::new(Ctx::default());
        let perms = Arc::new(Permissions::new(ctx.clone()));
        let mut waits = vec![];
        for (id, session, resource, save) in
            [("per_a", "ses_1", "a.env", true), ("per_b", "ses_2", "b.env", false), ("per_c", "ses_1", "veto.env", false), ("per_d", "ses_gone", "d.env", false)]
        {
            if session == "ses_gone" {
                // A pending request whose session disappeared is skipped by the re-check.
                let request = Permissions::request(&input(id, session, resource, save));
                waits.push(perms.create(request, Some("build".into())));
                continue;
            }
            let p = perms.clone();
            let i = input(id, session, resource, save);
            let handle = tokio::spawn(async move { p.assert(i).await });
            tokio::task::yield_now().await;
            while !perms.list().iter().any(|r| r.id == id) {
                tokio::task::yield_now().await;
            }
            let (tx, rx) = oneshot::channel();
            tokio::spawn(async move {
                let _ = tx.send(handle.await.unwrap());
            });
            waits.push(rx);
        }
        ctx.hooked.lock().clear();
        perms.reply("per_a", Reply::Always, None).await.unwrap();
        let remaining: Vec<String> = perms.list().into_iter().map(|r| r.id).collect();
        assert_eq!(remaining, vec!["per_c", "per_d"]);
        assert_eq!(*ctx.replies.lock(), vec![("per_a".into(), "always".into()), ("per_b".into(), "always".into())]);
        // The hook saw the re-checked requests with their stored metadata (not the missing session's).
        let hooked = ctx.hooked.lock().clone();
        assert_eq!(hooked.len(), 2);
        assert_eq!(hooked[0]["resources"], serde_json::json!(["b.env"]));
        assert_eq!(hooked[0]["metadata"], serde_json::json!({"file": "b.env"}));
        assert_eq!(hooked[0]["effect"], "allow");
        assert_eq!(hooked[1]["resources"], serde_json::json!(["veto.env"]));
        let mut waits = waits.into_iter();
        assert!(waits.next().unwrap().await.unwrap().is_ok());
        assert!(waits.next().unwrap().await.unwrap().is_ok());
    }
}

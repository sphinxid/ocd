//! Session export/import (`core/session/transfer.ts`).

use rusqlite::params;
use serde_json::{json, Map, Value};

use oc_schema::event::SessionEvent;
use oc_schema::message::CompactionStatus;
use oc_schema::types::{LocationRef, SessionInfo};
use oc_schema::Message;

use crate::bus::{Bus, Durable1};
use crate::session::{SessionError, Sessions};

fn is_settled(m: &Message) -> bool {
    match m {
        Message::Assistant(a) => a.time.completed.is_some(),
        Message::Shell { status, .. } => *status != oc_schema::message::ShellStatus::Running,
        Message::Compaction(c) => !matches!(c.status, CompactionStatus::Running { .. }),
        _ => true,
    }
}

pub fn export(bus: &Bus, sessions: &Sessions, session_id: &str, sanitize: bool) -> Result<Value, SessionError> {
    let info = sessions.get(session_id)?;
    let messages: Vec<Value> = bus
        .db()
        .session_messages(session_id)?
        .into_iter()
        .map(|(_, m)| m)
        .filter(is_settled)
        .map(|m| serde_json::to_value(&m).unwrap())
        .collect();
    let mut data = json!({"info": info, "messages": messages});
    if sanitize {
        data = sanitize_data(data);
    }
    Ok(data)
}

fn redact(kind: &str, id: &str, v: &str) -> String {
    if v.trim().is_empty() { v.to_string() } else { format!("[redacted:{kind}:{id}]") }
}

fn redact_field(o: &mut Map<String, Value>, key: &str, kind: &str, id: &str) {
    if let Some(Value::String(s)) = o.get(key) {
        let r = redact(kind, id, s);
        o.insert(key.into(), Value::String(r));
    }
}

fn redact_meta(o: &mut Map<String, Value>, key: &str, kind: &str, id: &str) {
    if let Some(Value::Object(m)) = o.get(key) {
        if !m.is_empty() {
            o.insert(key.into(), json!({"redacted": format!("{kind}:{id}")}));
        }
    }
}

fn sanitize_data(mut data: Value) -> Value {
    let id = data["info"]["id"].as_str().unwrap_or_default().to_string();
    if let Some(info) = data["info"].as_object_mut() {
        redact_field(info, "title", "session-title", &id);
        redact_meta(info, "metadata", "session-metadata", &id);
        if let Some(loc) = info.get_mut("location").and_then(Value::as_object_mut) {
            if let Some(Value::String(d)) = loc.get("directory") {
                let r = format!("/{}", redact("session-directory", &id, d));
                loc.insert("directory".into(), Value::String(r));
            }
        }
        if let Some(files) = info.get_mut("revert").and_then(|r| r.get_mut("files")).and_then(Value::as_array_mut) {
            for (i, f) in files.iter_mut().enumerate() {
                if let Some(f) = f.as_object_mut() {
                    redact_field(f, "file", "revert-file", &i.to_string());
                    redact_field(f, "patch", "revert-patch", &i.to_string());
                }
            }
        }
    }
    if let Some(msgs) = data["messages"].as_array_mut() {
        for m in msgs.iter_mut() {
            sanitize_message(m);
        }
    }
    data
}

fn sanitize_tool_content(id: &str, c: &mut Value) {
    let Some(o) = c.as_object_mut() else { return };
    if o.get("type").and_then(Value::as_str) == Some("text") {
        redact_field(o, "text", "tool-output", id);
    } else {
        redact_field(o, "uri", "tool-file-uri", id);
        redact_field(o, "name", "tool-file-name", id);
    }
}

fn sanitize_message(m: &mut Value) {
    let id = m["id"].as_str().unwrap_or_default().to_string();
    let kind = m["type"].as_str().unwrap_or_default().to_string();
    let Some(o) = m.as_object_mut() else { return };
    redact_meta(o, "metadata", "message-metadata", &id);
    let indexed = |arr: Option<&mut Value>, f: &dyn Fn(&mut Map<String, Value>, String)| {
        if let Some(items) = arr.and_then(Value::as_array_mut) {
            for (i, item) in items.iter_mut().enumerate() {
                if let Some(io) = item.as_object_mut() {
                    f(io, i.to_string());
                }
            }
        }
    };
    let mention = |io: &mut Map<String, Value>, kind: &str, i: &str| {
        if let Some(mo) = io.get_mut("mention").and_then(Value::as_object_mut) {
            redact_field(mo, "text", kind, i);
        }
    };
    match kind.as_str() {
        "user" => {
            redact_field(o, "text", "text", &id);
            indexed(o.get_mut("files"), &|f, i| {
                f.insert("data".into(), json!(""));
                f.insert("source".into(), json!({"type": "inline"}));
                redact_field(f, "name", "file-name", &i);
                redact_field(f, "description", "file-description", &i);
                mention(f, "file-mention", &i);
            });
            indexed(o.get_mut("agents"), &|a, i| {
                redact_field(a, "name", "agent-name", &i);
                mention(a, "agent-mention", &i);
            });
            indexed(o.get_mut("skills"), &|s, i| {
                redact_field(s, "name", "skill-name", &i);
                redact_field(s, "text", "skill", &i);
                mention(s, "skill-mention", &i);
            });
        }
        "synthetic" => {
            redact_field(o, "text", "synthetic", &id);
            redact_field(o, "description", "synthetic-description", &id);
        }
        "system" => redact_field(o, "text", "system", &id),
        "skill" => redact_field(o, "text", "skill", &id),
        "shell" => {
            redact_field(o, "command", "shell-command", &id);
            if let Some(out) = o.get_mut("output").and_then(Value::as_object_mut) {
                redact_field(out, "output", "shell-output", &id);
            }
        }
        "assistant" => {
            if let Some(content) = o.get_mut("content").and_then(Value::as_array_mut) {
                for c in content.iter_mut() {
                    let Some(co) = c.as_object_mut() else { continue };
                    match co.get("type").and_then(Value::as_str) {
                        Some("text") => {
                            redact_field(co, "text", "text", &id);
                            if co.contains_key("state") {
                                co.insert("state".into(), json!({"redacted": format!("text-state:{id}")}));
                            }
                        }
                        Some("reasoning") => {
                            redact_field(co, "text", "reasoning", &id);
                            if co.contains_key("state") {
                                co.insert("state".into(), json!({"redacted": format!("reasoning-state:{id}")}));
                            }
                        }
                        _ => {
                            if co.contains_key("providerState") {
                                co.insert("providerState".into(), json!({"redacted": format!("tool-provider-state:{id}")}));
                            }
                            if co.contains_key("providerResultState") {
                                co.insert("providerResultState".into(), json!({"redacted": format!("tool-provider-result-state:{id}")}));
                            }
                            if let Some(st) = co.get_mut("state").and_then(Value::as_object_mut) {
                                match st.get("status").and_then(Value::as_str) {
                                    Some("streaming") => redact_field(st, "input", "tool-input", &id),
                                    Some("running") => {
                                        st.insert("input".into(), json!({"redacted": format!("tool-input:{id}")}));
                                        st.insert("metadata".into(), json!({"redacted": format!("tool-metadata:{id}")}));
                                    }
                                    _ => {
                                        st.insert("input".into(), json!({"redacted": format!("tool-input:{id}")}));
                                        if st.contains_key("metadata") {
                                            st.insert("metadata".into(), json!({"redacted": format!("tool-metadata:{id}")}));
                                        }
                                        if let Some(items) = st.get_mut("content").and_then(Value::as_array_mut) {
                                            for item in items.iter_mut() {
                                                sanitize_tool_content(&id, item);
                                            }
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
        "compaction" => {
            if o.get("status").and_then(Value::as_str) != Some("failed") {
                redact_field(o, "summary", "compaction-summary", &id);
                redact_field(o, "recent", "compaction-recent", &id);
                if o.get("status").and_then(Value::as_str) == Some("completed") {
                    redact_meta(o, "providerState", "compaction-provider-state", &id);
                }
            }
        }
        _ => {}
    }
}

#[derive(Debug, thiserror::Error)]
pub enum ImportError {
    #[error("Session already exists: {0}")]
    Conflict(String),
    #[error(transparent)]
    Session(#[from] SessionError),
    #[error(transparent)]
    Other(#[from] anyhow::Error),
}

/// `SessionTransfer.import`
pub async fn import(bus: &Bus, sessions: &Sessions, data: &Value, location: LocationRef) -> Result<SessionInfo, ImportError> {
    let info: SessionInfo = serde_json::from_value(data["info"].clone()).map_err(anyhow::Error::from)?;
    if bus.db().session_get(&info.id)?.is_some() {
        return Err(ImportError::Conflict(info.id.clone()));
    }
    if let Some(p) = &info.parent_id {
        sessions.get(p)?;
    }
    let project = crate::project::resolve_and_persist(bus, std::path::Path::new(&location.directory)).await?;
    let imported_at = oc_schema::id::now_ms() as i64;
    let messages: Vec<Message> = data["messages"]
        .as_array()
        .cloned()
        .unwrap_or_default()
        .into_iter()
        .filter_map(|m| serde_json::from_value::<Message>(m).ok())
        .filter(is_settled)
        .collect();
    let subpath = crate::session::relative(&project.directory, std::path::Path::new(&location.directory));
    let event = SessionEvent::Created {
        session_id: info.id.clone(),
        project_id: project.id.clone(),
        location: location.clone(),
        subpath: Some(subpath),
        parent_id: info.parent_id.clone(),
        slug: oc_storage::session::slug(),
        title: info.title.clone(),
        agent: info.agent.clone(),
        model: info.model.clone(),
        metadata: info.metadata.clone(),
        permissions: info.permissions.clone(),
        version: sessions.version.clone(),
    };
    let sid = info.id.clone();
    let result = bus.commit_with(vec![Durable1::from(event)], |c, _, seq| {
        for (i, m) in messages.iter().enumerate() {
            c.execute(
                "INSERT INTO session_message (id, session_id, type, seq, time_created, time_updated, data) VALUES (?, ?, ?, ?, ?, ?, ?)",
                params![m.id(), sid, m.kind(), (i + 1) as i64, m.created() as i64, oc_schema::id::now_ms() as i64, m.to_row_data()],
            )?;
        }
        if !messages.is_empty() {
            oc_storage::projector::reserve_seq(c, &sid, seq + messages.len() as i64)?;
        }
        let t = &info.time;
        let viewed = match (t.idle, t.viewed) {
            (Some(i), Some(v)) => Some(i.min(v) as i64),
            _ => None,
        };
        c.execute(
            "UPDATE session_v2 SET cost = ?, tokens_input = ?, tokens_output = ?, tokens_reasoning = ?, tokens_cache_read = ?, tokens_cache_write = ?, time_created = ?, time_updated = ?, time_idle = ?, time_viewed = ?, idle_outcome = ?, time_archived = ? WHERE id = ?",
            params![
                info.cost,
                info.tokens.input as i64,
                info.tokens.output as i64,
                info.tokens.reasoning as i64,
                info.tokens.cache.read as i64,
                info.tokens.cache.write as i64,
                t.created as i64,
                imported_at,
                t.idle.map(|v| v as i64),
                viewed,
                t.idle.and(info.outcome).map(|o| serde_json::to_value(o).unwrap().as_str().unwrap().to_string()),
                t.archived.map(|v| v as i64),
                sid
            ],
        )?;
        Ok(())
    });
    if let Err(e) = result {
        if e.to_string().contains("Session already projected") {
            return Err(ImportError::Conflict(info.id.clone()));
        }
        return Err(e.into());
    }
    Ok(sessions.get(&info.id)?)
}


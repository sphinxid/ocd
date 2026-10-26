//! Forms (`core/src/form.ts`): interactive questions/elicitation answered by clients.

use std::sync::Arc;
use std::time::{Duration, Instant};

use indexmap::IndexMap;
use parking_lot::Mutex;
use serde_json::{json, Map, Value};
use tokio::sync::watch;

use crate::bus::Bus;

const RETENTION: Duration = Duration::from_secs(600);

#[derive(Debug, Clone, PartialEq)]
pub enum State {
    Pending,
    Answered(Map<String, Value>),
    Cancelled(Option<String>),
}

impl State {
    pub fn to_json(&self) -> Value {
        match self {
            State::Pending => json!({"status": "pending"}),
            State::Answered(a) => json!({"status": "answered", "answer": a}),
            State::Cancelled(None) => json!({"status": "cancelled"}),
            State::Cancelled(Some(m)) => json!({"status": "cancelled", "message": m}),
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum FormError {
    #[error("Form not found: {0}")]
    NotFound(String),
    #[error("Form already settled: {0}")]
    AlreadySettled(String),
    #[error("Form already exists: {0}")]
    AlreadyExists(String),
    #[error("{message}")]
    InvalidAnswer { id: String, message: String },
    #[error("{0}")]
    InvalidForm(String),
}

struct Entry {
    form: Value,
    state: watch::Sender<State>,
    settled_at: Option<Instant>,
    /// The Location the form's events are published in, when its owner session has no row (MCP
    /// elicitations owned by the `"global"` sentinel session).
    location: Option<oc_schema::types::LocationRef>,
}

#[derive(Clone)]
pub struct Forms {
    bus: Bus,
    forms: Arc<Mutex<IndexMap<String, Entry>>>,
    closed: Arc<Mutex<bool>>,
}

fn s<'a>(v: &'a Value, k: &str) -> Option<&'a str> {
    v.get(k).and_then(Value::as_str)
}

fn options_have(field: &Value, value: &str) -> bool {
    field.get("options").and_then(Value::as_array).is_some_and(|o| o.iter().any(|x| s(x, "value") == Some(value)))
}

fn is_active(field: &Value, answer: &Map<String, Value>) -> bool {
    let Some(when) = field.get("when").and_then(Value::as_array) else { return true };
    when.iter().all(|w| {
        let Some(value) = s(w, "key").and_then(|k| answer.get(k)) else { return false };
        let target = &w["value"];
        let hit = match value {
            Value::Array(items) => items.iter().any(|i| i == target),
            other => other == target,
        };
        if s(w, "op") == Some("eq") { hit } else { !hit }
    })
}

pub fn validate_fields(fields: &[Value]) -> Option<String> {
    if fields.is_empty() {
        return Some("Form must have at least one field".into());
    }
    let mut earlier: IndexMap<String, &Value> = IndexMap::new();
    let mut keys = std::collections::HashSet::new();
    for field in fields {
        let key = s(field, "key").unwrap_or_default();
        if !keys.insert(key.to_string()) {
            return Some(format!("Duplicate form field key: {key}"));
        }
        if s(field, "type") == Some("external") {
            continue;
        }
        for w in field.get("when").and_then(Value::as_array).into_iter().flatten() {
            let wk = s(w, "key").unwrap_or_default();
            let Some(target) = earlier.get(wk) else {
                return Some(format!("Form field condition must reference an earlier field: {key} -> {wk}"));
            };
            let value = &w["value"];
            let invalid = match s(target, "type") {
                Some("boolean") => (!value.is_boolean()).then_some("Form field condition value must be a boolean"),
                Some("number") | Some("integer") => (!value.is_number()).then_some("Form field condition value must be a number"),
                t => {
                    if let Some(v) = value.as_str() {
                        let custom = target.get("custom").and_then(Value::as_bool) == Some(true);
                        let closed = if t == Some("multiselect") { !custom } else { target.get("options").is_some() && !custom };
                        (closed && !options_have(target, v)).then_some("Form field condition value must be one of the field's options")
                    } else {
                        Some("Form field condition value must be a string")
                    }
                }
            };
            if let Some(m) = invalid {
                return Some(format!("{m}: {key} -> {wk}"));
            }
        }
        earlier.insert(key.to_string(), field);
    }
    None
}

fn is_date(v: &str) -> bool {
    regex::Regex::new(r"^\d{4}-\d{2}-\d{2}$").unwrap().is_match(v) && chrono::NaiveDate::parse_from_str(v, "%Y-%m-%d").is_ok()
}

fn validate_field(field: &Value, value: &Value) -> Option<String> {
    let key = s(field, "key").unwrap_or_default();
    let required = field.get("required").and_then(Value::as_bool) == Some(true);
    let custom = field.get("custom").and_then(Value::as_bool) == Some(true);
    let num = |k: &str| field.get(k).and_then(Value::as_f64);
    match s(field, "type") {
        Some("string") => {
            let Some(v) = value.as_str() else { return Some(format!("Expected string for form field: {key}")) };
            let len = v.encode_utf16().count() as f64;
            if required && v.is_empty() {
                return Some(format!("Missing required form field: {key}"));
            }
            if num("minLength").is_some_and(|m| len < m) {
                return Some(format!("Form field is too short: {key}"));
            }
            if num("maxLength").is_some_and(|m| len > m) {
                return Some(format!("Form field is too long: {key}"));
            }
            if let Some(p) = s(field, "pattern") {
                match regex::Regex::new(p) {
                    Ok(re) if !re.is_match(v) => return Some(format!("Form field does not match pattern: {key}")),
                    Err(_) => return Some(format!("Form field has invalid pattern: {key}")),
                    _ => {}
                }
            }
            match s(field, "format") {
                Some("email") if !regex::Regex::new(r"^[^\s@]+@[^\s@]+\.[^\s@]+$").unwrap().is_match(v) => {
                    return Some(format!("Expected email for form field: {key}"))
                }
                Some("uri") if url::Url::parse(v).is_err() => return Some(format!("Expected URI for form field: {key}")),
                Some("date") if !is_date(v) => return Some(format!("Expected date for form field: {key}")),
                Some("date-time") if chrono::DateTime::parse_from_rfc3339(v).is_err() => {
                    return Some(format!("Expected date-time for form field: {key}"))
                }
                _ => {}
            }
            if field.get("options").is_some() && !custom && !options_have(field, v) {
                return Some(format!("Invalid option for form field: {key}"));
            }
            None
        }
        Some(t @ ("number" | "integer")) => {
            let Some(v) = value.as_f64().filter(|f| f.is_finite()) else { return Some(format!("Expected number for form field: {key}")) };
            if t == "integer" && v.fract() != 0.0 {
                return Some(format!("Expected integer for form field: {key}"));
            }
            if num("minimum").is_some_and(|m| v < m) {
                return Some(format!("Form field is too small: {key}"));
            }
            if num("maximum").is_some_and(|m| v > m) {
                return Some(format!("Form field is too large: {key}"));
            }
            None
        }
        Some("boolean") => (!value.is_boolean()).then(|| format!("Expected boolean for form field: {key}")),
        Some("multiselect") => {
            let Some(items) = value.as_array().filter(|a| a.iter().all(Value::is_string)) else {
                return Some(format!("Expected string array for form field: {key}"));
            };
            if required && items.is_empty() {
                return Some(format!("Missing required form field: {key}"));
            }
            if num("minItems").is_some_and(|m| (items.len() as f64) < m) {
                return Some(format!("Too few selections for form field: {key}"));
            }
            if num("maxItems").is_some_and(|m| (items.len() as f64) > m) {
                return Some(format!("Too many selections for form field: {key}"));
            }
            if !custom && items.iter().any(|i| !options_have(field, i.as_str().unwrap())) {
                return Some(format!("Invalid option for form field: {key}"));
            }
            None
        }
        _ => None,
    }
}

pub fn validate_answer(fields: &[Value], answer: &Map<String, Value>) -> Option<String> {
    for key in answer.keys() {
        if !fields.iter().any(|f| s(f, "key") == Some(key)) {
            return Some(format!("Unknown form field: {key}"));
        }
    }
    for field in fields {
        let key = s(field, "key").unwrap_or_default();
        let value = answer.get(key);
        if s(field, "type") == Some("external") {
            if value != Some(&Value::Bool(true)) {
                return Some(format!("External form field must be acknowledged: {key}"));
            }
            continue;
        }
        let active = is_active(field, answer);
        let Some(value) = value else {
            if field.get("required").and_then(Value::as_bool) == Some(true) && active {
                return Some(format!("Missing required form field: {key}"));
            }
            continue;
        };
        if !active {
            return Some(format!("Form field is not active: {key}"));
        }
        if let Some(m) = validate_field(field, value) {
            return Some(m);
        }
    }
    None
}

impl Forms {
    pub fn new(bus: Bus) -> Self {
        Forms { bus, forms: Arc::new(Mutex::new(IndexMap::new())), closed: Arc::new(Mutex::new(false)) }
    }

    fn sweep(&self) {
        self.forms.lock().retain(|_, e| e.settled_at.is_none_or(|t| t.elapsed() < RETENTION));
    }

    /// `create`: input `{id?, sessionID, title, metadata?, fields}`.
    pub fn create(&self, input: &Value) -> Result<Value, FormError> {
        self.create_at(input, None)
    }

    /// Publishes a form event in the form's Location, or the owner session's.
    fn publish(&self, location: Option<&oc_schema::types::LocationRef>, session: &str, kind: &str, data: Value) {
        match location {
            Some(location) => {
                self.bus.publish(kind, data, Some(location.clone()));
            }
            None => {
                self.bus.publish_session_ephemeral(session, kind, data);
            }
        }
    }

    /// `create` inside a Location (`location`): the form's events carry it, so clients that drop
    /// unlocated events still see forms whose owner session has no row.
    pub fn create_at(&self, input: &Value, location: Option<oc_schema::types::LocationRef>) -> Result<Value, FormError> {
        self.sweep();
        let id = s(input, "id").map(str::to_string).unwrap_or_else(oc_schema::id::form);
        if self.forms.lock().contains_key(&id) {
            return Err(FormError::AlreadyExists(id));
        }
        let fields: Vec<Value> = input.get("fields").and_then(Value::as_array).cloned().unwrap_or_default();
        if let Some(m) = validate_fields(&fields) {
            return Err(FormError::InvalidForm(m));
        }
        let mut form = Map::new();
        form.insert("id".into(), json!(id));
        form.insert("sessionID".into(), input["sessionID"].clone());
        form.insert("title".into(), input["title"].clone());
        if let Some(m) = input.get("metadata").filter(|m| !m.is_null()) {
            form.insert("metadata".into(), m.clone());
        }
        form.insert("fields".into(), Value::Array(fields));
        let form = Value::Object(form);
        self.forms.lock().insert(
            id.clone(),
            Entry { form: form.clone(), state: watch::channel(State::Pending).0, settled_at: None, location: location.clone() },
        );
        let session = s(&form, "sessionID").unwrap_or_default().to_string();
        self.publish(location.as_ref(), &session, "form.created", json!({"form": form}));
        if *self.closed.lock() {
            let _ = self.cancel(&id, None);
        }
        Ok(form)
    }

    /// `ask`: create and wait for a terminal state; dropping the future cancels the form.
    pub async fn ask(&self, input: &Value) -> Result<State, FormError> {
        self.ask_at(input, None).await
    }

    /// [`Forms::ask`] inside a Location (see [`Forms::create_at`]).
    pub async fn ask_at(&self, input: &Value, location: Option<oc_schema::types::LocationRef>) -> Result<State, FormError> {
        let form = self.create_at(input, location)?;
        let id = s(&form, "id").unwrap().to_string();
        let mut rx = self.forms.lock()[&id].state.subscribe();
        struct Guard<'a>(&'a Forms, String, bool);
        impl Drop for Guard<'_> {
            fn drop(&mut self) {
                if !self.2 {
                    let _ = self.0.cancel(&self.1, None);
                }
            }
        }
        let mut guard = Guard(self, id, false);
        loop {
            let st = rx.borrow().clone();
            if st != State::Pending {
                guard.2 = true;
                return Ok(st);
            }
            if rx.changed().await.is_err() {
                guard.2 = true;
                return Ok(State::Cancelled(None));
            }
        }
    }

    pub fn get(&self, id: &str) -> Result<Value, FormError> {
        self.forms.lock().get(id).map(|e| e.form.clone()).ok_or_else(|| FormError::NotFound(id.into()))
    }

    pub fn state(&self, id: &str) -> Result<State, FormError> {
        self.forms.lock().get(id).map(|e| e.state.borrow().clone()).ok_or_else(|| FormError::NotFound(id.into()))
    }

    pub fn list(&self, session: Option<&str>) -> Vec<Value> {
        self.forms
            .lock()
            .values()
            .filter(|e| *e.state.borrow() == State::Pending)
            .filter(|e| session.is_none_or(|s0| s(&e.form, "sessionID") == Some(s0)))
            .map(|e| e.form.clone())
            .collect()
    }

    pub fn reply(&self, id: &str, answer: Map<String, Value>) -> Result<(), FormError> {
        let mut forms = self.forms.lock();
        let entry = forms.get_mut(id).ok_or_else(|| FormError::NotFound(id.into()))?;
        if *entry.state.borrow() != State::Pending {
            return Err(FormError::AlreadySettled(id.into()));
        }
        let fields: Vec<Value> = entry.form["fields"].as_array().cloned().unwrap_or_default();
        if let Some(message) = validate_answer(&fields, &answer) {
            return Err(FormError::InvalidAnswer { id: id.into(), message });
        }
        let session = s(&entry.form, "sessionID").unwrap_or_default().to_string();
        self.publish(entry.location.as_ref(), &session, "form.replied", json!({"id": id, "sessionID": session, "answer": answer}));
        entry.settled_at = Some(Instant::now());
        let _ = entry.state.send_replace(State::Answered(answer));
        Ok(())
    }

    pub fn cancel(&self, id: &str, message: Option<String>) -> Result<(), FormError> {
        let mut forms = self.forms.lock();
        let entry = forms.get_mut(id).ok_or_else(|| FormError::NotFound(id.into()))?;
        if *entry.state.borrow() != State::Pending {
            return Err(FormError::AlreadySettled(id.into()));
        }
        let session = s(&entry.form, "sessionID").unwrap_or_default().to_string();
        self.publish(entry.location.as_ref(), &session, "form.cancelled", json!({"id": id, "sessionID": session}));
        entry.settled_at = Some(Instant::now());
        let _ = entry.state.send_replace(State::Cancelled(message));
        Ok(())
    }

    /// Cancel pending forms owned by the matching sessions (a location scope closing).
    pub fn cancel_where(&self, mut matches: impl FnMut(&str) -> bool) {
        let ids: Vec<String> = self
            .list(None)
            .iter()
            .filter(|f| s(f, "sessionID").is_some_and(&mut matches))
            .filter_map(|f| s(f, "id").map(str::to_string))
            .collect();
        for id in ids {
            let _ = self.cancel(&id, None);
        }
    }

    pub fn close(&self) {
        *self.closed.lock() = true;
        let ids: Vec<String> = self.list(None).iter().filter_map(|f| s(f, "id").map(str::to_string)).collect();
        for id in ids {
            let _ = self.cancel(&id, None);
        }
    }
}

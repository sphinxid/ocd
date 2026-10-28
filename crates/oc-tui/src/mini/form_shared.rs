//! Form dock state machine (`src/mini/form.shared.ts`) over `util/form.ts` helpers (`ui::form`).

use super::types::MiniForm;
use crate::api::types::FormInfo;
use crate::ui::form as uf;
use serde_json::{Map, Value};

#[derive(Clone, Debug, PartialEq, Default)]
pub struct FormBodyState {
    pub form_id: String,
    pub field: usize,
    pub answers: Map<String, Value>,
    pub custom: Map<String, Value>,
    pub selected: usize,
    pub editing: bool,
    pub external_ready: std::collections::BTreeSet<String>,
    pub submitting: bool,
    pub error: String,
}

#[derive(Clone, Debug, PartialEq)]
pub struct FormReply {
    pub session_id: String,
    pub form_id: String,
    pub answer: Map<String, Value>,
    pub location: Option<String>,
}

/// `formInitialValues(fields)`: `(answers, custom)`.
pub fn form_initial_values(fields: &[Value]) -> (Map<String, Value>, Map<String, Value>) {
    let answers = uf::initial(fields);
    let mut custom = Map::new();
    for f in fields {
        if uf::kind(f) != "string" || uf::options(f).is_none() || f.get("custom").and_then(|c| c.as_bool()) != Some(true) {
            continue;
        }
        let Some(Value::String(d)) = f.get("default") else { continue };
        if uf::options(f).unwrap().iter().any(|o| o.get("value") == Some(&Value::String(d.clone()))) {
            continue;
        }
        custom.insert(uf::key(f), Value::String(d.clone()));
    }
    (answers, custom)
}

/// `formSelected(field, value)`.
pub fn form_selected(field: Option<&Value>, value: Option<&Value>) -> usize {
    let (Some(field), Some(value)) = (field, value) else { return 0 };
    if value.is_array() {
        return 0;
    }
    let rows = uf::rows(field);
    if let Some(i) = rows.iter().position(|(v, _, _)| v == value) {
        return i;
    }
    if value.is_string() && uf::custom(field) {
        return rows.len();
    }
    0
}

pub fn create_form_body_state(form: &FormInfo) -> FormBodyState {
    let (answers, custom) = form_initial_values(&form.fields);
    let first = form.fields.first();
    let key = first.map(uf::key).unwrap_or_default();
    FormBodyState {
        form_id: form.id.clone(),
        field: 0,
        selected: form_selected(first, answers.get(&key)),
        editing: first.is_some_and(uf::textual),
        answers,
        custom,
        ..Default::default()
    }
}

pub fn form_sync(state: &FormBodyState, form: &FormInfo) -> FormBodyState {
    if state.form_id == form.id { state.clone() } else { create_form_body_state(form) }
}

/// `formUnsupported(form)`.
pub fn form_unsupported(form: &FormInfo) -> Option<String> {
    if form.fields.is_empty() {
        return Some("This form has no supported fields.".into());
    }
    for field in &form.fields {
        let Some(kind) = field.get("type").and_then(|t| t.as_str()) else { return Some("This form uses an unknown field.".into()) };
        if field.get("key").and_then(|k| k.as_str()).is_none() {
            return Some("This form uses an invalid field.".into());
        }
        if field.get("when").and_then(|w| w.as_array()).is_some_and(|w| !w.is_empty()) {
            return Some("Conditional forms are not supported in Mini yet.".into());
        }
        if kind == "string" && field.get("pattern").is_some_and(|p| !p.is_null()) {
            return Some("Pattern-constrained forms are not supported in Mini yet.".into());
        }
        if !["string", "number", "integer", "boolean", "multiselect", "external"].contains(&kind) {
            return Some(format!("Field type {kind} is not supported in Mini yet."));
        }
    }
    None
}

pub fn form_current<'a>(form: &'a FormInfo, state: &FormBodyState) -> Option<&'a Value> {
    form.fields.get(state.field)
}

pub fn form_confirm(form: &FormInfo, state: &FormBodyState) -> bool {
    state.field >= form.fields.len()
}

/// `formSingle(form)`.
pub fn form_single(form: &FormInfo) -> bool {
    form.fields.len() == 1 && matches!(form.fields.first().map(uf::kind), Some("boolean" | "number" | "integer" | "external" | "string"))
}

/// `formPlaceholder(field)`.
pub fn form_placeholder(field: Option<&Value>) -> String {
    if let Some(f) = field
        && uf::kind(f) == "string"
    {
        return f.get("placeholder").and_then(|p| p.as_str()).unwrap_or("Type your answer").to_string();
    }
    "Enter a number".into()
}

pub fn form_move(state: &FormBodyState, form: &FormInfo, direction: i64) -> FormBodyState {
    let field = form_current(form, state);
    let total = field.map(|f| uf::rows(f).len() + if uf::custom(f) { 1 } else { 0 }).unwrap_or(0) as i64;
    if total == 0 {
        return state.clone();
    }
    FormBodyState { selected: ((state.selected as i64 + direction + total) % total) as usize, error: String::new(), ..state.clone() }
}

pub fn form_set_selected(state: &FormBodyState, selected: usize) -> FormBodyState {
    FormBodyState { selected, error: String::new(), ..state.clone() }
}

pub fn form_set_submitting(state: &FormBodyState, submitting: bool, error: &str) -> FormBodyState {
    FormBodyState { submitting, error: error.into(), ..state.clone() }
}

pub fn form_set_error(state: &FormBodyState, error: &str) -> FormBodyState {
    FormBodyState { error: error.into(), submitting: false, ..state.clone() }
}

pub fn form_set_external_ready(state: &FormBodyState, key: &str) -> FormBodyState {
    let mut ready = state.external_ready.clone();
    ready.insert(key.into());
    FormBodyState { external_ready: ready, error: String::new(), ..state.clone() }
}

/// `formDisplay(field, value)`.
pub fn form_display(field: &Value, value: Option<&Value>) -> String {
    uf::display(field, value, "")
}

/// `formInput(state, field)`.
pub fn form_input(state: &FormBodyState, field: Option<&Value>) -> String {
    let Some(field) = field.filter(|f| uf::kind(f) != "external") else { return String::new() };
    let key = uf::key(field);
    match state.custom.get(&key) {
        Some(Value::String(s)) => s.clone(),
        _ => form_display(field, state.answers.get(&key)),
    }
}

pub fn form_set_draft(state: &FormBodyState, field: Option<&Value>, value: &str) -> FormBodyState {
    let Some(field) = field.filter(|f| uf::kind(f) != "external") else { return state.clone() };
    let mut custom = state.custom.clone();
    custom.insert(uf::key(field), Value::String(value.into()));
    FormBodyState { custom, ..state.clone() }
}

/// `formValidate(form, state)`.
pub fn form_validate(form: &FormInfo, state: &FormBodyState) -> Option<String> {
    if let Some(u) = form_unsupported(form) {
        return Some(u);
    }
    for field in &form.fields {
        let key = uf::key(field);
        if uf::kind(field) == "external" {
            if state.answers.get(&key) != Some(&Value::Bool(true)) {
                return Some(format!("Acknowledge {}", uf::label(field)));
            }
            continue;
        }
        if let Some(invalid) = uf::validate(field, state.answers.get(&key)) {
            return Some(format!("{}: {invalid}", uf::label(field)));
        }
    }
    None
}

/// `formAnswer(form, state)`.
pub fn form_answer(form: &FormInfo, state: &FormBodyState) -> Option<Map<String, Value>> {
    if form_validate(form, state).is_some() {
        return None;
    }
    let mut out = Map::new();
    for field in &form.fields {
        let key = uf::key(field);
        if let Some(v) = state.answers.get(&key) {
            out.insert(key, v.clone());
        }
    }
    Some(out)
}

pub fn form_reply(form: &MiniForm, state: &FormBodyState) -> Option<FormReply> {
    let answer = form_answer(&form.form, state)?;
    Some(FormReply { session_id: form.form.session_id.clone(), form_id: form.form.id.clone(), answer, location: form.location.clone() })
}

/// `formSetField(state, form, index)`.
pub fn form_set_field(state: &FormBodyState, form: &FormInfo, index: usize) -> FormBodyState {
    let bounded = index.min(form.fields.len());
    let field = form.fields.get(bounded);
    let value = field.and_then(|f| state.answers.get(&uf::key(f)));
    FormBodyState { field: bounded, selected: form_selected(field, value), editing: field.is_some_and(uf::textual), error: String::new(), ..state.clone() }
}

fn toggle_multiselect(value: Option<&Value>, item: &str) -> Value {
    let mut values: Vec<Value> = value.and_then(|v| v.as_array()).cloned().unwrap_or_default();
    match values.iter().position(|v| v.as_str() == Some(item)) {
        Some(i) => {
            values.remove(i);
        }
        None => values.push(Value::String(item.into())),
    }
    Value::Array(values)
}

fn set_multiselect_custom(value: Option<&Value>, previous: Option<&str>, next: &str) -> Value {
    let values: Vec<Value> = value.and_then(|v| v.as_array()).cloned().unwrap_or_default();
    let index = previous.filter(|p| !p.is_empty()).and_then(|p| values.iter().position(|v| v.as_str() == Some(p)));
    let mut current = values.clone();
    if let Some(i) = index {
        current.remove(i);
    }
    if !next.is_empty() && !current.iter().any(|v| v.as_str() == Some(next)) {
        current.push(Value::String(next.into()));
    }
    Value::Array(current)
}

/// `formPick(state, form)`.
pub fn form_pick(state: &FormBodyState, form: &FormInfo) -> FormBodyState {
    let Some(field) = form_current(form, state) else { return state.clone() };
    if uf::kind(field) == "external" || uf::textual(field) {
        return state.clone();
    }
    let rows = uf::rows(field);
    if state.selected == rows.len() && uf::custom(field) {
        return FormBodyState { editing: true, error: String::new(), ..state.clone() };
    }
    let Some(row) = rows.get(state.selected) else { return state.clone() };
    let key = uf::key(field);
    let mut answers = state.answers.clone();
    if uf::kind(field) == "multiselect" {
        let item = super::util::js_string(&row.0);
        answers.insert(key.clone(), toggle_multiselect(state.answers.get(&key), &item));
        return FormBodyState { answers, error: String::new(), ..state.clone() };
    }
    answers.insert(key, row.0.clone());
    let next = FormBodyState { answers, error: String::new(), ..state.clone() };
    form_set_field(&next, form, if form_single(form) { state.field } else { state.field + 1 })
}

fn js_number_value(input: &str) -> Value {
    match input.parse::<f64>() {
        Ok(n) if n.is_finite() => {
            if n.fract() == 0.0 && n.abs() < 9e15 {
                Value::from(n as i64)
            } else {
                serde_json::json!(n)
            }
        }
        // `Number("abc")` is NaN; validation reports "Expected a number".
        _ => Value::Null,
    }
}

/// `formCommitInput(state, form, text)`.
pub fn form_commit_input(state: &FormBodyState, form: &FormInfo, text: &str) -> FormBodyState {
    let Some(field) = form_current(form, state) else { return state.clone() };
    let kind = uf::kind(field);
    if kind == "external" || kind == "boolean" {
        return state.clone();
    }
    let input = text.trim();
    let key = uf::key(field);
    if kind == "multiselect" {
        let previous = state.custom.get(&key).and_then(|v| v.as_str()).map(|s| s.to_string());
        let values = set_multiselect_custom(state.answers.get(&key), previous.as_deref(), input);
        if let Some(invalid) = uf::validate(field, Some(&values)) {
            return form_set_error(state, &invalid);
        }
        let mut answers = state.answers.clone();
        answers.insert(key.clone(), values);
        let mut custom = state.custom.clone();
        custom.insert(key, Value::String(input.into()));
        return FormBodyState { answers, custom, editing: false, error: String::new(), ..state.clone() };
    }
    let value = if input.is_empty() {
        None
    } else if kind == "number" || kind == "integer" {
        Some(js_number_value(input))
    } else {
        Some(Value::String(input.into()))
    };
    if let Some(invalid) = uf::validate(field, value.as_ref()) {
        return form_set_error(state, &invalid);
    }
    let mut answers = state.answers.clone();
    match &value {
        Some(v) => {
            answers.insert(key.clone(), v.clone());
        }
        None => {
            answers.remove(&key);
        }
    }
    let mut custom = state.custom.clone();
    custom.insert(key, Value::String(match &value {
        Some(Value::String(s)) => s.clone(),
        _ => input.into(),
    }));
    FormBodyState { answers, custom, editing: false, error: String::new(), ..state.clone() }
}

/// `formAcknowledge(state, form)`.
pub fn form_acknowledge(state: &FormBodyState, form: &FormInfo) -> FormBodyState {
    let Some(field) = form_current(form, state) else { return state.clone() };
    let key = uf::key(field);
    if uf::kind(field) != "external" || !state.external_ready.contains(&key) {
        return state.clone();
    }
    let mut answers = state.answers.clone();
    answers.insert(key, Value::Bool(true));
    let next = FormBodyState { answers, error: String::new(), ..state.clone() };
    form_set_field(&next, form, if form_single(form) { state.field } else { state.field + 1 })
}

/// `formErrorMessage(error)`.
pub fn form_error_message(error: &anyhow::Error) -> String {
    if let Some(api) = error.downcast_ref::<crate::api::ApiError>() {
        if !api.message.trim().is_empty() {
            return api.message.clone();
        }
        if !api.tag.trim().is_empty() {
            return api.tag.clone();
        }
    }
    let s = error.to_string();
    if s.trim().is_empty() { "Form request failed".into() } else { s }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn request(fields: Value) -> FormInfo {
        FormInfo { id: "frm_1".into(), session_id: "ses_1".into(), title: "Input".into(), metadata: None, fields: fields.as_array().unwrap().clone() }
    }

    #[test]
    fn builds_every_supported_answer() {
        let form = request(json!([
            {"key": "choice", "type": "string", "options": [{"value": "fast", "label": "Fast"}], "default": "fast"},
            {"key": "count", "type": "number"},
            {"key": "whole", "type": "integer", "default": 2},
            {"key": "enabled", "type": "boolean", "default": false},
            {"key": "tags", "type": "multiselect", "options": [], "custom": true},
            {"key": "external", "type": "external", "url": "https://example.com/action"}
        ]));
        let mut state = form_set_field(&create_form_body_state(&form), &form, 1);
        state = form_commit_input(&state, &form, "1.5");
        state = form_set_field(&state, &form, 4);
        state = form_set_selected(&state, 0);
        state = form_pick(&state, &form);
        state = form_commit_input(&state, &form, "custom");
        state = form_set_field(&state, &form, 5);
        state = form_acknowledge(&form_set_external_ready(&state, "external"), &form);
        let answer = json!({"choice": "fast", "count": 1.5, "whole": 2, "enabled": false, "tags": ["custom"], "external": true});
        assert_eq!(Value::Object(form_answer(&form, &state).unwrap()), answer);
        let mini = MiniForm { form: form.clone(), location: Some("/tmp".into()) };
        let reply = form_reply(&mini, &state).unwrap();
        assert_eq!((reply.session_id.as_str(), reply.form_id.as_str(), reply.location.as_deref()), ("ses_1", "frm_1", Some("/tmp")));
        assert_eq!(Value::Object(reply.answer), answer);
    }

    #[test]
    fn rejects_invalid_and_unsupported() {
        let invalid = request(json!([{"key": "required", "type": "string", "required": true}, {"key": "external", "type": "external", "url": "https://example.com"}]));
        assert!(form_validate(&invalid, &create_form_body_state(&invalid)).unwrap().contains("Answer required"));
        assert!(form_unsupported(&request(json!([{"key": "value", "type": "string", "pattern": "^a"}]))).unwrap().contains("Pattern"));
        assert!(form_unsupported(&request(json!([{"key": "toggle", "type": "boolean"}, {"key": "value", "type": "string", "when": [{"key": "toggle", "op": "eq", "value": true}]}]))).unwrap().contains("Conditional"));
    }
}

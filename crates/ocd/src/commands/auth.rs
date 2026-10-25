//! `auth list/login/logout/switch/export/import` (`handlers/auth/*.ts`, `form.ts`) and the
//! OAuth flow shared with `mcp auth`.

use std::time::Duration;

use oc_util::Global;
use serde_json::{Map, Value, json};

use super::{cwd, pretty, report, server_args};
use crate::cli::parse::Input;
use crate::client::{Client, Req};
use crate::connection;
use crate::term::{EOL, write_stderr, write_stdout};
use crate::ui::{self, Cancelled};

/// A failure inside a clack flow: cancelled prompt (130) or an error (`log.error` + `Failed`, 1).
pub enum Flow {
    Cancelled,
    Error(anyhow::Error),
}

impl<E: Into<anyhow::Error>> From<E> for Flow {
    fn from(e: E) -> Flow {
        let e: anyhow::Error = e.into();
        if e.downcast_ref::<Cancelled>().is_some() { Flow::Cancelled } else { Flow::Error(e) }
    }
}

/// `handlePromptErrors`
pub fn handle(result: Result<(), Flow>) -> i32 {
    match result {
        Ok(()) => 0,
        Err(Flow::Cancelled) => {
            ui::cancel("Cancelled");
            130
        }
        Err(Flow::Error(e)) => {
            ui::log::error(&e.to_string());
            ui::outro("Failed");
            1
        }
    }
}

pub fn require_interactive(message: &str) -> anyhow::Result<()> {
    if ui::interactive() { Ok(()) } else { anyhow::bail!("{message}") }
}

fn s<'a>(v: &'a Value, k: &str) -> &'a str {
    v.get(k).and_then(Value::as_str).unwrap_or("")
}

fn arr(v: &Value, k: &str) -> Vec<Value> {
    v.get(k).and_then(Value::as_array).cloned().unwrap_or_default()
}

/// `loadIntegrations`: model readiness barrier, then integrations with `opencode-go` first.
pub async fn load_integrations(client: &Client) -> anyhow::Result<Vec<Value>> {
    let dir = cwd();
    client.call("model.default", Req::new().location(&dir)).await?;
    let mut list = arr(&client.call("integration.list", Req::new().location(&dir)).await?, "data");
    list.sort_by_key(|i| if s(i, "id") == "opencode-go" { 0 } else { 1 });
    Ok(list)
}

/// `resolveIntegration(integrations, target)`
pub fn resolve_integration(integrations: &[Value], target: &str) -> anyhow::Result<Value> {
    let normalized = target.trim_end_matches('/');
    if let Some(i) = integrations.iter().find(|i| s(i, "id") == normalized) {
        return Ok(i.clone());
    }
    let matches: Vec<&Value> = integrations.iter().filter(|i| s(i, "name").to_lowercase() == normalized.to_lowercase()).collect();
    match matches.len() {
        1 => Ok(matches[0].clone()),
        0 => anyhow::bail!("Integration not found: {target}"),
        _ => anyhow::bail!(
            "Integration name \"{target}\" is ambiguous: {}",
            matches.iter().map(|i| s(i, "id")).collect::<Vec<_>>().join(", ")
        ),
    }
}

fn connections(i: &Value, kind: &str) -> Vec<Value> {
    arr(i, "connections").into_iter().filter(|c| s(c, "type") == kind).collect()
}

/// `connectMethods`: non-`env` methods, `key` methods last.
pub fn connect_methods(i: &Value) -> Vec<Value> {
    let mut m: Vec<Value> = arr(i, "methods").into_iter().filter(|m| s(m, "type") != "env").collect();
    m.sort_by_key(|m| s(m, "type") == "key");
    m
}

pub fn resolve_method(methods: &[Value], target: &str) -> anyhow::Result<Value> {
    let normalized = target.to_lowercase();
    let matches: Vec<&Value> = methods
        .iter()
        .filter(|m| {
            let label = m.get("label").and_then(Value::as_str).map(str::to_lowercase);
            if s(m, "type") == "key" {
                normalized == "key" || label.as_deref() == Some(normalized.as_str())
            } else {
                s(m, "id") == target || label.as_deref() == Some(normalized.as_str())
            }
        })
        .collect();
    match matches.len() {
        1 => Ok(matches[0].clone()),
        0 => {
            let available: Vec<&str> = methods.iter().map(|m| if s(m, "type") == "key" { "key" } else { s(m, "id") }).collect();
            let available = available.join(", ");
            anyhow::bail!(
                "Authentication method not found: {target}{}",
                if available.is_empty() { String::new() } else { format!(". Available: {available}") }
            )
        }
        _ => anyhow::bail!("Authentication method \"{target}\" is ambiguous"),
    }
}

// ---------------------------------------------------------------------------------------------
// list / export / import

pub async fn list(global: &Global, input: &Input) -> anyhow::Result<i32> {
    let inner = async {
        let server = connection::resolve(global, server_args(input)).await?;
        let integrations: Vec<Value> =
            load_integrations(&server.client()).await?.into_iter().filter(|i| !arr(i, "connections").is_empty()).collect();
        if input.str("format").as_deref() == Some("json") {
            let rows: Vec<Value> = integrations
                .iter()
                .map(|i| json!({"id": i.get("id"), "name": i.get("name"), "connections": i.get("connections")}))
                .collect();
            write_stdout(&format!("{}{EOL}", pretty(&Value::Array(rows))));
            return Ok::<(), anyhow::Error>(());
        }
        let mut rows = vec![];
        for i in &integrations {
            for c in arr(i, "connections") {
                let credential = s(&c, "type") == "credential";
                rows.push((
                    s(i, "name").to_string(),
                    if credential { s(&c, "label") } else { s(&c, "name") }.to_string(),
                    if credential { "stored" } else { "environment" },
                ));
            }
        }
        if rows.is_empty() {
            write_stdout(&format!("No authenticated integrations{EOL}"));
            return Ok(());
        }
        let width = rows.iter().map(|r| r.0.encode_utf16().count()).max().unwrap_or(0) + 2;
        let lines: Vec<String> = rows.iter().map(|(n, src, t)| format!("{}{}{t}", pad(n, width), pad(src, 28))).collect();
        write_stdout(&format!("{}{EOL}", lines.join(EOL)));
        Ok(())
    };
    Ok(match inner.await {
        Ok(()) => 0,
        Err(e) => report(e),
    })
}

/// `String.prototype.padEnd` (UTF-16 length).
pub fn pad(s: &str, width: usize) -> String {
    let len = s.encode_utf16().count();
    format!("{s}{}", " ".repeat(width.saturating_sub(len)))
}

pub async fn export(global: &Global, input: &Input) -> anyhow::Result<i32> {
    let inner = async {
        let server = connection::resolve(global, server_args(input)).await?;
        let client = server.client();
        let integration = match input.str("target") {
            Some(t) => Some(s(&resolve_integration(&load_integrations(&client).await?, &t)?, "id").to_string()),
            None => None,
        };
        let all = client.call("credential.list", Req::new()).await?;
        let credentials: Vec<Value> = all
            .get("data")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default()
            .into_iter()
            .filter(|c| integration.as_deref().is_none_or(|id| s(c, "integrationID") == id))
            .collect();
        if crate::term::stdout_is_tty() {
            write_stderr(&format!("Warning: the output contains secrets; redirect it to a file or pipe it to auth import{EOL}"));
        }
        write_stdout(&format!("{}{EOL}", pretty(&Value::Array(credentials))));
        Ok::<(), anyhow::Error>(())
    };
    Ok(match inner.await {
        Ok(()) => 0,
        Err(e) => report(e),
    })
}

pub async fn import(global: &Global, input: &Input) -> anyhow::Result<i32> {
    let inner = async {
        let file = input.str("file");
        if file.is_none() && crate::term::stdin_is_tty() {
            anyhow::bail!("Pipe auth export output into stdin or pass a file to import");
        }
        let text = match &file {
            Some(f) => std::fs::read_to_string(f)?,
            None => {
                let mut t = String::new();
                std::io::Read::read_to_string(&mut std::io::stdin(), &mut t)?;
                t
            }
        };
        let mut entries: Vec<Value> = serde_json::from_str(&text)?;
        for e in entries.iter_mut() {
            if !(e.get("id").is_some_and(Value::is_string) && e.get("integrationID").is_some_and(Value::is_string) && e.get("value").is_some_and(Value::is_object)) {
                anyhow::bail!("Expected an array of credential entries");
            }
            // `CredentialLegacy.Entry`: files exported before v2.1 keep key form answers in `configuration`.
            match serde_json::from_value::<oc_ai::credentials::CredentialValue>(e["value"].clone()) {
                Ok(value) => e["value"] = serde_json::to_value(value)?,
                Err(_) => anyhow::bail!("Expected an array of credential entries"),
            }
        }
        let server = connection::resolve(global, server_args(input)).await?;
        let client = server.client();
        let existing = arr(&client.call("credential.list", Req::new()).await?, "data");
        let ids: std::collections::HashSet<String> = existing.iter().map(|c| s(c, "id").to_string()).collect();
        let integrations: std::collections::HashSet<String> = existing.iter().map(|c| s(c, "integrationID").to_string()).collect();
        let mut imported = 0;
        for c in &entries {
            if ids.contains(s(c, "id")) {
                continue;
            }
            let activate = c.get("active").and_then(Value::as_bool).unwrap_or(false) && !integrations.contains(s(c, "integrationID"));
            let body = json!({
                "id": c.get("id"), "integrationID": c.get("integrationID"), "label": c.get("label"),
                "value": c.get("value"), "activate": activate,
            });
            match client.call("credential.create", Req::new().body(body)).await {
                Ok(_) => imported += 1,
                Err(e) if e.tag() == Some("ConflictError") || e.status() == Some(409) => {}
                Err(e) => return Err(e.into()),
            }
        }
        let skipped = entries.len() - imported;
        write_stderr(&format!(
            "Imported {imported} {}{}{EOL}",
            if imported == 1 { "credential" } else { "credentials" },
            if skipped > 0 { format!(", skipped {skipped} already present") } else { String::new() }
        ));
        Ok(())
    };
    Ok(match inner.await {
        Ok(()) => 0,
        Err(e) => report(e),
    })
}

// ---------------------------------------------------------------------------------------------
// logout / switch

fn choose_integration(integrations: &[Value], target: Option<&str>) -> Result<Value, Flow> {
    if let Some(t) = target {
        return Ok(resolve_integration(integrations, t)?);
    }
    let configured: Vec<&Value> = integrations.iter().filter(|i| !connections(i, "credential").is_empty()).collect();
    if configured.is_empty() {
        return Err(Flow::Error(anyhow::anyhow!("No stored credentials found")));
    }
    let choices: Vec<ui::Choice> = configured
        .iter()
        .map(|i| ui::Choice {
            value: s(i, "id").into(),
            label: s(i, "name").into(),
            hint: Some(connections(i, "credential").iter().map(|c| s(c, "label").to_string()).collect::<Vec<_>>().join(", ")),
        })
        .collect();
    let id = ui::select("Select integration", &choices, 8)?;
    Ok(resolve_integration(&configured.into_iter().cloned().collect::<Vec<_>>(), &id)?)
}

fn choose_credential(integration: &Value, action: &str, target: Option<&str>) -> Result<String, Flow> {
    let name = s(integration, "name");
    let credentials = connections(integration, "credential");
    if credentials.is_empty() {
        let env: Vec<String> = connections(integration, "env").iter().map(|c| s(c, "name").to_string()).collect();
        if !env.is_empty() && action == "log out" {
            return Err(Flow::Error(anyhow::anyhow!(
                "{name} is authenticated through {}; unset the environment variable to disconnect",
                env.join(", ")
            )));
        }
        return Err(Flow::Error(anyhow::anyhow!("No stored credentials for {name}")));
    }
    if let Some(t) = target {
        if let Some(c) = credentials.iter().find(|c| s(c, "id") == t) {
            return Ok(s(c, "id").into());
        }
        let matches: Vec<&Value> = credentials.iter().filter(|c| s(c, "label").to_lowercase() == t.to_lowercase()).collect();
        return match matches.len() {
            1 => Ok(s(matches[0], "id").into()),
            0 => Err(Flow::Error(anyhow::anyhow!("Credential not found for {name}: {t}"))),
            _ => Err(Flow::Error(anyhow::anyhow!(
                "Credential label \"{t}\" is ambiguous. Use an ID: {}",
                matches.iter().map(|c| s(c, "id")).collect::<Vec<_>>().join(", ")
            ))),
        };
    }
    let choices: Vec<ui::Choice> = credentials
        .iter()
        .enumerate()
        .map(|(i, c)| ui::Choice {
            value: s(c, "id").into(),
            label: if i == 0 { format!("{} (active)", s(c, "label")) } else { s(c, "label").into() },
            hint: Some(s(c, "id").into()),
        })
        .collect();
    Ok(ui::select(&format!("Select {name} account to {action}"), &choices, 8)?)
}

async fn account(global: &Global, input: &Input, switch: bool) -> Result<(), Flow> {
    let target = input.str("target");
    let credential = input.str("credential");
    if target.is_none() {
        require_interactive("Pass an integration ID or name when running without an interactive terminal")?;
    }
    if credential.is_none() {
        require_interactive("Pass a credential ID or label when running without an interactive terminal")?;
    }
    ui::intro(if switch { "Switch account" } else { "Log out of an account" });
    let server = connection::resolve(global, server_args(input)).await?;
    let client = server.client();
    let integrations = load_integrations(&client).await?;
    let integration = choose_integration(&integrations, target.as_deref())?;
    let id = choose_credential(&integration, if switch { "switch to" } else { "log out" }, credential.as_deref())?;
    let name = s(&integration, "name");
    if switch {
        let sp = ui::Spinner::start("Switching account...");
        match client.call("credential.activate", Req::new().path("credentialID", &id)).await {
            Ok(_) => sp.stop(&format!("Switched account for {name}"), 0),
            Err(e) => {
                sp.stop("Failed to switch account", 1);
                return Err(e.into());
            }
        }
    } else {
        let sp = ui::Spinner::start("Removing credential...");
        match client.call("credential.remove", Req::new().path("credentialID", &id)).await {
            Ok(_) => sp.stop(&format!("Removed account from {name}"), 0),
            Err(e) => {
                sp.stop("Failed to remove credential", 1);
                return Err(e.into());
            }
        }
    }
    ui::outro("Done");
    Ok(())
}

pub async fn logout(global: &Global, input: &Input) -> anyhow::Result<i32> {
    Ok(handle(account(global, input, false).await))
}

pub async fn switch(global: &Global, input: &Input) -> anyhow::Result<i32> {
    Ok(handle(account(global, input, true).await))
}

// ---------------------------------------------------------------------------------------------
// login

const PRIORITY: &[&str] = &["opencode-go", "opencode", "openai", "github-copilot", "anthropic", "google"];

fn priority(id: &str) -> usize {
    PRIORITY.iter().position(|p| *p == id).unwrap_or(PRIORITY.len())
}

pub fn login_choices(integrations: &[Value]) -> Vec<ui::Choice> {
    let mut list: Vec<&Value> = integrations.iter().filter(|i| !connect_methods(i).is_empty()).collect();
    let mcp = |i: &Value| i.pointer("/metadata/source").and_then(Value::as_str) == Some("mcp");
    list.sort_by(|a, b| {
        (mcp(b) as i32)
            .cmp(&(mcp(a) as i32))
            .then(priority(s(a, "id")).cmp(&priority(s(b, "id"))))
            .then(crate::term::locale_cmp(s(a, "name"), s(b, "name")))
            .then(crate::term::locale_cmp(s(a, "id"), s(b, "id")))
    });
    list.iter()
        .map(|i| {
            let category = if mcp(i) {
                "MCP"
            } else if PRIORITY.contains(&s(i, "id")) {
                "Popular"
            } else {
                "Services"
            };
            let connected = !arr(i, "connections").is_empty();
            ui::Choice {
                value: s(i, "id").into(),
                label: format!("{}{}", s(i, "name"), if connected { " ✓" } else { "" }),
                hint: Some(category.into()),
            }
        })
        .collect()
}

fn active(field: &Value, answer: &Map<String, Value>) -> bool {
    if s(field, "type") == "external" {
        return true;
    }
    let Some(when) = field.get("when").and_then(Value::as_array) else { return true };
    when.iter().all(|c| {
        let Some(value) = answer.get(s(c, "key")) else { return false };
        let expected = c.get("value").cloned().unwrap_or(Value::Null);
        let matches = match value {
            Value::Array(items) => {
                let text = match &expected {
                    Value::String(s) => s.clone(),
                    other => other.to_string(),
                };
                items.iter().any(|i| i.as_str() == Some(text.as_str()))
            }
            v => *v == expected,
        };
        if s(c, "op") == "eq" { matches } else { !matches }
    })
}

fn validate_text(field: &Value, value: &str) -> Option<String> {
    let required = field.get("required").and_then(Value::as_bool).unwrap_or(false);
    if value.is_empty() {
        return required.then(|| "Required".into());
    }
    let ty = s(field, "type");
    if ty == "number" || ty == "integer" {
        let Ok(n) = value.trim().parse::<f64>() else { return Some("Expected a number".into()) };
        if !n.is_finite() {
            return Some("Expected a number".into());
        }
        if ty == "integer" && n.fract() != 0.0 {
            return Some("Expected an integer".into());
        }
        if let Some(min) = field.get("minimum").and_then(Value::as_f64)
            && n < min
        {
            return Some(format!("Must be at least {}", field["minimum"]));
        }
        if let Some(max) = field.get("maximum").and_then(Value::as_f64)
            && n > max
        {
            return Some(format!("Must be at most {}", field["maximum"]));
        }
        return None;
    }
    let len = value.encode_utf16().count() as u64;
    if let Some(min) = field.get("minLength").and_then(Value::as_u64)
        && len < min
    {
        return Some(format!("Must be at least {min} characters"));
    }
    if let Some(max) = field.get("maxLength").and_then(Value::as_u64)
        && len > max
    {
        return Some(format!("Must be at most {max} characters"));
    }
    if let Some(p) = field.get("pattern").and_then(Value::as_str) {
        match regex::Regex::new(p) {
            Ok(re) if re.is_match(value) => {}
            _ => return Some("Invalid format".into()),
        }
    }
    match s(field, "format") {
        "uri" if url::Url::parse(value).is_err() => Some("Expected a URL".into()),
        "email" if !regex::Regex::new(r"^[^\s@]+@[^\s@]+\.[^\s@]+$").unwrap().is_match(value) => Some("Expected an email address".into()),
        "date" if chrono::NaiveDate::parse_from_str(value, "%Y-%m-%d").is_err() || value.len() != 10 => Some("Expected a date".into()),
        "date-time" if chrono::DateTime::parse_from_rfc3339(value).is_err() => Some("Expected a date and time".into()),
        _ => None,
    }
}

/// `formValidateValue` (subset: required/type checks of supplied answers).
fn validate_value(field: &Value, value: Option<&Value>) -> Option<String> {
    let required = field.get("required").and_then(Value::as_bool).unwrap_or(false);
    match value {
        None => required.then(|| "Required".into()),
        Some(Value::String(v)) => validate_text(field, v),
        Some(Value::Number(n)) => validate_text(field, &n.to_string()),
        _ => None,
    }
}

fn answer_field(field: &Value) -> Result<Option<Value>, Flow> {
    require_interactive("Authentication form input requires an interactive terminal")?;
    let key = s(field, "key");
    let message = field.get("title").and_then(Value::as_str).unwrap_or(key).to_string();
    if let Some(d) = field.get("description").and_then(Value::as_str) {
        ui::log::info(d);
    }
    let required = field.get("required").and_then(Value::as_bool).unwrap_or(false);
    let ty = s(field, "type");
    match ty {
        "external" => {
            let url = s(field, "url");
            ui::log::info(url);
            ui::open_url(url);
            let msg = if message.is_empty() { "Continue after completing this step?".to_string() } else { message.clone() };
            if !ui::confirm(&msg, true)? {
                return Err(Flow::Error(anyhow::anyhow!("{} is required", if message.is_empty() { "External step" } else { &message })));
            }
            Ok(Some(Value::Bool(true)))
        }
        "boolean" => {
            if required {
                return Ok(Some(Value::Bool(ui::confirm(&message, field.get("default").and_then(Value::as_bool).unwrap_or(true))?)));
            }
            let choices = [("true", "Yes"), ("false", "No"), ("skip", "Skip")]
                .map(|(v, l)| ui::Choice { value: v.into(), label: l.into(), hint: None });
            Ok(match ui::select(&message, &choices, 3)?.as_str() {
                "true" => Some(Value::Bool(true)),
                "false" => Some(Value::Bool(false)),
                _ => None,
            })
        }
        "multiselect" => {
            let options: Vec<String> = arr(field, "options").iter().map(|o| s(o, "value").to_string()).collect();
            let raw = ui::text(&format!("{message} ({})", options.join(", ")), false, &|_| None)?;
            let selected: Vec<Value> = raw.split(',').map(str::trim).filter(|v| !v.is_empty()).map(|v| Value::String(v.into())).collect();
            if let Some(min) = field.get("minItems").and_then(Value::as_u64)
                && (selected.len() as u64) < min
            {
                return Err(Flow::Error(anyhow::anyhow!("Select at least {min}")));
            }
            if let Some(max) = field.get("maxItems").and_then(Value::as_u64)
                && (selected.len() as u64) > max
            {
                return Err(Flow::Error(anyhow::anyhow!("Select at most {max}")));
            }
            Ok(Some(Value::Array(selected)))
        }
        _ => {
            if ty == "string"
                && let Some(options) = field.get("options").and_then(Value::as_array)
            {
                let mut choices: Vec<ui::Choice> = options
                    .iter()
                    .map(|o| ui::Choice { value: s(o, "value").into(), label: s(o, "label").into(), hint: o.get("description").and_then(Value::as_str).map(String::from) })
                    .collect();
                if field.get("custom").and_then(Value::as_bool).unwrap_or(false) {
                    choices.push(ui::Choice { value: "\u{0}custom".into(), label: "Type your own answer".into(), hint: None });
                }
                if !required {
                    choices.push(ui::Choice { value: "\u{0}skip".into(), label: "Skip".into(), hint: None });
                }
                let v = ui::select(&message, &choices, 10)?;
                if v == "\u{0}skip" {
                    return Ok(None);
                }
                if v != "\u{0}custom" {
                    return Ok(Some(Value::String(v)));
                }
            }
            let f = field.clone();
            let value = ui::text(&message, false, &|v| validate_text(&f, v))?;
            if value.is_empty() && !required {
                return Ok(None);
            }
            if ty == "string" {
                return Ok(Some(Value::String(value)));
            }
            Ok(Some(value.trim().parse::<f64>().ok().and_then(serde_json::Number::from_f64).map(Value::Number).unwrap_or(Value::Null)))
        }
    }
}

/// `answerForm(fields, --answer items)`
pub fn answer_form(fields: Option<&Value>, input: &[String]) -> Result<Option<Value>, Flow> {
    let fields = fields.and_then(Value::as_array);
    let mut supplied = indexmap::IndexMap::new();
    for item in input {
        let sep = item.find('=').filter(|i| *i >= 1).ok_or_else(|| anyhow::anyhow!("Expected --answer key=value"))?;
        let key = &item[..sep];
        if !fields.is_some_and(|f| f.iter().any(|x| s(x, "key") == key)) {
            return Err(Flow::Error(anyhow::anyhow!("Unknown form field: {key}")));
        }
        supplied.insert(key.to_string(), item[sep + 1..].to_string());
    }
    let Some(fields) = fields else { return Ok(None) };
    let mut answer = Map::new();
    for field in fields {
        let key = s(field, "key").to_string();
        if !active(field, &answer) {
            if supplied.contains_key(&key) {
                return Err(Flow::Error(anyhow::anyhow!("Form field is not active: {key}")));
            }
            continue;
        }
        let ty = s(field, "type");
        let value = match supplied.get(&key) {
            Some(raw) => {
                if ty == "external" {
                    return Err(Flow::Error(anyhow::anyhow!("External form field requires interactive confirmation: {key}")));
                }
                if ty == "string" {
                    Some(Value::String(raw.clone()))
                } else {
                    Some(serde_json::from_str::<Value>(raw).map_err(|_| anyhow::anyhow!("Expected a JSON value for form field: {key}"))?)
                }
            }
            None if ty != "external" && field.get("hidden").and_then(Value::as_bool).unwrap_or(false) => field.get("default").cloned(),
            None => answer_field(field)?,
        };
        if ty != "external"
            && let Some(invalid) = validate_value(field, value.as_ref())
        {
            return Err(Flow::Error(anyhow::anyhow!("{key}: {invalid}")));
        }
        if let Some(v) = value {
            answer.insert(key, v);
        }
    }
    Ok(Some(Value::Object(answer)))
}

pub async fn login(global: &Global, input: &Input) -> anyhow::Result<i32> {
    Ok(handle(
        login_flow(global, input.str("target"), input.str("method"), input.list("answer"), server_args(input)).await,
    ))
}

/// `auth login` with explicit arguments (also used by `acp --login`).
pub async fn login_flow(
    global: &Global,
    target: Option<String>,
    method: Option<String>,
    answers: Vec<String>,
    server: connection::Args<'_>,
) -> Result<(), Flow> {
    if target.is_none() {
        require_interactive("Pass an integration ID or name when running without an interactive terminal")?;
    }
    ui::intro("Connect an integration");
    let server = connection::resolve(global, server).await?;
    let client = server.client();
    let dir = cwd();
    if let Some(t) = &target
        && let Ok(u) = url::Url::parse(t)
        && matches!(u.scheme(), "http" | "https")
    {
        let sp = ui::Spinner::start("Discovering authentication provider...");
        match client
            .call("experimental.integration.wellknown.add", Req::new().body(json!({"url": t, "location": {"directory": dir}})))
            .await
        {
            Ok(_) => sp.stop("Authentication provider discovered", 0),
            Err(e) => {
                sp.stop("Discovery failed", 1);
                return Err(e.into());
            }
        }
    }
    let integrations = load_integrations(&client).await?;
    let integration = match &target {
        Some(t) => resolve_integration(&integrations, t)?,
        None => {
            let choices = login_choices(&integrations);
            if choices.is_empty() {
                return Err(Flow::Error(anyhow::anyhow!("No authentication integrations are available")));
            }
            let id = ui::select("Select integration", &choices, 10)?;
            resolve_integration(&integrations, &id)?
        }
    };
    let name = s(&integration, "name").to_string();
    let methods = connect_methods(&integration);
    if methods.is_empty() {
        return Err(Flow::Error(anyhow::anyhow!("{name} has no interactive login methods")));
    }
    let method = match method {
        Some(m) => resolve_method(&methods, &m)?,
        None if methods.len() == 1 => methods[0].clone(),
        None => {
            require_interactive("Pass --method when running without an interactive terminal")?;
            let choices: Vec<ui::Choice> = methods
                .iter()
                .map(|m| {
                    if s(m, "type") == "key" {
                        ui::Choice { value: "key".into(), label: m.get("label").and_then(Value::as_str).unwrap_or("API key").into(), hint: None }
                    } else {
                        ui::Choice { value: s(m, "id").into(), label: s(m, "label").into(), hint: None }
                    }
                })
                .collect();
            let id = ui::select("Select login method", &choices, 10)?;
            resolve_method(&methods, &id)?
        }
    };
    let form = if s(&method, "type") == "command" { None } else { method.get("form") };
    let answer = answer_form(form, &answers)?;
    let id = s(&integration, "id").to_string();
    match s(&method, "type") {
        "key" => {
            require_interactive("API key input requires an interactive terminal")?;
            let prompt = method.get("label").and_then(Value::as_str).map(String::from).unwrap_or_else(|| format!("Enter your {name} API key"));
            let key = ui::text(&prompt, true, &|v| v.is_empty().then(|| "Required".to_string()))?;
            let sp = ui::Spinner::start("Saving credential...");
            let mut body = json!({"key": key, "location": {"directory": dir}});
            if let Some(a) = &answer {
                body["answer"] = a.clone();
            }
            match client.call("integration.connect.key", Req::new().path("integrationID", &id).body(body)).await {
                Ok(_) => sp.stop(&format!("Connected to {name}"), 0),
                Err(e) => {
                    sp.stop("Authentication failed", 1);
                    return Err(e.into());
                }
            }
        }
        "external" => {
            let sp = ui::Spinner::start("Saving credential...");
            let mut body = json!({"methodID": s(&method, "id"), "location": {"directory": dir}});
            if let Some(a) = &answer {
                body["answer"] = a.clone();
            }
            match client.call("integration.connect.external", Req::new().path("integrationID", &id).body(body)).await {
                Ok(_) => sp.stop(&format!("Connected to {name}"), 0),
                Err(e) => {
                    sp.stop("Authentication failed", 1);
                    return Err(e.into());
                }
            }
        }
        "command" => command_login(&client, &integration, &method).await?,
        _ => oauth_login(&client, &integration, &method, answer, None).await?,
    }
    ui::outro("Done");
    Ok(())
}

async fn command_login(client: &Client, integration: &Value, method: &Value) -> Result<(), Flow> {
    let (id, name, dir) = (s(integration, "id").to_string(), s(integration, "name").to_string(), cwd());
    let sp = ui::Spinner::start("Starting authentication command...");
    let started = match client
        .call("integration.command.connect", Req::new().path("integrationID", &id).body(json!({"methodID": s(method, "id"), "location": {"directory": dir}})))
        .await
    {
        Ok(v) => v,
        Err(e) => {
            sp.stop("Authentication failed", 1);
            return Err(e.into());
        }
    };
    let attempt = started.pointer("/data/attemptID").and_then(Value::as_str).unwrap_or("").to_string();
    sp.stop("Authentication command started", 0);
    ui::log::step("Waiting for authentication command...");
    let mut printed = 0usize;
    let result = loop {
        let status = client
            .call("integration.command.status", Req::new().path("integrationID", &id).path("attemptID", &attempt).location(&dir))
            .await;
        let status = match status {
            Ok(s) => s.get("data").cloned().unwrap_or(Value::Null),
            Err(e) => break Err(Flow::from(e)),
        };
        if s(&status, "status") != "pending" {
            break match s(&status, "status") {
                "complete" => {
                    ui::log::success(&format!("Connected to {name}"));
                    Ok(())
                }
                "failed" => Err(Flow::Error(anyhow::anyhow!("{}", s(&status, "message")))),
                _ => Err(Flow::Error(anyhow::anyhow!("Authentication expired"))),
            };
        }
        let message = s(&status, "message");
        if let Some(end) = message.rfind('\n').map(|i| i + 1)
            && end > printed
        {
            let output = message[printed..end].trim();
            printed = end;
            if !output.is_empty() {
                ui::log::message(output);
            }
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    };
    let _ = client
        .call(
            "integration.command.cancel",
            Req::new().path("integrationID", &id).path("attemptID", &attempt).location(&dir).timeout(Duration::from_secs(5)),
        )
        .await;
    result
}

/// `oauthLogin` (also used by `mcp auth`).
pub async fn oauth_login(client: &Client, integration: &Value, method: &Value, answer: Option<Value>, label: Option<String>) -> Result<(), Flow> {
    let (id, name, dir) = (s(integration, "id").to_string(), s(integration, "name").to_string(), cwd());
    let sp = ui::Spinner::start("Starting authorization...");
    let mut body = json!({"methodID": s(method, "id"), "location": {"directory": dir}});
    if let Some(a) = answer {
        body["answer"] = a;
    }
    if let Some(l) = label {
        body["label"] = Value::String(l);
    }
    let started = match client.call("integration.oauth.connect", Req::new().path("integrationID", &id).body(body)).await {
        Ok(v) => v.get("data").cloned().unwrap_or(Value::Null),
        Err(e) => {
            sp.stop("Authentication failed", 1);
            return Err(e.into());
        }
    };
    let attempt = s(&started, "attemptID").to_string();
    sp.stop("Authorization started", 0);
    ui::log::info(s(&started, "instructions"));
    ui::log::info(s(&started, "url"));
    if ui::interactive() {
        ui::open_url(s(&started, "url"));
    }
    let result = async {
        if s(&started, "mode") == "code" {
            require_interactive("This login requires an interactive terminal to enter the authorization code")?;
            let code = ui::text("Paste the authorization code", false, &|v| v.is_empty().then(|| "Required".to_string()))?;
            let sp = ui::Spinner::start("Completing authorization...");
            return match client
                .call(
                    "integration.oauth.complete",
                    Req::new().path("integrationID", &id).path("attemptID", &attempt).body(json!({"code": code, "location": {"directory": dir}})),
                )
                .await
            {
                Ok(_) => {
                    sp.stop(&format!("Connected to {name}"), 0);
                    Ok(())
                }
                Err(e) => {
                    sp.stop("Authentication failed", 1);
                    Err(Flow::from(e))
                }
            };
        }
        ui::log::step("Waiting for authorization...");
        loop {
            let status = client
                .call("integration.oauth.status", Req::new().path("integrationID", &id).path("attemptID", &attempt).location(&dir))
                .await?;
            let status = status.get("data").cloned().unwrap_or(Value::Null);
            match s(&status, "status") {
                "pending" => tokio::time::sleep(Duration::from_millis(500)).await,
                "complete" => {
                    ui::log::success(&format!("Connected to {name}"));
                    return Ok(());
                }
                "failed" => return Err(Flow::Error(anyhow::anyhow!("{}", s(&status, "message")))),
                _ => return Err(Flow::Error(anyhow::anyhow!("Authorization expired"))),
            }
        }
    }
    .await;
    let _ = client
        .call(
            "integration.oauth.cancel",
            Req::new().path("integrationID", &id).path("attemptID", &attempt).location(&dir).timeout(Duration::from_secs(5)),
        )
        .await;
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolves_integrations_and_methods() {
        let list = vec![json!({"id": "openai", "name": "OpenAI"}), json!({"id": "a", "name": "Dup"}), json!({"id": "b", "name": "dup"})];
        assert_eq!(s(&resolve_integration(&list, "openai/").unwrap(), "id"), "openai");
        assert_eq!(s(&resolve_integration(&list, "OPENAI").unwrap(), "id"), "openai");
        assert_eq!(resolve_integration(&list, "dup").unwrap_err().to_string(), "Integration name \"dup\" is ambiguous: a, b");
        assert_eq!(resolve_integration(&list, "x").unwrap_err().to_string(), "Integration not found: x");
        let methods = vec![json!({"type": "oauth", "id": "o", "label": "Browser"}), json!({"type": "key"})];
        assert_eq!(s(&resolve_method(&methods, "KEY").unwrap(), "type"), "key");
        assert_eq!(resolve_method(&methods, "zz").unwrap_err().to_string(), "Authentication method not found: zz. Available: o, key");
    }

    #[test]
    fn form_answers() {
        let fields = json!([{"key": "region", "type": "string"}, {"key": "n", "type": "integer", "when": [{"key": "region", "op": "eq", "value": "x"}]}]);
        let Ok(Some(a)) = answer_form(Some(&fields), &["region=x".into(), "n=3".into()]) else { panic!() };
        assert_eq!(a, json!({"region": "x", "n": 3}));
        let Err(Flow::Error(e)) = answer_form(Some(&fields), &["region=y".into(), "n=3".into()]) else { panic!() };
        assert_eq!(e.to_string(), "Form field is not active: n");
        let Err(Flow::Error(e)) = answer_form(Some(&fields), &["=3".into()]) else { panic!() };
        assert_eq!(e.to_string(), "Expected --answer key=value");
        let Err(Flow::Error(e)) = answer_form(Some(&fields), &["q=3".into()]) else { panic!() };
        assert_eq!(e.to_string(), "Unknown form field: q");
    }
}

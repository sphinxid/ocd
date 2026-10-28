//! `mcp list/add/auth/logout` (`handlers/mcp/*.ts`).

use std::path::{Path, PathBuf};

use oc_config::jsonc_edit::{ModifyOptions, Segment, apply_modify};
use oc_util::Global;
use serde_json::{Value, json};

use super::auth::{Flow, answer_form, handle, load_integrations, oauth_login, require_interactive};
use super::cwd;
use crate::cli::parse::Input;
use crate::client::{Client, Req};
use crate::connection::{self, Args};
use crate::term::{EOL, locale_cmp, write_stdout};
use crate::ui;

fn s<'a>(v: &'a Value, k: &str) -> &'a str {
    v.get(k).and_then(Value::as_str).unwrap_or("")
}

async fn servers(client: &Client) -> anyhow::Result<Vec<Value>> {
    Ok(client.call("mcp.list", Req::new().location(&cwd())).await?.get("data").and_then(Value::as_array).cloned().unwrap_or_default())
}

pub async fn list(global: &Global) -> anyhow::Result<i32> {
    let server = connection::resolve(global, Args::default()).await?;
    let mut list = servers(&server.client()).await?;
    list.sort_by(|a, b| locale_cmp(s(a, "name"), s(b, "name")));
    if list.is_empty() {
        write_stdout(&format!("No MCP servers configured{EOL}"));
        return Ok(0);
    }
    let width = list.iter().map(|x| s(x, "name").encode_utf16().count()).max().unwrap_or(0);
    let lines: Vec<String> = list
        .iter()
        .map(|x| {
            let status = x.get("status").cloned().unwrap_or(Value::Null);
            let (icon, describe) = match s(&status, "status") {
                "connected" => ("✓", "connected".to_string()),
                "needs_auth" => ("⚠", "needs authentication".to_string()),
                "failed" => ("✗", format!("failed: {}", s(&status, "error"))),
                other => ("○", other.to_string()),
            };
            format!("{icon} {}  {describe}", super::auth::pad(s(x, "name"), width))
        })
        .collect();
    write_stdout(&format!("{}{EOL}", lines.join(EOL)));
    Ok(0)
}

/// `resolveConfigPath(dir)`
pub fn resolve_config_path(dir: &Path) -> PathBuf {
    let candidates =
        [dir.join("opencode.json"), dir.join("opencode.jsonc"), dir.join(".opencode").join("opencode.json"), dir.join(".opencode").join("opencode.jsonc")];
    candidates.iter().find(|c| c.is_file()).cloned().unwrap_or_else(|| candidates[0].clone())
}

pub fn add(global: &Global, input: &Input) -> anyhow::Result<i32> {
    let name = input.str("name").unwrap_or_default();
    let url = input.str("url");
    let headers = input.map("header");
    let environment = input.map("env");
    let argv: Vec<String> = std::env::args().collect();
    let command = match argv.iter().position(|a| a == "--") {
        Some(i) => argv[i + 1..].to_vec(),
        None => input.list("command"),
    };
    let has_command = !command.is_empty();
    if url.is_some() && has_command {
        anyhow::bail!(oc_mcp::cli::ADD_BOTH);
    }
    if url.is_none() && !has_command {
        anyhow::bail!(oc_mcp::cli::ADD_NEITHER);
    }
    if let Some(u) = &url
        && url::Url::parse(u).is_err()
    {
        anyhow::bail!(oc_mcp::cli::invalid_url(u));
    }
    if url.is_some() && environment.is_some() {
        anyhow::bail!(oc_mcp::cli::ADD_ENV_REMOTE);
    }
    if has_command && headers.is_some() {
        anyhow::bail!(oc_mcp::cli::ADD_HEADER_LOCAL);
    }
    let entry = match &url {
        Some(u) => {
            let mut e = json!({"type": "remote", "url": u});
            if let Some(h) = headers {
                e["headers"] = serde_json::to_value(h)?;
            }
            e
        }
        None => {
            let mut e = json!({"type": "local", "command": command});
            if let Some(env) = environment {
                e["environment"] = serde_json::to_value(env)?;
            }
            e
        }
    };
    let dir = if input.flag("global") { global.config.clone() } else { std::env::current_dir()? };
    let path = resolve_config_path(&dir);
    let text = match std::fs::read_to_string(&path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => "{}".into(),
        Err(e) => return Err(e.into()),
    };
    let segments = [Segment::Key("mcp".into()), Segment::Key("servers".into()), Segment::Key(name.clone())];
    let next = apply_modify(&text, &segments, Some(&entry), &ModifyOptions::formatted()).map_err(|e| anyhow::anyhow!(e))?;
    std::fs::write(&path, next)?;
    write_stdout(&format!("{}{EOL}", oc_mcp::cli::added(&name, &path.display().to_string())));
    Ok(0)
}

/// `resolveIntegration(client, name)` (`mcp/resolve.ts`)
async fn resolve_integration(client: &Client, name: &str) -> anyhow::Result<Option<Value>> {
    let list = servers(client).await?;
    let server = list.iter().find(|x| s(x, "name") == name).ok_or_else(|| anyhow::anyhow!(oc_mcp::cli::not_found(name)))?;
    let Some(id) = server.get("integrationID").and_then(Value::as_str).filter(|i| !i.is_empty()) else { return Ok(None) };
    let found = client.call("integration.get", Req::new().path("integrationID", id).location(&cwd())).await?;
    Ok(found.get("data").filter(|d| !d.is_null()).cloned())
}

pub async fn logout(global: &Global, input: &Input) -> anyhow::Result<i32> {
    let name = input.str("name").unwrap_or_default();
    let server = connection::resolve(global, Args::default()).await?;
    let client = server.client();
    let Some(integration) = resolve_integration(&client, &name).await? else {
        write_stdout(&format!("{}{EOL}", oc_mcp::cli::no_credentials(&name)));
        return Ok(0);
    };
    let credentials: Vec<Value> = integration
        .get("connections")
        .and_then(Value::as_array)
        .map(|a| a.iter().filter(|c| s(c, "type") == "credential").cloned().collect())
        .unwrap_or_default();
    if credentials.is_empty() {
        write_stdout(&format!("{}{EOL}", oc_mcp::cli::no_credentials(&name)));
        return Ok(0);
    }
    for c in &credentials {
        client.call("credential.remove", Req::new().path("credentialID", s(c, "id"))).await?;
    }
    write_stdout(&format!("{}{EOL}", oc_mcp::cli::removed_credentials(&name)));
    Ok(0)
}

pub async fn auth(global: &Global, input: &Input) -> anyhow::Result<i32> {
    Ok(handle(auth_flow(global, input.str("name")).await))
}

async fn auth_flow(global: &Global, name: Option<String>) -> Result<(), Flow> {
    if name.is_none() {
        require_interactive(oc_mcp::cli::AUTH_NEEDS_NAME)?;
    }
    ui::intro(oc_mcp::cli::AUTH_INTRO);
    let server = connection::resolve(global, Args::default()).await?;
    let client = server.client();
    let integrations = load_integrations(&client).await?;
    let list = servers(&client).await?;
    let by_id = |id: &str| integrations.iter().find(|i| s(i, "id") == id).cloned();
    let has_oauth = |i: &Value| i.get("methods").and_then(Value::as_array).is_some_and(|m| m.iter().any(|m| s(m, "type") == "oauth"));
    let mut choices: Vec<ui::IntegrationChoice> = list
        .iter()
        .filter_map(|server| {
            let integration = server.get("integrationID").and_then(Value::as_str).and_then(by_id)?;
            if !has_oauth(&integration) {
                return None;
            }
            let status = server.pointer("/status/status").and_then(Value::as_str).unwrap_or("");
            let hint = match status {
                "needs_auth" => Some("needs authentication".to_string()),
                "failed" | "disabled" => Some(status.to_string()),
                _ => None,
            };
            Some(ui::IntegrationChoice {
                value: s(&integration, "id").into(),
                label: s(server, "name").into(),
                category: "MCP".into(),
                connected: integration.get("connections").and_then(Value::as_array).is_some_and(|c| !c.is_empty()),
                hint,
            })
        })
        .collect();
    choices.sort_by(|a, b| locale_cmp(&a.label, &b.label).then(locale_cmp(&a.value, &b.value)));
    if name.is_none() && choices.is_empty() {
        ui::log::warn(oc_mcp::cli::AUTH_NONE);
        ui::log::info(&format!("{}\n{}", oc_mcp::cli::AUTH_NONE_HINT.replace("`opencode ", "`ocd "), oc_mcp::cli::AUTH_NONE_EXAMPLE));
        ui::outro("Done");
        return Ok(());
    }
    let integration_id = match &name {
        Some(n) => {
            let server = list.iter().find(|x| s(x, "name") == n).ok_or_else(|| anyhow::anyhow!(oc_mcp::cli::not_found(n)))?;
            s(server, "integrationID").to_string()
        }
        None => ui::select_integration(choices, "MCP server")?,
    };
    let integration = by_id(&integration_id);
    let method = integration
        .as_ref()
        .and_then(|i| i.get("methods").and_then(Value::as_array).and_then(|m| m.iter().find(|m| s(m, "type") == "oauth").cloned()));
    let (Some(integration), Some(method)) = (integration, method) else {
        return Err(Flow::Error(anyhow::anyhow!(oc_mcp::cli::not_oauth_capable(name.as_deref().unwrap_or("undefined")))));
    };
    let connections: Vec<Value> = integration.get("connections").and_then(Value::as_array).cloned().unwrap_or_default();
    let iname = s(&integration, "name").to_string();
    if !connections.is_empty() {
        let status = list
            .iter()
            .find(|x| s(x, "integrationID") == s(&integration, "id"))
            .and_then(|x| x.pointer("/status/status").and_then(Value::as_str))
            .unwrap_or("");
        if status == "needs_auth" {
            ui::log::warn(&oc_mcp::cli::expired_credentials(&iname));
        }
        if status != "needs_auth" && ui::interactive() && !ui::confirm(&oc_mcp::cli::reauthenticate_confirm(&iname), true)? {
            ui::outro("Cancelled");
            return Ok(());
        }
    }
    let previous: Vec<Value> = connections.into_iter().filter(|c| s(c, "type") == "credential").collect();
    let answer = answer_form(method.get("form"), &[])?;
    let label = previous.first().map(|c| s(c, "label").to_string());
    oauth_login(&client, &integration, &method, answer, label).await?;
    for c in &previous {
        client.call("credential.remove", Req::new().path("credentialID", s(c, "id"))).await?;
    }
    ui::outro("Done");
    Ok(())
}

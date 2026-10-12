//! `session list/delete/export/import` (`handlers/session/*.ts`).

use oc_util::Global;
use serde_json::{Value, json};

use super::{cwd, pretty, report, server_args};
use crate::cli::parse::Input;
use crate::client::Req;
use crate::connection;
use crate::term::{EOL, write_stderr, write_stdout};
use crate::ui;

fn format_list(sessions: &[Value]) -> String {
    sessions
        .iter()
        .map(|s| {
            let title = s.get("title").and_then(Value::as_str).unwrap_or("Untitled session").replace(['\r', '\n', '\t'], " ");
            let updated = s.pointer("/time/updated").and_then(Value::as_i64).unwrap_or(0);
            format!("{}\t{}\t{}", s.get("id").and_then(Value::as_str).unwrap_or(""), title, crate::term::locale_string(updated))
        })
        .collect::<Vec<_>>()
        .join(EOL)
}

pub async fn list(global: &Global, input: &Input) -> anyhow::Result<i32> {
    let inner = async {
        let server = connection::resolve(global, server_args(input)).await?;
        let client = server.client();
        let location = client.call("location.get", Req::new().location(&cwd())).await?;
        let project = location.pointer("/project/id").cloned().unwrap_or(Value::Null);
        let max_count = input.int("maxCount");
        let page = client
            .call(
                "session.list",
                Req::new()
                    .query("project", project)
                    .query("parentID", Value::Null)
                    .query("order", json!("desc"))
                    .query("limit", json!(max_count.unwrap_or(100))),
            )
            .await?;
        let data = page.get("data").and_then(Value::as_array).cloned().unwrap_or_default();
        let format = input.str("format").unwrap_or_else(|| "table".into());
        if format == "table" && data.is_empty() {
            return Ok::<(), anyhow::Error>(());
        }
        let output = if format == "json" {
            let rows: Vec<Value> = data
                .iter()
                .map(|s| {
                    let mut o = serde_json::Map::new();
                    let mut put = |k: &str, v: Option<&Value>| {
                        if let Some(v) = v {
                            o.insert(k.into(), v.clone());
                        }
                    };
                    put("id", s.get("id"));
                    put("title", s.get("title"));
                    put("updated", s.pointer("/time/updated"));
                    put("created", s.pointer("/time/created"));
                    put("projectId", s.get("projectID"));
                    put("directory", s.pointer("/location/directory"));
                    Value::Object(o)
                })
                .collect();
            pretty(&Value::Array(rows))
        } else {
            format_list(&data)
        } + EOL;
        if !crate::term::stdout_is_tty() || max_count.is_some() || format == "json" {
            write_stdout(&output);
            return Ok(());
        }
        let pager = if cfg!(windows) { ("cmd", vec!["/c", "more"]) } else { ("less", vec!["-R", "-S"]) };
        match std::process::Command::new(pager.0).args(&pager.1).stdin(std::process::Stdio::piped()).spawn() {
            Ok(mut child) => {
                if let Some(mut stdin) = child.stdin.take() {
                    use std::io::Write;
                    let _ = stdin.write_all(output.as_bytes());
                }
                let _ = child.wait();
            }
            Err(_) => write_stdout(&output),
        }
        Ok(())
    };
    Ok(match inner.await {
        Ok(()) => 0,
        Err(e) => report(e),
    })
}

pub async fn delete(global: &Global, input: &Input) -> anyhow::Result<i32> {
    let id = input.str("sessionID").unwrap_or_default();
    let inner = async {
        let server = connection::resolve(global, server_args(input)).await?;
        server.client().call("session.remove", Req::new().path("sessionID", &id)).await?;
        Ok::<(), anyhow::Error>(())
    };
    Ok(match inner.await {
        Ok(()) => {
            write_stdout(&format!("Session {id} deleted{EOL}"));
            0
        }
        Err(e) => report(e),
    })
}

pub async fn export(global: &Global, input: &Input) -> anyhow::Result<i32> {
    let requested = input.str("session");
    let inner = async {
        if requested.is_none() && !crate::term::stdin_is_tty() {
            anyhow::bail!("Pass a session ID when running without an interactive terminal");
        }
        let server = connection::resolve(global, server_args(input)).await?;
        let client = server.client();
        let id = match requested {
            Some(id) => id,
            None => {
                ui::intro_to(ui::Out::Stderr, "Export session");
                let location = client.call("location.get", Req::new().location(&cwd())).await?;
                let page = client
                    .call(
                        "session.list",
                        Req::new()
                            .query("directory", location.get("directory").cloned().unwrap_or(Value::Null))
                            .query("parentID", Value::Null)
                            .query("order", json!("desc"))
                            .query("limit", json!(50)),
                    )
                    .await?;
                let data = page.get("data").and_then(Value::as_array).cloned().unwrap_or_default();
                if data.is_empty() {
                    ui::log::error_to(ui::Out::Stderr, "No sessions found");
                    ui::outro_to(ui::Out::Stderr, "Done");
                    return Ok(0);
                }
                let choices: Vec<ui::Choice> = data
                    .iter()
                    .map(|s| {
                        let id = s.get("id").and_then(Value::as_str).unwrap_or("").to_string();
                        let tail: String = id.chars().rev().take(8).collect::<Vec<_>>().into_iter().rev().collect();
                        ui::Choice {
                            label: s.get("title").and_then(Value::as_str).unwrap_or("").to_string(),
                            hint: Some(format!("{} - {tail}", crate::term::locale_string(s.pointer("/time/updated").and_then(Value::as_i64).unwrap_or(0)))),
                            value: id,
                        }
                    })
                    .collect();
                match ui::select("Select session to export", &choices, 10) {
                    Ok(id) => {
                        ui::outro_to(ui::Out::Stderr, "Exporting session...");
                        id
                    }
                    Err(_) => {
                        ui::cancel_to(ui::Out::Stderr, "Cancelled");
                        return Ok(130);
                    }
                }
            }
        };
        let data = client
            .call("experimental.session.export", Req::new().path("sessionID", &id).query("sanitize", json!(input.flag("sanitize"))))
            .await?
            .get("data")
            .cloned()
            .unwrap_or(Value::Null);
        write_stdout(&format!("{}{EOL}", pretty(&data)));
        Ok(0)
    };
    Ok(match inner.await {
        Ok(code) => code,
        Err(e) => report(e),
    })
}

pub async fn import(global: &Global, input: &Input) -> anyhow::Result<i32> {
    let file = input.str("file").unwrap_or_default();
    let text = if file.starts_with("http://") || file.starts_with("https://") {
        let response = reqwest::get(&file).await.map_err(|e| anyhow::anyhow!("Failed to read session data: {e}"))?;
        if !response.status().is_success() {
            anyhow::bail!(
                "Failed to read session data: Failed to fetch session data: {}",
                response.status().canonical_reason().unwrap_or("")
            );
        }
        response.text().await.map_err(|e| anyhow::anyhow!("Failed to read session data: {e}"))?
    } else {
        std::fs::read_to_string(&file).map_err(|e| anyhow::anyhow!("Failed to read session data: {e}"))?
    };
    let data: Value = serde_json::from_str(&text)?;
    let Some(obj) = data.as_object().filter(|o| o.get("info").is_some_and(Value::is_object)) else {
        anyhow::bail!("Expected session transfer data with an info object");
    };
    let server = connection::resolve(global, server_args(input)).await?;
    let client = server.client();
    let dir = input.str("directory").map(std::path::PathBuf::from).unwrap_or_else(|| std::env::current_dir().unwrap_or_default());
    let dir = if dir.is_absolute() { dir } else { std::env::current_dir()?.join(dir) };
    let location = client.call("location.get", Req::new().location(&dir.display().to_string())).await?;
    let mut body = obj.clone();
    body.insert("location".into(), json!({ "directory": location.get("directory").cloned().unwrap_or(Value::Null) }));
    let auth: Vec<(String, String)> = server
        .endpoint
        .authorization()
        .map(|a| vec![("authorization".into(), a)])
        .unwrap_or_default()
        .into_iter()
        .chain([("content-type".to_string(), "application/json".to_string())])
        .collect();
    let response = client.raw("POST", "/api/experimental/session/import", &auth, Some(Value::Object(body).to_string())).await?;
    if response.status().as_u16() == 409 {
        write_stderr(&format!("Session already exists{EOL}"));
        return Ok(0);
    }
    if !response.status().is_success() {
        anyhow::bail!("Failed to import session: {}", response.status().canonical_reason().unwrap_or(""));
    }
    let imported: Value = response.json().await?;
    let id = imported.pointer("/data/id").and_then(Value::as_str).ok_or_else(|| anyhow::anyhow!("Invalid import response"))?;
    write_stdout(&format!("Imported session: {id}{EOL}"));
    Ok(0)
}

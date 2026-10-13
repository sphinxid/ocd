//! `plugin list/add/check/update/remove` (`handlers/plugin/*.ts`).
//!
//! Server plugins are listed/checked/updated through the server API exactly like upstream.
//! Packages are installed into upstream's npm cache layout with Bun (`oc_plugin::npm`, the same
//! installer the plugin host uses) and their entrypoints resolved with Bun's module resolution
//! (upstream `Host.resolve`). TUI plugins are installed, configured and checked, but ocd's TUI does
//! not run them.

use std::path::Path;

use oc_config::jsonc_edit::{self, ModifyOptions, Segment};
use oc_util::Global;
use serde_json::{Value, json};

use super::{cwd, mcp::resolve_config_path};
use crate::cli::parse::Input;
use crate::client::Req;
use crate::connection::{self, Args};
use crate::term::{EOL, locale_cmp, write_stderr, write_stdout};

fn s<'a>(v: &'a Value, k: &str) -> &'a str {
    v.get(k).and_then(Value::as_str).unwrap_or("")
}

fn hash_version(v: &str) -> String {
    let hex = |n: usize| v.len() == n && v.chars().all(|c| c.is_ascii_hexdigit());
    if hex(40) || hex(64) { v[..7].to_string() } else { v.to_string() }
}

/// `format(plugins, tui, builtin)` of `plugin/list.ts` (server rows only).
pub fn format_list(plugins: &[Value], builtin: bool) -> String {
    let mut rows: Vec<[String; 3]> = plugins
        .iter()
        .filter(|p| builtin || p.pointer("/source/type").and_then(Value::as_str) != Some("builtin"))
        .map(|p| {
            let source = p.get("source").cloned().unwrap_or(Value::Null);
            let ty = s(&source, "type");
            let version = match ty {
                "package" => source.get("version").and_then(Value::as_str).unwrap_or("-").to_string(),
                "local" => "local".into(),
                _ => "-".into(),
            };
            let target = match ty {
                "package" => s(&source, "target").to_string(),
                "local" => s(&source, "path").to_string(),
                other => other.to_string(),
            };
            [p.get("id").and_then(Value::as_str).unwrap_or("-").to_string(), hash_version(&version), target]
        })
        .collect();
    rows.sort_by(|a, b| locale_cmp(&a[0], &b[0]).then(locale_cmp(&a[2], &b[2])));
    if rows.is_empty() {
        return String::new();
    }
    let mut table = vec![["ID".to_string(), "VERSION".to_string(), "SOURCE".to_string()]];
    table.extend(rows);
    let w0 = table.iter().map(|r| r[0].encode_utf16().count()).max().unwrap_or(0);
    let w1 = table.iter().map(|r| r[1].encode_utf16().count()).max().unwrap_or(0);
    table
        .iter()
        .map(|r| format!("{}  {}  {}", super::auth::pad(&r[0], w0), super::auth::pad(&r[1], w1), r[2]))
        .collect::<Vec<_>>()
        .join(EOL)
}

pub async fn list(global: &Global, input: &Input) -> anyhow::Result<i32> {
    let server = connection::resolve(global, Args::default()).await?;
    let response = server.client().call("plugin.list", Req::new().location(&cwd())).await?;
    let plugins = response.get("data").and_then(Value::as_array).cloned().unwrap_or_default();
    let output = format_list(&plugins, input.flag("builtin"));
    if output.is_empty() {
        write_stdout(&format!("No plugins found{EOL}"));
    } else {
        write_stdout(&format!("{output}{EOL}"));
    }
    Ok(0)
}

struct Item {
    runtime: &'static str,
    target: String,
    name: String,
    version: Option<String>,
    outdated: bool,
    error: Option<String>,
}

async fn inspect(global: &Global, selected: Option<&str>) -> anyhow::Result<(crate::connection::Resolved, Vec<Item>)> {
    let server = connection::resolve(global, Args::default()).await?;
    let client = server.client();
    let dir = cwd();
    let listed = client.call("plugin.list", Req::new().location(&dir)).await?;
    let data = |v: &Value| v.get("data").and_then(Value::as_array).cloned().unwrap_or_default();
    let targets: Vec<String> = data(&listed)
        .iter()
        .filter(|p| p.pointer("/source/type").and_then(Value::as_str) == Some("package"))
        .map(|p| p.pointer("/source/target").and_then(Value::as_str).unwrap_or("").to_string())
        .collect();
    let checked = if selected.is_none_or(|t| targets.iter().any(|x| x == t)) {
        let mut body = json!({"location": {"directory": dir}});
        if let Some(t) = selected {
            body["target"] = json!(t);
        }
        client.call("plugin.check", Req::new().body(body)).await?
    } else {
        listed
    };
    let items: Vec<Item> = data(&checked)
        .iter()
        .filter(|p| p.pointer("/source/type").and_then(Value::as_str) == Some("package"))
        .filter(|p| selected.is_none_or(|t| p.pointer("/source/target").and_then(Value::as_str) == Some(t)))
        .map(|p| {
            let target = p.pointer("/source/target").and_then(Value::as_str).unwrap_or("").to_string();
            Item {
                runtime: "Server",
                name: p.get("id").and_then(Value::as_str).map(String::from).unwrap_or_else(|| target.clone()),
                version: p.pointer("/source/version").and_then(Value::as_str).filter(|v| !v.is_empty()).map(hash_version),
                outdated: p.pointer("/source/outdated").and_then(Value::as_bool) == Some(true),
                error: (p.pointer("/state/status").and_then(Value::as_str) == Some("failed"))
                    .then(|| p.pointer("/state/error").and_then(Value::as_str).unwrap_or("").to_string()),
                target,
            }
        })
        .collect();
    let mut items = items;
    items.extend(tui_items(global, selected).await);
    if let Some(t) = selected
        && items.is_empty()
    {
        anyhow::bail!("Plugin is not configured: {t}");
    }
    Ok((server, items))
}

/// TUI package plugins configured in `cli.json` (`inventory.ts`): installed version and update check.
async fn tui_items(global: &Global, selected: Option<&str>) -> Vec<Item> {
    let cli = oc_config::cli::CliConfig::new(&global.config, &global.state);
    let info = cli.get().await;
    let mut configured: Vec<String> = Vec::new();
    for entry in info.get("plugins").and_then(Value::as_array).cloned().unwrap_or_default() {
        let target = entry.as_str().map(String::from).or_else(|| entry.get("package").and_then(Value::as_str).map(String::from));
        let Some(target) = target else { continue };
        if target.starts_with('-') || target == "*" || target.ends_with(".*") || target.starts_with("opencode.") {
            continue;
        }
        if !configured.contains(&target) {
            configured.push(target);
        }
    }
    let targets: Vec<String> = configured.into_iter().filter(|t| is_installable(t) && selected.is_none_or(|s| s == t)).collect();
    if targets.is_empty() {
        return vec![];
    }
    let npm = oc_plugin::npm::Npm::new(&global.cache, global.locks());
    let bun = oc_plugin::bun::BunResolver::new(global.bin.clone()).resolve().await;
    let mut out = Vec::new();
    for target in targets {
        let (installed, _) = npm.resolve(&target);
        let checked = match &bun {
            Ok(bun) => npm.check(bun, &target).await.map_err(|e| e.to_string()),
            Err(e) => Err(e.clone()),
        };
        out.push(Item {
            runtime: "TUI",
            name: target.clone(),
            version: installed.version.as_deref().filter(|v| !v.is_empty()).map(hash_version),
            outdated: checked.as_ref().is_ok_and(|v| *v),
            error: checked.err().map(|e| format!("Error: {e}")),
            target,
        });
    }
    out
}

fn format_items(items: &[Item]) -> String {
    let mut lines = vec![];
    for runtime in ["Server", "TUI"] {
        let rows: Vec<&Item> = items.iter().filter(|i| i.runtime == runtime).collect();
        if rows.is_empty() {
            continue;
        }
        lines.push(runtime.to_string());
        for i in rows {
            let state = if i.error.is_some() {
                "check failed"
            } else if i.outdated {
                "update available"
            } else {
                "current"
            };
            lines.push(format!("  {}{} ({state})", i.name, i.version.as_ref().map(|v| format!(" {v}")).unwrap_or_default()));
        }
    }
    lines.join(EOL)
}

pub async fn check(global: &Global, input: &Input) -> anyhow::Result<i32> {
    let (_server, items) = inspect(global, input.str("target").as_deref()).await?;
    let out = format_items(&items);
    write_stdout(&format!("{}{EOL}", if out.is_empty() { "No package plugins found".to_string() } else { out }));
    Ok(if items.iter().any(|i| i.error.is_some()) { 1 } else { 0 })
}

pub async fn update(global: &Global, input: &Input) -> anyhow::Result<i32> {
    let (server, items) = inspect(global, input.str("target").as_deref()).await?;
    for i in &items {
        if let Some(e) = &i.error {
            write_stderr(&format!("Failed to check {} plugin \"{}\": {e}{EOL}", i.runtime, i.name));
        }
    }
    let selected: Vec<&Item> = items.iter().filter(|i| i.outdated).collect();
    if selected.is_empty() {
        write_stdout(&format!("No plugin updates available{EOL}"));
        return Ok(if items.iter().any(|i| i.error.is_some()) { 1 } else { 0 });
    }
    let client = server.client();
    let dir = cwd();
    let results = futures::future::join_all(selected.iter().map(|i| {
        let client = client.clone();
        let body = json!({"location": {"directory": dir}, "targets": [i.target]});
        let server = i.runtime == "Server";
        async move { if server { client.call("plugin.update", Req::new().body(body)).await } else { Ok(Value::Null) } }
    }))
    .await;
    let npm = oc_plugin::npm::Npm::new(&global.cache, global.locks());
    let mut code = 0;
    for (i, r) in selected.iter().zip(results) {
        let r = if i.runtime == "TUI" {
            match oc_plugin::bun::BunResolver::new(global.bin.clone()).resolve().await {
                Ok(bun) => npm.update(&bun, &i.target).await.map(|_| Value::Null).map_err(|e| anyhow::anyhow!(e.to_string())),
                Err(e) => Err(anyhow::anyhow!(e)),
            }
        } else {
            r.map_err(|e| anyhow::anyhow!(e.to_string()))
        };
        match r {
            Ok(_) => write_stdout(&format!("Updated {} plugin \"{}\"{EOL}", i.runtime, i.name)),
            Err(e) => {
                write_stderr(&format!("Failed to update {} plugin \"{}\": Error: {e}{EOL}", i.runtime, i.name));
                code = 1;
            }
        }
    }
    Ok(code)
}

/// `Npm.isInstallablePackage`: npm registry or Git package specifiers (`npm.ts` `parse`).
pub fn is_installable(spec: &str) -> bool {
    oc_plugin::npm::is_installable(spec)
}

/// `Host.resolve(installed)`: the package's `server` (then root) and `tui` entrypoints, resolved
/// with Bun's module resolution from the installed package directory.
async fn resolve_entrypoints(bun: &Path, installed: &oc_plugin::Installed) -> anyhow::Result<(Option<String>, Option<String>)> {
    const SCRIPT: &str = r#"
const [directory, name] = process.argv.slice(-2)
const path = require("node:path")
const resolve = (specifier) => { try { return Bun.resolveSync(specifier, directory) } catch { return undefined } }
const entry = (subpaths) => {
  for (const subpath of subpaths) {
    const found = resolve(name ? [name, subpath].filter(Boolean).join("/") : path.resolve(directory, subpath || "index"))
    if (found) return found
  }
}
console.log(JSON.stringify({ server: entry(["server", ""]), tui: entry(["tui"]) }))
"#;
    let out = tokio::process::Command::new(bun)
        .args(["-e", SCRIPT, &installed.directory, &installed.name])
        .current_dir(&installed.directory)
        .stdin(std::process::Stdio::null())
        .output()
        .await?;
    if !out.status.success() {
        anyhow::bail!("{}", String::from_utf8_lossy(&out.stderr).trim());
    }
    let value: Value = serde_json::from_slice(String::from_utf8_lossy(&out.stdout).trim().as_bytes())?;
    let get = |k: &str| value.get(k).and_then(Value::as_str).map(String::from);
    Ok((get("server"), get("tui")))
}

/// `writePluginConfig(configPath, spec)`: appends the package to the global config's `plugins`.
fn write_plugin_config(path: &Path, spec: &str) -> anyhow::Result<bool> {
    let text = match std::fs::read_to_string(path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => "{}".to_string(),
        Err(e) => return Err(e.into()),
    };
    let (parsed, errors) = jsonc_edit::parse(&text, jsonc_edit::ParseOptions { allow_trailing_comma: true, ..Default::default() });
    let config = match parsed {
        Some(Value::Object(o)) if errors.is_empty() => o,
        _ => anyhow::bail!("Invalid global configuration: {}", path.display()),
    };
    let plugins = match config.get("plugins") {
        None => vec![],
        Some(Value::Array(a)) => a.clone(),
        Some(_) => anyhow::bail!("Invalid plugins configuration: {}", path.display()),
    };
    if plugins.iter().any(|p| matches(p, spec)) {
        return Ok(false);
    }
    let mut next = plugins;
    next.push(Value::String(spec.to_string()));
    let updated = jsonc_edit::apply_modify(&text, &[Segment::Key("plugins".into())], Some(&Value::Array(next)), &ModifyOptions::formatted())
        .map_err(|e| anyhow::anyhow!(e))?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let tmp = std::path::PathBuf::from(format!("{}.tmp", path.display()));
    let body = if updated.ends_with('\n') { updated } else { format!("{updated}\n") };
    std::fs::write(&tmp, body)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600));
    }
    std::fs::rename(&tmp, path)?;
    Ok(true)
}

pub async fn add(global: &Global, input: &Input) -> anyhow::Result<i32> {
    let package = input.str("package").unwrap_or_default();
    if !is_installable(&package) {
        anyhow::bail!("Plugin target must be an npm registry package or Git package specifier");
    }
    let bun = oc_plugin::bun::BunResolver::new(global.bin.clone()).resolve().await.map_err(|e| anyhow::anyhow!(e))?;
    let npm = oc_plugin::npm::Npm::new(&global.cache, global.locks());
    let installed = npm.add(&bun, &package).await.map_err(|e| anyhow::anyhow!(e.to_string()))?;
    let (server, tui) = resolve_entrypoints(&bun, &installed).await?;
    if server.is_some() {
        let config_path = resolve_config_path(&global.config);
        let changed = write_plugin_config(&config_path, &package)?;
        if changed {
            write_stdout(&format!("Plugin \"{package}\" installed and added to {}{EOL}", config_path.display()));
        } else {
            write_stdout(&format!("Plugin \"{package}\" is already configured in {}{EOL}", config_path.display()));
        }
        return Ok(0);
    }
    if tui.is_none() {
        anyhow::bail!("Plugin package has no server or TUI entrypoint: {package}");
    }
    let cli = oc_config::cli::CliConfig::new(&global.config, &global.state);
    cli.update(|draft| {
        let configured = draft.get("plugins").and_then(Value::as_array).is_some_and(|a| a.iter().any(|p| matches(p, &package)));
        if configured {
            return;
        }
        let mut plugins = draft.get("plugins").and_then(Value::as_array).cloned().unwrap_or_default();
        plugins.push(Value::String(package.clone()));
        if let Some(o) = draft.as_object_mut() {
            o.insert("plugins".into(), Value::Array(plugins));
        }
    })
    .await
    .map_err(|e| anyhow::anyhow!("{}", e.cause))?;
    write_stdout(&format!("TUI plugin \"{package}\" installed and added to {}{EOL}", oc_config::cli::config::path(&global.config).display()));
    Ok(0)
}

fn matches(entry: &Value, spec: &str) -> bool {
    entry.as_str() == Some(spec) || entry.get("package").and_then(Value::as_str) == Some(spec)
}

/// `removePluginConfig(configPath, spec)`
fn remove_plugin_config(path: &Path, spec: &str) -> anyhow::Result<bool> {
    let text = match std::fs::read_to_string(path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(e) => return Err(e.into()),
    };
    let (parsed, errors) =
        jsonc_edit::parse(&text, jsonc_edit::ParseOptions { allow_trailing_comma: true, ..Default::default() });
    let config = match parsed {
        Some(Value::Object(o)) if errors.is_empty() => o,
        _ => anyhow::bail!("Invalid global configuration: {}", path.display()),
    };
    let plugins = match config.get("plugins") {
        None => return Ok(false),
        Some(Value::Array(a)) => a.clone(),
        Some(_) => anyhow::bail!("Invalid plugins configuration: {}", path.display()),
    };
    if !plugins.iter().any(|p| matches(p, spec)) {
        return Ok(false);
    }
    let kept: Vec<Value> = plugins.into_iter().filter(|p| !matches(p, spec)).collect();
    let updated = jsonc_edit::apply_modify(&text, &[Segment::Key("plugins".into())], Some(&Value::Array(kept)), &ModifyOptions::formatted())
        .map_err(|e| anyhow::anyhow!(e))?;
    let tmp = std::path::PathBuf::from(format!("{}.tmp", path.display()));
    let body = if updated.ends_with('\n') { updated } else { format!("{updated}\n") };
    std::fs::write(&tmp, body)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600));
    }
    std::fs::rename(&tmp, path)?;
    Ok(true)
}

pub async fn remove(global: &Global, input: &Input) -> anyhow::Result<i32> {
    let package = input.str("package").unwrap_or_default();
    let config_path = resolve_config_path(&global.config);
    let server = remove_plugin_config(&config_path, &package)?;
    let cli = oc_config::cli::CliConfig::new(&global.config, &global.state);
    let info = cli.get().await;
    let tui = info.get("plugins").and_then(Value::as_array).is_some_and(|a| a.iter().any(|p| matches(p, &package)));
    if tui {
        cli.update(|draft| {
            if let Some(Value::Array(a)) = draft.get_mut("plugins") {
                a.retain(|p| !matches(p, &package));
            }
        })
        .await
        .map_err(|e| anyhow::anyhow!("{}", e.cause))?;
    }
    let mut removed = vec![];
    if server {
        removed.push(config_path.display().to_string());
    }
    if tui {
        removed.push(oc_config::cli::config::path(&global.config).display().to_string());
    }
    if removed.is_empty() {
        write_stdout(&format!("Plugin \"{package}\" is not configured{EOL}"));
    } else {
        write_stdout(&format!("Plugin \"{package}\" removed from {}{EOL}", removed.join(", ")));
    }
    Ok(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn list_table() {
        let plugins = vec![
            json!({"id": "b", "source": {"type": "package", "target": "pkg-b", "version": "0123456789abcdef0123456789abcdef01234567"}}),
            json!({"id": "a", "source": {"type": "local", "path": "/x/a.ts"}}),
            json!({"source": {"type": "builtin"}}),
        ];
        assert_eq!(format_list(&plugins, false), "ID  VERSION  SOURCE\na   local    /x/a.ts\nb   0123456  pkg-b");
        assert!(format_list(&plugins, true).contains("-   -        builtin"));
    }

    #[test]
    fn installable() {
        assert!(is_installable("@scope/pkg@1.2.3"));
        assert!(is_installable("github:user/repo"));
        assert!(!is_installable("./local"));
    }
}

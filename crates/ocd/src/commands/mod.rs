//! Command handlers (`cli/src/commands/handlers/**`).

pub mod auth;
pub mod mcp;
pub mod plugin;
pub mod session;
pub mod stats;
pub mod upgrade;

use oc_util::Global;
use serde_json::Value;

use crate::cli::Invocation;
use crate::cli::parse::Input;
use crate::client::{Client, Req};
use crate::connection::{self, Args, Mismatch};
use crate::service::{self, Pty, ServiceConfig};
use crate::term::{EOL, write_stderr, write_stdout};

/// Server params (`--server`, `--standalone`) of an input.
pub fn server_args<'a>(input: &Input) -> Args<'a> {
    Args { server: input.str("server"), standalone: input.flag("standalone"), mismatch: None, on_start: None }
}

pub fn cwd() -> String {
    std::env::current_dir().map(|p| p.display().to_string()).unwrap_or_default()
}

/// `JSON.stringify(value, null, 2)`
pub fn pretty(v: &Value) -> String {
    serde_json::to_string_pretty(v).unwrap_or_default()
}

/// Write `errorMessage(error) + EOL` to stderr and return exit code 1.
pub fn report(e: impl std::fmt::Display) -> i32 {
    write_stderr(&format!("{e}{EOL}"));
    1
}

pub async fn dispatch(global: &Global, inv: &Invocation) -> anyhow::Result<i32> {
    let input = &inv.input;
    match inv.name.as_str() {
        "" => default_command(global, input).await,
        "serve" => serve(global, input).await,
        "run" => crate::run::run(global, input).await,
        "mini" => mini(global, input).await,
        "models" => models(global, input).await,
        "reload" => reload(global, input).await,
        "api" => api(global, input).await,
        "pair" => pair(global, input).await,
        "stats" => stats::run(global, input).await,
        "acp" => crate::acp::run(global, input).await,
        "upgrade" => upgrade::upgrade(global, input).await,
        "uninstall" => upgrade::uninstall(global, input).await,
        "debug paths" => debug_paths(global, input),
        "debug config" => debug_config(global).await,
        "debug agents" => debug_agents(global).await,
        "service start" => service_start(global).await,
        "service stop" => service_stop(global).await,
        "service restart" => service_restart(global).await,
        "service status" => service_status(global).await,
        "service get" => service_get(global, input).await,
        "service set" => service_set(global, input).await,
        "service unset" => service_unset(global, input).await,
        "session list" => session::list(global, input).await,
        "session delete" => session::delete(global, input).await,
        "session export" => session::export(global, input).await,
        "session import" => session::import(global, input).await,
        "auth list" => auth::list(global, input).await,
        "auth login" => auth::login(global, input).await,
        "auth logout" => auth::logout(global, input).await,
        "auth switch" => auth::switch(global, input).await,
        "auth export" => auth::export(global, input).await,
        "auth import" => auth::import(global, input).await,
        "mcp list" => mcp::list(global).await,
        "mcp add" => mcp::add(global, input),
        "mcp auth" => mcp::auth(global, input).await,
        "mcp logout" => mcp::logout(global, input).await,
        "plugin list" => plugin::list(global, input).await,
        "plugin add" => plugin::add(global, input).await,
        "plugin check" => plugin::check(global, input).await,
        "plugin update" => plugin::update(global, input).await,
        "plugin remove" => plugin::remove(global, input).await,
        other => anyhow::bail!("Unknown command: {other}"),
    }
}

// ---------------------------------------------------------------------------------------------
// serve / default / mini

async fn serve(global: &Global, input: &Input) -> anyhow::Result<i32> {
    let (svc, stdio) = (input.flag("service"), input.flag("stdio"));
    if svc && stdio {
        anyhow::bail!("--service and --stdio cannot be combined");
    }
    let cors = input.list("cors");
    crate::serve::run(
        global,
        crate::serve::Options {
            mode: if svc {
                crate::serve::Mode::Service
            } else if stdio {
                crate::serve::Mode::Stdio
            } else {
                crate::serve::Mode::Default
            },
            hostname: input.str("hostname"),
            port: input.int("port"),
            cors: (!cors.is_empty()).then_some(cors),
        },
    )
    .await
}

fn on_start(reason: service::StartReason, previous: Option<&str>) {
    tracing::info!(reason = reason.as_str(), previousVersion = previous.unwrap_or(""), "background service starting");
    write_stderr(match reason {
        service::StartReason::VersionMismatch => "Restarting background server (version mismatch)...\n",
        service::StartReason::Missing => "Starting background server...\n",
    });
}

fn tui_options(input: &Input, server: &connection::Resolved, requested_server: bool) -> oc_tui::TuiOptions {
    oc_tui::TuiOptions {
        url: server.endpoint.url.clone(),
        password: server.endpoint.password.clone(),
        directory: cwd(),
        session: input.str("session"),
        continue_last: input.flag("continue"),
        fork: input.flag("fork"),
        prompt: input.str("prompt"),
        model: input.str("model"),
        agent: input.str("agent"),
        auto: input.flag("auto") || input.flag("yolo") || input.flag("dangerouslySkipPermissions"),
        channel: std::env::var("OPENCODE_TUI_CHANNEL").unwrap_or_else(|_| oc_util::version::CHANNEL.to_string()),
        version: oc_util::version::VERSION.to_string(),
        environment: if requested_server { Default::default() } else { connection::session_env().into_iter().collect() },
        service: server.service.clone().map(service_hooks),
    }
}

/// `server.service` for the TUI (`handlers/default.ts`, `handlers/mini.ts`): `reconnect` runs
/// `Service.ensure` again, `restart` stops the service and ensures a new one.
fn service_hooks(options: service::EnsureOptions) -> oc_tui::ServiceHooks {
    fn endpoint(e: crate::client::Endpoint) -> oc_tui::api::Endpoint {
        oc_tui::api::Endpoint { url: e.url, password: e.password }
    }
    let reconnect = std::sync::Arc::new(options);
    let restart = reconnect.clone();
    oc_tui::ServiceHooks {
        reconnect: std::sync::Arc::new(move || {
            let o = reconnect.clone();
            Box::pin(async move { connection::reconnect(&o).await.map(endpoint) })
        }),
        restart: std::sync::Arc::new(move || {
            let o = restart.clone();
            Box::pin(async move { connection::restart(&o).await.map(endpoint) })
        }),
    }
}

/// `$` — the TUI (`handlers/default.ts`).
async fn default_command(global: &Global, input: &Input) -> anyhow::Result<i32> {
    if let Some(dir) = input.str("directory") {
        std::env::set_current_dir(&dir).map_err(|e| anyhow::anyhow!("{e}: chdir '{dir}'"))?;
    }
    let requested = input.str("server");
    let cb = on_start;
    let server = connection::resolve(
        global,
        Args { server: requested.clone(), standalone: input.flag("standalone"), mismatch: Some(Mismatch::Replace), on_start: Some(&cb) },
    )
    .await?;
    let opts = tui_options(input, &server, requested.is_some());
    let code = oc_tui::run(opts).await?;
    drop(server);
    Ok(code)
}

async fn mini(global: &Global, input: &Input) -> anyhow::Result<i32> {
    if !crate::term::stdout_is_tty() {
        write_stderr("\x1b[91m\x1b[1mError: \x1b[0mocd mini requires a TTY stdout\n");
        return Ok(1);
    }
    let requested = input.str("server");
    let server = connection::resolve(
        global,
        Args { server: requested.clone(), standalone: input.flag("standalone"), mismatch: Some(Mismatch::Replace), on_start: None },
    )
    .await?;
    if let Some(m) = input.str("model")
        && crate::run::parse_model(&m).is_err()
    {
        write_stderr("\x1b[91m\x1b[1mError: \x1b[0m--model must use the format provider/model[#variant]\n");
        return Ok(1);
    }
    let opts = tui_options(input, &server, server.service.is_none());
    let code = oc_tui::run_mini(opts, oc_tui::MiniOptions { replay: input.opt_bool("replay"), replay_limit: input.int("replayLimit") }).await?;
    drop(server);
    Ok(code)
}

// ---------------------------------------------------------------------------------------------
// debug

fn debug_paths(global: &Global, input: &Input) -> anyhow::Result<i32> {
    let db = global.database(oc_util::version::CHANNEL);
    let rows: Vec<(&str, String)> = vec![
        ("home", global.home.display().to_string()),
        ("data", global.data.display().to_string()),
        ("cache", global.cache.display().to_string()),
        ("config", global.config.display().to_string()),
        ("state", global.state.display().to_string()),
        ("tmp", global.tmp.display().to_string()),
        ("bin", global.bin.display().to_string()),
        ("log", global.log.display().to_string()),
        ("repos", global.repos.display().to_string()),
        ("db", db.display().to_string()),
    ];
    if let Some(name) = input.str("name") {
        let v = rows.iter().find(|(k, _)| *k == name).map(|(_, v)| v.clone()).unwrap_or_default();
        write_stdout(&format!("{v}{EOL}"));
        return Ok(0);
    }
    let mut out = String::new();
    for (k, v) in rows {
        out.push_str(&format!("{k:<10} {v}{EOL}"));
    }
    write_stdout(&out);
    Ok(0)
}

async fn debug_config(global: &Global) -> anyhow::Result<i32> {
    let server = connection::resolve(global, Args::default()).await?;
    let entries = server.client().call("config.get", Req::new().location(&cwd())).await?;
    write_stdout(&format!("{}{EOL}", pretty(&oc_config::redact::redact_config(&entries))));
    Ok(0)
}

async fn debug_agents(global: &Global) -> anyhow::Result<i32> {
    let server = connection::resolve(global, Args::default()).await?;
    let response = server.client().call("agent.list", Req::new().location(&cwd())).await?;
    let mut agents = response.get("data").and_then(Value::as_array).cloned().unwrap_or_default();
    agents.sort_by(|a, b| {
        crate::term::locale_cmp(a.get("id").and_then(Value::as_str).unwrap_or(""), b.get("id").and_then(Value::as_str).unwrap_or(""))
    });
    write_stdout(&format!("{}{EOL}", pretty(&Value::Array(agents))));
    Ok(0)
}

// ---------------------------------------------------------------------------------------------
// models / reload / api / pair

async fn models(global: &Global, input: &Input) -> anyhow::Result<i32> {
    let server = connection::resolve(global, server_args(input)).await?;
    let response = server.client().call("model.list", Req::new().location(&cwd())).await?;
    let mut models: Vec<String> = response
        .get("data")
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .map(|m| {
                    format!(
                        "{}/{}",
                        m.get("providerID").and_then(Value::as_str).unwrap_or(""),
                        m.get("id").and_then(Value::as_str).unwrap_or("")
                    )
                })
                .collect()
        })
        .unwrap_or_default();
    models.sort_by(|a, b| crate::term::locale_cmp(a, b));
    if !models.is_empty() {
        write_stdout(&format!("{}{EOL}", models.join(EOL)));
    }
    Ok(0)
}

async fn reload(global: &Global, input: &Input) -> anyhow::Result<i32> {
    let server = connection::resolve(global, server_args(input)).await?;
    server.client().call("location.reload", Req::new()).await?;
    write_stdout(&format!("Configuration reloaded{EOL}"));
    Ok(0)
}

const METHODS: &[&str] = &["delete", "get", "head", "options", "patch", "post", "put"];

/// `interpolate(path, params)`
pub fn interpolate(path: &str, params: &indexmap::IndexMap<String, String>) -> anyhow::Result<String> {
    let mut used = std::collections::HashSet::new();
    let mut out = String::new();
    let mut rest = path;
    while let Some(start) = rest.find('{') {
        let Some(end) = rest[start..].find('}') else { break };
        let name = &rest[start + 1..start + end];
        out.push_str(&rest[..start]);
        let value = params.get(name).ok_or_else(|| anyhow::anyhow!("Missing path parameter: {name}"))?;
        used.insert(name.to_string());
        out.push_str(&crate::client::encode_component(value));
        rest = &rest[start + end + 1..];
    }
    out.push_str(rest);
    let query: Vec<(&String, &String)> = params.iter().filter(|(k, _)| !used.contains(k.as_str())).collect();
    if query.is_empty() {
        return Ok(out);
    }
    let mut s = url::form_urlencoded::Serializer::new(String::new());
    for (k, v) in query {
        s.append_pair(k, v);
    }
    Ok(format!("{out}?{}", s.finish()))
}

pub fn resolve_operation(spec: &Value, operation: &str, params: &indexmap::IndexMap<String, String>) -> anyhow::Result<(String, String)> {
    if let Some(paths) = spec.get("paths").and_then(Value::as_object) {
        for (path, ops) in paths {
            let Some(ops) = ops.as_object() else { continue };
            for (method, op) in ops {
                if !METHODS.contains(&method.as_str()) || op.get("operationId").and_then(Value::as_str) != Some(operation) {
                    continue;
                }
                return Ok((method.to_uppercase(), interpolate(path, params)?));
            }
        }
    }
    anyhow::bail!("Operation not found: {operation}")
}

async fn api(global: &Global, input: &Input) -> anyhow::Result<i32> {
    let mut args = server_args(input);
    args.mismatch = Some(Mismatch::Ignore);
    let server = connection::resolve(global, args).await?;
    let client = server.client();
    let params = input.map("param").unwrap_or_default();
    let request = input.list("request");
    let (method, path) = if request.len() == 2 && METHODS.contains(&request[0].to_lowercase().as_str()) && request[1].starts_with('/') {
        (request[0].to_uppercase(), request[1].clone())
    } else {
        if request.len() != 1 {
            anyhow::bail!("Expected an operation name or an HTTP method and path");
        }
        let auth: Vec<(String, String)> = server.endpoint.authorization().map(|a| vec![("authorization".to_string(), a)]).unwrap_or_default();
        let response = client.raw("GET", "/openapi.json", &auth, None).await?;
        if !response.status().is_success() {
            anyhow::bail!("Failed to load OpenAPI document: HTTP {}", response.status().as_u16());
        }
        let spec: Value = response.json().await?;
        resolve_operation(&spec, &request[0], &params)?
    };
    let mut headers: indexmap::IndexMap<String, (String, String)> = indexmap::IndexMap::new();
    if let Some(a) = server.endpoint.authorization() {
        headers.insert("authorization".into(), ("authorization".into(), a));
    }
    for h in input.list("header") {
        let index = h.find(':').filter(|i| *i >= 1).ok_or_else(|| anyhow::anyhow!("Invalid header, expected name:value: {h}"))?;
        let name = h[..index].trim().to_string();
        headers.insert(name.to_lowercase(), (name, h[index + 1..].trim().to_string()));
    }
    let body = input.str("data");
    if body.is_some() && !headers.contains_key("content-type") {
        headers.insert("content-type".into(), ("content-type".into(), "application/json".into()));
    }
    let list: Vec<(String, String)> = headers.into_values().collect();
    let response = client.raw(&method, &path, &list, body).await?;
    let status = response.status();
    let output = response.text().await.unwrap_or_default();
    if !output.is_empty() {
        write_stdout(&format!("{output}{}", if output.ends_with(EOL) { "" } else { EOL }));
    }
    if !status.is_success() {
        write_stderr(&format!("HTTP {} {}{EOL}", status.as_u16(), status.canonical_reason().unwrap_or("")));
        return Ok(1);
    }
    Ok(0)
}

fn is_loopback(hostname: &str) -> bool {
    hostname == "localhost" || hostname.ends_with(".localhost") || hostname.starts_with("127.") || hostname == "[::1]"
}

/// `uqr.renderUnicodeCompact(data, {border: 2})`: two modules per character (`▀ ▄ █`).
pub fn qr_compact(data: &str) -> String {
    let Ok(code) = qrcode::QrCode::with_error_correction_level(data.as_bytes(), qrcode::EcLevel::M) else { return String::new() };
    let width = code.width();
    let border = 2usize;
    let size = width + border * 2;
    let dark = |x: usize, y: usize| -> bool {
        if x < border || y < border || x >= width + border || y >= width + border {
            return false;
        }
        code[(x - border, y - border)] == qrcode::Color::Dark
    };
    let mut rows = vec![];
    let mut y = 0;
    while y < size {
        let mut line = String::new();
        for x in 0..size {
            let top = dark(x, y);
            let bottom = y + 1 < size && dark(x, y + 1);
            line.push(match (top, bottom) {
                (true, true) => '█',
                (true, false) => '▀',
                (false, true) => '▄',
                (false, false) => ' ',
            });
        }
        rows.push(line);
        y += 2;
    }
    rows.join("\n")
}

async fn pair(global: &Global, input: &Input) -> anyhow::Result<i32> {
    let config = ServiceConfig::new(global);
    let info = config.read();
    if info.disabled == Some(true) {
        anyhow::bail!("Pairing requires the background service; run `ocd service unset disabled` first");
    }
    let remote = input.flag("remote");
    let url = input.str("url");
    if remote && url.is_some() {
        anyhow::bail!("--remote cannot be combined with --url");
    }
    if remote {
        anyhow::bail!("Remote access through OpenTunnel is not supported by this build");
    }
    let endpoint = service::ensure(&config.options(false), None).await?;
    let client = Client::new(&endpoint);
    let urls: Vec<String> = match &url {
        Some(u) => vec![u.clone()],
        None => client
            .call("server.info", Req::new())
            .await?
            .get("urls")
            .and_then(Value::as_array)
            .map(|a| a.iter().filter_map(|u| u.as_str().map(String::from)).collect())
            .unwrap_or_default(),
    };
    let pairing = client.call("server.pair", Req::new()).await?;
    let code = pairing.get("code").and_then(Value::as_str).unwrap_or("").to_string();
    let expires = pairing.get("expires_in").and_then(Value::as_f64).unwrap_or(300.0);
    let links: Vec<String> = urls
        .iter()
        .filter_map(|u| url::Url::parse(u).ok().and_then(|b| b.join(&format!("/auth/connect/{code}")).ok()).map(|l| l.to_string()))
        .collect();
    let remote_urls: Vec<&String> = urls
        .iter()
        .filter(|u| url::Url::parse(u).ok().and_then(|p| p.host_str().map(|h| !is_loopback(&host_display(&p, h)))).unwrap_or(false))
        .collect();
    let mut lines = vec![String::new(), format!("  Open a link to connect. Links work once and expire in {} minutes.", (expires / 60.0).round()), String::new()];
    if links.is_empty() {
        lines.push("  (no server URLs)".into());
    } else {
        lines.extend(links.iter().map(|l| format!("  {l}")));
    }
    if !remote_urls.is_empty() {
        lines.push(String::new());
        let qr = qr_compact(&serde_json::json!({"code": code, "urls": remote_urls}).to_string());
        lines.push(qr.split('\n').map(|l| format!("  {l}")).collect::<Vec<_>>().join(EOL));
    }
    lines.push(String::new());
    write_stdout(&format!("{}{EOL}", lines.join(EOL)));
    if url.is_some() {
        return Ok(0);
    }
    let parsed = url::Url::parse(&endpoint.url)?;
    let host = parsed.host_str().map(|h| host_display(&parsed, h)).unwrap_or_default();
    if !["localhost", "127.0.0.1", "[::1]"].contains(&host.as_str()) {
        return Ok(0);
    }
    let port = parsed.port().map(|p| p.to_string()).unwrap_or_default();
    write_stderr(
        &([
            "  Over SSH? Forward the port, then open the link on your machine:".to_string(),
            format!("  ssh -L {port}:{host}:{port} <host>"),
            format!("  If port {port} is busy locally, forward another port and use it in the link."),
            String::new(),
            "  To connect from other devices, run `ocd service set hostname 0.0.0.0`.".to_string(),
            String::new(),
        ]
        .join(EOL)
            + EOL),
    );
    Ok(0)
}

/// WHATWG `url.hostname` (IPv6 bracketed).
fn host_display(u: &url::Url, h: &str) -> String {
    match u.host() {
        Some(url::Host::Ipv6(_)) => format!("[{}]", h.trim_start_matches('[').trim_end_matches(']')),
        _ => h.to_string(),
    }
}

// ---------------------------------------------------------------------------------------------
// service

async fn service_start(global: &Global) -> anyhow::Result<i32> {
    let endpoint = service::ensure(&ServiceConfig::new(global).options(false), None).await?;
    write_stdout(&format!("{}{EOL}", endpoint.url));
    Ok(0)
}

async fn service_status(global: &Global) -> anyhow::Result<i32> {
    let options = ServiceConfig::new(global).options(false);
    let found = service::discover(&options.file, None).await;
    write_stdout(&format!("{}{EOL}", found.map(|e| e.url).unwrap_or_else(|| "stopped".into())));
    Ok(0)
}

async fn service_stop(global: &Global) -> anyhow::Result<i32> {
    let options = ServiceConfig::new(global).options(false);
    service::shutdown_persistent_pty(&options.file).await;
    service::stop(&options.file, Pty::Clear).await?;
    Ok(0)
}

async fn service_restart(global: &Global) -> anyhow::Result<i32> {
    let options = ServiceConfig::new(global).options(false);
    service::shutdown_persistent_pty(&options.file).await;
    service::stop(&options.file, Pty::Clear).await?;
    let endpoint = service::ensure(&options, None).await?;
    write_stdout(&format!("{}{EOL}", endpoint.url));
    Ok(0)
}

/// `disableRemote`: forgets the route; having had remote access on counts as accepting it.
fn disable_remote(info: &mut crate::service::ServiceInfo) {
    if info.remote.take().is_some() {
        info.remote_accepted = Some(true);
    }
}

const KEYS: &[&str] = &["disabled", "remote", "hostname", "port", "password", "cors", "env"];

fn config_key(key: &str) -> anyhow::Result<&'static str> {
    KEYS.iter().copied().find(|k| *k == key).ok_or_else(|| anyhow::anyhow!("Unknown service config key: {key}"))
}

async fn service_get(global: &Global, input: &Input) -> anyhow::Result<i32> {
    let config = ServiceConfig::new(global);
    let key = input.str("key");
    let name = input.str("name");
    let out = match key {
        None => {
            let mut info = config.read();
            info.password = None;
            let mut v = serde_json::to_value(&info)?;
            if info.remote.is_some() {
                v["remote"] = serde_json::json!({});
            }
            pretty(&v)
        }
        Some(key) => {
            let selected = config_key(&key)?;
            if selected != "env" && name.is_some() {
                anyhow::bail!("Usage: ocd service get {selected}");
            }
            let info = config.read();
            match selected {
                "disabled" => info.disabled.unwrap_or(false).to_string(),
                "remote" => info.remote.is_some().to_string(),
                "hostname" => info.hostname.unwrap_or_default(),
                "port" => info.port.map(|p| p.to_string()).unwrap_or_default(),
                "password" => config.password(None)?,
                "cors" => pretty(&serde_json::to_value(info.cors.unwrap_or_default())?),
                _ => {
                    let env = info.env.unwrap_or_default();
                    match name {
                        None => pretty(&serde_json::to_value(env)?),
                        Some(n) => env.get(&n).cloned().unwrap_or_default(),
                    }
                }
            }
        }
    };
    write_stdout(&format!("{out}{EOL}"));
    Ok(0)
}

async fn stop_service(config: &ServiceConfig) -> anyhow::Result<()> {
    service::stop(&config.options(false).file, Pty::Clear).await
}

async fn service_set(global: &Global, input: &Input) -> anyhow::Result<i32> {
    let config = ServiceConfig::new(global);
    let key = input.str("key").unwrap_or_default();
    let value = input.str("value").unwrap_or_default();
    let nested = input.str("nestedValue");
    let selected = config_key(&key)?;
    if selected != "env" && nested.is_some() {
        anyhow::bail!("Usage: ocd service set {selected} <value>");
    }
    match selected {
        "disabled" => {
            if value != "true" && value != "false" {
                anyhow::bail!("Disabled must be true or false");
            }
            if value == "true" {
                stop_service(&config).await?;
            }
            let mut info = config.read();
            info.disabled = Some(value == "true");
            config.write(&info)?;
        }
        "remote" => {
            if value != "true" && value != "false" {
                anyhow::bail!("Remote must be true or false");
            }
            if value == "true" {
                anyhow::bail!("Remote access through OpenTunnel is not supported by this build");
            }
            stop_service(&config).await?;
            let mut info = config.read();
            disable_remote(&mut info);
            config.write(&info)?;
        }
        "hostname" => {
            stop_service(&config).await?;
            let mut info = config.read();
            info.hostname = Some(value);
            config.write(&info)?;
        }
        "port" => {
            let port = js_number(&value);
            if !port.is_some_and(|p| p.fract() == 0.0 && (1.0..=65_535.0).contains(&p)) {
                anyhow::bail!("Port must be between 1 and 65535");
            }
            stop_service(&config).await?;
            let mut info = config.read();
            info.port = port.map(|p| p as i64);
            config.write(&info)?;
        }
        "password" => {
            stop_service(&config).await?;
            config.password(Some(&value))?;
        }
        "env" => {
            let Some(nested) = nested else { anyhow::bail!("Usage: ocd service set env <key> <value>") };
            stop_service(&config).await?;
            let mut info = config.read();
            info.env.get_or_insert_with(Default::default).insert(value, nested);
            config.write(&info)?;
        }
        _ => {
            let cors: Vec<String> = value.split(',').map(|o| o.trim().to_string()).collect();
            let bad = cors.iter().any(|origin| match url::Url::parse(origin) {
                Err(_) => true,
                Ok(u) => !matches!(u.scheme(), "http" | "https") || u.origin().ascii_serialization() != *origin,
            });
            if bad {
                anyhow::bail!("CORS must be a comma-separated list of HTTP(S) origins without paths or trailing slashes");
            }
            stop_service(&config).await?;
            let mut info = config.read();
            info.cors = Some(cors);
            config.write(&info)?;
        }
    }
    Ok(0)
}

/// JS `Number(value)` for decimal strings (trimmed; empty → 0).
fn js_number(v: &str) -> Option<f64> {
    let t = v.trim();
    if t.is_empty() {
        return Some(0.0);
    }
    if let Some(hex) = t.strip_prefix("0x").or_else(|| t.strip_prefix("0X")) {
        return i64::from_str_radix(hex, 16).ok().map(|n| n as f64);
    }
    if t.chars().any(|c| c.is_ascii_alphabetic() && c != 'e' && c != 'E') {
        return if t == "Infinity" || t == "+Infinity" { Some(f64::INFINITY) } else { None };
    }
    t.parse::<f64>().ok()
}

async fn service_unset(global: &Global, input: &Input) -> anyhow::Result<i32> {
    let config = ServiceConfig::new(global);
    let key = input.str("key").unwrap_or_default();
    let name = input.str("name");
    let selected = config_key(&key)?;
    if selected != "env" && name.is_some() {
        anyhow::bail!("Usage: ocd service unset {selected}");
    }
    if selected == "env" && name.is_none() {
        anyhow::bail!("Usage: ocd service unset env <key>");
    }
    if selected != "disabled" {
        stop_service(&config).await?;
    }
    let mut info = config.read();
    match selected {
        "disabled" => info.disabled = None,
        "remote" => disable_remote(&mut info),
        "hostname" => info.hostname = None,
        "port" => info.port = None,
        "password" => info.password = None,
        "cors" => info.cors = None,
        _ => {
            let mut env = info.env.take().unwrap_or_default();
            env.shift_remove(name.as_deref().unwrap_or(""));
            info.env = (!env.is_empty()).then_some(env);
        }
    }
    config.write(&info)?;
    Ok(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn interpolates_like_api_ts() {
        let mut p = indexmap::IndexMap::new();
        p.insert("sessionID".to_string(), "ses/a".to_string());
        p.insert("workspace".to_string(), "work".to_string());
        assert_eq!(interpolate("/api/session/{sessionID}", &p).unwrap(), "/api/session/ses%2Fa?workspace=work");
        assert!(interpolate("/x/{missing}", &p).unwrap_err().to_string().contains("Missing path parameter: missing"));
    }

    #[test]
    fn js_numbers() {
        assert_eq!(js_number(" 80 "), Some(80.0));
        assert_eq!(js_number("1e3"), Some(1000.0));
        assert_eq!(js_number("abc"), None);
    }
}

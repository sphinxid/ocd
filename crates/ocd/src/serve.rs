//! `opencode serve` (`handlers/serve.ts`, `server-process.ts`): foreground, `--stdio` (private
//! standalone child) and `--service` (background service election + registration) modes.

use std::time::{Duration, Instant};

use oc_server::service::{Registration, write_registration};
use oc_server::webui::WebUiSource;
use oc_util::Global;
use oc_util::version::{CHANNEL, VERSION};

use crate::service::{self, ServiceConfig};

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Mode {
    Default,
    Service,
    Stdio,
}

pub struct Options {
    pub mode: Mode,
    pub hostname: Option<String>,
    pub port: Option<i64>,
    pub cors: Option<Vec<String>>,
}

fn service_url(hostname: &str, port: u16) -> String {
    if hostname.contains(':') { format!("http://[{hostname}]:{port}") } else { format!("http://{hostname}:{port}") }
}

fn addr_in_use(e: &anyhow::Error) -> bool {
    e.chain().any(|c| c.downcast_ref::<std::io::Error>().is_some_and(|io| io.kind() == std::io::ErrorKind::AddrInUse))
        || e.to_string().contains("Address already in use")
}

fn server_options(
    hostname: &str,
    port: Option<u16>,
    cors: Vec<String>,
    password: &str,
    global: &Global,
    web_ui: &WebUiSource,
) -> oc_server::ServerOptions {
    oc_server::ServerOptions {
        hostname: hostname.to_string(),
        port,
        password: Some(password.to_string()),
        cors,
        version: VERSION.to_string(),
        tmp_dir: global.tmp.display().to_string(),
        cwd: None,
        web_ui: Some(web_ui.clone()),
        ready: false,
    }
}

/// Wait for SIGINT, SIGTERM or SIGHUP and log which one ended the server: a managed service is
/// detached, so this line is the only trace of who stopped it.
async fn wait_for_signal() {
    #[cfg(unix)]
    let name = {
        use tokio::signal::unix::{SignalKind, signal};
        let mut term = signal(SignalKind::terminate()).ok();
        let mut hup = signal(SignalKind::hangup()).ok();
        tokio::select! {
            _ = tokio::signal::ctrl_c() => "SIGINT",
            _ = async { match term.as_mut() { Some(s) => { s.recv().await; } None => std::future::pending().await } } => "SIGTERM",
            _ = async { match hup.as_mut() { Some(s) => { s.recv().await; } None => std::future::pending().await } } => "SIGHUP",
        }
    };
    #[cfg(not(unix))]
    let name = {
        let _ = tokio::signal::ctrl_c().await;
        "ctrl-c"
    };
    tracing::info!(signal = name, "server stopping");
}

pub async fn run(global: &Global, opts: Options) -> anyhow::Result<i32> {
    // `OPENCODE_PTY_HANDOFF` (published by the client that stopped the previous service) is
    // consumed at boot by the backend (`oc_persistent_pty::handoff::take_from_env()`); here it is
    // only validated so an unusable ticket is dropped with upstream's warning.
    if let Ok(handoff) = std::env::var("OPENCODE_PTY_HANDOFF") {
        let valid = serde_json::from_str::<serde_json::Value>(&handoff).ok().is_some_and(|v| {
            v.get("directory").is_some_and(|x| x.is_string())
                && v.get("instanceID").is_some_and(|x| x.is_string())
                && v.get("ticket").is_some_and(|x| x.is_string())
                && v.get("expiresAt").is_some_and(|x| x.is_number())
        });
        if !valid {
            unsafe { std::env::remove_var("OPENCODE_PTY_HANDOFF") };
            tracing::warn!("Ignoring invalid PTY restart handoff; persistent terminals will start fresh");
        }
    }
    let port = match opts.port {
        Some(p) if !(0..=65_535).contains(&p) => anyhow::bail!("Invalid port: {p}"),
        Some(p) => Some(p as u16),
        None => None,
    };
    // Every mode serves the web UI for non-API paths (`WebUi.handler()` in server-process.ts).
    let web_ui = crate::web_ui::load()?;
    match opts.mode {
        Mode::Service => run_service(global, opts.hostname, port, opts.cors, web_ui).await,
        Mode::Stdio | Mode::Default => run_plain(global, opts.mode, opts.hostname, port, opts.cors, web_ui).await,
    }
}

async fn run_plain(
    global: &Global,
    mode: Mode,
    hostname: Option<String>,
    port: Option<u16>,
    cors: Option<Vec<String>>,
    web_ui: WebUiSource,
) -> anyhow::Result<i32> {
    let environment = crate::connection::env_password();
    if mode == Mode::Stdio {
        unsafe {
            std::env::remove_var("OPENCODE_PASSWORD");
            std::env::remove_var("OPENCODE_SERVER_PASSWORD");
        }
    }
    let password = match &environment {
        Some(p) if !p.is_empty() => p.clone(),
        Some(_) => anyhow::bail!("Missing server password"),
        None => oc_server::service::generate_password(),
    };
    let hostname = hostname.unwrap_or_else(|| "127.0.0.1".into());
    let rt = crate::runtime::build(global)?;
    let (server, _engine) = crate::runtime::serve(&rt, server_options(&hostname, port, cors.unwrap_or_default(), &password, global, &web_ui)).await?;
    let url = server.url().to_string();
    if mode == Mode::Stdio {
        crate::term::write_stdout(&format!("{}\n", serde_json::json!({ "url": url })));
        // The owner holds our stdin; EOF ends the lease.
        let (tx, rx) = tokio::sync::oneshot::channel::<()>();
        std::thread::spawn(move || {
            use std::io::Read;
            let mut buf = [0u8; 1024];
            let mut stdin = std::io::stdin();
            while let Ok(n) = stdin.read(&mut buf) {
                if n == 0 {
                    break;
                }
            }
            let _ = tx.send(());
        });
        tokio::select! {
            _ = rx => {}
            _ = wait_for_signal() => {}
        }
        server.shutdown().await;
        rt.app.plugins.shutdown().await;
        rt.app.model_transport.shutdown().await;
        return Ok(0);
    }
    crate::term::write_stdout(&format!("server listening on {url}\n"));
    if environment.is_none() {
        crate::term::write_stdout(&format!("server password {password}\n"));
    }
    wait_for_signal().await;
    server.shutdown().await;
    rt.app.plugins.shutdown().await;
    rt.app.model_transport.shutdown().await;
    Ok(0)
}

async fn run_service(
    global: &Global,
    hostname: Option<String>,
    port: Option<u16>,
    cors: Option<Vec<String>>,
    web_ui: WebUiSource,
) -> anyhow::Result<i32> {
    // A sandbox may deny $HOME; the service then keeps the cwd it was launched from (63c1a27af).
    if let Err(cause) = std::env::set_current_dir(&global.home) {
        tracing::warn!(cause = %cause, "failed to change service directory to home");
    }
    let config = ServiceConfig::new(global);
    let options = config.options(false);
    let info = config.read();
    let hostname = hostname.or(info.hostname.clone()).unwrap_or_else(|| "127.0.0.1".into());
    let port = port.or(info.port.map(|p| p as u16)).unwrap_or_else(|| service::default_port(CHANNEL));
    let url = service_url(&hostname, port);
    if service::incumbent(&options.file, &url, service::PROBE_TIMEOUT).await.is_some() {
        return Ok(0);
    }
    let password = info.password.clone().filter(|p| !p.is_empty()).unwrap_or_else(oc_server::service::generate_password);
    let cors = cors.or(info.cors.clone()).unwrap_or_default();

    // Election: whoever binds the fixed port owns the service.
    let host = hostname.trim_start_matches('[').trim_end_matches(']').to_string();
    let deadline = Instant::now() + Duration::from_secs(15);
    let mut contended = false;
    loop {
        match std::net::TcpListener::bind((host.as_str(), port)) {
            Ok(l) => {
                drop(l);
                break;
            }
            Err(e) if e.kind() == std::io::ErrorKind::AddrInUse => {
                contended = true;
                if Instant::now() >= deadline {
                    anyhow::bail!(
                        "Managed service port {port} on {hostname} is already in use by another process. Configure another port with `ocd service set port <port>` and start the service again."
                    );
                }
                let left = deadline.saturating_duration_since(Instant::now());
                if service::incumbent(&options.file, &url, left.min(service::PROBE_TIMEOUT)).await.is_some() {
                    return Ok(0);
                }
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
            Err(e) => return Err(e.into()),
        }
    }
    let _ = contended;
    let rt = crate::runtime::build(global)?;
    let server = loop {
        match crate::runtime::serve(&rt, server_options(&hostname, Some(port), cors.clone(), &password, global, &web_ui)).await {
            Ok((s, _engine)) => break s,
            Err(e) if addr_in_use(&e) => {
                if Instant::now() >= deadline {
                    anyhow::bail!(
                        "Managed service port {port} on {hostname} is already in use by another process. Configure another port with `ocd service set port <port>` and start the service again."
                    );
                }
                if service::incumbent(&options.file, &url, service::PROBE_TIMEOUT).await.is_some() {
                    return Ok(0);
                }
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
            Err(e) => return Err(e),
        }
    };
    // onListen: persist the password, then register.
    if info.password.as_deref().is_none_or(str::is_empty) {
        config.password(Some(&password))?;
    }
    let bound = server.url().to_string();
    let registration = Registration::new(VERSION, &bound, &password);
    write_registration(&options.file, &registration)?;
    if info.remote.is_some() {
        tracing::warn!("remote access is configured but the OpenTunnel client is not supported by this build");
    }
    crate::term::write_stdout(&format!("server listening on {bound}\n"));
    let file = options.file.clone();
    tokio::select! {
        _ = oc_server::service::watch_registration(file.clone(), registration.clone()) => {}
        _ = wait_for_signal() => {}
    }
    server.shutdown().await;
    rt.app.plugins.shutdown().await;
    rt.app.model_transport.shutdown().await;
    oc_server::service::remove_registration_if_owned(&file, &registration);
    Ok(0)
}

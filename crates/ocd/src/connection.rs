//! `ServerConnection.resolve` (`cli/src/services/server-connection.ts`) and the private
//! standalone server (`cli/src/services/standalone.ts`).

use std::io::{BufRead, BufReader};
use std::process::{Child, Stdio};
use std::time::Duration;

use oc_util::Global;
use oc_util::version::VERSION;

use crate::client::{Client, Endpoint, Req};
use crate::service::{self, EnsureOptions, OnStart, Pty, ServiceConfig};

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Mismatch {
    Replace,
    Ignore,
    #[allow(dead_code)]
    Error,
}

#[derive(Default)]
pub struct Args<'a> {
    pub server: Option<String>,
    pub standalone: bool,
    pub mismatch: Option<Mismatch>,
    pub on_start: OnStart<'a>,
}

/// A private `serve --stdio` child owned by this process.
pub struct Standalone {
    child: Child,
}

impl Standalone {
    /// `Standalone.start()`
    pub fn start(global_cwd: &std::path::Path) -> anyhow::Result<(Standalone, Endpoint)> {
        let password = oc_server::service::generate_password();
        let mut cmd = std::process::Command::new(service::self_exe());
        cmd.args(["serve", "--stdio", "--port", "0"])
            .current_dir(global_cwd)
            .env("OPENCODE_PASSWORD", &password)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(if std::env::var("OPENCODE_PRINT_LOGS").as_deref() == Ok("1") { Stdio::inherit() } else { Stdio::null() });
        let mut child = cmd.spawn()?;
        let stdout = child.stdout.take().ok_or_else(|| anyhow::anyhow!("Standalone server exited before reporting readiness"))?;
        let (tx, rx) = std::sync::mpsc::channel::<Option<String>>();
        std::thread::spawn(move || {
            let mut reader = BufReader::new(stdout);
            let mut first = true;
            let mut line = String::new();
            loop {
                line.clear();
                match reader.read_line(&mut line) {
                    Ok(0) | Err(_) => break,
                    Ok(_) => {
                        if first {
                            let _ = tx.send(Some(line.trim_end_matches(['\n', '\r']).to_string()));
                            first = false;
                        }
                    }
                }
            }
            if first {
                let _ = tx.send(None);
            }
        });
        let line = rx.recv().ok().flatten();
        let Some(line) = line else {
            let _ = child.kill();
            anyhow::bail!("Standalone server exited before reporting readiness");
        };
        let ready: serde_json::Value = serde_json::from_str(&line)?;
        let url = ready.get("url").and_then(|u| u.as_str()).ok_or_else(|| anyhow::anyhow!("Invalid standalone readiness line"))?;
        Ok((Standalone { child }, Endpoint { url: url.to_string(), password: Some(password) }))
    }

    /// Wait for the child to exit (blocking); `(code, signal)`.
    pub fn child_mut(&mut self) -> &mut Child {
        &mut self.child
    }
}

impl Drop for Standalone {
    fn drop(&mut self) {
        // Closing the stdin lease ends the server; SIGTERM, then force-kill after 3 s.
        drop(self.child.stdin.take());
        #[cfg(unix)]
        unsafe {
            libc::kill(self.child.id() as i32, libc::SIGTERM);
        }
        let deadline = std::time::Instant::now() + Duration::from_secs(3);
        while std::time::Instant::now() < deadline {
            if let Ok(Some(_)) = self.child.try_wait() {
                return;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// `ServerConnection.Resolved`
pub struct Resolved {
    pub endpoint: Endpoint,
    /// Present for the managed background service (`reconnect`/`restart` controls).
    pub service: Option<EnsureOptions>,
    #[allow(dead_code)] // owned for its lifetime (dropping it ends the private server)
    pub standalone: Option<Standalone>,
}

#[allow(dead_code)] // `service.reconnect/restart` controls (TUI hooks)
impl Resolved {
    pub fn client(&self) -> Client {
        Client::new(&self.endpoint)
    }

    /// `service.reconnect()`
    pub async fn reconnect(&self) -> anyhow::Result<Endpoint> {
        let Some(opts) = &self.service else { anyhow::bail!("not a managed service") };
        let mut o = opts.clone();
        o.version = None;
        service::ensure(&o, None).await
    }

    /// `service.restart()`
    pub async fn restart(&self) -> anyhow::Result<Endpoint> {
        let Some(opts) = &self.service else { anyhow::bail!("not a managed service") };
        service::stop(&opts.file, Pty::Handoff).await?;
        let mut o = opts.clone();
        o.version = None;
        service::ensure(&o, None).await
    }
}

/// The directory the CLI started in (standalone children run there).
pub fn startup_dir() -> std::path::PathBuf {
    static DIR: std::sync::OnceLock<std::path::PathBuf> = std::sync::OnceLock::new();
    DIR.get_or_init(|| std::env::current_dir().unwrap_or_default()).clone()
}

/// `Env.password`
pub fn env_password() -> Option<String> {
    std::env::var("OPENCODE_PASSWORD").ok().or_else(|| std::env::var("OPENCODE_SERVER_PASSWORD").ok())
}

/// `Env.session()`: the process environment minus the server passwords.
pub fn session_env() -> indexmap::IndexMap<String, String> {
    std::env::vars().filter(|(k, _)| k != "OPENCODE_PASSWORD" && k != "OPENCODE_SERVER_PASSWORD").collect()
}

pub async fn resolve(global: &Global, args: Args<'_>) -> anyhow::Result<Resolved> {
    if args.server.is_some() && args.standalone {
        anyhow::bail!("--server and --standalone cannot be combined");
    }
    if let Some(server) = args.server {
        let endpoint = Endpoint { url: server.clone(), password: env_password() };
        let client = Client::new(&endpoint);
        let info = client.call("server.info", Req::new().timeout(Duration::from_secs(5))).await.map_err(|e| {
            if e.tag() == Some("UnauthorizedError") {
                if endpoint.password.is_none() {
                    anyhow::anyhow!("Server at {} requires a password; set OPENCODE_PASSWORD", endpoint.url)
                } else {
                    anyhow::anyhow!("Server at {} rejected the password", endpoint.url)
                }
            } else if e.is_transport() {
                anyhow::anyhow!("Could not reach server at {}", endpoint.url)
            } else {
                anyhow::anyhow!("Server at {} did not provide a compatible V2 health response", endpoint.url)
            }
        })?;
        let version = info.get("version").and_then(|v| v.as_str()).ok_or_else(|| {
            anyhow::anyhow!("Server at {} did not provide a compatible V2 health response", endpoint.url)
        })?;
        if version != VERSION {
            crate::term::write_stderr(&format!(
                "Warning: Server at {} has version {version}; this client is {VERSION}. Continuing anyway.\n",
                endpoint.url
            ));
        }
        return Ok(Resolved { endpoint, service: None, standalone: None });
    }
    let config = ServiceConfig::new(global);
    if args.standalone || config.read().disabled == Some(true) {
        let (child, endpoint) = Standalone::start(&startup_dir())?;
        return Ok(Resolved { endpoint, service: None, standalone: Some(child) });
    }
    let mismatch = args.mismatch.unwrap_or(Mismatch::Ignore);
    let options = config.options(mismatch != Mismatch::Ignore);
    let endpoint = match mismatch {
        Mismatch::Replace => service::ensure(&options, args.on_start).await?,
        Mismatch::Ignore => {
            let mut o = options.clone();
            o.version = None;
            service::ensure(&o, args.on_start).await?
        }
        Mismatch::Error => match service::discover(&options.file, options.version.as_deref()).await {
            Some(e) => e,
            None => {
                if service::discover(&options.file, None).await.is_some() {
                    anyhow::bail!("Background server version does not match this client");
                }
                service::ensure(&options, args.on_start).await?
            }
        },
    };
    Ok(Resolved { endpoint, service: Some(options), standalone: None })
}

//! The background service: CLI-owned service config (`cli/src/services/service-config.ts`), the
//! client-side discovery contract (`client/src/effect/service.ts`, `service-probe.ts`,
//! `service-contender.ts`, `pty-handoff.ts`) and termination.

use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use indexmap::IndexMap;
use oc_server::service::{Registration, read_registration};
use oc_util::Global;
use oc_util::version::{CHANNEL, VERSION};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::client::{Client, Endpoint, Req};

// ---------------------------------------------------------------------------------------------
// timing (`service-timing.ts`)

pub const POLL: Duration = Duration::from_millis(25);
pub const PROBE_TIMEOUT: Duration = Duration::from_millis(2_000);
pub const SPAWN_DELAY: Duration = Duration::from_millis(5_000);
pub const MAX_SPAWN_DELAY: Duration = Duration::from_millis(30_000);
pub const ENSURE_LIMIT: Duration = Duration::from_millis(120_000);
pub const STOP_POLL: Duration = Duration::from_millis(50);
pub const STOP_ATTEMPTS: usize = 100;

// ---------------------------------------------------------------------------------------------
// service config

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Remote {
    pub route: String,
}

/// `ServiceConfig.Info` (keys serialized in schema order).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ServiceInfo {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub disabled: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub remote: Option<Remote>,
    /// Set once remote access was accepted or had been on, so turning it on again doesn't ask twice.
    #[serde(default, rename = "remoteAccepted", skip_serializing_if = "Option::is_none")]
    pub remote_accepted: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hostname: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub port: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub password: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cors: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub env: Option<IndexMap<String, String>>,
}

fn decode_info(text: &str) -> Option<ServiceInfo> {
    let info: ServiceInfo = serde_json::from_str(text).ok()?;
    if info.port.is_some_and(|p| !(1..=65_535).contains(&p)) || info.remote_accepted == Some(false) {
        return None;
    }
    Some(info)
}

/// The legacy form with `"remote": true|false`.
fn decode_legacy(text: &str) -> Option<(ServiceInfo, bool)> {
    let mut v: Value = serde_json::from_str(text).ok()?;
    let remote = v.as_object_mut()?.remove("remote")?.as_bool()?;
    let info = decode_info(&v.to_string())?;
    Some((info, remote))
}

pub fn filename(channel: &str) -> String {
    oc_server::service::service_file_name(channel)
}

pub fn default_port(channel: &str) -> u16 {
    oc_server::service::default_service_port(channel)
}

fn legacy_filename(channel: &str) -> Option<String> {
    if channel == "latest" || channel == "local" {
        return None;
    }
    Some(format!("service-{}.json", oc_util::hash::fast(channel)))
}

/// `versionBelongsToChannel`
pub fn version_belongs_to_channel(version: Option<&str>, channel: &str, installed: &str) -> bool {
    let Some(version) = version else { return false };
    if version == installed {
        return true;
    }
    let prefix = format!("0.0.0-{channel}-");
    let Some(rest) = version.strip_prefix(&prefix) else { return false };
    static RE: std::sync::LazyLock<regex::Regex> = std::sync::LazyLock::new(|| regex::Regex::new(r"^\d+(?:\.\d+)?$").unwrap());
    RE.is_match(rest)
}

fn write_private_new(path: &Path, text: &str) {
    let mut o = std::fs::OpenOptions::new();
    o.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        o.mode(0o600);
    }
    if let Ok(mut f) = o.open(path) {
        use std::io::Write;
        let _ = f.write_all(text.as_bytes());
    }
}

fn write_private(path: &Path, text: &str) -> std::io::Result<()> {
    let mut o = std::fs::OpenOptions::new();
    o.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        o.mode(0o600);
    }
    use std::io::Write;
    o.open(path)?.write_all(text.as_bytes())
}

fn random_route() -> String {
    let mut b = [0u8; 8];
    rand::fill(&mut b);
    hex::encode(b)
}

/// CLI binding of the service operations (`ServiceConfig.options()` + paths).
#[derive(Debug, Clone)]
pub struct ServiceConfig {
    pub global: Global,
    pub channel: String,
}

/// `Service.EnsureOptions`
#[derive(Debug, Clone, Default)]
pub struct EnsureOptions {
    pub file: PathBuf,
    pub version: Option<String>,
    pub env: IndexMap<String, String>,
    pub command: Vec<String>,
}

impl ServiceConfig {
    pub fn new(global: &Global) -> Self {
        ServiceConfig { global: global.clone(), channel: CHANNEL.to_string() }
    }

    pub fn config_file(&self) -> PathBuf {
        self.global.config.join(filename(&self.channel))
    }

    pub fn registration_file(&self) -> PathBuf {
        self.global.state.join(filename(&self.channel))
    }

    /// `options({checkVersion})`, running the registration migrations.
    pub fn options(&self, check_version: bool) -> EnsureOptions {
        let file = self.registration_file();
        let name = filename(&self.channel);
        let mut legacy = vec![];
        if let Some(l) = legacy_filename(&self.channel) {
            legacy.push(self.global.state.join(l));
        }
        if name != "service.json" && self.channel != "local" {
            legacy.push(self.global.state.join("service.json"));
        }
        for l in legacy {
            if let Ok(text) = std::fs::read_to_string(&l)
                && let Some(reg) = read_registration_text(&text)
                && version_belongs_to_channel(reg.version.as_deref(), &self.channel, VERSION)
            {
                write_private_new(&file, &text);
            }
        }
        EnsureOptions {
            file,
            version: check_version.then(|| VERSION.to_string()),
            env: self.read().env.unwrap_or_default(),
            command: vec![self_exe(), "serve".into(), "--service".into()],
        }
    }

    /// `ServiceConfig.read()`
    pub fn read(&self) -> ServiceInfo {
        let file = self.config_file();
        if let Some(l) = legacy_filename(&self.channel) {
            let legacy = self.global.config.join(l);
            if let Ok(text) = std::fs::read_to_string(&legacy)
                && decode_info(&text).is_some()
            {
                write_private_new(&file, &text);
            }
        }
        let Ok(text) = std::fs::read_to_string(&file) else { return ServiceInfo::default() };
        if let Some(info) = decode_info(&text) {
            return info;
        }
        let Some((mut info, enabled)) = decode_legacy(&text) else { return ServiceInfo::default() };
        info.remote = enabled.then(|| Remote { route: random_route() });
        let _ = self.write(&info);
        info
    }

    /// `write(value)`: `<file>.tmp` (0600) + rename, `JSON.stringify(value, null, 2) + "\n"`.
    pub fn write(&self, info: &ServiceInfo) -> std::io::Result<()> {
        let file = self.config_file();
        if let Some(dir) = file.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let tmp = PathBuf::from(format!("{}.tmp", file.display()));
        write_private(&tmp, &(serde_json::to_string_pretty(info).unwrap_or_default() + "\n"))?;
        std::fs::rename(&tmp, &file)
    }

    /// `ServiceConfig.password(value?)`
    pub fn password(&self, value: Option<&str>) -> anyhow::Result<String> {
        let mut existing = self.read();
        if value.is_none()
            && let Some(p) = existing.password.as_ref().filter(|p| !p.is_empty())
        {
            return Ok(p.clone());
        }
        let next = value.map(String::from).unwrap_or_else(oc_server::service::generate_password);
        existing.password = Some(next.clone());
        self.write(&existing)?;
        Ok(next)
    }

    /// `ServiceConfig.remote()`: turns remote access on and returns its route, created once and kept so the remote
    /// URL survives restarts.
    pub fn remote(&self) -> anyhow::Result<String> {
        let mut existing = self.read();
        if let Some(r) = &existing.remote {
            return Ok(r.route.clone());
        }
        let next = random_route();
        existing.remote = Some(Remote { route: next.clone() });
        self.write(&existing)?;
        Ok(next)
    }
}

pub fn self_exe() -> String {
    std::env::current_exe().map(|p| p.display().to_string()).unwrap_or_else(|_| crate::cli::spec::CLI_NAME.into())
}

fn read_registration_text(text: &str) -> Option<Registration> {
    let r: Registration = serde_json::from_str(text).ok()?;
    (r.pid > 0).then_some(r)
}

// ---------------------------------------------------------------------------------------------
// probe (`service-probe.ts`)

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum State {
    Ready,
    Waiting,
    Failed,
}

#[derive(Debug, Clone)]
pub struct LocalService {
    pub info: Registration,
    pub endpoint: Endpoint,
    pub version: Option<String>,
    pub state: State,
    pub compatible: bool,
}

pub struct ProbeResult {
    pub service: Option<LocalService>,
    pub timed_out: bool,
}

fn http() -> reqwest::Client {
    reqwest::Client::builder().no_proxy().build().unwrap_or_default()
}

/// `probeResult(info, timeout)`
pub async fn probe(info: &Registration, timeout: Duration) -> ProbeResult {
    let endpoint = Endpoint { url: info.url.clone(), password: info.password.clone() };
    let Ok(url) = url::Url::parse(&info.url).and_then(|u| u.join("/api/info")) else {
        return ProbeResult { service: None, timed_out: false };
    };
    let mut req = http().get(url).timeout(timeout);
    if let Some(a) = endpoint.authorization() {
        req = req.header("authorization", a);
    }
    let response = match req.send().await {
        Ok(r) => r,
        Err(e) => return ProbeResult { service: None, timed_out: e.is_timeout() },
    };
    let status = response.status().as_u16();
    if status == 404 {
        return ProbeResult {
            service: Some(LocalService {
                info: info.clone(),
                endpoint,
                version: info.version.clone(),
                state: State::Ready,
                compatible: false,
            }),
            timed_out: false,
        };
    }
    let body: Option<Value> = match response.bytes().await {
        Ok(b) => serde_json::from_slice(&b).ok(),
        Err(e) => return ProbeResult { service: None, timed_out: e.is_timeout() },
    };
    let Some(body) = body else { return ProbeResult { service: None, timed_out: false } };
    let (Some(version), Some(pid)) = (body.get("version").and_then(Value::as_str), body.get("pid").and_then(Value::as_u64)) else {
        return ProbeResult { service: None, timed_out: false };
    };
    if pid != info.pid as u64 {
        return ProbeResult { service: None, timed_out: false };
    }
    if let Some(v) = &info.version
        && v != version
    {
        return ProbeResult { service: None, timed_out: false };
    }
    let state = if (200..300).contains(&status) {
        State::Ready
    } else if status == 500 {
        State::Failed
    } else {
        State::Waiting
    };
    ProbeResult {
        service: Some(LocalService { info: info.clone(), endpoint, version: Some(version.into()), state, compatible: true }),
        timed_out: false,
    }
}

fn matches_version(version: Option<&str>, required: Option<&str>) -> bool {
    match required {
        None => true,
        Some(r) => version == Some(r),
    }
}

struct Registered {
    info: Option<Registration>,
    service: Option<LocalService>,
    timed_out: bool,
}

async fn registered(file: &Path, timeout: Duration) -> Registered {
    let Some(info) = read_registration(file) else { return Registered { info: None, service: None, timed_out: false } };
    let r = probe(&info, timeout).await;
    Registered { info: Some(info), service: r.service, timed_out: r.timed_out }
}

/// `Service.discover(options)`: a ready, compatible, version-matching service.
pub async fn discover(file: &Path, version: Option<&str>) -> Option<Endpoint> {
    let found = registered(file, PROBE_TIMEOUT).await.service?;
    if found.state != State::Ready || !found.compatible || !matches_version(found.version.as_deref(), version) {
        return None;
    }
    Some(found.endpoint)
}

/// `Service.incumbent({file, url})`: an authenticated compatible owner at `url` in any state.
pub async fn incumbent(file: &Path, url: &str, timeout: Duration) -> Option<(Endpoint, State)> {
    let mut info = read_registration(file)?;
    info.url = url.to_string();
    let found = probe(&info, timeout).await.service?;
    if !found.compatible {
        return None;
    }
    Some((found.endpoint, found.state))
}

// ---------------------------------------------------------------------------------------------
// contenders (`service-contender.ts`)

const STDERR_LIMIT: usize = 8 * 1024;

struct Contender {
    child: std::process::Child,
    stderr: Arc<Mutex<Vec<u8>>>,
    exit: Option<std::process::ExitStatus>,
    error: Option<String>,
}

impl Contender {
    fn spawn(command: &[String], env: &IndexMap<String, Option<String>>) -> Result<Contender, String> {
        let (exe, args) = command.split_first().ok_or("Missing service command")?;
        let mut cmd = std::process::Command::new(exe);
        cmd.args(args)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::piped());
        for (k, v) in env {
            match v {
                Some(v) => cmd.env(k, v),
                None => cmd.env_remove(k),
            };
        }
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt;
            unsafe {
                cmd.pre_exec(|| {
                    libc::setsid();
                    Ok(())
                });
            }
        }
        let mut child = cmd.spawn().map_err(|_| "Failed to start server".to_string())?;
        let stderr = Arc::new(Mutex::new(Vec::new()));
        if let Some(mut pipe) = child.stderr.take() {
            let buf = stderr.clone();
            std::thread::spawn(move || {
                let mut chunk = [0u8; 4096];
                while let Ok(n) = pipe.read(&mut chunk) {
                    if n == 0 {
                        break;
                    }
                    let mut b = buf.lock().unwrap();
                    b.extend_from_slice(&chunk[..n]);
                    if b.len() > STDERR_LIMIT {
                        let cut = b.len() - STDERR_LIMIT;
                        b.drain(..cut);
                    }
                }
            });
        }
        Ok(Contender { child, stderr, exit: None, error: None })
    }

    fn finished(&mut self) -> bool {
        if self.error.is_some() || self.exit.is_some() {
            return true;
        }
        match self.child.try_wait() {
            Ok(Some(s)) => {
                // let the reader thread drain the tail
                std::thread::sleep(Duration::from_millis(5));
                self.exit = Some(s);
                true
            }
            Ok(None) => false,
            Err(e) => {
                self.error = Some(format!("Failed to start server: {e}"));
                true
            }
        }
    }

    fn stderr_text(&self) -> String {
        String::from_utf8_lossy(&self.stderr.lock().unwrap()).trim().to_string()
    }

    fn failure(&self) -> Option<String> {
        if let Some(e) = &self.error {
            return Some(e.clone());
        }
        let status = self.exit?;
        let with = |m: String| {
            let s = self.stderr_text();
            if s.is_empty() { m } else { format!("{m}\n{s}") }
        };
        if let Some(code) = status.code() {
            if code != 0 {
                return Some(with(format!("Server process exited with code {code}")));
            }
            return None;
        }
        #[cfg(unix)]
        {
            use std::os::unix::process::ExitStatusExt;
            if let Some(sig) = status.signal() {
                return Some(with(format!("Server process terminated by {}", signal_name(sig))));
            }
        }
        None
    }
}

#[cfg(unix)]
pub fn signal_name(sig: i32) -> String {
    match sig {
        1 => "SIGHUP",
        2 => "SIGINT",
        3 => "SIGQUIT",
        6 => "SIGABRT",
        9 => "SIGKILL",
        11 => "SIGSEGV",
        13 => "SIGPIPE",
        14 => "SIGALRM",
        15 => "SIGTERM",
        _ => return format!("SIG{sig}"),
    }
    .to_string()
}

struct Pool {
    contenders: Vec<Contender>,
    failure: Option<String>,
    spawn_delay: Duration,
    last_spawn: Option<Instant>,
}

impl Pool {
    fn new() -> Pool {
        Pool { contenders: vec![], failure: None, spawn_delay: SPAWN_DELAY, last_spawn: None }
    }
    fn service_answered(&mut self) {
        self.spawn_delay = SPAWN_DELAY;
    }
    fn evict(&mut self, pid: u32) {
        self.contenders.retain_mut(|c| !(c.child.id() == pid || c.finished()));
        self.failure = None;
        self.last_spawn = None;
    }
    fn recruit_now(&mut self) {
        self.last_spawn = Instant::now().checked_sub(self.spawn_delay);
    }
    fn reap(&mut self) -> Option<String> {
        let mut clean_exit = false;
        let mut keep = vec![];
        for mut c in std::mem::take(&mut self.contenders) {
            if c.finished() {
                if self.failure.is_none() {
                    self.failure = c.failure();
                }
                if c.exit.and_then(|s| s.code()) == Some(0) {
                    clean_exit = true;
                }
            } else {
                keep.push(c);
            }
        }
        self.contenders = keep;
        if clean_exit {
            self.spawn_delay = (self.spawn_delay * 2).min(MAX_SPAWN_DELAY);
        }
        if self.contenders.is_empty() { self.failure.clone() } else { None }
    }
    fn should_recruit(&mut self, registered: bool) -> bool {
        if self.last_spawn.is_none() && registered {
            self.last_spawn = Some(Instant::now());
        }
        let elapsed = self.last_spawn.map_or(Duration::MAX, |t| t.elapsed());
        self.failure.is_none() && self.contenders.len() < 2 && elapsed >= self.spawn_delay
    }
    fn add(&mut self, c: Contender) {
        self.contenders.push(c);
        self.last_spawn = Some(Instant::now());
    }
}

/// Why `ensure` starts a service (`onStart(reason, previousVersion)`).
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum StartReason {
    Missing,
    VersionMismatch,
}

impl StartReason {
    pub fn as_str(self) -> &'static str {
        match self {
            StartReason::Missing => "missing",
            StartReason::VersionMismatch => "version-mismatch",
        }
    }
}

pub type OnStart<'a> = Option<&'a (dyn Fn(StartReason, Option<&str>) + Send + Sync)>;

/// `Service.ensure(options)`
pub async fn ensure(options: &EnsureOptions, on_start: OnStart<'_>) -> anyhow::Result<Endpoint> {
    let started = Instant::now();
    let mut pool = Pool::new();
    let mut timeouts: Option<(Registration, usize)> = None;
    let mut announced = false;
    let mut announce = |reason: StartReason, previous: Option<&str>| {
        if announced {
            return;
        }
        announced = true;
        if let Some(f) = on_start {
            f(reason, previous);
        }
    };
    loop {
        let registration = registered(&options.file, PROBE_TIMEOUT).await;
        if registration.timed_out
            && let Some(info) = &registration.info
        {
            let count = match &timeouts {
                Some((prev, n)) if prev.same_instance(info) => n + 1,
                _ => 1,
            };
            timeouts = Some((info.clone(), count));
            if count >= 3 {
                announce(StartReason::Missing, None);
                tracing::warn!("Background service is unresponsive; recovery cannot preserve persistent terminals");
                pty_clear(&options.file);
                terminate(info, &options.file).await;
                pool.evict(info.pid);
                pool.recruit_now();
                timeouts = None;
            }
        } else {
            timeouts = None;
        }
        if let Some(service) = registration.service {
            pool.service_answered();
            if !matches_version(service.version.as_deref(), options.version.as_deref()) {
                announce(StartReason::VersionMismatch, service.version.as_deref());
                if service.state != State::Ready {
                    tracing::warn!("Background service is not ready; replacement cannot preserve persistent terminals");
                }
                let _ = stop(&options.file, if service.state == State::Ready { Pty::Handoff } else { Pty::Clear }).await;
                pool.evict(service.info.pid);
            } else if !service.compatible {
                anyhow::bail!(
                    "Background service uses an incompatible health protocol. Update this client or explicitly restart the service."
                );
            } else if service.state == State::Ready {
                pty_complete(&options.file, &service.info);
                return Ok(service.endpoint);
            } else if service.state == State::Failed {
                anyhow::bail!("Background service failed to start");
            }
        } else {
            if let Some(failed) = pool.reap() {
                anyhow::bail!(failed);
            }
            if pool.should_recruit(registration.info.is_some()) {
                announce(StartReason::Missing, None);
                let mut env: IndexMap<String, Option<String>> = options.env.iter().map(|(k, v)| (k.clone(), Some(v.clone()))).collect();
                env.insert("OPENCODE_PTY_HANDOFF".into(), pty_environment(&options.file));
                match Contender::spawn(&options.command, &env) {
                    Ok(c) => pool.add(c),
                    Err(e) => anyhow::bail!(e),
                }
            }
        }
        if started.elapsed() >= ENSURE_LIMIT {
            break;
        }
        tokio::time::sleep(POLL).await;
    }
    anyhow::bail!(pool.failure.unwrap_or_else(|| "Timed out waiting for the background service to start".into()))
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Pty {
    Handoff,
    Clear,
}

/// `Service.stop({file, pty})`
pub async fn stop(file: &Path, pty: Pty) -> anyhow::Result<()> {
    let info = read_registration(file);
    match (&info, pty) {
        (Some(info), Pty::Handoff) => pty_prepare(file, info).await,
        _ => pty_clear(file),
    }
    if let Some(info) = info {
        terminate(&info, file).await;
    }
    Ok(())
}

fn alive(pid: u32) -> bool {
    #[cfg(unix)]
    unsafe {
        libc::kill(pid as i32, 0) == 0
    }
    #[cfg(not(unix))]
    {
        let _ = pid;
        false
    }
}

fn signal(pid: u32, sig: i32) {
    #[cfg(unix)]
    unsafe {
        libc::kill(pid as i32, sig);
    }
    #[cfg(not(unix))]
    let _ = (pid, sig);
}

async fn wait_stopped(pid: u32) -> bool {
    for _ in 0..=STOP_ATTEMPTS {
        if !alive(pid) {
            return true;
        }
        tokio::time::sleep(STOP_POLL).await;
    }
    !alive(pid)
}

/// `terminate(info)`: SIGTERM, wait ≤5 s, SIGKILL, then drop the registration if unchanged.
pub async fn terminate(info: &Registration, file: &Path) {
    match read_registration(file) {
        Some(current) if current.same_instance(info) => {}
        _ => return,
    }
    signal(info.pid, 15);
    if !wait_stopped(info.pid).await {
        signal(info.pid, 9);
        if !wait_stopped(info.pid).await {
            tracing::warn!("Server process {} is still running", info.pid);
        }
    }
    if let Some(latest) = read_registration(file)
        && latest.same_instance(info)
    {
        let _ = std::fs::remove_file(file);
    }
}

// ---------------------------------------------------------------------------------------------
// PTY handoff sidecar (`pty-handoff.ts`)

fn sidecar(file: &Path) -> PathBuf {
    PathBuf::from(format!("{}.pty-handoff", file.display()))
}

#[derive(Debug, Clone)]
struct Sidecar {
    source_id: Option<String>,
    source_pid: u64,
    source_url: String,
    handoff: Value,
    expires_at: f64,
}

fn is_handoff(v: &Value) -> bool {
    v.get("directory").is_some_and(Value::is_string)
        && v.get("instanceID").is_some_and(Value::is_string)
        && v.get("ticket").is_some_and(Value::is_string)
        && v.get("expiresAt").and_then(Value::as_f64).is_some_and(f64::is_finite)
}

fn read_sidecar(file: &Path) -> Option<Sidecar> {
    let v: Value = serde_json::from_str(&std::fs::read_to_string(sidecar(file)).ok()?).ok()?;
    let source = v.get("source")?.as_object()?;
    let handoff = v.get("handoff")?.clone();
    if !handoff.is_null() && !is_handoff(&handoff) {
        return None;
    }
    if source.get("id").is_some_and(|i| !i.is_string()) {
        return None;
    }
    Some(Sidecar {
        source_id: source.get("id").and_then(Value::as_str).map(String::from),
        source_pid: source.get("pid")?.as_f64()? as u64,
        source_url: source.get("url")?.as_str()?.to_string(),
        handoff,
        expires_at: v.get("expiresAt")?.as_f64().filter(|f| f.is_finite())?,
    })
}

fn sidecar_same(s: &Sidecar, info: &Registration) -> bool {
    s.source_id == info.id && s.source_pid == info.pid as u64 && s.source_url == info.url
}

fn now_ms() -> f64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_millis() as f64).unwrap_or(0.0)
}

pub fn pty_clear(file: &Path) {
    let _ = std::fs::remove_file(sidecar(file));
}

fn pty_complete(file: &Path, info: &Registration) {
    if let Some(s) = read_sidecar(file)
        && !sidecar_same(&s, info)
    {
        pty_clear(file);
    }
}

/// `PtyHandoff.environment(file)` → the `OPENCODE_PTY_HANDOFF` value (None removes it).
fn pty_environment(file: &Path) -> Option<String> {
    let record = read_sidecar(file)?;
    let current = read_registration(file);
    let valid = record.expires_at > now_ms() && current.as_ref().is_none_or(|c| sidecar_same(&record, c));
    if !valid || record.handoff.is_null() {
        return None;
    }
    Some(record.handoff.to_string())
}

async fn pty_prepare(file: &Path, info: &Registration) {
    if let Some(existing) = read_sidecar(file)
        && existing.expires_at > now_ms()
        && sidecar_same(&existing, info)
    {
        return;
    }
    let client = Client::new(&Endpoint { url: info.url.clone(), password: info.password.clone() });
    let handoff = match client.call("server.experimental.persistentPty.handoff", Req::new().timeout(PROBE_TIMEOUT)).await {
        Ok(body) => match body.get("handoff") {
            Some(Value::Null) => Some(Value::Null),
            Some(h) if is_handoff(h) && h.get("expiresAt").and_then(Value::as_f64).unwrap_or(0.0) > now_ms() => Some(h.clone()),
            _ => None,
        },
        Err(_) => None,
    };
    let handoff = match handoff {
        Some(h) => h,
        None => {
            if let Some(c) = read_sidecar(file)
                && c.expires_at > now_ms()
                && sidecar_same(&c, info)
            {
                return;
            }
            tracing::warn!("Background service cannot hand off persistent terminals; shutting them down before replacement");
            let _ = client.call("server.experimental.persistentPty.shutdown", Req::new().timeout(PROBE_TIMEOUT)).await;
            Value::Null
        }
    };
    let expires = handoff.get("expiresAt").and_then(Value::as_f64).unwrap_or(now_ms() + 30_000.0);
    let record = serde_json::json!({
        "source": {"id": info.id, "pid": info.pid, "url": info.url},
        "handoff": handoff,
        "expiresAt": expires,
    });
    let tmp = PathBuf::from(format!("{}.pty-handoff.{}.tmp", file.display(), uuid::Uuid::new_v4()));
    write_private_new(&tmp, &record.to_string());
    if std::fs::rename(&tmp, sidecar(file)).is_err() {
        tracing::warn!("Failed to publish persistent terminal handoff");
    }
    let _ = std::fs::remove_file(&tmp);
}

/// `shutdownPersistentPty(options)`: best-effort, against the discovered service.
pub async fn shutdown_persistent_pty(file: &Path) {
    if let Some(endpoint) = discover(file, None).await {
        let _ = Client::new(&endpoint).call("server.experimental.persistentPty.shutdown", Req::new()).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn version_channel() {
        assert!(version_belongs_to_channel(Some("2.0.0"), "beta", "2.0.0"));
        assert!(version_belongs_to_channel(Some("0.0.0-beta-12"), "beta", "x"));
        assert!(version_belongs_to_channel(Some("0.0.0-beta-12.3"), "beta", "x"));
        assert!(!version_belongs_to_channel(Some("0.0.0-beta-x"), "beta", "x"));
        assert!(!version_belongs_to_channel(None, "beta", "x"));
    }

    #[test]
    fn config_key_order_and_legacy() {
        let info = decode_info(r#"{"env":{"A":"1"},"port":5,"disabled":true}"#).unwrap();
        assert_eq!(serde_json::to_string(&info).unwrap(), r#"{"disabled":true,"port":5,"env":{"A":"1"}}"#);
        assert!(decode_info(r#"{"port":0}"#).is_none());
        let (info, remote) = decode_legacy(r#"{"remote":true,"hostname":"h"}"#).unwrap();
        assert!(remote);
        assert_eq!(info.hostname.as_deref(), Some("h"));
    }

    #[test]
    fn remote_accepted_is_kept_in_schema_order() {
        let info = decode_info(r#"{"hostname":"h","remoteAccepted":true,"disabled":false}"#).unwrap();
        assert_eq!(serde_json::to_string(&info).unwrap(), r#"{"disabled":false,"remoteAccepted":true,"hostname":"h"}"#);
        assert!(decode_info(r#"{"remoteAccepted":false}"#).is_none());
    }
}

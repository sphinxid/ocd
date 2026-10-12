//! Non-interactive shell command processes (`core/src/shell.ts`).
//!
//! Each command spawns once, streams combined stdout+stderr into
//! `<data>/shell/<projectID>/<id>.out`, and keeps the last 25 exited commands observable.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use oc_schema::Metadata;
use oc_schema::message::{ShellInfo, ShellOutput, ShellStatus, ShellTime};
use parking_lot::Mutex;
use tokio::io::AsyncReadExt;
use tokio::sync::watch;

use super::result::{Capture, ShellResult, capture_tail};
use crate::host::ShellInvocation;

pub const EXITED_LIMIT: usize = 25;
pub const RETENTION: Duration = Duration::from_secs(7 * 24 * 3600);
pub const DIRECTORY: &str = "shell";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NotFound {
    pub id: String,
}

/// Bus events (`Shell.Event.*`).
#[derive(Debug, Clone, PartialEq)]
pub enum ShellEvent {
    Created { info: ShellInfo },
    Exited { id: String, exit: Option<i64>, status: ShellStatus },
    Deleted { id: String },
}

pub type EventSink = Arc<dyn Fn(ShellEvent) + Send + Sync>;

type Done = Option<Result<ShellInfo, NotFound>>;

struct Active {
    info: Mutex<ShellInfo>,
    file: PathBuf,
    size: AtomicU64,
    newlines: AtomicU64,
    done: watch::Sender<Done>,
    deadline: watch::Sender<Option<tokio::time::Instant>>,
    pid: Option<u32>,
}

pub struct ShellService {
    data: PathBuf,
    commands: Mutex<HashMap<String, Arc<Active>>>,
    exit_order: Mutex<Vec<String>>,
    events: Option<EventSink>,
}

/// Signal number → name.
pub fn signal_name(signal: i32) -> String {
    let name = match signal {
        1 => "SIGHUP",
        2 => "SIGINT",
        3 => "SIGQUIT",
        4 => "SIGILL",
        5 => "SIGTRAP",
        6 => "SIGABRT",
        7 => "SIGBUS",
        8 => "SIGFPE",
        9 => "SIGKILL",
        10 => "SIGUSR1",
        11 => "SIGSEGV",
        12 => "SIGUSR2",
        13 => "SIGPIPE",
        14 => "SIGALRM",
        15 => "SIGTERM",
        _ => return format!("SIG{signal}"),
    };
    name.to_string()
}

#[cfg(unix)]
fn kill_group(pid: u32, signal: i32) {
    // SAFETY: plain libc call with a process-group id we spawned.
    unsafe {
        libc::kill(-(pid as i32), signal);
    }
}

#[cfg(not(unix))]
fn kill_group(_pid: u32, _signal: i32) {}

fn kill_tree(pid: Option<u32>) {
    let Some(pid) = pid else { return };
    #[cfg(unix)]
    {
        kill_group(pid, libc::SIGTERM);
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_secs(3)).await;
            kill_group(pid, libc::SIGKILL);
        });
    }
    #[cfg(not(unix))]
    {
        let _ = std::process::Command::new("taskkill").args(["/pid", &pid.to_string(), "/T", "/F"]).output();
    }
}

impl ShellService {
    /// `data` = global data dir.
    pub fn new(data: PathBuf, events: Option<EventSink>) -> Arc<Self> {
        Arc::new(Self { data, commands: Mutex::new(HashMap::new()), exit_order: Mutex::new(Vec::new()), events })
    }

    pub fn output_dir(&self, project_id: &str) -> PathBuf {
        self.data.join(DIRECTORY).join(project_id)
    }

    fn emit(&self, event: ShellEvent) {
        if let Some(sink) = &self.events {
            sink(event);
        }
    }

    fn require(&self, id: &str) -> Result<Arc<Active>, NotFound> {
        self.commands.lock().get(id).cloned().ok_or_else(|| NotFound { id: id.to_string() })
    }

    fn remove_command(&self, id: &str) {
        self.exit_order.lock().retain(|x| x != id);
        let Some(command) = self.commands.lock().remove(id) else { return };
        let _ = command.deadline.send(None);
        let running = command.info.lock().status == ShellStatus::Running;
        if running {
            kill_tree(command.pid);
        }
        command.done.send_if_modified(|done| {
            if done.is_none() {
                *done = Some(Err(NotFound { id: id.to_string() }));
                true
            } else {
                false
            }
        });
        let _ = std::fs::remove_file(&command.file);
        self.emit(ShellEvent::Deleted { id: id.to_string() });
    }

    pub fn remove(&self, id: &str) -> Result<(), NotFound> {
        self.require(id)?;
        self.remove_command(id);
        Ok(())
    }

    /// Running commands only.
    pub fn list(&self) -> Vec<ShellInfo> {
        self.commands.lock().values().map(|c| c.info.lock().clone()).filter(|i| i.status == ShellStatus::Running).collect()
    }

    pub fn get(&self, id: &str) -> Result<ShellInfo, NotFound> {
        Ok(self.require(id)?.info.lock().clone())
    }

    pub async fn wait(&self, id: &str) -> Result<ShellInfo, NotFound> {
        let command = self.require(id)?;
        let mut rx = command.done.subscribe();
        loop {
            if let Some(done) = rx.borrow().clone() {
                return done;
            }
            if rx.changed().await.is_err() {
                return Err(NotFound { id: id.to_string() });
            }
        }
    }

    /// Replace the running command's timeout from now; zero clears it.
    pub fn timeout(&self, id: &str, duration_ms: u64) -> Result<ShellInfo, NotFound> {
        let command = self.require(id)?;
        let info = command.info.lock().clone();
        if info.status != ShellStatus::Running {
            return Ok(info);
        }
        let _ = command
            .deadline
            .send((duration_ms > 0).then(|| tokio::time::Instant::now() + Duration::from_millis(duration_ms)));
        Ok(info)
    }

    /// Output page by byte cursor.
    pub fn output(&self, id: &str, cursor: Option<u64>, limit: Option<u64>) -> Result<ShellOutput, NotFound> {
        let command = self.require(id)?;
        let size = command.size.load(Ordering::SeqCst);
        let cursor = cursor.unwrap_or(0);
        let limit = limit.unwrap_or(65536);
        if cursor >= size {
            return Ok(ShellOutput { output: String::new(), cursor: size, size, truncated: false });
        }
        let length = limit.min(size - cursor);
        let bytes = read_range(&command.file, cursor, length).unwrap_or_default();
        Ok(ShellOutput {
            output: String::from_utf8_lossy(&bytes).into_owned(),
            cursor: cursor + bytes.len() as u64,
            size,
            truncated: false,
        })
    }

    /// Terminal state and bounded tail.
    pub async fn result(&self, started: &ShellInfo, max_lines: usize, max_bytes: usize) -> ShellResult {
        let info = match self.wait(&started.id).await {
            Ok(info) => info,
            Err(_) => {
                let mut info = started.clone();
                info.status = ShellStatus::Killed;
                info.time.completed = Some(crate::util::now_ms());
                info
            }
        };
        let capture = self.require(&info.id).ok().map(|command| {
            let size = command.size.load(Ordering::SeqCst);
            let start = size.saturating_sub(max_bytes as u64);
            let bytes = read_range(&command.file, start, (size - start).min(max_bytes as u64)).unwrap_or_default();
            let page = String::from_utf8_lossy(&bytes).into_owned();
            capture_tail(&page, size, command.newlines.load(Ordering::SeqCst), max_lines, max_bytes, &info.file)
        });
        ShellResult { info, capture: capture.map(|c: Capture| c) }
    }

    /// Spawn a prepared invocation. Returns once the process is registered.
    pub async fn spawn(self: &Arc<Self>, invocation: &ShellInvocation, metadata: Metadata, project_id: &str) -> Result<ShellInfo, String> {
        let id = oc_schema::id::shell();
        let dir = self.output_dir(project_id);
        std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
        let file = dir.join(format!("{id}.out"));
        let args = super::select::args(&invocation.shell, &invocation.command);
        let mut command = tokio::process::Command::new(&invocation.shell);
        command
            .args(&args)
            .current_dir(&invocation.cwd)
            .env_clear()
            .envs(&invocation.env)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        #[cfg(unix)]
        command.process_group(0);
        let mut child = command.spawn().map_err(|e| e.to_string())?;
        let pid = child.id();
        let info = ShellInfo {
            id: id.clone(),
            status: ShellStatus::Running,
            command: invocation.command.clone(),
            cwd: invocation.cwd.clone(),
            shell: invocation.shell.clone(),
            file: file.to_string_lossy().into_owned(),
            pid,
            exit: None,
            signal: None,
            metadata,
            time: ShellTime { started: crate::util::now_ms(), completed: None },
        };
        let (done, _) = watch::channel(None);
        let (deadline, mut deadline_rx) = watch::channel(None);
        let active = Arc::new(Active {
            info: Mutex::new(info.clone()),
            file: file.clone(),
            size: AtomicU64::new(0),
            newlines: AtomicU64::new(0),
            done,
            deadline,
            pid,
        });
        self.commands.lock().insert(id.clone(), active.clone());
        if invocation.timeout > 0 {
            let _ = active.deadline.send(Some(tokio::time::Instant::now() + Duration::from_millis(invocation.timeout)));
        }

        let out = std::sync::Arc::new(Mutex::new(std::fs::File::create(&file).map_err(|e| e.to_string())?));
        let pump = |mut reader: Box<dyn tokio::io::AsyncRead + Unpin + Send>, active: Arc<Active>, out: Arc<Mutex<std::fs::File>>| async move {
            let mut buffer = vec![0u8; 16 * 1024];
            loop {
                match reader.read(&mut buffer).await {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        use std::io::Write;
                        let chunk = &buffer[..n];
                        let _ = out.lock().write_all(chunk);
                        active.size.fetch_add(n as u64, Ordering::SeqCst);
                        active.newlines.fetch_add(chunk.iter().filter(|&&b| b == b'\n').count() as u64, Ordering::SeqCst);
                    }
                }
            }
        };
        let stdout = child.stdout.take().map(|s| Box::new(s) as Box<dyn tokio::io::AsyncRead + Unpin + Send>);
        let stderr = child.stderr.take().map(|s| Box::new(s) as Box<dyn tokio::io::AsyncRead + Unpin + Send>);
        let output_task = {
            let a = active.clone();
            let o = out.clone();
            tokio::spawn(async move {
                let first = stdout.map(|s| tokio::spawn(pump(s, a.clone(), o.clone())));
                let second = stderr.map(|s| tokio::spawn(pump(s, a.clone(), o.clone())));
                if let Some(t) = first {
                    let _ = t.await;
                }
                if let Some(t) = second {
                    let _ = t.await;
                }
                use std::io::Write;
                let _ = o.lock().flush();
            })
        };

        let service = Arc::downgrade(self);
        let manager_active = active.clone();
        tokio::spawn(async move {
            let active = manager_active;
            let mut timed_out = false;
            let status = loop {
                let deadline = *deadline_rx.borrow();
                tokio::select! {
                    status = child.wait() => break status.ok(),
                    _ = async {
                        match deadline {
                            Some(at) => tokio::time::sleep_until(at).await,
                            None => std::future::pending::<()>().await,
                        }
                    } => {
                        // Timeout: mark, kill the process group, keep waiting for exit.
                        timed_out = true;
                        {
                            let mut info = active.info.lock();
                            if info.status == ShellStatus::Running {
                                info.status = ShellStatus::Timeout;
                                info.time.completed = Some(crate::util::now_ms());
                            }
                        }
                        kill_tree(active.pid);
                        let _ = active.deadline.send(None);
                        let status = child.wait().await.ok();
                        break status;
                    }
                    changed = deadline_rx.changed() => {
                        if changed.is_err() {
                            break child.wait().await.ok();
                        }
                    }
                }
            };
            let _ = output_task.await;
            let snapshot = {
                let mut info = active.info.lock();
                if !timed_out && info.status == ShellStatus::Running {
                    info.status = ShellStatus::Exited;
                    if let Some(status) = status {
                        match status.code() {
                            Some(code) => info.exit = Some(code as i64),
                            None => {
                                #[cfg(unix)]
                                {
                                    use std::os::unix::process::ExitStatusExt;
                                    info.signal = status.signal().map(signal_name);
                                }
                            }
                        }
                    }
                    info.time.completed = Some(crate::util::now_ms());
                }
                info.clone()
            };
            if snapshot.status == ShellStatus::Running {
                return;
            }
            active.done.send_if_modified(|done| {
                if done.is_none() {
                    *done = Some(Ok(snapshot.clone()));
                    true
                } else {
                    false
                }
            });
            let Some(service) = service.upgrade() else { return };
            if !service.commands.lock().contains_key(&snapshot.id) {
                return;
            }
            service.emit(ShellEvent::Exited { id: snapshot.id.clone(), exit: snapshot.exit, status: snapshot.status });
            let evict: Vec<String> = {
                let mut order = service.exit_order.lock();
                order.push(snapshot.id.clone());
                let excess = order.len().saturating_sub(EXITED_LIMIT);
                order.iter().take(excess).cloned().collect()
            };
            for old in evict {
                service.remove_command(&old);
            }
        });
        self.emit(ShellEvent::Created { info: info.clone() });
        Ok(info)
    }

    /// Hourly sweep of `<data>/shell/*/sh_*.out` files older than 7 days.
    pub fn cleanup(data: &Path) {
        let directory = data.join(DIRECTORY);
        let re = regex::Regex::new(r"^sh_[0-9a-f]{12}.*\.out$").unwrap();
        let mut files = Vec::new();
        if let Ok(projects) = std::fs::read_dir(&directory) {
            for project in projects.flatten().filter(|p| p.file_type().map(|t| t.is_dir()).unwrap_or(false)) {
                if let Ok(entries) = std::fs::read_dir(project.path()) {
                    for entry in entries.flatten() {
                        if entry.file_type().map(|t| t.is_file()).unwrap_or(false) && re.is_match(&entry.file_name().to_string_lossy()) {
                            files.push(entry.path());
                        }
                    }
                }
            }
        }
        crate::tool_output::retention_cleanup(&files, RETENTION);
    }
}

fn read_range(path: &Path, start: u64, length: u64) -> std::io::Result<Vec<u8>> {
    use std::io::{Read, Seek, SeekFrom};
    let mut file = std::fs::File::open(path)?;
    file.seek(SeekFrom::Start(start))?;
    let mut buffer = Vec::with_capacity(length as usize);
    file.take(length).read_to_end(&mut buffer)?;
    Ok(buffer)
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    fn invocation(command: &str, cwd: &Path, timeout: u64) -> ShellInvocation {
        ShellInvocation {
            command: command.into(),
            cwd: cwd.to_string_lossy().into_owned(),
            timeout,
            shell: "/bin/sh".into(),
            env: std::env::vars().collect(),
        }
    }

    #[tokio::test]
    async fn runs_and_captures() {
        let dir = tempfile::tempdir().unwrap();
        let service = ShellService::new(dir.path().to_path_buf(), None);
        let info = service.spawn(&invocation("echo hi; echo err 1>&2; exit 3", dir.path(), 0), Metadata::new(), "proj").await.unwrap();
        assert!(info.id.starts_with("sh_"));
        let result = service.result(&info, 2000, 51200).await;
        assert_eq!(result.info.status, ShellStatus::Exited);
        assert_eq!(result.info.exit, Some(3));
        let capture = result.capture.unwrap();
        assert!(capture.output.contains("hi\n") && capture.output.contains("err\n"));
        assert!(!capture.truncated);
    }

    #[tokio::test]
    async fn times_out() {
        let dir = tempfile::tempdir().unwrap();
        let service = ShellService::new(dir.path().to_path_buf(), None);
        let info = service.spawn(&invocation("echo start; sleep 5", dir.path(), 200), Metadata::new(), "p").await.unwrap();
        let result = service.result(&info, 2000, 51200).await;
        assert_eq!(result.info.status, ShellStatus::Timeout);
        assert_eq!(result.capture.unwrap().output, "start\n");
    }

    #[tokio::test]
    async fn removal_reports_killed() {
        let dir = tempfile::tempdir().unwrap();
        let service = ShellService::new(dir.path().to_path_buf(), None);
        let info = service.spawn(&invocation("sleep 5", dir.path(), 0), Metadata::new(), "p").await.unwrap();
        let s2 = service.clone();
        let id = info.id.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(100)).await;
            s2.remove(&id).unwrap();
        });
        let result = service.result(&info, 2000, 51200).await;
        assert_eq!(result.info.status, ShellStatus::Killed);
        assert!(result.capture.is_none());
    }
}

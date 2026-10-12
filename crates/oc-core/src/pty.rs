//! Ephemeral PTY service (`core/src/pty.ts`). Buffer lengths and cursors count UTF-16 code units,
//! like the JS original, so cursor values stay wire-compatible.

use std::collections::VecDeque;
use std::io::{Read, Write};
use std::sync::Arc;

use indexmap::IndexMap;
use parking_lot::Mutex;
use portable_pty::{native_pty_system, CommandBuilder, PtySize};
use serde::{Deserialize, Serialize};
use serde_json::json;
use tokio::sync::mpsc;

use crate::bus::Bus;

pub const BUFFER_LIMIT: usize = 1024 * 1024 * 2;
pub const EXITED_LIMIT: usize = 25;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PtyInfo {
    pub id: String,
    pub title: String,
    pub command: String,
    pub args: Vec<String>,
    pub cwd: String,
    pub status: String,
    pub pid: u32,
    #[serde(rename = "exitCode", skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<i64>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct CreateInput {
    pub command: Option<String>,
    pub args: Option<Vec<String>>,
    pub cwd: Option<String>,
    pub title: Option<String>,
    pub env: Option<std::collections::HashMap<String, String>>,
}

#[derive(Debug, thiserror::Error)]
pub enum PtyError {
    #[error("PTY session not found: {0}")]
    NotFound(String),
    #[error("PTY session exited: {0}")]
    Exited(String),
    #[error("{0}")]
    Spawn(String),
}

pub enum Chunk {
    Data(String),
    End(Option<i64>),
}

struct Subscriber {
    tx: mpsc::UnboundedSender<Chunk>,
    active: bool,
    pending: Vec<String>,
    end: Option<Option<i64>>,
}

struct Session {
    info: PtyInfo,
    buffer: VecDeque<u16>,
    buffer_cursor: u64,
    cursor: u64,
    subscribers: IndexMap<u64, Subscriber>,
    writer: Option<Box<dyn Write + Send>>,
    master: Option<Box<dyn portable_pty::MasterPty + Send>>,
    killer: Option<Box<dyn portable_pty::ChildKiller + Send + Sync>>,
}

#[derive(Clone)]
pub struct Ptys {
    bus: Bus,
    sessions: Arc<Mutex<IndexMap<String, Session>>>,
    exited: Arc<Mutex<Vec<String>>>,
    next_sub: Arc<Mutex<u64>>,
}

pub struct Attachment {
    pub replay: String,
    pub cursor: u64,
    pub rx: mpsc::UnboundedReceiver<Chunk>,
    ptys: Ptys,
    id: String,
    sub: u64,
}

impl Attachment {
    pub fn write(&self, data: &str) {
        self.ptys.write(&self.id, data);
    }
    /// Flush queued chunks and go live.
    pub fn activate(&self) {
        let mut s = self.ptys.sessions.lock();
        let Some(session) = s.get_mut(&self.id) else { return };
        if let Some(sub) = session.subscribers.get_mut(&self.sub) {
            for c in sub.pending.drain(..) {
                let _ = sub.tx.send(Chunk::Data(c));
            }
            if let Some(end) = sub.end.take() {
                let _ = sub.tx.send(Chunk::End(end));
            }
            sub.active = true;
        }
    }
    pub fn detach(&self) {
        if let Some(s) = self.ptys.sessions.lock().get_mut(&self.id) {
            s.subscribers.shift_remove(&self.sub);
        }
    }
}

impl Drop for Attachment {
    fn drop(&mut self) {
        self.detach();
    }
}

impl Ptys {
    pub fn new(bus: Bus) -> Self {
        Ptys { bus, sessions: Default::default(), exited: Default::default(), next_sub: Default::default() }
    }

    fn publish(&self, kind: &str, data: serde_json::Value) {
        self.bus.publish(kind, data, None);
    }

    pub fn list(&self) -> Vec<PtyInfo> {
        self.sessions.lock().values().map(|s| s.info.clone()).collect()
    }

    pub fn get(&self, id: &str) -> Result<PtyInfo, PtyError> {
        self.sessions.lock().get(id).map(|s| s.info.clone()).ok_or_else(|| PtyError::NotFound(id.into()))
    }

    pub fn create(&self, input: CreateInput, default_cwd: &str, default_shell: &str) -> Result<PtyInfo, PtyError> {
        let id = oc_schema::id::pty();
        let command = input.command.filter(|c| !c.is_empty()).unwrap_or_else(|| default_shell.to_string());
        let mut args = input.args.unwrap_or_default();
        if oc_tools::shell::select::login(&command) {
            args.push("-l".into());
        }
        let cwd = input.cwd.filter(|c| !c.is_empty()).unwrap_or_else(|| default_cwd.to_string());
        let pair = native_pty_system()
            .openpty(PtySize { rows: 24, cols: 80, pixel_width: 0, pixel_height: 0 })
            .map_err(|e| PtyError::Spawn(e.to_string()))?;
        let mut cmd = CommandBuilder::new(&command);
        cmd.args(&args);
        cmd.cwd(&cwd);
        for (k, v) in std::env::vars() {
            cmd.env(k, v);
        }
        for (k, v) in input.env.unwrap_or_default() {
            cmd.env(k, v);
        }
        cmd.env("TERM", "xterm-256color");
        cmd.env("OPENCODE_TERMINAL", "1");
        let mut child = pair.slave.spawn_command(cmd).map_err(|e| PtyError::Spawn(e.to_string()))?;
        let pid = child.process_id().unwrap_or(0);
        let killer = child.clone_killer();
        let mut reader = pair.master.try_clone_reader().map_err(|e| PtyError::Spawn(e.to_string()))?;
        let writer = pair.master.take_writer().map_err(|e| PtyError::Spawn(e.to_string()))?;
        let title = input.title.filter(|t| !t.is_empty()).unwrap_or_else(|| format!("Terminal {}", &id[id.len() - 4..]));
        let info = PtyInfo { id: id.clone(), title, command, args, cwd, status: "running".into(), pid, exit_code: None };
        self.sessions.lock().insert(
            id.clone(),
            Session {
                info: info.clone(),
                buffer: VecDeque::new(),
                buffer_cursor: 0,
                cursor: 0,
                subscribers: IndexMap::new(),
                writer: Some(writer),
                master: Some(pair.master),
                killer: Some(killer),
            },
        );
        // reader thread
        let this = self.clone();
        let rid = id.clone();
        std::thread::spawn(move || {
            let mut buf = [0u8; 8192];
            let mut carry: Vec<u8> = Vec::new();
            loop {
                let n = match reader.read(&mut buf) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => n,
                };
                carry.extend_from_slice(&buf[..n]);
                let valid = match std::str::from_utf8(&carry) {
                    Ok(_) => carry.len(),
                    Err(e) => e.valid_up_to(),
                };
                let chunk = String::from_utf8_lossy(&carry[..valid]).into_owned();
                carry.drain(..valid);
                if !chunk.is_empty() {
                    this.on_data(&rid, chunk);
                }
            }
        });
        // exit waiter
        let this = self.clone();
        std::thread::spawn(move || {
            let code = child.wait().ok().map(|s| s.exit_code() as i64);
            this.on_exit(&id, code);
        });
        self.publish("pty.created", json!({"info": info}));
        Ok(info)
    }

    fn on_data(&self, id: &str, chunk: String) {
        let mut s = self.sessions.lock();
        let Some(session) = s.get_mut(id) else { return };
        let units: Vec<u16> = chunk.encode_utf16().collect();
        session.cursor += units.len() as u64;
        session.subscribers.retain(|_, sub| {
            if sub.active {
                sub.tx.send(Chunk::Data(chunk.clone())).is_ok()
            } else {
                sub.pending.push(chunk.clone());
                true
            }
        });
        session.buffer.extend(units);
        if session.buffer.len() > BUFFER_LIMIT {
            let excess = session.buffer.len() - BUFFER_LIMIT;
            session.buffer.drain(..excess);
            session.buffer_cursor += excess as u64;
        }
    }

    fn on_exit(&self, id: &str, code: Option<i64>) {
        let info = {
            let mut s = self.sessions.lock();
            let Some(session) = s.get_mut(id) else { return };
            session.info.status = "exited".into();
            session.info.exit_code = code;
            for (_, sub) in session.subscribers.drain(..) {
                if sub.active {
                    let _ = sub.tx.send(Chunk::End(code));
                }
            }
            session.info.clone()
        };
        let _ = info;
        self.publish("pty.exited", json!({"id": id, "exitCode": code}));
        let mut exited = self.exited.lock();
        exited.push(id.to_string());
        while exited.len() > EXITED_LIMIT {
            let old = exited.remove(0);
            let _ = self.remove(&old);
        }
    }

    pub fn update(&self, id: &str, title: Option<String>, size: Option<(u16, u16)>) -> Result<PtyInfo, PtyError> {
        let info = {
            let mut s = self.sessions.lock();
            let session = s.get_mut(id).ok_or_else(|| PtyError::NotFound(id.into()))?;
            if let Some(t) = title.filter(|t| !t.is_empty()) {
                session.info.title = t;
            }
            if let (Some((rows, cols)), "running") = (size, session.info.status.as_str()) {
                if let Some(m) = &session.master {
                    let _ = m.resize(PtySize { rows, cols, pixel_width: 0, pixel_height: 0 });
                }
            }
            session.info.clone()
        };
        self.publish("pty.updated", json!({"info": info}));
        Ok(info)
    }

    pub fn write(&self, id: &str, data: &str) {
        let mut s = self.sessions.lock();
        if let Some(session) = s.get_mut(id) {
            if session.info.status == "running" {
                if let Some(w) = &mut session.writer {
                    let _ = w.write_all(data.as_bytes());
                    let _ = w.flush();
                }
            }
        }
    }

    pub fn remove(&self, id: &str) -> Result<(), PtyError> {
        let session = self.sessions.lock().shift_remove(id).ok_or_else(|| PtyError::NotFound(id.into()))?;
        if session.info.status == "running" {
            if let Some(mut k) = session.killer {
                let _ = k.kill();
            }
        }
        for (_, sub) in session.subscribers {
            let _ = sub.tx.send(Chunk::End(None));
        }
        self.exited.lock().retain(|e| e != id);
        self.publish("pty.deleted", json!({"id": id}));
        Ok(())
    }

    /// `attach(id, cursor)`: `cursor = -1` → live only; `None` → full retained buffer.
    pub fn attach(&self, id: &str, cursor: Option<i64>) -> Result<Attachment, PtyError> {
        let sub = {
            let mut n = self.next_sub.lock();
            *n += 1;
            *n
        };
        let (tx, rx) = mpsc::unbounded_channel();
        let mut s = self.sessions.lock();
        let session = s.get_mut(id).ok_or_else(|| PtyError::NotFound(id.into()))?;
        if session.info.status != "running" {
            return Err(PtyError::Exited(id.into()));
        }
        session.subscribers.insert(sub, Subscriber { tx, active: false, pending: vec![], end: None });
        let end = session.cursor;
        let from = match cursor {
            Some(-1) => end,
            Some(c) if c >= 0 => c as u64,
            _ => 0,
        };
        let replay = if from >= end || session.buffer.is_empty() {
            String::new()
        } else {
            let start = from.saturating_sub(session.buffer_cursor) as usize;
            let units: Vec<u16> = session.buffer.iter().skip(start).copied().collect();
            String::from_utf16_lossy(&units)
        };
        drop(s);
        Ok(Attachment { replay, cursor: end, rx, ptys: self.clone(), id: id.into(), sub })
    }

    pub fn close(&self) {
        let ids: Vec<String> = self.sessions.lock().keys().cloned().collect();
        for id in ids {
            let _ = self.remove(&id);
        }
    }
}

//! SSH askpass helper mode (`cli/src/ssh-askpass.ts`).

use std::io::{Read, Write};
use std::net::TcpStream;
use std::time::{Duration, Instant};

/// Returns the process exit code; never logs.
pub fn run() -> i32 {
    let deadline = Instant::now() + Duration::from_secs(300);
    let inner = || -> Option<i32> {
        let port: i64 = std::env::var("OPENCODE_SSH_ASKPASS_PORT").ok()?.parse().ok()?;
        if !(1..=65_535).contains(&port) {
            return None;
        }
        let mut socket = TcpStream::connect(("127.0.0.1", port as u16)).ok()?;
        socket.set_read_timeout(Some(deadline.saturating_duration_since(Instant::now()))).ok()?;
        let text = std::env::args().skip(1).collect::<Vec<_>>().join(" ");
        let token = std::env::var("OPENCODE_SSH_ASKPASS_TOKEN").ok();
        let mut line = serde_json::Map::new();
        if let Some(t) = token {
            line.insert("token".into(), t.into());
        }
        line.insert("text".into(), text.into());
        line.insert("confirm".into(), (std::env::var("SSH_ASKPASS_PROMPT").as_deref() == Ok("confirm")).into());
        socket.write_all((serde_json::Value::Object(line).to_string() + "\n").as_bytes()).ok()?;
        let mut response = Vec::new();
        let mut buf = [0u8; 4096];
        loop {
            if Instant::now() >= deadline {
                return None;
            }
            match socket.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => response.extend_from_slice(&buf[..n]),
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(e) if matches!(e.kind(), std::io::ErrorKind::ConnectionReset | std::io::ErrorKind::BrokenPipe) => break,
                Err(_) => return None,
            }
        }
        let v: serde_json::Value = serde_json::from_slice(&response).ok()?;
        match v.as_object()?.get("value")? {
            serde_json::Value::Null => Some(1),
            serde_json::Value::String(s) => {
                let mut out = std::io::stdout().lock();
                out.write_all(format!("{s}\n").as_bytes()).ok()?;
                out.flush().ok()?;
                Some(0)
            }
            _ => None,
        }
    };
    inner().unwrap_or(1)
}

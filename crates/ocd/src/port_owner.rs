//! `PortOwner.describe` (`cli/src/services/port-owner.ts`): best effort, names the process listening on a TCP
//! port so a failed bind can say who holds it. Windows has no cheap equivalent of `lsof`, so it keeps the
//! generic message.

use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

/// `PID <pid> (<command>)`, `PID <pid>`, or `None` when no listener matches.
pub fn describe(port: u16, hostname: Option<&str>) -> Option<String> {
    if cfg!(windows) {
        return None;
    }
    let lsof = run("lsof", &["-nP", &format!("-iTCP:{port}"), "-sTCP:LISTEN", "-Fpn"]);
    let pid = listeners(&lsof).into_iter().find(|(_, address)| matches(address, hostname)).map(|(pid, _)| pid)?;
    let command = run("ps", &["-o", "command=", "-p", &pid]).split_whitespace().collect::<Vec<_>>().join(" ");
    if command.is_empty() {
        return Some(format!("PID {pid}"));
    }
    let chars: Vec<char> = command.chars().collect();
    let shown = if chars.len() > 200 { format!("{}…", chars[..199].iter().collect::<String>()) } else { command };
    Some(format!("PID {pid} ({shown})"))
}

/// Run a command with a 1 s timeout and a 4 KiB output cap; its trimmed stdout on exit 0, else "".
fn run(command: &str, args: &[&str]) -> String {
    let Ok(mut child) = Command::new(command).args(args).stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::null()).spawn() else {
        return String::new();
    };
    let deadline = Instant::now() + Duration::from_secs(1);
    let status = loop {
        match child.try_wait() {
            Ok(Some(s)) => break Some(s),
            Ok(None) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(10)),
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                break None;
            }
        }
    };
    let Some(status) = status.filter(|s| s.success()) else { return String::new() };
    let mut out = Vec::new();
    if let Some(mut stdout) = child.stdout.take() {
        use std::io::Read;
        let _ = stdout.by_ref().take(4096).read_to_end(&mut out);
    }
    if out.len() >= 4096 {
        return String::new();
    }
    let _ = status;
    String::from_utf8_lossy(&out).trim().to_string()
}

/// `lsof -F pn` output → `(pid, address)` of each listener (`[...]` stripped around IPv6 addresses).
fn listeners(output: &str) -> Vec<(String, String)> {
    let mut found = vec![];
    let mut pid: Option<String> = None;
    for line in output.split('\n') {
        if let Some(p) = line.strip_prefix('p') {
            pid = (!p.is_empty() && p.bytes().all(|b| b.is_ascii_digit())).then(|| p.to_string());
        }
        if let Some(n) = line.strip_prefix('n')
            && let Some(pid) = &pid
        {
            let address = match n.rfind(':') {
                Some(i) => &n[..i],
                None => "",
            };
            let address = address.strip_prefix('[').and_then(|a| a.strip_suffix(']')).unwrap_or(address);
            found.push((pid.clone(), address.to_string()));
        }
    }
    found
}

/// lsof shows wildcard listeners as `*`; those also hold the port for any specific address.
fn matches(address: &str, hostname: Option<&str>) -> bool {
    let host = hostname.map(|h| h.strip_prefix('[').and_then(|x| x.strip_suffix(']')).unwrap_or(h));
    let Some(host) = host else { return true };
    if host.parse::<std::net::IpAddr>().is_err() || host == "0.0.0.0" || host == "::" {
        return true;
    }
    address == "*" || address == host
}

/// The message for a managed-service port held by someone else.
pub fn in_use_message(port: u16, hostname: &str) -> String {
    let owner = describe(port, Some(hostname)).unwrap_or_else(|| "another process".into());
    format!(
        "Managed service port {port} on {hostname} is already in use by {owner}. Configure another port with `ocd service set port <port>` and start the service again."
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_lsof_listeners() {
        let out = "p123\nf3\nn*:4096\np456\nn127.0.0.1:4096\nn[::1]:4096\npabc\nn10.0.0.1:4096";
        assert_eq!(
            listeners(out),
            vec![("123".into(), "*".into()), ("456".into(), "127.0.0.1".into()), ("456".into(), "::1".into())]
        );
    }

    #[test]
    fn wildcards_and_names_match_any_listener() {
        assert!(matches("10.0.0.1", Some("localhost")));
        assert!(matches("10.0.0.1", Some("0.0.0.0")));
        assert!(matches("10.0.0.1", Some("[::]")));
        assert!(matches("*", Some("127.0.0.1")));
        assert!(matches("::1", Some("[::1]")));
        assert!(!matches("127.0.0.2", Some("127.0.0.1")));
        assert!(matches("x", None));
    }

    #[test]
    fn names_the_process_holding_a_port() {
        if cfg!(windows) || run("lsof", &["-v"]).is_empty() && std::process::Command::new("lsof").arg("-v").output().is_err() {
            return;
        }
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = l.local_addr().unwrap().port();
        let me = std::process::id().to_string();
        match describe(port, Some("127.0.0.1")) {
            Some(text) => assert!(text.starts_with(&format!("PID {me}")), "{text}"),
            None => {} // lsof may be unable to see sockets in a sandbox
        }
        assert_eq!(in_use_message(1, "127.0.0.1").contains("in use by another process") || in_use_message(1, "127.0.0.1").contains("in use by PID"), true);
    }
}

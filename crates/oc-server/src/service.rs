//! Service-mode helpers (spec 01 §7): default port, registration file, password generation.

use std::io::Write;
use std::path::{Path, PathBuf};

use base64::Engine;
use serde::{Deserialize, Serialize};
use sha1::{Digest, Sha1};

fn is_release(channel: &str) -> bool {
    matches!(channel, "latest" | "dev" | "beta" | "next")
}

/// Default service port: `0xC0DE` for latest/dev/beta/next, `0xC0DF` for local, else
/// `10000 + (parseInt(sha1hex(channel)[0..8], 16) % 50000)`.
pub fn default_service_port(channel: &str) -> u16 {
    if is_release(channel) {
        return 0xC0DE;
    }
    if channel == "local" {
        return 0xC0DF;
    }
    let hex = hex_sha1(channel);
    let n = u64::from_str_radix(&hex[..8], 16).unwrap_or(0);
    (10_000 + n % 50_000) as u16
}

fn hex_sha1(s: &str) -> String {
    Sha1::digest(s.as_bytes()).iter().map(|b| format!("{b:02x}")).collect()
}

/// `service.json` for release channels, else `service-<sanitized channel>.json`.
pub fn service_file_name(channel: &str) -> String {
    if is_release(channel) {
        return "service.json".into();
    }
    let sanitized: String = channel.chars().map(|c| if c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.' { c } else { '-' }).collect();
    format!("service-{sanitized}.json")
}

/// Default registration path: `$XDG_STATE_HOME ?? ~/.local/state` + `/opencode/<name>`.
pub fn default_registration_path(channel: &str) -> PathBuf {
    let base = std::env::var_os("XDG_STATE_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| std::env::home_dir().unwrap_or_default().join(".local").join("state"));
    base.join("opencode").join(service_file_name(channel))
}

/// Discovery contract written by a running service.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Registration {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    pub url: String,
    pub pid: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub password: Option<String>,
}

impl Registration {
    /// `{id: uuid, version, url, pid, password}` for this process.
    pub fn new(version: &str, url: &str, password: &str) -> Self {
        Self {
            id: Some(uuid::Uuid::new_v4().to_string()),
            version: Some(version.into()),
            url: url.into(),
            pid: std::process::id(),
            password: Some(password.into()),
        }
    }

    /// Same instance: id, version, url, pid equal.
    pub fn same_instance(&self, other: &Registration) -> bool {
        self.id == other.id && self.version == other.version && self.url == other.url && self.pid == other.pid
    }
}

/// Write atomically (`<file>.<id>.tmp` + rename), mode 0600.
pub fn write_registration(path: &Path, reg: &Registration) -> std::io::Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let id = reg.id.clone().unwrap_or_else(|| "tmp".into());
    let tmp = path.with_file_name(format!(
        "{}.{id}.tmp",
        path.file_name().map(|f| f.to_string_lossy().into_owned()).unwrap_or_default()
    ));
    {
        let mut opts = std::fs::OpenOptions::new();
        opts.write(true).create(true).truncate(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            opts.mode(0o600);
        }
        let mut f = opts.open(&tmp)?;
        f.write_all(serde_json::to_string(reg).unwrap_or_default().as_bytes())?;
    }
    std::fs::rename(&tmp, path)
}

/// Read a registration; missing or corrupt = `None`.
pub fn read_registration(path: &Path) -> Option<Registration> {
    let data = std::fs::read(path).ok()?;
    let r: Registration = serde_json::from_slice(&data).ok()?;
    (r.pid > 0).then_some(r)
}

/// Remove the registration only if it still describes `reg`.
pub fn remove_registration_if_owned(path: &Path, reg: &Registration) {
    if read_registration(path).is_some_and(|r| r.same_instance(reg) && r.password == reg.password) {
        let _ = std::fs::remove_file(path);
    }
}

/// Spawn the 5 s watchdog: resolves the returned future when the registration was replaced
/// or became unreadable (caller then shuts down).
pub async fn watch_registration(path: PathBuf, reg: Registration) {
    loop {
        tokio::time::sleep(std::time::Duration::from_secs(5)).await;
        match read_registration(&path) {
            None => {
                tracing::warn!("managed service registration check failed; shutting down");
                return;
            }
            Some(r) if !(r.same_instance(&reg) && r.password == reg.password) => {
                tracing::warn!("managed service registration replaced; shutting down");
                return;
            }
            _ => {}
        }
    }
}

/// Random 32-byte base64url password.
pub fn generate_password() -> String {
    let mut b = [0u8; 32];
    rand::fill(&mut b);
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(b)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ports_and_names() {
        assert_eq!(default_service_port("latest"), 49374);
        assert_eq!(default_service_port("local"), 49375);
        let p = default_service_port("preview/a");
        assert!((10_000..60_000).contains(&p));
        assert_eq!(service_file_name("dev"), "service.json");
        assert_eq!(service_file_name("preview/a"), "service-preview-a.json");
        assert_eq!(generate_password().len(), 43);
    }

    #[test]
    fn registration_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("opencode/service.json");
        let reg = Registration::new("1.0.0", "http://127.0.0.1:49374", "pw");
        write_registration(&path, &reg).unwrap();
        assert_eq!(read_registration(&path).unwrap(), reg);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(std::fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o600);
        }
        let other = Registration::new("1.0.0", "http://127.0.0.1:49374", "pw");
        remove_registration_if_owned(&path, &other);
        assert!(path.exists());
        remove_registration_if_owned(&path, &reg);
        assert!(!path.exists());
    }
}

//! Tunnel identities on disk (`OpenTunnelStorage.xdg()`): `$XDG_DATA_HOME/opentunnel/<profile>/`, the layout the
//! opentunnel CLI and every app using the SDK share, so one device keeps one tunnel.
//!
//! - `tunnel.json`: `{"id", "hostname", "certificateExpiry"}` (two-space JSON, no trailing newline), once the
//!   certificate is ready;
//! - `pending.json`: `{"id", "hostname", "csr"}` while the certificate is being issued;
//! - `token` (with a trailing newline), `private-key.pem`, `certificate.pem`, `chain.pem`.
//!
//! Files are created with mode 0600 and directories with 0700.

use std::path::{Path, PathBuf};

use chrono::{DateTime, SecondsFormat, Utc};
use serde::{Deserialize, Serialize};

use crate::Error;

/// A tunnel with a ready certificate.
#[derive(Debug, Clone, PartialEq)]
pub struct Identity {
    pub id: String,
    pub hostname: String,
    pub token: String,
    pub private_key: String,
    pub certificate: String,
    pub chain: String,
    pub certificate_expiry: DateTime<Utc>,
}

/// A tunnel whose certificate is still being issued.
#[derive(Debug, Clone, PartialEq)]
pub struct Pending {
    pub id: String,
    pub hostname: String,
    pub token: String,
    pub private_key: String,
    pub csr: String,
}

#[derive(Serialize, Deserialize)]
struct TunnelFile {
    id: serde_json::Value,
    hostname: serde_json::Value,
    #[serde(rename = "certificateExpiry")]
    certificate_expiry: serde_json::Value,
}

#[derive(Serialize, Deserialize)]
struct PendingFile {
    id: serde_json::Value,
    hostname: serde_json::Value,
    csr: serde_json::Value,
}

/// `Date.prototype.toISOString`.
pub fn iso(date: &DateTime<Utc>) -> String {
    date.to_rfc3339_opts(SecondsFormat::Millis, true)
}

/// `new Date(text)` for the ISO 8601 forms a server sends.
pub fn parse_date(text: &str) -> Option<DateTime<Utc>> {
    if let Ok(d) = DateTime::parse_from_rfc3339(text) {
        return Some(d.with_timezone(&Utc));
    }
    // Date-only forms are UTC midnight; a date-time without an offset is local time in JS (rare from a server).
    if let Ok(d) = chrono::NaiveDate::parse_from_str(text, "%Y-%m-%d") {
        return Some(d.and_hms_opt(0, 0, 0)?.and_utc());
    }
    for format in ["%Y-%m-%dT%H:%M:%S%.f", "%Y-%m-%dT%H:%M"] {
        if let Ok(d) = chrono::NaiveDateTime::parse_from_str(text, format) {
            return d.and_local_timezone(chrono::Local).single().map(|d| d.with_timezone(&Utc));
        }
    }
    None
}

fn string(value: &serde_json::Value) -> String {
    match value {
        serde_json::Value::String(s) => s.clone(),
        serde_json::Value::Null => String::new(),
        other => other.to_string(),
    }
}

/// The XDG store.
#[derive(Debug, Clone)]
pub struct Storage {
    root: PathBuf,
}

impl Storage {
    /// `xdg()` from the process environment: `$XDG_DATA_HOME` (even when empty) or `~/.local/share`.
    pub fn xdg() -> Storage {
        let data_home = match std::env::var_os("XDG_DATA_HOME") {
            Some(v) => PathBuf::from(v),
            None => home_dir().join(".local").join("share"),
        };
        Storage::at(data_home.join("opentunnel"))
    }

    /// A store rooted at `<dir>` (holding one directory per profile).
    pub fn at(root: impl Into<PathBuf>) -> Storage {
        Storage { root: root.into() }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    fn profile(&self, profile: &str) -> PathBuf {
        self.root.join(profile)
    }

    fn read_optional(path: &Path) -> Result<Option<String>, Error> {
        match std::fs::read(path) {
            Ok(bytes) => Ok(Some(String::from_utf8_lossy(&bytes).into_owned())),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(Error::storage(format!("Failed to read {}", path.display()), e.to_string())),
        }
    }

    /// Profiles with a directory, sorted.
    pub fn profiles(&self) -> Result<Vec<String>, Error> {
        let entries = match std::fs::read_dir(&self.root) {
            Ok(e) => e,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => {
                return Err(Error::storage(format!("Failed to list {}", self.root.display()), e.to_string()));
            }
        };
        let mut names: Vec<String> = entries
            .filter_map(Result::ok)
            .filter(|e| e.file_type().is_ok_and(|t| t.is_dir()))
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        Ok(names)
    }

    pub fn load(&self, profile: &str) -> Result<Option<Identity>, Error> {
        let root = self.profile(profile);
        let Some(metadata) = Self::read_optional(&root.join("tunnel.json"))? else {
            return Ok(None);
        };
        let token = Self::read_optional(&root.join("token"))?;
        let private_key = Self::read_optional(&root.join("private-key.pem"))?;
        let certificate = Self::read_optional(&root.join("certificate.pem"))?;
        let chain = Self::read_optional(&root.join("chain.pem"))?;
        let (Some(token), Some(private_key), Some(certificate), Some(chain)) = (token, private_key, certificate, chain)
        else {
            return Err(Error::storage_message(format!("Incomplete tunnel identity for {profile}")));
        };
        if token.is_empty() || private_key.is_empty() || certificate.is_empty() {
            return Err(Error::storage_message(format!("Incomplete tunnel identity for {profile}")));
        }
        let value: TunnelFile = serde_json::from_str(&metadata).map_err(|e| {
            Error::storage(format!("Failed to read {}", root.join("tunnel.json").display()), e.to_string())
        })?;
        let expiry = parse_date(&string(&value.certificate_expiry)).ok_or_else(|| {
            Error::storage_message(format!("Invalid certificate expiry in {}", root.join("tunnel.json").display()))
        })?;
        Ok(Some(Identity {
            id: string(&value.id),
            hostname: string(&value.hostname),
            token: js_trim(&token).to_string(),
            private_key,
            certificate,
            chain,
            certificate_expiry: expiry,
        }))
    }

    pub fn load_pending(&self, profile: &str) -> Result<Option<Pending>, Error> {
        let root = self.profile(profile);
        let Some(metadata) = Self::read_optional(&root.join("pending.json"))? else {
            return Ok(None);
        };
        let token = Self::read_optional(&root.join("token"))?.filter(|t| !t.is_empty());
        let private_key = Self::read_optional(&root.join("private-key.pem"))?.filter(|k| !k.is_empty());
        let (Some(token), Some(private_key)) = (token, private_key) else {
            return Err(Error::storage_message(format!("Incomplete pending tunnel identity for {profile}")));
        };
        let value: PendingFile = serde_json::from_str(&metadata).map_err(|e| {
            Error::storage(format!("Failed to read {}", root.join("pending.json").display()), e.to_string())
        })?;
        Ok(Some(Pending {
            id: string(&value.id),
            hostname: string(&value.hostname),
            token: js_trim(&token).to_string(),
            private_key,
            csr: string(&value.csr),
        }))
    }

    pub fn save_pending(&self, profile: &str, tunnel: &Pending) -> Result<(), Error> {
        let fail = |e: std::io::Error| {
            Error::storage(format!("Failed to save pending tunnel identity for {profile}"), e.to_string())
        };
        let root = self.profile(profile);
        make_dir(&root).map_err(fail)?;
        let metadata = PendingFile {
            id: tunnel.id.clone().into(),
            hostname: tunnel.hostname.clone().into(),
            csr: tunnel.csr.clone().into(),
        };
        write_private(&root.join("pending.json"), &pretty(&metadata)).map_err(fail)?;
        write_private(&root.join("token"), &format!("{}\n", tunnel.token)).map_err(fail)?;
        write_private(&root.join("private-key.pem"), &tunnel.private_key).map_err(fail)?;
        Ok(())
    }

    pub fn save(&self, profile: &str, tunnel: &Identity) -> Result<(), Error> {
        let fail =
            |e: std::io::Error| Error::storage(format!("Failed to save tunnel identity for {profile}"), e.to_string());
        let root = self.profile(profile);
        make_dir(&root).map_err(fail)?;
        let metadata = TunnelFile {
            id: tunnel.id.clone().into(),
            hostname: tunnel.hostname.clone().into(),
            certificate_expiry: iso(&tunnel.certificate_expiry).into(),
        };
        write_private(&root.join("tunnel.json"), &pretty(&metadata)).map_err(fail)?;
        write_private(&root.join("token"), &format!("{}\n", tunnel.token)).map_err(fail)?;
        write_private(&root.join("private-key.pem"), &tunnel.private_key).map_err(fail)?;
        write_private(&root.join("certificate.pem"), &tunnel.certificate).map_err(fail)?;
        write_private(&root.join("chain.pem"), &tunnel.chain).map_err(fail)?;
        match std::fs::remove_file(root.join("pending.json")) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(fail(e)),
        }
    }

    pub fn remove(&self, profile: &str) -> Result<(), Error> {
        match std::fs::remove_dir_all(self.profile(profile)) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(Error::storage(format!("Failed to remove tunnel identity for {profile}"), e.to_string())),
        }
    }
}

/// `os.homedir()`: `$HOME` on Unix and `%USERPROFILE%` on Windows, then the account's home.
fn home_dir() -> PathBuf {
    #[allow(deprecated)]
    std::env::home_dir().unwrap_or_default()
}

/// `String.prototype.trim` (JS whitespace is close enough to Unicode White_Space for a token file).
fn js_trim(value: &str) -> &str {
    value.trim_matches(|c: char| c.is_whitespace() || c == '\u{feff}')
}

/// `JSON.stringify(value, null, 2)`.
fn pretty(value: &impl Serialize) -> String {
    serde_json::to_string_pretty(value).expect("json")
}

fn make_dir(path: &Path) -> std::io::Result<()> {
    let mut builder = std::fs::DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(0o700);
    }
    builder.create(path)
}

/// `fs.writeFile(path, text, { mode: 0o600 })`: the mode applies when the file is created.
fn write_private(path: &Path, text: &str) -> std::io::Result<()> {
    use std::io::Write;
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(path)?;
    file.write_all(text.as_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn identity() -> Identity {
        Identity {
            id: "tun_1".into(),
            hostname: "abc.example".into(),
            token: "secret".into(),
            private_key: "KEY\n".into(),
            certificate: "CERT\n".into(),
            chain: String::new(),
            certificate_expiry: parse_date("2027-01-02T03:04:05Z").unwrap(),
        }
    }

    #[test]
    fn identity_files_match_the_sdk_layout() {
        let tmp = tempfile::tempdir().unwrap();
        let store = Storage::at(tmp.path().join("opentunnel"));
        assert_eq!(store.load("default").unwrap(), None);
        let pending = Pending {
            id: "tun_1".into(),
            hostname: "abc.example".into(),
            token: "secret".into(),
            private_key: "KEY\n".into(),
            csr: "CSR".into(),
        };
        store.save_pending("default", &pending).unwrap();
        let dir = tmp.path().join("opentunnel/default");
        assert_eq!(
            std::fs::read_to_string(dir.join("pending.json")).unwrap(),
            "{\n  \"id\": \"tun_1\",\n  \"hostname\": \"abc.example\",\n  \"csr\": \"CSR\"\n}"
        );
        assert_eq!(std::fs::read_to_string(dir.join("token")).unwrap(), "secret\n");
        assert_eq!(store.load_pending("default").unwrap(), Some(pending));
        assert_eq!(store.load("default").unwrap(), None);

        store.save("default", &identity()).unwrap();
        assert!(!dir.join("pending.json").exists());
        assert_eq!(
            std::fs::read_to_string(dir.join("tunnel.json")).unwrap(),
            "{\n  \"id\": \"tun_1\",\n  \"hostname\": \"abc.example\",\n  \"certificateExpiry\": \"2027-01-02T03:04:05.000Z\"\n}"
        );
        assert_eq!(store.load("default").unwrap(), Some(identity()));
        assert_eq!(store.profiles().unwrap(), vec!["default"]);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(std::fs::metadata(dir.join("token")).unwrap().permissions().mode() & 0o777, 0o600);
            assert_eq!(std::fs::metadata(&dir).unwrap().permissions().mode() & 0o777, 0o700);
        }

        std::fs::remove_file(dir.join("certificate.pem")).unwrap();
        assert_eq!(store.load("default").unwrap_err().to_string(), "Incomplete tunnel identity for default");
        store.remove("default").unwrap();
        assert_eq!(store.load("default").unwrap(), None);
    }
}

//! Restart handoff (`PersistentPty.Handoff`, `OPENCODE_PTY_HANDOFF`, `client/src/pty-handoff.ts`).
//!
//! Flow: before a managed background service is replaced, the CLI calls the old server's
//! `persistentPty.handoff` route (→ `prepare_handoff` on the owner connection) and writes a
//! sidecar `<service-file>.pty-handoff`; [`environment`] then yields the JSON for
//! `OPENCODE_PTY_HANDOFF`, which the replacement server reads ([`take_from_env`]) and passes to
//! [`crate::Options::handoff`] so it adopts the daemon with the ticket.

use std::future::Future;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Deserializer, Serialize};
use serde_json::{Map, Value};

/// Environment variable carrying the inherited handoff.
pub const ENV: &str = "OPENCODE_PTY_HANDOFF";

/// `PersistentPty.Handoff` (`{directory, instanceID, ticket, expiresAt}`; `expiresAt` is Unix ms).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Handoff {
    pub directory: String,
    #[serde(rename = "instanceID")]
    pub instance_id: String,
    pub ticket: String,
    #[serde(rename = "expiresAt", deserialize_with = "js_integer")]
    pub expires_at: i64,
}

fn js_integer<'de, D: Deserializer<'de>>(deserializer: D) -> Result<i64, D::Error> {
    let value = f64::deserialize(deserializer)?;
    if !value.is_finite() {
        return Err(serde::de::Error::custom("expected a finite number"));
    }
    Ok(value as i64)
}

impl Handoff {
    /// Decode `OPENCODE_PTY_HANDOFF` (`Schema.fromJsonString(Handoff)`).
    pub fn parse(text: &str) -> Result<Self, String> {
        serde_json::from_str(text).map_err(|e| e.to_string())
    }

    pub fn to_json(&self) -> String {
        serde_json::to_string(self).expect("handoff serializes")
    }

    fn from_value(value: &Value) -> Option<Self> {
        let object = value.as_object()?;
        let expires_at = object.get("expiresAt")?.as_f64().filter(|v| v.is_finite())?;
        Some(Handoff {
            directory: object.get("directory")?.as_str()?.to_string(),
            instance_id: object.get("instanceID")?.as_str()?.to_string(),
            ticket: object.get("ticket")?.as_str()?.to_string(),
            expires_at: expires_at as i64,
        })
    }
}

/// `server-process.ts:48-58`: read and delete `OPENCODE_PTY_HANDOFF`. Invalid JSON logs
/// `Ignoring invalid PTY restart handoff; persistent terminals will start fresh`.
///
/// Mutates the process environment; call during startup before spawning threads that read it.
pub fn take_from_env() -> Option<Handoff> {
    let inherited = std::env::var(ENV).ok();
    // SAFETY: documented requirement — called during single-threaded startup.
    unsafe { std::env::remove_var(ENV) };
    parse_inherited(inherited.as_deref())
}

/// Decoding half of [`take_from_env`] (no env mutation).
pub fn parse_inherited(value: Option<&str>) -> Option<Handoff> {
    let value = value?;
    match Handoff::parse(value) {
        Ok(handoff) => Some(handoff),
        Err(_) => {
            tracing::warn!("Ignoring invalid PTY restart handoff; persistent terminals will start fresh");
            None
        }
    }
}

/// `Sidecar.source` / the identifying part of a service `Info` (`{id?, pid, url}`).
#[derive(Debug, Clone, PartialEq)]
pub struct Source {
    pub id: Option<String>,
    pub pid: f64,
    pub url: String,
}

impl Source {
    fn to_value(&self) -> Value {
        let mut map = Map::new();
        if let Some(id) = &self.id {
            map.insert("id".into(), id.clone().into());
        }
        map.insert("pid".into(), js_number(self.pid));
        map.insert("url".into(), self.url.clone().into());
        Value::Object(map)
    }

    /// Lenient read of a service info file (`{id?, pid, url, ...}`).
    pub fn from_info(value: &Value) -> Option<Self> {
        let object = value.as_object()?;
        let id = match object.get("id") {
            None => None,
            Some(Value::String(s)) => Some(s.clone()),
            Some(_) => return None,
        };
        Some(Source { id, pid: object.get("pid")?.as_f64()?, url: object.get("url")?.as_str()?.to_string() })
    }
}

/// `<service-file>.pty-handoff` contents.
#[derive(Debug, Clone, PartialEq)]
pub struct Sidecar {
    pub source: Source,
    pub handoff: Option<Handoff>,
    pub expires_at: f64,
}

impl Sidecar {
    pub fn to_json(&self) -> String {
        let mut map = Map::new();
        map.insert("source".into(), self.source.to_value());
        map.insert(
            "handoff".into(),
            self.handoff.as_ref().map(|h| serde_json::to_value(h).expect("handoff")).unwrap_or(Value::Null),
        );
        map.insert("expiresAt".into(), js_number(self.expires_at));
        Value::Object(map).to_string()
    }

    /// `read()` validation.
    pub fn from_value(value: &Value) -> Option<Self> {
        let object = value.as_object()?;
        let source = object.get("source")?.as_object()?;
        let pid = source.get("pid")?.as_f64()?;
        let url = source.get("url")?.as_str()?.to_string();
        let id = match source.get("id") {
            None => None,
            Some(Value::String(s)) => Some(s.clone()),
            Some(_) => return None,
        };
        let handoff = match object.get("handoff")? {
            Value::Null => None,
            other => Some(Handoff::from_value(other)?),
        };
        let expires_at = object.get("expiresAt")?.as_f64().filter(|v| v.is_finite())?;
        Some(Sidecar { source: Source { id, pid, url }, handoff, expires_at })
    }
}

fn js_number(value: f64) -> Value {
    if value.fract() == 0.0 && value.abs() < 9_007_199_254_740_992.0 {
        Value::from(value as i64)
    } else {
        serde_json::Number::from_f64(value).map(Value::Number).unwrap_or(Value::Null)
    }
}

fn now() -> f64 {
    crate::daemon::now_ms() as f64
}

fn same(left: &Source, right: &Source) -> bool {
    left.id == right.id && left.pid == right.pid && left.url == right.url
}

pub fn sidecar_path(file: &Path) -> PathBuf {
    let mut name = file.as_os_str().to_owned();
    name.push(".pty-handoff");
    PathBuf::from(name)
}

/// `read(file)`.
pub async fn read(file: &Path) -> Option<Sidecar> {
    let text = tokio::fs::read_to_string(sidecar_path(file)).await.ok()?;
    Sidecar::from_value(&serde_json::from_str(&text).ok()?)
}

/// `publish(file, info, handoff)`: atomic write (mode 0600) of the sidecar.
pub async fn publish(file: &Path, source: &Source, handoff: Option<Handoff>) -> std::io::Result<()> {
    let expires_at = handoff.as_ref().map(|h| h.expires_at as f64).unwrap_or_else(|| now() + 30_000.0);
    let sidecar = Sidecar { source: source.clone(), handoff, expires_at };
    let mut temporary = file.as_os_str().to_owned();
    temporary.push(format!(".pty-handoff.{}.tmp", uuid::Uuid::new_v4()));
    let temporary = PathBuf::from(temporary);
    let target = sidecar_path(file);
    let body = sidecar.to_json();
    let tmp = temporary.clone();
    let result = tokio::task::spawn_blocking(move || -> std::io::Result<()> {
        use std::io::Write;
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        options.open(&tmp)?.write_all(body.as_bytes())?;
        std::fs::rename(&tmp, &target)
    })
    .await
    .map_err(std::io::Error::other)
    .and_then(|r| r);
    let _ = tokio::fs::remove_file(&temporary).await;
    result
}

/// `environment(file)`: the `OPENCODE_PTY_HANDOFF` value for a replacement server, if the
/// sidecar is unexpired and still describes the current service (`None` = leave unset).
pub async fn environment(file: &Path) -> Option<String> {
    let record = read(file).await?;
    let current = tokio::fs::read_to_string(file)
        .await
        .ok()
        .and_then(|text| serde_json::from_str::<Value>(&text).ok());
    if record.expires_at <= now() {
        return None;
    }
    if let Some(current) = current {
        let info = Source::from_info(&current);
        if !info.is_some_and(|info| same(&record.source, &info)) {
            return None;
        }
    }
    record.handoff.map(|h| h.to_json())
}

/// `complete(file, info)`: drop a sidecar that belongs to another service instance.
pub async fn complete(file: &Path, info: &Source) {
    if let Some(record) = read(file).await
        && !same(&record.source, info) {
            clear(file).await;
        }
}

/// `clear(file)`.
pub async fn clear(file: &Path) {
    let _ = tokio::fs::remove_file(sidecar_path(file)).await;
}

/// Decode a `persistentPty.handoff` HTTP response body (`{handoff: Handoff | null}`).
pub fn parse_response(body: &Value) -> Result<Option<Handoff>, String> {
    let handoff = body
        .as_object()
        .and_then(|o| o.get("handoff"))
        .ok_or_else(|| "Invalid persistent terminal handoff response".to_string())?;
    if handoff.is_null() {
        return Ok(None);
    }
    match Handoff::from_value(handoff) {
        Some(h) if (h.expires_at as f64) > now() => Ok(Some(h)),
        _ => Err("Invalid or expired persistent terminal handoff".into()),
    }
}

/// `prepare(file, info, timeout)`. `request_handoff` performs the `persistentPty.handoff` call and
/// returns its JSON body; `shutdown` performs `persistentPty.shutdown`. Callers apply timeouts.
/// Never fails: terminals are best-effort.
pub async fn prepare<H, HF, S, SF>(file: &Path, info: &Source, request_handoff: H, shutdown: S)
where
    H: FnOnce() -> HF,
    HF: Future<Output = Result<Value, String>>,
    S: FnOnce() -> SF,
    SF: Future<Output = ()>,
{
    let valid = |record: &Option<Sidecar>| {
        record.as_ref().is_some_and(|r| r.expires_at > now() && same(&r.source, info))
    };
    if valid(&read(file).await) {
        return;
    }
    let handoff = match request_handoff().await.and_then(|body| parse_response(&body)) {
        Ok(handoff) => handoff,
        Err(cause) => {
            // Another caller may already have prepared and stopped this server.
            if valid(&read(file).await) {
                return;
            }
            tracing::warn!(
                cause = %cause,
                "Background service cannot hand off persistent terminals; shutting them down before replacement"
            );
            shutdown().await;
            None
        }
    };
    if let Err(cause) = publish(file, info, handoff).await {
        tracing::warn!(cause = %cause, "Failed to publish persistent terminal handoff");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn handoff_json_round_trip() {
        let h = Handoff { directory: "/s/pty/abc".into(), instance_id: "i".into(), ticket: "t".into(), expires_at: 1_700_000_000_000 };
        assert_eq!(h.to_json(), r#"{"directory":"/s/pty/abc","instanceID":"i","ticket":"t","expiresAt":1700000000000}"#);
        assert_eq!(Handoff::parse(&h.to_json()).unwrap(), h);
        assert_eq!(
            Handoff::parse(r#"{"directory":"d","instanceID":"i","ticket":"t","expiresAt":12.5}"#).unwrap().expires_at,
            12
        );
        assert!(Handoff::parse(r#"{"directory":"d","ticket":"t","expiresAt":1}"#).is_err());
        assert!(parse_inherited(Some("{bad")).is_none());
        assert!(parse_inherited(None).is_none());
    }

    #[test]
    fn sidecar_json() {
        let s = Sidecar {
            source: Source { id: None, pid: 10.0, url: "http://x".into() },
            handoff: None,
            expires_at: 5.0,
        };
        assert_eq!(s.to_json(), r#"{"source":{"pid":10,"url":"http://x"},"handoff":null,"expiresAt":5}"#);
        assert_eq!(Sidecar::from_value(&serde_json::from_str(&s.to_json()).unwrap()).unwrap(), s);
        // id must be a string when present.
        let bad = serde_json::json!({"source": {"id": 1, "pid": 1, "url": "u"}, "handoff": null, "expiresAt": 1});
        assert!(Sidecar::from_value(&bad).is_none());
    }

    #[test]
    fn response_parsing() {
        assert_eq!(parse_response(&serde_json::json!({"handoff": null})).unwrap(), None);
        assert!(parse_response(&serde_json::json!({})).is_err());
        let expired = serde_json::json!({"handoff": {"directory": "d", "instanceID": "i", "ticket": "t", "expiresAt": 1}});
        assert_eq!(parse_response(&expired).unwrap_err(), "Invalid or expired persistent terminal handoff");
        let future = serde_json::json!({"handoff": {"directory": "d", "instanceID": "i", "ticket": "t", "expiresAt": now() + 60_000.0}});
        assert!(parse_response(&future).unwrap().is_some());
    }

    #[tokio::test]
    async fn sidecar_files() {
        let dir = std::env::temp_dir().join(format!("oc-pty-handoff-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("service.json");
        let info = Source { id: Some("svc".into()), pid: 42.0, url: "http://127.0.0.1:1".into() };
        std::fs::write(&file, r#"{"id":"svc","pid":42,"url":"http://127.0.0.1:1","password":"p"}"#).unwrap();
        let handoff =
            Handoff { directory: "/d".into(), instance_id: "i".into(), ticket: "t".into(), expires_at: now() as i64 + 60_000 };
        let body = serde_json::json!({ "handoff": serde_json::to_value(&handoff).unwrap() });
        prepare(&file, &info, || async move { Ok(body) }, || async {}).await;
        assert_eq!(environment(&file).await.unwrap(), handoff.to_json());
        // A different running service invalidates the sidecar.
        std::fs::write(&file, r#"{"id":"other","pid":42,"url":"http://127.0.0.1:1"}"#).unwrap();
        assert_eq!(environment(&file).await, None);
        complete(&file, &Source { id: Some("other".into()), pid: 42.0, url: "http://127.0.0.1:1".into() }).await;
        assert!(read(&file).await.is_none());
        // Failure: shuts down and publishes a null handoff.
        let mut shut = false;
        prepare(&file, &info, || async { Err("boom".to_string()) }, || {
            shut = true;
            async {}
        })
        .await;
        assert!(shut);
        let record = read(&file).await.unwrap();
        assert_eq!(record.handoff, None);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(std::fs::metadata(sidecar_path(&file)).unwrap().permissions().mode() & 0o777, 0o600);
        }
        let _ = std::fs::remove_dir_all(&dir);
    }
}

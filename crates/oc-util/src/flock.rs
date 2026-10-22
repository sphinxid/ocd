//! Directory-based cross-process lock (`util/src/flock.ts`), interoperable with the TS implementation.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime};

#[derive(Debug, Clone)]
pub struct Options {
    pub stale: Duration,
    pub timeout: Duration,
    pub base_delay: Duration,
    pub max_delay: Duration,
}

impl Default for Options {
    fn default() -> Self {
        Options {
            stale: Duration::from_secs(60),
            timeout: Duration::from_secs(300),
            base_delay: Duration::from_millis(100),
            max_delay: Duration::from_millis(2000),
        }
    }
}

pub struct Lease {
    dir: PathBuf,
    token: String,
    stop: Option<tokio::sync::oneshot::Sender<()>>,
}

fn mtime_age(p: &Path) -> Option<Duration> {
    let m = std::fs::metadata(p).ok()?.modified().ok()?;
    Some(SystemTime::now().duration_since(m).unwrap_or_default())
}

fn stale(dir: &Path, stale: Duration) -> bool {
    for p in [dir.join("heartbeat"), dir.join("meta.json"), dir.to_path_buf()] {
        if let Some(age) = mtime_age(&p) {
            return age > stale;
        }
    }
    false
}

fn mkdir700(p: &Path) -> std::io::Result<()> {
    #[cfg_attr(not(unix), allow(unused_mut))]
    let mut b = std::fs::DirBuilder::new();
    #[cfg(unix)]
    std::os::unix::fs::DirBuilderExt::mode(&mut b, 0o700);
    b.create(p)
}

fn try_acquire(dir: &Path, opts: &Options) -> anyhow::Result<Option<String>> {
    use std::io::ErrorKind;
    match mkdir700(dir) {
        Ok(()) => {}
        Err(e) if e.kind() == ErrorKind::AlreadyExists => {
            if !stale(dir, opts.stale) {
                return Ok(None);
            }
            let breaker = PathBuf::from(format!("{}.breaker", dir.display()));
            match mkdir700(&breaker) {
                Ok(()) => {}
                Err(e) if e.kind() == ErrorKind::AlreadyExists => {
                    if mtime_age(&breaker).is_some_and(|a| a > opts.stale) {
                        let _ = std::fs::remove_dir_all(&breaker);
                    }
                    return Ok(None);
                }
                Err(e) if e.kind() == ErrorKind::NotFound => return Ok(None),
                Err(e) => return Err(e.into()),
            }
            let result = (|| -> anyhow::Result<bool> {
                if !stale(dir, opts.stale) {
                    return Ok(false);
                }
                let _ = std::fs::remove_dir_all(dir);
                match mkdir700(dir) {
                    Ok(()) => Ok(true),
                    Err(e) if e.kind() == ErrorKind::AlreadyExists => Ok(false),
                    Err(e) => Err(e.into()),
                }
            })();
            let _ = std::fs::remove_dir_all(&breaker);
            if !result? {
                return Ok(None);
            }
        }
        Err(e) => return Err(e.into()),
    }
    let token = uuid::Uuid::new_v4().to_string();
    let create_new = |p: PathBuf, body: &str, what: &str| -> anyhow::Result<()> {
        use std::io::Write;
        match std::fs::OpenOptions::new().write(true).create_new(true).open(&p) {
            Ok(mut f) => Ok(f.write_all(body.as_bytes())?),
            Err(_) => {
                let _ = std::fs::remove_dir_all(dir);
                anyhow::bail!("Lock acquired but {what} already existed (possible compromise).")
            }
        }
    };
    create_new(dir.join("heartbeat"), "", "heartbeat")?;
    let meta = serde_json::json!({
        "token": token,
        "pid": std::process::id(),
        "hostname": hostname(),
        "createdAt": crate::log::iso_now(),
    });
    create_new(dir.join("meta.json"), &serde_json::to_string_pretty(&meta)?, "meta.json")?;
    Ok(Some(token))
}

fn hostname() -> String {
    std::fs::read_to_string("/etc/hostname")
        .map(|s| s.trim().to_string())
        .or_else(|_| std::env::var("HOSTNAME"))
        .unwrap_or_default()
}

fn jitter(d: Duration) -> Duration {
    let ms = d.as_millis() as i64;
    let j = ms * 3 / 10;
    let delta = rand::random_range(-j..=j);
    Duration::from_millis((ms + delta).max(0) as u64)
}

/// Acquire the lock for `key` under `root` (default `<state>/locks`).
pub async fn acquire(root: &Path, key: &str, opts: Options) -> anyhow::Result<Lease> {
    tokio::fs::create_dir_all(root).await?;
    let dir = root.join(format!("{}.lock", crate::hash::fast(key)));
    let deadline = Instant::now() + opts.timeout;
    let mut delay = opts.base_delay;
    let token = loop {
        if let Some(token) = try_acquire(&dir, &opts)? {
            break token;
        }
        if Instant::now() > deadline {
            anyhow::bail!("Timed out waiting for lock: {key}");
        }
        tokio::time::sleep(jitter(delay)).await;
        delay = opts.max_delay.min(Duration::from_millis((delay.as_millis() as f64 * 1.7) as u64));
    };
    let (tx, mut rx) = tokio::sync::oneshot::channel::<()>();
    let heartbeat = dir.join("heartbeat");
    let interval = Duration::from_millis((opts.stale.as_millis() as u64 / 3).max(100));
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(interval);
        tick.tick().await;
        loop {
            tokio::select! {
                _ = &mut rx => break,
                _ = tick.tick() => {
                    if let Ok(f) = std::fs::File::options().write(true).open(&heartbeat) {
                        let _ = f.set_modified(SystemTime::now());
                    }
                }
            }
        }
    });
    Ok(Lease { dir, token, stop: Some(tx) })
}

impl Lease {
    pub fn release(mut self) -> anyhow::Result<()> {
        self.release_inner()
    }

    fn release_inner(&mut self) -> anyhow::Result<()> {
        let Some(stop) = self.stop.take() else { return Ok(()) };
        let _ = stop.send(());
        let raw = std::fs::read_to_string(self.dir.join("meta.json"))
            .map_err(|_| anyhow::anyhow!("Refusing to release: lock is compromised (metadata missing)."))?;
        let parsed: serde_json::Value = serde_json::from_str(&raw)
            .map_err(|_| anyhow::anyhow!("Refusing to release: lock is compromised (metadata invalid)."))?;
        if parsed.get("token").and_then(|t| t.as_str()) != Some(self.token.as_str()) {
            anyhow::bail!("Refusing to release: lock token mismatch (not the owner).");
        }
        std::fs::remove_dir_all(&self.dir)?;
        Ok(())
    }
}

impl Drop for Lease {
    fn drop(&mut self) {
        let _ = self.release_inner();
    }
}

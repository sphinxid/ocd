//! npm plugin packages in upstream's cache layout (`packages/util/src/npm.ts`), installed with
//! `bun add` instead of in-process Arborist.
//!
//! Layout: `<cache>/npm/<key>/<generation>/node_modules/<name>`; key = `<name>@<spec>` for
//! registry specs (`latest` when bare), `git-<slug>-<sha256(spec)[0..12]>` for git specs, the raw
//! spec otherwise (Windows-illegal characters → `_`). Installs go to `.staging-<ms>-<uuid>` and
//! are renamed to `max(now, newest + 1)`; the current generation is the highest numeric child.
//! Lifecycle scripts are disabled (`--ignore-scripts`), versions saved exactly (`--exact`). Install
//! is serialized per directory with the cross-process lock `npm-install:<dir>`. Bun reads
//! `.npmrc` (registry, auth) like npm.

use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::protocol::Installed;

const RETENTION: Duration = Duration::from_secs(7 * 24 * 60 * 60);
const STAGING_RETENTION: Duration = Duration::from_secs(60 * 60);

/// A parsed package spec (the subset of npm-package-arg upstream accepts).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Target {
    Registry { name: String, spec: String, mutable: bool },
    Git { name: Option<String>, slug: String, mutable: bool },
}

fn valid_name(name: &str) -> bool {
    let check = |part: &str| {
        !part.is_empty()
            && !part.starts_with('.')
            && !part.starts_with('_')
            && part.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || "-._~".contains(c))
    };
    match name.strip_prefix('@') {
        Some(scoped) => scoped.split_once('/').is_some_and(|(scope, pkg)| check(scope) && check(pkg) && !pkg.contains('/')),
        None => name.len() <= 214 && check(name) && !name.contains('/'),
    }
}

fn is_hex_commit(value: &str) -> bool {
    (value.len() == 40 || value.len() == 64) && value.chars().all(|c| c.is_ascii_hexdigit())
}

fn is_git(spec: &str) -> bool {
    const PREFIXES: [&str; 8] = ["git+", "git://", "github:", "gitlab:", "bitbucket:", "gist:", "git@", "ssh://git@"];
    if PREFIXES.iter().any(|p| spec.starts_with(p)) {
        return true;
    }
    let base = spec.split('#').next().unwrap_or(spec);
    if (base.starts_with("https://") || base.starts_with("http://")) && base.ends_with(".git") {
        return true;
    }
    // GitHub shorthand `user/repo[#ref]`.
    !spec.starts_with('@')
        && !spec.starts_with('.')
        && !spec.starts_with('/')
        && !base.contains(':')
        && base.split('/').count() == 2
        && base.split('/').all(|p| !p.is_empty() && p.chars().all(|c| c.is_ascii_alphanumeric() || "-._".contains(c)))
}

fn git_slug(spec: &str) -> String {
    let base = spec.split('#').next().unwrap_or(spec);
    let decoded = percent_decode(base).unwrap_or_else(|| base.to_string());
    let trimmed = decoded.strip_suffix(".git").or_else(|| decoded.strip_suffix(".GIT")).unwrap_or(&decoded).to_string();
    let last = trimmed.rsplit(['/', ':', '\\']).next().unwrap_or("");
    let mut out = String::new();
    let mut dash = false;
    for c in last.chars() {
        if c.is_ascii_alphanumeric() || "._-".contains(c) {
            out.push(c);
            dash = false;
        } else if !dash {
            out.push('-');
            dash = true;
        }
    }
    let out = out.trim_matches('-').to_string();
    if out.is_empty() { "repository".into() } else { out }
}

fn percent_decode(input: &str) -> Option<String> {
    let bytes = input.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' {
            let hex = std::str::from_utf8(bytes.get(i + 1..i + 3)?).ok()?;
            out.push(u8::from_str_radix(hex, 16).ok()?);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8(out).ok()
}

fn is_exact_version(spec: &str) -> bool {
    let core = spec.strip_prefix('v').unwrap_or(spec).strip_prefix('=').unwrap_or(spec);
    let main = core.split(['-', '+']).next().unwrap_or("");
    let parts: Vec<&str> = main.split('.').collect();
    parts.len() == 3 && parts.iter().all(|p| !p.is_empty() && p.chars().all(|c| c.is_ascii_digit()))
}

/// `parse` in npm.ts.
pub fn parse(pkg: &str) -> Option<Target> {
    let pkg = pkg.trim();
    if pkg.is_empty() {
        return None;
    }
    // `name@git-url` keeps the name.
    let (named, rest) = match pkg.strip_prefix('@') {
        Some(scoped) => match scoped.find('@') {
            Some(i) => (Some(&pkg[..i + 1]), Some(&pkg[i + 2..])),
            None => (Some(pkg), None),
        },
        None => match pkg.find('@') {
            Some(0) | None => (Some(pkg), None),
            Some(i) if pkg[..i].contains(['/', ':']) => (None, None),
            Some(i) => (Some(&pkg[..i]), Some(&pkg[i + 1..])),
        },
    };
    if let (Some(name), Some(rest)) = (named, rest)
        && valid_name(name)
    {
        if is_git(rest) && rest.contains(['/', ':']) {
            let committish = rest.split_once('#').map(|(_, c)| c).unwrap_or("");
            return Some(Target::Git { name: Some(name.into()), slug: git_slug(pkg), mutable: !is_hex_commit(committish) });
        }
        if rest.is_empty() || rest.starts_with("npm:") || rest.starts_with("file:") || rest.contains("://") || rest.starts_with('.') || rest.starts_with('/') {
            return None;
        }
        return Some(Target::Registry { name: name.into(), spec: rest.into(), mutable: !is_exact_version(rest) });
    }
    if is_git(pkg) {
        let committish = pkg.split_once('#').map(|(_, c)| c).unwrap_or("");
        return Some(Target::Git { name: None, slug: git_slug(pkg), mutable: !is_hex_commit(committish) });
    }
    if named == Some(pkg) && valid_name(pkg) {
        return Some(Target::Registry { name: pkg.into(), spec: "latest".into(), mutable: true });
    }
    None
}

/// Upstream's install target check (`"Plugin target must be an npm registry package or Git package specifier"`).
pub fn is_installable(pkg: &str) -> bool {
    parse(pkg).is_some()
}

pub fn sanitize(pkg: &str) -> String {
    if !cfg!(windows) {
        return pkg.to_string();
    }
    pkg.chars().map(|c| if "<>:\"|?*".contains(c) || (c as u32) < 32 { '_' } else { c }).collect()
}

/// `cacheKey`.
pub fn cache_key(pkg: &str) -> String {
    match parse(pkg) {
        Some(Target::Git { slug, .. }) => format!("git-{slug}-{}", &hex::encode(Sha256::digest(pkg.as_bytes()))[..12]),
        Some(Target::Registry { name, spec, .. }) => sanitize(&format!("{name}@{spec}")),
        None => sanitize(pkg),
    }
}

/// Install failures.
#[derive(Debug, thiserror::Error)]
pub enum NpmError {
    #[error("Failed to install {pkg}: {message}")]
    Install { pkg: String, message: String },
    #[error("{0}")]
    Io(String),
}

/// The npm cache rooted at `<Global.cache>/npm`.
#[derive(Debug, Clone)]
pub struct Npm {
    pub root: PathBuf,
    /// Lock root (`<Global.state>/locks`).
    pub locks: PathBuf,
}

fn now_ms() -> u128 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis()).unwrap_or(0)
}

fn read_json(path: &Path) -> Option<Value> {
    serde_json::from_slice(&std::fs::read(path).ok()?).ok()
}

impl Npm {
    pub fn new(cache: &Path, locks: PathBuf) -> Self {
        Self { root: cache.join("npm"), locks }
    }

    pub fn directory(&self, pkg: &str) -> PathBuf {
        self.root.join(cache_key(pkg))
    }

    /// Numeric generations, ascending.
    pub fn generations(dir: &Path) -> Vec<u128> {
        let mut out: Vec<u128> = std::fs::read_dir(dir)
            .map(|r| r.flatten().filter_map(|e| e.file_name().to_str().and_then(|n| n.parse().ok())).collect())
            .unwrap_or_default();
        out.sort();
        out
    }

    pub fn current(dir: &Path) -> Option<PathBuf> {
        Self::generations(dir).last().map(|g| dir.join(g.to_string()))
    }

    fn installed_name(pkg: &str, dir: &Path, target: Option<&Target>) -> String {
        match target {
            Some(Target::Registry { name, .. }) | Some(Target::Git { name: Some(name), .. }) => return name.clone(),
            _ => {}
        }
        read_json(&dir.join("package.json"))
            .and_then(|m| m.get("dependencies")?.as_object()?.keys().next().cloned())
            .unwrap_or_else(|| pkg.to_string())
    }

    fn git_revision(resolved: &str) -> Option<String> {
        let hash = resolved.rsplit('#').next()?;
        let hash = hash.split("::").next().unwrap_or(hash);
        is_hex_commit(hash).then(|| hash.to_string())
    }

    /// Git revision recorded by the installer (npm `package-lock.json` or bun's `bun.lock`).
    fn installed_revision(root: &Path, name: &str, target: &Target) -> Option<String> {
        let dir = root.join("node_modules").join(name);
        if let Target::Registry { .. } = target {
            return read_json(&dir.join("package.json"))?.get("version")?.as_str().map(String::from);
        }
        for file in [root.join("package-lock.json"), root.join("node_modules").join(".package-lock.json")] {
            if let Some(resolved) = read_json(&file)
                .and_then(|l| l.get("packages")?.get(format!("node_modules/{name}"))?.get("resolved")?.as_str().map(String::from))
                && let Some(rev) = Self::git_revision(&resolved)
            {
                return Some(rev);
            }
        }
        // bun.lock is JSONC; find the entry for `name` and take its `#<sha>`.
        let lock = std::fs::read_to_string(root.join("bun.lock")).ok()?;
        let needle = format!("\"{name}\": [");
        let start = lock.find(&needle)? + needle.len();
        let entry = &lock[start..lock[start..].find(']').map(|e| start + e).unwrap_or(lock.len())];
        entry.split('"').filter(|s| s.contains('#')).find_map(Self::git_revision)
    }

    fn metadata(root: &Path, name: &str, dir: &Path, target: Option<&Target>) -> Installed {
        let manifest_version = read_json(&dir.join("package.json")).and_then(|m| m.get("version")?.as_str().map(String::from));
        let revision = target.and_then(|t| Self::installed_revision(root, name, t)).or_else(|| manifest_version.clone().filter(|_| target.is_some()));
        let version = match target {
            Some(Target::Git { .. }) => revision.clone(),
            _ => manifest_version,
        };
        Installed { directory: dir.to_string_lossy().into_owned(), name: name.to_string(), version, revision }
    }

    /// `Npm.resolve`: what is on disk, without installing. `exists` tells whether it is installed.
    pub fn resolve(&self, pkg: &str) -> (Installed, bool) {
        let target = parse(pkg);
        let root = self.directory(pkg);
        let generation = Self::current(&root);
        let base = generation.clone().unwrap_or_else(|| root.clone());
        let name = Self::installed_name(pkg, &base, target.as_ref());
        let dir = base.join("node_modules").join(&name);
        if !dir.exists() {
            return (Installed { directory: dir.to_string_lossy().into_owned(), name, version: None, revision: None }, false);
        }
        (Self::metadata(&base, &name, &dir, target.as_ref()), true)
    }

    /// `Npm.add`: installs unless the current generation already has the package.
    pub async fn add(&self, bun: &Path, pkg: &str) -> Result<Installed, NpmError> {
        self.install(bun, pkg, false).await
    }

    /// `Npm.check`: whether a newer revision of a mutable spec is available (the registry's
    /// resolution of the spec through `bun info`, or the Git ref through `git ls-remote`).
    pub async fn check(&self, bun: &Path, pkg: &str) -> Result<bool, NpmError> {
        let failed = |message: String| NpmError::Install { pkg: pkg.into(), message };
        let target = parse(pkg).ok_or_else(|| failed("Package checks only support registry and Git package specs".into()))?;
        let mutable = match &target {
            Target::Registry { mutable, .. } | Target::Git { mutable, .. } => *mutable,
        };
        if !mutable {
            return Ok(false);
        }
        let (installed, exists) = self.resolve(pkg);
        if !exists {
            return Err(failed(format!("Package is not installed: {pkg}")));
        }
        let root = Self::current(&self.directory(pkg)).unwrap_or_else(|| self.directory(pkg));
        let available = match &target {
            Target::Registry { name, spec, .. } => {
                let out = tokio::process::Command::new(bun)
                    .args(["info", &format!("{name}@{spec}"), "version", "--json"])
                    .current_dir(&root)
                    .stdin(std::process::Stdio::null())
                    .output()
                    .await
                    .map_err(|e| failed(e.to_string()))?;
                if !out.status.success() {
                    return Err(failed(String::from_utf8_lossy(&out.stderr).trim().to_string()));
                }
                let text = String::from_utf8_lossy(&out.stdout).trim().to_string();
                serde_json::from_str::<serde_json::Value>(&text)
                    .ok()
                    .and_then(|v| match v {
                        serde_json::Value::String(s) => Some(s),
                        serde_json::Value::Array(a) => a.last().and_then(|v| v.as_str().map(String::from)),
                        _ => None,
                    })
                    .or_else(|| (!text.is_empty()).then_some(text))
            }
            Target::Git { .. } => {
                let spec = pkg.split_once('@').filter(|(n, r)| !n.is_empty() && !n.contains(['/', ':']) && r.contains(['/', ':'])).map(|(_, r)| r).unwrap_or(pkg);
                let (url, reference) = spec.split_once('#').unwrap_or((spec, "HEAD"));
                let url = url.strip_prefix("git+").unwrap_or(url);
                let url = match url.strip_prefix("github:") {
                    Some(repo) => format!("https://github.com/{repo}.git"),
                    None => url.to_string(),
                };
                let out = tokio::process::Command::new("git")
                    .args(["ls-remote", &url, reference])
                    .stdin(std::process::Stdio::null())
                    .output()
                    .await
                    .map_err(|e| failed(e.to_string()))?;
                String::from_utf8_lossy(&out.stdout).split_whitespace().next().map(String::from)
            }
        };
        let available = available.ok_or_else(|| failed(format!("Package revision not found: {pkg}")))?;
        let current = match &target {
            Target::Git { .. } => installed.revision.clone(),
            Target::Registry { .. } => installed.version.clone(),
        };
        Ok(current.as_deref() != Some(available.as_str()))
    }

    /// `Npm.update`: reinstalls mutable specs, then garbage-collects generations.
    pub async fn update(&self, bun: &Path, pkg: &str) -> Result<Installed, NpmError> {
        let mutable = match parse(pkg) {
            Some(Target::Registry { mutable, .. } | Target::Git { mutable, .. }) => mutable,
            None => {
                return Err(NpmError::Install {
                    pkg: pkg.into(),
                    message: "Package updates only support registry and Git package specs".into(),
                });
            }
        };
        if !mutable {
            return self.add(bun, pkg).await;
        }
        let installed = self.install(bun, pkg, true).await?;
        Self::collect(&self.directory(pkg));
        Ok(installed)
    }

    async fn install(&self, bun: &Path, pkg: &str, update: bool) -> Result<Installed, NpmError> {
        let target = parse(pkg);
        let dir = self.directory(pkg);
        let lock = oc_util::flock::acquire(&self.locks, &format!("npm-install:{}", dir.display()), Default::default())
            .await
            .map_err(|e| NpmError::Io(e.to_string()))?;
        let result = self.install_locked(bun, pkg, target.as_ref(), &dir, update).await;
        let _ = lock.release();
        result
    }

    async fn install_locked(&self, bun: &Path, pkg: &str, target: Option<&Target>, dir: &Path, update: bool) -> Result<Installed, NpmError> {
        let failed = |message: String| NpmError::Install { pkg: pkg.to_string(), message };
        let active = Self::current(dir);
        let name = Self::installed_name(pkg, active.as_deref().unwrap_or(dir), target);
        if let Some(active) = &active
            && !update
            && active.join("node_modules").join(&name).exists()
        {
            return Ok(Self::metadata(active, &name, &active.join("node_modules").join(&name), target));
        }
        std::fs::create_dir_all(dir).map_err(|e| failed(e.to_string()))?;
        let root = std::fs::canonicalize(dir).map_err(|e| failed(e.to_string()))?;
        let staging = root.join(format!(".staging-{}-{:032x}", now_ms(), rand::random::<u128>()));
        let staged = async {
            std::fs::create_dir_all(&staging).map_err(|e| failed(e.to_string()))?;
            std::fs::write(staging.join("package.json"), "{}\n").map_err(|e| failed(e.to_string()))?;
            // Keep the user's registry configuration (`.npmrc` next to the cache dir, as upstream's NpmConfig).
            if let Ok(npmrc) = std::fs::read(dir.join(".npmrc")) {
                let _ = std::fs::write(staging.join(".npmrc"), npmrc);
            }
            let mut command = tokio::process::Command::new(bun);
            command
                .arg("add")
                .arg(pkg)
                .args(["--ignore-scripts", "--exact", "--no-progress", "--no-summary"])
                .current_dir(&staging)
                .stdin(std::process::Stdio::null())
                .kill_on_drop(true);
            if update {
                command.arg("--force");
            }
            let output = command.output().await.map_err(|e| failed(e.to_string()))?;
            if !output.status.success() {
                let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
                return Err(failed(if stderr.is_empty() { format!("bun add exited with {}", output.status) } else { stderr }));
            }
            let name = Self::installed_name(pkg, &staging, target);
            let package = staging.join("node_modules").join(&name);
            if !package.is_dir() {
                return Err(failed(format!("package directory missing after install: {name}")));
            }
            Ok(Self::metadata(&staging, &name, &package, target))
        }
        .await;
        let staged = match staged {
            Ok(staged) => staged,
            Err(error) => {
                let _ = std::fs::remove_dir_all(&staging);
                return Err(error);
            }
        };
        if let Some(active) = &active {
            let current = Self::metadata(active, &name, &active.join("node_modules").join(&name), target);
            if current.revision.is_some() && current.revision == staged.revision {
                let _ = std::fs::remove_dir_all(&staging);
                return Ok(current);
            }
        }
        let newest = Self::generations(dir).last().copied().unwrap_or(0);
        let generation = dir.join(now_ms().max(newest + 1).to_string());
        std::fs::rename(&staging, &generation).map_err(|e| failed(e.to_string()))?;
        let package = generation.join("node_modules").join(&staged.name);
        Ok(Installed { directory: package.to_string_lossy().into_owned(), ..staged })
    }

    /// `collect`: keeps the newest two generations, drops others older than 7 days and stagings
    /// older than an hour.
    pub fn collect(dir: &Path) {
        let now = now_ms();
        let completed = Self::generations(dir);
        let keep: Vec<String> = completed.iter().rev().take(2).map(|g| g.to_string()).collect();
        let Ok(read) = std::fs::read_dir(dir) else { return };
        for entry in read.flatten() {
            let Some(name) = entry.file_name().to_str().map(String::from) else { continue };
            let (timestamp, max_age) = if let Ok(n) = name.parse::<u128>() {
                (Some(n), RETENTION)
            } else if let Some(rest) = name.strip_prefix(".staging-") {
                (rest.split('-').next().and_then(|n| n.parse().ok()), STAGING_RETENTION)
            } else {
                (None, RETENTION)
            };
            let Some(timestamp) = timestamp else { continue };
            if keep.contains(&name) || now.saturating_sub(timestamp) <= max_age.as_millis() {
                continue;
            }
            if let Err(error) = std::fs::remove_dir_all(entry.path()) {
                tracing::warn!(dir = %dir.display(), name, %error, "failed to remove stale npm generation");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_specs() {
        assert_eq!(parse("pkg"), Some(Target::Registry { name: "pkg".into(), spec: "latest".into(), mutable: true }));
        assert_eq!(parse("pkg@1.2.3"), Some(Target::Registry { name: "pkg".into(), spec: "1.2.3".into(), mutable: false }));
        assert_eq!(parse("pkg@^1.2"), Some(Target::Registry { name: "pkg".into(), spec: "^1.2".into(), mutable: true }));
        assert_eq!(parse("@scope/pkg@beta"), Some(Target::Registry { name: "@scope/pkg".into(), spec: "beta".into(), mutable: true }));
        assert_eq!(parse("@scope/pkg"), Some(Target::Registry { name: "@scope/pkg".into(), spec: "latest".into(), mutable: true }));
        assert!(matches!(parse("git+https://github.com/a/b.git"), Some(Target::Git { name: None, ref slug, mutable: true }) if slug == "b"));
        assert!(matches!(parse("github:a/b#0123456789abcdef0123456789abcdef01234567"), Some(Target::Git { mutable: false, .. })));
        assert!(matches!(parse("a/b"), Some(Target::Git { .. })));
        assert!(matches!(parse("x@git+https://h/a/repo.git"), Some(Target::Git { name: Some(ref n), .. }) if n == "x"));
        assert_eq!(parse("file:../x"), None);
        assert_eq!(parse("Bad Name"), None);
        assert_eq!(parse("x@npm:y"), None);
    }

    #[test]
    fn cache_keys() {
        assert_eq!(cache_key("pkg"), "pkg@latest");
        assert_eq!(cache_key("@s/p@1.0.0"), "@s/p@1.0.0");
        let git = cache_key("git+https://github.com/a/my%20repo.git#main");
        assert!(git.starts_with("git-my-repo-"), "{git}");
        assert_eq!(git.len(), "git-my-repo-".len() + 12);
    }

    #[test]
    fn generations_and_collect() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        for g in ["1", "2", "3", "notes"] {
            std::fs::create_dir(root.join(g)).unwrap();
        }
        std::fs::create_dir(root.join(".staging-5-abc")).unwrap();
        assert_eq!(Npm::generations(root), vec![1, 2, 3]);
        assert_eq!(Npm::current(root), Some(root.join("3")));
        Npm::collect(root);
        let mut left: Vec<String> = std::fs::read_dir(root).unwrap().flatten().map(|e| e.file_name().into_string().unwrap()).collect();
        left.sort();
        assert_eq!(left, ["2", "3", "notes"]);
    }

    #[test]
    fn resolve_reads_layout() {
        let dir = tempfile::tempdir().unwrap();
        let npm = Npm::new(dir.path(), dir.path().join("locks"));
        let (missing, exists) = npm.resolve("pkg@1.0.0");
        assert!(!exists);
        assert!(missing.directory.ends_with("npm/pkg@1.0.0/node_modules/pkg"));
        let package = dir.path().join("npm/pkg@1.0.0/17/node_modules/pkg");
        std::fs::create_dir_all(&package).unwrap();
        std::fs::write(package.join("package.json"), r#"{"name":"pkg","version":"1.0.0"}"#).unwrap();
        let (found, exists) = npm.resolve("pkg@1.0.0");
        assert!(exists);
        assert_eq!(found.version.as_deref(), Some("1.0.0"));
        assert_eq!(found.revision.as_deref(), Some("1.0.0"));
    }
}

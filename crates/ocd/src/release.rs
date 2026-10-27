//! ocd's own releases on GitHub: `ocd upgrade` and the update notice (upstream `services/updater.ts`,
//! `updater-action.ts`).
//!
//! Upstream upgrades through its install method (the curl installer, npm, Homebrew or WinGet) and asks
//! `opencode.ai/update` for the latest version. ocd ships only as release archives on GitHub
//! (`.github/workflows/release.yml`), so it looks the release up with the GitHub API, downloads the archive for
//! this platform, checks it against the release's `SHA256SUMS`, runs the new binary once (`--version`, in empty
//! directories) and then replaces its own executable:
//!
//! - Unix: the new binary is written next to the real path of the executable with the same permissions and
//!   renamed over it. A running process keeps the old inode, so a running TUI or service is not disturbed.
//! - Windows: a running executable cannot be replaced, but it can be renamed, so `ocd.exe` becomes `ocd.exe.old`
//!   and the new file takes its name. The `.old` file is removed on the next upgrade or start.
//!
//! The update check behind the notice runs at most once a day (the result is kept in
//! `<cache>/ocd-update.json`) and follows upstream's policy: `OPENCODE_DISABLE_AUTOUPDATE`, then the `update` /
//! `autoupdate` setting of the global config (default `notify`; `auto` installs, but unlike `ocd upgrade` never
//! restarts the background service). A local build (one not made from a release tag) never checks, as upstream's
//! `local` channel doesn't, and only a newer release counts (upstream offers any other version).

use std::path::{Path, PathBuf};
use std::time::Duration;

use oc_config::schema::UpdatePolicy;
use oc_util::Global;
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// The GitHub API base of ocd's repository.
pub const API: &str = "https://api.github.com/repos/sphinxid/ocd";
/// The releases page, for manual downloads.
pub const RELEASES: &str = "https://github.com/sphinxid/ocd/releases";
/// Test only: a GitHub API base (`…/repos/<owner>/<repo>`) to use instead of [`API`]. The GitHub token is never
/// sent to it.
pub const API_OVERRIDE: &str = "OCD_TEST_RELEASES_API";
/// Test only, honoured only together with [`API_OVERRIDE`]: the ocd build to report instead of [`crate::BUILD`]
/// (empty: no build version).
pub const BUILD_OVERRIDE: &str = "OCD_TEST_BUILD_VERSION";

/// How long a successful check is reused.
pub const CHECK_INTERVAL: Duration = Duration::from_secs(24 * 60 * 60);
/// How long a failed check waits before trying again.
pub const RETRY_INTERVAL: Duration = Duration::from_secs(60 * 60);

/// Where releases come from and what is installed: [`API`] and [`crate::BUILD`], or the test overrides.
#[derive(Clone, Debug)]
pub struct Source {
    /// The GitHub API base of the repository.
    pub api: String,
    /// Sent as a bearer token (`GITHUB_TOKEN` / `GH_TOKEN`, real API only).
    pub token: Option<String>,
    pub installed: Installed,
    /// `OPENCODE_DISABLE_AUTOUPDATE` is set: no background check.
    pub disabled: bool,
    /// The executable to replace (`None`: the running one).
    pub exe: Option<PathBuf>,
}

impl Source {
    pub fn from_env() -> Source {
        let disabled = disabled_by_env();
        let api = std::env::var(API_OVERRIDE).ok().filter(|v| !v.is_empty());
        let installed = match (&api, std::env::var(BUILD_OVERRIDE)) {
            (Some(_), Ok(build)) => Installed::from_build(Some(&build)),
            _ => Installed::from_build(crate::BUILD),
        };
        let token = match api {
            Some(_) => None,
            None => ["GITHUB_TOKEN", "GH_TOKEN"].iter().find_map(|k| std::env::var(k).ok().filter(|v| !v.trim().is_empty())),
        };
        let api = api.unwrap_or_else(|| API.to_string()).trim_end_matches('/').to_string();
        Source { api, token, installed, disabled, exe: None }
    }

    /// The executable an upgrade replaces.
    pub fn executable(&self) -> Result<PathBuf, UpgradeError> {
        match &self.exe {
            Some(p) => Ok(p.clone()),
            None => executable(),
        }
    }
}

// ---------------------------------------------------------------------------------------------
// versions

/// A release version (`parseReleaseVersion`): `[v]MAJOR.MINOR.PATCH[-PRERELEASE][+BUILD]`, without leading zeros.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Version {
    pub major: u64,
    pub minor: u64,
    pub patch: u64,
    pub pre: Vec<String>,
}

fn identifier(s: &str) -> bool {
    !s.is_empty() && s.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
}

fn numeric(s: &str) -> bool {
    !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit())
}

impl Version {
    pub fn parse(input: &str) -> Option<Version> {
        if input.len() > 256 {
            return None;
        }
        let s = input.trim();
        let s = s.strip_prefix('v').unwrap_or(s);
        let (s, build) = match s.split_once('+') {
            Some((a, b)) => (a, Some(b)),
            None => (s, None),
        };
        if build.is_some_and(|b| !b.split('.').all(identifier)) {
            return None;
        }
        let (core, pre) = match s.split_once('-') {
            Some((a, b)) => (a, Some(b)),
            None => (s, None),
        };
        let parts: Vec<&str> = core.split('.').collect();
        if parts.len() != 3 || parts.iter().any(|p| !numeric(p) || (p.len() > 1 && p.starts_with('0'))) {
            return None;
        }
        let pre: Vec<String> = match pre {
            None => vec![],
            Some(p) => {
                let ids: Vec<&str> = p.split('.').collect();
                if ids.iter().any(|i| !identifier(i) || (i.len() > 1 && i.starts_with('0') && numeric(i))) {
                    return None;
                }
                ids.into_iter().map(String::from).collect()
            }
        };
        Some(Version { major: parts[0].parse().ok()?, minor: parts[1].parse().ok()?, patch: parts[2].parse().ok()?, pre })
    }
}

impl std::fmt::Display for Version {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}.{}.{}", self.major, self.minor, self.patch)?;
        if !self.pre.is_empty() {
            write!(f, "-{}", self.pre.join("."))?;
        }
        Ok(())
    }
}

impl PartialOrd for Version {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

/// Semver precedence: a pre-release sorts before its release; identifiers compare numerically when both are
/// numbers, numbers before words, and a shorter list first.
impl Ord for Version {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        use std::cmp::Ordering;
        let core = (self.major, self.minor, self.patch).cmp(&(other.major, other.minor, other.patch));
        if core != Ordering::Equal {
            return core;
        }
        match (self.pre.is_empty(), other.pre.is_empty()) {
            (true, true) => return Ordering::Equal,
            (true, false) => return Ordering::Greater,
            (false, true) => return Ordering::Less,
            _ => {}
        }
        for (a, b) in self.pre.iter().zip(&other.pre) {
            let ord = match (numeric(a), numeric(b)) {
                (true, true) => {
                    let (a, b) = (a.trim_start_matches('0'), b.trim_start_matches('0'));
                    a.len().cmp(&b.len()).then_with(|| a.cmp(b))
                }
                (true, false) => Ordering::Less,
                (false, true) => Ordering::Greater,
                (false, false) => a.cmp(b),
            };
            if ord != Ordering::Equal {
                return ord;
            }
        }
        self.pre.len().cmp(&other.pre.len())
    }
}

/// What this binary is.
#[derive(Clone, Debug, PartialEq)]
pub enum Installed {
    /// Built from a release tag (`0.2.0+abc1234`), possibly with commits after it (`0.2.0+3.abc1234`, which
    /// counts as 0.2.0).
    Release { version: Version, build: String },
    /// Built without a release tag (`20261027.124153+abc1234`), or without any build version.
    Local { build: Option<String> },
}

impl Installed {
    pub fn from_build(build: Option<&str>) -> Installed {
        let Some(build) = build.filter(|b| !b.is_empty()) else { return Installed::Local { build: None } };
        let release = build.split('+').next().unwrap_or_default();
        match Version::parse(release) {
            Some(version) => Installed::Release { version, build: build.to_string() },
            None => Installed::Local { build: Some(build.to_string()) },
        }
    }

    /// The build as `ocd --version` shows it.
    pub fn build(&self) -> Option<&str> {
        match self {
            Installed::Release { build, .. } => Some(build),
            Installed::Local { build } => build.as_deref(),
        }
    }
}

// ---------------------------------------------------------------------------------------------
// errors

/// `Updater.UpgradeError`: what failed, why, and what to do next.
#[derive(Clone, Debug, PartialEq)]
pub struct UpgradeError {
    pub title: String,
    pub detail: String,
    pub command: Option<String>,
    pub retry: String,
}

impl UpgradeError {
    fn new(title: impl Into<String>, detail: impl Into<String>, retry: impl Into<String>) -> UpgradeError {
        UpgradeError { title: title.into(), detail: detail.into(), command: None, retry: retry.into() }
    }
}

/// `wrap(value, width)` of `handlers/upgrade.ts`.
fn wrap(value: &str, width: usize) -> Vec<String> {
    value
        .split('\n')
        .flat_map(|source| {
            if source.chars().count() <= width {
                return vec![source.to_string()];
            }
            let mut out: Vec<String> = vec![];
            for word in source.split_whitespace() {
                match out.last_mut() {
                    Some(last) if last.chars().count() + word.chars().count() < width => {
                        last.push(' ');
                        last.push_str(word);
                    }
                    _ => out.push(word.to_string()),
                }
            }
            out
        })
        .collect()
}

/// `formatUpgradeError`, for a terminal `columns` wide.
pub fn format_error(e: &UpgradeError, columns: usize) -> String {
    let clean = |v: &str| crate::cli::help::strip_ansi(v).replace('\r', "").trim().to_string();
    let line = |v: &str| clean(v).split_whitespace().collect::<Vec<_>>().join(" ");
    let field = |label: &str, value: &str| {
        let prefix = format!("{label:<9}");
        let width = 24usize.max(61usize.min(columns.saturating_sub(prefix.len() + 10)));
        wrap(value, width)
            .iter()
            .enumerate()
            .map(|(i, l)| if i == 0 { format!("{prefix}{l}") } else { format!("{}{l}", " ".repeat(prefix.len())) })
            .collect::<Vec<_>>()
            .join("\n")
    };
    let detail = wrap(&clean(&e.detail), 24usize.max(columns.saturating_sub(7))).join("\n");
    let mut out = vec![line(&e.title), String::new(), detail];
    if let Some(c) = &e.command {
        out.push(String::new());
        out.push(field("Command", &line(c)));
    }
    out.push(field("Retry", &line(&e.retry)));
    out.join("\n")
}

impl std::fmt::Display for UpgradeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&format_error(self, crate::term::columns().unwrap_or(80)))
    }
}

impl std::error::Error for UpgradeError {}

/// `errorDetail`: the error and its causes.
fn error_detail(e: &(dyn std::error::Error + 'static)) -> String {
    let mut parts = vec![e.to_string()];
    let mut source = e.source();
    while let Some(s) = source {
        let text = s.to_string();
        if !parts.iter().any(|p| p.contains(&text)) {
            parts.push(text);
        }
        source = s.source();
    }
    parts.join(": ")
}

// ---------------------------------------------------------------------------------------------
// GitHub

#[derive(Clone, Debug, PartialEq)]
pub struct Asset {
    pub name: String,
    pub url: String,
    /// GitHub's `digest` (`sha256:<hex>`), when it reports one.
    pub digest: Option<String>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Release {
    pub tag: String,
    pub version: Version,
    /// The release page (`html_url`).
    pub page: String,
    pub assets: Vec<Asset>,
}

impl Release {
    pub fn asset(&self, name: &str) -> Option<&Asset> {
        self.assets.iter().find(|a| a.name == name)
    }
}

fn user_agent() -> String {
    format!("ocd/{} (opencode {})", crate::BUILD.unwrap_or("local"), oc_util::version::VERSION)
}

fn client() -> reqwest::Client {
    oc_util::http::client_builder()
        .user_agent(user_agent())
        .connect_timeout(Duration::from_secs(10))
        .build()
        .unwrap_or_default()
}

/// The tag a `target` names (`1.2.3` or `v1.2.3`): `v1.2.3`.
pub fn tag_of(target: &str) -> String {
    format!("v{}", target.trim().trim_start_matches('v'))
}

const RETRY_NETWORK: &str = "Check your network, then run ocd upgrade again.";
const RETRY_LATER: &str = "Try again in a few minutes.";
const CHECK_TITLE: &str = "Could not check for ocd updates";

/// The latest release, or the release tagged `target`.
pub async fn fetch(source: &Source, target: Option<&str>, timeout: Duration) -> Result<Release, UpgradeError> {
    let base = &source.api;
    let url = match target {
        None => format!("{base}/releases/latest"),
        Some(t) => format!("{base}/releases/tags/{}", tag_of(t)),
    };
    let mut req = client()
        .get(&url)
        .header("accept", "application/vnd.github+json")
        .header("x-github-api-version", "2022-11-28")
        .timeout(timeout);
    if let Some(t) = &source.token {
        req = req.header("authorization", format!("Bearer {t}"));
    }
    let res = req.send().await.map_err(|e| UpgradeError::new(CHECK_TITLE, error_detail(&e), RETRY_NETWORK))?;
    let status = res.status();
    if !status.is_success() {
        let header = |k: &str| res.headers().get(k).and_then(|v| v.to_str().ok()).map(String::from);
        let limited = status.as_u16() == 429 || (status.as_u16() == 403 && header("x-ratelimit-remaining").as_deref() == Some("0"));
        if limited {
            let reset = header("x-ratelimit-reset")
                .and_then(|r| r.parse::<i64>().ok())
                .and_then(|r| chrono::DateTime::from_timestamp(r, 0))
                .map(|t| format!(" until {}", t.with_timezone(&chrono::Local).format("%H:%M")))
                .unwrap_or_default();
            return Err(UpgradeError::new(
                "GitHub API rate limit reached",
                format!("GitHub refuses more release lookups from this address{reset} (HTTP {}).", status.as_u16()),
                "Wait for the limit to reset, or set GITHUB_TOKEN to a GitHub token, then run ocd upgrade again.",
            ));
        }
        if status.as_u16() == 404 {
            return Err(match target {
                None => UpgradeError::new(
                    "No ocd release found",
                    "sphinxid/ocd has no published release yet.",
                    format!("Check {RELEASES}, or build ocd from source."),
                ),
                Some(t) => UpgradeError::new(
                    format!("ocd {} was not found", tag_of(t)),
                    format!("There is no release tagged {} in sphinxid/ocd.", tag_of(t)),
                    format!("Check the versions at {RELEASES}, then run ocd upgrade again."),
                ),
            });
        }
        return Err(UpgradeError::new(CHECK_TITLE, format!("The GitHub API returned HTTP {}.", status.as_u16()), RETRY_LATER));
    }
    let read_error = |detail: String| UpgradeError::new("Could not read the ocd release information", detail, RETRY_LATER);
    let body: Value = res.json().await.map_err(|e| read_error(error_detail(&e)))?;
    parse_release(&body).map_err(read_error)
}

/// The fields of a GitHub release object that the upgrade uses.
pub fn parse_release(body: &Value) -> Result<Release, String> {
    let s = |v: &Value, k: &str| v.get(k).and_then(Value::as_str).map(String::from);
    let tag = s(body, "tag_name").ok_or("The release has no tag.")?;
    let version = Version::parse(&tag).ok_or_else(|| format!("The release tag {tag} is not a version."))?;
    let assets = body
        .get("assets")
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(|x| Some(Asset { name: s(x, "name")?, url: s(x, "browser_download_url")?, digest: s(x, "digest") }))
                .collect()
        })
        .unwrap_or_default();
    Ok(Release { page: s(body, "html_url").unwrap_or_else(|| format!("{RELEASES}/tag/{tag}")), tag, version, assets })
}

// ---------------------------------------------------------------------------------------------
// platform and archive

/// The release platform this binary was built for. `OCD_RELEASE_PLATFORM` at build time overrides it, so a
/// variant build (`release-glibc2.17.yml`: `linux-x64-glibc2.17`) upgrades to the same variant.
pub const PLATFORM: Option<&str> = match option_env!("OCD_RELEASE_PLATFORM") {
    Some(p) if !p.is_empty() => Some(p),
    _ => {
        if cfg!(all(target_os = "linux", target_arch = "x86_64")) {
            Some("linux-x64")
        } else if cfg!(all(target_os = "linux", target_arch = "aarch64")) {
            Some("linux-arm64")
        } else if cfg!(all(target_os = "macos", target_arch = "aarch64")) {
            Some("darwin-arm64")
        } else if cfg!(all(target_os = "windows", target_arch = "x86_64")) {
            Some("windows-x64")
        } else {
            None
        }
    }
};

pub fn unsupported_platform() -> UpgradeError {
    UpgradeError::new(
        "No ocd release for this platform",
        format!(
            "ocd releases are built for Linux (x86_64, ARM64), macOS on Apple silicon and Windows (x86_64); this is {} {}.",
            std::env::consts::OS,
            std::env::consts::ARCH
        ),
        "Build ocd from source: cargo build --release -p ocd",
    )
}

/// `ocd-<tag>-<platform>.tar.gz` (`.zip` on Windows).
pub fn archive_name(tag: &str, platform: &str) -> String {
    let ext = if platform.starts_with("windows") { "zip" } else { "tar.gz" };
    format!("ocd-{tag}-{platform}.{ext}")
}

/// The binary inside the archive: `ocd-<tag>-<platform>/ocd` (`ocd.exe` on Windows).
pub fn binary_entry(tag: &str, platform: &str) -> String {
    let exe = if platform.starts_with("windows") { "ocd.exe" } else { "ocd" };
    format!("ocd-{tag}-{platform}/{exe}")
}

/// The checksum `SHA256SUMS` (`sha256sum` output) lists for `name`.
pub fn checksum_for(sums: &str, name: &str) -> Option<String> {
    sums.lines().find_map(|line| {
        let (hash, file) = line.trim().split_once(char::is_whitespace)?;
        let file = file.trim_start();
        let file = file.strip_prefix('*').unwrap_or(file);
        (file == name && hash.len() == 64 && hash.bytes().all(|b| b.is_ascii_hexdigit())).then(|| hash.to_ascii_lowercase())
    })
}

pub fn sha256_hex(bytes: &[u8]) -> String {
    use sha2::Digest;
    hex::encode(sha2::Sha256::digest(bytes))
}

/// The executable from a release archive (in memory).
pub fn extract(archive: &[u8], tag: &str, platform: &str) -> Result<Vec<u8>, String> {
    use std::io::Read;
    let entry = binary_entry(tag, platform);
    let found = if platform.starts_with("windows") {
        oc_plugin::zip::extract(archive, &entry)?
    } else {
        let mut tar = Vec::new();
        flate2::read::GzDecoder::new(archive).read_to_end(&mut tar).map_err(|e| format!("not a gzip archive: {e}"))?;
        let entries = oc_persistent_pty::binary::tar_entries(&tar)?;
        entries.into_iter().find(|(name, _)| name.strip_prefix("./").unwrap_or(name) == entry).map(|(_, data)| data.to_vec())
    };
    match found {
        Some(b) if !b.is_empty() => Ok(b),
        Some(_) => Err(format!("{entry} is empty")),
        None => Err(format!("{entry} is missing from the archive")),
    }
}

async fn get_bytes(url: &str, what: &str, version: &Version) -> Result<Vec<u8>, UpgradeError> {
    let failed = |detail: String| UpgradeError::new(format!("Could not download ocd {version}"), detail, RETRY_NETWORK);
    let res = client().get(url).timeout(Duration::from_secs(10 * 60)).send().await.map_err(|e| failed(error_detail(&e)))?;
    if !res.status().is_success() {
        return Err(failed(format!("Downloading {what} returned HTTP {}.", res.status().as_u16())));
    }
    res.bytes().await.map(|b| b.to_vec()).map_err(|e| failed(error_detail(&e)))
}

/// Downloads this platform's archive and `SHA256SUMS`, checks the archive and returns the executable in it.
pub async fn download(release: &Release, platform: &str) -> Result<Vec<u8>, UpgradeError> {
    let v = &release.version;
    let name = archive_name(&release.tag, platform);
    let manual = format!("Download it from {} and replace the binary yourself, or build ocd from source.", release.page);
    let asset = release.asset(&name).ok_or_else(|| {
        UpgradeError::new(format!("ocd {v} has no download for {platform}"), format!("The release has no {name}."), manual.clone())
    })?;
    let unverified = |detail: String| UpgradeError::new(format!("ocd {v} cannot be verified"), detail, manual.clone());
    let sums = release.asset("SHA256SUMS").ok_or_else(|| unverified("The release has no SHA256SUMS file.".into()))?;
    let sums = get_bytes(&sums.url, "SHA256SUMS", v).await?;
    let expected = checksum_for(&String::from_utf8_lossy(&sums), &name)
        .ok_or_else(|| unverified(format!("SHA256SUMS has no entry for {name}.")))?;
    let archive = get_bytes(&asset.url, &name, v).await?;
    let actual = sha256_hex(&archive);
    let digest = asset.digest.as_deref().and_then(|d| d.strip_prefix("sha256:")).map(str::to_ascii_lowercase);
    for want in std::iter::once(&expected).chain(digest.as_ref()) {
        if *want != actual {
            return Err(UpgradeError::new(
                format!("Checksum mismatch for {name}"),
                format!("Expected sha256 {want}, but the download has {actual}. Nothing was installed."),
                "Run ocd upgrade again. If it keeps failing, report it at https://github.com/sphinxid/ocd/issues.",
            ));
        }
    }
    extract(&archive, &release.tag, platform)
        .map_err(|e| UpgradeError::new(format!("Could not unpack {name}"), e, "Run ocd upgrade again."))
}

// ---------------------------------------------------------------------------------------------
// replacing the executable

/// The real path of the running executable.
pub fn executable() -> Result<PathBuf, UpgradeError> {
    let exe = std::env::current_exe()
        .map_err(|e| UpgradeError::new("Could not find the ocd executable", e.to_string(), "Replace the binary yourself."))?;
    Ok(std::fs::canonicalize(&exe).unwrap_or(exe))
}

fn file_name(exe: &Path) -> String {
    exe.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| "ocd".into())
}

/// Fails unless the directory of `exe` can take a new file (checked before downloading anything).
pub fn check_writable(exe: &Path, page: &str) -> Result<(), UpgradeError> {
    let dir = exe.parent().unwrap_or(Path::new("."));
    let probe = dir.join(format!(".{}.upgrade-probe-{}", file_name(exe), std::process::id()));
    match std::fs::OpenOptions::new().write(true).create_new(true).open(&probe) {
        Ok(_) => {
            let _ = std::fs::remove_file(&probe);
            Ok(())
        }
        Err(e) => Err(UpgradeError::new(
            format!("Cannot replace {}", exe.display()),
            format!("{} is not writable by this user ({e}).", dir.display()),
            if cfg!(windows) {
                format!("Run ocd upgrade from an administrator terminal, or download the release from {page} and replace the binary yourself.")
            } else {
                format!("Run sudo ocd upgrade, or download the release from {page} and replace the binary yourself.")
            },
        )),
    }
}

/// Where a Windows upgrade moves the running executable.
#[cfg_attr(not(windows), allow(dead_code))]
pub fn old_path(exe: &Path) -> PathBuf {
    exe.with_file_name(format!("{}.old", file_name(exe)))
}

/// Removes executables that earlier Windows upgrades moved aside (`ocd.exe.old`, `ocd.exe.old-<pid>`). They
/// stay while a process still runs from them. Does nothing elsewhere.
pub fn cleanup_old() {
    if !cfg!(windows) {
        return;
    }
    let Ok(exe) = executable() else { return };
    let (Some(dir), prefix) = (exe.parent(), format!("{}.old", file_name(&exe))) else { return };
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    for e in entries.flatten() {
        let name = e.file_name().to_string_lossy().into_owned();
        if name == prefix || name.starts_with(&format!("{prefix}-")) {
            let _ = std::fs::remove_file(e.path());
        }
    }
}

/// The staging file next to `exe`.
fn staging_path(exe: &Path) -> PathBuf {
    let pid = std::process::id();
    if cfg!(windows) {
        let stem = exe.file_stem().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| "ocd".into());
        exe.with_file_name(format!("{stem}.upgrade-{pid}.exe"))
    } else {
        exe.with_file_name(format!(".{}.upgrade-{pid}", file_name(exe)))
    }
}

/// Removes staged binaries an interrupted upgrade left next to `exe` (older than a day, so never one that another
/// upgrade is still checking).
fn remove_stale_staging(exe: &Path) {
    let (Some(dir), Some(staged)) = (exe.parent(), staging_path(exe).file_name().map(|n| n.to_string_lossy().into_owned())) else {
        return;
    };
    let prefix = staged.trim_end_matches(".exe").trim_end_matches(|c: char| c.is_ascii_digit()).to_string();
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    for e in entries.flatten() {
        let name = e.file_name().to_string_lossy().into_owned();
        let old = e.metadata().and_then(|m| m.modified()).ok().and_then(|t| t.elapsed().ok()).is_some_and(|age| age > CHECK_INTERVAL);
        if name.starts_with(&prefix) && old {
            let _ = std::fs::remove_file(e.path());
        }
    }
}

/// Runs `bin --version` with empty home, config, data, state and cache directories and checks that it reports
/// `version`.
async fn sanity_check(bin: &Path, version: &Version) -> Result<(), String> {
    let root = std::env::temp_dir().join(format!("ocd-upgrade-check-{}-{:08x}", std::process::id(), rand::random::<u32>()));
    let result = async {
        for d in ["home", "config", "data", "state", "cache"] {
            std::fs::create_dir_all(root.join(d)).map_err(|e| e.to_string())?;
        }
        let mut cmd = tokio::process::Command::new(bin);
        cmd.arg("--version")
            .env("HOME", root.join("home"))
            .env("USERPROFILE", root.join("home"))
            .env("OPENCODE_TEST_HOME", root.join("home"))
            .env("XDG_CONFIG_HOME", root.join("config"))
            .env("XDG_DATA_HOME", root.join("data"))
            .env("XDG_STATE_HOME", root.join("state"))
            .env("XDG_CACHE_HOME", root.join("cache"))
            .env("OPENCODE_DISABLE_AUTOUPDATE", "1")
            .env("NO_COLOR", "1")
            .env_remove("OPENCODE_CONFIG_DIR")
            .env_remove("OPENCODE_CONFIG")
            .env_remove("OPENCODE_CONFIG_CONTENT")
            .stdin(std::process::Stdio::null())
            .kill_on_drop(true);
        let out = tokio::time::timeout(Duration::from_secs(30), cmd.output())
            .await
            .map_err(|_| "ocd --version did not finish within 30 seconds".to_string())?
            .map_err(|e| format!("could not run it: {e}"))?;
        let stdout = crate::cli::help::strip_ansi(&String::from_utf8_lossy(&out.stdout));
        if !out.status.success() {
            let stderr = String::from_utf8_lossy(&out.stderr);
            return Err(format!("ocd --version exited with {}: {}", out.status, stderr.trim()));
        }
        if !stdout.contains(&format!("ocd v{version}+")) {
            return Err(format!("ocd --version printed {:?}, not version {version}", stdout.trim()));
        }
        Ok(())
    }
    .await;
    let _ = std::fs::remove_dir_all(&root);
    result
}

#[cfg(not(windows))]
fn swap(staged: &Path, exe: &Path) -> std::io::Result<()> {
    std::fs::rename(staged, exe)
}

#[cfg(windows)]
fn swap(staged: &Path, exe: &Path) -> std::io::Result<()> {
    let mut old = old_path(exe);
    if old.exists() && std::fs::remove_file(&old).is_err() {
        // Still in use by a process started from it before the last upgrade.
        old = exe.with_file_name(format!("{}.old-{}", file_name(exe), std::process::id()));
    }
    std::fs::rename(exe, &old)?;
    if let Err(e) = std::fs::rename(staged, exe) {
        let _ = std::fs::rename(&old, exe);
        return Err(e);
    }
    Ok(())
}

/// Writes `binary` next to `exe` (same permissions), checks that it runs and reports `version`, then puts it in
/// place of `exe`.
pub async fn replace(exe: &Path, binary: &[u8], version: &Version) -> Result<(), UpgradeError> {
    let staged = staging_path(exe);
    let retry = "Run ocd upgrade again.";
    let write = || -> std::io::Result<()> {
        std::fs::write(&staged, binary)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(exe).map(|m| m.permissions().mode()).unwrap_or(0o755) | 0o100;
            std::fs::set_permissions(&staged, std::fs::Permissions::from_mode(mode & 0o7777))?;
        }
        Ok(())
    };
    if let Err(e) = write() {
        let _ = std::fs::remove_file(&staged);
        return Err(UpgradeError::new(format!("Could not write {}", staged.display()), e.to_string(), retry));
    }
    if let Err(e) = sanity_check(&staged, version).await {
        let _ = std::fs::remove_file(&staged);
        return Err(UpgradeError::new(
            format!("The downloaded ocd {version} does not run on this system"),
            format!("{e}. Nothing was replaced."),
            format!("Download a build from {RELEASES}, or build ocd from source."),
        ));
    }
    if let Err(e) = swap(&staged, exe) {
        let _ = std::fs::remove_file(&staged);
        return Err(UpgradeError::new(format!("Could not replace {}", exe.display()), e.to_string(), retry));
    }
    Ok(())
}

/// The whole install: one upgrade at a time (`cli-upgrade` lock), download, verify, check and replace `exe`.
pub async fn install(global: &Global, release: &Release, platform: &str, exe: &Path) -> Result<(), UpgradeError> {
    let opts = oc_util::flock::Options { timeout: Duration::from_secs(15 * 60), ..Default::default() };
    let lease = oc_util::flock::acquire(&global.locks(), "cli-upgrade", opts).await.map_err(|e| {
        UpgradeError::new("Could not prepare the ocd upgrade", e.to_string(), "Fix the issue above, then run ocd upgrade again.")
    })?;
    let result = async {
        check_writable(exe, &release.page)?;
        remove_stale_staging(exe);
        let binary = download(release, platform).await?;
        replace(exe, &binary, &release.version).await
    }
    .await;
    let _ = lease.release();
    result
}

// ---------------------------------------------------------------------------------------------
// update check (`updater.run`, `check`, `apply`)

/// `OPENCODE_DISABLE_AUTOUPDATE` is `1` or `true`.
pub fn disabled_by_env() -> bool {
    std::env::var("OPENCODE_DISABLE_AUTOUPDATE").is_ok_and(|v| matches!(v.to_ascii_lowercase().as_str(), "1" | "true"))
}

/// `<cache>/ocd-update.json`: when the latest release was last looked up and what it was.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct CheckCache {
    /// Milliseconds since the epoch.
    pub checked: i64,
    /// The latest release tag.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub latest: Option<String>,
    /// The last lookup failed (tried again after [`RETRY_INTERVAL`]).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub failed: bool,
}

pub fn cache_file(global: &Global) -> PathBuf {
    global.cache.join("ocd-update.json")
}

fn now_ms() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

impl CheckCache {
    pub fn read(file: &Path) -> Option<CheckCache> {
        serde_json::from_str(&std::fs::read_to_string(file).ok()?).ok()
    }

    fn write(&self, file: &Path) {
        if let Some(dir) = file.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let tmp = file.with_extension(format!("json.{}.tmp", std::process::id()));
        if std::fs::write(&tmp, serde_json::to_string(self).unwrap_or_default()).is_ok() {
            let _ = std::fs::rename(&tmp, file);
        }
    }

    /// Whether a lookup at `now` (ms) can reuse this result.
    pub fn fresh(&self, now: i64) -> bool {
        let age = now - self.checked;
        let limit = if self.failed { RETRY_INTERVAL } else { CHECK_INTERVAL };
        age >= 0 && (age as u128) < limit.as_millis()
    }
}

/// The latest release version, looked up at most once a day.
pub async fn latest_cached(global: &Global, source: &Source) -> Option<Version> {
    let file = cache_file(global);
    let cached = CheckCache::read(&file);
    let now = now_ms();
    if let Some(c) = cached.as_ref().filter(|c| c.fresh(now)) {
        return c.latest.as_deref().and_then(Version::parse);
    }
    match fetch(source, None, Duration::from_secs(10)).await {
        Ok(release) => {
            CheckCache { checked: now, latest: Some(release.tag), failed: false }.write(&file);
            Some(release.version)
        }
        Err(e) => {
            tracing::warn!(error = %e.title, detail = %e.detail, "update check failed");
            let latest = cached.and_then(|c| c.latest);
            CheckCache { checked: now, latest: latest.clone(), failed: true }.write(&file);
            latest.as_deref().and_then(Version::parse)
        }
    }
}

/// An update the check found.
#[derive(Clone, Debug, PartialEq)]
pub struct Notice {
    pub policy: UpdatePolicy,
    pub version: Version,
}

/// `inspect()`: is a newer release out, and what does the policy say to do with it?
pub async fn inspect(global: &Global, source: &Source) -> Option<Notice> {
    if source.disabled {
        tracing::info!(reason = "disabled", "update check skipped");
        return None;
    }
    let Installed::Release { version: current, .. } = &source.installed else {
        tracing::info!(reason = "local-build", "update check skipped");
        return None;
    };
    let policy = oc_config::policy::read_update_policy(&global.config);
    if policy == UpdatePolicy::Disable {
        tracing::info!(reason = "policy-disabled", "update check skipped");
        return None;
    }
    let latest = latest_cached(global, source).await?;
    tracing::info!(current = %current, latest = %latest, "update check");
    if latest <= *current {
        return None;
    }
    tracing::info!(current = %current, latest = %latest, policy = policy.as_str(), "ocd update available");
    Some(Notice { policy, version: latest })
}

/// `RunResult` / `CheckResult` of upstream's updater.
#[derive(Clone, Debug, PartialEq)]
pub enum Outcome {
    Available(String),
    Installed(String),
    Unavailable(String),
}

/// `updater.run(onInstall)`: notifies about a newer release, or installs it when the policy is `auto`. Never
/// fails; problems are logged.
pub async fn run(global: &Global, source: &Source, on_install: impl FnOnce(&str)) -> Option<Outcome> {
    let notice = inspect(global, source).await?;
    let version = notice.version.to_string();
    if notice.policy != UpdatePolicy::Auto {
        return Some(Outcome::Available(version));
    }
    on_install(&version);
    match apply(global, source, &version).await {
        Ok(()) => Some(Outcome::Installed(version)),
        Err(e) => {
            tracing::warn!(error = %e.title, detail = %e.detail, "update failed");
            None
        }
    }
}

pub const LOCAL_BUILD: &str = "This ocd is a local build, not a release. Run ocd upgrade to replace it with the latest release.";

/// `updater.check()` (the TUI's `/update`): looks the latest release up now.
pub async fn check(source: &Source) -> Result<Option<Outcome>, UpgradeError> {
    let Installed::Release { version: current, .. } = &source.installed else {
        return Ok(Some(Outcome::Unavailable(LOCAL_BUILD.into())));
    };
    if PLATFORM.is_none() {
        return Ok(Some(Outcome::Unavailable(unsupported_platform().detail)));
    }
    let release = fetch(source, None, Duration::from_secs(10)).await?;
    Ok((release.version > *current).then(|| Outcome::Available(release.version.to_string())))
}

/// `updater.apply(version)`: installs `version` over this executable. The background service is left alone (it
/// may be running this client's session); it uses the new version once restarted.
pub async fn apply(global: &Global, source: &Source, version: &str) -> Result<(), UpgradeError> {
    let platform = PLATFORM.ok_or_else(unsupported_platform)?;
    let exe = source.executable()?;
    let release = fetch(source, Some(version), Duration::from_secs(10)).await?;
    install(global, &release, platform, &exe).await?;
    tracing::info!(to = %release.version, "updated ocd");
    Ok(())
}

#[cfg(test)]
mod tests;

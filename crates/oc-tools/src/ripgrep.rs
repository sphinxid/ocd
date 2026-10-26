//! Ripgrep adapter and binary locator (`core/src/ripgrep.ts`, `core/src/ripgrep/binary.ts`).

use std::path::{Path, PathBuf};
use std::process::Stdio;

use serde_json::Value;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, BufReader};
use tokio::sync::Mutex;

use crate::util::js;

pub const VERSION: &str = "15.1.0";
const ERROR_BYTES: usize = 8 * 1024;
const MAX_SUBMATCHES: usize = 100;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RipgrepError {
    /// `Ripgrep.Error`.
    Failed(String),
    /// `Ripgrep.InvalidPatternError` (rg stderr, trimmed).
    InvalidPattern(String),
}

impl std::fmt::Display for RipgrepError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RipgrepError::Failed(m) | RipgrepError::InvalidPattern(m) => f.write_str(m),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Submatch {
    pub text: String,
    pub start: u64,
    pub end: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GrepMatch {
    /// Path relative to the search cwd (normalized).
    pub path: String,
    pub line: u64,
    pub offset: u64,
    pub text: String,
    pub submatches: Vec<Submatch>,
}

pub struct GlobInput<'a> {
    pub cwd: &'a Path,
    pub pattern: &'a str,
    pub limit: usize,
    pub hidden: bool,
    pub follow: bool,
}

pub struct GrepInput<'a> {
    pub cwd: &'a Path,
    pub pattern: &'a str,
    pub file: Option<&'a str>,
    pub include: Option<&'a str>,
    pub literal: bool,
    pub case_sensitive: Option<bool>,
    pub limit: usize,
}

pub struct FindInput<'a> {
    pub cwd: &'a Path,
    pub pattern: &'a str,
    pub limit: usize,
    pub exclude: &'a [String],
    pub hidden: bool,
    pub follow: bool,
}

/// `normalizePath`: strip leading `./`, leading slashes, `\` → `/`.
pub fn normalize_path(value: &str) -> String {
    let mut v = value;
    while let Some(rest) = v.strip_prefix("./").or_else(|| v.strip_prefix(".\\")) {
        v = rest;
    }
    let v = v.trim_start_matches(['/', '\\']);
    v.replace('\\', "/")
}

fn is_invalid_pattern(stderr: &str) -> bool {
    stderr.contains("regex parse error") || stderr.contains("error parsing regex")
}

pub fn glob_args(pattern: &str, hidden: bool, follow: bool) -> Vec<String> {
    let mut args = vec!["--no-config".to_string(), "--files".into()];
    if hidden {
        args.push("--hidden".into());
    }
    if follow {
        args.push("--follow".into());
    }
    args.push(format!("--glob={pattern}"));
    if !hidden {
        args.push("--glob=!**/.*".into());
    }
    args.push("--glob=!**/.git/**".into());
    args.push(".".into());
    args
}

pub fn find_args(pattern: &str, exclude: &[String], hidden: bool, follow: bool) -> Vec<String> {
    let mut args = vec!["--no-config".to_string(), "--files".into()];
    if hidden {
        args.push("--hidden".into());
    }
    if follow {
        args.push("--follow".into());
    }
    if pattern != "*" {
        args.push(format!("--glob={pattern}"));
    }
    for ex in exclude {
        args.push(format!("--glob=!{ex}"));
    }
    args.push("--glob=!**/.git/**".into());
    args.push(".".into());
    args
}

pub fn grep_args(input: &GrepInput<'_>) -> Vec<String> {
    let mut args = vec!["--no-config".to_string(), "--json".into(), "--hidden".into(), "--no-messages".into()];
    if input.literal {
        args.push("--fixed-strings".into());
    }
    if input.case_sensitive == Some(false) {
        args.push("--ignore-case".into());
    }
    if let Some(include) = input.include {
        args.push(format!("--glob={include}"));
    }
    args.push("--glob=!**/.git/**".into());
    args.push("--".into());
    args.push(input.pattern.to_string());
    args.push(input.file.unwrap_or(".").to_string());
    args
}

/// Parse one `rg --json` line; `Ok(None)` for non-match records.
pub fn parse_grep_line(line: &str) -> Result<Option<GrepMatch>, RipgrepError> {
    let json: Value = serde_json::from_str(line).map_err(|_| RipgrepError::Failed("Invalid ripgrep JSON output".into()))?;
    if json.get("type").and_then(Value::as_str) != Some("match") {
        return Ok(None);
    }
    let invalid = || RipgrepError::Failed("Invalid ripgrep match output".into());
    let data = json.get("data").ok_or_else(invalid)?;
    let path = data.pointer("/path/text").and_then(Value::as_str).ok_or_else(invalid)?;
    let text = data.pointer("/lines/text").and_then(Value::as_str).ok_or_else(invalid)?;
    let line = data.get("line_number").and_then(Value::as_u64).filter(|n| *n > 0).ok_or_else(invalid)?;
    let offset = data.get("absolute_offset").and_then(Value::as_u64).ok_or_else(invalid)?;
    let mut submatches = Vec::new();
    for sub in data.get("submatches").and_then(Value::as_array).ok_or_else(invalid)? {
        submatches.push(Submatch {
            text: sub.pointer("/match/text").and_then(Value::as_str).ok_or_else(invalid)?.to_string(),
            start: sub.get("start").and_then(Value::as_u64).ok_or_else(invalid)?,
            end: sub.get("end").and_then(Value::as_u64).ok_or_else(invalid)?,
        });
    }
    submatches.truncate(MAX_SUBMATCHES);
    let text = if js::utf16_len(text) > 2_000 { format!("{}...", js::utf16_prefix(text, 2_000)) } else { text.to_string() };
    Ok(Some(GrepMatch { path: normalize_path(path), line, offset, text, submatches }))
}

fn platform() -> Result<(&'static str, &'static str), String> {
    let key = format!(
        "{}-{}",
        match std::env::consts::ARCH {
            "aarch64" => "arm64",
            "x86_64" => "x64",
            "x86" => "ia32",
            other => other,
        },
        match std::env::consts::OS {
            "macos" => "darwin",
            "windows" => "win32",
            other => other,
        }
    );
    Ok(match key.as_str() {
        "arm64-darwin" => ("aarch64-apple-darwin", "tar.gz"),
        "arm64-linux" => ("aarch64-unknown-linux-gnu", "tar.gz"),
        "x64-darwin" => ("x86_64-apple-darwin", "tar.gz"),
        "x64-linux" => ("x86_64-unknown-linux-musl", "tar.gz"),
        "arm64-win32" => ("aarch64-pc-windows-msvc", "zip"),
        "ia32-win32" => ("i686-pc-windows-msvc", "zip"),
        "x64-win32" => ("x86_64-pc-windows-msvc", "zip"),
        _ => return Err(format!("unsupported platform for ripgrep: {key}")),
    })
}

/// Ripgrep runner with a per-process cached binary path.
pub struct Ripgrep {
    bin: PathBuf,
    http: reqwest::Client,
    cached: Mutex<Option<PathBuf>>,
    /// Override the executable (tests / hosted environments).
    pub override_path: Option<PathBuf>,
}

impl Ripgrep {
    /// `bin` = `<cache>/bin`.
    pub fn new(bin: PathBuf, http: reqwest::Client) -> Self {
        Self { bin, http, cached: Mutex::new(None), override_path: None }
    }

    /// `RipgrepBinary.filepath`.
    pub async fn filepath(&self) -> Result<PathBuf, String> {
        if let Some(path) = &self.override_path {
            return Ok(path.clone());
        }
        let mut cached = self.cached.lock().await;
        if let Some(path) = cached.as_ref() {
            return Ok(path.clone());
        }
        let exe = if cfg!(windows) { "rg.exe" } else { "rg" };
        let path = match crate::util::which(exe, Some(&self.bin)).filter(|p| p.is_file()) {
            Some(system) => system,
            None => {
                let target = self.bin.join(exe);
                if target.is_file() { target } else { self.download(&target).await? }
            }
        };
        *cached = Some(path.clone());
        Ok(path)
    }

    async fn download(&self, target: &Path) -> Result<PathBuf, String> {
        let (platform, extension) = platform()?;
        let filename = format!("ripgrep-{VERSION}-{platform}.{extension}");
        let url = format!("https://github.com/BurntSushi/ripgrep/releases/download/{VERSION}/{filename}");
        let archive = self.bin.join(&filename);
        tracing::info!(url = %url, "downloading ripgrep");
        std::fs::create_dir_all(&self.bin).map_err(|e| e.to_string())?;
        let mut request = self.http.get(&url);
        for (k, v) in oc_util::trace::headers() {
            request = request.header(k, v);
        }
        let response = request.send().await.map_err(|e| e.to_string())?;
        if !response.status().is_success() {
            return Err(format!("StatusCode: non 2xx status code ({} GET {url})", response.status().as_u16()));
        }
        let bytes = response.bytes().await.map_err(|e| e.to_string())?;
        if bytes.is_empty() {
            return Err(format!("failed to download ripgrep from {url}"));
        }
        std::fs::write(&archive, &bytes).map_err(|e| e.to_string())?;
        let dir = self.bin.join(format!("ripgrep-{}", oc_schema::id::create(false, None)));
        std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
        let result = self.extract(&archive, &dir, platform, extension, target).await;
        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_file(&archive);
        result.map(|_| target.to_path_buf())
    }

    async fn extract(&self, archive: &Path, dir: &Path, platform: &str, extension: &str, target: &Path) -> Result<(), String> {
        // Windows zips go through the bsdtar in System32 (Windows 10 1803+), not PowerShell, which policy can block.
        // Git's GNU tar can come first on PATH and cannot read zip, so the system copy is called by path. bsdtar
        // decodes its arguments in the ANSI code page, so it runs inside the temp dir and only sees the ASCII
        // archive name (126b7b633).
        let output = if extension == "zip" {
            let system_root = std::env::var("SystemRoot").unwrap_or_else(|_| "C:\\Windows".into());
            let tar = Path::new(&system_root).join("System32").join("tar.exe");
            let relative = archive
                .file_name()
                .map(|name| Path::new("..").join(name))
                .filter(|_| archive.parent() == dir.parent())
                .unwrap_or_else(|| archive.to_path_buf());
            tokio::process::Command::new(tar).arg("-xf").arg(relative).current_dir(dir).stdin(Stdio::null()).output().await
        } else {
            tokio::process::Command::new("tar")
                .arg("-xzf")
                .arg(archive)
                .arg("-C")
                .arg(dir)
                .current_dir(dir)
                .stdin(Stdio::null())
                .output()
                .await
        }
        .map_err(|e| e.to_string())?;
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
            let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
            return Err(if !stderr.is_empty() {
                stderr
            } else if !stdout.is_empty() {
                stdout
            } else {
                format!("ripgrep extraction failed with code {}", output.status.code().unwrap_or(-1))
            });
        }
        let extracted = dir.join(format!("ripgrep-{VERSION}-{platform}")).join(if cfg!(windows) { "rg.exe" } else { "rg" });
        if !extracted.is_file() {
            return Err(format!("ripgrep archive did not contain executable: {}", extracted.to_string_lossy()));
        }
        std::fs::copy(&extracted, target).map_err(|e| e.to_string())?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(target, std::fs::Permissions::from_mode(0o755)).map_err(|e| e.to_string())?;
        }
        Ok(())
    }

    async fn run<T>(
        &self,
        cwd: &Path,
        args: Vec<String>,
        limit: usize,
        pattern: Option<&str>,
        parse: impl Fn(&str) -> Result<Option<T>, RipgrepError>,
    ) -> Result<Vec<T>, RipgrepError> {
        let exe = self.filepath().await.map_err(RipgrepError::Failed)?;
        let mut child = tokio::process::Command::new(exe)
            .args(&args)
            .current_dir(cwd)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .map_err(|e| RipgrepError::Failed(format!("ripgrep execution failed: {e}")))?;
        let mut stderr = child.stderr.take().expect("stderr");
        let stderr_task = tokio::spawn(async move {
            let mut buffer = Vec::new();
            let mut chunk = [0u8; 4096];
            loop {
                match stderr.read(&mut chunk).await {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        if buffer.len() < ERROR_BYTES {
                            let take = n.min(ERROR_BYTES - buffer.len());
                            buffer.extend_from_slice(&chunk[..take]);
                        }
                    }
                }
            }
            String::from_utf8_lossy(&buffer).into_owned()
        });
        let stdout = child.stdout.take().expect("stdout");
        let mut lines = BufReader::new(stdout).lines();
        let mut rows = Vec::new();
        while let Some(line) = lines.next_line().await.map_err(|e| RipgrepError::Failed(e.to_string()))? {
            if line.is_empty() {
                continue;
            }
            if let Some(row) = parse(&line)? {
                rows.push(row);
                if rows.len() > limit {
                    let _ = child.start_kill();
                    rows.truncate(limit);
                    return Ok(rows);
                }
            }
        }
        let status = child.wait().await.map_err(|e| RipgrepError::Failed(e.to_string()))?;
        let code = status.code().unwrap_or(-1);
        let stderr = stderr_task.await.unwrap_or_default();
        if pattern.is_some() && code == 2 && is_invalid_pattern(&stderr) {
            return Err(RipgrepError::InvalidPattern(stderr.trim().to_string()));
        }
        if code != 0 && code != 1 && code != 2 {
            let trimmed = stderr.trim();
            return Err(RipgrepError::Failed(if trimmed.is_empty() { format!("ripgrep failed with code {code}") } else { trimmed.to_string() }));
        }
        Ok(if code == 1 { Vec::new() } else { rows })
    }

    /// File paths matching a glob (relative to `cwd`).
    pub async fn glob(&self, input: GlobInput<'_>) -> Result<Vec<String>, RipgrepError> {
        self.run(input.cwd, glob_args(input.pattern, input.hidden, input.follow), input.limit, None, |line| Ok(Some(normalize_path(line))))
            .await
            .map_err(|e| match e {
                RipgrepError::InvalidPattern(m) => RipgrepError::Failed(m),
                other => other,
            })
    }

    pub async fn find(&self, input: FindInput<'_>) -> Result<Vec<String>, RipgrepError> {
        self.run(input.cwd, find_args(input.pattern, input.exclude, input.hidden, input.follow), input.limit, None, |line| {
            Ok(Some(normalize_path(line)))
        })
        .await
    }

    pub async fn grep(&self, input: GrepInput<'_>) -> Result<Vec<GrepMatch>, RipgrepError> {
        self.run(input.cwd, grep_args(&input), input.limit, Some(input.pattern), parse_grep_line).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn args_exact() {
        assert_eq!(glob_args("**/*.ts", false, false), vec!["--no-config", "--files", "--glob=**/*.ts", "--glob=!**/.*", "--glob=!**/.git/**", "."]);
        assert_eq!(glob_args("*", true, true), vec!["--no-config", "--files", "--hidden", "--follow", "--glob=*", "--glob=!**/.git/**", "."]);
        let input = GrepInput { cwd: Path::new("."), pattern: "foo", file: Some("a.ts"), include: Some("*.ts"), literal: true, case_sensitive: Some(false), limit: 1 };
        assert_eq!(
            grep_args(&input),
            vec!["--no-config", "--json", "--hidden", "--no-messages", "--fixed-strings", "--ignore-case", "--glob=*.ts", "--glob=!**/.git/**", "--", "foo", "a.ts"]
        );
        assert_eq!(find_args("*", &["node_modules".into()], false, false), vec!["--no-config", "--files", "--glob=!node_modules", "--glob=!**/.git/**", "."]);
    }

    #[test]
    fn paths_and_json() {
        assert_eq!(normalize_path("././a/b"), "a/b");
        assert_eq!(normalize_path(".\\a\\b"), "a/b");
        assert_eq!(normalize_path("//x"), "x");
        let line = r#"{"type":"match","data":{"path":{"text":"./src/a.ts"},"lines":{"text":"let x = 1\n"},"line_number":3,"absolute_offset":10,"submatches":[{"match":{"text":"x"},"start":4,"end":5}]}}"#;
        let m = parse_grep_line(line).unwrap().unwrap();
        assert_eq!(m.path, "src/a.ts");
        assert_eq!(m.text, "let x = 1\n");
        assert_eq!(m.line, 3);
        assert_eq!(m.submatches[0], Submatch { text: "x".into(), start: 4, end: 5 });
        assert!(parse_grep_line(r#"{"type":"begin","data":{}}"#).unwrap().is_none());
        assert!(parse_grep_line("nope").is_err());
    }

    #[tokio::test]
    async fn runs_system_ripgrep_when_available() {
        let Some(rg) = crate::util::which("rg", None) else { return };
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.txt"), "hello\nworld\n").unwrap();
        std::fs::write(dir.path().join(".hidden"), "hello\n").unwrap();
        let mut ripgrep = Ripgrep::new(dir.path().join("bin"), reqwest::Client::new());
        ripgrep.override_path = Some(rg);
        let files = ripgrep.glob(GlobInput { cwd: dir.path(), pattern: "*", limit: 10, hidden: false, follow: false }).await.unwrap();
        assert_eq!(files, vec!["a.txt"]);
        let matches = ripgrep
            .grep(GrepInput { cwd: dir.path(), pattern: "hello", file: None, include: None, literal: false, case_sensitive: None, limit: 10 })
            .await
            .unwrap();
        assert_eq!(matches.len(), 2);
        let err = ripgrep
            .grep(GrepInput { cwd: dir.path(), pattern: "(", file: None, include: None, literal: false, case_sensitive: None, limit: 10 })
            .await
            .unwrap_err();
        assert!(matches!(err, RipgrepError::InvalidPattern(m) if m.contains("regex parse error")));
        let none = ripgrep
            .grep(GrepInput { cwd: dir.path(), pattern: "zzz", file: None, include: None, literal: false, case_sensitive: None, limit: 10 })
            .await
            .unwrap();
        assert!(none.is_empty());
        let one = ripgrep
            .grep(GrepInput { cwd: dir.path(), pattern: "hello", file: None, include: None, literal: false, case_sensitive: None, limit: 1 })
            .await
            .unwrap();
        assert_eq!(one.len(), 1);
    }
}

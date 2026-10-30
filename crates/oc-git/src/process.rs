//! Minimal `AppProcess.run` equivalent for the `git` executable.

use std::path::Path;
use std::process::Stdio;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::process::Command;

const DARWIN_SHIM: &str = "/usr/bin/git";
const XCODE_SELECT_LINK: &str = "/var/db/xcode_select_link";
const DEFAULT_DEVELOPER_DIRS: [&str; 2] =
    ["/Applications/Xcode.app/Contents/Developer", "/Library/Developer/CommandLineTools"];

/// `darwinDeveloperDirs`: `$DEVELOPER_DIR` alone when set, else the `xcode-select` choice ahead of
/// the two default locations.
pub fn darwin_developer_dirs(developer_dir: Option<&str>, read_link: impl Fn(&str) -> Option<String>) -> Vec<String> {
    if let Some(dir) = developer_dir.map(str::trim).filter(|dir| !dir.is_empty()) {
        return vec![dir.to_string()];
    }
    let defaults = DEFAULT_DEVELOPER_DIRS.iter().map(|dir| dir.to_string());
    match read_link(XCODE_SELECT_LINK) {
        Some(selected) => std::iter::once(selected).chain(defaults).collect(),
        None => defaults.collect(),
    }
}

fn read_xcode_select_link(link: &str) -> Option<String> {
    if !Path::new(link).exists() {
        return None;
    }
    std::fs::canonicalize(link).ok().map(|path| path.to_string_lossy().into_owned())
}

/// Node `path.posix.resolve` of an absolute or cwd-relative path, lexically.
fn posix_resolve(file: &str) -> String {
    let joined = if file.starts_with('/') {
        file.to_string()
    } else {
        let cwd = std::env::current_dir().map(|dir| dir.to_string_lossy().into_owned()).unwrap_or_default();
        format!("{cwd}/{file}")
    };
    let mut parts: Vec<&str> = Vec::new();
    for part in joined.split('/') {
        match part {
            "" | "." => {}
            ".." => {
                parts.pop();
            }
            other => parts.push(other),
        }
    }
    format!("/{}", parts.join("/"))
}

/// `resolveGitExecutable`: the absolute `which git` on Windows; on macOS the Apple shim at
/// `/usr/bin/git` (which runs `xcrun` on every call) is swapped for the developer tools' own Git;
/// `git` otherwise.
pub fn resolve_git_executable(
    platform: &str,
    resolved: Option<&str>,
    developer_dirs: Option<&[String]>,
    exists: &dyn Fn(&str) -> bool,
) -> String {
    if platform == "win32" {
        return match resolved {
            Some(resolved) => std::path::absolute(resolved)
                .map(|path| path.to_string_lossy().into_owned())
                .unwrap_or_else(|_| resolved.to_string()),
            None => "git".to_string(),
        };
    }
    let Some(resolved) = resolved.filter(|_| platform == "darwin") else { return "git".to_string() };
    let full = posix_resolve(resolved);
    if full != DARWIN_SHIM {
        return full;
    }
    let computed;
    let dirs = match developer_dirs {
        Some(dirs) => dirs,
        None => {
            computed = darwin_developer_dirs(std::env::var("DEVELOPER_DIR").ok().as_deref(), read_xcode_select_link);
            &computed
        }
    };
    dirs.iter()
        .filter(|dir| !dir.is_empty() && dir.starts_with('/'))
        .map(|dir| posix_resolve(&format!("{dir}/usr/bin/git")))
        .find(|candidate| candidate != DARWIN_SHIM && exists(candidate))
        .unwrap_or_else(|| "git".to_string())
}

fn platform() -> &'static str {
    match std::env::consts::OS {
        "windows" => "win32",
        "macos" => "darwin",
        other => other,
    }
}

/// `which`: the first executable `name` on `PATH` (with `PATHEXT` on Windows).
fn which(name: &str) -> Option<String> {
    let path = std::env::var_os("PATH")?;
    let extensions: Vec<String> = if cfg!(windows) {
        std::env::var("PATHEXT")
            .unwrap_or_else(|_| ".EXE;.CMD;.BAT;.COM".to_string())
            .split(';')
            .filter(|ext| !ext.is_empty())
            .map(str::to_string)
            .collect()
    } else {
        vec![String::new()]
    };
    for dir in std::env::split_paths(&path) {
        for ext in &extensions {
            let candidate = dir.join(format!("{name}{ext}"));
            let Ok(meta) = std::fs::metadata(&candidate) else { continue };
            #[cfg(unix)]
            let executable = {
                use std::os::unix::fs::PermissionsExt;
                meta.is_file() && meta.permissions().mode() & 0o111 != 0
            };
            #[cfg(not(unix))]
            let executable = meta.is_file();
            if executable {
                return Some(candidate.to_string_lossy().into_owned());
            }
        }
    }
    None
}

/// `util/git-executable.ts` `gitExecutable()`: resolved once, on first use.
pub fn git_executable() -> &'static str {
    static EXECUTABLE: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    EXECUTABLE.get_or_init(|| {
        let platform = platform();
        let resolved = if platform == "win32" || platform == "darwin" { which("git") } else { None };
        resolve_git_executable(platform, resolved.as_deref(), None, &|file| Path::new(file).exists())
    })
}

#[derive(Debug, Clone, Default)]
pub struct RunOptions<'a> {
    pub env: Vec<(&'a str, String)>,
    /// `None` → stdin ignored; `Some` → piped and written.
    pub stdin: Option<Vec<u8>>,
    pub max_output_bytes: Option<usize>,
}

#[derive(Debug, Clone)]
pub struct RunResult {
    pub exit_code: i32,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
    pub stdout_truncated: bool,
}

impl RunResult {
    pub fn text(&self) -> String {
        String::from_utf8_lossy(&self.stdout).into_owned()
    }
    pub fn stderr_text(&self) -> String {
        String::from_utf8_lossy(&self.stderr).into_owned()
    }
}

async fn collect<R: AsyncReadExt + Unpin>(mut reader: R, max: Option<usize>) -> std::io::Result<(Vec<u8>, bool)> {
    let mut out = Vec::new();
    let mut total = 0usize;
    let mut truncated = false;
    let mut buf = vec![0u8; 64 * 1024];
    loop {
        let n = reader.read(&mut buf).await?;
        if n == 0 {
            break;
        }
        match max {
            None => out.extend_from_slice(&buf[..n]),
            Some(max) => {
                let remaining = max.saturating_sub(total);
                if remaining > 0 {
                    out.extend_from_slice(&buf[..n.min(remaining)]);
                }
                total += n;
                truncated = truncated || total > max;
            }
        }
    }
    Ok((out, truncated))
}

/// Run `git <args>` in `cwd`, inheriting the process environment plus `options.env`.
pub async fn run(cwd: &Path, args: &[String], options: RunOptions<'_>) -> std::io::Result<RunResult> {
    let mut command = Command::new(git_executable());
    command.args(args).current_dir(cwd).kill_on_drop(true);
    for (key, value) in &options.env {
        command.env(key, value);
    }
    command.stdout(Stdio::piped()).stderr(Stdio::piped());
    command.stdin(if options.stdin.is_some() { Stdio::piped() } else { Stdio::null() });
    let mut child = command.spawn()?;
    let stdin = child.stdin.take();
    let input = options.stdin;
    let writer = async move {
        if let (Some(mut stdin), Some(input)) = (stdin, input) {
            // A process may exit before reading its input; a broken pipe is not an error here.
            let _ = stdin.write_all(&input).await;
            let _ = stdin.shutdown().await;
        }
    };
    let stdout = child.stdout.take().expect("stdout piped");
    let stderr = child.stderr.take().expect("stderr piped");
    let max = options.max_output_bytes;
    let (_, out, err, status) =
        tokio::join!(writer, collect(stdout, max), collect(stderr, None), async { child.wait().await });
    let (stdout, stdout_truncated) = out?;
    let (stderr, _) = err?;
    let status = status?;
    Ok(RunResult { exit_code: status.code().unwrap_or(-1), stdout, stderr, stdout_truncated })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;

    fn dirs(values: &[&str]) -> Vec<String> {
        values.iter().map(|value| value.to_string()).collect()
    }

    // test/util/git-executable.test.ts
    #[test]
    fn uses_a_non_shim_path_executable_on_macos_without_probing() {
        let calls = Cell::new(0);
        let exists = |_: &str| {
            calls.set(calls.get() + 1);
            true
        };
        let resolved = resolve_git_executable(
            "darwin",
            Some("/opt/homebrew/bin/git"),
            Some(&dirs(&["/Library/Developer/CommandLineTools"])),
            &exists,
        );
        assert_eq!(resolved, "/opt/homebrew/bin/git");
        assert_eq!(calls.get(), 0);
    }

    #[test]
    fn resolves_the_apple_shim_from_developer_directories() {
        let resolved = resolve_git_executable(
            "darwin",
            Some("/usr/bin/git"),
            Some(&dirs(&["/Applications/Xcode.app/Contents/Developer", "/Library/Developer/CommandLineTools"])),
            &|file| file == "/Library/Developer/CommandLineTools/usr/bin/git",
        );
        assert_eq!(resolved, "/Library/Developer/CommandLineTools/usr/bin/git");
    }

    #[test]
    fn falls_back_to_path_lookup_on_macos() {
        let both = dirs(&["/Applications/Xcode.app/Contents/Developer", "/Library/Developer/CommandLineTools"]);
        assert_eq!(resolve_git_executable("darwin", Some("/usr/bin/git"), Some(&both), &|_| false), "git");
        let relative = dirs(&["relative/dir"]);
        assert_eq!(resolve_git_executable("darwin", Some("/usr/bin/git"), Some(&relative), &|_| true), "git");
        assert_eq!(resolve_git_executable("darwin", Some("/usr/bin/git"), Some(&dirs(&["/"])), &|_| true), "git");
        let tools = dirs(&["/Library/Developer/CommandLineTools"]);
        assert_eq!(resolve_git_executable("darwin", None, Some(&tools), &|_| true), "git");
    }

    #[test]
    fn orders_developer_directories() {
        assert_eq!(
            darwin_developer_dirs(Some("/Custom/Developer"), |_| Some("/Library/Developer/CommandLineTools".into())),
            dirs(&["/Custom/Developer"])
        );
        assert_eq!(
            darwin_developer_dirs(Some(""), |_| Some("/Applications/Xcode-beta.app/Contents/Developer".into())),
            dirs(&[
                "/Applications/Xcode-beta.app/Contents/Developer",
                "/Applications/Xcode.app/Contents/Developer",
                "/Library/Developer/CommandLineTools",
            ])
        );
        assert_eq!(
            darwin_developer_dirs(None, |_| None),
            dirs(&["/Applications/Xcode.app/Contents/Developer", "/Library/Developer/CommandLineTools"])
        );
    }

    #[test]
    fn leaves_lookup_to_the_process_elsewhere() {
        assert_eq!(resolve_git_executable("linux", Some("/usr/bin/git"), None, &|_| true), "git");
        assert_eq!(resolve_git_executable("win32", None, None, &|_| true), "git");
        let absolute = resolve_git_executable("win32", Some("bin/git.exe"), None, &|_| true);
        assert_eq!(std::path::PathBuf::from(absolute), std::path::absolute("bin/git.exe").unwrap());
    }
}

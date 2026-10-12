//! Shell selection (`core/src/shell/select.ts`).

use std::path::{Path, PathBuf};
use std::sync::LazyLock;

use parking_lot::Mutex;

struct Meta {
    deny: bool,
    login: bool,
    ps: bool,
}

fn meta(file: &str) -> Option<Meta> {
    Some(match name(file).as_str() {
        "bash" | "dash" | "ksh" | "sh" | "zsh" => Meta { deny: false, login: true, ps: false },
        "fish" => Meta { deny: true, login: true, ps: false },
        "nu" => Meta { deny: true, login: false, ps: false },
        "powershell" | "pwsh" => Meta { deny: false, login: false, ps: true },
        _ => return None,
    })
}

/// `ShellSelect.name`: lowercase basename (win32: parsed name without extension).
pub fn name(file: &str) -> String {
    if cfg!(windows) {
        let normalized = crate::file_access::windows_path(file).replace('\\', "/");
        let base = normalized.rsplit('/').next().unwrap_or("").to_string();
        return match base.rfind('.') {
            Some(i) if i > 0 => base[..i].to_lowercase(),
            _ => base.to_lowercase(),
        };
    }
    Path::new(file).file_name().map(|n| n.to_string_lossy().to_lowercase()).unwrap_or_default()
}

pub fn login(file: &str) -> bool {
    meta(file).is_some_and(|m| m.login)
}

pub fn ps(file: &str) -> bool {
    meta(file).is_some_and(|m| m.ps)
}

fn compatible(file: &str) -> bool {
    !meta(file).is_some_and(|m| m.deny)
}

/// `ShellSelect.args(file, command)`.
pub fn args(file: &str, command: &str) -> Vec<String> {
    let n = name(file);
    if n == "cmd" {
        return vec!["/c".into(), command.into()];
    }
    if ps(file) {
        return vec!["-NoLogo".into(), "-NoProfile".into(), "-NonInteractive".into(), "-Command".into(), command.into()];
    }
    vec!["-c".into(), command.into()]
}

fn find(name: &str, bin: Option<&Path>) -> Option<String> {
    crate::util::which(name, bin).map(|p| p.to_string_lossy().into_owned())
}

fn gitbash(bin: Option<&Path>) -> Option<String> {
    if !cfg!(windows) {
        return None;
    }
    let git = find("git", bin)?;
    let file = PathBuf::from(git).join("..").join("..").join("bin").join("bash.exe");
    std::fs::metadata(&file).ok().filter(|m| m.len() > 0).map(|_| file.to_string_lossy().into_owned())
}

fn full(file: &str, bin: Option<&Path>) -> String {
    if !cfg!(windows) {
        return file.to_string();
    }
    let shell = crate::file_access::windows_path(file);
    let has_dir = shell.contains('/') || shell.contains('\\');
    if has_dir {
        if shell.starts_with('/') && name(&shell) == "bash" {
            return gitbash(bin).unwrap_or(shell);
        }
        return shell;
    }
    if name(&shell) == "bash" {
        return gitbash(bin).or_else(|| find(&shell, bin)).unwrap_or(shell);
    }
    find(&shell, bin).unwrap_or(shell)
}

fn executable(file: &str, bin: Option<&Path>) -> Option<String> {
    let shell = full(file, bin);
    if Path::new(&crate::file_access::windows_path(&shell)).is_absolute() {
        return std::fs::metadata(&shell).ok().filter(|m| m.is_file()).map(|_| shell);
    }
    find(&shell, bin)
}

fn fallback(bin: Option<&Path>) -> String {
    if cfg!(target_os = "macos") {
        return "/bin/zsh".into();
    }
    find("bash", bin).unwrap_or_else(|| "/bin/sh".into())
}

fn win(bin: Option<&Path>) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for item in [
        find("pwsh", bin),
        find("powershell", bin),
        gitbash(bin),
        Some(std::env::var("COMSPEC").ok().filter(|s| !s.is_empty()).unwrap_or_else(|| "cmd.exe".into())),
    ]
    .into_iter()
    .flatten()
    {
        let f = full(&item, bin);
        if !out.contains(&f) {
            out.push(f);
        }
    }
    out
}

fn select(file: Option<&str>, compat: bool, bin: Option<&Path>) -> String {
    if let Some(file) = file.filter(|f| !f.is_empty()) {
        if !compat || compatible(file) {
            if let Some(shell) = executable(file, bin) {
                return shell;
            }
        }
    }
    if cfg!(windows) {
        return win(bin).into_iter().next().unwrap_or_else(|| fallback(bin));
    }
    fallback(bin)
}

static CACHE: LazyLock<Mutex<[Option<(Option<PathBuf>, String)>; 2]>> = LazyLock::new(|| Mutex::new([None, None]));

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Priority {
    Config,
    Compat,
}

/// `ShellSelect.resolve({priority}, configShell, options, bin)`.
pub fn resolve(priority: Priority, config_shell: Option<&str>, bin: Option<&Path>) -> String {
    let compat = priority == Priority::Compat;
    if let Some(shell) = config_shell.filter(|s| !s.is_empty()) {
        return select(Some(shell), compat, bin);
    }
    let slot = if compat { 1 } else { 0 };
    let key = bin.map(Path::to_path_buf);
    if let Some((cached_bin, value)) = &CACHE.lock()[slot] {
        if *cached_bin == key {
            return value.clone();
        }
    }
    let env_shell = std::env::var("SHELL").ok();
    let value = select(env_shell.as_deref(), compat, bin);
    CACHE.lock()[slot] = Some((key, value.clone()));
    value
}

pub fn reset() {
    *CACHE.lock() = [None, None];
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_and_args() {
        assert_eq!(name("/usr/bin/Bash"), "bash");
        assert!(ps("/usr/bin/pwsh"));
        assert!(login("/bin/zsh"));
        assert_eq!(args("/bin/bash", "ls"), vec!["-c", "ls"]);
        assert_eq!(args("pwsh", "ls"), vec!["-NoLogo", "-NoProfile", "-NonInteractive", "-Command", "ls"]);
        assert_eq!(args("cmd", "dir"), vec!["/c", "dir"]);
    }

    #[test]
    fn compat_rejects_fish() {
        let chosen = select(Some("/usr/bin/fish"), true, None);
        assert_ne!(name(&chosen), "fish");
        assert_eq!(select(Some("/bin/sh"), true, None), "/bin/sh");
    }
}

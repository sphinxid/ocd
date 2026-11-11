//! Shared helpers.

pub mod bom;
pub mod js;
pub mod wildcard;

use std::path::{Component, Path, PathBuf};

/// `which(name, undefined, bin)`: search `PATH` followed by `<bin>`.
pub fn which(name: &str, bin: Option<&Path>) -> Option<PathBuf> {
    let mut paths = std::env::var_os("PATH").map(|p| std::env::split_paths(&p).collect::<Vec<_>>()).unwrap_or_default();
    if let Some(bin) = bin {
        paths.push(bin.to_path_buf());
    }
    let joined = std::env::join_paths(paths).ok()?;
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("/"));
    which::which_in(name, Some(&joined), cwd)
        .ok()
        .or_else(|| alias_candidates(name, &joined.to_string_lossy()).into_iter().find(|file| is_app_execution_alias(file)))
}

/// Microsoft Store apps, such as PowerShell 7, put app execution aliases in WindowsApps on PATH. They launch like any
/// executable, but stat rejects them with EACCES, so `which` skips them (`appExecutionAlias`): the `<cmd>.exe` files of
/// a bare command name in each folder of the search path, on Windows.
fn alias_candidates(cmd: &str, search: &str) -> Vec<PathBuf> {
    if cfg!(windows) { win32_alias_candidates(cmd, search) } else { Vec::new() }
}

/// [`alias_candidates`] with `path.win32` rules: `dirname(cmd)` must be `.`, and quoted folders are unquoted.
fn win32_alias_candidates(cmd: &str, search: &str) -> Vec<PathBuf> {
    let bare = !cmd.contains(['/', '\\']) && !(cmd.len() >= 2 && cmd.as_bytes()[1] == b':' && cmd.as_bytes()[0].is_ascii_alphabetic());
    if search.is_empty() || !bare {
        return Vec::new();
    }
    let name = if cmd.to_lowercase().ends_with(".exe") && cmd.len() > 4 { cmd.to_string() } else { format!("{cmd}.exe") };
    search
        .split(';')
        .filter(|dir| !dir.is_empty())
        .map(|dir| {
            let dir = dir.strip_prefix('"').and_then(|d| d.strip_suffix('"')).unwrap_or(dir).replace('/', "\\");
            let dir = if dir.len() > 3 { dir.trim_end_matches('\\').to_string() } else { dir };
            PathBuf::from(if dir.ends_with('\\') { format!("{dir}{name}") } else { format!("{dir}\\{name}") })
        })
        .collect()
}

/// `isAppExecutionAlias(file)`: on Windows, a path whose `lstat` is a link — as libuv reads it, which includes the
/// `IO_REPARSE_TAG_APPEXECLINK` reparse points of Microsoft Store apps that `std` reports as plain files.
pub fn is_app_execution_alias(file: &Path) -> bool {
    #[cfg(windows)]
    {
        windows_link(file)
    }
    #[cfg(not(windows))]
    {
        let _ = file;
        false
    }
}

#[cfg(windows)]
fn windows_link(file: &Path) -> bool {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Foundation::INVALID_HANDLE_VALUE;
    use windows_sys::Win32::Storage::FileSystem::{FILE_ATTRIBUTE_REPARSE_POINT, FindClose, FindFirstFileW, WIN32_FIND_DATAW};
    // The reparse tags libuv's lstat reports as links: symlinks, junctions and app execution aliases.
    const LINK_TAGS: [u32; 3] = [0xA000_000C, 0xA000_0003, 0x8000_001B];
    if std::fs::symlink_metadata(file).is_ok_and(|m| m.file_type().is_symlink()) {
        return true;
    }
    let wide: Vec<u16> = file.as_os_str().encode_wide().chain(std::iter::once(0)).collect();
    // SAFETY: `wide` is NUL-terminated and `data` is a plain struct the call fills in.
    unsafe {
        let mut data: WIN32_FIND_DATAW = std::mem::zeroed();
        let handle = FindFirstFileW(wide.as_ptr(), &mut data);
        if handle == INVALID_HANDLE_VALUE {
            return false;
        }
        FindClose(handle);
        data.dwFileAttributes & FILE_ATTRIBUTE_REPARSE_POINT != 0 && LINK_TAGS.contains(&data.dwReserved0)
    }
}

/// Node's `path.resolve(base, input)`: lexical, no symlink resolution.
pub fn resolve(base: &Path, input: &str) -> PathBuf {
    let joined = if Path::new(input).is_absolute() { PathBuf::from(input) } else { base.join(input) };
    normalize(&joined)
}

/// Lexically normalize `.`/`..` components (like `path.normalize` on an absolute path),
/// dropping a trailing separator.
pub fn normalize(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::Prefix(p) => out.push(p.as_os_str()),
            Component::RootDir => out.push(Component::RootDir.as_os_str()),
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            Component::Normal(part) => out.push(part),
        }
    }
    if out.as_os_str().is_empty() {
        out.push(".");
    }
    out
}

/// Node's `path.relative(from, to)` for absolute normalized paths.
pub fn relative(from: &Path, to: &Path) -> String {
    let from = normalize(from);
    let to = normalize(to);
    let a: Vec<_> = from.components().collect();
    let b: Vec<_> = to.components().collect();
    let mut common = 0;
    while common < a.len() && common < b.len() && a[common] == b[common] {
        common += 1;
    }
    let mut parts: Vec<String> = Vec::new();
    for _ in common..a.len() {
        parts.push("..".into());
    }
    for c in &b[common..] {
        parts.push(c.as_os_str().to_string_lossy().into_owned());
    }
    parts.join(std::path::MAIN_SEPARATOR_STR)
}

/// `FSUtil.contains(parent, child)`.
pub fn contains(parent: &Path, child: &Path) -> bool {
    let rel = relative(parent, child);
    rel.is_empty() || (!Path::new(&rel).is_absolute() && rel != ".." && !rel.starts_with(&format!("..{}", std::path::MAIN_SEPARATOR)))
}

/// `path.dirname`.
pub fn dirname(path: &Path) -> PathBuf {
    match path.parent() {
        Some(p) if !p.as_os_str().is_empty() => p.to_path_buf(),
        Some(_) => PathBuf::from("."),
        None => path.to_path_buf(),
    }
}

/// `path.dirname` on a display string (keeps `.` for bare names).
pub fn dirname_str(path: &str) -> String {
    let p = Path::new(path);
    match p.parent() {
        Some(parent) if !parent.as_os_str().is_empty() => parent.to_string_lossy().into_owned(),
        Some(_) => ".".into(),
        None => path.to_string(),
    }
}

/// `path.join(a, b)` on strings (normalizes; `join(".", x)` → `x`).
pub fn join_str(a: &str, b: &str) -> String {
    let joined = if a.is_empty() { b.to_string() } else { format!("{a}/{b}") };
    let normalized = normalize_relative(&joined);
    normalized
}

/// `path.normalize` for possibly-relative strings.
pub fn normalize_relative(input: &str) -> String {
    let absolute = input.starts_with('/');
    let mut parts: Vec<&str> = Vec::new();
    for part in input.split('/') {
        match part {
            "" | "." => {}
            ".." => {
                if parts.last().is_some_and(|p| *p != "..") {
                    parts.pop();
                } else if !absolute {
                    parts.push("..");
                }
            }
            other => parts.push(other),
        }
    }
    let body = parts.join("/");
    if absolute {
        format!("/{body}")
    } else if body.is_empty() {
        ".".into()
    } else {
        body
    }
}

/// `path.basename`.
pub fn basename(path: &str) -> String {
    let trimmed = path.trim_end_matches('/');
    trimmed.rsplit('/').next().unwrap_or("").to_string()
}

pub fn slash(value: &str) -> String {
    value.replace('\\', "/")
}

pub fn now_ms() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paths() {
        assert_eq!(resolve(Path::new("/a/b"), "../c"), PathBuf::from("/a/c"));
        assert_eq!(resolve(Path::new("/a/b"), "/x/./y/"), PathBuf::from("/x/y"));
        // `path.relative`: platform separators.
        let sep = std::path::MAIN_SEPARATOR;
        assert_eq!(relative(Path::new("/a/b"), Path::new("/a/b/c/d")), format!("c{sep}d"));
        assert_eq!(relative(Path::new("/a/b"), Path::new("/a/x")), format!("..{sep}x"));
        assert_eq!(relative(Path::new("/a/b"), Path::new("/a/b")), "");
        assert!(contains(Path::new("/a/b"), Path::new("/a/b/c")));
        assert!(contains(Path::new("/a/b"), Path::new("/a/b")));
        assert!(!contains(Path::new("/a/b"), Path::new("/a/bc")));
        assert_eq!(join_str(".", "x.txt"), "x.txt");
        assert_eq!(join_str("src", "a.ts"), "src/a.ts");
        assert_eq!(dirname_str("a.txt"), ".");
        assert_eq!(dirname_str("src/a.txt"), "src");
        assert_eq!(basename("src/a.txt"), "a.txt");
    }

    /// util/which.ts `appExecutionAlias` (#61952): the WindowsApps candidates of a bare command name.
    #[test]
    fn app_execution_alias_candidates() {
        let search = r#"C:\Windows;"C:\Users\me\AppData\Local\Microsoft\WindowsApps";;D:\tools\"#;
        let found: Vec<String> = win32_alias_candidates("pwsh", search).iter().map(|p| p.to_string_lossy().into_owned()).collect();
        assert_eq!(found, [r"C:\Windows\pwsh.exe", r"C:\Users\me\AppData\Local\Microsoft\WindowsApps\pwsh.exe", r"D:\tools\pwsh.exe"]);
        assert_eq!(win32_alias_candidates("pwsh.EXE", "C:\\a")[0], PathBuf::from(r"C:\a\pwsh.EXE"));
        assert_eq!(win32_alias_candidates("x", "C:\\")[0], PathBuf::from(r"C:\x.exe"));
        assert!(win32_alias_candidates("bin\\pwsh", search).is_empty());
        assert!(win32_alias_candidates("C:pwsh", search).is_empty());
        assert!(win32_alias_candidates("pwsh", "").is_empty());
        // Outside Windows nothing is an alias, and `which` never looks for one.
        if !cfg!(windows) {
            assert!(alias_candidates("pwsh", search).is_empty());
            assert!(!is_app_execution_alias(Path::new("/bin/sh")));
        }
    }
}

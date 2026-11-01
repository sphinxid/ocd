//! Server-local file IO of the browser tools (plugin-browser/src/files.ts): upload paths are read on the
//! server and sent as bytes; captures and downloads come back as bytes and are saved to temporary files.

use std::io::Read;
use std::path::{Path, PathBuf};

use oc_tools::ToolError;
use oc_tools::tool::ErrorCause;

use super::MAX_FILE_BYTES;

/// `Browser.File`: bytes crossing the RPC (base64 on the wire).
#[derive(Debug, Clone, PartialEq)]
pub struct File {
    pub id: String,
    pub name: String,
    pub mime: String,
    pub data: Vec<u8>,
}

/// An error with Node's `code`, so the failure text can prefix it like `failure` does.
struct NodeError {
    message: String,
    code: Option<String>,
}

impl NodeError {
    fn plain(message: impl Into<String>) -> Self {
        NodeError { message: message.into(), code: None }
    }

    /// A Node `fs` error: `<CODE>: <description>, <syscall> '<path>'`.
    fn fs(error: &std::io::Error, syscall: &str, path: Option<&Path>) -> Self {
        let (code, description) = errno(error);
        let target = path.map(|p| format!(" '{}'", p.display())).unwrap_or_default();
        NodeError { message: format!("{code}: {description}, {syscall}{target}"), code: Some(code.to_string()) }
    }
}

/// Node's error code and libuv description of an IO error.
pub(crate) fn errno(error: &std::io::Error) -> (&'static str, &'static str) {
    let code = error.raw_os_error().unwrap_or(0);
    #[cfg(unix)]
    {
        let known = match code {
            libc_codes::ENOENT => Some(("ENOENT", "no such file or directory")),
            libc_codes::EACCES => Some(("EACCES", "permission denied")),
            libc_codes::EPERM => Some(("EPERM", "operation not permitted")),
            libc_codes::ENOTDIR => Some(("ENOTDIR", "not a directory")),
            libc_codes::EISDIR => Some(("EISDIR", "illegal operation on a directory")),
            libc_codes::ELOOP => Some(("ELOOP", "too many symbolic links encountered")),
            libc_codes::ENAMETOOLONG => Some(("ENAMETOOLONG", "name too long")),
            libc_codes::EMFILE => Some(("EMFILE", "too many open files")),
            libc_codes::EEXIST => Some(("EEXIST", "file already exists")),
            libc_codes::ENOSPC => Some(("ENOSPC", "no space left on device")),
            libc_codes::EROFS => Some(("EROFS", "read-only file system")),
            libc_codes::EIO => Some(("EIO", "i/o error")),
            libc_codes::ECONNREFUSED => Some(("ECONNREFUSED", "connection refused")),
            libc_codes::ECONNRESET => Some(("ECONNRESET", "connection reset by peer")),
            libc_codes::EPIPE => Some(("EPIPE", "broken pipe")),
            libc_codes::ETIMEDOUT => Some(("ETIMEDOUT", "connection timed out")),
            libc_codes::EHOSTUNREACH => Some(("EHOSTUNREACH", "host is unreachable")),
            libc_codes::ENETUNREACH => Some(("ENETUNREACH", "network is unreachable")),
            libc_codes::EADDRNOTAVAIL => Some(("EADDRNOTAVAIL", "address not available")),
            libc_codes::EADDRINUSE => Some(("EADDRINUSE", "address already in use")),
            _ => None,
        };
        if let Some(known) = known {
            return known;
        }
    }
    let _ = code;
    match error.kind() {
        std::io::ErrorKind::NotFound => ("ENOENT", "no such file or directory"),
        std::io::ErrorKind::PermissionDenied => ("EACCES", "permission denied"),
        std::io::ErrorKind::ConnectionRefused => ("ECONNREFUSED", "connection refused"),
        std::io::ErrorKind::ConnectionReset => ("ECONNRESET", "connection reset by peer"),
        std::io::ErrorKind::BrokenPipe => ("EPIPE", "broken pipe"),
        std::io::ErrorKind::TimedOut => ("ETIMEDOUT", "connection timed out"),
        _ => ("EIO", "i/o error"),
    }
}

/// Linux errno values (the platforms ocd ships on share them except where noted by libc).
#[cfg(unix)]
#[allow(dead_code)]
mod libc_codes {
    pub const EPERM: i32 = 1;
    pub const ENOENT: i32 = 2;
    pub const EIO: i32 = 5;
    pub const EACCES: i32 = 13;
    pub const EEXIST: i32 = 17;
    pub const ENOTDIR: i32 = 20;
    pub const EISDIR: i32 = 21;
    pub const EMFILE: i32 = 24;
    pub const ENOSPC: i32 = 28;
    pub const EROFS: i32 = 30;
    pub const EPIPE: i32 = 32;
    #[cfg(target_os = "linux")]
    pub const ENAMETOOLONG: i32 = 36;
    #[cfg(not(target_os = "linux"))]
    pub const ENAMETOOLONG: i32 = 63;
    #[cfg(target_os = "linux")]
    pub const ELOOP: i32 = 40;
    #[cfg(not(target_os = "linux"))]
    pub const ELOOP: i32 = 62;
    #[cfg(target_os = "linux")]
    pub const EADDRINUSE: i32 = 98;
    #[cfg(not(target_os = "linux"))]
    pub const EADDRINUSE: i32 = 48;
    #[cfg(target_os = "linux")]
    pub const EADDRNOTAVAIL: i32 = 99;
    #[cfg(not(target_os = "linux"))]
    pub const EADDRNOTAVAIL: i32 = 49;
    #[cfg(target_os = "linux")]
    pub const ENETUNREACH: i32 = 101;
    #[cfg(not(target_os = "linux"))]
    pub const ENETUNREACH: i32 = 51;
    #[cfg(target_os = "linux")]
    pub const ECONNRESET: i32 = 104;
    #[cfg(not(target_os = "linux"))]
    pub const ECONNRESET: i32 = 54;
    #[cfg(target_os = "linux")]
    pub const ETIMEDOUT: i32 = 110;
    #[cfg(not(target_os = "linux"))]
    pub const ETIMEDOUT: i32 = 60;
    #[cfg(target_os = "linux")]
    pub const ECONNREFUSED: i32 = 111;
    #[cfg(not(target_os = "linux"))]
    pub const ECONNREFUSED: i32 = 61;
    #[cfg(target_os = "linux")]
    pub const EHOSTUNREACH: i32 = 113;
    #[cfg(not(target_os = "linux"))]
    pub const EHOSTUNREACH: i32 = 65;
}

/// `BrowserFiles.read`: reads upload paths (relative to `directory`) on the server, at most 5 MiB in total.
pub fn read(paths: &[String], directory: &Path) -> Result<Vec<File>, ToolError> {
    let mut files = Vec::with_capacity(paths.len());
    for input in paths {
        files.push(read_one(input, directory).map_err(|e| failure("read", e))?);
    }
    if files.iter().map(|f| f.data.len()).sum::<usize>() > MAX_FILE_BYTES {
        return Err(failure(
            "read",
            NodeError::plain(
                "The selected upload files exceed 5 MiB in total. Send fewer or smaller files; splitting them into one batch does not bypass the total limit.",
            ),
        ));
    }
    Ok(files)
}

fn read_one(input: &str, directory: &Path) -> Result<File, NodeError> {
    let path = crate::session::relative_join(directory, input);
    let mut file = std::fs::File::open(&path).map_err(|e| NodeError::fs(&e, "open", Some(&path)))?;
    let stat = file.metadata().map_err(|e| NodeError::fs(&e, "fstat", None))?;
    if !stat.is_file() {
        return Err(NodeError::plain("Upload paths must name files, not directories. Select a server-local file."));
    }
    if stat.len() > MAX_FILE_BYTES as u64 {
        return Err(NodeError::plain(format!(
            "Upload is {} bytes; the limit is {MAX_FILE_BYTES} bytes (5 MiB). Select a smaller file; do not retry the same upload.",
            stat.len()
        )));
    }
    let mut data = Vec::with_capacity(stat.len() as usize);
    file.read_to_end(&mut data).map_err(|e| NodeError::fs(&e, "read", None))?;
    Ok(File {
        id: format!("file_{}", uuid::Uuid::new_v4()),
        name: basename(input).to_string(),
        mime: mime(&extname(input).to_lowercase()).to_string(),
        data,
    })
}

fn mime(extension: &str) -> &'static str {
    match extension {
        ".txt" => "text/plain",
        ".csv" => "text/csv",
        ".json" => "application/json",
        ".html" => "text/html",
        ".png" => "image/png",
        ".jpg" | ".jpeg" => "image/jpeg",
        ".webp" => "image/webp",
        ".gif" => "image/gif",
        ".svg" => "image/svg+xml",
        ".pdf" => "application/pdf",
        ".zip" => "application/zip",
        ".gz" => "application/gzip",
        _ => "application/octet-stream",
    }
}

/// Node's `path.basename` (POSIX).
pub fn basename(path: &str) -> &str {
    let trimmed = path.trim_end_matches('/');
    trimmed.rsplit('/').next().unwrap_or(trimmed)
}

/// Node's `path.extname` (POSIX), ported from its scan so `..x` is `.x` and `.bashrc` has none.
pub fn extname(path: &str) -> &str {
    let bytes = path.as_bytes();
    let (mut start_dot, mut start_part, mut end) = (None::<usize>, 0usize, None::<usize>);
    let mut matched_slash = true;
    let mut pre_dot_state = 0i8;
    for i in (0..bytes.len()).rev() {
        let c = bytes[i];
        if c == b'/' {
            if !matched_slash {
                start_part = i + 1;
                break;
            }
            continue;
        }
        if end.is_none() {
            matched_slash = false;
            end = Some(i + 1);
        }
        if c == b'.' {
            if start_dot.is_none() {
                start_dot = Some(i);
            } else if pre_dot_state != 1 {
                pre_dot_state = 1;
            }
        } else if start_dot.is_some() {
            pre_dot_state = -1;
        }
    }
    match (start_dot, end) {
        (Some(dot), Some(end)) if pre_dot_state != 0 && !(pre_dot_state == 1 && dot + 1 == end && dot == start_part + 1) => &path[dot..end],
        _ => "",
    }
}

/// A capture saved on the server (`Browser.FileInfo`).
#[derive(Debug, Clone, PartialEq)]
pub struct Saved {
    pub id: String,
    pub name: String,
    pub mime: String,
    pub bytes: usize,
    pub path: PathBuf,
}

/// `BrowserFiles.save`: writes captures under a fresh `<tmp>/opencode-browser-XXXXXX/<index>/<name>`.
pub fn save(files: &[File]) -> Result<Vec<Saved>, ToolError> {
    if files.is_empty() {
        return Ok(vec![]);
    }
    if files.iter().map(|f| f.data.len()).sum::<usize>() > MAX_FILE_BYTES {
        return Err(failure(
            "save",
            NodeError::plain(
                "Capture files exceed the 5 MiB total transfer limit. Use a smaller screenshot, a shorter trace/profile, or a smaller page for heap capture; do not retry the identical capture.",
            ),
        ));
    }
    let directory = mkdtemp(&tmpdir().join("opencode-browser-")).map_err(|e| failure("save", e))?;
    let mut out = Vec::with_capacity(files.len());
    for (index, file) in files.iter().enumerate() {
        let name = capture_name(&file.name);
        let parent = directory.join(index.to_string());
        std::fs::create_dir(&parent).map_err(|e| failure("save", NodeError::fs(&e, "mkdir", Some(&parent))))?;
        let path = parent.join(&name);
        write_new(&path, &file.data).map_err(|e| failure("save", NodeError::fs(&e, "open", Some(&path))))?;
        out.push(Saved { id: file.id.clone(), name: file.name.clone(), mime: file.mime.clone(), bytes: file.data.len(), path });
    }
    Ok(out)
}

fn write_new(path: &Path, data: &[u8]) -> std::io::Result<()> {
    use std::io::Write;
    let mut file = std::fs::OpenOptions::new().write(true).create_new(true).open(path)?;
    file.write_all(data)
}

/// Node's `os.tmpdir()`: `TMPDIR`, `TMP` or `TEMP`, else `/tmp`, without a trailing slash.
fn tmpdir() -> PathBuf {
    #[cfg(unix)]
    {
        let value = ["TMPDIR", "TMP", "TEMP"]
            .iter()
            .find_map(|k| std::env::var(k).ok().filter(|v| !v.is_empty()))
            .unwrap_or_else(|| "/tmp".to_string());
        let trimmed = if value.len() > 1 { value.trim_end_matches('/') } else { value.as_str() };
        PathBuf::from(if trimmed.is_empty() { "/" } else { trimmed })
    }
    #[cfg(not(unix))]
    {
        std::env::temp_dir()
    }
}

/// `fs.mkdtemp(prefix)`: `prefix` + six random alphanumerics, created with mode 0700.
fn mkdtemp(prefix: &Path) -> Result<PathBuf, NodeError> {
    use rand::Rng;
    const CHARS: &[u8] = b"abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789";
    let mut rng = rand::rng();
    loop {
        let suffix: String = (0..6).map(|_| CHARS[rng.random_range(0..CHARS.len())] as char).collect();
        let path = PathBuf::from(format!("{}{suffix}", prefix.display()));
        let mut builder = std::fs::DirBuilder::new();
        #[cfg(unix)]
        std::os::unix::fs::DirBuilderExt::mode(&mut builder, 0o700);
        match builder.create(&path) {
            Ok(()) => return Ok(path),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(e) => {
                let template = PathBuf::from(format!("{}XXXXXX", prefix.display()));
                return Err(NodeError::fs(&e, "mkdtemp", Some(&template)));
            }
        }
    }
}

/// `captureName`: a file name that cannot leave its directory or name a Windows device.
pub fn capture_name(name: &str) -> String {
    let mut units: Vec<char> = Vec::new();
    for c in name.chars() {
        if c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-') {
            units.push(c);
        } else {
            // One `_` per UTF-16 code unit, like the regex replacement.
            units.extend(std::iter::repeat_n('_', c.len_utf16()));
        }
    }
    let start = units.len().saturating_sub(160);
    let sanitized: String = units[start..].iter().collect();
    if sanitized.is_empty() || sanitized == "." || sanitized == ".." || device(&sanitized) {
        return "capture".into();
    }
    sanitized
}

/// `/^(?:con|prn|aux|nul|com[1-9]|lpt[1-9])(?:\..*)?$/i`
fn device(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    let stem = lower.split('.').next().unwrap_or("");
    matches!(stem, "con" | "prn" | "aux" | "nul")
        || (stem.len() == 4 && (stem.starts_with("com") || stem.starts_with("lpt")) && matches!(stem.as_bytes()[3], b'1'..=b'9'))
}

/// `failure(operation, error)`: the tool error for a server-side file operation.
fn failure(operation: &str, error: NodeError) -> ToolError {
    let detail = js_slice(&error.message, 400);
    let code = match &error.code {
        Some(code) if !detail.starts_with(code.as_str()) => format!("{code}: "),
        _ => String::new(),
    };
    let recovery = if operation == "save" {
        "The browser may have completed the capture, but no server-local export is confirmed. Check free space and write access on the server. Use browser.files.list({tabID}) and browser.files.get({tabID,fileID}) to retrieve an existing completed capture instead of repeating its browser action."
    } else {
        "Upload paths are on the server, not the desktop. Check that each path exists, is a file, and is readable on the server; correct paths or select smaller files before retrying."
    };
    ToolError::with_cause(
        format!("Cannot {operation} browser files on the server. {recovery} Details: {code}{detail}"),
        ErrorCause::Error(error.message),
    )
}

/// `text.slice(0, n)` in UTF-16 code units.
fn js_slice(text: &str, n: usize) -> String {
    let mut units = 0;
    let mut out = String::new();
    for c in text.chars() {
        units += c.len_utf16();
        if units > n {
            break;
        }
        out.push(c);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn capture_names_never_escape_or_name_devices() {
        for (input, output) in [
            ("..", "capture"),
            (".", "capture"),
            ("CON.txt", "capture"),
            ("lpt1", "capture"),
            ("report.html", "report.html"),
            ("", "capture"),
            ("a/b\\c d.png", "a_b_c_d.png"),
            ("é.png", "_.png"),
            ("😀.png", "__.png"),
            ("com1.tar.gz", "capture"),
            ("nul", "capture"),
            ("conx", "conx"),
            ("com0", "com0"),
            ("../CON.txt", ".._CON.txt"),
        ] {
            assert_eq!(capture_name(input), output, "{input}");
        }
        let long = format!("{}.png", "x".repeat(200));
        assert_eq!(capture_name(&long).len(), 160);
        assert!(capture_name(&long).ends_with(".png"));
    }

    #[test]
    fn saved_captures_stay_in_their_directories() {
        let file = |name: &str| File {
            id: format!("file_{}", uuid::Uuid::new_v4()),
            name: name.into(),
            mime: "text/plain".into(),
            data: name.as_bytes().to_vec(),
        };
        let saved = save(&[file(".."), file("."), file("CON.txt"), file("lpt1"), file("report.html")]).unwrap();
        let names: Vec<String> = saved.iter().map(|s| s.path.file_name().unwrap().to_string_lossy().into_owned()).collect();
        assert_eq!(names, ["capture", "capture", "capture", "capture", "report.html"]);
        assert_eq!(saved.iter().map(|s| s.name.as_str()).collect::<Vec<_>>(), ["..", ".", "CON.txt", "lpt1", "report.html"]);
        assert_eq!(std::fs::read_to_string(&saved[0].path).unwrap(), "..");
        let root = saved[0].path.parent().unwrap().parent().unwrap().to_path_buf();
        assert!(root.file_name().unwrap().to_string_lossy().starts_with("opencode-browser-"));
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn uploads_report_server_paths_and_limits() {
        let dir = std::env::temp_dir().join(format!("ocd-browser-files-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(dir.join("sub")).unwrap();
        std::fs::write(dir.join("a.PNG"), b"png").unwrap();
        let files = read(&["a.PNG".into()], &dir).unwrap();
        assert_eq!((files[0].name.as_str(), files[0].mime.as_str(), files[0].data.as_slice()), ("a.PNG", "image/png", &b"png"[..]));
        assert!(files[0].id.starts_with("file_") && files[0].id.len() == 41);
        let missing = read(&["missing.txt".into()], &dir).unwrap_err();
        assert_eq!(
            missing.message,
            format!(
                "Cannot read browser files on the server. Upload paths are on the server, not the desktop. Check that each path exists, is a file, and is readable on the server; correct paths or select smaller files before retrying. Details: ENOENT: no such file or directory, open '{}'",
                dir.join("missing.txt").display()
            )
        );
        let directory = read(&["sub".into()], &dir).unwrap_err();
        assert!(directory.message.ends_with("Details: Upload paths must name files, not directories. Select a server-local file."));
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn node_path_helpers() {
        assert_eq!(basename("dir/file.txt"), "file.txt");
        assert_eq!(basename("dir/"), "dir");
        assert_eq!(extname("a/b.tar.GZ"), ".GZ");
        assert_eq!(extname(".bashrc"), "");
        assert_eq!(extname("file."), ".");
        for (input, ext) in
            [("..x", ".x"), ("a/..x", ".x"), ("...", "."), ("..", ""), ("x..", "."), ("/", ""), ("", ""), (".a.b", ".b"), ("dir/", "")]
        {
            assert_eq!(extname(input), ext, "{input}");
        }
    }
}

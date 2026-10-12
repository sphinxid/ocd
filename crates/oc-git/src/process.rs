//! Minimal `AppProcess.run` equivalent for the `git` executable.

use std::path::Path;
use std::process::Stdio;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::process::Command;

/// `util/git-executable.ts`: `git` everywhere (on Windows `Command` searches `PATH` itself).
pub const GIT_EXECUTABLE: &str = "git";

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
    let mut command = Command::new(GIT_EXECUTABLE);
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

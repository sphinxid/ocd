//! Port of `src/util/system.ts`: OS and terminal descriptions for the debug dialog.

/// Node's `os.release()`: the kernel release (`uname -r`) on Unix.
fn release() -> String {
    #[cfg(unix)]
    {
        // SAFETY: `uname` fills a caller-owned, zero-initialized struct.
        unsafe {
            let mut u: libc::utsname = std::mem::zeroed();
            if libc::uname(&mut u) == 0 {
                return std::ffi::CStr::from_ptr(u.release.as_ptr()).to_string_lossy().into_owned();
            }
        }
        String::new()
    }
    #[cfg(not(unix))]
    {
        std::process::Command::new("cmd")
            .args(["/c", "ver"])
            .output()
            .ok()
            .and_then(|o| {
                let s = String::from_utf8_lossy(&o.stdout).to_string();
                s.split("Version ").nth(1).map(|v| v.trim().trim_end_matches(']').to_string())
            })
            .unwrap_or_default()
    }
}

/// Node's `process.arch` for this build.
fn arch() -> &'static str {
    match std::env::consts::ARCH {
        "x86_64" => "x64",
        "aarch64" => "arm64",
        "x86" => "ia32",
        "arm" => "arm",
        other => other,
    }
}

/// `describeOS()`: `Linux 6.8.0 (x64)`, `macOS 24.1.0 (arm64)`, `Windows 10.0.26100 (x64, emulated on ARM64)`.
pub fn describe_os() -> String {
    let name = match std::env::consts::OS {
        "macos" => "macOS",
        "windows" => "Windows",
        "linux" => "Linux",
        other => other,
    };
    let emulated = cfg!(windows) && arch() == "x64" && std::env::var("PROCESSOR_IDENTIFIER").is_ok_and(|p| p.starts_with("ARM"));
    format!("{name} {} ({}{})", release(), arch(), if emulated { ", emulated on ARM64" } else { "" })
}

/// `describeTerminal()`: `TERM_PROGRAM` (or `TERM`) with its version and multiplexer.
pub fn describe_terminal() -> String {
    let env = |k: &str| std::env::var(k).ok().filter(|v| !v.is_empty());
    let program = env("TERM_PROGRAM").or_else(|| env("TERM")).unwrap_or_else(|| "unknown".into());
    let version = env("TERM_PROGRAM_VERSION").map(|v| format!(" {v}")).unwrap_or_default();
    let multiplexer = if env("TMUX").is_some() {
        " in tmux"
    } else if env("STY").is_some() {
        " in screen"
    } else {
        ""
    };
    format!("{program}{version}{multiplexer}")
}

#[cfg(test)]
mod tests {
    #[test]
    fn os_description_shape() {
        let d = super::describe_os();
        assert!(d.ends_with(')'), "{d}");
        assert!(d.contains(" ("), "{d}");
    }
}

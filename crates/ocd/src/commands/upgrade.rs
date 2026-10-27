//! `upgrade` / `uninstall` (`handlers/upgrade.ts`, `uninstall.ts`, `services/updater.ts`).
//!
//! Native-binary adaptation: method detection recognises the curl install location
//! (`~/.opencode/bin/opencode`), Homebrew cellars and an npm-style `package.json` next to the
//! executable; anything else (e.g. a `cargo build` binary) is "unknown" and needs `--method`.
//! The release lookup, package-manager commands and error formatting follow upstream.

use std::path::{Path, PathBuf};

use oc_util::Global;
use serde_json::Value;

use super::auth::{Flow, handle};
use crate::cli::parse::Input;
use crate::service;
use crate::ui;

fn home() -> PathBuf {
    std::env::var_os("OPENCODE_TEST_HOME").map(PathBuf::from).or_else(std::env::home_dir).unwrap_or_default()
}

fn exe() -> PathBuf {
    std::env::current_exe().unwrap_or_default()
}

/// `executable`: the real path of the running binary.
fn executable() -> PathBuf {
    let exe = exe();
    std::fs::canonicalize(&exe).unwrap_or(exe)
}

/// WinGet's package identifier and the arguments every `winget` call passes.
const WINGET_PACKAGE: &str = "Anomaly.OpenCode";
const WINGET_ARGS: &[&str] =
    &["--id", WINGET_PACKAGE, "--exact", "--source", "winget", "--accept-source-agreements", "--disable-interactivity"];

/// `installedWinget`: WinGet extracts portable packages into a directory named after the package identifier.
fn installed_winget(executable: &Path) -> bool {
    regex::Regex::new(r"(?i)[\\/]winget[\\/]packages[\\/]anomaly\.opencode_").unwrap().is_match(&executable.to_string_lossy())
}

/// The package-manager probes, the ones named in the executable's path first (`checks.sort`).
fn probe_order(executable: &Path) -> Vec<&'static str> {
    let parts: Vec<String> = executable
        .to_string_lossy()
        .to_lowercase()
        .split(std::path::MAIN_SEPARATOR)
        .map(|p| p.strip_prefix('.').unwrap_or(p).to_string())
        .collect();
    let mut order = vec!["npm", "pnpm", "bun", "yarn", "vp"];
    order.sort_by_key(|m| !parts.iter().any(|p| p == m));
    order
}

/// `installedPackage`: the npm package owning the executable, if any.
fn installed_package() -> Option<String> {
    let real = std::fs::canonicalize(exe()).ok()?;
    let pkg = real.parent()?.parent()?.join("package.json");
    let v: Value = serde_json::from_str(&std::fs::read_to_string(&pkg).ok()?).ok()?;
    let name = v.get("name")?.as_str()?.to_string();
    let re = regex::Regex::new(r"^@opencode(?:-ai)?/cli(?:-node)?$").unwrap();
    if !re.is_match(&name) {
        return None;
    }
    let dir = pkg.parent()?;
    let owns = match v.get("bin") {
        Some(Value::String(s)) => std::fs::canonicalize(dir.join(s)).ok().as_ref() == Some(&real),
        Some(Value::Object(m)) => m.values().filter_map(Value::as_str).any(|s| std::fs::canonicalize(dir.join(s)).ok().as_ref() == Some(&real)),
        _ => false,
    };
    owns.then_some(name)
}

fn brew_formula() -> Option<String> {
    let real = std::fs::canonicalize(exe()).ok()?.display().to_string();
    ["opencode", "opencode-beta", "opencode-v2"].iter().find(|f| real.contains(&format!("/Cellar/{f}/"))).map(|f| f.to_string())
}

/// `updater.method()`
pub fn detect_method() -> Option<String> {
    let executable = executable();
    let curl_path = home().join(".opencode").join("bin").join(if cfg!(windows) { "opencode.exe" } else { "opencode" });
    let curl = std::fs::canonicalize(&curl_path).unwrap_or(curl_path);
    if executable == curl {
        return Some("curl".into());
    }
    if brew_formula().is_some() {
        return Some("brew".into());
    }
    if installed_winget(&executable) {
        return Some("winget".into());
    }
    let pkg = installed_package()?;
    let check = |cmd: &str, args: &[&str]| -> Option<String> {
        let out = std::process::Command::new(cmd).args(args).stderr(std::process::Stdio::null()).output().ok()?;
        out.status.success().then(|| String::from_utf8_lossy(&out.stdout).into_owned())
    };
    for method in probe_order(&executable) {
        let found = match method {
            "npm" | "pnpm" => check(method, &["list", "-g", "--depth=0", &pkg]).is_some_and(|o| o.contains(&pkg)),
            "bun" => check("bun", &["pm", "ls", "-g"]).is_some_and(|o| o.contains(&pkg)),
            "yarn" => check("yarn", &["global", "list"]).is_some_and(|o| o.contains(&pkg)),
            _ => check("vp", &["list", "-g", "--json", &pkg]).is_some_and(|o| {
                serde_json::from_str::<Vec<Value>>(&o)
                    .ok()
                    .is_some_and(|a| a.iter().any(|x| x.get("name").and_then(Value::as_str) == Some(&pkg)))
            }),
        };
        if found {
            return Some(method.into());
        }
    }
    None
}

/// `conciseDetail`: the last 12 non-empty lines (at most 2,000 chars) of a command's output.
fn concise(text: &str) -> String {
    let stripped = crate::cli::help::strip_ansi(text);
    let lines: Vec<&str> =
        stripped.trim().split(['\r', '\n']).map(str::trim_end).filter(|l| !l.trim().is_empty()).collect();
    let tail = lines[lines.len().saturating_sub(12)..].join("\n");
    let count = tail.chars().count();
    let clipped = count > 2_000;
    let detail = if clipped { format!("…{}", tail.chars().skip(count - 1_999).collect::<String>()) } else { tail };
    if detail.is_empty() || (lines.len() <= 12 && !clipped) {
        return detail;
    }
    format!("{detail}\n\nOutput shortened to the last 12 lines.")
}

/// `resultDetail`
fn detail_of(o: &std::process::Output) -> String {
    let stderr = concise(&String::from_utf8_lossy(&o.stderr));
    if !stderr.is_empty() {
        return stderr;
    }
    let stdout = concise(&String::from_utf8_lossy(&o.stdout));
    if !stdout.is_empty() {
        return stdout;
    }
    format!("The command exited with code {} without any error output.", o.status.code().unwrap_or(1))
}

/// ocd is built from source, so it never downloads an upstream opencode release over itself.
pub const UPGRADE_FROM_SOURCE: &str = "ocd is built from source; rebuild it to upgrade (cargo build --release -p ocd).";

pub async fn upgrade(_global: &Global, _input: &Input) -> anyhow::Result<i32> {
    ui::intro("Upgrade");
    ui::log::info(UPGRADE_FROM_SOURCE);
    ui::outro("Done");
    Ok(0)
}

fn removal(method: Option<&str>) -> Option<Vec<String>> {
    if method == Some("winget") {
        return Some(["winget", "uninstall"].iter().chain(WINGET_ARGS).map(|a| a.to_string()).collect());
    }
    let pkg = installed_package()?;
    let c: Vec<&str> = match method? {
        "npm" => vec!["npm", "uninstall", "--global"],
        "pnpm" => vec!["pnpm", "remove", "--global"],
        "bun" => vec!["bun", "remove", "--global"],
        "yarn" => vec!["yarn", "global", "remove"],
        "vp" => vec!["vp", "uninstall", "-g"],
        _ => return None,
    };
    Some(c.into_iter().map(String::from).chain([pkg]).collect())
}

fn shell_configs() -> Vec<PathBuf> {
    let home = home();
    let xdg = std::env::var_os("XDG_CONFIG_HOME").filter(|v| !v.is_empty()).map(PathBuf::from).unwrap_or_else(|| home.join(".config"));
    let shell = std::env::var("SHELL").ok().filter(|s| !s.is_empty()).unwrap_or_else(|| "bash".into());
    let name = Path::new(&shell).file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    let zdot = std::env::var_os("ZDOTDIR").map(PathBuf::from).unwrap_or_else(|| home.clone());
    let mut list = match name.as_str() {
        "fish" => vec![home.join(".config/fish/config.fish"), xdg.join("fish/config.fish")],
        "zsh" => vec![zdot.join(".zshrc"), zdot.join(".zshenv"), xdg.join("zsh/.zshrc"), xdg.join("zsh/.zshenv")],
        "ash" | "sh" => vec![home.join(".ashrc"), home.join(".profile")],
        _ => vec![home.join(".bashrc"), home.join(".bash_profile"), home.join(".profile"), xdg.join("bash/.bashrc"), xdg.join("bash/.bash_profile")],
    };
    let mut seen = std::collections::HashSet::new();
    list.retain(|p| seen.insert(p.clone()));
    list.into_iter()
        .filter(|f| std::fs::read_to_string(f).ok().is_some_and(|t| clean_shell_config(&t) != t))
        .collect()
}

/// `cleanShellConfig`
pub fn clean_shell_config(text: &str) -> String {
    let lines: Vec<&str> = text.split('\n').collect();
    let opencode = |l: &str| {
        let t = l.trim();
        (t.starts_with("export PATH=") || t.starts_with("fish_add_path ")) && t.contains(".opencode/bin")
    };
    let mut out = vec![];
    for (i, l) in lines.iter().enumerate() {
        if opencode(l) {
            continue;
        }
        if l.trim() == "# opencode" && lines.get(i + 1).is_some_and(|n| opencode(n)) {
            continue;
        }
        out.push(*l);
    }
    out.join("\n")
}

pub async fn uninstall(global: &Global, input: &Input) -> anyhow::Result<i32> {
    Ok(handle(
        async {
            ui::intro("Uninstall OpenCode");
            let method = detect_method();
            let remove_cmd = removal(method.as_deref());
            let services: Vec<PathBuf> = std::fs::read_dir(&global.state)
                .map(|rd| {
                    let re = regex::Regex::new(r"^service(?:-.*)?\.json$").unwrap();
                    let mut v: Vec<PathBuf> = rd
                        .filter_map(|e| e.ok())
                        .filter(|e| re.is_match(&e.file_name().to_string_lossy()))
                        .map(|e| e.path())
                        .collect();
                    v.sort();
                    v
                })
                .unwrap_or_default();
            let exe = exe();
            let shared_bin = exe.parent().and_then(|d| std::fs::read_dir(d).ok()).is_some_and(|rd| rd.count() > 1);
            let shells = if method.as_deref() == Some("curl") && !shared_bin { shell_configs() } else { vec![] };
            ui::log::info(&format!("Installation method: {}", method.as_deref().unwrap_or("unknown")));
            let mut lines = vec!["The following global files will be removed (shared by OpenCode versions and channels):".to_string()];
            if global.cache.exists() {
                lines.push(format!("  Cache: {}", global.cache.display()));
            }
            for s in &services {
                lines.push(format!("  Stop background service and persistent terminals: {}", s.display()));
            }
            for s in &shells {
                lines.push(format!("  Shell PATH: {}", s.display()));
            }
            if let Some(c) = &remove_cmd {
                lines.push(format!("  Package: {}", c.join(" ")));
            }
            if method.as_deref() == Some("curl") {
                lines.push(format!("  Binary (manual removal): {}", exe.display()));
            }
            ui::log::message(&lines.join("\n"));
            if method.is_none() {
                ui::log::warn("Could not detect the installation method. Remove the installation manually after cleanup.");
            }
            if input.flag("dryRun") {
                ui::log::warn("Dry run - no changes made");
                ui::outro("Done");
                return Ok::<(), Flow>(());
            }
            if !input.flag("force") {
                if !ui::interactive() {
                    return Err(Flow::Error(anyhow::anyhow!(
                        "Use --force to uninstall without an interactive terminal, or --dry-run to preview."
                    )));
                }
                if !ui::confirm("Are you sure you want to uninstall?", false)? {
                    ui::outro("Cancelled");
                    return Ok(());
                }
            }
            let sp = ui::Spinner::start("Stopping background services...");
            for s in &services {
                service::shutdown_persistent_pty(s).await;
                if let Err(e) = service::stop(s, service::Pty::Clear).await {
                    sp.stop("Failed to stop background services", 1);
                    return Err(e.into());
                }
            }
            sp.stop("Background services stopped", 0);
            let mut errors = vec![];
            let sp = ui::Spinner::start("Removing Cache...");
            match std::fs::remove_dir_all(&global.cache) {
                Ok(()) => sp.stop("Removed Cache", 0),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => sp.stop("Removed Cache", 0),
                Err(e) => {
                    sp.stop("Failed to remove Cache", 1);
                    errors.push(format!("Cache: {e}"));
                }
            }
            for f in &shells {
                if let Err(e) = std::fs::read_to_string(f).and_then(|t| std::fs::write(f, clean_shell_config(&t))) {
                    errors.push(format!("Shell config {}: {e}", f.display()));
                }
            }
            if let Some(c) = &remove_cmd {
                let display = c.join(" ");
                let sp = ui::Spinner::start(&format!("Running {display}..."));
                match tokio::process::Command::new(&c[0]).args(&c[1..]).output().await {
                    Ok(o) if o.status.success() => sp.stop("Package removed", 0),
                    other => {
                        sp.stop("Package manager uninstall failed", 1);
                        errors.push(match other {
                            Ok(o) => detail_of(&o),
                            Err(e) => e.to_string(),
                        });
                        ui::log::warn(&format!("Run manually: {display}"));
                    }
                }
            }
            if method.as_deref() == Some("curl") {
                ui::log::message("To finish removing the binary, run:");
                ui::log::info(&format!("  rm '{}'", exe.display().to_string().replace('\'', "'\\''")));
            }
            if !errors.is_empty() {
                return Err(Flow::Error(anyhow::anyhow!(errors.join("\n"))));
            }
            ui::outro("Done");
            Ok(())
        }
        .await,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;


    #[test]
    fn winget_installs() {
        let p = |s: &str| PathBuf::from(s);
        assert!(installed_winget(&p(
            r"C:\Users\u\AppData\Local\Microsoft\WinGet\Packages\Anomaly.OpenCode_Microsoft.Winget.Source_8wekyb3d8bbwe\opencode.exe"
        )));
        assert!(installed_winget(&p("/x/winget/packages/anomaly.opencode_abc/opencode")));
        assert!(!installed_winget(&p("/usr/local/bin/opencode")));
        assert_eq!(
            removal(Some("winget")).unwrap().join(" "),
            "winget uninstall --id Anomaly.OpenCode --exact --source winget --accept-source-agreements --disable-interactivity"
        );
    }

    #[test]
    fn probes_start_with_the_manager_in_the_path() {
        let sep = std::path::MAIN_SEPARATOR;
        let path = PathBuf::from(format!("{sep}home{sep}u{sep}.bun{sep}install{sep}global{sep}node_modules{sep}x{sep}bin{sep}opencode"));
        assert_eq!(probe_order(&path), ["bun", "npm", "pnpm", "yarn", "vp"]);
        let path = PathBuf::from(format!("{sep}usr{sep}lib{sep}node_modules{sep}opencode"));
        assert_eq!(probe_order(&path), ["npm", "pnpm", "bun", "yarn", "vp"]);
    }

    #[test]
    fn shell_cleanup() {
        let t = "a\n# opencode\nexport PATH=/home/u/.opencode/bin:$PATH\nb";
        assert_eq!(clean_shell_config(t), "a\nb");
    }

}

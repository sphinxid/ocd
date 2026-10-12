//! Skills (`core/skill.ts`, `core/plugin/skill.ts`, `core/config/plugin/skill.ts`, compatibility roots).

use std::path::PathBuf;

use indexmap::IndexMap;
use oc_config::sources::{Skill, SkillSource};

use crate::location::Location;

const OPENCODE_CONTENT: &str = include_str!("../../../assets/skill/opencode.md");
const REPORT_CONTENT: &str = include_str!("../../../assets/skill/report.md");
pub const OPENCODE_DESCRIPTION: &str = "Use this skill for any question about OpenCode itself, including how OpenCode works, using or configuring it, migrating from V1 to V2, troubleshooting it, developing plugins or integrations, using the OpenCode SDK, clients, server, or API, and contributing to the OpenCode codebase. Also use it for OpenCode agents, commands, skills, tools, permissions, MCP servers, providers, models, themes, keybinds, formatters, the CLI, TUI, desktop app, and web app.";
const REPORT_DESCRIPTION: &str = "Use when the user wants to report an opencode issue or bug. Collect standard diagnostics, add user-specific reproduction context, and publish the issue with GitHub CLI.";

fn os_type() -> &'static str {
    match std::env::consts::OS {
        "linux" => "Linux",
        "macos" => "Darwin",
        "windows" => "Windows_NT",
        other => other,
    }
}

fn os_release() -> String {
    std::fs::read_to_string("/proc/sys/kernel/osrelease").map(|s| s.trim().to_string()).unwrap_or_default()
}

fn arch() -> &'static str {
    match std::env::consts::ARCH {
        "x86_64" => "x64",
        "aarch64" => "arm64",
        other => other,
    }
}

fn report_content(version: &str, channel: &str, plugins: &[String]) -> String {
    let terminal = ["TERM_PROGRAM", "TERM", "COLORTERM"]
        .iter()
        .filter_map(|k| std::env::var(k).ok().map(|v| format!("{k}={v}")))
        .collect::<Vec<_>>()
        .join(", ");
    let terminal = if terminal.is_empty() { "Unavailable: terminal environment variables are not set".to_string() } else { terminal };
    let shell = std::env::var("SHELL")
        .or_else(|_| std::env::var("ComSpec"))
        .or_else(|_| std::env::var("COMSPEC"))
        .unwrap_or_else(|_| "Unavailable: shell environment variable is not set".into());
    let mut sorted = plugins.to_vec();
    sorted.sort();
    [
        REPORT_CONTENT.to_string(),
        String::new(),
        "## Runtime Diagnostics Snapshot".into(),
        String::new(),
        "These values were captured when the built-in report skill was registered. Verify them before publishing.".into(),
        String::new(),
        format!("- opencode version: {version}"),
        format!("- install/channel: {channel}"),
        format!("- OS: {} {} ({} {})", os_type(), os_release(), crate::instructions::js_platform(), arch()),
        format!("- Terminal: {terminal}"),
        format!("- Shell: {shell}"),
        format!("- Active plugins: {}", if sorted.is_empty() { "None found in config".to_string() } else { sorted.join(", ") }),
    ]
    .join("\n")
}

/// All skills for a location, later sources replacing earlier ones by id.
pub async fn list(location: &Location, global: &oc_util::Global, version: &str) -> Vec<Skill> {
    let home = &global.home;
    let mut out: IndexMap<String, Skill> = IndexMap::new();
    let plugins: Vec<String> = oc_config::fold::plugins(&location.entries)
        .iter()
        .map(|p| serde_json::to_value(p).ok().and_then(|v| v.as_str().map(str::to_string).or_else(|| v["package"].as_str().map(str::to_string))).unwrap_or_default())
        .collect();
    for (id, name, description, path, content) in [
        ("opencode", "OpenCode", OPENCODE_DESCRIPTION, "/builtin/opencode.md", OPENCODE_CONTENT.to_string()),
        ("report", "Report", REPORT_DESCRIPTION, "/builtin/report.md", report_content(version, oc_util::version::CHANNEL, &plugins)),
    ] {
        out.insert(
            id.into(),
            Skill { id: id.into(), name: name.into(), description: Some(description.into()), autoinvoke: None, path: PathBuf::from(path), content },
        );
    }
    // Compatibility roots register before config sources, so configured skills win (`plugin/internal.ts`).
    let (claude, agents) = location.sources.compatibility();
    for s in oc_config::sources::compatibility_skills(&claude, &agents) {
        out.insert(s.id.clone(), s);
    }
    for source in oc_config::sources::skill_sources(&location.entries, home, &location.directory) {
        let dirs = match source {
            SkillSource::Directory { path } => vec![path],
            SkillSource::Url { url } => crate::skill_discovery::pull_cached(&global.cache, &url).await,
        };
        for path in dirs {
            let Ok(dir) = std::fs::canonicalize(&path) else { continue };
            for s in oc_config::sources::scan_skills(&dir) {
                out.insert(s.id.clone(), s);
            }
        }
    }
    out.into_values().collect()
}

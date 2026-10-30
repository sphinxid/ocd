//! User-visible strings of `opencode mcp list/add/auth/logout`
//! (`packages/cli/src/commands/handlers/mcp/*.ts`, spec 01 §3.9-3.11).

use crate::types::{ServerInfo, Status};

pub const MCP_DESCRIPTION: &str = "Manage MCP (Model Context Protocol) servers";
pub const LIST_DESCRIPTION: &str = "List configured MCP servers and their status";
pub const ADD_DESCRIPTION: &str = "Add an MCP server to your configuration";
pub const AUTH_DESCRIPTION: &str = "Authenticate with an OAuth-capable remote MCP server";
pub const LOGOUT_DESCRIPTION: &str = "Remove stored OAuth credentials for an MCP server";

pub const NO_SERVERS: &str = "No MCP servers configured";
pub const ADD_BOTH: &str = "Provide either --url <url> or a command after --, not both";
pub const ADD_NEITHER: &str = "Provide either --url <url> or a command after --";
pub const ADD_ENV_REMOTE: &str = "--env is only valid for local MCP servers";
pub const ADD_HEADER_LOCAL: &str = "--header is only valid for remote MCP servers";
pub const AUTH_NEEDS_NAME: &str = "Pass an MCP server name when running without an interactive terminal";
pub const AUTH_INTRO: &str = "Authenticate an MCP server";
pub const AUTH_NONE: &str = "No OAuth-capable MCP servers configured";
pub const AUTH_NONE_HINT: &str =
    "Remote MCP servers support OAuth by default. Add one with `opencode mcp add` or in opencode.json:";
pub const AUTH_NONE_EXAMPLE: &str = "  \"mcp\": {\n    \"servers\": {\n      \"my-server\": {\n        \"type\": \"remote\",\n        \"url\": \"https://example.com/mcp\"\n      }\n    }\n  }";

pub fn invalid_url(url: &str) -> String {
    format!("Invalid URL: {url}")
}

pub fn added(name: &str, path: &str) -> String {
    format!("MCP server \"{name}\" added to {path}")
}

pub fn not_found(name: &str) -> String {
    format!("MCP server not found: {name}")
}

pub fn not_oauth_capable(name: &str) -> String {
    format!("MCP server \"{name}\" is not an OAuth-capable remote server")
}

pub fn expired_credentials(name: &str) -> String {
    format!("{name} has expired credentials. Re-authenticating...")
}

pub fn reauthenticate_confirm(name: &str) -> String {
    format!("{name} already has valid credentials. Re-authenticate?")
}

pub fn no_credentials(name: &str) -> String {
    format!("No stored credentials for {name}")
}

pub fn removed_credentials(name: &str) -> String {
    format!("Removed OAuth credentials for {name}")
}

/// `(icon, description)` for one status in `mcp list`.
pub fn status_line_parts(status: &Status) -> (&'static str, String) {
    match status {
        Status::Connected => ("✓", "connected".into()),
        Status::NeedsAuth { .. } => ("⚠", "needs authentication".into()),
        Status::Failed { error } => ("✗", format!("failed: {error}")),
        other => ("○", other.as_str().into()),
    }
}

/// The `mcp list` output (without trailing newline): servers sorted by name, names padded to
/// the widest.
pub fn format_list(servers: &[ServerInfo]) -> String {
    if servers.is_empty() {
        return NO_SERVERS.to_string();
    }
    let mut servers = servers.to_vec();
    servers.sort_by(|a, b| crate::manager::locale_cmp(&a.name, &b.name));
    let width = servers.iter().map(|s| s.name.encode_utf16().count()).max().unwrap_or(0);
    servers
        .iter()
        .map(|s| {
            let (icon, description) = status_line_parts(&s.status);
            let pad = width - s.name.encode_utf16().count();
            format!("{icon} {}{}  {description}", s.name, " ".repeat(pad))
        })
        .collect::<Vec<_>>()
        .join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn list_format() {
        let servers = vec![
            ServerInfo { name: "zeta".into(), status: Status::failed("boom"), title: None, integration_id: None },
            ServerInfo { name: "a".into(), status: Status::Connected, title: None, integration_id: None },
            ServerInfo { name: "mid".into(), status: Status::needs_auth("x"), title: None, integration_id: None },
            ServerInfo { name: "off".into(), status: Status::Disabled, title: None, integration_id: None },
        ];
        assert_eq!(
            format_list(&servers),
            "✓ a     connected\n⚠ mid   needs authentication\n○ off   disabled\n✗ zeta  failed: boom"
        );
        assert_eq!(format_list(&[]), "No MCP servers configured");
    }
}

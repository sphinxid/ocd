//! Build-time constants (`cli/src/version.ts`).

/// Reported version. Override at build time with `OPENCODE_VERSION`.
pub const VERSION: &str = match option_env!("OPENCODE_VERSION") {
    Some(v) => v,
    None => env!("CARGO_PKG_VERSION"),
};

/// The ocd build, e.g. `1.2.0+abc1234` (release tag and commit), set by the release workflow
/// with `OCD_BUILD_VERSION`. Only `--version` and the log show it; everything opencode-compatible
/// (user agent, plugins, service registration) keeps [`VERSION`].
pub const BUILD: Option<&str> = match option_env!("OCD_BUILD_VERSION") {
    Some(v) if !v.is_empty() => Some(v),
    _ => None,
};

/// Release channel. Override at build time with `OPENCODE_CHANNEL`.
pub const CHANNEL: &str = match option_env!("OPENCODE_CHANNEL") {
    Some(v) => v,
    None => "latest",
};

pub const ARTIFACT: &str = "cli";
pub const CLI_NAME: &str = "opencode";

pub fn local() -> bool {
    CHANNEL == "local"
}

/// Channels that share files/ports with releases.
pub fn is_release_channel(channel: &str) -> bool {
    matches!(channel, "latest" | "dev" | "beta" | "next")
}

/// `channel.replace(/[^a-zA-Z0-9._-]/g, "-")`
pub fn sanitize(channel: &str) -> String {
    channel
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-') { c } else { '-' })
        .collect()
}

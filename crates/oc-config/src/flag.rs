//! Feature flags (`schema/src/config/flag.ts`, `core/src/flag.ts`, 2.2.7): one registry of opt-in features, each
//! read with [`enabled`]. A flag is on in local and dev builds and off in released ones; `experimental.<key>` in
//! config and `OPENCODE_FLAGS` override that, and `OPENCODE_FLAGS=all` or `experimental.all_flags: true` turn every
//! flag on.

use indexmap::IndexMap;

use crate::schema::Entry;

/// The `OPENCODE_FLAGS` entry that turns every flag on, as in `all,-media_blob`. It is not a flag key.
pub const WILDCARD: &str = "all";

/// Every feature flag, by key, with what it turns on (`ConfigFlag.registry`, in its order).
pub const REGISTRY: &[(&str, &str)] = &[
    ("todo", "Enable the experimental todowrite tool."),
    (
        "background_compaction",
        "Enable experimental background compaction: summarize the conversation in the background as it nears the compaction threshold, so automatic compaction does not wait for the summary. Background summaries that are never used still cost tokens.",
    ),
    ("media_blob", "Store new session media once in a deduplicated blob table instead of inline in every message row."),
    ("schedules", "Enable experimental scheduled tasks that wake a session with a prompt."),
    ("artifacts", "Enable experimental artifacts backed by pluggable providers."),
    ("codemode_vm", "Run the Code Mode execute tool on the experimental bytecode VM instead of the legacy interpreter."),
    (
        "console_watches",
        "Watch Slack threads, GitHub pull requests, issues, commits and Actions runs, and the workspace's MCP servers' events through OpenCode Console, and wake sessions when they change.",
    ),
];

/// Where a flag's value came from, highest priority first.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Source {
    Env,
    Config,
    Default,
}

impl Source {
    pub fn as_str(self) -> &'static str {
        match self {
            Source::Env => "env",
            Source::Config => "config",
            Source::Default => "default",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct State {
    pub enabled: bool,
    pub source: Source,
}

/// Any other channel, such as a release or a branch preview build, keeps flags off so none turns on by accident.
fn on_by_default(channel: &str) -> bool {
    channel == "local" || channel == "dev"
}

/// `Flag.resolve`: `OPENCODE_FLAGS` (`-key`, then `all`), then `experimental.<key>` from the highest-priority
/// document that sets `experimental`, then `all_flags: true` from the highest-priority document that sets `all_flags`
/// (`false` falls through rather than turning flags off), then on for local and dev builds.
pub fn resolve(entries: &[Entry], channel: &str, overrides: Option<&str>) -> IndexMap<&'static str, State> {
    let mut env: IndexMap<&str, bool> = IndexMap::new();
    for item in overrides.unwrap_or_default().split(',').map(str::trim).filter(|s| !s.is_empty()) {
        match item.strip_prefix('-') {
            Some(key) => env.insert(key, false),
            None => env.insert(item, true),
        };
    }
    let all = env.get(WILDCARD) == Some(&true);
    let experimental = crate::fold::experimental(entries);
    let all_flags =
        crate::fold::latest(entries, |i| i.experimental.as_ref().and_then(|e| e.all_flags.as_ref())) == Some(&true);
    REGISTRY
        .iter()
        .map(|(key, _)| {
            let override_ = env.get(key).copied().or(all.then_some(true));
            let configured = experimental.and_then(|e| e.flag(key)).or(all_flags.then_some(true));
            let state = match (override_, configured) {
                (Some(enabled), _) => State { enabled, source: Source::Env },
                (None, Some(enabled)) => State { enabled, source: Source::Config },
                (None, None) => State { enabled: on_by_default(channel), source: Source::Default },
            };
            (*key, state)
        })
        .collect()
}

/// `Flag.enabled(key)` with this build's channel and the process's `OPENCODE_FLAGS`.
pub fn enabled(entries: &[Entry], key: &str) -> bool {
    let overrides = std::env::var("OPENCODE_FLAGS").ok();
    resolve(entries, oc_util::version::CHANNEL, overrides.as_deref()).get(key).is_some_and(|s| s.enabled)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn doc(experimental: serde_json::Value) -> Entry {
        let info = <crate::schema::Info as crate::decode::Decode>::decode(&json!({ "experimental": experimental })).expect("config");
        Entry::Document { path: None, info, integration: None, wellknown: None }
    }

    fn get(entries: &[Entry], channel: &str, overrides: Option<&str>, key: &str) -> (bool, &'static str) {
        let s = resolve(entries, channel, overrides)[key];
        (s.enabled, s.source.as_str())
    }

    /// `flag.test.ts`.
    #[test]
    fn resolves_env_then_config_then_all_flags_then_channel() {
        // Default: on only for local and dev builds.
        assert_eq!(get(&[], "latest", None, "todo"), (false, "default"));
        assert_eq!(get(&[], "local", None, "todo"), (true, "default"));
        assert_eq!(get(&[], "dev", None, "schedules"), (true, "default"));
        assert_eq!(get(&[], "detached", None, "schedules"), (false, "default"));
        // Config beats the channel, either way.
        let on = [doc(json!({"todo": true}))];
        assert_eq!(get(&on, "latest", None, "todo"), (true, "config"));
        let off = [doc(json!({"todo": false}))];
        assert_eq!(get(&off, "dev", None, "todo"), (false, "config"));
        // OPENCODE_FLAGS beats config: an explicit key or `-key` wins over `all`.
        assert_eq!(get(&off, "latest", Some("todo"), "todo"), (true, "env"));
        assert_eq!(get(&on, "latest", Some(" all , -todo "), "todo"), (false, "env"));
        assert_eq!(get(&on, "latest", Some("all,-todo"), "media_blob"), (true, "env"));
        assert_eq!(get(&[], "latest", Some("-all"), "media_blob"), (false, "default"));
        // Only the highest-priority document that sets `experimental` counts for per-flag settings...
        let shadowed = [doc(json!({"todo": true})), doc(json!({"subagent_depth": 2}))];
        assert_eq!(get(&shadowed, "latest", None, "todo"), (false, "default"));
        // ...while `all_flags` comes from the highest-priority document that sets it, and `false` falls through.
        let global = [doc(json!({"all_flags": true})), doc(json!({"subagent_depth": 2}))];
        assert_eq!(get(&global, "latest", None, "artifacts"), (true, "config"));
        let explicit = [doc(json!({"all_flags": true})), doc(json!({"artifacts": false}))];
        assert_eq!(get(&explicit, "latest", None, "artifacts"), (false, "config"));
        assert_eq!(get(&explicit, "latest", None, "schedules"), (true, "config"));
        let disabled = [doc(json!({"all_flags": true})), doc(json!({"all_flags": false}))];
        assert_eq!(get(&disabled, "local", None, "schedules"), (true, "default"));
        assert_eq!(get(&disabled, "latest", None, "schedules"), (false, "default"));
        assert_eq!(resolve(&[], "latest", None).keys().copied().collect::<Vec<_>>(), REGISTRY.iter().map(|(k, _)| *k).collect::<Vec<_>>());
    }

    fn every(enabled: bool, source: Source) -> IndexMap<&'static str, State> {
        REGISTRY.iter().map(|(k, _)| (*k, State { enabled, source })).collect()
    }

    fn with(mut map: IndexMap<&'static str, State>, key: &'static str, enabled: bool, source: Source) -> IndexMap<&'static str, State> {
        map.insert(key, State { enabled, source });
        map
    }

    /// `flag.test.ts` `Flag.resolve`, case by case.
    #[test]
    fn upstream_resolve_cases() {
        for (channel, on) in [("local", true), ("dev", true), ("beta", false), ("latest", false), ("my-branch", false), ("unknown", false)] {
            assert_eq!(resolve(&[], channel, None)["todo"].enabled, on, "{channel}");
        }
        assert_eq!(resolve(&[], "latest", None), every(false, Source::Default));
        assert!(resolve(&[], "dev", None)["codemode_vm"].enabled);

        let flags = resolve(&[doc(json!({"todo": true, "schedules": false}))], "dev", Some(" media_blob , -todo,unknown_flag"));
        assert_eq!(
            ["todo", "schedules", "media_blob", "artifacts"].map(|k| (flags[k].enabled, flags[k].source)),
            [(false, Source::Env), (false, Source::Config), (true, Source::Env), (true, Source::Default)]
        );

        let entries = [doc(json!({"media_blob": false}))];
        assert_eq!(resolve(&entries, "latest", Some("all")), every(true, Source::Env));
        assert_eq!(resolve(&[], "latest", Some(" -media_blob , all, ")), with(every(true, Source::Env), "media_blob", false, Source::Env));
        assert_eq!(resolve(&entries, "latest", Some("-all")), resolve(&entries, "latest", None));

        let entries = [doc(json!({"all_flags": true, "media_blob": false}))];
        let expected = with(with(every(true, Source::Config), "media_blob", false, Source::Config), "artifacts", false, Source::Env);
        assert_eq!(resolve(&entries, "latest", Some("-artifacts")), expected);

        assert_eq!(resolve(&[doc(json!({"all_flags": false}))], "dev", None), every(true, Source::Default));
        assert_eq!(resolve(&[], "latest", Some("all_flags")), resolve(&[], "latest", None));

        let global = doc(json!({"all_flags": true}));
        assert_eq!(
            resolve(&[global.clone(), doc(json!({"subagent_depth": 2, "todo": false}))], "latest", None),
            with(every(true, Source::Config), "todo", false, Source::Config)
        );
        assert_eq!(resolve(&[global, doc(json!({"all_flags": false}))], "latest", None), every(false, Source::Default));
    }
}

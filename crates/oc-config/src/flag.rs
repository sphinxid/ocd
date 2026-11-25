//! Feature flags (`schema/src/config/flag.ts`, `core/src/flag/resolve.ts`, 2.4.1): one registry of experimental
//! features, each read with [`enabled`]. A flag is on in local and dev builds and off in released ones;
//! `experimental.<key>` in config and `OPENCODE_FLAGS` override that, and `OPENCODE_FLAGS=all` or
//! `experimental.all_flags: true` turn every flag on except the opt-in-only ones ([`OPT_IN_ONLY`]), which must be
//! named and are off by default on every channel. Local-only flags ([`LOCAL_ONLY`]) stay off outside local and dev
//! builds whatever the environment or config say; ocd is a release-channel build, so they never turn on.
//!
//! 2.4.1 removed the `automations`, `video_generate` and `mcp_events` flags (and the `schedules` alias) because those
//! features are on by default there. Until ocd ports their 2.4.1 behaviour it keeps them off by default: automations
//! read the top-level `automations` setting (`oc_core::automation::settings_of`), video generation answers 403, and
//! MCP Events stay behind the hidden [`STAGED`] flag `mcp_events`, which is not part of the registry.

use indexmap::IndexMap;

use crate::schema::Entry;

/// The `OPENCODE_FLAGS` entry that enables all flags except opt-in-only flags, as in `all,-media_blob`. It is not a
/// flag key.
pub const WILDCARD: &str = "all";

/// Every feature flag, by key, with what it turns on (`ConfigFlag.registry`, in its order).
pub const REGISTRY: &[(&str, &str)] = &[
    (
        "todo",
        "Enable the experimental todowrite tool. Off by default in every build, including local and dev; opt in with OPENCODE_FLAGS=todo or experimental.todo: true.",
    ),
    ("media_blob", "Store new session media once in a deduplicated blob table instead of inline in every message row."),
    ("artifacts", "Enable experimental artifacts backed by pluggable providers."),
    (
        "console_watches",
        "Watch Slack threads, GitHub pull requests, issues, commits and Actions runs, and the workspace's MCP servers' events through OpenCode Console, and wake sessions when they change.",
    ),
    ("recommended_api", "Use each model's recommended API from models.dev instead of the first API it lists."),
    (
        "model_speeds",
        "Offer a model's faster and slower service tiers (fast, flex…) as speeds of that model: one entry in the model picker with a speed selector, instead of a separate entry per tier such as gpt-5.5-fast.",
    ),
    ("full_codemode", "Enable the full Code Mode work in progress."),
    (
        "machines",
        "Let a tool path name a machine, as in gpu:/work/train.py, and act on that machine's files and processes.",
    ),
    (
        "home_memory",
        "Give sessions in the home folder a persistent memory/, docs/ and scripts/ workspace the agent can use freely, tidied by a daily maintenance session that reads the day's home-folder conversations.",
    ),
    (
        "project_overlay",
        "Load a personal config dir for each project from ~/.config/opencode/project/<project-id>/, read like an extra .opencode dir that ranks above the repo's own, and let home memory keep the skills it builds for the user there.",
    ),
    (
        "opencode_api",
        "Let Code Mode scripts call the OpenCode HTTP API with fetch(\"opencode:///api/...\"), in-process and already signed in, with no permission checks. Only in local and dev builds, and off there until you opt in with OPENCODE_FLAGS=opencode_api or experimental.opencode_api: true.",
    ),
    (
        "sqlite_fts",
        "Build an SQLite full-text index of session titles and message text in the background, and enable the opencode.session.search and opencode.session.read tools. The index covers every session in the database and starts while any loaded location enables it; SQLite keeps it current from then on, also after this is turned off.",
    ),
];

/// `ConfigFlag.optInOnly`: flags that `all` and `all_flags` skip, off by default on every channel.
pub const OPT_IN_ONLY: &[&str] = &["todo", "opencode_api"];

/// `ConfigFlag.localOnly`: flags that stay off outside local and dev builds, whatever `OPENCODE_FLAGS` or config say.
pub const LOCAL_ONLY: &[&str] = &["opencode_api"];

/// ocd-only, until the batch that ports the feature as 2.4.1 ships it: a flag 2.4.1 removed because the feature is
/// on by default there, which ocd keeps behind its old gate (resolved like a registry flag, opt-in in a release
/// build) so a half-ported feature never turns on by itself. Not in [`REGISTRY`], [`resolve`]'s output or the
/// config schema document. `mcp_events`: MCP Events, flagless in 2.4.1.
pub const STAGED: &[&str] = &["mcp_events"];

/// The description of `experimental.<key>` in the config schema (`ConfigExperimental.Info`): the registry's, with
/// the build-default note on flags that are neither opt-in-only nor local-only.
pub fn schema_description(key: &str) -> Option<String> {
    let (_, description) = REGISTRY.iter().find(|(k, _)| *k == key)?;
    Some(if OPT_IN_ONLY.contains(&key) || LOCAL_ONLY.contains(&key) {
        description.to_string()
    } else {
        format!("{description} Defaults to on in local and dev builds and off in releases.")
    })
}

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

/// `OPENCODE_FLAGS`, a comma-separated list such as `artifacts,-media_blob`: `key` on, `-key` off.
fn overrides_of(overrides: Option<&str>) -> IndexMap<&str, bool> {
    let mut env: IndexMap<&str, bool> = IndexMap::new();
    for item in overrides.unwrap_or_default().split(',').map(str::trim).filter(|s| !s.is_empty()) {
        match item.strip_prefix('-') {
            Some(key) => env.insert(key, false),
            None => env.insert(item, true),
        };
    }
    env
}

/// One flag's state (see [`resolve`]).
fn state(entries: &[Entry], channel: &str, env: &IndexMap<&str, bool>, key: &str) -> State {
    if LOCAL_ONLY.contains(&key) && !on_by_default(channel) {
        return State { enabled: false, source: Source::Default };
    }
    let all = env.get(WILDCARD) == Some(&true);
    let experimental = crate::fold::experimental(entries);
    let all_flags =
        crate::fold::latest(entries, |i| i.experimental.as_ref().and_then(|e| e.all_flags.as_ref())) == Some(&true);
    let opt_in_only = OPT_IN_ONLY.contains(&key);
    let override_ = env.get(key).copied().or((all && !opt_in_only).then_some(true));
    let configured = experimental.and_then(|e| e.flag(key)).or((all_flags && !opt_in_only).then_some(true));
    match (override_, configured) {
        (Some(enabled), _) => State { enabled, source: Source::Env },
        (None, Some(enabled)) => State { enabled, source: Source::Config },
        (None, None) => State { enabled: !opt_in_only && on_by_default(channel), source: Source::Default },
    }
}

/// `FlagResolve.resolve`: `OPENCODE_FLAGS` (`key` or `-key`, then `all`), then `experimental.<key>` from the
/// highest-priority document that sets `experimental`, then `all_flags: true` from the highest-priority document that
/// sets `all_flags` (`false` falls through rather than turning flags off), then on for local and dev builds.
/// Opt-in-only flags skip both wildcards and default to off; local-only flags are off outside local and dev builds.
pub fn resolve(entries: &[Entry], channel: &str, overrides: Option<&str>) -> IndexMap<&'static str, State> {
    let env = overrides_of(overrides);
    REGISTRY.iter().map(|(key, _)| (*key, state(entries, channel, &env, key))).collect()
}

/// [`resolve`] for one key of the registry or of [`STAGED`]; `None` for any other key.
pub fn resolve_key(entries: &[Entry], channel: &str, overrides: Option<&str>, key: &str) -> Option<State> {
    let known = REGISTRY.iter().any(|(k, _)| *k == key) || STAGED.contains(&key);
    known.then(|| state(entries, channel, &overrides_of(overrides), key))
}

/// `Flag.enabled(key)` with this build's channel and the process's `OPENCODE_FLAGS`; also reads a [`STAGED`] flag.
pub fn enabled(entries: &[Entry], key: &str) -> bool {
    let overrides = std::env::var("OPENCODE_FLAGS").ok();
    resolve_key(entries, oc_util::version::CHANNEL, overrides.as_deref(), key).is_some_and(|s| s.enabled)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn doc(experimental: serde_json::Value) -> Entry {
        let info = <crate::schema::Info as crate::decode::Decode>::decode(&json!({ "experimental": experimental })).expect("config");
        Entry::Document { path: None, info, integration: None, wellknown: None }
    }

    fn empty_doc() -> Entry {
        let info = <crate::schema::Info as crate::decode::Decode>::decode(&json!({})).expect("config");
        Entry::Document { path: None, info, integration: None, wellknown: None }
    }

    fn get(entries: &[Entry], channel: &str, overrides: Option<&str>, key: &str) -> (bool, &'static str) {
        let s = resolve(entries, channel, overrides)[key];
        (s.enabled, s.source.as_str())
    }

    /// `every(state)`: every flag in `state`, except `todo` and `opencode_api`, which are opt-in only, so the build
    /// defaults and both wildcards leave them off.
    fn every(enabled: bool, source: Source) -> IndexMap<&'static str, State> {
        REGISTRY
            .iter()
            .map(|(k, _)| {
                let state = if OPT_IN_ONLY.contains(k) { State { enabled: false, source: Source::Default } } else { State { enabled, source } };
                (*k, state)
            })
            .collect()
    }

    fn with(mut map: IndexMap<&'static str, State>, key: &'static str, enabled: bool, source: Source) -> IndexMap<&'static str, State> {
        map.insert(key, State { enabled, source });
        map
    }

    #[test]
    fn registry_order_and_retired_keys() {
        assert_eq!(
            REGISTRY.iter().map(|(k, _)| *k).collect::<Vec<_>>(),
            [
                "todo",
                "media_blob",
                "artifacts",
                "console_watches",
                "recommended_api",
                "model_speeds",
                "full_codemode",
                "machines",
                "home_memory",
                "project_overlay",
                "opencode_api",
                "sqlite_fts"
            ]
        );
        assert_eq!(resolve(&[], "latest", None).keys().copied().collect::<Vec<_>>(), REGISTRY.iter().map(|(k, _)| *k).collect::<Vec<_>>());
        // 2.4.1 retires `automations`, `video_generate`, `mcp_events` and the `schedules` alias; 2.3.1 retired
        // `background_compaction` and `shell_timeout_background`, 2.3.0 `codemode_vm`.
        for gone in ["automations", "video_generate", "mcp_events", "schedules", "background_compaction", "shell_timeout_background", "codemode_vm"] {
            assert!(!resolve(&[], "dev", Some(gone)).contains_key(gone), "{gone}");
        }
        for gone in ["automations", "video_generate", "schedules", "nope"] {
            assert_eq!(resolve_key(&[], "dev", Some(gone), gone), None, "{gone}");
        }
    }

    /// `ConfigExperimental.Info`'s annotations: opt-in-only and local-only flags keep the registry's text.
    #[test]
    fn schema_descriptions() {
        assert_eq!(
            schema_description("media_blob").unwrap(),
            "Store new session media once in a deduplicated blob table instead of inline in every message row. Defaults to on in local and dev builds and off in releases."
        );
        assert_eq!(
            schema_description("full_codemode").unwrap(),
            "Enable the full Code Mode work in progress. Defaults to on in local and dev builds and off in releases."
        );
        assert_eq!(schema_description("todo").unwrap(), REGISTRY[0].1);
        assert!(schema_description("opencode_api").unwrap().ends_with("experimental.opencode_api: true."));
        assert_eq!(schema_description("mcp_events"), None);
    }

    /// `flag.test.ts` (2.4.1), case by case.
    #[test]
    fn applies_build_defaults() {
        for (channel, on) in [("local", true), ("dev", true), ("beta", false), ("latest", false), ("my-branch", false), ("unknown", false)] {
            assert_eq!(resolve(&[], channel, None), every(on, Source::Default), "{channel}");
        }
    }

    #[test]
    fn todo_is_opt_in_only_on_only_when_named() {
        assert_eq!(get(&[], "dev", Some("all"), "todo"), (false, "default"));
        assert_eq!(get(&[], "latest", Some("todo"), "todo"), (true, "env"));
        assert_eq!(get(&[doc(json!({"todo": true}))], "latest", None, "todo"), (true, "config"));
    }

    #[test]
    fn opencode_api_turns_on_only_by_name_and_only_in_local_and_dev_builds() {
        let resolve_api = |channel: &str, overrides: Option<&str>, experimental: Option<serde_json::Value>| {
            let entries: Vec<Entry> = experimental.map(doc).into_iter().collect();
            get(&entries, channel, overrides, "opencode_api")
        };
        assert_eq!(resolve_api("dev", Some("all"), None), (false, "default"));
        assert_eq!(resolve_api("local", Some("opencode_api"), None), (true, "env"));
        assert_eq!(resolve_api("dev", None, Some(json!({"opencode_api": true}))), (true, "config"));
        for channel in ["beta", "latest", "my-branch", "unknown"] {
            assert_eq!(
                [resolve_api(channel, Some("opencode_api"), None), resolve_api(channel, None, Some(json!({"opencode_api": true})))],
                [(false, "default"), (false, "default")],
                "{channel}"
            );
        }
    }

    #[test]
    fn full_codemode_and_sqlite_fts_follow_build_defaults_overrides_and_wildcards() {
        for key in ["full_codemode", "sqlite_fts"] {
            assert_eq!(get(&[], "local", None, key), (true, "default"));
            assert_eq!(get(&[], "dev", None, key), (true, "default"));
            assert_eq!(get(&[], "latest", None, key), (false, "default"));
            assert_eq!(get(&[], "latest", Some(key), key), (true, "env"));
            assert_eq!(get(&[doc(json!({ key: true }))], "latest", None, key), (true, "config"));
            assert_eq!(get(&[doc(json!({ key: false }))], "dev", None, key), (false, "config"));
            assert_eq!(get(&[], "dev", Some(&format!("-{key}")), key), (false, "env"));
            assert_eq!(get(&[doc(json!({ key: false }))], "latest", Some("all"), key), (true, "env"));
            assert_eq!(get(&[doc(json!({"all_flags": true}))], "latest", None, key), (true, "config"));
        }
    }

    #[test]
    fn config_overrides_the_default_and_opencode_flags_overrides_config() {
        let flags = resolve(&[doc(json!({"full_codemode": true, "machines": false}))], "dev", Some(" media_blob , -full_codemode,unknown_flag"));
        assert_eq!(
            ["full_codemode", "machines", "media_blob", "artifacts"].map(|k| (flags[k].enabled, flags[k].source)),
            [(false, Source::Env), (false, Source::Config), (true, Source::Env), (true, Source::Default)]
        );
    }

    #[test]
    fn reads_experimental_from_the_highest_priority_document_that_sets_it() {
        let shadowed = [doc(json!({"full_codemode": true})), doc(json!({"subagent_depth": 2})), empty_doc()];
        assert_eq!(get(&shadowed, "latest", None, "full_codemode"), (false, "default"));
        // A document with an empty `experimental` block counts as setting it.
        let shadowed = [doc(json!({"full_codemode": true})), doc(json!({}))];
        assert_eq!(get(&shadowed, "latest", None, "full_codemode"), (false, "default"));
    }

    #[test]
    fn opencode_flags_all_turns_every_flag_on_and_a_minus_key_still_wins() {
        let entries = [doc(json!({"media_blob": false}))];
        assert_eq!(resolve(&entries, "latest", Some("all")), every(true, Source::Env));
        assert_eq!(resolve(&[], "latest", Some(" -media_blob , all, ")), with(every(true, Source::Env), "media_blob", false, Source::Env));
        assert_eq!(resolve(&entries, "latest", Some("-all")), resolve(&entries, "latest", None));
    }

    #[test]
    fn all_flags_turns_every_flag_on_below_opencode_flags_and_a_per_flag_setting() {
        let entries = [doc(json!({"all_flags": true, "media_blob": false}))];
        let expected = with(with(every(true, Source::Config), "media_blob", false, Source::Config), "artifacts", false, Source::Env);
        assert_eq!(resolve(&entries, "latest", Some("-artifacts")), expected);
    }

    #[test]
    fn all_flags_false_is_not_a_kill_switch_and_all_flags_in_opencode_flags_does_nothing() {
        assert_eq!(resolve(&[doc(json!({"all_flags": false}))], "dev", None), every(true, Source::Default));
        assert_eq!(resolve(&[], "latest", Some("all_flags")), resolve(&[], "latest", None));
    }

    #[test]
    fn a_global_all_flags_survives_a_project_block_and_a_project_all_flags_false_stops_it() {
        let global = doc(json!({"all_flags": true}));
        assert_eq!(
            resolve(&[global.clone(), doc(json!({"subagent_depth": 2, "full_codemode": false}))], "latest", None),
            with(every(true, Source::Config), "full_codemode", false, Source::Config)
        );
        assert_eq!(resolve(&[global, doc(json!({"all_flags": false}))], "latest", None), every(false, Source::Default));
    }

    /// ocd's staged `mcp_events` gate resolves like a 2.3.2 flag (off in a release build; on by name, `all`,
    /// `all_flags` or `experimental.mcp_events`) without appearing among the flags.
    #[test]
    fn the_staged_mcp_events_flag_keeps_its_old_gate() {
        let key = "mcp_events";
        let state = |entries: &[Entry], channel: &str, overrides: Option<&str>| {
            resolve_key(entries, channel, overrides, key).map(|s| (s.enabled, s.source.as_str()))
        };
        assert_eq!(state(&[], "latest", None), Some((false, "default")));
        assert_eq!(state(&[], "dev", None), Some((true, "default")));
        assert_eq!(state(&[], "latest", Some(key)), Some((true, "env")));
        assert_eq!(state(&[], "latest", Some("all")), Some((true, "env")));
        assert_eq!(state(&[doc(json!({ key: true }))], "latest", None), Some((true, "config")));
        assert_eq!(state(&[doc(json!({ key: true }))], "latest", Some("-mcp_events")), Some((false, "env")));
        assert_eq!(state(&[doc(json!({"all_flags": true}))], "latest", None), Some((true, "config")));
        assert!(!resolve(&[doc(json!({ key: true }))], "latest", Some(key)).contains_key(key));
        assert_eq!(resolve_key(&[], "latest", Some("todo"), "todo"), Some(resolve(&[], "latest", Some("todo"))["todo"]));
    }
}

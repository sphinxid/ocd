//! Plugin selection (`PluginSupervisor.resolve`, `packages/core/src/plugin/supervisor.ts:19-122`).
//!
//! Applies configured operations in order to the internal plugin ids plus package/local plugins:
//! a remove disables every matching id except the guarded ones; an add whose target matches a
//! known id, or is `*`, ends with `.*` or starts with `opencode.`, only (re-)enables ids; any other
//! add loads a package/local plugin. Boot order is pre internals → packages → post internals;
//! duplicate ids keep the first and report the rest as `Duplicate plugin ID: <id>`.

use std::collections::{HashMap, HashSet};
use std::future::Future;
use std::path::Path;

use indexmap::IndexMap;
use serde_json::{Value, json};

use crate::protocol::Operation;

/// `PluginInternal.guarded`: repository config cannot disable these.
pub const GUARDED: [&str; 2] = ["opencode.provider.opencode", "opencode.config.policy"];

/// The internal (Rust-native) plugin ids in boot order (`internal.ts:216-270`).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct InternalPlugins {
    pub pre: Vec<String>,
    pub post: Vec<String>,
}

/// A package/local plugin that loaded successfully.
#[derive(Debug, Clone, PartialEq)]
pub struct Loaded {
    pub id: String,
    pub target: String,
    pub revision: String,
    pub source: Value,
    pub features: Value,
}

impl Loaded {
    /// `Plugin.Info.features`: the parts the package lists (`{server?, tui?, rpc?, gui?}`), or `{server: true}`
    /// when the loader reported none (`slot.plugin.features ?? { server: true }`, core/src/plugin.ts 2.2).
    pub fn info_features(&self) -> Value {
        if self.features.is_object() { self.features.clone() } else { json!({ "server": true }) }
    }
}

/// What loading one add operation produced.
#[derive(Debug, Clone, PartialEq)]
pub enum LoadOutcome {
    Loaded(Loaded),
    /// Managed policy denied `plugin:<target>`.
    Blocked,
    /// Load failed: `error` is the inventory message (`Plugin failed to load` unless a LoadError).
    Failed { error: String, r#ref: Option<String> },
}

/// One enabled plugin in boot order.
#[derive(Debug, Clone, PartialEq)]
pub enum Entry {
    Internal(String),
    Package(Loaded),
}

impl Entry {
    pub fn id(&self) -> &str {
        match self {
            Entry::Internal(id) => id,
            Entry::Package(p) => &p.id,
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Resolution {
    /// Enabled plugins in boot order, duplicates removed.
    pub plugins: Vec<Entry>,
    /// Enabled package/local plugins keyed by target (the next `running` map).
    pub packages: IndexMap<String, Loaded>,
    /// `Plugin.Info` values of failed loads and duplicates.
    pub failures: Vec<Value>,
}

impl Resolution {
    pub fn enabled_ids(&self) -> Vec<String> {
        self.plugins.iter().map(|p| p.id().to_string()).collect()
    }

    pub fn package_entries(&self) -> Vec<&Loaded> {
        self.plugins
            .iter()
            .filter_map(|p| match p {
                Entry::Package(p) => Some(p),
                Entry::Internal(_) => None,
            })
            .collect()
    }
}

fn matches(selector: &str, target: &str) -> bool {
    selector == "*"
        || match selector.strip_suffix(".*") {
            Some(prefix) => target.starts_with(&selector[..prefix.len() + 1]),
            None => selector == target,
        }
}

fn source_for(target: &str) -> Value {
    if Path::new(target).is_absolute() {
        json!({ "type": "local", "path": target })
    } else {
        json!({ "type": "package", "target": target })
    }
}

/// Resolves operations. `load` is called for each add that does not select existing ids, in
/// order; `running` holds the generations of the previous resolution (a failed reload keeps its
/// previous generation in place).
pub async fn resolve<F, Fut>(
    internal: &InternalPlugins,
    operations: &[Operation],
    running: &IndexMap<String, Loaded>,
    mut load: F,
) -> Resolution
where
    F: FnMut(&Operation) -> Fut,
    Fut: Future<Output = LoadOutcome>,
{
    let definitions: Vec<String> = internal.pre.iter().chain(&internal.post).cloned().collect();
    let mut enabled: HashSet<String> = definitions.iter().cloned().collect();
    let mut packages: IndexMap<String, Loaded> = IndexMap::new();
    let mut failures: IndexMap<String, Value> = IndexMap::new();
    let ids = |packages: &IndexMap<String, Loaded>| -> Vec<String> {
        definitions.iter().cloned().chain(packages.values().map(|p| p.id.clone())).collect()
    };

    for operation in operations {
        let Operation::Add { target, .. } = operation else {
            let target = operation.target();
            if target == "*" {
                failures.clear();
            }
            for id in ids(&packages) {
                if matches(target, &id) && !GUARDED.contains(&id.as_str()) {
                    enabled.remove(&id);
                }
            }
            continue;
        };
        let matched: Vec<String> = ids(&packages).into_iter().filter(|id| matches(target, id)).collect();
        let selects = !matched.is_empty() || target == "*" || target.ends_with(".*") || target.starts_with("opencode.");
        if selects {
            enabled.extend(matched);
            continue;
        }
        match load(operation).await {
            LoadOutcome::Blocked => {
                if let Some(previous) = packages.shift_remove(target) {
                    enabled.remove(&previous.id);
                }
                failures.shift_remove(target);
            }
            LoadOutcome::Failed { error, r#ref } => {
                let mut state = json!({ "status": "failed", "error": error });
                if let Some(r) = r#ref {
                    state["ref"] = Value::String(r);
                }
                failures.insert(
                    target.clone(),
                    json!({ "source": source_for(target), "state": state, "features": { "server": true } }),
                );
                let retained = packages.get(target).cloned().or_else(|| running.get(target).cloned());
                if let Some(retained) = retained {
                    enabled.insert(retained.id.clone());
                    packages.insert(target.clone(), retained);
                }
            }
            LoadOutcome::Loaded(plugin) => {
                failures.shift_remove(target);
                if let Some(previous) = packages.get(target) {
                    enabled.remove(&previous.id);
                }
                enabled.insert(plugin.id.clone());
                packages.insert(target.clone(), plugin);
            }
        }
    }

    let mut ordered: Vec<Entry> = Vec::new();
    ordered.extend(internal.pre.iter().filter(|id| enabled.contains(*id)).cloned().map(Entry::Internal));
    ordered.extend(packages.values().filter(|p| enabled.contains(&p.id)).cloned().map(Entry::Package));
    ordered.extend(internal.post.iter().filter(|id| enabled.contains(*id)).cloned().map(Entry::Internal));

    let mut seen: HashMap<String, ()> = HashMap::new();
    let mut plugins = Vec::new();
    let mut duplicates = Vec::new();
    for entry in ordered {
        if seen.insert(entry.id().to_string(), ()).is_some() {
            duplicates.push(entry);
        } else {
            plugins.push(entry);
        }
    }
    let mut failures: Vec<Value> = failures.into_values().collect();
    for entry in duplicates {
        let (source, features) = match &entry {
            Entry::Internal(_) => (json!({ "type": "builtin" }), json!({ "server": true })),
            Entry::Package(p) => (p.source.clone(), p.info_features()),
        };
        failures.push(json!({
            "id": entry.id(),
            "source": source,
            "state": { "status": "failed", "error": format!("Duplicate plugin ID: {}", entry.id()) },
            "features": features,
        }));
    }
    let packages = packages.into_iter().filter(|(_, p)| enabled.contains(&p.id)).collect();
    Resolution { plugins, packages, failures }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn internal() -> InternalPlugins {
        InternalPlugins {
            pre: vec!["opencode.agent".into(), "opencode.provider.opencode".into()],
            post: vec!["opencode.browser".into(), "opencode.config.policy".into()],
        }
    }

    fn loaded(id: &str, target: &str) -> Loaded {
        Loaded { id: id.into(), target: target.into(), revision: "r".into(), source: json!({"type":"package","target":target}), features: json!({}) }
    }

    async fn run(ops: Vec<Operation>) -> Resolution {
        resolve(&internal(), &ops, &IndexMap::new(), |op| {
            let target = op.target().to_string();
            async move {
                match target.as_str() {
                    "bad" => LoadOutcome::Failed { error: "Plugin failed to load".into(), r#ref: Some("err_1".into()) },
                    "blocked" => LoadOutcome::Blocked,
                    "dup" => LoadOutcome::Loaded(loaded("opencode.agent", "dup")),
                    t => LoadOutcome::Loaded(loaded(&format!("pkg.{t}"), t)),
                }
            }
        })
        .await
    }

    #[tokio::test]
    async fn order_and_removal() {
        let r = run(vec![Operation::add("a"), Operation::Remove { target: "opencode.browser".into() }]).await;
        assert_eq!(r.enabled_ids(), ["opencode.agent", "opencode.provider.opencode", "pkg.a", "opencode.config.policy"]);
    }

    #[tokio::test]
    async fn guarded_and_wildcards() {
        let r = run(vec![Operation::Remove { target: "*".into() }, Operation::add("opencode.agent")]).await;
        assert_eq!(r.enabled_ids(), ["opencode.agent", "opencode.provider.opencode", "opencode.config.policy"]);
        let r = run(vec![Operation::add("a"), Operation::Remove { target: "pkg.*".into() }]).await;
        assert!(!r.enabled_ids().contains(&"pkg.a".to_string()));
    }

    #[tokio::test]
    async fn failures_and_duplicates() {
        let r = run(vec![Operation::add("bad"), Operation::add("dup"), Operation::add("blocked")]).await;
        assert_eq!(r.failures.len(), 2);
        assert_eq!(r.failures[0]["state"]["error"], "Plugin failed to load");
        assert_eq!(r.failures[0]["source"], json!({"type":"package","target":"bad"}));
        assert_eq!(r.failures[1]["state"]["error"], "Duplicate plugin ID: opencode.agent");
        assert_eq!(r.enabled_ids(), ["opencode.agent", "opencode.provider.opencode", "opencode.browser", "opencode.config.policy"]);
    }

    #[tokio::test]
    async fn selector_does_not_load() {
        let mut calls = 0;
        let ops = vec![Operation::add("opencode.something"), Operation::add("x.*")];
        let _ = resolve(&internal(), &ops, &IndexMap::new(), |_| {
            calls += 1;
            async { LoadOutcome::Blocked }
        })
        .await;
        assert_eq!(calls, 0);
    }
}

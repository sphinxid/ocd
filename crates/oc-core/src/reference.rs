//! Project references (`core/src/reference.ts`, `core/reference/instructions.ts`).

use std::sync::Arc;

use oc_config::fold::ReferenceSource;
use serde_json::{json, Value};

use crate::instructions::{self, locale_compare, Read, Source};
use crate::location::Location;

#[derive(Debug, Clone)]
pub struct Info {
    pub name: String,
    pub source: ReferenceSource,
    pub description: Option<String>,
    pub hidden: Option<bool>,
    pub path: String,
}

impl Info {
    pub fn to_json(&self) -> Value {
        let mut v = json!({"name": self.name, "source": self.source});
        if let Some(d) = &self.description {
            v["description"] = json!(d);
        }
        if let Some(h) = self.hidden {
            v["hidden"] = json!(h);
        }
        v["path"] = json!(self.path);
        v
    }
}

pub fn list(location: &Location, global: &oc_util::Global) -> Vec<Info> {
    oc_config::fold::references(&location.entries, &global.home, &location.directory)
        .into_iter()
        .filter_map(|(name, source)| {
            let (description, hidden) = match &source {
                ReferenceSource::Local { description, hidden, .. } | ReferenceSource::Git { description, hidden, .. } => {
                    (description.clone(), *hidden)
                }
            };
            let path = match &source {
                ReferenceSource::Local { path, .. } => path.display().to_string(),
                ReferenceSource::Git { repository, branch, .. } => {
                    let r = oc_git::repository::parse(repository)?;
                    if !oc_git::repository::is_remote(&r) {
                        return None;
                    }
                    if let Some(b) = branch {
                        oc_git::repository::validate_branch(b).ok()?;
                    }
                    oc_git::repository::cache_path(&global.repos, &r, branch.as_deref()).display().to_string()
                }
            };
            Some(Info { name, source, description, hidden, path })
        })
        .collect()
}

struct KvAdapter(oc_storage::Database);
impl oc_git::Kv for KvAdapter {
    fn get(&self, key: &str) -> anyhow::Result<Option<Value>> {
        self.0.kv_get(key)
    }
    fn set(&self, key: &str, value: &Value) -> anyhow::Result<()> {
        self.0.kv_set(key, value)
    }
}

/// Materialize git references in the background (daily refresh).
pub fn refresh(app: &Arc<crate::app::App>, refs: Vec<Info>) {
    let cache = Arc::new(oc_git::RepositoryCache::new(
        oc_git::Git::new(),
        app.global.repos.clone(),
        app.global.locks(),
        Arc::new(KvAdapter(app.db.clone())),
    ));
    for r in refs {
        let ReferenceSource::Git { repository, branch, .. } = &r.source else { continue };
        let Ok(reference) = oc_git::repository::parse_remote(repository) else { continue };
        let cache = cache.clone();
        let branch = branch.clone();
        let name = r.name.clone();
        tokio::spawn(async move {
            if let Err(e) = cache.ensure(oc_git::EnsureInput { reference, refresh: oc_git::Refresh::Daily, branch }).await {
                tracing::warn!(name = %name, error = %e, "failed to materialize reference");
            }
        });
    }
}

/// `core/reference-guidance`
pub fn source(refs: &[Info]) -> Source {
    #[derive(serde::Serialize, serde::Deserialize, Clone, PartialEq)]
    struct S {
        name: String,
        path: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        description: Option<String>,
    }
    let mut available: Vec<S> = refs
        .iter()
        .filter(|r| r.description.is_some())
        .map(|r| S { name: r.name.clone(), path: r.path.clone(), description: r.description.clone() })
        .collect();
    available.sort_by(|a, b| locale_compare(&a.name, &b.name));
    fn entries(list: &[&S]) -> Vec<String> {
        list.iter()
            .flat_map(|r| {
                let mut l = vec!["  <reference>".to_string(), format!("    <name>{}</name>", r.name), format!("    <path>{}</path>", r.path)];
                if let Some(d) = &r.description {
                    l.push(format!("    <description>{d}</description>"));
                }
                l.push("  </reference>".into());
                l
            })
            .collect()
    }
    fn render(list: &[S]) -> String {
        let mut l = vec!["Project references provide additional directories that can be accessed when relevant.".to_string(), "<available_references>".into()];
        l.extend(entries(&list.iter().collect::<Vec<_>>()));
        l.push("</available_references>".into());
        l.join("\n")
    }
    let decode = |v: &Value| serde_json::from_value::<Vec<S>>(v.clone()).ok();
    Source {
        key: "core/reference-guidance".into(),
        read: if available.is_empty() { Read::Removed } else { Read::Value(serde_json::to_value(&available).unwrap()) },
        initial: Box::new(move |v| decode(v).map(|s| render(&s))),
        changed: Box::new(move |p, c| {
            let cur = decode(c)?;
            let Some(prev) = decode(p) else { return Some(render(&cur)) };
            let (added, removed, changed) = instructions::diff_by_key(&prev, &cur, |r| r.name.clone(), |a, b| a.path != b.path || a.description != b.description);
            if !changed.is_empty() || (added.is_empty() && removed.is_empty()) {
                return Some(format!("The available project references have changed. This list supersedes the previous reference list.\n{}", render(&cur)));
            }
            let mut l = Vec::new();
            if !added.is_empty() {
                l.push("New project references are available in addition to those previously listed:".to_string());
                l.extend(entries(&added));
            }
            if !removed.is_empty() {
                l.push(format!(
                    "The following project references are no longer available and must not be used: {}.",
                    removed.iter().map(|r| r.name.as_str()).collect::<Vec<_>>().join(", ")
                ));
            }
            Some(l.join("\n"))
        }),
        removed: Box::new(|_| Some("Project reference guidance is no longer available. Do not use previously listed references.".into())),
    }
}

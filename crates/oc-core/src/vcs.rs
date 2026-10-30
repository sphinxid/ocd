//! The VCS provider registry (`core/src/vcs.ts`): the built-in git provider
//! (`core/src/plugin/vcs/git.ts`) and the providers plugins add through `vcs` transforms
//! ([`crate::plugins::Plugins::vcs`]).
//!
//! A location uses the provider its registry selects (`default.set`), else the one whose id is the
//! location's detected VCS type. Failures of `info`, `branches` and `status` are logged and replaced
//! with empty results; `base` and `diff` keep a provider's `Vcs.DiffError` and turn anything else
//! into upstream's generic `DiffError` messages.

use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;
use oc_git::vcs::{Base, BranchOptions, DiffError, Info, Mode, UntrackedMode, VcsFileStatus};
use oc_schema::types::FileDiff;
use serde_json::{Value, json};

/// `VcsScope` (`plugin/src/effect/vcs.ts`).
#[derive(Debug, Clone, PartialEq)]
pub struct Scope {
    /// The location directory.
    pub directory: PathBuf,
    /// The project worktree root (`location.project.directory`).
    pub worktree: PathBuf,
    /// The canonical checkout (`location.project.canonical`).
    pub canonical: PathBuf,
    /// The VCS store (git common directory, `.hg`).
    pub store: Option<PathBuf>,
}

impl Scope {
    pub fn to_json(&self) -> Value {
        let mut v = json!({
            "directory": self.directory.display().to_string(),
            "worktree": self.worktree.display().to_string(),
            "canonical": self.canonical.display().to_string(),
        });
        if let Some(store) = &self.store {
            v["store"] = json!(store.display().to_string());
        }
        v
    }

    pub fn from_json(value: &Value) -> Option<Scope> {
        let path = |k: &str| value.get(k).and_then(Value::as_str).map(PathBuf::from);
        Some(Scope { directory: path("directory")?, worktree: path("worktree")?, canonical: path("canonical")?, store: path("store") })
    }

    /// The scope of a loaded location.
    pub fn of(location: &crate::location::Location) -> Scope {
        let project = &location.project;
        Scope {
            directory: location.directory.clone(),
            worktree: project.directory.clone(),
            canonical: project.canonical.clone(),
            store: project.vcs.as_ref().map(|v| match v {
                crate::project::Vcs::Git { store } | crate::project::Vcs::Hg { store } => store.clone(),
            }),
        }
    }
}

/// `VcsDefinition`: one provider's operations. Errors are messages, except `base`/`diff`
/// which distinguish a provider's `Vcs.DiffError`.
#[async_trait]
pub trait Provider: Send + Sync {
    fn id(&self) -> &str;
    fn name(&self) -> &str;
    fn has_init(&self) -> bool;
    fn has_base(&self) -> bool;
    async fn init(&self, scope: &Scope) -> Result<(), String>;
    async fn info(&self, scope: &Scope) -> Result<Info, String>;
    async fn base(&self, scope: &Scope) -> Result<Option<Base>, ProviderError>;
    async fn branches(&self, scope: &Scope, options: &BranchOptions) -> Result<Vec<String>, String>;
    async fn status(&self, scope: &Scope) -> Result<Vec<VcsFileStatus>, String>;
    /// `diff({...scope, mode, base?, untracked?, context, maxOutputBytes})`.
    async fn diff(
        &self,
        scope: &Scope,
        mode: Mode,
        base: Option<&str>,
        untracked: Option<UntrackedMode>,
        context: u64,
    ) -> Result<Vec<FileDiff>, ProviderError>;
}

/// A provider failure: its own `Vcs.DiffError`, or anything else.
#[derive(Debug, Clone, PartialEq)]
pub enum ProviderError {
    Diff(DiffError),
    Other(String),
}

/// The built-in git provider (`core/src/plugin/vcs/git.ts`).
pub struct Git;

impl Git {
    fn vcs(scope: &Scope) -> oc_git::GitVcs {
        oc_git::GitVcs::new(&scope.directory, &scope.worktree)
    }
}

#[async_trait]
impl Provider for Git {
    fn id(&self) -> &str {
        oc_git::GitVcs::ID
    }
    fn name(&self) -> &str {
        oc_git::GitVcs::NAME
    }
    fn has_init(&self) -> bool {
        true
    }
    fn has_base(&self) -> bool {
        true
    }
    async fn init(&self, scope: &Scope) -> Result<(), String> {
        oc_git::GitVcs::init(&scope.worktree).await.map_err(|e| e.message)
    }
    async fn info(&self, scope: &Scope) -> Result<Info, String> {
        Ok(Self::vcs(scope).info().await)
    }
    async fn base(&self, scope: &Scope) -> Result<Option<Base>, ProviderError> {
        Self::vcs(scope).base().await.map_err(ProviderError::Diff)
    }
    async fn branches(&self, scope: &Scope, options: &BranchOptions) -> Result<Vec<String>, String> {
        Ok(Self::vcs(scope).branches(options).await)
    }
    async fn status(&self, scope: &Scope) -> Result<Vec<VcsFileStatus>, String> {
        Ok(Self::vcs(scope).status().await)
    }
    async fn diff(
        &self,
        scope: &Scope,
        mode: Mode,
        base: Option<&str>,
        untracked: Option<UntrackedMode>,
        context: u64,
    ) -> Result<Vec<FileDiff>, ProviderError> {
        let options = oc_git::vcs::DiffOptions { context: Some(context), base: base.map(str::to_string), untracked };
        Self::vcs(scope).diff(mode, &options).await.map_err(ProviderError::Diff)
    }
}

/// A location's providers and selection (`{providers: Map, selection?}`).
#[derive(Clone, Default)]
pub struct Registry {
    pub providers: Vec<Arc<dyn Provider>>,
    pub selection: Option<String>,
}

impl Registry {
    /// The built-ins (internal.ts `pre`): git unless the location is another VCS's repository.
    /// (The hg provider is not ported.)
    pub fn native(vcs: Option<&str>) -> Registry {
        let mut providers: Vec<Arc<dyn Provider>> = Vec::new();
        if vcs.is_none_or(|v| v == "git") {
            providers.push(Arc::new(Git));
        }
        Registry { providers, selection: None }
    }

    pub fn get(&self, id: &str) -> Option<Arc<dyn Provider>> {
        self.providers.iter().find(|p| p.id() == id).cloned()
    }

    /// `selected()`: the selection, else the location's VCS type.
    pub fn selected(&self, vcs: Option<&str>) -> Option<Arc<dyn Provider>> {
        self.get(self.selection.as_deref().or(vcs)?)
    }
}

/// A location's VCS service: the selected provider over the location's scope.
pub struct Vcs {
    pub provider: Option<Arc<dyn Provider>>,
    pub scope: Scope,
}

impl Vcs {
    /// `info()`: `{branch:{}}` without a provider; failures are logged and read as empty.
    pub async fn info(&self) -> Info {
        let Some(provider) = &self.provider else { return Info::default() };
        match provider.info(&self.scope).await {
            Ok(mut info) => {
                info.provider = Some(provider.id().to_string());
                info
            }
            Err(error) => {
                tracing::error!(provider = provider.id(), %error, "vcs info failed");
                Info { provider: Some(provider.id().to_string()), ..Info::default() }
            }
        }
    }

    pub async fn base(&self) -> Result<Option<Base>, DiffError> {
        let Some(provider) = self.provider.as_ref().filter(|p| p.has_base()) else { return Ok(None) };
        provider.base(&self.scope).await.map_err(|e| review_error(e, oc_git::vcs::BASE_FAILED_MESSAGE))
    }

    pub async fn branches(&self, options: &BranchOptions) -> Vec<String> {
        let Some(provider) = &self.provider else { return vec![] };
        provider.branches(&self.scope, options).await.unwrap_or_else(|error| {
            tracing::error!(provider = provider.id(), %error, "vcs branches failed");
            vec![]
        })
    }

    pub async fn status(&self) -> Vec<VcsFileStatus> {
        let Some(provider) = &self.provider else { return vec![] };
        provider.status(&self.scope).await.unwrap_or_else(|error| {
            tracing::error!(provider = provider.id(), %error, "vcs status failed");
            vec![]
        })
    }

    /// `diff(mode, {context, base, untracked})`: `context ?? PATCH_CONTEXT_LINES`, then the 10 MB budget.
    pub async fn diff(
        &self,
        mode: Mode,
        context: Option<u64>,
        base: Option<&str>,
        untracked: Option<UntrackedMode>,
    ) -> Result<Vec<FileDiff>, DiffError> {
        let Some(provider) = &self.provider else { return Ok(vec![]) };
        let rows = provider
            .diff(&self.scope, mode, base, untracked, context.unwrap_or(oc_git::patch::PATCH_CONTEXT_LINES))
            .await
            .map_err(|e| review_error(e, oc_git::vcs::DIFF_FAILED_MESSAGE))?;
        Ok(oc_git::vcs::apply_total_patch_budget(rows))
    }
}

fn review_error(error: ProviderError, fallback: &str) -> DiffError {
    match error {
        ProviderError::Diff(e) => e,
        ProviderError::Other(cause) => {
            tracing::error!(%cause, "{fallback}");
            DiffError { message: fallback.to_string() }
        }
    }
}

/// `VcsBranchesInput` / `VcsDiffInput` JSON for plugin providers.
pub fn branches_input(scope: &Scope, options: &BranchOptions) -> Value {
    let mut v = scope.to_json();
    if let Some(search) = &options.search {
        v["search"] = json!(search);
    }
    if let Some(limit) = options.limit {
        v["limit"] = json!(limit);
    }
    v
}

pub fn diff_input(scope: &Scope, mode: Mode, base: Option<&str>, untracked: Option<UntrackedMode>, context: u64) -> Value {
    let mut v = scope.to_json();
    v["mode"] = serde_json::to_value(mode).unwrap_or(Value::Null);
    if let Some(base) = base {
        v["base"] = json!(base);
    }
    if let Some(untracked) = untracked {
        v["untracked"] = json!(untracked.as_str());
    }
    v["context"] = json!(context);
    v["maxOutputBytes"] = json!(oc_git::patch::MAX_TOTAL_PATCH_BYTES);
    v
}

pub fn branch_options(input: &Value) -> BranchOptions {
    BranchOptions {
        search: input.get("search").and_then(Value::as_str).map(String::from),
        limit: input.get("limit").and_then(Value::as_u64),
    }
}

pub fn diff_args(input: &Value) -> (Mode, Option<String>, Option<UntrackedMode>, u64) {
    let mode = input.get("mode").cloned().and_then(|m| serde_json::from_value(m).ok()).unwrap_or(Mode::Working);
    let base = input.get("base").and_then(Value::as_str).map(String::from);
    let untracked = input.get("untracked").cloned().and_then(|u| serde_json::from_value(u).ok());
    let context = input.get("context").and_then(Value::as_u64).unwrap_or(oc_git::patch::PATCH_CONTEXT_LINES);
    (mode, base, untracked, context)
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Fake(&'static str, Result<Vec<FileDiff>, ProviderError>);

    #[async_trait]
    impl Provider for Fake {
        fn id(&self) -> &str {
            self.0
        }
        fn name(&self) -> &str {
            "Fake"
        }
        fn has_init(&self) -> bool {
            false
        }
        fn has_base(&self) -> bool {
            false
        }
        async fn init(&self, _: &Scope) -> Result<(), String> {
            Err("no".into())
        }
        async fn info(&self, _: &Scope) -> Result<Info, String> {
            Err("broken".into())
        }
        async fn base(&self, _: &Scope) -> Result<Option<Base>, ProviderError> {
            Ok(None)
        }
        async fn branches(&self, _: &Scope, _: &BranchOptions) -> Result<Vec<String>, String> {
            Err("broken".into())
        }
        async fn status(&self, _: &Scope) -> Result<Vec<VcsFileStatus>, String> {
            Ok(vec![])
        }
        async fn diff(
            &self,
            _: &Scope,
            _: Mode,
            _: Option<&str>,
            _: Option<UntrackedMode>,
            _: u64,
        ) -> Result<Vec<FileDiff>, ProviderError> {
            self.1.clone()
        }
    }

    fn scope() -> Scope {
        Scope { directory: "/a/b".into(), worktree: "/a".into(), canonical: "/a".into(), store: None }
    }

    #[test]
    fn selection_and_scope() {
        let registry = Registry {
            providers: vec![Arc::new(Git), Arc::new(Fake("jj", Ok(vec![])))],
            selection: None,
        };
        assert_eq!(registry.selected(Some("git")).unwrap().id(), "git");
        assert!(registry.selected(None).is_none());
        assert!(registry.selected(Some("hg")).is_none());
        let picked = Registry { selection: Some("jj".into()), ..registry };
        assert_eq!(picked.selected(Some("git")).unwrap().id(), "jj");
        assert_eq!(Registry::native(Some("hg")).providers.len(), 0);
        assert_eq!(Registry::native(None).providers[0].id(), "git");
        let s = scope();
        assert_eq!(s.to_json(), json!({"directory": "/a/b", "worktree": "/a", "canonical": "/a"}));
        assert_eq!(Scope::from_json(&s.to_json()), Some(s.clone()));
        let input = diff_input(&s, Mode::Branch, Some("main"), None, 3);
        assert_eq!(input["mode"], "branch");
        assert_eq!(input["maxOutputBytes"], 10_000_000);
        assert_eq!(diff_args(&input), (Mode::Branch, Some("main".into()), None, 3));
        let input = diff_input(&s, Mode::Working, None, Some(UntrackedMode::Folders), 3);
        let keys: Vec<&String> = input.as_object().unwrap().keys().collect();
        assert_eq!(keys[keys.len() - 4..], ["mode", "untracked", "context", "maxOutputBytes"]);
        assert_eq!(diff_args(&input), (Mode::Working, None, Some(UntrackedMode::Folders), 3));
    }

    #[tokio::test]
    async fn failures_read_as_empty_or_diff_errors() {
        let vcs = Vcs { provider: Some(Arc::new(Fake("jj", Err(ProviderError::Other("x".into()))))), scope: scope() };
        assert_eq!(serde_json::to_value(vcs.info().await).unwrap(), json!({"provider": "jj", "branch": {}}));
        assert!(vcs.branches(&BranchOptions::default()).await.is_empty());
        assert_eq!(vcs.base().await, Ok(None));
        assert_eq!(vcs.diff(Mode::Working, None, None, None).await.unwrap_err().message, oc_git::vcs::DIFF_FAILED_MESSAGE);
        let own = DiffError { message: "own".into() };
        let vcs = Vcs { provider: Some(Arc::new(Fake("jj", Err(ProviderError::Diff(own.clone()))))), scope: scope() };
        assert_eq!(vcs.diff(Mode::Working, None, None, None).await.unwrap_err(), own);
        let none = Vcs { provider: None, scope: scope() };
        assert_eq!(serde_json::to_value(none.info().await).unwrap(), json!({"branch": {}}));
    }
}

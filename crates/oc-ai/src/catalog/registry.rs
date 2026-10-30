//! Provider and model registries (`packages/core/src/provider.ts`, `packages/core/src/model.ts`):
//! plugin folds over provider definitions, availability, model transforms, effective model merge,
//! sorting, default and small model selection.

use super::native::{rewrite_model, rewrite_provider};
use super::types::{merge_headers, merge_overlay, model_settings, supports_text, Activation, ModelInfo, ModelStatus, ProviderInfo};
use indexmap::IndexMap;
use std::collections::HashMap;
use std::sync::Arc;

/// A provider definition with its model inventory (`Provider.Definition`).
#[derive(Debug, Clone, PartialEq)]
pub struct ProviderRecord {
    pub provider: ProviderInfo,
    pub models: IndexMap<String, ModelInfo>,
    /// Key of the integration connection the models were discovered with (account-specific inventories).
    pub source_connection: Option<String>,
}

impl ProviderRecord {
    fn empty(id: &str) -> Self {
        Self { provider: ProviderInfo::empty(id), models: IndexMap::new(), source_connection: None }
    }
}

/// Model definitions take the registering provider's identity.
fn index(provider_id: &str, models: Vec<ModelInfo>) -> IndexMap<String, ModelInfo> {
    models
        .into_iter()
        .map(|mut m| {
            m.provider_id = provider_id.to_string();
            (m.id.clone(), m)
        })
        .collect()
}

/// Provider fold editor (`Provider.Editor`).
#[derive(Debug, Default, Clone)]
pub struct ProviderEditor {
    records: IndexMap<String, ProviderRecord>,
}

impl ProviderEditor {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn list(&self) -> impl Iterator<Item = &ProviderRecord> {
        self.records.values()
    }

    pub fn ids(&self) -> Vec<String> {
        self.records.keys().cloned().collect()
    }

    pub fn get(&self, id: &str) -> Option<&ProviderRecord> {
        self.records.get(id)
    }

    /// Replace a definition (keeps the provider's position when it already exists).
    pub fn add(&mut self, info: ProviderInfo, models: Vec<ModelInfo>, source_connection: Option<String>) {
        let id = info.id.clone();
        let models = index(&id, models);
        self.records.insert(id, ProviderRecord { provider: info, models, source_connection });
    }

    fn entry(&mut self, id: &str) -> &mut ProviderRecord {
        self.records.entry(id.to_string()).or_insert_with(|| ProviderRecord::empty(id))
    }

    /// Edit (or create) a provider; AI SDK packages are rewritten to native packages on write.
    pub fn update(&mut self, id: &str, f: impl FnOnce(&mut ProviderInfo)) {
        let record = self.entry(id);
        f(&mut record.provider);
        record.provider.id = id.to_string();
        rewrite_provider(&mut record.provider);
    }

    pub fn remove(&mut self, id: &str) {
        self.records.shift_remove(id);
    }

    pub fn set_models(&mut self, id: &str, models: Vec<ModelInfo>) {
        let models = index(id, models);
        self.entry(id).models = models;
    }

    /// Edit (or create from `Model.Info.default`) a model definition.
    pub fn update_model(&mut self, provider_id: &str, model_id: &str, f: impl FnOnce(&mut ModelInfo)) {
        let record = self.entry(provider_id);
        let mut model =
            record.models.get(model_id).cloned().unwrap_or_else(|| ModelInfo::default_for(provider_id, model_id));
        f(&mut model);
        model.id = model_id.to_string();
        model.provider_id = provider_id.to_string();
        rewrite_model(&mut model, Some(&record.provider));
        record.models.insert(model_id.to_string(), model);
    }

    pub fn remove_model(&mut self, provider_id: &str, model_id: &str) {
        if let Some(record) = self.records.get_mut(provider_id) {
            record.models.shift_remove(model_id);
        }
    }

    pub fn into_records(self) -> IndexMap<String, ProviderRecord> {
        self.records
    }
}

/// Model fold editor (`Model.Editor`): edits only models of available providers.
pub struct ModelEditor<'a> {
    models: IndexMap<String, IndexMap<String, ModelInfo>>,
    default_model: Option<(String, String)>,
    records: &'a IndexMap<String, ProviderRecord>,
}

impl<'a> ModelEditor<'a> {
    /// Model ids (including disabled candidates) of one or every available provider.
    pub fn list(&self, provider_id: Option<&str>) -> Vec<(String, String)> {
        let mut out = vec![];
        for (pid, models) in &self.models {
            if provider_id.map(|p| p != pid).unwrap_or(false) {
                continue;
            }
            for mid in models.keys() {
                out.push((pid.clone(), mid.clone()));
            }
        }
        out
    }

    pub fn get(&self, provider_id: &str, model_id: &str) -> Option<&ModelInfo> {
        self.models.get(provider_id).and_then(|m| m.get(model_id))
    }

    /// Edit (or create) a model of an available provider; no-op for unavailable providers.
    pub fn update(&mut self, provider_id: &str, model_id: &str, f: impl FnOnce(&mut ModelInfo)) {
        let provider = self.records.get(provider_id).map(|r| &r.provider);
        let Some(models) = self.models.get_mut(provider_id) else { return };
        let mut model = models.get(model_id).cloned().unwrap_or_else(|| ModelInfo::default_for(provider_id, model_id));
        f(&mut model);
        model.id = model_id.to_string();
        model.provider_id = provider_id.to_string();
        rewrite_model(&mut model, provider);
        models.insert(model_id.to_string(), model);
    }

    pub fn remove(&mut self, provider_id: &str, model_id: &str) {
        if let Some(models) = self.models.get_mut(provider_id) {
            models.shift_remove(model_id);
        }
    }

    pub fn default_model(&self) -> Option<&(String, String)> {
        self.default_model.as_ref()
    }

    pub fn set_default(&mut self, provider_id: &str, model_id: &str) {
        self.default_model = Some((provider_id.to_string(), model_id.to_string()));
    }

    /// Read-only provider definitions, including inactive providers.
    pub fn provider(&self, id: &str) -> Option<&ProviderRecord> {
        self.records.get(id)
    }

    pub fn providers(&self) -> impl Iterator<Item = &ProviderRecord> {
        self.records.values()
    }
}

/// A catalog plugin: a provider fold step and/or a model fold step (state may flow between them).
pub trait CatalogPlugin {
    fn providers(&mut self, _editor: &mut ProviderEditor) {}
    fn models(&mut self, _editor: &mut ModelEditor<'_>) {}
}

/// Integration access used for provider availability: registered integrations and their connection
/// keys (the active connection first).
#[derive(Debug, Clone, Default)]
pub struct Access {
    integrations: HashMap<String, Vec<String>>,
}

impl Access {
    pub fn new() -> Self {
        Self::default()
    }

    /// Register an integration with its connection keys (empty when not connected).
    pub fn integration(mut self, id: impl Into<String>, connections: Vec<String>) -> Self {
        self.integrations.insert(id.into(), connections);
        self
    }

    pub fn set(&mut self, id: impl Into<String>, connections: Vec<String>) {
        self.integrations.insert(id.into(), connections);
    }

    pub fn connections(&self, id: &str) -> Option<&Vec<String>> {
        self.integrations.get(id)
    }

    pub fn ids(&self) -> impl Iterator<Item = &String> {
        self.integrations.keys()
    }
}

/// Provider availability (`provider.ts:713-726`).
pub fn is_available(record: &ProviderRecord, access: &Access) -> bool {
    if record.provider.activation == Activation::Disabled {
        return false;
    }
    let integration_id = record.provider.integration_id.as_deref().unwrap_or(&record.provider.id);
    let integration = access.connections(integration_id);
    if let Some(source) = &record.source_connection {
        if integration.and_then(|c| c.first()) != Some(source) {
            return false;
        }
    }
    if record.provider.activation == Activation::Enabled {
        return true;
    }
    if integration.map(|c| !c.is_empty()).unwrap_or(false) {
        return true;
    }
    record.provider.integration_id.is_none() && integration.is_none()
}

/// Effective model: the model overlaid on its provider (`model.ts:191-215`).
pub fn effective_model(model: &ModelInfo, provider: Option<&ProviderInfo>) -> ModelInfo {
    let mut value = model.clone();
    if let Some(canonical) = provider.and_then(|p| p.canonical.clone()) {
        value.canonical = Some(canonical);
    }
    value.package = model.package.clone().or_else(|| provider.map(|p| p.package.clone()));
    if let Some(base) = provider.and_then(|p| p.compatibility.as_ref()) {
        value.compatibility = Some(base.merged(&model.compatibility.clone().unwrap_or_default()));
    }
    value.settings = merge_overlay(
        model_settings(provider.and_then(|p| p.settings.as_ref())).as_ref(),
        model_settings(model.settings.as_ref()).as_ref(),
    );
    value.headers = merge_headers(provider.and_then(|p| p.headers.as_ref()), model.headers.as_ref());
    value.body = merge_overlay(provider.and_then(|p| p.body.as_ref()), model.body.as_ref());
    value
}

/// Small model family priority.
pub const SMALL_MODEL_FAMILIES: &[&str] = &["gpt-luna", "gemini-flash-lite", "gemini-flash", "claude-haiku"];

/// The materialized catalog for one location. Immutable once built and cheap to clone (the data is
/// shared), so a location can keep its catalog and hand it to every request.
#[derive(Debug, Clone, Default)]
pub struct Catalog {
    inner: Arc<CatalogData>,
}

#[derive(Debug, Default)]
struct CatalogData {
    records: IndexMap<String, ProviderRecord>,
    available: Vec<String>,
    by_provider: IndexMap<String, IndexMap<String, ModelInfo>>,
    all: Vec<ModelInfo>,
    /// Indexes into `all` of the enabled models, in `Model.available` order.
    enabled: Vec<usize>,
    /// Enabled non-chat models (image, decision…) by type, newest first. They only show up when listed by
    /// type, so nothing that picks or resolves a session model can reach them.
    other: IndexMap<String, Vec<ModelInfo>>,
    default_model: Option<(String, String)>,
}

/// Signed out, only free OpenCode models are enabled. Clients default to the first available model,
/// so these free models stay ahead of each new free launch instead of letting release order pick
/// the default (`model.ts` `PREFERRED_FREE`).
pub const PREFERRED_FREE: &[&str] = &["big-pickle", "deepseek-v4.1-flash-free", "glm-5.3-flash-free", "muse-spark-1.3-contributor-free"];

fn is_free(model: &ModelInfo) -> bool {
    !model.cost.iter().any(|c| c.input > 0.0)
}

/// The effective models of the available providers, materialized (`Model.snapshot` `read`).
fn materialize(
    records: IndexMap<String, ProviderRecord>,
    available: Vec<String>,
    models: IndexMap<String, IndexMap<String, ModelInfo>>,
    default_model: Option<(String, String)>,
) -> Catalog {
    let mut by_provider: IndexMap<String, IndexMap<String, ModelInfo>> = models
        .into_iter()
        .map(|(pid, models)| {
            let provider = records.get(&pid).map(|r| &r.provider);
            let merged = models.into_iter().map(|(mid, m)| (mid, effective_model(&m, provider))).collect();
            (pid, merged)
        })
        .collect();
    if super::plugins::free_models_disabled() {
        for pid in ["opencode", "opencode-go"] {
            if let Some(models) = by_provider.get_mut(pid) {
                models.retain(|_, m| !is_free(m));
            }
        }
    }
    let mut other: IndexMap<String, Vec<ModelInfo>> = IndexMap::new();
    for models in by_provider.values_mut() {
        models.retain(|_, model| {
            let kind = model.kind.as_deref().unwrap_or("chat");
            if kind == "chat" {
                return true;
            }
            if model.enabled {
                other.entry(kind.to_string()).or_default().push(model.clone());
            }
            false
        });
    }
    let mut all: Vec<ModelInfo> = by_provider.values().flat_map(|m| m.values().cloned()).collect();
    all.sort_by(|a, b| b.time.released.partial_cmp(&a.time.released).unwrap_or(std::cmp::Ordering::Equal));
    let mut enabled: Vec<usize> = (0..all.len()).filter(|i| all[*i].enabled).collect();
    // `preferFree`: when every available model is a free OpenCode model, the preferred ones lead.
    if enabled.iter().all(|i| all[*i].provider_id == "opencode" && is_free(&all[*i])) {
        let picks: Vec<usize> =
            PREFERRED_FREE.iter().filter_map(|id| enabled.iter().copied().find(|i| all[*i].id == *id)).collect();
        let rest: Vec<usize> = enabled.iter().copied().filter(|i| !picks.contains(i)).collect();
        enabled = picks.into_iter().chain(rest).collect();
    }
    for models in other.values_mut() {
        models.sort_by(|a, b| b.time.released.partial_cmp(&a.time.released).unwrap_or(std::cmp::Ordering::Equal));
    }
    Catalog { inner: Arc::new(CatalogData { records, available, by_provider, all, enabled, other, default_model }) }
}

impl Catalog {
    /// Run the provider folds, compute availability, run the model folds and materialize.
    pub fn build(plugins: &mut [Box<dyn CatalogPlugin + '_>], access: &Access) -> Catalog {
        let mut providers = ProviderEditor::new();
        for plugin in plugins.iter_mut() {
            plugin.providers(&mut providers);
        }
        let records = providers.into_records();
        let available: Vec<String> =
            records.values().filter(|r| is_available(r, access)).map(|r| r.provider.id.clone()).collect();
        let models: IndexMap<String, IndexMap<String, ModelInfo>> =
            available.iter().map(|id| (id.clone(), records[id].models.clone())).collect();
        let mut editor = ModelEditor { models, default_model: None, records: &records };
        for plugin in plugins.iter_mut() {
            plugin.models(&mut editor);
        }
        let ModelEditor { models, default_model, .. } = editor;
        materialize(records, available, models, default_model)
    }

    /// [`Catalog::build`] with hooks between the plugins before `post` and those from `post` on (the
    /// place upstream's plugin host runs plugin `provider`/`model` transforms: after the internal
    /// `pre` plugins, before the `post` ones such as the configured-provider plugin). The hooks get
    /// and return the provider records, and the available models with the default model.
    pub async fn build_staged<P, PF, M, MF>(
        plugins: &mut [Box<dyn CatalogPlugin + Send + '_>],
        post: usize,
        access: &Access,
        provider_hook: P,
        model_hook: M,
    ) -> Catalog
    where
        P: FnOnce(IndexMap<String, ProviderRecord>) -> PF,
        PF: std::future::Future<Output = IndexMap<String, ProviderRecord>>,
        M: FnOnce(IndexMap<String, IndexMap<String, ModelInfo>>, Option<(String, String)>, IndexMap<String, ProviderRecord>) -> MF,
        MF: std::future::Future<Output = (IndexMap<String, IndexMap<String, ModelInfo>>, Option<(String, String)>)>,
    {
        let post = post.min(plugins.len());
        let mut providers = ProviderEditor::new();
        for plugin in plugins[..post].iter_mut() {
            plugin.providers(&mut providers);
        }
        providers.records = provider_hook(std::mem::take(&mut providers.records)).await;
        for plugin in plugins[post..].iter_mut() {
            plugin.providers(&mut providers);
        }
        let records = providers.into_records();
        let available: Vec<String> =
            records.values().filter(|r| is_available(r, access)).map(|r| r.provider.id.clone()).collect();
        let models: IndexMap<String, IndexMap<String, ModelInfo>> =
            available.iter().map(|id| (id.clone(), records[id].models.clone())).collect();
        let (models, default_model) = {
            let mut editor = ModelEditor { models, default_model: None, records: &records };
            for plugin in plugins[..post].iter_mut() {
                plugin.models(&mut editor);
            }
            (editor.models, editor.default_model)
        };
        let (models, default_model) = model_hook(models, default_model, records.clone()).await;
        let mut editor = ModelEditor { models, default_model, records: &records };
        for plugin in plugins[post..].iter_mut() {
            plugin.models(&mut editor);
        }
        let ModelEditor { models, default_model, .. } = editor;
        materialize(records, available, models, default_model)
    }

    /// Whether both handles share one built catalog (no rebuild happened in between).
    pub fn ptr_eq(&self, other: &Catalog) -> bool {
        Arc::ptr_eq(&self.inner, &other.inner)
    }

    /// Any provider definition (`Provider.get`), available or not.
    pub fn provider(&self, id: &str) -> Option<&ProviderInfo> {
        self.inner.records.get(id).map(|r| &r.provider)
    }

    pub fn record(&self, id: &str) -> Option<&ProviderRecord> {
        self.inner.records.get(id)
    }

    /// Every provider definition (`Provider.all`).
    pub fn providers(&self) -> impl Iterator<Item = &ProviderInfo> {
        self.inner.records.values().map(|r| &r.provider)
    }

    /// Available providers (`Provider.available`).
    pub fn available_providers(&self) -> Vec<&ProviderInfo> {
        self.inner.available.iter().filter_map(|id| self.provider(id)).collect()
    }

    pub fn is_available(&self, provider_id: &str) -> bool {
        self.inner.available.iter().any(|id| id == provider_id)
    }

    /// Integration owning a provider's credentials (`integrationID ?? providerID`).
    pub fn integration_id(&self, provider_id: &str) -> String {
        self.provider(provider_id).and_then(|p| p.integration_id.clone()).unwrap_or_else(|| provider_id.to_string())
    }

    /// Effective model (`Model.get`).
    pub fn model(&self, provider_id: &str, model_id: &str) -> Option<&ModelInfo> {
        self.inner.by_provider.get(provider_id).and_then(|m| m.get(model_id))
    }

    /// Every model of available providers, newest release first (`Model.all`).
    pub fn models(&self) -> &[ModelInfo] {
        &self.inner.all
    }

    /// Models of one available provider in definition order.
    pub fn provider_models(&self, provider_id: &str) -> Vec<&ModelInfo> {
        self.inner.by_provider.get(provider_id).map(|m| m.values().collect()).unwrap_or_default()
    }

    /// Enabled models, newest first, with the preferred free models first when only free OpenCode
    /// models are available (`Model.available`).
    pub fn available_models(&self) -> Vec<&ModelInfo> {
        self.inner.enabled.iter().map(|i| &self.inner.all[*i]).collect()
    }

    /// `Model.available(types)`: enabled models of the given types, chat when none are given. Each
    /// distinct type is listed once, in request order; only chat models are offered for sessions.
    pub fn available_models_of(&self, types: &[String]) -> Vec<&ModelInfo> {
        if types.is_empty() {
            return self.available_models();
        }
        let mut seen: Vec<&str> = vec![];
        let mut out = vec![];
        for kind in types {
            if seen.contains(&kind.as_str()) {
                continue;
            }
            seen.push(kind);
            if kind == "chat" {
                out.extend(self.available_models());
            } else if let Some(models) = self.inner.other.get(kind) {
                out.extend(models.iter());
            }
        }
        out
    }

    /// Configured default selection, if any.
    pub fn configured_default(&self) -> Option<&(String, String)> {
        self.inner.default_model.as_ref()
    }

    /// `Model.default`: the configured model when enabled, else the newest available text model.
    pub fn default_model(&self) -> Option<&ModelInfo> {
        if let Some((pid, mid)) = &self.inner.default_model {
            if let Some(model) = self.model(pid, mid) {
                if model.enabled {
                    return Some(model);
                }
            }
        }
        self.available_models().into_iter().find(|m| supports_text(&m.capabilities))
    }

    /// `Model.small(providerID)`: first active text model of the highest-priority small family.
    pub fn small_model(&self, provider_id: &str) -> Option<&ModelInfo> {
        let candidates: Vec<&ModelInfo> = self
            .available_models()
            .into_iter()
            .filter(|m| {
                m.provider_id == provider_id
                    && m.status == ModelStatus::Active
                    && m.capabilities.input.iter().any(|i| i.starts_with("text"))
                    && m.capabilities.output.iter().any(|i| i.starts_with("text"))
            })
            .collect();
        SMALL_MODEL_FAMILIES
            .iter()
            .find_map(|family| candidates.iter().find(|m| m.family.as_deref() == Some(*family)).copied())
    }
}

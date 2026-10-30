//! Provider registry, models.dev catalog, custom providers, variants and model resolution (spec §5).
//!
//! * [`types`]: `Provider.Info` / `Model.Info` data model and overlay helpers.
//! * [`models_dev`]: models.dev fetch/cache/snapshot and normalization.
//! * [`native`]: AI SDK npm package → native package mapping and settings translation.
//! * [`variant`]: reasoning variants per package.
//! * [`registry`]: provider/model folds, availability, effective models, default and small model.
//! * [`config`]: custom providers from config and the v1 `provider` migration.
//! * [`plugins`]: data-only effects of core provider plugins.
//! * [`openai`]: the Codex, ChatGPT token-sharing and ChatGPT input-cap effects of the OpenAI sign-ins.
//! * [`resolve`]: catalog model → runtime [`crate::LanguageModel`].
//!
//! Typical use:
//!
//! ```ignore
//! let data = models_dev.get().await?;
//! let mut plugins: Vec<Box<dyn CatalogPlugin>> = standard_plugins(&data, configured, ConfigProviders::from_value(&cfg));
//! let catalog = Catalog::build(&mut plugins, &access);
//! let resolved = catalog.resolve("anthropic", "claude-sonnet-4-5", Some("high"), credential.as_ref())?;
//! ```

pub mod config;
pub mod copilot;
pub mod models_dev;
pub mod models_dev_v1;
pub mod native;
pub mod openai;
pub mod plugins;
pub mod registry;
pub mod resolve;
pub mod transforms;
pub mod types;
pub mod variant;

pub use config::{migrate_v1_providers, ConfigModel, ConfigProvider, ConfigProviders, ModelSelection};
pub use models_dev::{CatalogCache, IntegrationMethod, IntegrationSpec, MemoryCache, ModelsDev, ModelsDevOptions, Snapshot};
pub use plugins::{DataPlugins, ModelsDevPlugin};
pub use registry::{Access, Catalog, CatalogPlugin, ModelEditor, ProviderEditor, ProviderRecord};
pub use resolve::{from_catalog_model, with_variant, PackageLoader, ResolveError, ResolveOptions, ResolvedModel};
pub use types::{
    parse_model, parse_model_ref, Activation, Capabilities, Cost, CostCache, CostTier, HeaderMap, Limit, ModelInfo,
    ModelStatus, ModelTime, ProviderInfo, Variant,
};

use std::collections::HashSet;

/// The upstream plugin order for catalog data: models.dev, data-only provider plugins, then configured
/// providers. `configured` lists registered integration ids (LLM Gateway attribution). Other provider
/// plugins can be inserted between [`DataPlugins`] and [`ConfigProviders`].
pub fn standard_plugins<'a>(
    data: &[Snapshot],
    configured: HashSet<String>,
    config: ConfigProviders,
) -> Vec<Box<dyn CatalogPlugin + 'a>> {
    vec![
        Box::new(ModelsDevPlugin::new(data)),
        Box::new(DataPlugins::new(configured)),
        Box::new(plugins::ResponsesWebSockets),
        Box::new(config),
    ]
}

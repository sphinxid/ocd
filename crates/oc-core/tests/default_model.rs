//! Session model selection without a session model (`SessionRunnerModel.resolve` → `ModelResolver.resolve()` →
//! `Model.default`): a configured model the catalog does not have falls back to the first available text model.

use std::sync::Arc;

use oc_config::schema::{Entry, Info};
use oc_core::models::Models;
use oc_schema::types::ModelRef;
use serde_json::json;

const KEY: &str = "/loc";

fn models(home: &std::path::Path) -> Arc<Models> {
    // SAFETY: every test of this binary sets the same values.
    unsafe {
        std::env::set_var("OPENCODE_DISABLE_MODELS_FETCH", "1");
        std::env::set_var("OPENCODE_TEST_HOME", home);
        for name in ["OPENAI_API_KEY", "OPENCODE_API_KEY", "ANTHROPIC_API_KEY", "GITHUB_TOKEN"] {
            std::env::remove_var(name);
        }
    }
    let db = oc_storage::Database::memory().unwrap();
    let bus = oc_core::bus::Bus::new(db.clone());
    Arc::new(Models::new(db, bus, "latest", "1.2.3"))
}

fn model_ref(provider: &str, id: &str) -> ModelRef {
    ModelRef { id: id.into(), provider_id: provider.into(), variant: None }
}

#[tokio::test]
async fn a_missing_configured_model_falls_back_to_the_default() {
    let home = tempfile::tempdir().unwrap();
    let models = models(home.path());
    let config = vec![Entry::document(None, Info::from_value(&json!({"model": "github-copilot/gpt-5.4"})).unwrap())];
    let catalog = models.catalog_at(&config, Some(KEY)).await.unwrap();
    assert!(catalog.model("github-copilot", "gpt-5.4").is_none());
    let fallback = catalog.select(None).expect("an available text model");

    let configured = model_ref("github-copilot", "gpt-5.4");
    let resolved = models.resolve_for_session(Some(KEY), &catalog, "ses_1", None, Some(&configured)).await.unwrap();
    assert_eq!(resolved.model_ref().provider_id, fallback.provider_id);
    assert_eq!(resolved.model_ref().id, fallback.id);

    // An explicitly selected session model is still required to exist.
    let error = models.resolve_for_session(Some(KEY), &catalog, "ses_1", Some(&configured), None).await.unwrap_err();
    assert_eq!(error.to_string(), "Model unavailable: github-copilot/gpt-5.4");
}

#[tokio::test]
async fn an_available_configured_model_keeps_its_variant() {
    let home = tempfile::tempdir().unwrap();
    let models = models(home.path());
    let catalog = models.catalog_at(&[], Some(KEY)).await.unwrap();
    let available = catalog.select(None).expect("an available text model");
    let configured =
        ModelRef { id: available.id.clone(), provider_id: available.provider_id.clone(), variant: Some("default".into()) };
    let resolved = models.resolve_for_session(Some(KEY), &catalog, "ses_1", None, Some(&configured)).await.unwrap();
    assert_eq!(resolved.model_ref().id, available.id);
    assert_eq!(resolved.model_ref().variant.as_deref(), Some("default"));
}

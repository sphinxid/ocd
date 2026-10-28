//! Model variant resolution and labels (`src/mini/variant.shared.ts`), plus `cycleModelVariant` /
//! `normalizeModelVariant` from `model-preference.ts`.

use super::session_shared::RunSession;
use super::types::{ModelSel, RunProvider};

/// `modelInfo(providers, model)`: provider and model display names.
pub fn model_info(providers: Option<&[RunProvider]>, model: &ModelSel) -> (String, String) {
    let provider = providers.and_then(|p| p.iter().find(|p| p.id == model.provider_id));
    (
        provider.map(|p| p.name.clone()).unwrap_or_else(|| model.provider_id.clone()),
        provider.and_then(|p| p.models.get(&model.model_id)).and_then(|m| m.name.clone()).unwrap_or_else(|| model.model_id.clone()),
    )
}

/// `formatModelLabel(model, variant, providers)`: `Model · Provider[ · variant]`.
pub fn format_model_label(model: &ModelSel, variant: Option<&str>, providers: Option<&[RunProvider]>) -> String {
    let (provider, name) = model_info(providers, model);
    let label = variant.map(|v| format!(" · {v}")).unwrap_or_default();
    format!("{name} · {provider}{label}")
}

pub fn normalize_model_variant(value: Option<&str>) -> Option<String> {
    value.filter(|v| *v != "default").map(|s| s.to_string())
}

/// `cycleVariant(current, variants)`.
pub fn cycle_variant(current: Option<&str>, variants: &[String]) -> Option<String> {
    crate::state::cycle_variant(current, variants)
}

/// `sessionVariant(session, model)`.
pub fn session_variant(session: &RunSession, model: Option<&ModelSel>) -> Option<String> {
    let model = model?;
    if session.model.as_ref() == Some(model) {
        return session.variant.clone();
    }
    session.turns.iter().rev().find(|t| t.provider.as_deref() == Some(&model.provider_id) && t.model.as_deref() == Some(&model.model_id)).and_then(|t| t.variant.clone())
}

/// `pickVariant(model, session)`.
pub fn pick_variant(model: Option<&ModelSel>, session: &RunSession) -> Option<String> {
    session_variant(session, model)
}

fn fit_variant(value: Option<&str>, variants: &[String]) -> Option<String> {
    let normalized = normalize_model_variant(value)?;
    (variants.is_empty() || variants.contains(&normalized)).then_some(normalized)
}

/// `resolveVariant(input, session, saved, variants)`: CLI flag, then valid session history, then saved.
pub fn resolve_variant(input: Option<&str>, session: Option<&str>, saved: Option<&str>, variants: &[String]) -> Option<String> {
    if input.is_some() {
        return normalize_model_variant(input);
    }
    fit_variant(session, variants).or_else(|| fit_variant(saved, variants))
}

#[cfg(test)]
mod tests {
    use super::super::session_shared::Turn;
    use super::super::types::{RunPrompt, RunProviderModel};
    use super::*;

    fn model() -> ModelSel {
        ModelSel { provider_id: "openai".into(), model_id: "gpt-5".into() }
    }

    fn providers() -> Vec<RunProvider> {
        let mut models = indexmap::IndexMap::new();
        models.insert("gpt-5".into(), RunProviderModel { name: Some("GPT-5".into()), ..Default::default() });
        vec![RunProvider { id: "openai".into(), name: "OpenAI".into(), models }]
    }

    fn v(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn prefers_cli_then_session_then_saved() {
        assert_eq!(resolve_variant(Some("max"), Some("high"), Some("low"), &v(&["low", "high"])).as_deref(), Some("max"));
        assert_eq!(resolve_variant(Some("default"), Some("high"), Some("low"), &v(&["low", "high"])), None);
        assert_eq!(resolve_variant(None, Some("high"), Some("low"), &v(&["low", "high"])).as_deref(), Some("high"));
        assert_eq!(resolve_variant(None, Some("missing"), Some("low"), &v(&["low", "high"])).as_deref(), Some("low"));
    }

    #[test]
    fn cycles_and_back_to_default() {
        assert_eq!(cycle_variant(None, &v(&["low", "high"])).as_deref(), Some("low"));
        assert_eq!(cycle_variant(Some("default"), &v(&["low", "high"])).as_deref(), Some("low"));
        assert_eq!(cycle_variant(Some("low"), &v(&["low", "high"])).as_deref(), Some("high"));
        assert_eq!(cycle_variant(Some("high"), &v(&["low", "high"])), None);
        assert_eq!(cycle_variant(None, &[]), None);
    }

    #[test]
    fn formats_labels() {
        assert_eq!(format_model_label(&model(), None, None), "gpt-5 · openai");
        assert_eq!(format_model_label(&model(), Some("high"), None), "gpt-5 · openai · high");
        let p = providers();
        assert_eq!(format_model_label(&model(), None, Some(&p)), "GPT-5 · OpenAI");
        assert_eq!(format_model_label(&model(), Some("high"), Some(&p)), "GPT-5 · OpenAI · high");
    }

    #[test]
    fn picks_latest_matching_variant() {
        let turn = |text: &str, provider: &str, model: &str, variant: &str| Turn {
            prompt: RunPrompt::text(text),
            provider: Some(provider.into()),
            model: Some(model.into()),
            variant: Some(variant.into()),
        };
        let session = RunSession {
            first: false,
            turns: vec![turn("one", "openai", "gpt-5", "high"), turn("two", "anthropic", "sonnet", "max"), turn("three", "openai", "gpt-5", "minimal")],
            model: None,
            variant: None,
        };
        assert_eq!(pick_variant(Some(&model()), &session).as_deref(), Some("minimal"));
    }
}

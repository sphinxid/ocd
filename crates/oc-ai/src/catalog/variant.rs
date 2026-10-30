//! Reasoning variants per native package (`packages/core/src/variant.ts`).

use super::types::{ModelInfo, Variant};
use crate::json::{num, Object};
use once_cell::sync::Lazy;
use regex::Regex;
use serde_json::{json, Value};

/// `Variant.Support` (models.dev `reasoning_options`).
#[derive(Debug, Clone, PartialEq)]
pub enum Support {
    Effort { values: Option<Vec<String>> },
    Toggle,
    BudgetTokens { min: Option<f64>, max: Option<f64> },
}

impl Support {
    pub fn effort() -> Self {
        Self::Effort { values: None }
    }
    pub fn efforts(values: &[&str]) -> Self {
        Self::Effort { values: Some(values.iter().map(|s| s.to_string()).collect()) }
    }
    fn kind(&self) -> u8 {
        match self {
            Self::Effort { .. } => 0,
            Self::Toggle => 1,
            Self::BudgetTokens { .. } => 2,
        }
    }
}

type Variants = Vec<Variant>;

#[derive(Default, Clone)]
struct Overlay {
    settings: Option<Value>,
    body: Option<Value>,
}

fn settings(value: Value) -> Overlay {
    Overlay { settings: Some(value), body: None }
}
fn body(value: Value) -> Overlay {
    Overlay { settings: None, body: Some(value) }
}

fn variant(id: &str, overlay: Overlay) -> Variant {
    let to_obj = |v: Option<Value>| v.and_then(|v| v.as_object().cloned());
    Variant { id: id.into(), settings: to_obj(overlay.settings), headers: None, body: to_obj(overlay.body) }
}

fn efforts(values: &[String], spell: impl Fn(&str) -> Overlay) -> Variants {
    values.iter().map(|effort| variant(effort, spell(effort))).collect()
}

fn toggle(off: Overlay, on: Overlay) -> Variants {
    vec![variant("none", off), variant("thinking", on)]
}

fn strs(values: &[&str]) -> Vec<String> {
    values.iter().map(|s| s.to_string()).collect()
}

const EFFORTS: &[&str] = &["low", "medium", "high"];
const ANTHROPIC_OUTPUT_TOKEN_MAX: f64 = 32_000.0;
const ALIBABA_THINKING_BUDGET_MAX: f64 = 64_000.0;

fn adaptive() -> Value {
    json!({"type": "adaptive", "display": "summarized"})
}

fn encrypted_reasoning() -> Value {
    json!(["reasoning.encrypted_content"])
}

/// `Variant.resolve(model, supports)`; pass `&[Support::effort()]` for the upstream default argument.
pub fn resolve(model: &ModelInfo, supports: &[Support]) -> Variants {
    let Some(protocol) = model.package.as_deref().and_then(protocol_for) else { return vec![] };
    let toggle_variants =
        if supports.iter().any(|s| matches!(s, Support::Toggle)) { protocol(model, &Support::Toggle) } else { vec![] };
    let effort = supports.iter().find(|s| s.kind() == 0);
    let budget = supports.iter().find(|s| s.kind() == 2);
    let main = match (effort, budget) {
        (Some(e), _) => protocol(model, e),
        (None, Some(b)) => protocol(model, b),
        (None, None) => toggle_variants.clone(),
    };
    let mut variants: Variants = toggle_variants.into_iter().filter(|v| v.id == "none").collect();
    variants.extend(main);
    let mut seen = std::collections::HashSet::new();
    variants.into_iter().filter(|v| seen.insert(v.id.clone())).collect()
}

/// `Variant.supports`: reasoning controls in models.dev v2's shape (`Config.Model.Reasoning`), where an
/// effort of `default` is the provider default rather than a variant.
pub fn supports(reasoning: &super::config::ConfigReasoning) -> Vec<Support> {
    if !reasoning.supported {
        return vec![];
    }
    let mut out = vec![];
    if reasoning.toggle == Some(true) {
        out.push(Support::Toggle);
    }
    if let Some(effort) = &reasoning.effort {
        out.push(Support::Effort { values: Some(effort.iter().filter(|v| v.as_str() != "default").cloned().collect()) });
    }
    if let Some(budget) = &reasoning.budget {
        out.push(Support::BudgetTokens { min: budget.min.map(|v| v as f64), max: budget.max.map(|v| v as f64) });
    }
    out
}

/// `resolve` with the default `[{type: "effort"}]` support list.
pub fn resolve_default(model: &ModelInfo) -> Variants {
    resolve(model, &[Support::effort()])
}

fn budgets(model: &ModelInfo, min: Option<f64>, max: Option<f64>, spell: impl Fn(f64) -> Overlay, ceiling: Option<f64>) -> Variants {
    let output = model.limit.output as f64;
    let ceiling = ceiling.unwrap_or(output);
    let maximum = max.unwrap_or(ceiling - 1.0).min(output - 1.0).min(ceiling - 1.0);
    if maximum <= 0.0 {
        return vec![];
    }
    let high = min.unwrap_or(0.0).max(((maximum + 1.0) / 2.0).floor()).min(maximum);
    vec![variant("high", spell(high)), variant("max", spell(maximum))]
}

fn model_id(model: &ModelInfo) -> &str {
    &model.model_id
}

struct ClaudeInfo {
    family: Option<String>,
    major: Option<f64>,
    minor: Option<f64>,
    manual: bool,
    always: bool,
}

static FAMILY_FIRST: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"(?i)(?:claude-)?(opus|sonnet|haiku|fable|mythos)-(\d+)(?:[.-](\d+))?").unwrap());
static VERSION_FIRST: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"(?i)claude-(\d+)(?:[.-](\d+))?-(opus|sonnet|haiku|fable|mythos)").unwrap());
static GLM_VERSION: Lazy<Regex> = Lazy::new(|| Regex::new(r"glm-?(\d+)(?:(?:[.-]|p)(\d+))?").unwrap());
static MINIMAX_M2: Lazy<Regex> = Lazy::new(|| Regex::new(r"(?i)minimax[-.]?m2(?:[.-]|$)").unwrap());
static KIMI_K2: Lazy<Regex> = Lazy::new(|| Regex::new(r"(?i)kimi[-.]?k2").unwrap());
static KIMI_K27_CODE: Lazy<Regex> = Lazy::new(|| Regex::new(r"(?i)kimi[-.]?k2[.-]7-code").unwrap());

fn number(s: Option<&str>) -> Option<f64> {
    s.and_then(|s| s.parse::<f64>().ok())
}

fn claude_info(model: &ModelInfo) -> ClaudeInfo {
    let id = model_id(model);
    let family_first = FAMILY_FIRST.captures(id);
    let version_first = VERSION_FIRST.captures(id);
    let group = |c: &Option<regex::Captures<'_>>, i: usize| c.as_ref().and_then(|c| c.get(i)).map(|m| m.as_str().to_string());
    let family = group(&family_first, 1).or_else(|| group(&version_first, 3)).map(|f| f.to_lowercase());
    let major = number(group(&family_first, 2).or_else(|| group(&version_first, 1)).as_deref());
    let minor = match group(&family_first, 3).or_else(|| group(&version_first, 2)) {
        Some(s) => number(Some(&s)),
        None => Some(0.0),
    };
    let manual = (major == Some(3.0) && minor == Some(7.0)) || (major == Some(4.0) && minor.map(|m| m < 6.0).unwrap_or(false));
    let always = matches!(family.as_deref(), Some("fable") | Some("mythos")) || id.to_lowercase().contains("mythos-preview");
    ClaudeInfo { family, major, minor, manual, always }
}

fn manual_thinking(model: &ModelInfo) -> Option<Overlay> {
    let tokens = 16_000f64.min(model.limit.output as f64 - 1.0);
    if tokens < 1024.0 {
        return None;
    }
    Some(settings(json!({"thinking": {"type": "enabled", "budgetTokens": num(tokens)}})))
}

type Protocol = fn(&ModelInfo, &Support) -> Variants;

fn openai_chat(_: &ModelInfo, support: &Support) -> Variants {
    let Support::Effort { values } = support else { return vec![] };
    efforts(&values.clone().unwrap_or_else(|| strs(EFFORTS)), |e| settings(json!({"reasoningEffort": e})))
}

fn vertex_chat(model: &ModelInfo, support: &Support) -> Variants {
    if !matches!(support, Support::Toggle) {
        return openai_chat(model, support);
    }
    let key = if model_id(model).to_lowercase().contains("deepseek") { "thinking" } else { "enable_thinking" };
    toggle(body(json!({"chat_template_kwargs": {key: false}})), body(json!({"chat_template_kwargs": {key: true}})))
}

fn responses_effort(effort: &str) -> Overlay {
    settings(json!({"reasoningEffort": effort, "reasoningSummary": "auto", "include": encrypted_reasoning()}))
}

fn openai_responses(_: &ModelInfo, support: &Support) -> Variants {
    let Support::Effort { values } = support else { return vec![] };
    efforts(&values.clone().unwrap_or_else(|| strs(&["none", "minimal", "low", "medium", "high", "xhigh"])), responses_effort)
}

fn open_responses(_: &ModelInfo, support: &Support) -> Variants {
    let Support::Effort { values } = support else { return vec![] };
    efforts(&values.clone().unwrap_or_else(|| strs(EFFORTS)), responses_effort)
}

fn google_interactions(_: &ModelInfo, support: &Support) -> Variants {
    let Support::Effort { values } = support else { return vec![] };
    efforts(&values.clone().unwrap_or_else(|| strs(EFFORTS)), |e| {
        settings(json!({"thinkingLevel": e, "thinkingSummaries": "auto"}))
    })
}

fn cloudflare_ai_gateway(model: &ModelInfo, support: &Support) -> Variants {
    let id = model_id(model);
    if id.starts_with("openai/") {
        return openai_responses(model, support);
    }
    if id.starts_with("anthropic/") {
        return anthropic_messages(model, support);
    }
    openai_chat(model, support)
}

fn deepseek_chat(_: &ModelInfo, support: &Support) -> Variants {
    match support {
        Support::Effort { values } => efforts(&values.clone().unwrap_or_else(|| strs(&["low", "high", "max"])), |e| {
            if e == "none" {
                body(json!({"thinking": {"type": "disabled"}}))
            } else {
                Overlay { settings: Some(json!({"reasoningEffort": e})), body: Some(json!({"thinking": {"type": "enabled"}})) }
            }
        }),
        Support::Toggle => toggle(body(json!({"thinking": {"type": "disabled"}})), body(json!({"thinking": {"type": "enabled"}}))),
        Support::BudgetTokens { .. } => vec![],
    }
}

fn nvidia_template(kwargs: Value) -> Overlay {
    body(json!({"chat_template_kwargs": kwargs}))
}

fn nvidia_toggle(key: &str) -> Variants {
    toggle(nvidia_template(json!({key: false})), nvidia_template(json!({key: true})))
}

fn nvidia_chat(model: &ModelInfo, support: &Support) -> Variants {
    let id = model_id(model).to_lowercase();
    if id.contains("deepseek-v4") {
        if matches!(support, Support::Toggle) {
            return nvidia_toggle("thinking");
        }
        let Support::Effort { values } = support else { return vec![] };
        return efforts(&values.clone().unwrap_or_else(|| strs(&["none", "high", "max"])), |e| {
            if e == "none" {
                nvidia_template(json!({"thinking": false}))
            } else {
                nvidia_template(json!({"thinking": true, "reasoning_effort": e}))
            }
        });
    }
    if id.contains("kimi-k2.6") || id.contains("kimi-k2-6") {
        return nvidia_toggle("thinking");
    }
    if id.contains("kimi-k3") {
        if matches!(support, Support::Toggle) {
            return nvidia_toggle("thinking");
        }
        let Support::Effort { values } = support else { return vec![] };
        return efforts(&values.clone().unwrap_or_else(|| strs(&["low", "high", "max"])), |e| settings(json!({"reasoningEffort": e})));
    }
    if id.contains("minimax-m3") {
        return toggle(nvidia_template(json!({"thinking_mode": "disabled"})), nvidia_template(json!({"thinking_mode": "enabled"})));
    }
    if id.contains("glm") {
        return toggle(
            nvidia_template(json!({"enable_thinking": false})),
            nvidia_template(json!({"enable_thinking": true, "clear_thinking": false})),
        );
    }
    let toggle_only = id.contains("qwen") || id.contains("gemma") || id.contains("nemotron");
    match support {
        Support::Effort { values } => {
            if toggle_only && values.is_none() { nvidia_toggle("enable_thinking") } else { openai_chat(model, support) }
        }
        Support::Toggle => nvidia_toggle("enable_thinking"),
        Support::BudgetTokens { min, max } => budgets(
            model,
            *min,
            *max,
            |t| body(json!({"chat_template_kwargs": {"enable_thinking": true}, "reasoning_budget": num(t)})),
            None,
        ),
    }
}

fn workers_ai_template(thinking: bool) -> Overlay {
    body(json!({"chat_template_kwargs": {"enable_thinking": thinking, "thinking": thinking}}))
}

fn workers_ai_chat(_: &ModelInfo, support: &Support) -> Variants {
    match support {
        Support::Effort { values } => efforts(&values.clone().unwrap_or_else(|| strs(EFFORTS)), |e| {
            if e == "none" { workers_ai_template(false) } else { settings(json!({"reasoningEffort": e})) }
        }),
        Support::Toggle => toggle(workers_ai_template(false), workers_ai_template(true)),
        Support::BudgetTokens { .. } => vec![],
    }
}

fn baseten_template(enable: bool) -> Value {
    json!({"chat_template_args": {"enable_thinking": enable}})
}

fn baseten_chat(model: &ModelInfo, support: &Support) -> Variants {
    let id = model_id(model).to_lowercase();
    match support {
        Support::Effort { values } => {
            let glm = id.contains("glm-5.2") || id.contains("glm-5-2") || id.contains("glm-5p2");
            efforts(&values.clone().unwrap_or_else(|| strs(EFFORTS)), |e| {
                let mut overlay = settings(json!({"reasoningEffort": e}));
                if glm {
                    overlay.body = Some(baseten_template(e != "none"));
                }
                if id.contains("deepseek-v4-pro-0813") && e != "none" {
                    overlay.body = Some(json!({"thinking": {"type": "enabled"}}));
                }
                overlay
            })
        }
        Support::Toggle => toggle(body(baseten_template(false)), body(baseten_template(true))),
        Support::BudgetTokens { .. } => vec![],
    }
}

fn deepinfra_chat(model: &ModelInfo, support: &Support) -> Variants {
    if KIMI_K27_CODE.is_match(model_id(model)) {
        return vec![];
    }
    match support {
        Support::Effort { values } => efforts(&values.clone().unwrap_or_else(|| strs(EFFORTS)), |e| settings(json!({"reasoningEffort": e}))),
        Support::Toggle => toggle(body(json!({"reasoning": {"enabled": false}})), body(json!({"reasoning": {"enabled": true}}))),
        Support::BudgetTokens { .. } => vec![],
    }
}

fn openai_compatible(model: &ModelInfo, support: &Support) -> Variants {
    if model.provider_id == "nvidia" {
        return nvidia_chat(model, support);
    }
    openai_chat(model, support)
}

fn venice_chat(_: &ModelInfo, support: &Support) -> Variants {
    match support {
        Support::Effort { values } => efforts(&values.clone().unwrap_or_else(|| strs(EFFORTS)), |e| settings(json!({"reasoningEffort": e}))),
        Support::Toggle => toggle(settings(json!({"reasoning": {"enabled": false}})), settings(json!({"reasoning": {"enabled": true}}))),
        Support::BudgetTokens { .. } => vec![],
    }
}

fn thinking_type(kind: &str) -> Overlay {
    settings(json!({"thinking": {"type": kind}}))
}

fn moonshot_chat(model: &ModelInfo, support: &Support) -> Variants {
    let id = model_id(model).to_lowercase();
    let toggleable = id.contains("k2.5") || id.contains("k2-5") || id.contains("k2.6") || id.contains("k2-6");
    let fixed = id.contains("k2-thinking") || id.contains("k2.7-code") || id.contains("k2-7-code");
    if fixed {
        return vec![];
    }
    let default_effort = matches!(support, Support::Effort { values: None });
    if toggleable && (matches!(support, Support::Toggle) || default_effort) {
        return toggle(thinking_type("disabled"), thinking_type("enabled"));
    }
    let Support::Effort { values } = support else { return vec![] };
    if toggleable {
        return vec![];
    }
    efforts(&values.clone().unwrap_or_else(|| strs(&["low", "high", "max"])), |e| settings(json!({"reasoningEffort": e})))
}

fn alibaba_chat(model: &ModelInfo, support: &Support) -> Variants {
    let id = model_id(model).to_lowercase();
    let fixed = id.contains("-thinking") || id.contains("qwq") || id.contains("deepseek-r1") || id.contains("minimax-m2");
    match support {
        Support::Effort { values } => {
            if fixed && values.is_none() {
                return vec![];
            }
            let hosted = id.contains("glm") || id.contains("deepseek");
            let values = values.clone().unwrap_or_else(|| if hosted { strs(&["high", "max"]) } else { strs(&["low", "medium", "xhigh"]) });
            efforts(&values, |e| {
                if e == "none" {
                    settings(json!({"enableThinking": false}))
                } else {
                    settings(json!({"enableThinking": true, "reasoningEffort": e}))
                }
            })
        }
        Support::Toggle => toggle(settings(json!({"enableThinking": false})), settings(json!({"enableThinking": true}))),
        Support::BudgetTokens { min, max } => budgets(
            model,
            *min,
            *max,
            |t| settings(json!({"enableThinking": true, "thinkingBudget": num(t)})),
            Some(ALIBABA_THINKING_BUDGET_MAX),
        ),
    }
}

fn zai_chat(model: &ModelInfo, support: &Support) -> Variants {
    let id = model_id(model).to_lowercase();
    let version = GLM_VERSION.captures(&id);
    let major = number(version.as_ref().and_then(|c| c.get(1)).map(|m| m.as_str()));
    let minor = number(version.as_ref().and_then(|c| c.get(2)).map(|m| m.as_str())).unwrap_or(0.0);
    let coding = model.package.as_deref().map(|p| p.contains("zai-coding-plan")).unwrap_or(false);
    let latest = coding || major.is_none() || major.map(|m| m > 5.0).unwrap_or(false) || (major == Some(5.0) && minor >= 3.0);
    let effort = major == Some(5.0) && minor == 2.0;
    let default_effort = matches!(support, Support::Effort { values: None });
    if matches!(support, Support::Toggle) || (!latest && !effort && default_effort) {
        if latest {
            return vec![];
        }
        return toggle(
            thinking_type("disabled"),
            settings(json!({"thinking": {"type": "enabled", "clear_thinking": false}})),
        );
    }
    let Support::Effort { values } = support else { return vec![] };
    let defaults = if latest { strs(&["low", "high", "max"]) } else { strs(&["high", "max"]) };
    efforts(&values.clone().unwrap_or(defaults), |value| {
        if value == "none" || value == "minimal" {
            thinking_type("disabled")
        } else {
            settings(json!({"thinking": {"type": "enabled", "clear_thinking": false}, "reasoningEffort": value}))
        }
    })
}

fn with_effort(thinking: &Overlay, effort: &str) -> Overlay {
    let mut s = thinking.settings.as_ref().and_then(|v| v.as_object().cloned()).unwrap_or_default();
    s.insert("effort".into(), json!(effort));
    settings(Value::Object(s))
}

fn anthropic_messages(model: &ModelInfo, support: &Support) -> Variants {
    let info = claude_info(model);
    let opus45 = info.family.as_deref() == Some("opus") && info.major == Some(4.0) && info.minor == Some(5.0);
    match support {
        Support::Effort { values } => {
            if info.manual && !opus45 {
                return anthropic_messages(model, &Support::BudgetTokens { min: Some(1024.0), max: None });
            }
            let thinking = if opus45 { manual_thinking(model) } else { Some(settings(json!({"thinking": adaptive()}))) };
            let Some(thinking) = thinking else { return vec![] };
            let defaults = if info.major == Some(4.0) && info.minor == Some(6.0) {
                strs(&["low", "medium", "high", "max"])
            } else {
                strs(&["low", "medium", "high", "xhigh", "max"])
            };
            let values: Vec<String> =
                values.clone().unwrap_or(defaults).into_iter().filter(|e| !info.always || e != "none").collect();
            efforts(&values, |e| if e == "none" { thinking_type("disabled") } else { with_effort(&thinking, e) })
        }
        Support::Toggle => {
            if info.always {
                return vec![];
            }
            let thinking = if info.manual { manual_thinking(model) } else { Some(settings(json!({"thinking": adaptive()}))) };
            let Some(thinking) = thinking else { return vec![] };
            toggle(thinking_type("disabled"), thinking)
        }
        Support::BudgetTokens { min, max } => budgets(
            model,
            *min,
            *max,
            |t| settings(json!({"thinking": {"type": "enabled", "budgetTokens": num(t)}})),
            Some(ANTHROPIC_OUTPUT_TOKEN_MAX),
        ),
    }
}

fn minimax_messages(model: &ModelInfo, support: &Support) -> Variants {
    if MINIMAX_M2.is_match(model_id(model)) {
        return vec![];
    }
    let configurable = matches!(support, Support::Toggle) || matches!(support, Support::Effort { values: None });
    if !configurable {
        return vec![];
    }
    toggle(thinking_type("disabled"), thinking_type("adaptive"))
}

fn moonshot_messages(model: &ModelInfo, support: &Support) -> Variants {
    let Support::Effort { values } = support else { return vec![] };
    if KIMI_K2.is_match(model_id(model)) {
        return vec![];
    }
    efforts(&values.clone().unwrap_or_else(|| strs(&["low", "high", "max"])), |e| settings(json!({"effort": e})))
}

fn alibaba_messages(model: &ModelInfo, support: &Support) -> Variants {
    let id = model_id(model).to_lowercase();
    match support {
        Support::Effort { values } => {
            let hosted = id.contains("glm") || id.contains("deepseek");
            let values = values.clone().unwrap_or_else(|| if hosted { strs(&["high", "max"]) } else { strs(&["low", "medium", "xhigh"]) });
            efforts(&values, |e| {
                if e == "none" {
                    thinking_type("disabled")
                } else {
                    settings(json!({"thinking": {"type": "enabled"}, "effort": e}))
                }
            })
        }
        Support::Toggle => toggle(thinking_type("disabled"), thinking_type("enabled")),
        Support::BudgetTokens { min, max } => budgets(
            model,
            *min,
            *max,
            |t| settings(json!({"thinking": {"type": "enabled", "budgetTokens": num(t)}})),
            Some(ALIBABA_THINKING_BUDGET_MAX),
        ),
    }
}

fn zai_messages(model: &ModelInfo, support: &Support) -> Variants {
    let id = model_id(model).to_lowercase();
    let forced = id.contains("glm-5.3") || id.contains("glm-5-3") || id.contains("glm-5p3");
    match support {
        Support::Effort { values } => {
            let defaults = if forced { strs(&["low", "high", "max"]) } else { strs(&["high", "max"]) };
            efforts(&values.clone().unwrap_or(defaults), |e| {
                let kind = if e == "none" || e == "minimal" { "disabled" } else { "enabled" };
                settings(json!({"thinking": {"type": kind}, "effort": e}))
            })
        }
        Support::Toggle => {
            if forced { vec![] } else { toggle(thinking_type("disabled"), thinking_type("enabled")) }
        }
        Support::BudgetTokens { .. } => vec![],
    }
}

fn gemini(model: &ModelInfo, support: &Support) -> Variants {
    match support {
        Support::Effort { values } => efforts(&values.clone().unwrap_or_else(|| strs(EFFORTS)), |e| {
            settings(json!({"thinkingConfig": {"includeThoughts": true, "thinkingLevel": e}}))
        }),
        Support::Toggle => toggle(
            settings(json!({"thinkingConfig": {"includeThoughts": false, "thinkingBudget": 0}})),
            settings(json!({"thinkingConfig": {"includeThoughts": true, "thinkingBudget": -1}})),
        ),
        Support::BudgetTokens { min, max } => budgets(
            model,
            *min,
            *max,
            |t| settings(json!({"thinkingConfig": {"includeThoughts": true, "thinkingBudget": num(t)}})),
            None,
        ),
    }
}

fn openrouter(model: &ModelInfo, support: &Support) -> Variants {
    match support {
        Support::Effort { values } => {
            efforts(&values.clone().unwrap_or_else(|| strs(EFFORTS)), |e| settings(json!({"reasoning": {"effort": e}})))
        }
        Support::Toggle => toggle(settings(json!({"reasoning": {"enabled": false}})), settings(json!({"reasoning": {"enabled": true}}))),
        Support::BudgetTokens { min, max } => {
            budgets(model, *min, *max, |t| settings(json!({"reasoning": {"max_tokens": num(t)}})), None)
        }
    }
}

fn fields(value: Value) -> Overlay {
    body(json!({"additionalModelRequestFields": value}))
}

fn bedrock_converse(model: &ModelInfo, support: &Support) -> Variants {
    let id = model_id(model).to_string();
    let claude = id.contains("anthropic");
    match support {
        Support::Effort { values } => efforts(&values.clone().unwrap_or_else(|| strs(EFFORTS)), |e| {
            if claude {
                let mut f = Object::new();
                if !claude_info(model).manual {
                    f.insert("thinking".into(), adaptive());
                }
                f.insert("output_config".into(), json!({"effort": e}));
                return fields(Value::Object(f));
            }
            if id.contains("openai.gpt-oss") {
                return fields(json!({"reasoning_effort": e}));
            }
            if id.contains("openai.") || id.contains("xai.") {
                return fields(json!({"reasoning": {"effort": e}}));
            }
            fields(json!({"reasoningConfig": {"type": "enabled", "maxReasoningEffort": e}}))
        }),
        Support::Toggle => {
            if claude {
                toggle(fields(json!({"thinking": {"type": "disabled"}})), fields(json!({"thinking": adaptive()})))
            } else {
                toggle(fields(json!({"reasoningConfig": {"type": "disabled"}})), fields(json!({"reasoningConfig": {"type": "enabled"}})))
            }
        }
        Support::BudgetTokens { min, max } => budgets(
            model,
            *min,
            *max,
            |t| {
                if claude {
                    settings(json!({"thinking": {"type": "enabled", "budgetTokens": num(t)}}))
                } else {
                    fields(json!({"reasoningConfig": {"type": "enabled", "budgetTokens": num(t)}}))
                }
            },
            Some(if claude { ANTHROPIC_OUTPUT_TOKEN_MAX } else { model.limit.output as f64 }),
        ),
    }
}

fn cohere(model: &ModelInfo, support: &Support) -> Variants {
    match support {
        Support::Effort { .. } => vec![],
        Support::Toggle => toggle(thinking_type("disabled"), thinking_type("enabled")),
        Support::BudgetTokens { min, max } => {
            budgets(model, *min, *max, |t| settings(json!({"thinking": {"type": "enabled", "tokenBudget": num(t)}})), None)
        }
    }
}

fn vercel_gateway(model: &ModelInfo, support: &Support) -> Variants {
    let id = model_id(model);
    if id.starts_with("openai/gpt-") {
        return openai_responses(model, support);
    }
    if id.starts_with("spacexai/grok-") {
        return open_responses(model, support);
    }
    vercel_messages(model, support)
}

fn vercel_responses(model: &ModelInfo, support: &Support) -> Variants {
    if model_id(model).starts_with("openai/gpt-") { openai_responses(model, support) } else { open_responses(model, support) }
}

fn vercel_messages(model: &ModelInfo, support: &Support) -> Variants {
    if model_id(model).starts_with("anthropic/") { anthropic_messages(model, support) } else { vercel_chat(model, support) }
}

fn vercel_chat(model: &ModelInfo, support: &Support) -> Variants {
    let id = model_id(model);
    match support {
        Support::Effort { .. } => openai_chat(model, support),
        Support::Toggle => toggle(thinking_type("disabled"), thinking_type("adaptive")),
        Support::BudgetTokens { min, max } => budgets(
            model,
            *min,
            *max,
            |t| settings(json!({"thinking": {"type": "enabled", "budgetTokens": num(t)}})),
            Some(if id.starts_with("alibaba/") { ALIBABA_THINKING_BUDGET_MAX } else { model.limit.output as f64 }),
        ),
    }
}

fn sap(params: Value) -> Overlay {
    settings(json!({"modelParams": params}))
}

fn sap_ai_core(model: &ModelInfo, support: &Support) -> Variants {
    let id = model_id(model).to_string();
    match support {
        Support::Effort { values } => efforts(&values.clone().unwrap_or_else(|| strs(EFFORTS)), |e| {
            if id.contains("anthropic") {
                let mut f = Object::new();
                if !claude_info(model).manual {
                    f.insert("thinking".into(), adaptive());
                }
                f.insert("output_config".into(), json!({"effort": e}));
                return sap(json!({"additionalModelRequestFields": f}));
            }
            if id.contains("gemini") {
                return sap(json!({"thinkingConfig": {"includeThoughts": true, "thinkingLevel": e}}));
            }
            if id.contains("amazon--nova") {
                return sap(json!({"additionalModelRequestFields": {"output_config": {"effort": e}}}));
            }
            sap(json!({"reasoning_effort": e}))
        }),
        Support::Toggle => {
            if id.contains("gemini") {
                return toggle(
                    sap(json!({"thinkingConfig": {"includeThoughts": false, "thinkingBudget": 0}})),
                    sap(json!({"thinkingConfig": {"includeThoughts": true, "thinkingBudget": -1}})),
                );
            }
            if id.contains("cohere") {
                return toggle(sap(json!({"thinking": {"type": "disabled"}})), sap(json!({"thinking": {"type": "enabled"}})));
            }
            if id.contains("amazon--nova") {
                return toggle(
                    sap(json!({"additionalModelRequestFields": {"thinking": {"type": "disabled"}}})),
                    sap(json!({"additionalModelRequestFields": {"thinking": {"type": "enabled"}}})),
                );
            }
            if id.contains("anthropic") {
                return toggle(
                    sap(json!({"additionalModelRequestFields": {"thinking": {"type": "disabled"}}})),
                    sap(json!({"additionalModelRequestFields": {"thinking": adaptive()}})),
                );
            }
            vec![]
        }
        Support::BudgetTokens { min, max } => {
            if id.contains("anthropic") {
                return budgets(
                    model,
                    *min,
                    *max,
                    |t| sap(json!({"additionalModelRequestFields": {"thinking": {"type": "enabled", "budget_tokens": num(t)}}})),
                    Some(ANTHROPIC_OUTPUT_TOKEN_MAX),
                );
            }
            if id.contains("gemini") {
                return budgets(
                    model,
                    *min,
                    *max,
                    |t| sap(json!({"thinkingConfig": {"includeThoughts": true, "thinkingBudget": num(t)}})),
                    None,
                );
            }
            if id.contains("cohere") {
                return budgets(model, *min, *max, |t| sap(json!({"thinking": {"type": "enabled", "token_budget": num(t)}})), None);
            }
            vec![]
        }
    }
}

fn gitlab_anthropic(model: &ModelInfo, support: &Support) -> Variants {
    let info = claude_info(model);
    let adaptive_effort = |e: &str| settings(json!({"thinking": {"type": "adaptive", "effort": e}}));
    match support {
        Support::Effort { values } => {
            if info.manual {
                return gitlab_anthropic(model, &Support::BudgetTokens { min: Some(1024.0), max: None });
            }
            let defaults = if info.major == Some(4.0) && info.minor == Some(6.0) {
                strs(&["low", "medium", "high", "max"])
            } else {
                strs(&["low", "medium", "high", "xhigh", "max"])
            };
            efforts(&values.clone().unwrap_or(defaults), |e| if e == "none" { thinking_type("disabled") } else { adaptive_effort(e) })
        }
        Support::Toggle => {
            if info.always {
                return vec![];
            }
            let thinking = if info.manual { manual_thinking(model) } else { Some(adaptive_effort("high")) };
            let Some(thinking) = thinking else { return vec![] };
            toggle(thinking_type("disabled"), thinking)
        }
        Support::BudgetTokens { min, max } => budgets(
            model,
            *min,
            *max,
            |t| settings(json!({"thinking": {"type": "enabled", "budgetTokens": num(t)}})),
            Some(ANTHROPIC_OUTPUT_TOKEN_MAX),
        ),
    }
}

fn gitlab(model: &ModelInfo, support: &Support) -> Variants {
    let id = model_id(model).to_lowercase();
    if id.starts_with("duo-workflow") {
        return vec![];
    }
    if claude_info(model).family.is_some() {
        return gitlab_anthropic(model, support);
    }
    if id.contains("gpt") || id.contains("codex") {
        return openai_chat(model, support);
    }
    vec![]
}

/// `PROTOCOLS[package]`.
fn protocol_for(package: &str) -> Option<Protocol> {
    Some(match package {
        "@opencode/ai/providers/openai"
        | "@opencode/ai/providers/openai/responses"
        | "@opencode/ai/providers/azure"
        | "@opencode/ai/providers/azure/responses"
        | "@opencode/ai/providers/amazon-bedrock/mantle"
        | "@opencode/ai/providers/amazon-bedrock/mantle/responses"
        | "@opencode/ai/providers/alibaba/responses"
        | "@opencode/ai/providers/meta"
        | "@opencode/ai/providers/meta/responses"
        | "@opencode/ai/providers/minimax/responses"
        | "@opencode/ai/providers/moonshot/responses"
        | "@opencode/ai/providers/zai-coding-plan/responses" => openai_responses,

        "@opencode/ai/providers/openai-compatible/responses"
        | "@opencode/ai/providers/openai-compatible-responses"
        | "@opencode/ai/providers/google-vertex/responses"
        | "@opencode/ai/providers/xai" => open_responses,

        "@opencode/ai/providers/openai-compatible" => openai_compatible,
        "@opencode/ai/providers/openai/chat" => openai_chat,
        "@opencode/ai/providers/azure/chat" => openai_chat,
        "@opencode/ai/providers/amazon-bedrock/mantle/chat" => openai_chat,
        "@opencode/ai/providers/google-vertex/chat" => vertex_chat,
        "@opencode/ai/providers/alibaba" | "@opencode/ai/providers/alibaba/chat" => alibaba_chat,
        "@opencode/ai/providers/baseten" => baseten_chat,
        "@opencode/ai/providers/cerebras" => openai_chat,
        "@opencode/ai/providers/cloudflare-workers-ai" => workers_ai_chat,
        "@opencode/ai/providers/cohere/chat" => openai_chat,
        "@opencode/ai/providers/deepinfra" => deepinfra_chat,
        "@opencode/ai/providers/deepseek" => deepseek_chat,
        "@opencode/ai/providers/digitalocean" => openai_chat,
        "@opencode/ai/providers/fireworks" => openai_chat,
        "@opencode/ai/providers/groq" => openai_chat,
        "@opencode/ai/providers/meta/chat" => openai_chat,
        "@opencode/ai/providers/minimax/chat" => openai_chat,
        "@opencode/ai/providers/mistral" => openai_chat,
        "@opencode/ai/providers/google-vertex/mistral" => openai_chat,
        "@opencode/ai/providers/moonshot" | "@opencode/ai/providers/moonshot/chat" => moonshot_chat,
        "@opencode/ai/providers/togetherai" => openai_chat,
        "@opencode/ai/providers/venice" => venice_chat,
        "@opencode/ai/providers/zai"
        | "@opencode/ai/providers/zai/chat"
        | "@opencode/ai/providers/zai-coding-plan"
        | "@opencode/ai/providers/zai-coding-plan/chat" => zai_chat,

        "@opencode/ai/providers/anthropic" => anthropic_messages,
        "@opencode/ai/providers/anthropic-compatible" => anthropic_messages,
        "@opencode/ai/providers/amazon-bedrock/mantle/messages" => anthropic_messages,
        "@opencode/ai/providers/google-vertex/messages" | "@opencode/ai/providers/google-vertex-messages" => anthropic_messages,
        "@opencode/ai/providers/alibaba/messages" => alibaba_messages,
        "@opencode/ai/providers/meta/messages" => anthropic_messages,
        "@opencode/ai/providers/minimax" | "@opencode/ai/providers/minimax/messages" => minimax_messages,
        "@opencode/ai/providers/moonshot/messages" => moonshot_messages,
        "@opencode/ai/providers/zai-coding-plan/messages" => zai_messages,

        "@opencode/ai/providers/google" => gemini,
        "@opencode/ai/providers/google-vertex" => gemini,
        "@opencode/ai/providers/google-vertex/gemini" => gemini,
        "@opencode/ai/providers/google/images" => gemini,
        "@opencode/ai/providers/google-vertex/images" => gemini,
        "@opencode/ai/providers/google/interactions" | "@opencode/ai/providers/google-vertex/interactions" => google_interactions,

        "@opencode/ai/providers/amazon-bedrock" => bedrock_converse,
        "@opencode/ai/providers/cohere" => cohere,
        "@opencode/ai/providers/openrouter" => openrouter,

        "aisdk:venice-ai-sdk-provider" => venice_chat,
        "@opencode/ai/providers/cloudflare-ai-gateway" => cloudflare_ai_gateway,
        "@opencode/ai/providers/vercel-ai-gateway" => vercel_gateway,
        "@opencode/ai/providers/vercel-ai-gateway/chat" => vercel_chat,
        "@opencode/ai/providers/vercel-ai-gateway/messages" => vercel_messages,
        "@opencode/ai/providers/vercel-ai-gateway/responses" => vercel_responses,
        "aisdk:@ai-sdk/gateway" => vercel_gateway,
        "aisdk:@jerome-benoit/sap-ai-provider-v2" => sap_ai_core,
        "aisdk:gitlab-ai-provider" => gitlab,
        _ => return None,
    })
}

/// Parse a models.dev `reasoning_options` entry (`effort` values with `null`/`"null"` removed).
pub fn support_from_json(value: &Value) -> Option<Support> {
    let o = value.as_object()?;
    match o.get("type").and_then(Value::as_str)? {
        "effort" => {
            let values = o.get("values").and_then(Value::as_array).map(|items| {
                items
                    .iter()
                    .filter_map(|v| v.as_str())
                    .filter(|v| *v != "null")
                    .map(String::from)
                    .collect::<Vec<_>>()
            });
            Some(Support::Effort { values })
        }
        "toggle" => Some(Support::Toggle),
        "budget_tokens" => Some(Support::BudgetTokens {
            min: o.get("min").and_then(Value::as_f64),
            max: o.get("max").and_then(Value::as_f64),
        }),
        _ => None,
    }
}

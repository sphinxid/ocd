//! OpenCode Console usage limits (`tui/src/util/usage-limits.ts`): which workspace limits apply to a
//! model, how close it is to them, and how limits and refusals read.

use crate::api::types::{ModelInfo, ProviderInfo, SessionError};
use crate::util::locale;
use serde::{Deserialize, Serialize};

/// `UsageLimits.Info`: the limits of the OpenCode Console workspace a location is signed in to.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Info {
    #[serde(default)]
    pub workspace: Workspace,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub go: Option<serde_json::Value>,
    #[serde(default)]
    pub budgets: Vec<Budget>,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Workspace {
    #[serde(default)]
    pub id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(rename = "usageURL", default)]
    pub usage_url: String,
}

/// `UsageLimits.Budget`: a monthly spend limit the workspace sets on the member.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Budget {
    #[serde(default)]
    pub id: String,
    #[serde(default)]
    pub limit: f64,
    #[serde(default)]
    pub spent: f64,
    /// `each` or `combined`.
    #[serde(default)]
    pub scope: String,
    /// `set` or `model`.
    #[serde(default)]
    pub per: String,
    /// `all` or `models`.
    #[serde(default)]
    pub coverage: String,
    #[serde(default)]
    pub models: Vec<BudgetModel>,
    #[serde(default)]
    pub vendors: Vec<String>,
    #[serde(rename = "inputPriceAtLeast", default, skip_serializing_if = "Option::is_none")]
    pub input_price_at_least: Option<f64>,
    #[serde(default)]
    pub picked: bool,
    #[serde(default)]
    pub shared: bool,
    #[serde(rename = "resetsAt", default)]
    pub resets_at: f64,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct BudgetModel {
    #[serde(rename = "providerID", default)]
    pub provider_id: String,
    #[serde(rename = "modelID", default)]
    pub model_id: String,
    #[serde(default)]
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub spent: Option<f64>,
}

/// One limit on a model: what has been spent against it and what is left.
#[derive(Clone, Debug, PartialEq)]
pub struct ModelLimit {
    pub budget: Budget,
    pub spent: f64,
    pub left: f64,
}

/// What a model has left: the limit closest to running out sets it.
#[derive(Clone, Debug, PartialEq)]
pub struct ModelUsage {
    pub left: f64,
    pub reached: bool,
    pub resets_at: f64,
    /// Every limit on the model, the one that sets `left` first.
    pub limits: Vec<ModelLimit>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Level {
    Plenty,
    Low,
    Out,
    Reached,
}

/// OpenCode Console serves the model, so the workspace's limits can apply; OpenCode Go bills its own allowance.
pub fn served_by_console(provider: Option<&ProviderInfo>) -> bool {
    provider.is_some_and(|p| p.integration_id.as_deref() == Some("opencode") && p.id != "opencode-go")
}

fn limit(budget: &Budget, spent: f64) -> ModelLimit {
    ModelLimit { budget: budget.clone(), spent, left: (budget.limit - spent).max(0.0) }
}

/// The limits on a paid model, or nothing when none applies; free models stay usable whatever is spent.
pub fn model_usage(info: &Info, model: &ModelInfo, provider: Option<&ProviderInfo>) -> Option<ModelUsage> {
    if model.free() {
        return None;
    }
    let mut limits: Vec<ModelLimit> = info
        .budgets
        .iter()
        .filter_map(|budget| {
            if budget.coverage == "all" {
                return served_by_console(provider).then(|| limit(budget, budget.spent));
            }
            let entry = budget.models.iter().find(|m| m.provider_id == model.provider_id && m.model_id == model.id)?;
            Some(limit(budget, if budget.per == "model" { entry.spent.unwrap_or(0.0) } else { budget.spent }))
        })
        .collect();
    // `toSorted` is stable.
    limits.sort_by(|a, b| a.left.partial_cmp(&b.left).unwrap_or(std::cmp::Ordering::Equal));
    let binding = limits.first()?;
    Some(ModelUsage {
        left: binding.left,
        reached: limits.iter().any(|l| l.spent >= l.budget.limit),
        resets_at: limits.iter().map(|l| l.budget.resets_at).fold(f64::INFINITY, f64::min),
        limits,
    })
}

/// `low`: what's left won't cover three requests that fill the model's context; `out`: not one.
pub fn usage_level(usage: &ModelUsage, model: &ModelInfo) -> Level {
    if usage.reached || usage.left <= 0.0 {
        return Level::Reached;
    }
    let request = (model.limit.context / 1_000_000.0) * model.cost.first().map(|c| c.input).unwrap_or(0.0);
    if usage.left < request {
        return Level::Out;
    }
    if usage.left < request * 3.0 {
        return Level::Low;
    }
    Level::Plenty
}

/// Dollars as limits read: whole amounts without cents, and a sliver as "<$0.01" rather than "$0.00".
pub fn format_dollars(value: f64) -> String {
    if value > 0.0 && value < 0.01 {
        return "<$0.01".into();
    }
    if value.fract() == 0.0 && value.is_finite() {
        let sign = if value < 0.0 { "-" } else { "" };
        return format!("{sign}${}", locale::grouped(value.abs() as i64));
    }
    if value < 0.0 { format!("-{}", locale::usd(-value)) } else { locale::usd(value) }
}

/// When limits reset: the day, with the time unless it is midnight here, or just the time when that is today.
pub fn format_reset(at: f64) -> String {
    format_reset_at(at, locale::now_ms())
}

pub fn format_reset_at(at: f64, now: i64) -> String {
    use chrono::{Local, TimeZone, Timelike};
    let Some(date) = Local.timestamp_millis_opt(at as i64).single() else { return "Invalid Date".into() };
    let time = date.format("%-I:%M %p").to_string();
    if locale::date_string(at as i64) == locale::date_string(now) {
        return format!("{time} today");
    }
    let day = date.format("%b %-d").to_string();
    if date.hour() == 0 && date.minute() == 0 { day } else { format!("{day}, {time}") }
}

fn body(error: Option<&SessionError>) -> &str {
    error.and_then(|e| e.response.as_ref()).map(|r| r.body.as_str()).unwrap_or("")
}

fn capture(re: &str, text: &str) -> Option<String> {
    regex::Regex::new(re).ok()?.captures(text).map(|c| c[1].to_string())
}

/// When a refused request's limit resets: OpenCode Console names it `resetsAt` in every protocol's error envelope.
pub fn refusal_reset(error: Option<&SessionError>) -> Option<f64> {
    js_date_parse(&capture(r#""resetsAt"\s*:\s*"([^"]+)""#, body(error))?)
}

/// When a refused request may retry, from its `retry-after` counted from `base`; nothing past a week.
pub fn retry_reset(error: Option<&SessionError>, base: i64) -> Option<f64> {
    let delay = error?.retry_after_ms?;
    if !delay.is_finite() || !(0.0..=7.0 * 86_400_000.0).contains(&delay) {
        return None;
    }
    Some(base as f64 + delay)
}

/// Which Go limit refused a request (5 hour, weekly or monthly).
pub fn go_limit_name(error: Option<&SessionError>) -> Option<String> {
    capture(r#""limitName"\s*:\s*"([^"]+)""#, body(error))
}

/// The OpenCode Console Go page for the workspace a Go usage limit names.
pub fn go_limit_link(error: Option<&SessionError>) -> String {
    match capture(r#""workspace"\s*:\s*"(wrk_\w+)""#, body(error)) {
        Some(w) => format!("https://opencode.ai/console/{w}/go"),
        None => "https://opencode.ai/console".into(),
    }
}

/// Who spends from a limit and, when it pools spend across models, which ones.
pub fn limit_scope(budget: &Budget, workspace: Option<&str>) -> String {
    let who = if budget.scope == "each" {
        "Only you".to_string()
    } else if budget.shared {
        format!("Everyone in {}", workspace.unwrap_or("the workspace"))
    } else {
        "You and others".to_string()
    };
    if budget.per == "model" {
        return who;
    }
    format!("{who} · {}", limit_models(budget))
}

fn limit_models(budget: &Budget) -> String {
    if budget.coverage == "all" {
        return "All models".into();
    }
    let vendors = (!budget.vendors.is_empty()).then(|| list(&budget.vendors));
    let price = budget.input_price_at_least.map(|p| format!("{} per 1M", format_dollars(p)));
    match (vendors, price) {
        (Some(v), Some(p)) => format!("{v} models over {p}"),
        (Some(v), None) => format!("All {v} models"),
        (None, Some(p)) => format!("Models over {p}"),
        (None, None) => {
            let names: Vec<&str> = budget.models.iter().map(|m| m.name.as_str()).collect();
            if names.len() > 3 { format!("{} +{} more", names[..3].join(", "), names.len() - 3) } else { names.join(", ") }
        }
    }
}

/// `Intl.ListFormat("en-US", { style: "long", type: "conjunction" })`.
fn list(items: &[String]) -> String {
    match items {
        [] => String::new(),
        [a] => a.clone(),
        [a, b] => format!("{a} and {b}"),
        [rest @ .., last] => format!("{}, and {last}", rest.join(", ")),
    }
}

/// `Date.parse` for the ISO forms an error envelope carries: date-only strings are UTC, date-times
/// without an offset are local time.
pub fn js_date_parse(s: &str) -> Option<f64> {
    use chrono::{DateTime, Local, NaiveDate, NaiveDateTime, TimeZone};
    let s = s.trim();
    if let Ok(d) = DateTime::parse_from_rfc3339(s) {
        return Some(d.timestamp_millis() as f64);
    }
    for fmt in ["%Y-%m-%dT%H:%M:%S%.f", "%Y-%m-%dT%H:%M:%S", "%Y-%m-%dT%H:%M"] {
        if let Ok(n) = NaiveDateTime::parse_from_str(s, fmt) {
            return Local.from_local_datetime(&n).earliest().map(|d| d.timestamp_millis() as f64);
        }
    }
    if let Ok(d) = NaiveDate::parse_from_str(s, "%Y-%m-%d") {
        return Some(d.and_hms_opt(0, 0, 0)?.and_utc().timestamp_millis() as f64);
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::types::{ErrorResponse, ModelCost, ModelLimit as Window};
    use chrono::{Local, TimeZone};

    const RESET: f64 = 1_793_491_200_000.0; // Date.UTC(2026, 10, 1)

    fn budget(id: &str) -> Budget {
        Budget {
            id: id.into(),
            limit: 100.0,
            spent: 0.0,
            scope: "each".into(),
            per: "set".into(),
            coverage: "all".into(),
            resets_at: RESET,
            ..Default::default()
        }
    }

    fn info(budgets: Vec<Budget>) -> Info {
        Info {
            workspace: Workspace { id: "wrk_1".into(), name: Some("Acme".into()), usage_url: "https://opencode.ai/wrk_1/activity/overview".into() },
            go: None,
            budgets,
        }
    }

    fn sol() -> ModelInfo {
        ModelInfo {
            id: "gpt-5.6-sol".into(),
            provider_id: "opencode".into(),
            cost: vec![ModelCost { input: 4.0, output: 0.0 }],
            limit: Window { context: 1_000_000.0, ..Default::default() },
            ..Default::default()
        }
    }

    fn served() -> ProviderInfo {
        ProviderInfo { id: "opencode".into(), integration_id: Some("opencode".into()), ..Default::default() }
    }

    fn bm(provider: &str, model: &str, name: &str, spent: Option<f64>) -> BudgetModel {
        BudgetModel { provider_id: provider.into(), model_id: model.into(), name: name.into(), spent }
    }

    #[test]
    fn the_limit_closest_to_running_out_sets_what_a_model_has_left() {
        let limits = info(vec![
            Budget { limit: 500.0, spent: 100.0, ..budget("all") },
            Budget {
                coverage: "models".into(),
                per: "model".into(),
                limit: 50.0,
                spent: 50.0,
                models: vec![bm("opencode", "gpt-5.6-sol", "GPT 5.6 Sol", Some(20.0)), bm("opencode", "claude-opus-5", "Claude Opus 5", Some(50.0))],
                ..budget("each")
            },
        ]);
        let u = model_usage(&limits, &sol(), Some(&served())).unwrap();
        assert_eq!((u.left, u.reached, u.resets_at), (30.0, false, RESET));
        assert_eq!(u.limits.iter().map(|l| (l.budget.id.as_str(), l.spent)).collect::<Vec<_>>(), vec![("each", 20.0), ("all", 100.0)]);
        let opus = ModelInfo { id: "claude-opus-5".into(), ..sol() };
        let u = model_usage(&limits, &opus, Some(&served())).unwrap();
        assert_eq!((u.left, u.reached), (0.0, true));
        let go = ProviderInfo { id: "opencode-go".into(), integration_id: Some("opencode".into()), ..Default::default() };
        assert!(model_usage(&limits, &ModelInfo { provider_id: "opencode-go".into(), ..sol() }, Some(&go)).is_none());
        let openai = ProviderInfo { id: "openai".into(), ..Default::default() };
        assert!(model_usage(&limits, &ModelInfo { provider_id: "openai".into(), ..sol() }, Some(&openai)).is_none());
        let free = ModelInfo { cost: vec![ModelCost { input: 0.0, output: 0.0 }], ..sol() };
        assert!(model_usage(&limits, &free, Some(&served())).is_none());
    }

    #[test]
    fn warns_by_how_many_full_requests_are_left_on_the_model() {
        let level = |spent: f64| usage_level(&model_usage(&info(vec![Budget { spent, ..budget("all") }]), &sol(), Some(&served())).unwrap(), &sol());
        assert_eq!(level(80.0), Level::Plenty);
        assert_eq!(level(90.0), Level::Low);
        assert_eq!(level(97.0), Level::Out);
        assert_eq!(level(100.0), Level::Reached);
    }

    #[test]
    fn writes_dollars_and_resets_the_way_limits_read() {
        let v: Vec<String> = [6.0, 1.2, 0.004, 0.0, 1240.0].into_iter().map(format_dollars).collect();
        assert_eq!(v, ["$6", "$1.20", "<$0.01", "$0", "$1,240"]);
        let local = |y, mo, d, h| Local.with_ymd_and_hms(y, mo, d, h, 0, 0).unwrap().timestamp_millis();
        let now = local(2026, 10, 24, 9);
        assert_eq!(format_reset_at(local(2026, 10, 24, 17) as f64, now), "5:00 PM today");
        assert_eq!(format_reset_at(local(2026, 11, 1, 0) as f64, now), "Nov 1");
        assert_eq!(format_reset_at(local(2026, 10, 31, 19) as f64, now), "Oct 31, 7:00 PM");
    }

    #[test]
    fn says_who_spends_from_a_limit_and_which_models_it_pools() {
        assert_eq!(limit_scope(&Budget { coverage: "models".into(), vendors: vec!["OpenAI".into()], ..budget("a") }, Some("Acme")), "Only you · All OpenAI models");
        assert_eq!(limit_scope(&Budget { scope: "combined".into(), shared: true, ..budget("b") }, Some("Acme")), "Everyone in Acme · All models");
        assert_eq!(
            limit_scope(&Budget { scope: "combined".into(), coverage: "models".into(), input_price_at_least: Some(3.0), ..budget("c") }, Some("Acme")),
            "You and others · Models over $3 per 1M"
        );
        let picked = Budget {
            coverage: "models".into(),
            picked: true,
            models: ["A", "B", "C", "D"].iter().map(|n| bm("opencode", n, n, None)).collect(),
            ..budget("d")
        };
        assert_eq!(limit_scope(&picked, Some("Acme")), "Only you · A, B, C +1 more");
        assert_eq!(limit_scope(&Budget { coverage: "models".into(), per: "model".into(), vendors: vec!["OpenAI".into()], ..budget("e") }, Some("Acme")), "Only you");
        assert_eq!(limit_scope(&Budget { coverage: "models".into(), vendors: vec!["A".into(), "B".into(), "C".into()], input_price_at_least: Some(1.5), ..budget("f") }, None), "Only you · A, B, and C models over $1.50 per 1M");
    }

    #[test]
    fn reads_when_a_refused_requests_limit_resets_from_either_error_envelope() {
        let err = |body: String| SessionError { response: Some(ErrorResponse { body }), ..Default::default() };
        let at = "2026-11-01T00:00:00.000Z";
        let a = err(serde_json::json!({ "type": "error", "error": {}, "metadata": { "coverage": "all", "resetsAt": at } }).to_string());
        assert_eq!(refusal_reset(Some(&a)), Some(RESET));
        let b = err(serde_json::json!({ "error": { "code": 429, "details": [{ "reason": "BudgetLimitError", "metadata": { "coverage": "models", "resetsAt": at } }] } }).to_string());
        assert_eq!(refusal_reset(Some(&b)), Some(RESET));
        assert_eq!(refusal_reset(Some(&err("{}".into()))), None);
    }

    #[test]
    fn go_limit_metadata() {
        let e = SessionError {
            response: Some(ErrorResponse { body: r#"{"metadata":{"limitName":"5 hour","workspace":"wrk_01ABC"}}"#.into() }),
            ..Default::default()
        };
        assert_eq!(go_limit_name(Some(&e)).as_deref(), Some("5 hour"));
        assert_eq!(go_limit_link(Some(&e)), "https://opencode.ai/console/wrk_01ABC/go");
        assert_eq!(go_limit_link(None), "https://opencode.ai/console");
        let r = SessionError { retry_after_ms: Some(1000.0), ..Default::default() };
        assert_eq!(retry_reset(Some(&r), 5), Some(1005.0));
        let r = SessionError { retry_after_ms: Some(8.0 * 86_400_000.0), ..Default::default() };
        assert_eq!(retry_reset(Some(&r), 5), None);
    }
}

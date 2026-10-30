//! Usage and cost (`core/session/usage.ts`).

use oc_schema::types::{CacheUsage, TokenUsage};

#[derive(Debug, Clone, Default)]
pub struct RawUsage {
    pub non_cached_input: Option<f64>,
    pub visible_output: Option<f64>,
    pub reasoning: Option<f64>,
    pub cache_read: Option<f64>,
    pub cache_write: Option<f64>,
}

fn safe(x: Option<f64>) -> u64 {
    match x {
        Some(v) if v.is_finite() && v > 0.0 => v as u64,
        _ => 0,
    }
}

pub fn tokens(u: &RawUsage) -> TokenUsage {
    TokenUsage {
        input: safe(u.non_cached_input),
        output: safe(u.visible_output),
        reasoning: safe(u.reasoning),
        cache: CacheUsage { read: safe(u.cache_read), write: safe(u.cache_write) },
    }
}

/// One cost entry (USD per 1M tokens); `tier_size` set for context tiers.
#[derive(Debug, Clone, Default)]
pub struct Cost {
    pub tier_size: Option<u64>,
    pub input: f64,
    pub output: f64,
    pub cache_read: f64,
    pub cache_write: f64,
}

pub fn calculate_cost(costs: &[Cost], t: &TokenUsage) -> f64 {
    let ctx = t.input + t.cache.read + t.cache.write;
    let tier = costs
        .iter()
        .filter(|c| c.tier_size.is_some_and(|s| ctx > s))
        .max_by_key(|c| c.tier_size.unwrap())
        .or_else(|| costs.iter().find(|c| c.tier_size.is_none()));
    let Some(c) = tier else { return 0.0 };
    (t.input as f64 * c.input
        + (t.output + t.reasoning) as f64 * c.output
        + t.cache.read as f64 * c.cache_read
        + t.cache.write as f64 * c.cache_write)
        / 1_000_000.0
}

/// `SessionUsage.record`: the session tokens and cost of one response. What an endpoint says it charged
/// (`Usage.cost`) beats the catalog, which can't see its peak pricing, subscriptions or routing; a reported zero is
/// authoritative.
pub fn record(usage: Option<&oc_ai::Usage>, costs: &[Cost]) -> (f64, TokenUsage) {
    let tokens = crate::publisher::tokens(usage);
    let cost = usage.and_then(|u| u.cost).unwrap_or_else(|| calculate_cost(costs, &tokens));
    (cost, tokens)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn tiers() {
        let costs = vec![
            Cost { input: 3.0, output: 15.0, ..Default::default() },
            Cost { tier_size: Some(200_000), input: 6.0, output: 22.5, ..Default::default() },
        ];
        let small = TokenUsage { input: 1000, output: 100, reasoning: 0, cache: CacheUsage::default() };
        assert!((calculate_cost(&costs, &small) - 0.0045).abs() < 1e-12);
        let big = TokenUsage { input: 300_000, output: 0, reasoning: 0, cache: CacheUsage::default() };
        assert!((calculate_cost(&costs, &big) - 1.8).abs() < 1e-12);
    }

    // session-usage.test.ts: the reported cost beats the catalog price, a reported zero included.
    #[test]
    fn record_prefers_the_reported_cost() {
        let costs = vec![Cost { input: 3.0, output: 15.0, ..Default::default() }];
        let usage = |cost: Option<f64>| oc_ai::Usage {
            input_tokens: Some(1000),
            non_cached_input_tokens: Some(1000),
            output_tokens: Some(100),
            cost,
            ..Default::default()
        };
        let (cost, tokens) = record(Some(&usage(None)), &costs);
        assert!((cost - 0.0045).abs() < 1e-12);
        assert_eq!((tokens.input, tokens.output), (1000, 100));
        assert_eq!(record(Some(&usage(Some(0.25))), &costs).0, 0.25);
        assert_eq!(record(Some(&usage(Some(0.0))), &costs).0, 0.0);
        assert_eq!(record(None, &costs).0, 0.0);
    }
}

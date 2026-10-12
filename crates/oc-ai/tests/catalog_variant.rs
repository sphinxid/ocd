//! Port of `packages/core/test/variant.test.ts`.

use oc_ai::catalog::types::ModelInfo;
use oc_ai::catalog::variant::{resolve, Support};
use serde_json::{json, Value};

fn model(package: &str, model_id: &str, output: Option<i64>, provider_id: &str) -> ModelInfo {
    let mut m = ModelInfo::default_for(provider_id, model_id);
    m.package = Some(package.into());
    m.model_id = model_id.into();
    if let Some(output) = output {
        m.limit.output = output;
    }
    m
}

fn m(package: &str, id: &str) -> ModelInfo {
    model(package, id, None, "test")
}

fn run(input: &ModelInfo, supports: &[Support]) -> Value {
    serde_json::to_value(resolve(input, supports)).unwrap()
}

fn ids(input: &ModelInfo, supports: &[Support]) -> Vec<String> {
    resolve(input, supports).into_iter().map(|v| v.id).collect()
}

fn budget(min: Option<f64>, max: Option<f64>) -> Support {
    Support::BudgetTokens { min, max }
}

fn adaptive_effort(effort: &str) -> Value {
    json!({"id": effort, "settings": {"effort": effort, "thinking": {"type": "adaptive", "display": "summarized"}}})
}

fn responses(effort: &str) -> Value {
    json!({"id": effort, "settings": {"reasoningEffort": effort, "reasoningSummary": "auto", "include": ["reasoning.encrypted_content"]}})
}

#[test]
fn spells_messages_variants_for_each_provider() {
    assert_eq!(
        run(&m("@opencode/ai/providers/anthropic", "claude-opus-4-5"), &[Support::efforts(&["low", "high"]), budget(Some(1024.0), None)]),
        json!([
            {"id": "low", "settings": {"thinking": {"type": "enabled", "budgetTokens": 16000}, "effort": "low"}},
            {"id": "high", "settings": {"thinking": {"type": "enabled", "budgetTokens": 16000}, "effort": "high"}},
        ])
    );
    assert_eq!(
        run(&m("@opencode/ai/providers/anthropic", "claude-opus-4-8"), &[Support::efforts(&["low", "max"])]),
        json!([adaptive_effort("low"), adaptive_effort("max")])
    );
    assert_eq!(
        run(&m("@opencode/ai/providers/minimax/messages", "MiniMax-M3"), &[Support::Toggle]),
        json!([
            {"id": "none", "settings": {"thinking": {"type": "disabled"}}},
            {"id": "thinking", "settings": {"thinking": {"type": "adaptive"}}},
        ])
    );
    assert_eq!(
        run(&m("@opencode/ai/providers/moonshot/messages", "k3"), &[Support::Toggle, Support::efforts(&["low", "high", "max"])]),
        json!([
            {"id": "low", "settings": {"effort": "low"}},
            {"id": "high", "settings": {"effort": "high"}},
            {"id": "max", "settings": {"effort": "max"}},
        ])
    );
    assert_eq!(
        run(
            &m("@opencode/ai/providers/alibaba/messages", "qwen3.8-max"),
            &[Support::Toggle, Support::efforts(&["low", "medium", "xhigh"]), budget(Some(1024.0), Some(262_144.0))]
        ),
        json!([
            {"id": "none", "settings": {"thinking": {"type": "disabled"}}},
            {"id": "low", "settings": {"thinking": {"type": "enabled"}, "effort": "low"}},
            {"id": "medium", "settings": {"thinking": {"type": "enabled"}, "effort": "medium"}},
            {"id": "xhigh", "settings": {"thinking": {"type": "enabled"}, "effort": "xhigh"}},
        ])
    );
    assert_eq!(
        run(
            &m("@opencode/ai/providers/zai-coding-plan/messages", "glm-5.3"),
            &[Support::Toggle, Support::efforts(&["low", "high", "max"]), budget(Some(1024.0), None)]
        ),
        json!([
            {"id": "low", "settings": {"thinking": {"type": "enabled"}, "effort": "low"}},
            {"id": "high", "settings": {"thinking": {"type": "enabled"}, "effort": "high"}},
            {"id": "max", "settings": {"thinking": {"type": "enabled"}, "effort": "max"}},
        ])
    );
}

#[test]
fn recognizes_claude_version_spellings_and_future_models() {
    for id in ["claude-sonnet-3.7", "claude-sonnet-3-7", "claude-3.7-sonnet", "claude-3-7-sonnet"] {
        assert_eq!(
            run(&m("@opencode/ai/providers/anthropic", id), &[Support::effort()]),
            json!([
                {"id": "high", "settings": {"thinking": {"type": "enabled", "budgetTokens": 16000}}},
                {"id": "max", "settings": {"thinking": {"type": "enabled", "budgetTokens": 31999}}},
            ]),
            "{id}"
        );
    }
    assert_eq!(
        run(&m("@opencode/ai/providers/anthropic", "claude-opus-6"), &[Support::effort()]),
        Value::Array(["low", "medium", "high", "xhigh", "max"].iter().map(|e| adaptive_effort(e)).collect())
    );
    assert_eq!(
        run(&model("@opencode/ai/providers/anthropic", "claude-haiku-4-5", Some(64_000), "test"), &[budget(Some(1024.0), Some(64_000.0))]),
        json!([
            {"id": "high", "settings": {"thinking": {"type": "enabled", "budgetTokens": 16000}}},
            {"id": "max", "settings": {"thinking": {"type": "enabled", "budgetTokens": 31999}}},
        ])
    );
}

#[test]
fn spells_cloudflare_ai_gateway_variants() {
    let pkg = "@opencode/ai/providers/cloudflare-ai-gateway";
    assert_eq!(run(&m(pkg, "openai/gpt-5.4"), &[Support::efforts(&["low", "xhigh"])]), json!([responses("low"), responses("xhigh")]));
    assert_eq!(
        run(&m(pkg, "anthropic/claude-sonnet-4-6"), &[Support::efforts(&["low", "high"])]),
        json!([adaptive_effort("low"), adaptive_effort("high")])
    );
    assert_eq!(
        run(&m(pkg, "xai/grok-4.6"), &[Support::efforts(&["low", "high"])]),
        json!([{"id": "low", "settings": {"reasoningEffort": "low"}}, {"id": "high", "settings": {"reasoningEffort": "high"}}])
    );
}

#[test]
fn spells_vercel_ai_gateway_variants() {
    let pkg = "@opencode/ai/providers/vercel-ai-gateway";
    assert_eq!(run(&m(pkg, "openai/gpt-5.4"), &[Support::efforts(&["low", "xhigh"])]), json!([responses("low"), responses("xhigh")]));
    assert_eq!(ids(&m(pkg, "spacexai/grok-4.7"), &[Support::effort()]), ["low", "medium", "high"]);
    assert_eq!(
        run(&m(pkg, "anthropic/claude-sonnet-4.6"), &[Support::efforts(&["none", "low", "high"])]),
        json!([{"id": "none", "settings": {"thinking": {"type": "disabled"}}}, adaptive_effort("low"), adaptive_effort("high")])
    );
    assert_eq!(
        run(&m(pkg, "meta/muse-spark-1.3"), &[Support::efforts(&["low", "high"])]),
        json!([{"id": "low", "settings": {"reasoningEffort": "low"}}, {"id": "high", "settings": {"reasoningEffort": "high"}}])
    );
    assert_eq!(
        run(&model(pkg, "alibaba/qwen-3-32b", Some(38_912), "test"), &[Support::Toggle, budget(Some(1.0), Some(38_912.0))]),
        json!([
            {"id": "none", "settings": {"thinking": {"type": "disabled"}}},
            {"id": "high", "settings": {"thinking": {"type": "enabled", "budgetTokens": 19456}}},
            {"id": "max", "settings": {"thinking": {"type": "enabled", "budgetTokens": 38911}}},
        ])
    );
}

#[test]
fn spells_xai_responses_variants() {
    assert_eq!(
        run(&m("@opencode/ai/providers/xai", "grok-4.6"), &[Support::efforts(&["low", "medium", "high", "xhigh"])]),
        Value::Array(["low", "medium", "high", "xhigh"].iter().map(|e| responses(e)).collect())
    );
    assert_eq!(
        run(&m("@opencode/ai/providers/xai", "grok-4.3"), &[Support::efforts(&["none", "low"])]),
        json!([responses("none"), responses("low")])
    );
    assert_eq!(ids(&m("@opencode/ai/providers/xai", "grok-4.6"), &[Support::effort()]), ["low", "medium", "high"]);
}

#[test]
fn spells_chat_completions_variants_for_direct_providers() {
    assert_eq!(
        run(&m("@opencode/ai/providers/deepseek", "deepseek-v4-flash"), &[Support::Toggle, Support::efforts(&["low", "high", "max"])]),
        json!([
            {"id": "none", "body": {"thinking": {"type": "disabled"}}},
            {"id": "low", "settings": {"reasoningEffort": "low"}, "body": {"thinking": {"type": "enabled"}}},
            {"id": "high", "settings": {"reasoningEffort": "high"}, "body": {"thinking": {"type": "enabled"}}},
            {"id": "max", "settings": {"reasoningEffort": "max"}, "body": {"thinking": {"type": "enabled"}}},
        ])
    );
    assert_eq!(
        run(&m("@opencode/ai/providers/moonshot/chat", "kimi-k3"), &[Support::Toggle, Support::efforts(&["low", "high", "max"])]),
        json!([
            {"id": "low", "settings": {"reasoningEffort": "low"}},
            {"id": "high", "settings": {"reasoningEffort": "high"}},
            {"id": "max", "settings": {"reasoningEffort": "max"}},
        ])
    );
    assert_eq!(
        run(&m("@opencode/ai/providers/moonshot/chat", "kimi-k2.6"), &[Support::Toggle]),
        json!([
            {"id": "none", "settings": {"thinking": {"type": "disabled"}}},
            {"id": "thinking", "settings": {"thinking": {"type": "enabled"}}},
        ])
    );
    assert_eq!(
        run(
            &m("@opencode/ai/providers/alibaba/chat", "qwen3.8-max"),
            &[Support::Toggle, Support::efforts(&["low", "medium", "xhigh"]), budget(Some(0.0), Some(262_144.0))]
        ),
        json!([
            {"id": "none", "settings": {"enableThinking": false}},
            {"id": "low", "settings": {"enableThinking": true, "reasoningEffort": "low"}},
            {"id": "medium", "settings": {"enableThinking": true, "reasoningEffort": "medium"}},
            {"id": "xhigh", "settings": {"enableThinking": true, "reasoningEffort": "xhigh"}},
        ])
    );
    assert_eq!(
        run(&model("@opencode/ai/providers/alibaba/chat", "qwen3.7-plus", Some(300_000), "test"), &[Support::Toggle, budget(Some(0.0), Some(262_144.0))]),
        json!([
            {"id": "none", "settings": {"enableThinking": false}},
            {"id": "high", "settings": {"enableThinking": true, "thinkingBudget": 32000}},
            {"id": "max", "settings": {"enableThinking": true, "thinkingBudget": 63999}},
        ])
    );
    let enabled = |e: &str| json!({"id": e, "settings": {"thinking": {"type": "enabled", "clear_thinking": false}, "reasoningEffort": e}});
    assert_eq!(
        run(&m("@opencode/ai/providers/zai/chat", "glm-5.3"), &[Support::Toggle, Support::efforts(&["low", "high", "max"])]),
        json!([enabled("low"), enabled("high"), enabled("max")])
    );
    assert_eq!(
        run(&m("@opencode/ai/providers/zai/chat", "glm-4.7"), &[Support::Toggle]),
        json!([
            {"id": "none", "settings": {"thinking": {"type": "disabled"}}},
            {"id": "thinking", "settings": {"thinking": {"type": "enabled", "clear_thinking": false}}},
        ])
    );
}

#[test]
fn spells_bedrock_converse_variants() {
    assert_eq!(
        run(
            &model("@opencode/ai/providers/amazon-bedrock", "us.anthropic.claude-haiku-4-5-20251001-v1:0", Some(64_000), "test"),
            &[budget(Some(1024.0), None)]
        ),
        json!([
            {"id": "high", "settings": {"thinking": {"type": "enabled", "budgetTokens": 16000}}},
            {"id": "max", "settings": {"thinking": {"type": "enabled", "budgetTokens": 31999}}},
        ])
    );
    let supports = [Support::efforts(&["low", "xhigh"])];
    assert_eq!(
        run(&m("@opencode/ai/providers/amazon-bedrock", "us.xai.grok-4.6"), &supports),
        json!([
            {"id": "low", "body": {"additionalModelRequestFields": {"reasoning": {"effort": "low"}}}},
            {"id": "xhigh", "body": {"additionalModelRequestFields": {"reasoning": {"effort": "xhigh"}}}},
        ])
    );
    assert_eq!(
        run(&m("@opencode/ai/providers/amazon-bedrock", "us.amazon.nova-2-lite-v1:0"), &supports),
        json!([
            {"id": "low", "body": {"additionalModelRequestFields": {"reasoningConfig": {"type": "enabled", "maxReasoningEffort": "low"}}}},
            {"id": "xhigh", "body": {"additionalModelRequestFields": {"reasoningConfig": {"type": "enabled", "maxReasoningEffort": "xhigh"}}}},
        ])
    );
}

#[test]
fn caps_alibaba_thinking_budget_variants() {
    let supports = [Support::Toggle, budget(None, None)];
    assert_eq!(
        run(&model("@opencode/ai/providers/alibaba/chat", "kimi-k2.6", Some(262_144), "test"), &supports),
        json!([
            {"id": "none", "settings": {"enableThinking": false}},
            {"id": "high", "settings": {"enableThinking": true, "thinkingBudget": 32000}},
            {"id": "max", "settings": {"enableThinking": true, "thinkingBudget": 63999}},
        ])
    );
    assert_eq!(
        run(&model("@opencode/ai/providers/alibaba/messages", "kimi-k2.6", Some(262_144), "test"), &supports),
        json!([
            {"id": "none", "settings": {"thinking": {"type": "disabled"}}},
            {"id": "high", "settings": {"thinking": {"type": "enabled", "budgetTokens": 32000}}},
            {"id": "max", "settings": {"thinking": {"type": "enabled", "budgetTokens": 63999}}},
        ])
    );
    assert_eq!(
        run(&model("@opencode/ai/providers/alibaba/chat", "kimi-k2.5", Some(32_768), "test"), &supports),
        json!([
            {"id": "none", "settings": {"enableThinking": false}},
            {"id": "high", "settings": {"enableThinking": true, "thinkingBudget": 16384}},
            {"id": "max", "settings": {"enableThinking": true, "thinkingBudget": 32767}},
        ])
    );
}

#[test]
fn spells_workers_ai_thinking_controls() {
    let pkg = "@opencode/ai/providers/cloudflare-workers-ai";
    let off = json!({"chat_template_kwargs": {"enable_thinking": false, "thinking": false}});
    let on = json!({"chat_template_kwargs": {"enable_thinking": true, "thinking": true}});
    assert_eq!(
        run(&m(pkg, "@cf/deepseek-ai/deepseek-v4-flash-0731"), &[Support::efforts(&["none", "low", "high", "max"])]),
        json!([
            {"id": "none", "body": off},
            {"id": "low", "settings": {"reasoningEffort": "low"}},
            {"id": "high", "settings": {"reasoningEffort": "high"}},
            {"id": "max", "settings": {"reasoningEffort": "max"}},
        ])
    );
    assert_eq!(run(&m(pkg, "@cf/zai-org/glm-4.7-flash"), &[Support::Toggle]), json!([{"id": "none", "body": off}, {"id": "thinking", "body": on}]));
    assert_eq!(
        run(&m(pkg, "@cf/qwen/qwen3.8-27b"), &[Support::Toggle, Support::efforts(&["low", "medium", "xhigh"])]),
        json!([
            {"id": "none", "body": off},
            {"id": "low", "settings": {"reasoningEffort": "low"}},
            {"id": "medium", "settings": {"reasoningEffort": "medium"}},
            {"id": "xhigh", "settings": {"reasoningEffort": "xhigh"}},
        ])
    );
}

#[test]
fn spells_cohere_variants() {
    assert_eq!(
        run(&model("@opencode/ai/providers/cohere", "command-a-reasoning-08-2025", Some(32_000), "test"), &[Support::Toggle, budget(Some(1.0), None)]),
        json!([
            {"id": "none", "settings": {"thinking": {"type": "disabled"}}},
            {"id": "high", "settings": {"thinking": {"type": "enabled", "tokenBudget": 16000}}},
            {"id": "max", "settings": {"thinking": {"type": "enabled", "tokenBudget": 31999}}},
        ])
    );
    assert_eq!(
        run(&m("@opencode/ai/providers/cohere/chat", "north-mini-code-1-0"), &[Support::efforts(&["none", "high"])]),
        json!([{"id": "none", "settings": {"reasoningEffort": "none"}}, {"id": "high", "settings": {"reasoningEffort": "high"}}])
    );
}

#[test]
fn spells_chat_completions_variants_for_hosting_providers() {
    assert_eq!(
        run(
            &model("@opencode/ai/providers/digitalocean", "openai-gpt-5-nano", None, "digitalocean"),
            &[Support::efforts(&["minimal", "low", "medium", "high"])]
        ),
        json!([
            {"id": "minimal", "settings": {"reasoningEffort": "minimal"}},
            {"id": "low", "settings": {"reasoningEffort": "low"}},
            {"id": "medium", "settings": {"reasoningEffort": "medium"}},
            {"id": "high", "settings": {"reasoningEffort": "high"}},
        ])
    );
    let oc = "@opencode/ai/providers/openai-compatible";
    assert_eq!(
        run(&model(oc, "deepseek-ai/deepseek-v4-pro", None, "nvidia"), &[Support::efforts(&["none", "high", "max"])]),
        json!([
            {"id": "none", "body": {"chat_template_kwargs": {"thinking": false}}},
            {"id": "high", "body": {"chat_template_kwargs": {"thinking": true, "reasoning_effort": "high"}}},
            {"id": "max", "body": {"chat_template_kwargs": {"thinking": true, "reasoning_effort": "max"}}},
        ])
    );
    assert_eq!(
        run(&model(oc, "moonshotai/kimi-k2.6", None, "nvidia"), &[Support::efforts(&["none", "low", "high", "max"])]),
        json!([
            {"id": "none", "body": {"chat_template_kwargs": {"thinking": false}}},
            {"id": "thinking", "body": {"chat_template_kwargs": {"thinking": true}}},
        ])
    );
    assert_eq!(
        run(&model(oc, "moonshotai/kimi-k3", None, "nvidia"), &[Support::Toggle, Support::efforts(&["low", "high", "max"])]),
        json!([
            {"id": "none", "body": {"chat_template_kwargs": {"thinking": false}}},
            {"id": "low", "settings": {"reasoningEffort": "low"}},
            {"id": "high", "settings": {"reasoningEffort": "high"}},
            {"id": "max", "settings": {"reasoningEffort": "max"}},
        ])
    );
    assert_eq!(
        run(&model(oc, "minimaxai/minimax-m3", None, "nvidia"), &[Support::Toggle]),
        json!([
            {"id": "none", "body": {"chat_template_kwargs": {"thinking_mode": "disabled"}}},
            {"id": "thinking", "body": {"chat_template_kwargs": {"thinking_mode": "enabled"}}},
        ])
    );
    assert_eq!(
        run(&m("@opencode/ai/providers/baseten", "zai-org/GLM-5.2"), &[Support::efforts(&["none", "high", "max"])]),
        json!([
            {"id": "none", "settings": {"reasoningEffort": "none"}, "body": {"chat_template_args": {"enable_thinking": false}}},
            {"id": "high", "settings": {"reasoningEffort": "high"}, "body": {"chat_template_args": {"enable_thinking": true}}},
            {"id": "max", "settings": {"reasoningEffort": "max"}, "body": {"chat_template_args": {"enable_thinking": true}}},
        ])
    );
    assert_eq!(
        run(&m("@opencode/ai/providers/baseten", "moonshotai/Kimi-K2.6"), &[Support::Toggle]),
        json!([
            {"id": "none", "body": {"chat_template_args": {"enable_thinking": false}}},
            {"id": "thinking", "body": {"chat_template_args": {"enable_thinking": true}}},
        ])
    );
    assert_eq!(
        run(&m("@opencode/ai/providers/deepinfra", "zai-org/GLM-5.2"), &[Support::Toggle, Support::efforts(&["low", "high", "xhigh"])]),
        json!([
            {"id": "none", "body": {"reasoning": {"enabled": false}}},
            {"id": "low", "settings": {"reasoningEffort": "low"}},
            {"id": "high", "settings": {"reasoningEffort": "high"}},
            {"id": "xhigh", "settings": {"reasoningEffort": "xhigh"}},
        ])
    );
}

#[test]
fn spells_gitlab_duo_variants() {
    let pkg = "aisdk:gitlab-ai-provider";
    let gitlab = |id: &str, output: Option<i64>| model(pkg, id, output, "gitlab");
    let adaptive = |e: &str| json!({"id": e, "settings": {"thinking": {"type": "adaptive", "effort": e}}});
    assert_eq!(
        run(&gitlab("duo-chat-opus-5-5", None), &[Support::efforts(&["low", "medium", "high", "xhigh", "max"])]),
        Value::Array(["low", "medium", "high", "xhigh", "max"].iter().map(|e| adaptive(e)).collect())
    );
    assert_eq!(
        run(&gitlab("duo-chat-sonnet-5", None), &[Support::Toggle, Support::efforts(&["low", "max"])]),
        json!([{"id": "none", "settings": {"thinking": {"type": "disabled"}}}, adaptive("low"), adaptive("max")])
    );
    assert_eq!(run(&gitlab("duo-chat-fable-5-1", None), &[Support::Toggle, Support::efforts(&["high"])]), json!([adaptive("high")]));
    assert_eq!(
        run(&gitlab("duo-chat-opus-4-5", Some(64_000)), &[Support::effort()]),
        json!([
            {"id": "high", "settings": {"thinking": {"type": "enabled", "budgetTokens": 16000}}},
            {"id": "max", "settings": {"thinking": {"type": "enabled", "budgetTokens": 31999}}},
        ])
    );
    assert_eq!(
        run(&gitlab("duo-chat-gpt-6-sol", None), &[Support::efforts(&["none", "low", "max"])]),
        json!([
            {"id": "none", "settings": {"reasoningEffort": "none"}},
            {"id": "low", "settings": {"reasoningEffort": "low"}},
            {"id": "max", "settings": {"reasoningEffort": "max"}},
        ])
    );
    assert_eq!(run(&gitlab("duo-chat-gpt-5-3-codex", None), &[Support::efforts(&["low"])]), json!([{"id": "low", "settings": {"reasoningEffort": "low"}}]));
    assert_eq!(run(&gitlab("duo-chat-gpt-5-4", None), &[]), json!([]));
    assert_eq!(run(&gitlab("duo-workflow-opus-5-5", None), &[Support::effort()]), json!([]));
}

#[test]
fn unknown_packages_have_no_variants_and_key_order_is_upstream() {
    assert_eq!(run(&m("aisdk:custom-sdk", "x"), &[Support::effort()]), json!([]));
    let v = resolve(&m("@opencode/ai/providers/anthropic", "claude-opus-4-5"), &[Support::efforts(&["low"])]);
    assert_eq!(
        serde_json::to_string(&v).unwrap(),
        r#"[{"id":"low","settings":{"thinking":{"type":"enabled","budgetTokens":16000},"effort":"low"}}]"#
    );
}

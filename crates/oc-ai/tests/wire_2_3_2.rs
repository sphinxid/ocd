//! opencode 2.3.2 (#64542, #64544): every Responses route sends the system prompt as a leading developer message
//! instead of top-level `instructions`, which GPT-6.2 models never cache. Port of the new
//! `provider/openai-responses-cache.recorded.test.ts` case; the request bodies are compared with upstream's
//! recording byte for byte.

mod common;

use common::*;
use oc_ai::testing::decode_ok;
use oc_ai::*;
use serde_json::{json, Value};

fn usage(events: &[LLMEvent]) -> Usage {
    events
        .iter()
        .rev()
        .find_map(|e| match e {
            LLMEvent::Finish { usage, .. } => usage.clone(),
            _ => None,
        })
        .expect("usage")
}

/// "caches the system prompt on gpt-6.2-sol"
#[tokio::test]
async fn gpt_6_2_caches_the_system_prompt_sent_as_a_developer_message() {
    let rec = load("recorded/openai-responses-cache/caches-the-system-prompt-on-gpt-6-2-sol.json");
    let recorded: Value = serde_json::from_str(&rec.interactions[0].request.body).unwrap();
    // The recording's system prompt (upstream's `LARGE_CACHEABLE_SYSTEM`).
    let system = recorded["input"][0]["content"].as_str().unwrap().to_string();
    let model = oc_ai::providers::language_model(
        "@opencode/ai/providers/openai",
        "gpt-6.2-sol",
        json!({"apiKey": "fixture"}).as_object().unwrap(),
    )
    .unwrap();
    let mut req = LLMRequest::new(model).system(system).prompt("Say hi.").provider_option("reasoningEffort", json!("low"));
    req.generation = Some(GenerationOptions { max_tokens: Some(64.0), ..Default::default() });
    req.prompt_cache_key = Some("recorded-cache-test-gpt-6-2".into());
    let compiled = Client::new().compile(&req).await.unwrap();
    for interaction in &rec.interactions {
        json_eq(&compiled.prepared.body, &interaction.request.body);
    }
    let body: Value = serde_json::from_str(&compiled.prepared.body).unwrap();
    assert!(body.get("instructions").is_none());
    assert_eq!(body["input"][0]["role"], "developer");

    let first = usage(&decode_ok(&req, &rec.interactions[0].response.bytes()));
    assert!(first.cache_write_input_tokens.unwrap_or(0) > 1024, "{first:?}");
    let second = usage(&decode_ok(&req, &rec.interactions[1].response.bytes()));
    assert!(second.cache_read_input_tokens.unwrap_or(0) > 1024, "{second:?}");
}

//! Decode every recorded success response (upstream `packages/ai/test/fixtures/recordings`) through the
//! provider factory's route, asserting each stream ends with exactly one `finish` and no errors.

use oc_ai::testing::{decode, recording};
use oc_ai::*;
use serde_json::{json, Value};

fn package_for(dir: &str, path: &str) -> Option<&'static str> {
    let kind = if path.ends_with("/chat/completions") {
        "chat"
    } else if path.ends_with("/messages") {
        "messages"
    } else if path.ends_with("/responses") {
        "responses"
    } else {
        "native"
    };
    Some(match (dir, kind) {
        ("alibaba-chat" | "alibaba-messages" | "alibaba-responses", "chat") => "alibaba/chat",
        ("alibaba-chat" | "alibaba-messages" | "alibaba-responses", "messages") => "alibaba/messages",
        ("alibaba-chat" | "alibaba-messages" | "alibaba-responses", "responses") => "alibaba/responses",
        ("azure", "chat") => "azure/chat",
        ("azure", "responses") => "azure/responses",
        ("bedrock-mantle" | "bedrock-mantle-messages", "responses") => "amazon-bedrock/mantle/responses",
        ("bedrock-mantle" | "bedrock-mantle-messages", "chat") => "amazon-bedrock/mantle/chat",
        ("bedrock-mantle" | "bedrock-mantle-messages", "messages") => "amazon-bedrock/mantle/messages",
        ("cerebras-chat", _) => "cerebras",
        ("cloudflare-workers-ai", _) => "cloudflare-workers-ai",
        ("deepinfra-chat", _) => "deepinfra",
        ("digitalocean-chat", _) => "digitalocean",
        ("groq-chat", _) => "groq",
        (d, "chat") if d.starts_with("meta") => "meta/chat",
        (d, "messages") if d.starts_with("meta") => "meta/messages",
        (d, "responses") if d.starts_with("meta") => "meta/responses",
        (d, "chat") if d.starts_with("minimax") => "minimax/chat",
        (d, "messages") if d.starts_with("minimax") => "minimax/messages",
        (d, "responses") if d.starts_with("minimax") => "minimax/responses",
        (d, "chat") if d.starts_with("moonshot") => "moonshot/chat",
        (d, "messages") if d.starts_with("moonshot") => "moonshot/messages",
        (d, "responses") if d.starts_with("moonshot") => "moonshot/responses",
        ("venice-chat", _) => "venice",
        ("zai-chat", _) => "zai/chat",
        ("vercel-native", "chat") => "vercel-ai-gateway/chat",
        ("vercel-native", "messages") => "vercel-ai-gateway/messages",
        ("vercel-native", "responses") => "vercel-ai-gateway/responses",
        ("openai-responses-cache", _) => "openai/responses",
        ("gemini-cache", _) => "google",
        ("anthropic-messages-cache", _) => "anthropic",
        ("bedrock-converse-cache", _) => "amazon-bedrock",
        _ => return None,
    })
}

#[test]
fn recorded_streams_decode() {
    let root = format!("{}/tests/fixtures/recorded", env!("CARGO_MANIFEST_DIR"));
    let mut checked = 0;
    let mut failures = vec![];
    let mut dirs: Vec<_> = std::fs::read_dir(&root).unwrap().flatten().collect();
    dirs.sort_by_key(|d| d.file_name());
    for dir in dirs {
        let dir_name = dir.file_name().to_string_lossy().to_string();
        let mut files: Vec<_> = std::fs::read_dir(dir.path()).unwrap().flatten().collect();
        files.sort_by_key(|f| f.file_name());
        for file in files {
            let rec = recording(&std::fs::read_to_string(file.path()).unwrap());
            for (i, interaction) in rec.interactions.iter().enumerate() {
                if interaction.response.status != 200 || interaction.response.body.len() < 10 {
                    continue;
                }
                let url = url::Url::parse(&interaction.request.url).unwrap();
                let Some(package) = package_for(&dir_name, url.path()) else { continue };
                let body: Value = serde_json::from_str(&interaction.request.body).unwrap_or(json!({}));
                let model_id = body
                    .get("model")
                    .or_else(|| body.get("modelId"))
                    .and_then(Value::as_str)
                    .map(String::from)
                    .unwrap_or_else(|| "gemini-2.5-flash".into());
                let mut settings = serde_json::Map::new();
                settings.insert("apiKey".into(), json!("fixture"));
                settings.insert("region".into(), json!("us-east-1"));
                settings.insert("resourceName".into(), json!("fixture"));
                settings.insert("accountId".into(), json!("fixture"));
                let model = oc_ai::providers::language_model(&format!("@opencode/ai/providers/{package}"), &model_id, &settings)
                    .unwrap_or_else(|e| panic!("{package}: {e}"));
                let req = LLMRequest::new(model);
                let out = decode(&req, &interaction.response.bytes());
                checked += 1;
                let name = format!("{dir_name}/{}#{i}", file.file_name().to_string_lossy());
                match out.last() {
                    Some(Ok(LLMEvent::Finish { .. })) => {}
                    Some(Err(e)) => failures.push(format!("{name}: {} ({})", e.message, e.tag())),
                    other => failures.push(format!("{name}: ended with {other:?}")),
                }
                let finishes = out.iter().filter(|e| matches!(e, Ok(LLMEvent::Finish { .. }))).count();
                if finishes > 1 {
                    failures.push(format!("{name}: {finishes} finish events"));
                }
            }
        }
    }
    eprintln!("decoded {checked} recorded responses");
    assert!(checked > 100, "checked {checked}");
    assert!(failures.is_empty(), "{} failures:\n{}", failures.len(), failures.join("\n"));
}

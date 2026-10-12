//! System prompt assembly (`session/system-prompt.ts`, `plugin/optimize.ts`, `plugin/identity.ts`)
//! and request shaping (`session/model-request.ts` output limit + media rules).

use oc_ai::types::{ContentPart, Message, ToolResultValue};

pub const SYSTEM: &str = include_str!("../../../assets/prompt/system/default.txt");
const GPT: &str = include_str!("../../../assets/prompt/system/gpt.txt");
const ASTRA: &str = include_str!("../../../assets/prompt/system/gpt-astra.txt");
const KIMI: &str = include_str!("../../../assets/prompt/system/kimi.txt");
const META: &str = include_str!("../../../assets/prompt/system/meta.txt");
const TRINITY: &str = include_str!("../../../assets/prompt/system/trinity.txt");
const ANTHROPIC: &str = include_str!("../../../assets/prompt/system/anthropic.txt");

/// `SessionSystemPrompt.render`: replaces the first `${OPENCODE_TOOL_GUIDANCE}`.
pub fn render(prompt: &str, tools: &[&str]) -> String {
    let mut lines: Vec<&str> = Vec::new();
    if tools.contains(&"shell") {
        lines.push("- Prefer dedicated tools over shell commands; fall back to the shell when a tool cannot do what you need.");
        lines.push("- Do not chain shell commands with separators like `echo \"====\";` or `printf '---'`; the output becomes noisy in a way that makes the user's side of the conversation worse.");
    }
    if tools.contains(&"write") {
        lines.push("- Use the write tool to create files or completely replace their content. Prefer using the edit tool for targeted changes.");
    }
    if tools.contains(&"edit") {
        lines.push("- Use the edit tool for targeted changes to existing text files. It replaces the exact text in `oldString` with `newString`, and the values must differ. By default, `oldString` must occur exactly once. If it occurs multiple times, include more surrounding context to make it unique or set `replaceAll` to true to replace every occurrence.");
    }
    prompt.replacen("${OPENCODE_TOOL_GUIDANCE}", &lines.join("\n"), 1)
}

pub struct ModelIdentity<'a> {
    /// Catalog model id (`Model.Info.id`).
    pub id: &'a str,
    pub name: &'a str,
    pub provider_id: &'a str,
}

/// System parts for primary/compaction/generate requests:
/// `[base (agent.system || system.txt, then model-family hooks), identity, baseline]`.
pub fn system_parts(agent_system: Option<&str>, tools: &[&str], model: &ModelIdentity, baseline: &str) -> Vec<String> {
    let mut base = match agent_system {
        Some(s) => s.to_string(),
        None => render(SYSTEM, tools),
    };
    if agent_system.is_none() {
        let id = model.id.to_lowercase();
        let hooks: [(bool, Option<String>, bool); 5] = [
            (id.contains("gpt"), Some(if id.contains("astra") { ASTRA } else { GPT }.to_string()), false),
            (id.contains("claude"), Some(ANTHROPIC.to_string()), true),
            (id.contains("kimi"), Some(KIMI.to_string()), false),
            (id.contains("trinity"), Some(TRINITY.to_string()), false),
            (id.contains("muse"), Some(META.replace("{{MODEL_NAME}}", model.name)), false),
        ];
        for (matched, template, append) in hooks {
            if !matched {
                continue;
            }
            let rendered = render(&template.unwrap(), tools);
            base = if append { format!("{base}\n\n{rendered}") } else { rendered };
        }
    }
    let identity = format!(
        "# Your Model\n- Name: {}\n- Provider ID: {}\n- Model ID: {}",
        model.name, model.provider_id, model.id
    );
    [base, identity, baseline.to_string(), SESSION_MOVE_GUIDANCE.to_string()].into_iter().filter(|p| !p.is_empty()).collect()
}

/// System part the built-in `opencode.tools` plugin appends on every `context`, `compaction` and
/// `generate` hook (core/src/tool/plugin/opencode.ts).
pub const SESSION_MOVE_GUIDANCE: &str = "When you create a worktree outside the current working directory and intend to use it as your primary working directory, consider using `execute` to call `tools.opencode.session_move` and make the worktree the session's working directory.";

/// `outputLimit` (`model-request.ts:88-98`).
pub fn output_limit(limit_output: u64, limit_context: u64, compaction: bool, input: Option<(u64, u64)>) -> u64 {
    let model = (if limit_output > 0 { limit_output } else { 32_000 }).min(256_000);
    let requested = if compaction { model.min(32_000) } else { model };
    let Some((measured, estimated)) = input else { return requested };
    if limit_context == 0 {
        return requested;
    }
    let room = limit_context as i64 - measured as i64 - (estimated as f64 * 1.15).ceil() as i64;
    requested.min(room.max(1_024) as u64)
}

fn modality(mime: &str) -> Option<&'static str> {
    if mime.starts_with("image/") {
        Some("image")
    } else if mime.starts_with("audio/") {
        Some("audio")
    } else if mime.starts_with("video/") {
        Some("video")
    } else if mime == "application/pdf" {
        Some("pdf")
    } else {
        None
    }
}

fn unsupported(name: Option<&str>, modality: &str, mime: &str, by_mime: bool) -> String {
    let what = name.map(|n| format!("\"{n}\"")).unwrap_or_else(|| modality.to_string());
    let input = if by_mime { mime } else { modality };
    format!("ERROR: Cannot read {what} (this model does not support {input} input). Inform the user.")
}

/// `unsupportedParts`: replace media the model cannot read.
pub fn unsupported_parts(messages: &mut [Message], capabilities_input: &[String], provider: &str) {
    let supports = |m: &str| capabilities_input.iter().any(|c| c.starts_with(m));
    let xai_bad = |mime: &str| provider == "xai" && mime.starts_with("image/") && !["image/png", "image/jpeg", "image/webp"].contains(&mime);
    for msg in messages.iter_mut() {
        for part in msg.content.iter_mut() {
            if let ContentPart::Media(m) = part {
                let mime = m.media.media_type();
                let Some(md) = modality(&mime) else { continue };
                if !supports(md) || xai_bad(&mime) {
                    *part = ContentPart::text(unsupported(m.filename.as_deref(), md, &mime, xai_bad(&mime)));
                }
            }
        }
    }
}

fn encoded_len(m: &oc_ai::media::MediaAsset) -> usize {
    match &m.source {
        oc_ai::media::MediaSource::Base64 { data, .. } => data.len(),
        oc_ai::media::MediaSource::Bytes { data, .. } => data.len().div_ceil(3) * 4,
        _ => 0,
    }
}

const REMOVED_IMAGE: &str = "[This image was removed to reduce the request size and is no longer visible. Do not make claims about its contents from memory. If needed, retrieve it again with an available tool or ask the user to attach it again.]";

/// `boundImages`: drop oldest images when total > 25 MiB until ≤ 15 MiB.
pub fn bound_images(messages: &mut [Message]) {
    const MAX: usize = 25 * 1024 * 1024;
    const TARGET: usize = 15 * 1024 * 1024;
    let size = |p: &ContentPart| -> usize {
        match p {
            ContentPart::Media(m) if m.media.media_type().starts_with("image/") => encoded_len(&m.media),
            ContentPart::ToolResult(r) => match &r.result {
                ToolResultValue::Content(items) => items
                    .iter()
                    .filter_map(|c| serde_json::to_value(c).ok())
                    .filter(|v| v["type"] == "file" && v["mime"].as_str().is_some_and(|m| m.starts_with("image/")))
                    .map(|v| v["uri"].as_str().map(str::len).unwrap_or(0))
                    .sum(),
                _ => 0,
            },
            _ => 0,
        }
    };
    let total: usize = messages.iter().flat_map(|m| m.content.iter()).map(size).sum();
    if total <= MAX {
        return;
    }
    let mut removed = 0usize;
    for msg in messages.iter_mut() {
        for part in msg.content.iter_mut() {
            if total - removed <= TARGET {
                return;
            }
            if let ContentPart::Media(m) = part {
                if m.media.media_type().starts_with("image/") {
                    removed += encoded_len(&m.media);
                    *part = ContentPart::text(REMOVED_IMAGE);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn limits() {
        assert_eq!(output_limit(0, 0, false, None), 32_000);
        assert_eq!(output_limit(400_000, 0, false, None), 256_000);
        assert_eq!(output_limit(64_000, 200_000, true, None), 32_000);
        assert_eq!(output_limit(64_000, 100_000, false, Some((90_000, 10_000))), 1_024);
    }
    #[test]
    fn family_prompts() {
        let m = ModelIdentity { id: "claude-sonnet", name: "Claude", provider_id: "anthropic" };
        let parts = system_parts(None, &["shell", "edit"], &m, "");
        assert_eq!(parts.len(), 3);
        assert!(parts[0].ends_with(&format!("\n\n{ANTHROPIC}")));
        assert!(parts[1].starts_with("# Your Model\n- Name: Claude"));
        assert_eq!(parts[2], SESSION_MOVE_GUIDANCE);
    }
}

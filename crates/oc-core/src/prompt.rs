//! System prompt assembly (`session/system-prompt.ts`, `plugin/optimize.ts`, `plugin/identity.ts`)
//! and request shaping (`session/model-request.ts` output limit + media rules).

use oc_ai::types::{ContentPart, Message, ToolContent, ToolResultValue};

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

/// Share of the window a primary turn leaves unused when its output limit is fitted to the room left
/// (`WINDOW_HEADROOM`). Providers count a few tokens more than we do (chat template, tool schemas), and strict ones
/// reject input plus output past the window. Compaction sizes its summary against its own reserve instead.
pub const WINDOW_HEADROOM: f64 = 0.01;

/// `outputLimit` (`model-request.ts`): the catalog limit, fitted to the room the prompt leaves in the context
/// window less `headroom`. `input` is `(measured, estimated)`.
pub fn output_limit(limit_output: u64, limit_context: u64, input: Option<(u64, u64)>, headroom: f64) -> u64 {
    let requested = (if limit_output > 0 { limit_output } else { 32_000 }).min(256_000);
    let Some((measured, estimated)) = input else { return requested };
    if limit_context == 0 {
        return requested;
    }
    let window = (limit_context as f64 * (1.0 - headroom)).floor() as i64;
    let room = window - measured as i64 - (estimated as f64 * 1.15).ceil() as i64;
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

/// xAI rejects any other image type (e.g. GIF) with invalid_image, and the stored image would fail every later turn.
const XAI_IMAGE_MIMES: [&str; 3] = ["image/png", "image/jpeg", "image/webp"];

/// `unsupportedMedia`
fn unsupported_media(mime: &str, name: Option<&str>, capabilities_input: &[String], provider: &str) -> Option<String> {
    let md = modality(mime)?;
    let input = if !capabilities_input.iter().any(|c| c.starts_with(md)) {
        md.to_string()
    } else if provider == "xai" && md == "image" && !XAI_IMAGE_MIMES.contains(&mime.to_lowercase().as_str()) {
        mime.to_string()
    } else {
        return None;
    };
    let what = name.map(|n| format!("\"{n}\"")).unwrap_or_else(|| md.to_string());
    Some(format!("ERROR: Cannot read {what} (this model does not support {input} input). Inform the user."))
}

/// A media item as `replaceMedia` hands it to its callback.
struct MediaItem<'a> {
    mime: String,
    name: Option<&'a str>,
}

/// `replaceMedia`: replaces media parts, and the file items of tool results, with the returned text.
fn replace_media(messages: &mut [Message], mut replace: impl FnMut(&MediaItem) -> Option<String>) {
    for msg in messages.iter_mut() {
        for part in msg.content.iter_mut() {
            match part {
                ContentPart::Media(m) => {
                    let item = MediaItem { mime: m.media.media_type(), name: m.filename.as_deref() };
                    if let Some(text) = replace(&item) {
                        *part = ContentPart::text(text);
                    }
                }
                ContentPart::ToolResult(r) => {
                    let ToolResultValue::Content(items) = &mut r.result else { continue };
                    for item in items.iter_mut() {
                        let ToolContent::File { mime, name, .. } = item else { continue };
                        let media = MediaItem { mime: mime.clone(), name: name.as_deref() };
                        if let Some(text) = replace(&media) {
                            *item = ToolContent::Text { text };
                        }
                    }
                }
                _ => {}
            }
        }
    }
}

/// `unsupportedParts`: replace media the model cannot read.
pub fn unsupported_parts(messages: &mut [Message], capabilities_input: &[String], provider: &str) {
    replace_media(messages, |m| unsupported_media(&m.mime, m.name, capabilities_input, provider));
}

/// Remote and provider-referenced media carry no local payload and never count toward the inline budget.
fn encoded_len(m: &oc_ai::media::MediaAsset) -> usize {
    match &m.source {
        oc_ai::media::MediaSource::Base64 { data, .. } => data.len(),
        oc_ai::media::MediaSource::Bytes { data, .. } => data.len().div_ceil(3) * 4,
        _ => 0,
    }
}

pub const REMOVED_IMAGE: &str = "[This image was removed to reduce the request size and is no longer visible. Do not make claims about its contents from memory. If needed, retrieve it again with an available tool or ask the user to attach it again.]";
const IMAGE_BYTES_TRIGGER: usize = 25 * 1024 * 1024;
const IMAGE_BYTES_TARGET: usize = 15 * 1024 * 1024;

fn is_image(mime: &str) -> bool {
    mime.to_lowercase().starts_with("image/")
}

/// `boundImages`: images are replayed in order, and whenever the kept payload passes 25 MiB the oldest are
/// dropped down to 15 MiB. A new image then moves the cut only every 10 MiB instead of on every request, so the
/// replaced prefix, and with it the provider's prompt cache, stays the same between cuts. Image files in tool
/// results count and are dropped like attached images.
pub fn bound_images(messages: &mut [Message]) {
    let mut sizes: Vec<usize> = Vec::new();
    for part in messages.iter().flat_map(|m| m.content.iter()) {
        match part {
            ContentPart::Media(m) if is_image(&m.media.media_type()) => sizes.push(encoded_len(&m.media)),
            ContentPart::ToolResult(r) => {
                if let ToolResultValue::Content(items) = &r.result {
                    for item in items {
                        if let ToolContent::File { uri, mime, .. } = item
                            && is_image(mime)
                        {
                            sizes.push(uri.len());
                        }
                    }
                }
            }
            _ => {}
        }
    }
    let mut cut = 0usize;
    let mut kept = 0usize;
    for size in &sizes {
        kept += size;
        if kept <= IMAGE_BYTES_TRIGGER {
            continue;
        }
        while kept > IMAGE_BYTES_TARGET {
            kept -= sizes[cut];
            cut += 1;
        }
    }
    if cut == 0 {
        return;
    }
    let mut index = 0usize;
    replace_media(messages, |m| {
        if !is_image(&m.mime) {
            return None;
        }
        let removed = index < cut;
        index += 1;
        removed.then(|| REMOVED_IMAGE.to_string())
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn limits() {
        assert_eq!(output_limit(0, 0, None, 0.0), 32_000);
        assert_eq!(output_limit(400_000, 0, None, 0.0), 256_000);
        // Summaries no longer have their own 32k cap.
        assert_eq!(output_limit(64_000, 200_000, None, 0.0), 64_000);
        assert_eq!(output_limit(64_000, 100_000, Some((90_000, 10_000)), 0.0), 1_024);
        // Primary requests leave 1% of the window unused (#59934).
        assert_eq!(output_limit(64_000, 200_000, Some((150_000, 0)), 0.0), 50_000);
        assert_eq!(output_limit(64_000, 200_000, Some((150_000, 0)), WINDOW_HEADROOM), 48_000);
        assert_eq!(output_limit(64_000, 200_000, Some((140_000, 1_000)), WINDOW_HEADROOM), 56_850);
    }

    fn image(bytes: usize) -> ContentPart {
        ContentPart::Media(oc_ai::types::MediaPart {
            media: oc_ai::MediaAsset::base64("a".repeat(bytes), "image/png".to_string()),
            filename: None,
            cache: None,
            metadata: None,
            provider_metadata: None,
        })
    }

    fn tool_image(bytes: usize) -> ContentPart {
        ContentPart::ToolResult(oc_ai::types::ToolResultPart {
            id: "call".into(),
            name: "read".into(),
            namespace: None,
            result: ToolResultValue::Content(vec![
                ToolContent::Text { text: "Image read successfully".into() },
                ToolContent::File { uri: "u".repeat(bytes), mime: "image/png".into(), name: None },
            ]),
            provider_executed: None,
            cache: None,
            metadata: None,
            provider_metadata: None,
        })
    }

    fn removed(messages: &[Message]) -> Vec<bool> {
        messages
            .iter()
            .flat_map(|m| m.content.iter())
            .map(|p| match p {
                ContentPart::Text(t) => t.text == REMOVED_IMAGE,
                ContentPart::ToolResult(r) => matches!(&r.result, ToolResultValue::Content(items) if items.iter().any(|i| matches!(i, ToolContent::Text { text } if text == REMOVED_IMAGE))),
                _ => false,
            })
            .collect()
    }

    // session-model-request.test.ts "image budget": the cut is stable as images are added, and tool-result
    // images count and are removed too.
    #[test]
    fn bound_images_cut_is_stable() {
        const MIB: usize = 1024 * 1024;
        let build = |n: usize| -> Vec<Message> { (0..n).map(|i| Message::user(vec![if i % 2 == 0 { image(4 * MIB) } else { tool_image(4 * MIB) }])).collect() };
        // 6 × 4 MiB = 24 MiB: under the trigger, nothing removed.
        let mut six = build(6);
        bound_images(&mut six);
        assert_eq!(removed(&six), vec![false; 6]);
        // The 7th passes 25 MiB: the oldest go until at most 15 MiB is kept (28 → 12 MiB: 4 dropped).
        let mut seven = build(7);
        bound_images(&mut seven);
        assert_eq!(removed(&seven), vec![true, true, true, true, false, false, false]);
        // Adding more keeps the same cut until the kept payload passes the trigger again.
        for n in 8..=9 {
            let mut more = build(n);
            bound_images(&mut more);
            let mut expected = vec![true; 4];
            expected.extend(vec![false; n - 4]);
            assert_eq!(removed(&more), expected, "{n} images");
        }
        // 10 images: kept 12 + 3 × 4 = 24, still ≤ 25; 11: 28 → cut moves.
        let mut eleven = build(11);
        bound_images(&mut eleven);
        assert_eq!(removed(&eleven).iter().filter(|r| **r).count(), 8);
    }

    // Upstream: "keeps the replaced prefix stable as new images arrive, so the prompt cache survives".
    #[test]
    fn bound_images_keeps_replaced_prefix_stable() {
        let count = |n: usize| {
            let mut history: Vec<Message> = (0..n).map(|_| Message::user(vec![image(3 * 1024 * 1024)])).collect();
            bound_images(&mut history);
            history.iter().filter(|m| matches!(m.content[0], ContentPart::Text(_))).count()
        };
        assert_eq!(count(9), 4);
        assert_eq!(count(10), 4);
        assert_eq!(count(12), 4);
        assert_eq!(count(13), 8);
    }

    #[test]
    fn unsupported_tool_result_media() {
        let mut messages = vec![Message::user(vec![tool_image(10)])];
        unsupported_parts(&mut messages, &["text".to_string()], "");
        let ContentPart::ToolResult(r) = &messages[0].content[0] else { panic!() };
        assert_eq!(
            r.result,
            ToolResultValue::Content(vec![
                ToolContent::Text { text: "Image read successfully".into() },
                ToolContent::Text { text: "ERROR: Cannot read image (this model does not support image input). Inform the user.".into() },
            ])
        );
        // xAI: a supported modality with an unsupported image type names the type.
        let mut gif = vec![Message::user(vec![ContentPart::Media(oc_ai::types::MediaPart {
            media: oc_ai::MediaAsset::base64("aa".to_string(), "image/gif".to_string()),
            filename: Some("a.gif".into()),
            cache: None,
            metadata: None,
            provider_metadata: None,
        })])];
        unsupported_parts(&mut gif, &["image".to_string()], "xai");
        assert_eq!(
            gif[0].content[0],
            ContentPart::text("ERROR: Cannot read \"a.gif\" (this model does not support image/gif input). Inform the user.")
        );
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

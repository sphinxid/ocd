//! gray-matter compatible frontmatter parsing (`core/src/config/markdown.ts`).
//!
//! YAML is parsed with `serde_yaml` (YAML 1.2 core schema like js-yaml's safe schema, except that
//! timestamps stay strings instead of becoming `Date` objects).

use serde_json::{Map, Value};

/// Parsed Markdown file.
#[derive(Debug, Clone, PartialEq)]
pub struct Markdown {
    /// Frontmatter data (`{}` when absent or empty).
    pub data: Map<String, Value>,
    /// Body after the closing delimiter.
    pub content: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MarkdownError(pub String);

fn yaml_to_json(value: serde_yaml::Value) -> Value {
    match value {
        serde_yaml::Value::Null => Value::Null,
        serde_yaml::Value::Bool(b) => Value::Bool(b),
        serde_yaml::Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                Value::from(i)
            } else if let Some(u) = n.as_u64() {
                Value::from(u)
            } else {
                let f = n.as_f64().unwrap_or(0.0);
                serde_json::Number::from_f64(f).map(|n| Value::Number(crate::decode::js_number(&n))).unwrap_or(Value::Null)
            }
        }
        serde_yaml::Value::String(s) => Value::String(s),
        serde_yaml::Value::Sequence(items) => Value::Array(items.into_iter().map(yaml_to_json).collect()),
        serde_yaml::Value::Mapping(map) => Value::Object(
            map.into_iter()
                .map(|(k, v)| {
                    let key = match yaml_to_json(k) {
                        Value::String(s) => s,
                        Value::Null => "null".to_string(),
                        other => other.to_string(),
                    };
                    (key, yaml_to_json(v))
                })
                .collect(),
        ),
        serde_yaml::Value::Tagged(tagged) => yaml_to_json(tagged.value),
    }
}

/// `matter(content, {})`. Errors when the YAML block is invalid.
pub fn matter(input: &str) -> Result<Markdown, MarkdownError> {
    let input = input.strip_prefix('\u{feff}').unwrap_or(input);
    let empty = || Markdown { data: Map::new(), content: input.to_string() };
    const OPEN: &str = "---";
    if !input.starts_with(OPEN) {
        return Ok(empty());
    }
    // `----` is not a delimiter.
    if input[OPEN.len()..].starts_with('-') {
        return Ok(empty());
    }
    let mut str = &input[OPEN.len()..];
    let len = str.len();
    // Language after the opening delimiter (rest of the first line).
    let first = str.split('\n').next().unwrap_or("");
    let first = first.strip_suffix('\r').unwrap_or(first);
    let language = first.trim();
    if !language.is_empty() {
        if language != "yaml" && language != "yml" {
            return Err(MarkdownError(format!("gray-matter engine \"{language}\" is not registered")));
        }
        str = &str[first.len()..];
    }
    let len = if language.is_empty() { len } else { str.len() };
    let close = "\n---";
    let close_index = str.find(close).unwrap_or(len);
    let block = &str[..close_index];
    static COMMENT: std::sync::LazyLock<regex::Regex> =
        std::sync::LazyLock::new(|| regex::Regex::new(r"(?m)^\s*#[^\n]+").unwrap());
    let data = if COMMENT.replace_all(block, "").trim().is_empty() {
        Map::new()
    } else {
        let parsed: serde_yaml::Value = serde_yaml::from_str(block).map_err(|e| MarkdownError(e.to_string()))?;
        match yaml_to_json(parsed) {
            Value::Object(m) => m,
            Value::Null => Map::new(),
            _ => return Err(MarkdownError("frontmatter is not a mapping".into())),
        }
    };
    let content = if close_index == len {
        String::new()
    } else {
        let mut rest = &str[close_index + close.len()..];
        if let Some(r) = rest.strip_prefix('\r') {
            rest = r;
        }
        if let Some(r) = rest.strip_prefix('\n') {
            rest = r;
        }
        rest.to_string()
    };
    Ok(Markdown { data, content })
}

/// `ConfigMarkdown.parse`: on YAML failure retry with [`sanitize`].
pub fn parse(content: &str) -> Result<Markdown, MarkdownError> {
    matter(content).or_else(|_| matter(&sanitize(content)))
}

/// `ConfigMarkdown.parseOption`.
pub fn parse_option(content: &str) -> Option<Markdown> {
    parse(content).ok()
}

/// `ConfigMarkdown.joinLegacyVariant`: join a legacy separate `variant` into a string `model` so it survives native
/// decoding. Embedded and structured native selections, and a variant without a model, stay unchanged.
pub fn join_legacy_variant(mut data: Map<String, Value>) -> Map<String, Value> {
    let joined = match (data.get("model"), data.get("variant")) {
        (Some(Value::String(model)), Some(Value::String(variant)))
            if !model.contains('#') && !variant.is_empty() && !variant.contains('#') =>
        {
            format!("{model}#{variant}")
        }
        _ => return data,
    };
    data.insert("model".into(), Value::String(joined));
    data
}

/// Rewrite unquoted values containing `:`, or starting with a bracketed tag like `[Tag] text` (2.2.7), as YAML
/// block scalars: other coding agents accept them in frontmatter. Valid YAML flow collections, including ones
/// spanning several lines, are left as they are (2.4.1).
pub fn sanitize(content: &str) -> String {
    static FRONT: std::sync::LazyLock<regex::Regex> =
        std::sync::LazyLock::new(|| regex::Regex::new(r"^---\r?\n((?s:.*?))\r?\n---").unwrap());
    static ENTRY: std::sync::LazyLock<regex::Regex> =
        std::sync::LazyLock::new(|| regex::Regex::new(r"^([a-zA-Z_][a-zA-Z0-9_]*)\s*:\s*(.*)$").unwrap());
    let Some(caps) = FRONT.captures(content) else { return content.to_string() };
    let frontmatter = caps.get(1).unwrap().as_str();
    let mut flow: Option<FlowState> = None;
    let lines: Vec<String> = split_lines(frontmatter)
        .into_iter()
        .flat_map(|line| {
            if let Some(state) = flow.take() {
                flow = match scan_flow(line, state) {
                    Flow::Open(state) => Some(state),
                    _ => None,
                };
                return vec![line.to_string()];
            }
            let trimmed = line.trim();
            if trimmed.starts_with('#') || trimmed.is_empty() || line.starts_with(char::is_whitespace) {
                return vec![line.to_string()];
            }
            let Some(entry) = ENTRY.captures(line) else { return vec![line.to_string()] };
            let value = entry.get(2).unwrap().as_str().trim();
            if value.is_empty() || value.starts_with('"') || value.starts_with('\'') {
                return vec![line.to_string()];
            }
            if value.starts_with('[') || value.starts_with('{') {
                match scan_flow(value, FlowState::default()) {
                    Flow::Trailing => {}
                    Flow::Open(state) => {
                        flow = Some(state);
                        return vec![line.to_string()];
                    }
                    _ => return vec![line.to_string()],
                }
                return vec![format!("{}: |-", &entry[1]), format!("  {value}")];
            }
            if !value.contains(':') || is_valid_yaml(line) {
                return vec![line.to_string()];
            }
            vec![format!("{}: |-", &entry[1]), format!("  {value}")]
        })
        .collect();
    content.replacen(frontmatter, &lines.join("\n"), 1)
}

fn is_valid_yaml(line: &str) -> bool {
    matter(&format!("---\n{line}\n---")).is_ok()
}

/// `FlowState`: the closers still expected and the open quote, carried across lines of a flow collection.
#[derive(Debug, Clone, Default)]
struct FlowState {
    stack: Vec<char>,
    quote: Option<char>,
}

enum Flow {
    Invalid,
    Closed,
    Trailing,
    Open(FlowState),
}

/// `scanFlow`: follow brackets and quotes of a YAML flow collection through `text`.
fn scan_flow(text: &str, state: FlowState) -> Flow {
    let chars: Vec<char> = text.chars().collect();
    let FlowState { mut stack, mut quote } = state;
    let mut i = 0;
    while i < chars.len() {
        let ch = chars[i];
        match quote {
            Some('"') => {
                if ch == '\\' {
                    i += 2;
                    continue;
                }
                if ch == '"' {
                    quote = None;
                }
                i += 1;
                continue;
            }
            Some(_) => {
                if ch == '\'' {
                    if chars.get(i + 1) == Some(&'\'') {
                        i += 2;
                        continue;
                    }
                    quote = None;
                }
                i += 1;
                continue;
            }
            None => {}
        }
        let after = |set: &dyn Fn(char) -> bool| i == 0 || set(chars[i - 1]);
        if (ch == '"' || ch == '\'') && after(&|c| c.is_whitespace() || matches!(c, '[' | '{' | ',' | ':')) {
            quote = Some(ch);
            i += 1;
            continue;
        }
        if ch == '#' && after(&char::is_whitespace) {
            break;
        }
        match ch {
            '[' => stack.push(']'),
            '{' => stack.push('}'),
            ']' | '}' => {
                if stack.last() != Some(&ch) {
                    return Flow::Invalid;
                }
                stack.pop();
                if stack.is_empty() {
                    let rest: String = chars[i + 1..].iter().collect();
                    let rest = rest.trim();
                    return if rest.is_empty() || rest.starts_with('#') { Flow::Closed } else { Flow::Trailing };
                }
            }
            _ => {}
        }
        i += 1;
    }
    Flow::Open(FlowState { stack, quote })
}

fn split_lines(text: &str) -> Vec<&str> {
    text.split('\n').map(|l| l.strip_suffix('\r').unwrap_or(l)).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn parses_frontmatter_and_body() {
        let md = parse("---\ndescription: hi\nsteps: 3\n---\n\nBody text\n").unwrap();
        assert_eq!(Value::Object(md.data), json!({ "description": "hi", "steps": 3 }));
        assert_eq!(md.content, "\nBody text\n");
    }

    #[test]
    fn no_frontmatter() {
        let md = parse("Just text").unwrap();
        assert!(md.data.is_empty());
        assert_eq!(md.content, "Just text");
    }

    #[test]
    fn retries_unquoted_colons() {
        // Port of markdown.test.ts: values with colons parse after sanitize.
        let md = parse("---\ndescription: Use this: carefully\nmodel: anthropic/claude\n---\nBody").unwrap();
        assert_eq!(md.data["description"], json!("Use this: carefully"));
        assert_eq!(md.data["model"], json!("anthropic/claude"));
        assert_eq!(md.content, "Body");
    }

    /// `markdown.test.ts` "recovers a frontmatter value that starts with a bracketed tag" (2.2.7).
    #[test]
    fn recovers_a_value_that_starts_with_a_bracketed_tag() {
        let md = parse("---\ndescription: [Team] Does a thing\ntools: [read, edit]\n---\nbody").unwrap();
        assert_eq!(md.data["description"], json!("[Team] Does a thing"));
        assert_eq!(md.data["tools"], json!(["read", "edit"]));
        assert_eq!(sanitize("---\nd: {a} b\nt: [x]\n---\n"), "---\nd: |-\n  {a} b\nt: [x]\n---\n");
    }

    #[test]
    fn repeated_failures_still_sanitize() {
        let content = "---\nname: a: b\n---\nx";
        assert!(parse(content).is_ok());
        assert!(parse(content).is_ok());
    }

    #[test]
    fn sanitize_leaves_quoted_and_indented() {
        let s = sanitize("---\na: \"x: y\"\n  b: c: d\nc: |\n---\n");
        assert_eq!(s, "---\na: \"x: y\"\n  b: c: d\nc: |\n---\n");
        // A colon value that parses as YAML is left alone (2.4.1); one that does not is rewritten.
        assert_eq!(sanitize("---\nurl: http://x\n---\nbody"), "---\nurl: http://x\n---\nbody");
        assert_eq!(sanitize("---\nurl: a: b\n---\nbody"), "---\nurl: |-\n  a: b\n---\nbody");
    }

    #[test]
    fn invalid_yaml_is_none() {
        assert!(parse_option("---\n[unclosed\n---\nbody").is_none());
    }

    #[test]
    fn empty_frontmatter() {
        let md = parse("---\n# just a comment\n---\nbody").unwrap();
        assert!(md.data.is_empty());
        assert_eq!(md.content, "body");
    }
}

#[cfg(test)]
mod upstream_tests {
    //! Port of `packages/core/test/config/markdown.test.ts`.
    use super::*;
    use serde_json::json;

    const INVALID_YAML: &str =
        "---\ndescription: Use when the user needs to crawl pages. Keywords: crawl, scrape\n---\nbody";

    #[test]
    fn recovers_unquoted_colon_frontmatter_via_sanitize() {
        let parsed = parse(INVALID_YAML).unwrap();
        assert_eq!(parsed.data["description"], "Use when the user needs to crawl pages. Keywords: crawl, scrape");
        assert_eq!(parsed.content.trim(), "body");
    }

    #[test]
    fn preserves_flow_mappings_and_flow_sequences_when_repairing_another_field() {
        let parsed = parse(
            "---\ndescription: Review changes. Keywords: review, audit\nmodel: { providerID: example, model: chat, variant: high }\ntools: { edit: false }\nmetadata: { \"opencode/autoinvoke\": false }\npermissions: [{ action: edit, resource: \"*\", effect: deny }]\n---\nbody",
        )
        .unwrap();
        assert_eq!(parsed.data["description"], "Review changes. Keywords: review, audit");
        assert_eq!(parsed.data["model"], json!({"providerID": "example", "model": "chat", "variant": "high"}));
        assert_eq!(parsed.data["tools"], json!({"edit": false}));
        assert_eq!(parsed.data["metadata"], json!({"opencode/autoinvoke": false}));
        assert_eq!(parsed.data["permissions"], json!([{"action": "edit", "resource": "*", "effect": "deny"}]));
        assert_eq!(parsed.content.trim(), "body");
    }

    #[test]
    fn preserves_nested_flow_collections_quoted_delimiters_and_inline_comments_during_repair() {
        let parsed = parse(
            "---\ndescription: Review changes. Keywords: review, audit\nnested: [[read], [edit]]\ntools: [read, edit] # comment: with colon\nquoted: [\"a]b\", 'c''}d', don't]\n---\nbody",
        )
        .unwrap();
        assert_eq!(parsed.data["description"], "Review changes. Keywords: review, audit");
        assert_eq!(parsed.data["nested"], json!([["read"], ["edit"]]));
        assert_eq!(parsed.data["tools"], json!(["read", "edit"]));
        assert_eq!(parsed.data["quoted"], json!(["a]b", "c'}d", "don't"]));
    }

    #[test]
    fn preserves_multi_line_flow_collections_while_repairing_fields_before_and_after() {
        let parsed = parse(
            "---\ndescription: Review changes. Keywords: review, audit\nmodel: { providerID: example, # note: don't close } here\n  model: chat,\n  variant: high\n} # trailing: comment\npermissions: [{ action: edit,\nresource: \"*\",\neffect: deny }]\nnote: Remember: check tests\n---\nbody",
        )
        .unwrap();
        assert_eq!(parsed.data["description"], "Review changes. Keywords: review, audit");
        assert_eq!(parsed.data["model"], json!({"providerID": "example", "model": "chat", "variant": "high"}));
        assert_eq!(parsed.data["permissions"], json!([{"action": "edit", "resource": "*", "effect": "deny"}]));
        assert_eq!(parsed.data["note"], "Remember: check tests");
    }

    #[test]
    fn recovers_leading_bracketed_and_braced_tags_with_colons_or_apostrophes() {
        let parsed = parse(
            "---\ndescription: [Team: Core] Does a thing\nsummary: {Team} Review changes\nowner: [User's] Follow up\nmodel: { providerID: example, model: chat }\n---\nbody",
        )
        .unwrap();
        assert_eq!(parsed.data["description"], "[Team: Core] Does a thing");
        assert_eq!(parsed.data["summary"], "{Team} Review changes");
        assert_eq!(parsed.data["owner"], "[User's] Follow up");
        assert_eq!(parsed.data["model"], json!({"providerID": "example", "model": "chat"}));
    }

    #[test]
    fn preserves_flow_collections_with_crlf_line_endings_during_repair() {
        let parsed = parse(
            "---\r\ndescription: Review changes. Keywords: review, audit\r\nmodel: { providerID: example, model: chat }\r\n---\r\nbody",
        )
        .unwrap();
        assert_eq!(parsed.data["description"], "Review changes. Keywords: review, audit");
        assert_eq!(parsed.data["model"], json!({"providerID": "example", "model": "chat"}));
    }

    #[test]
    fn keeps_block_scalar_headers_with_commented_colons_during_repair() {
        let parsed = parse(
            "---\ndescription: Review changes. Keywords: review, audit\nprompt: |- # see: docs\n  Review carefully.\n---\nbody",
        )
        .unwrap();
        assert_eq!(parsed.data["description"], "Review changes. Keywords: review, audit");
        assert_eq!(parsed.data["prompt"], "Review carefully.");
    }

    #[test]
    fn still_fails_on_unclosed_or_mismatched_flow_collections() {
        assert!(parse("---\nmodel: { providerID: example\n---\nbody").is_err());
        assert!(parse("---\ndescription: [Tag} text\n---\nbody").is_err());
    }

    #[test]
    fn join_legacy_variant_rules() {
        let join = |v: serde_json::Value| Value::Object(join_legacy_variant(v.as_object().unwrap().clone()));
        assert_eq!(join(json!({"model": "example/chat", "variant": "high"}))["model"], "example/chat#high");
        assert_eq!(join(json!({"model": "example/chat#low", "variant": "high"}))["model"], "example/chat#low");
        assert_eq!(join(json!({"model": "example/chat", "variant": ""}))["model"], "example/chat");
        assert_eq!(join(json!({"model": "example/chat", "variant": "a#b"}))["model"], "example/chat");
        assert_eq!(join(json!({"variant": "high"})), json!({"variant": "high"}));
        let structured = json!({"model": {"providerID": "p", "model": "m"}, "variant": "high"});
        assert_eq!(join(structured.clone()), structured);
    }

    #[test]
    fn recovers_after_a_previous_failed_parse_and_keeps_failing_consistently() {
        let bad = "---\ndescription: [unclosed\n---\nbody";
        assert!(parse(bad).is_err());
        assert!(parse(bad).is_err());
        assert!(parse(INVALID_YAML).is_ok());
    }
}

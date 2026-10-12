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

/// Rewrite unquoted values containing `:` as YAML block scalars.
pub fn sanitize(content: &str) -> String {
    static FRONT: std::sync::LazyLock<regex::Regex> =
        std::sync::LazyLock::new(|| regex::Regex::new(r"^---\r?\n((?s:.*?))\r?\n---").unwrap());
    static ENTRY: std::sync::LazyLock<regex::Regex> =
        std::sync::LazyLock::new(|| regex::Regex::new(r"^([a-zA-Z_][a-zA-Z0-9_]*)\s*:\s*(.*)$").unwrap());
    let Some(caps) = FRONT.captures(content) else { return content.to_string() };
    let frontmatter = caps.get(1).unwrap().as_str();
    let lines: Vec<String> = split_lines(frontmatter)
        .into_iter()
        .flat_map(|line| {
            let trimmed = line.trim();
            if trimmed.starts_with('#') || trimmed.is_empty() || line.starts_with(char::is_whitespace) {
                return vec![line.to_string()];
            }
            let Some(entry) = ENTRY.captures(line) else { return vec![line.to_string()] };
            let value = entry.get(2).unwrap().as_str().trim();
            if value.is_empty() || value == ">" || value == "|" || value.starts_with('"') || value.starts_with('\'') {
                return vec![line.to_string()];
            }
            if !value.contains(':') {
                return vec![line.to_string()];
            }
            vec![format!("{}: |-", &entry[1]), format!("  {value}")]
        })
        .collect();
    content.replacen(frontmatter, &lines.join("\n"), 1)
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
        assert_eq!(sanitize("---\nurl: http://x\n---\nbody"), "---\nurl: |-\n  http://x\n---\nbody");
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

    const INVALID_YAML: &str =
        "---\ndescription: Use when the user needs to crawl pages. Keywords: crawl, scrape\n---\nbody";

    #[test]
    fn recovers_unquoted_colon_frontmatter_via_sanitize() {
        let parsed = parse(INVALID_YAML).unwrap();
        assert_eq!(parsed.data["description"], "Use when the user needs to crawl pages. Keywords: crawl, scrape");
        assert_eq!(parsed.content.trim(), "body");
    }

    #[test]
    fn recovers_after_a_previous_failed_parse_and_keeps_failing_consistently() {
        let bad = "---\ndescription: [unclosed\n---\nbody";
        assert!(parse(bad).is_err());
        assert!(parse(bad).is_err());
        assert!(parse(INVALID_YAML).is_ok());
    }
}

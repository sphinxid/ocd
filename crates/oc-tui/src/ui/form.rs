//! Form/question helpers (`src/util/form.ts`) and permission presentation (`src/util/permission.ts`).

use serde_json::{Map, Value};

pub fn kind(f: &Value) -> &str {
    f.get("type").and_then(|t| t.as_str()).unwrap_or("")
}

pub fn key(f: &Value) -> String {
    f.get("key").and_then(|t| t.as_str()).unwrap_or("").to_string()
}

pub fn label(f: &Value) -> String {
    f.get("title").and_then(|t| t.as_str()).map(|s| s.to_string()).unwrap_or_else(|| {
        if kind(f) == "external" { f.get("url").and_then(|u| u.as_str()).unwrap_or("").to_string() } else { key(f) }
    })
}

pub fn options(f: &Value) -> Option<&Vec<Value>> {
    f.get("options").and_then(|o| o.as_array())
}

pub fn textual(f: &Value) -> bool {
    matches!(kind(f), "number" | "integer") || (kind(f) == "string" && options(f).is_none())
}

pub fn custom(f: &Value) -> bool {
    let c = f.get("custom").and_then(|c| c.as_bool()) == Some(true);
    (kind(f) == "string" && options(f).is_some() && c) || (kind(f) == "multiselect" && c)
}

/// Choice rows: (value, label, description).
pub fn rows(f: &Value) -> Vec<(Value, String, Option<String>)> {
    if kind(f) == "boolean" {
        return vec![(Value::Bool(true), "Yes".into(), None), (Value::Bool(false), "No".into(), None)];
    }
    match kind(f) {
        "multiselect" | "string" => options(f)
            .map(|o| {
                o.iter()
                    .map(|x| {
                        (
                            x.get("value").cloned().unwrap_or(Value::Null),
                            x.get("label").and_then(|l| l.as_str()).unwrap_or("").to_string(),
                            x.get("description").and_then(|d| d.as_str()).map(|s| s.to_string()),
                        )
                    })
                    .collect()
            })
            .unwrap_or_default(),
        _ => vec![],
    }
}

/// Visible fields given `when` conditions on current answers.
pub fn visible(fields: &[Value], answers: &Map<String, Value>) -> Vec<Value> {
    fields
        .iter()
        .filter(|f| f.get("hidden").and_then(|h| h.as_bool()) != Some(true))
        .filter(|f| {
            f.get("when").and_then(|w| w.as_array()).is_none_or(|conds| {
                conds.iter().all(|c| {
                    let k = c.get("key").and_then(|k| k.as_str()).unwrap_or("");
                    let v = c.get("value").cloned().unwrap_or(Value::Null);
                    let eq = answers.get(k) == Some(&v);
                    if c.get("op").and_then(|o| o.as_str()) == Some("neq") { !eq } else { eq }
                })
            })
        })
        .cloned()
        .collect()
}

pub fn initial(fields: &[Value]) -> Map<String, Value> {
    let mut m = Map::new();
    for f in fields {
        if kind(f) != "external"
            && let Some(d) = f.get("default")
        {
            m.insert(key(f), d.clone());
        }
    }
    m
}

fn valid_url(v: &str) -> bool {
    regex::Regex::new(r"^[a-zA-Z][a-zA-Z0-9+.-]*://\S+$").unwrap().is_match(v)
}

fn valid_date(v: &str) -> bool {
    chrono::NaiveDate::parse_from_str(v, "%Y-%m-%d").is_ok()
}

/// `formValidateValue` (messages verbatim).
pub fn validate(f: &Value, value: Option<&Value>) -> Option<String> {
    let required = f.get("required").and_then(|r| r.as_bool()) == Some(true);
    let Some(value) = value else { return if required { Some("Answer required".into()) } else { None } };
    let empty = value.as_str() == Some("") || value.as_array().is_some_and(|a| a.is_empty());
    if required && empty {
        return Some(if kind(f) == "multiselect" { "Select at least one option" } else { "Answer required" }.into());
    }
    let num = |k: &str| f.get(k).and_then(|v| v.as_f64());
    match kind(f) {
        "string" => {
            let Some(s) = value.as_str() else { return Some("Expected text".into()) };
            let len = s.chars().count() as f64;
            if let Some(n) = num("minLength")
                && len < n
            {
                return Some(format!("Must be at least {} characters", n as i64));
            }
            if let Some(n) = num("maxLength")
                && len > n
            {
                return Some(format!("Must be at most {} characters", n as i64));
            }
            if let Some(p) = f.get("pattern").and_then(|p| p.as_str()) {
                match regex::Regex::new(p) {
                    Ok(re) if !re.is_match(s) => return Some(format!("Must match pattern: {p}")),
                    Err(_) => return Some(format!("Invalid pattern: {p}")),
                    _ => {}
                }
            }
            match f.get("format").and_then(|v| v.as_str()) {
                Some("email") if !regex::Regex::new(r"^[^\s@]+@[^\s@]+\.[^\s@]+$").unwrap().is_match(s) => return Some("Expected an email address".into()),
                Some("uri") if !valid_url(s) => return Some("Expected a URL".into()),
                Some("date") if !valid_date(s) => return Some("Expected a date (YYYY-MM-DD)".into()),
                Some("date-time") if chrono::DateTime::parse_from_rfc3339(s).is_err() => return Some("Expected a date and time".into()),
                _ => {}
            }
            if let Some(opts) = options(f)
                && !custom(f)
                && !opts.iter().any(|o| o.get("value").and_then(|v| v.as_str()) == Some(s))
            {
                return Some("Select an available option".into());
            }
            None
        }
        "number" | "integer" => {
            let Some(n) = value.as_f64().filter(|n| n.is_finite()) else { return Some("Expected a number".into()) };
            if kind(f) == "integer" && n.fract() != 0.0 {
                return Some("Expected an integer".into());
            }
            if let Some(m) = num("minimum")
                && n < m
            {
                return Some(format!("Must be at least {}", crate::util::locale::js_number(m)));
            }
            if let Some(m) = num("maximum")
                && n > m
            {
                return Some(format!("Must be at most {}", crate::util::locale::js_number(m)));
            }
            None
        }
        "boolean" => if value.is_boolean() { None } else { Some("Expected yes or no".into()) },
        "multiselect" => {
            let Some(arr) = value.as_array() else { return Some("Expected selections".into()) };
            if let Some(n) = num("minItems")
                && (arr.len() as f64) < n
            {
                return Some(format!("Select at least {}", n as i64));
            }
            if let Some(n) = num("maxItems")
                && (arr.len() as f64) > n
            {
                return Some(format!("Select at most {}", n as i64));
            }
            if !custom(f) && arr.iter().any(|i| !rows(f).iter().any(|(v, _, _)| v == i)) {
                return Some("Select only available options".into());
            }
            None
        }
        _ => None,
    }
}

pub fn display(f: &Value, value: Option<&Value>, empty_multi: &str) -> String {
    let Some(value) = value else { return String::new() };
    let rs = rows(f);
    let lab = |v: &Value| rs.iter().find(|(x, _, _)| x == v).map(|(_, l, _)| l.clone()).unwrap_or_else(|| match v {
        Value::String(s) => s.clone(),
        other => other.to_string(),
    });
    match value.as_array() {
        Some(a) if a.is_empty() => empty_multi.to_string(),
        Some(a) => a.iter().map(lab).collect::<Vec<_>>().join(", "),
        None => lab(value),
    }
}

pub fn placeholder(f: &Value) -> String {
    if let Some(p) = f.get("placeholder").and_then(|p| p.as_str()) {
        return p.to_string();
    }
    match f.get("format").and_then(|v| v.as_str()) {
        Some("email") => return "name@example.com".into(),
        Some("uri") => return "https://example.com".into(),
        Some("date") => return "YYYY-MM-DD".into(),
        Some("date-time") => return "YYYY-MM-DDTHH:MM:SSZ".into(),
        _ => {}
    }
    let min = f.get("minimum").and_then(|v| v.as_f64());
    let max = f.get("maximum").and_then(|v| v.as_f64());
    match (min, max) {
        (Some(a), Some(b)) => format!("{}-{}", crate::util::locale::js_number(a), crate::util::locale::js_number(b)),
        (Some(a), None) => format!("at least {}", crate::util::locale::js_number(a)),
        (None, Some(b)) => format!("at most {}", crate::util::locale::js_number(b)),
        _ => "Type your answer".into(),
    }
}

/// Permission presentation: (icon, title, lines).
pub fn permission_presentation(action: &str, resources: &[String], metadata: Option<&Value>, fmt: &dyn Fn(&str) -> String) -> (String, String, Vec<String>, Option<String>) {
    let canonical = match action {
        "bash" => "shell",
        "task" => "subagent",
        "apply_patch" => "patch",
        a => a,
    };
    let meta = metadata.cloned().unwrap_or(Value::Null);
    let text = |k: &str| meta.get(k).and_then(|v| v.as_str()).map(|s| s.to_string()).or_else(|| meta.pointer(&format!("/input/{k}")).and_then(|v| v.as_str()).map(|s| s.to_string()));
    let first = resources.first().cloned().unwrap_or_default();
    match canonical {
        "edit" => {
            let file = text("path").or(text("filepath")).unwrap_or(first);
            let diff = meta.pointer("/files/0/patch").and_then(|v| v.as_str()).map(|s| s.to_string()).or_else(|| text("diff"));
            ("→".into(), format!("Edit {}", fmt(&file)), vec![], diff)
        }
        "read" | "list" => {
            let v = text("path").unwrap_or(first);
            let t = if canonical == "read" { "Read" } else { "List" };
            let lines = if v.is_empty() { vec![] } else { vec![format!("Path: {}", fmt(&v))] };
            ("→".into(), format!("{t} {}", fmt(&v)), lines, None)
        }
        "glob" | "grep" => {
            let p = text("pattern").unwrap_or(first);
            let t = if canonical == "glob" { "Glob" } else { "Grep" };
            ("✱".into(), format!("{t} \"{p}\""), vec![format!("Pattern: {p}")], None)
        }
        "shell" => {
            let lines = match text("command") {
                Some(c) => vec![format!("$ {c}")],
                None => resources.iter().map(|r| format!("- {r}")).collect(),
            };
            ("#".into(), "Shell command".into(), lines, None)
        }
        "subagent" => {
            let agent = crate::util::locale::titlecase(&text("subagent_type").or(text("agent")).unwrap_or_else(|| "general".into()));
            let desc = text("description").unwrap_or_default();
            ("#".into(), format!("{agent} Subagent"), vec![format!("◉ {desc}")], None)
        }
        "webfetch" => {
            let url = text("url").unwrap_or(first);
            ("%".into(), format!("WebFetch {url}"), vec![format!("URL: {url}")], None)
        }
        "websearch" => {
            let q = text("query").unwrap_or(first);
            let title = match text("provider") {
                Some(p) => format!("Web Search via {} \"{q}\"", if p == "opencode" { "OpenCode".to_string() } else { crate::util::locale::titlecase(&p) }),
                None => "Web Search".into(),
            };
            ("◈".into(), title, vec![format!("Query: {q}")], None)
        }
        "external_directory" => {
            let raw = text("parentDir").or(text("filepath")).unwrap_or(first);
            let dir = match raw.find('*') {
                Some(i) => raw[..i].trim_end_matches(['/', '\\']).to_string(),
                None => raw,
            };
            ("←".into(), format!("Access external directory {}", fmt(&dir)), resources.iter().map(|r| format!("- {r}")).collect(), None)
        }
        "doom_loop" => ("⟳".into(), "Continue after repeated failures".into(), vec!["This keeps the session running despite repeated failures.".into()], None),
        _ => ("⚙".into(), format!("Call tool {action}"), vec![format!("Tool: {action}")], None),
    }
}

pub fn always_lines(action: &str, save: &[String]) -> Vec<String> {
    if save.len() == 1 && save[0] == "*" {
        return vec![format!("This will always allow {action} for this project.")];
    }
    let mut v = vec!["This will always allow the following patterns for this project.".to_string()];
    v.extend(save.iter().map(|s| format!("- {s}")));
    v
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn validation_messages() {
        let f = json!({"key":"a","type":"string","required":true,"minLength":3});
        assert_eq!(validate(&f, None).as_deref(), Some("Answer required"));
        assert_eq!(validate(&f, Some(&json!("ab"))).as_deref(), Some("Must be at least 3 characters"));
        let f = json!({"key":"a","type":"integer","minimum":1,"maximum":5});
        assert_eq!(validate(&f, Some(&json!(1.5))).as_deref(), Some("Expected an integer"));
        assert_eq!(validate(&f, Some(&json!(9))).as_deref(), Some("Must be at most 5"));
        assert_eq!(placeholder(&f), "1-5");
        let f = json!({"key":"a","type":"multiselect","options":[{"value":"x","label":"X"}],"required":true});
        assert_eq!(validate(&f, Some(&json!([]))).as_deref(), Some("Select at least one option"));
        assert_eq!(display(&f, Some(&json!(["x"])), "(none)"), "X");
        let f = json!({"key":"e","type":"string","format":"email"});
        assert_eq!(validate(&f, Some(&json!("nope"))).as_deref(), Some("Expected an email address"));
    }

    #[test]
    fn when_conditions() {
        let fields = vec![json!({"key":"a","type":"boolean"}), json!({"key":"b","type":"string","when":[{"key":"a","op":"eq","value":true}]})];
        let mut ans = Map::new();
        assert_eq!(visible(&fields, &ans).len(), 1);
        ans.insert("a".into(), json!(true));
        assert_eq!(visible(&fields, &ans).len(), 2);
    }

    #[test]
    fn permission_text() {
        let (icon, title, lines, _) = permission_presentation("bash", &["ls".into()], Some(&json!({"command":"ls -la"})), &|s| s.to_string());
        assert_eq!((icon.as_str(), title.as_str()), ("#", "Shell command"));
        assert_eq!(lines, vec!["$ ls -la"]);
        assert_eq!(always_lines("edit", &["*".into()]), vec!["This will always allow edit for this project."]);
    }
}

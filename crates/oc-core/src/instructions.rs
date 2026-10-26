//! Instruction sources (`core/instructions/index.ts`, `builtins.ts`, `instruction-discovery.ts`,
//! `skill/instructions.ts`, `session/instruction-entry.ts`).

use std::cmp::Ordering;

use serde_json::{Map, Value};
use sha2::Digest;

pub const REMOVED: &str = "removed";

pub enum Read {
    Value(Value),
    Unavailable,
    Removed,
}

type R1 = Box<dyn Fn(&Value) -> Option<String> + Send + Sync>;
type R2 = Box<dyn Fn(&Value, &Value) -> Option<String> + Send + Sync>;

pub struct Source {
    pub key: String,
    pub read: Read,
    pub initial: R1,
    pub changed: R2,
    pub removed: R1,
}

impl Source {
    pub fn new(
        key: impl Into<String>,
        read: Read,
        initial: impl Fn(&Value) -> Option<String> + Send + Sync + 'static,
        changed: impl Fn(&Value, &Value) -> Option<String> + Send + Sync + 'static,
        removed: Option<String>,
    ) -> Source {
        Source {
            key: key.into(),
            read,
            initial: Box::new(initial),
            changed: Box::new(changed),
            removed: Box::new(move |_| removed.clone()),
        }
    }
}

/// JavaScript `a.localeCompare(b)` (see [`oc_schema::collate`]).
pub fn locale_compare(a: &str, b: &str) -> Ordering {
    oc_schema::collate::locale_compare(a, b)
}

fn utf16_cmp(a: &str, b: &str) -> Ordering {
    a.encode_utf16().cmp(b.encode_utf16())
}

/// JS `JSON.stringify` for a number.
pub fn js_number(n: &serde_json::Number) -> String {
    if let Some(i) = n.as_i64() {
        return i.to_string();
    }
    if let Some(u) = n.as_u64() {
        return u.to_string();
    }
    let f = n.as_f64().unwrap_or(0.0);
    if !f.is_finite() {
        return "null".into();
    }
    if f == f.trunc() && f.abs() < 1e21 {
        return format!("{}", f as i128);
    }
    let s = format!("{f}");
    // Rust prints e.g. "1e-7" as "0.0000001"; JS uses exponent form below 1e-6.
    if f.abs() < 1e-6 || f.abs() >= 1e21 {
        let e = format!("{f:e}");
        return if e.contains("e-") { e } else { e.replace('e', "e+") };
    }
    s
}

/// Canonical JSON (keys sorted by UTF-16 code units).
pub fn canonical(v: &Value) -> String {
    match v {
        Value::Array(a) => format!("[{}]", a.iter().map(canonical).collect::<Vec<_>>().join(",")),
        Value::Object(o) => {
            let mut entries: Vec<(&String, &Value)> = o.iter().collect();
            entries.sort_by(|a, b| utf16_cmp(a.0, b.0));
            format!(
                "{{{}}}",
                entries
                    .iter()
                    .map(|(k, v)| format!("{}:{}", serde_json::to_string(k).unwrap(), canonical(v)))
                    .collect::<Vec<_>>()
                    .join(",")
            )
        }
        Value::Number(n) => js_number(n),
        other => serde_json::to_string(other).unwrap(),
    }
}

pub fn hash(v: &Value) -> String {
    hex::encode(sha2::Sha256::digest(canonical(v).as_bytes()))
}

pub struct Admission {
    pub delta: Map<String, Value>,
    pub blobs: Vec<(String, Value)>,
}

pub fn diff(sources: &[Source], previous: Option<&Map<String, Value>>) -> Result<Admission, String> {
    if previous.is_none() {
        let blocked: Vec<&str> =
            sources.iter().filter(|s| matches!(s.read, Read::Unavailable)).map(|s| s.key.as_str()).collect();
        if !blocked.is_empty() {
            return Err(format!("Instruction initialization blocked by unavailable sources: {}", blocked.join(", ")));
        }
    }
    let mut delta = Map::new();
    let mut blobs = Vec::new();
    for s in sources {
        match &s.read {
            Read::Unavailable => {}
            Read::Removed => {
                if previous.is_some_and(|p| p.contains_key(&s.key)) {
                    delta.insert(s.key.clone(), Value::String(REMOVED.into()));
                }
            }
            Read::Value(v) => {
                let h = hash(v);
                if previous.and_then(|p| p.get(&s.key)).and_then(Value::as_str) == Some(h.as_str()) {
                    continue;
                }
                delta.insert(s.key.clone(), Value::String(h.clone()));
                blobs.push((h, v.clone()));
            }
        }
    }
    Ok(Admission { delta, blobs })
}

pub fn render_initial(sources: &[Source], values: &Map<String, Value>) -> String {
    sources
        .iter()
        .filter_map(|s| values.get(&s.key).and_then(|v| (s.initial)(v)))
        .collect::<Vec<_>>()
        .join("\n\n")
}

/// `delta` maps key → `Some(current)` or `None` (removed).
pub fn render_update(sources: &[Source], previous: &Map<String, Value>, delta: &[(String, Option<Value>)]) -> String {
    sources
        .iter()
        .filter_map(|s| {
            let (_, cur) = delta.iter().find(|(k, _)| k == &s.key)?;
            match cur {
                None => previous.get(&s.key).and_then(|p| (s.removed)(p)),
                Some(c) => match previous.get(&s.key) {
                    Some(p) => (s.changed)(p, c),
                    None => (s.initial)(c),
                },
            }
        })
        .collect::<Vec<_>>()
        .join("\n\n")
}

/// `diffByKey`
pub fn diff_by_key<'a, A>(
    previous: &'a [A],
    current: &'a [A],
    key: impl Fn(&A) -> String,
    changed: impl Fn(&A, &A) -> bool,
) -> (Vec<&'a A>, Vec<&'a A>, Vec<(&'a A, &'a A)>) {
    let added = current.iter().filter(|c| !previous.iter().any(|p| key(p) == key(c))).collect();
    let removed = previous.iter().filter(|p| !current.iter().any(|c| key(c) == key(p))).collect();
    let ch = current
        .iter()
        .filter_map(|c| previous.iter().find(|p| key(p) == key(c)).filter(|p| changed(p, c)).map(|p| (p, c)))
        .collect();
    (added, removed, ch)
}

// ---------- built-in sources ----------

pub fn js_date_string(now: chrono::DateTime<chrono::Local>) -> String {
    now.format("%a %b %d %Y").to_string()
}

pub fn date_source() -> Source {
    Source::new(
        "core/date",
        Read::Value(Value::String(js_date_string(chrono::Local::now()))),
        |v| Some(format!("Today's date: {}", v.as_str()?)),
        |_, v| Some(format!("Today's date is now: {}", v.as_str()?)),
        None,
    )
}

pub fn js_platform() -> &'static str {
    match std::env::consts::OS {
        "macos" => "darwin",
        "windows" => "win32",
        other => other,
    }
}

pub fn environment_source(directory: &str, project_directory: &str, is_git: bool, tmp: &str) -> Source {
    let env = [
        "<env>".to_string(),
        format!("  Working directory: {directory}"),
        format!("  Workspace root folder: {project_directory}"),
        format!("  Is directory a git repo: {}", if is_git { "yes" } else { "no" }),
        format!("  Platform: {}", js_platform()),
        format!("  Prefer {tmp} over generic system temporary directories such as /tmp; it is pre-created and approved for external access."),
        "</env>".to_string(),
    ]
    .join("\n");
    Source::new(
        "core/environment",
        Read::Value(Value::String(env)),
        |v| Some(format!("Here is some useful information about the environment you are running in:\n{}", v.as_str()?)),
        |_, v| Some(format!("The environment you are running in is now:\n{}", v.as_str()?)),
        None,
    )
}

#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct File {
    pub path: String,
    pub content: String,
}

fn render_files(files: &[File]) -> String {
    files.iter().map(|f| format!("Instructions from: {}\n{}", f.path, f.content)).collect::<Vec<_>>().join("\n\n")
}

/// jsdiff `createPatch(...)` body from the first `@@`, trailing whitespace trimmed.
pub fn unified_hunks(before: &str, after: &str) -> String {
    let diff = similar::TextDiff::from_lines(before, after);
    let mut out = String::new();
    for hunk in diff.unified_diff().context_radius(3).iter_hunks() {
        out.push_str(&hunk.to_string());
    }
    out.trim_end().to_string()
}

/// AGENTS.md discovery source; `files = None` → unavailable.
pub fn discovery_source(files: Option<Vec<File>>) -> Source {
    let read = match files {
        None => Read::Unavailable,
        Some(f) if f.is_empty() => Read::Removed,
        Some(f) => Read::Value(serde_json::to_value(f).unwrap()),
    };
    let decode = |v: &Value| serde_json::from_value::<Vec<File>>(v.clone()).ok();
    Source {
        key: "core/instructions".into(),
        read,
        initial: Box::new(move |v| decode(v).map(|f| render_files(&f))),
        changed: Box::new(move |p, c| {
            let after = decode(c)?;
            let Some(before) = decode(p) else { return Some(render_files(&after)) };
            let (added, removed, changed) =
                diff_by_key(&before, &after, |f| f.path.clone(), |a, b| a.content != b.content);
            let mut parts: Vec<String> = removed.iter().map(|f| format!("The instructions from {} no longer apply.", f.path)).collect();
            parts.extend(added.iter().map(|f| format!("New instructions apply from:\n{}", render_files(&[(*f).clone()]))));
            parts.extend(changed.iter().map(|(b, a)| {
                let diff = ["The instructions from ".to_string() + &a.path + " changed. Here's the diff:", "```diff".into(), unified_hunks(&b.content, &a.content), "```".into()].join("\n");
                let replacement = format!("The instructions changed:\n{}", render_files(&[(*a).clone()]));
                if diff.encode_utf16().count() < replacement.encode_utf16().count() { diff } else { replacement }
            }));
            Some(parts.join("\n\n"))
        }),
        removed: Box::new(|_| Some("Previously loaded instructions no longer apply.".into())),
    }
}

#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct SkillSummary {
    pub id: String,
    pub name: String,
    pub description: String,
}

fn skill_entries(skills: &[&SkillSummary]) -> Vec<String> {
    skills
        .iter()
        .flat_map(|s| {
            [
                "  <skill>".to_string(),
                format!("    <id>{}</id>", s.id),
                format!("    <name>{}</name>", s.name),
                format!("    <description>{}</description>", s.description),
                "  </skill>".to_string(),
            ]
        })
        .collect()
}

fn render_skills(skills: &[SkillSummary]) -> String {
    let mut lines = vec![
        "Skills provide specialized instructions and workflows for specific tasks.".to_string(),
        "Use the skill tool to load a skill when a task matches its description.".to_string(),
        "The user may also invoke a skill directly. When that happens, its instructions appear in the conversation as a <skill_content> block, the same shape the skill tool returns. A skill that is already present this way does not need to be invoked again.".to_string(),
    ];
    if skills.is_empty() {
        lines.push("No skills are currently available.".into());
    } else {
        lines.push("<available_skills>".into());
        lines.extend(skill_entries(&skills.iter().collect::<Vec<_>>()));
        lines.push("</available_skills>".into());
    }
    lines.join("\n")
}

/// `skills` must already be filtered (not denied, has description, autoinvoke != false).
pub fn skill_source(mut skills: Vec<SkillSummary>) -> Source {
    skills.sort_by(|a, b| locale_compare(&a.id, &b.id));
    let read = if skills.is_empty() { Read::Removed } else { Read::Value(serde_json::to_value(&skills).unwrap()) };
    let decode = |v: &Value| serde_json::from_value::<Vec<SkillSummary>>(v.clone()).ok();
    Source {
        key: "core/skill-guidance".into(),
        read,
        initial: Box::new(move |v| decode(v).map(|s| render_skills(&s))),
        changed: Box::new(move |p, c| {
            let current = decode(c)?;
            let Some(previous) = decode(p) else { return Some(render_skills(&current)) };
            let (added, removed, changed) = diff_by_key(
                &previous,
                &current,
                |s| s.id.clone(),
                |a, b| a.name != b.name || a.description != b.description,
            );
            // Added, changed, and removed skills render as deltas; the full list is restated only when no entry differs.
            if added.is_empty() && changed.is_empty() && removed.is_empty() {
                return Some(format!(
                    "The available skills have changed. This list supersedes the previous available skills list.\n{}",
                    render_skills(&current)
                ));
            }
            let mut lines = Vec::new();
            if !added.is_empty() {
                lines.push("New skills are available in addition to those previously listed:".to_string());
                lines.extend(skill_entries(&added));
            }
            if !changed.is_empty() {
                lines.push(
                    "The following skills have changed. Each entry replaces the previously listed skill with the same ID:"
                        .to_string(),
                );
                lines.extend(skill_entries(&changed.iter().map(|(_, c)| *c).collect::<Vec<_>>()));
            }
            if !removed.is_empty() {
                lines.push(format!(
                    "The following skill IDs are no longer available and must not be used: {}.",
                    removed.iter().map(|s| s.id.as_str()).collect::<Vec<_>>().join(", ")
                ));
            }
            Some(lines.join("\n"))
        }),
        removed: Box::new(|_| Some("Skill guidance is no longer available. Do not use any previously listed skill.".into())),
    }
}

fn js_stringify_pretty(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        other => serde_json::to_string_pretty(other).unwrap(),
    }
}

/// Session context entry source `api/{key}`; `value = None` is a tombstone.
pub fn entry_source(key: &str, value: Option<Value>) -> Source {
    let k = key.to_string();
    let (k1, k2, k3) = (k.clone(), k.clone(), k.clone());
    Source {
        key: format!("api/{key}"),
        read: match value {
            Some(v) => Read::Value(v),
            None => Read::Removed,
        },
        initial: Box::new(move |v| Some(format!("<context key=\"{k1}\">\n{}\n</context>", js_stringify_pretty(v)))),
        changed: Box::new(move |_, v| {
            Some(format!(
                "The context under \"{k2}\" changed and supersedes the previous value:\n<context key=\"{k2}\">\n{}\n</context>",
                js_stringify_pretty(v)
            ))
        }),
        removed: Box::new(move |_| Some(format!("The context under \"{k3}\" no longer applies. Disregard it."))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn canonical_sorted() {
        let v = serde_json::json!({"b": 1, "a": [true, null, "x"], "B": 2.5});
        assert_eq!(canonical(&v), r#"{"B":2.5,"a":[true,null,"x"],"b":1}"#);
    }
    // skill/instructions.test.ts "renders only the changed skill when a description changes"
    #[test]
    fn renders_only_the_changed_skill_when_a_description_changes() {
        let skill = |description: &str| SkillSummary { id: "effect".into(), name: "Effect".into(), description: description.into() };
        let value = |d: &str| serde_json::to_value(vec![skill(d)]).unwrap();
        let source = skill_source(vec![skill("Build applications with Effect")]);
        assert_eq!(
            (source.changed)(&value("Build applications with Effect"), &value("Build applications with Effect v4")).unwrap(),
            [
                "The following skills have changed. Each entry replaces the previously listed skill with the same ID:",
                "  <skill>",
                "    <id>effect</id>",
                "    <name>Effect</name>",
                "    <description>Build applications with Effect v4</description>",
                "  </skill>",
            ]
            .join("\n")
        );
    }
    #[test]
    fn date_format() {
        use chrono::TimeZone;
        let d = chrono::Local.with_ymd_and_hms(2026, 10, 5, 12, 0, 0).unwrap();
        assert_eq!(js_date_string(d), "Mon Oct 05 2026");
    }
}

/// `core/mcp-guidance` (`core/mcp/instructions.ts`).
pub fn mcp_source(mut servers: Vec<oc_mcp::naming::InstructionSummary>) -> Source {
    servers.sort_by(|a, b| locale_compare(&a.server, &b.server));
    #[derive(serde::Serialize, serde::Deserialize)]
    struct S {
        server: String,
        instructions: String,
        codemode: bool,
    }
    let to = |v: &Value| -> Option<Vec<oc_mcp::naming::InstructionSummary>> {
        let items: Vec<S> = serde_json::from_value(v.clone()).ok()?;
        Some(items.into_iter().map(|s| oc_mcp::naming::InstructionSummary { server: s.server, instructions: s.instructions, codemode: s.codemode }).collect())
    };
    let read = if servers.is_empty() {
        Read::Removed
    } else {
        Read::Value(serde_json::to_value(servers.iter().map(|s| S { server: s.server.clone(), instructions: s.instructions.clone(), codemode: s.codemode }).collect::<Vec<_>>()).unwrap())
    };
    Source {
        key: "core/mcp-guidance".into(),
        read,
        initial: Box::new(move |v| to(v).map(|s| oc_mcp::naming::render_instructions(&s))),
        changed: Box::new(move |p, c| {
            let cur = to(c)?;
            Some(match to(p) {
                Some(prev) => oc_mcp::naming::render_instructions_update(&prev, &cur),
                None => oc_mcp::naming::render_instructions(&cur),
            })
        }),
        removed: Box::new(|_| Some("MCP server instructions are no longer available.".into())),
    }
}

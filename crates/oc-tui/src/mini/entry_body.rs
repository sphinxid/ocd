//! Scrollback entry bodies and flags (`src/mini/entry.body.ts`).

use super::mono::{mono_prefix, mono_tool_text};
use super::tool::{EntryBody, ToolSnapshot, tool_entry_body};
use super::types::*;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct EntryFlags {
    pub start_on_new_line: bool,
    pub trailing_newline: bool,
}

#[derive(Clone, Copy, Debug, Default)]
pub struct BodyOptions {
    pub shell_output: Option<bool>,
    pub mono: bool,
}

fn clean_run_text(text: &str) -> String {
    text.replace("\r\n", "\n").replace('\r', "\n")
}

fn text_body(content: String) -> EntryBody {
    if content.is_empty() { EntryBody::None } else { EntryBody::Text(content) }
}

fn code_body(content: String, filetype: &str) -> EntryBody {
    if content.is_empty() { EntryBody::None } else { EntryBody::Code { content, filetype: Some(filetype.into()) } }
}

fn markdown_body(content: String) -> EntryBody {
    if content.is_empty() { EntryBody::None } else { EntryBody::Markdown(content) }
}

fn lead(raw: &str) -> (&str, &str) {
    let n = raw.len() - raw.trim_start_matches('\n').len();
    raw.split_at(n)
}

fn user_body(raw: &str, mono: bool) -> EntryBody {
    if raw.trim().is_empty() {
        return EntryBody::None;
    }
    let (lead, body) = lead(raw);
    text_body(format!("{lead}{} {body}", if mono { ">" } else { "›" }))
}

fn reasoning_body(raw: &str, mono: bool) -> EntryBody {
    let clean = raw.replace("[REDACTED]", "");
    if clean.is_empty() {
        return EntryBody::None;
    }
    let (lead, body) = lead(&clean);
    let mark = "Thinking:";
    if let Some(rest) = body.strip_prefix(mark) {
        if mono {
            return text_body(format!("{lead}{mark} {}", rest.trim_start()));
        }
        return code_body(format!("{lead}_Thinking:_ {}", rest.trim_start()), "markdown");
    }
    if mono { text_body(clean.clone()) } else { code_body(clean.clone(), "markdown") }
}

fn system_body(raw: &str, phase: Phase) -> EntryBody {
    text_body(if phase == Phase::Progress { raw.to_string() } else { raw.trim().to_string() })
}

fn mono_body(body: EntryBody) -> EntryBody {
    match body {
        EntryBody::None | EntryBody::Text(_) | EntryBody::Markdown(_) => body,
        EntryBody::Code { content, .. } => text_body(content),
        EntryBody::Structured(snapshot) => match snapshot {
            // Todo cards are not flattened: the writer draws them with ASCII markers in mono.
            ToolSnapshot::Todo { .. } => EntryBody::Structured(snapshot),
            ToolSnapshot::Code { title, content, .. } => text_body(format!("{title}\n{content}")),
            ToolSnapshot::Diff { items } => text_body(
                items
                    .iter()
                    .map(|i| {
                        let d = i.diff.trim();
                        format!("{}\n{}", i.title, if d.is_empty() { format!("-{} lines", i.deletions.unwrap_or(0)) } else { d.to_string() })
                    })
                    .collect::<Vec<_>>()
                    .join("\n\n"),
            ),
            ToolSnapshot::Task { title, rows, tail } => {
                let mut v = vec![title];
                v.extend(rows);
                v.push(tail);
                text_body(v.into_iter().filter(|s| !s.is_empty()).collect::<Vec<_>>().join("\n"))
            }
            ToolSnapshot::Question { items, tail } => {
                let mut v = vec!["# Questions".to_string()];
                for (q, a) in items {
                    v.push(q);
                    v.push(a);
                }
                v.push(tail);
                text_body(v.into_iter().filter(|s| !s.is_empty()).collect::<Vec<_>>().join("\n"))
            }
        },
    }
}

/// `entryFlags(commit)`.
pub fn entry_flags(commit: &StreamCommit) -> EntryFlags {
    let f = |s, t| EntryFlags { start_on_new_line: s, trailing_newline: t };
    if commit.summary.is_some() {
        return f(true, false);
    }
    match commit.kind {
        EntryKind::User => f(true, false),
        EntryKind::Tool | EntryKind::Assistant | EntryKind::Reasoning => {
            if commit.phase == Phase::Progress {
                f(false, false)
            } else {
                f(true, true)
            }
        }
        EntryKind::Error => f(true, false),
        EntryKind::System => f(true, true),
    }
}

/// `entryDone(commit)`.
pub fn entry_done(commit: &StreamCommit) -> bool {
    match commit.kind {
        EntryKind::Assistant | EntryKind::Reasoning => commit.phase == Phase::Final,
        EntryKind::Tool => commit.phase == Phase::Final || (commit.phase == Phase::Progress && commit.tool_state == Some(ToolStateKind::Completed)),
        _ => true,
    }
}

/// `entryCanStream(commit, body)`.
pub fn entry_can_stream(commit: &StreamCommit, body: &EntryBody) -> bool {
    if commit.phase != Phase::Progress || matches!(body, EntryBody::None) {
        return false;
    }
    if commit.kind == EntryKind::Tool {
        return commit.tool_state != Some(ToolStateKind::Completed);
    }
    matches!(commit.kind, EntryKind::Assistant | EntryKind::Reasoning)
}

/// `entryBody(commit, options)`.
pub fn entry_body(commit: &StreamCommit, options: BodyOptions) -> EntryBody {
    if commit.summary.is_some() {
        return EntryBody::None;
    }
    let raw = clean_run_text(&commit.text);
    let mono = options.mono;
    if commit.image.is_some() {
        let caption = if raw.trim().is_empty() { "Image".to_string() } else { raw.trim().to_string() };
        return if commit.kind == EntryKind::User { user_body(&caption, mono) } else { text_body(mono_tool_text(&caption, mono)) };
    }
    match commit.kind {
        EntryKind::User => user_body(&raw, mono),
        EntryKind::Tool => {
            let body = tool_entry_body(commit, &raw, options.shell_output).unwrap_or(EntryBody::None);
            let result = if mono { mono_body(body.clone()) } else { body.clone() };
            let EntryBody::Text(content) = &result else { return result };
            if !mono || !matches!(body, EntryBody::Text(_)) || commit.phase == Phase::Progress {
                return result;
            }
            text_body(mono_tool_text(content, true))
        }
        EntryKind::Assistant => match commit.phase {
            Phase::Start => EntryBody::None,
            Phase::Final => {
                if commit.interrupted {
                    text_body("assistant interrupted".into())
                } else {
                    EntryBody::None
                }
            }
            Phase::Progress => markdown_body(raw),
        },
        EntryKind::Reasoning => match commit.phase {
            Phase::Start => EntryBody::None,
            Phase::Final => {
                if commit.interrupted {
                    text_body("reasoning interrupted".into())
                } else {
                    EntryBody::None
                }
            }
            Phase::Progress => reasoning_body(&raw, mono),
        },
        _ => {
            let body = system_body(&raw, commit.phase);
            match body {
                EntryBody::Text(c) if mono => text_body(mono_prefix(&c, true)),
                b => b,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mini::tool::DiffItem;
    use serde_json::{Value, json};

    fn part(name: &str, state: Value, id: Option<&str>) -> ToolPart {
        let status = state.get("status").and_then(|s| s.as_str()).unwrap_or("").to_string();
        let time = match status.as_str() {
            "streaming" => json!({"created": 1}),
            "completed" | "error" => json!({"created": 1, "ran": 1, "completed": 2}),
            _ => json!({"created": 1, "ran": 1}),
        };
        ToolPart::from_value(&json!({"type": "tool", "id": id.map(|s| s.to_string()).unwrap_or(format!("{name}-1")), "name": name, "state": state, "time": time}))
    }

    struct TC<'a> {
        tool: &'a str,
        state: Value,
        directory: Option<&'a str>,
        phase: Option<Phase>,
        tool_state: Option<ToolStateKind>,
        text: &'a str,
    }

    fn tool_commit(tc: TC) -> StreamCommit {
        let status = tc.state.get("status").and_then(|s| s.as_str()).unwrap_or("");
        StreamCommit {
            kind: EntryKind::Tool,
            text: tc.text.into(),
            phase: tc.phase.unwrap_or(Phase::Final),
            source: Source::Tool,
            tool: Some(tc.tool.into()),
            directory: tc.directory.map(|s| s.into()),
            tool_state: tc.tool_state.or(Some(match status {
                "error" => ToolStateKind::Error,
                "completed" => ToolStateKind::Completed,
                _ => ToolStateKind::Running,
            })),
            part: Some(part(tc.tool, tc.state, None)),
            ..Default::default()
        }
    }

    fn tc<'a>(tool: &'a str, state: Value) -> TC<'a> {
        TC { tool, state, directory: None, phase: None, tool_state: None, text: "" }
    }

    fn structured(c: &StreamCommit) -> ToolSnapshot {
        match entry_body(c, BodyOptions::default()) {
            EntryBody::Structured(s) => s,
            other => panic!("expected structured body, got {other:?}"),
        }
    }

    fn text(s: &str) -> EntryBody {
        EntryBody::Text(s.into())
    }

    #[test]
    fn failed_direct_shell_is_an_error() {
        let failed = StreamCommit {
            kind: EntryKind::Tool,
            text: "Shell exited with code 7".into(),
            phase: Phase::Final,
            source: Source::Tool,
            tool: Some("shell".into()),
            tool_state: Some(ToolStateKind::Error),
            tool_error: Some("Shell → exited · code 7".into()),
            shell: Some("false".into()),
            ..Default::default()
        };
        assert_eq!(entry_body(&failed, BodyOptions::default()), text("✖ shell failed: Shell → exited · code 7"));
        assert_eq!(entry_body(&failed, BodyOptions { mono: true, ..Default::default() }), text("! shell failed: Shell → exited · code 7"));
    }

    #[test]
    fn display_formats() {
        let a = StreamCommit { part_id: Some("part-1".into()), ..StreamCommit::new(EntryKind::Assistant, "# Title\n\nHello **world**", Phase::Progress, Source::Assistant) };
        assert_eq!(entry_body(&a, BodyOptions::default()), EntryBody::Markdown("# Title\n\nHello **world**".into()));
        let r = StreamCommit { part_id: Some("reason-1".into()), ..StreamCommit::new(EntryKind::Reasoning, "Thinking: plan next steps", Phase::Progress, Source::Reasoning) };
        let reasoning = entry_body(&r, BodyOptions::default());
        assert_eq!(reasoning, EntryBody::Code { content: "_Thinking:_ plan next steps".into(), filetype: Some("markdown".into()) });
        assert!(entry_can_stream(&StreamCommit::new(EntryKind::Reasoning, "Thinking: plan next steps", Phase::Progress, Source::Reasoning), &reasoning));
        let u = StreamCommit::new(EntryKind::User, "Inspect footer tabs", Phase::Start, Source::System);
        assert_eq!(entry_body(&u, BodyOptions::default()), text("› Inspect footer tabs"));
        assert_eq!(entry_body(&u, BodyOptions { mono: true, ..Default::default() }), text("> Inspect footer tabs"));
    }

    #[test]
    fn structured_finals() {
        let w = tool_commit(tc("write", json!({"status": "completed", "input": {"path": "src/a.ts", "content": "const x = 1\n"}, "metadata": {}, "content": [{"type": "text", "text": ""}]})));
        assert_eq!(structured(&w), ToolSnapshot::Code { title: "# Wrote src/a.ts".into(), content: "const x = 1\n".into(), file: Some("src/a.ts".into()) });
        let e = tool_commit(tc("edit", json!({"status": "completed", "input": {"path": "src/a.ts"}, "metadata": {"files": [{"file": "src/a.ts", "status": "modified", "patch": "@@ -1 +1 @@\n-old\n+new\n"}]}, "content": [{"type": "text", "text": ""}]})));
        assert_eq!(
            structured(&e),
            ToolSnapshot::Diff { items: vec![DiffItem { title: "# Edited src/a.ts".into(), diff: "@@ -1 +1 @@\n-old\n+new\n".into(), file: Some("src/a.ts".into()), deletions: None }] }
        );
        let p = tool_commit(tc("patch", json!({"status": "completed", "input": {}, "content": [{"type": "text", "text": ""}], "metadata": {"files": [{"status": "modified", "file": "src/a.ts", "patch": "@@ -1 +1 @@\n-old\n+new\n"}]}})));
        assert_eq!(
            structured(&p),
            ToolSnapshot::Diff { items: vec![DiffItem { title: "# Patched src/a.ts".into(), diff: "@@ -1 +1 @@\n-old\n+new\n".into(), file: Some("src/a.ts".into()), deletions: Some(0) }] }
        );
    }

    #[test]
    fn running_subagent_stays_out() {
        let c = tool_commit(TC {
            phase: Some(Phase::Start),
            tool_state: Some(ToolStateKind::Running),
            text: "running inspect reducer",
            ..tc("subagent", json!({"status": "running", "input": {"description": "Inspect reducer", "agent": "explore"}, "metadata": {"sessionID": "ses-child-1", "status": "running"}}))
        });
        assert_eq!(entry_body(&c, BodyOptions::default()), EntryBody::None);
    }

    #[test]
    fn subagent_results() {
        let result = tool_commit(tc(
            "subagent",
            json!({"status": "completed", "input": {"description": "Inspect reducer", "agent": "explore"}, "content": [{"type": "text", "text": "# Findings\n\n- Footer stays live"}], "metadata": {"sessionID": "ses-child-1", "status": "completed", "output": "# Findings\n\n- Footer stays live"}}),
        ));
        let md = EntryBody::Markdown("# Findings\n\n- Footer stays live".into());
        assert_eq!(entry_body(&result, BodyOptions::default()), md);
        assert_eq!(entry_body(&result, BodyOptions { mono: true, ..Default::default() }), md);
        let empty = tool_commit(tc(
            "subagent",
            json!({"status": "completed", "input": {"description": "Inspect reducer", "agent": "explore"}, "content": [{"type": "text", "text": ""}], "metadata": {"sessionID": "ses-child-1", "status": "completed", "output": ""}}),
        ));
        assert_eq!(structured(&empty), ToolSnapshot::Task { title: "# Explore Subagent".into(), rows: vec!["Inspect reducer".into()], tail: String::new() });
    }

    #[test]
    fn tool_progress_streams() {
        let c = StreamCommit { tool: Some("shell".into()), part_id: Some("tool-2".into()), ..StreamCommit::new(EntryKind::Tool, "partial output", Phase::Progress, Source::Tool) };
        let body = entry_body(&c, BodyOptions::default());
        assert_eq!(body, text("partial output"));
        assert!(entry_can_stream(&StreamCommit { part_id: None, ..c.clone() }, &body));
        let done = StreamCommit { tool: Some("shell".into()), tool_state: Some(ToolStateKind::Completed), ..StreamCommit::new(EntryKind::Tool, "output", Phase::Progress, Source::Tool) };
        assert!(entry_done(&done));
    }

    #[test]
    fn completed_shell_output() {
        let output = ["/tmp/demo", "git status", "On branch demo", "nothing to commit, working tree clean", ""].join("\n");
        let c = tool_commit(TC {
            phase: Some(Phase::Progress),
            tool_state: Some(ToolStateKind::Completed),
            text: &output,
            ..tc("shell", json!({"status": "completed", "input": {"command": "git status", "workdir": "/tmp/demo"}, "content": [{"type": "text", "text": output}], "metadata": {"exit": 0, "truncated": false}}))
        });
        assert_eq!(entry_body(&c, BodyOptions::default()), text("\nOn branch demo\nnothing to commit, working tree clean"));
    }

    #[test]
    fn shell_starts() {
        let c = tool_commit(TC {
            phase: Some(Phase::Start),
            tool_state: Some(ToolStateKind::Running),
            text: "running shell",
            ..tc("shell", json!({"status": "running", "input": {"command": "ls"}, "metadata": {}}))
        });
        assert_eq!(entry_body(&c, BodyOptions::default()), text("$ ls"));
        let c = tool_commit(TC {
            directory: Some("/work/project"),
            phase: Some(Phase::Start),
            tool_state: Some(ToolStateKind::Running),
            ..tc("shell", json!({"status": "running", "input": {"command": "ls", "workdir": "packages/foo"}, "metadata": {}}))
        });
        assert_eq!(entry_body(&c, BodyOptions::default()), text("packages/foo$ ls"));
        let home = crate::mini::util::home_dir();
        let dir = format!("{home}/project");
        let wd = format!("{home}/outside/folder");
        let c = tool_commit(TC {
            directory: Some(&dir),
            phase: Some(Phase::Start),
            tool_state: Some(ToolStateKind::Running),
            ..tc("shell", json!({"status": "running", "input": {"command": "pwd", "workdir": wd}, "metadata": {}}))
        });
        assert_eq!(entry_body(&c, BodyOptions::default()), text("~/outside/folder$ pwd"));
    }

    #[test]
    fn direct_shell_commits() {
        let start = StreamCommit {
            tool: Some("shell".into()),
            part_id: Some("shell:call-1".into()),
            tool_state: Some(ToolStateKind::Running),
            shell: Some("pwd".into()),
            ..StreamCommit::new(EntryKind::Tool, "running shell", Phase::Start, Source::Tool)
        };
        assert_eq!(entry_body(&start, BodyOptions::default()), text("$ pwd"));
        let out = StreamCommit { tool_state: Some(ToolStateKind::Completed), ..StreamCommit { text: "/tmp/demo\n".into(), phase: Phase::Progress, ..start.clone() } };
        assert_eq!(entry_body(&out, BodyOptions::default()), text("\n/tmp/demo"));
    }

    #[test]
    fn hides_shell_tool_output_only() {
        let output = StreamCommit { tool: Some("shell".into()), ..StreamCommit::new(EntryKind::Tool, "output", Phase::Progress, Source::Tool) };
        assert_eq!(entry_body(&output, BodyOptions { shell_output: Some(false), ..Default::default() }), EntryBody::None);
        let direct = StreamCommit { shell: Some("pwd".into()), ..output };
        assert_eq!(entry_body(&direct, BodyOptions { shell_output: Some(false), ..Default::default() }), text("\noutput"));
    }

    #[test]
    fn patch_summaries() {
        let c = tool_commit(tc("patch", json!({"status": "completed", "input": {"patchText": "*** Begin Patch\n*** End Patch"}, "content": [{"type": "text", "text": ""}], "metadata": {"files": [{"status": "modified", "file": "src/a.ts"}]}})));
        assert_eq!(entry_body(&c, BodyOptions::default()), text("~ Patched src/a.ts"));
        let c = tool_commit(tc(
            "patch",
            json!({"status": "completed", "input": {"patchText": "*** Begin Patch\n*** End Patch"}, "content": [{"type": "text", "text": ""}], "metadata": {"files": [{"status": "modified", "file": "src/a.ts"}, {"status": "added", "file": "README-demo.md"}]}}),
        ));
        assert_eq!(entry_body(&c, BodyOptions::default()), text("+ Created README-demo.md"));
    }

    #[test]
    fn glob_failure_raw_error() {
        let c = tool_commit(TC {
            phase: Some(Phase::Final),
            tool_state: Some(ToolStateKind::Error),
            ..tc("glob", json!({"status": "error", "input": {"pattern": "**/*tool*", "path": "/tmp/demo/run"}, "error": {"type": "unknown", "message": "No such file or directory: '/tmp/demo/run'"}, "metadata": {}}))
        });
        assert_eq!(entry_body(&c, BodyOptions::default()), text("No such file or directory: '/tmp/demo/run'"));
    }

    #[test]
    fn unknown_tool_structured_fallback() {
        let values: Vec<Value> = (0..40).map(|i| json!({"index": i})).collect();
        let c = tool_commit(TC {
            phase: Some(Phase::Final),
            tool_state: Some(ToolStateKind::Completed),
            ..tc("mcp_custom", json!({"status": "completed", "input": {"target": "demo"}, "metadata": {"result": {"ok": true, "nested": {"values": values}}, "large": "x".repeat(8000)}, "content": [{"type": "text", "text": ""}]}))
        });
        let EntryBody::Code { content, filetype } = entry_body(&c, BodyOptions::default()) else { panic!("expected code") };
        assert_eq!(filetype.as_deref(), Some("json"));
        assert!(content.contains("\"ok\": true"));
        assert!(content.contains("[truncated]"));
        assert!(content.encode_utf16().count() <= 4096);
    }

    #[test]
    fn interrupted_assistant() {
        let c = StreamCommit { interrupted: true, part_id: Some("part-1".into()), ..StreamCommit::new(EntryKind::Assistant, "", Phase::Final, Source::Assistant) };
        assert_eq!(entry_body(&c, BodyOptions::default()), text("assistant interrupted"));
    }
}

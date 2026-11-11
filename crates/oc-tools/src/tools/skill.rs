//! `skill` (`core/src/tool/plugin/skill.ts`, `core/src/skill.ts`).

use std::path::{Path, PathBuf};

use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::host::SkillInfo;
use crate::tool::{DefinitionContext, ErrorCause, Tool, ToolContext, ToolDefinition, ToolError, ToolOptions, ToolResult, decode, meta};
use crate::util::js;

pub const DESCRIPTION: &str = "Load a specialized skill's instructions and resources into the current conversation when the task at hand matches its description.\n\nThe skill ID must match an available skill or a skill explicitly referenced by the user.";

/// `Skill.isUri`: a skill that lives in memory rather than on disk.
pub fn is_uri(path: &Path) -> bool {
    path.to_string_lossy().starts_with("skill://")
}

/// `Skill.uri(id, file)`.
pub fn uri(id: &str, file: Option<&str>) -> String {
    match file {
        Some(file) => format!("skill://{id}/{file}"),
        None => format!("skill://{id}"),
    }
}

/// The skill's base directory: `skill://<id>` for an in-memory skill, else its file's folder.
fn directory(skill: &SkillInfo) -> String {
    if is_uri(&skill.path) { uri(&skill.id, None) } else { crate::util::dirname(&skill.path).to_string_lossy().into_owned() }
}

/// `Skill.toModelOutput`.
pub fn to_model_output(skill: &SkillInfo, files: &[String]) -> String {
    let directory = directory(skill);
    let mut lines = vec![
        format!("<skill_content name=\"{}\">", skill.name),
        format!("# Skill: {}", skill.name),
        String::new(),
        js::trim(&skill.content).to_string(),
        String::new(),
        format!("Base directory for this skill: {directory}"),
        "Relative paths in this skill (e.g., scripts/, reference/) are relative to this base directory.".into(),
        "Note: file list is sampled.".into(),
        String::new(),
        "<skill_files>".into(),
    ];
    lines.extend(files.iter().map(|f| format!("<file>{f}</file>")));
    lines.push("</skill_files>".into());
    lines.push("</skill_content>".into());
    lines.join("\n")
}

fn scan_files(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    for entry in entries.flatten() {
        let path = entry.path();
        match std::fs::metadata(&path) {
            Ok(m) if m.is_dir() => scan_files(&path, out),
            Ok(m) if m.is_file() => out.push(path),
            _ => {}
        }
    }
}

/// `Skill.prepare`: `{directory, output}`. An in-memory skill lists up to 10 of its virtual files as URIs.
pub fn prepare(skill: &SkillInfo) -> (String, String) {
    if is_uri(&skill.path) {
        let mut files: Vec<&str> = skill.files.iter().map(|(file, _)| file.as_str()).filter(|file| *file != "SKILL.md").collect();
        files.sort_by(|a, b| a.encode_utf16().cmp(b.encode_utf16()));
        let files: Vec<String> = files.into_iter().take(10).map(|file| uri(&skill.id, Some(file))).collect();
        return (directory(skill), to_model_output(skill, &files));
    }
    let directory = crate::util::dirname(&skill.path);
    let mut files: Vec<String> = Vec::new();
    if skill.path.file_name().is_some_and(|n| n == "SKILL.md") {
        let mut found = Vec::new();
        scan_files(&directory, &mut found);
        files = found
            .into_iter()
            .filter(|f| f.file_name().is_none_or(|n| n != "SKILL.md"))
            .map(|f| f.to_string_lossy().into_owned())
            .collect();
        files.sort_by(|a, b| a.encode_utf16().cmp(b.encode_utf16()));
        files.truncate(10);
    }
    (directory.to_string_lossy().into_owned(), to_model_output(skill, &files))
}

#[derive(Deserialize)]
struct Input {
    id: String,
}

pub struct SkillTool;

#[async_trait]
impl Tool for SkillTool {
    fn name(&self) -> &str {
        "skill"
    }

    fn options(&self) -> ToolOptions {
        ToolOptions::direct()
    }

    fn definition(&self, _ctx: &DefinitionContext) -> ToolDefinition {
        super::definition("skill", DESCRIPTION, include_str!("../../schemas/skill.json"))
    }

    async fn execute(&self, input: Value, ctx: ToolContext) -> Result<ToolResult, ToolError> {
        let input: Input = decode(input)?;
        let leaf = format!("Unable to load skill {}", input.id);
        let skill = match ctx.host.get_skill(&input.id).await {
            Ok(Some(skill)) => skill,
            Ok(None) => return Err(ToolError::new(leaf)),
            Err(m) => return Err(ToolError::with_cause(leaf, ErrorCause::Error(m))),
        };
        super::assert(&ctx, "skill", vec![skill.id.clone()], Some(vec![skill.id.clone()]), None).await.map_err(super::perm(leaf))?;
        let (directory, output) = prepare(&skill);
        Ok(ToolResult::text(output.clone())
            .with_output(json!({"name": skill.name, "directory": directory, "output": output}))
            .with_metadata(meta(vec![("name", json!(skill.name)), ("directory", json!(directory))])))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// skill.test.ts "stores virtual skill files ... and prepares skill:// output without disk scans" (#60921).
    #[test]
    fn prepares_in_memory_skills() {
        let files = vec![("SKILL.md".into(), "ignored".into()), ("references/b.md".into(), "B".into()), ("references/a.md".into(), "A".into())];
        let skill = SkillInfo { id: "review".into(), name: "Review".into(), description: None, path: "skill://review/SKILL.md".into(), content: "Body".into(), files };
        let (directory, output) = prepare(&skill);
        assert_eq!(directory, "skill://review");
        assert_eq!(output, to_model_output(&skill, &["skill://review/references/a.md".into(), "skill://review/references/b.md".into()]));
        assert!(output.contains("Base directory for this skill: skill://review\n"));
        let builtin = SkillInfo { files: vec![], path: "skill://opencode/SKILL.md".into(), id: "opencode".into(), ..skill };
        assert!(prepare(&builtin).1.ends_with("<skill_files>\n</skill_files>\n</skill_content>"));
    }

    #[test]
    fn prepares_output() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("my-skill");
        std::fs::create_dir_all(root.join("scripts")).unwrap();
        std::fs::write(root.join("SKILL.md"), "x").unwrap();
        std::fs::write(root.join("scripts/run.sh"), "x").unwrap();
        std::fs::write(root.join(".hidden"), "x").unwrap();
        let skill = SkillInfo { id: "my-skill".into(), name: "My Skill".into(), description: None, path: root.join("SKILL.md"), content: "\n# Body\n".into(), files: vec![] };
        let (directory, output) = prepare(&skill);
        let d = root.to_string_lossy();
        assert_eq!(directory, d);
        // Absolute glob matches in the platform's form.
        let (hidden, script) = (root.join(".hidden"), root.join("scripts").join("run.sh"));
        let (hidden, script) = (hidden.display(), script.display());
        assert_eq!(
            output,
            format!(
                "<skill_content name=\"My Skill\">\n# Skill: My Skill\n\n# Body\n\nBase directory for this skill: {d}\nRelative paths in this skill (e.g., scripts/, reference/) are relative to this base directory.\nNote: file list is sampled.\n\n<skill_files>\n<file>{hidden}</file>\n<file>{script}</file>\n</skill_files>\n</skill_content>"
            )
        );
    }
}

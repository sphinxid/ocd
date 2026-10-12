//! AGENTS.md discovery (`core/config/plugin/instruction.ts`), nested AGENTS.md on read
//! (`core/tool/plugin/read.ts`, `core/session/instructions.ts`), and the skill content block
//! (`core/skill.ts` toModelOutput/prepare).

use std::path::{Path, PathBuf};

use crate::instructions::File;

fn resolve(p: &Path) -> PathBuf {
    std::fs::canonicalize(p).unwrap_or_else(|_| p.to_path_buf())
}

/// Directories from `start` up to and including `stop` (nearest first).
pub fn ancestors(start: &Path, stop: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let mut cur = Some(start);
    while let Some(d) = cur {
        out.push(d.to_path_buf());
        if d == stop {
            break;
        }
        cur = d.parent();
    }
    out
}

pub struct DiscoveryInput<'a> {
    pub directory: &'a Path,
    pub project_directory: &'a Path,
    pub home: &'a Path,
    pub global_config: &'a Path,
    pub project: bool,
    pub global: bool,
}

/// Returns `None` when a source is unavailable (a listed file could not be read).
pub fn discover(input: &DiscoveryInput) -> Option<Vec<File>> {
    if !input.project && !input.global {
        return Some(vec![]);
    }
    let start = resolve(input.directory);
    let root = resolve(input.project_directory);
    let home = resolve(input.home);
    let project = input.project && start.starts_with(&root);
    let stop = if start.starts_with(&home) { home } else { root };
    let global_file = resolve(&input.global_config.join("AGENTS.md"));
    let read = |p: &Path| std::fs::read_to_string(p).ok().map(|content| File { path: p.display().to_string(), content });

    let mut files = Vec::new();
    if input.global && global_file.is_file() {
        // global read failures make the global source empty
        if let Some(f) = read(&global_file) {
            files.push(f);
        }
    }
    if project {
        let mut seen: Vec<PathBuf> = Vec::new();
        for dir in ancestors(&start, &stop) {
            let candidate = dir.join("AGENTS.md");
            if !candidate.is_file() {
                continue;
            }
            let r = resolve(&candidate);
            if (!input.global && r == global_file) || seen.contains(&r) {
                continue;
            }
            seen.push(r);
        }
        let mut project_files = Vec::new();
        for p in seen {
            project_files.push(read(&p)?);
        }
        files.extend(project_files);
    }
    Some(files)
}

/// Nested AGENTS.md for a successful read of `target` (directory listing or file).
pub fn nested_for_read(target: &Path, is_dir: bool, location: &Path) -> Vec<PathBuf> {
    let root = resolve(location);
    let dir = if is_dir { resolve(target) } else { resolve(target.parent().unwrap_or(target)) };
    if !dir.starts_with(&root) {
        return vec![];
    }
    let resolved_target = resolve(target);
    ancestors(&dir, &root)
        .into_iter()
        .filter(|d| !root.starts_with(d))
        .map(|d| d.join("AGENTS.md"))
        .filter(|p| p.is_file() && resolve(p) != resolved_target)
        .collect()
}

/// `SessionInstructions.load` synthetic payload for nested instruction files.
pub fn nested_synthetic(files: &[File], project_root: &Path) -> (String, String, serde_json::Value) {
    let text = files.iter().map(|f| format!("Instructions from: {}\n{}", f.path, f.content)).collect::<Vec<_>>().join("\n\n");
    let rels: Vec<String> = files
        .iter()
        .map(|f| {
            let p = Path::new(&f.path);
            p.strip_prefix(project_root).map(|r| r.display().to_string()).unwrap_or_else(|_| f.path.clone())
        })
        .collect();
    let description = format!("Loaded {}", rels.join(", "));
    let metadata = serde_json::json!({"instruction": {"paths": files.iter().map(|f| f.path.clone()).collect::<Vec<_>>()}});
    (text, description, metadata)
}

/// Skill content block (`Skill.toModelOutput` + `prepare`).
pub fn skill_block(name: &str, skill_path: &Path, content: &str) -> String {
    let directory = skill_path.parent().unwrap_or(Path::new("/"));
    let files: Vec<String> = if skill_path.file_name().is_some_and(|n| n == "SKILL.md") {
        let mut all: Vec<String> = walk_files(directory)
            .into_iter()
            .filter(|f| Path::new(f).file_name().is_some_and(|n| n != "SKILL.md"))
            .collect();
        all.sort();
        all.into_iter().take(10).collect()
    } else {
        vec![]
    };
    let mut lines = vec![
        format!("<skill_content name=\"{name}\">"),
        format!("# Skill: {name}"),
        String::new(),
        content.trim().to_string(),
        String::new(),
        format!("Base directory for this skill: {}", directory.display()),
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

fn walk_files(dir: &Path) -> Vec<String> {
    let mut out = Vec::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        let Ok(rd) = std::fs::read_dir(&d) else { continue };
        for e in rd.flatten() {
            let p = e.path();
            match e.file_type() {
                Ok(t) if t.is_dir() => stack.push(p),
                Ok(t) if t.is_file() => out.push(p.display().to_string()),
                _ => {}
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn walks_project_agents() {
        let tmp = std::env::temp_dir().join(format!("oc-disc-{}", std::process::id()));
        let sub = tmp.join("repo/pkg");
        std::fs::create_dir_all(&sub).unwrap();
        std::fs::write(tmp.join("repo/AGENTS.md"), "root").unwrap();
        std::fs::write(sub.join("AGENTS.md"), "pkg").unwrap();
        let files = discover(&DiscoveryInput {
            directory: &sub,
            project_directory: &tmp.join("repo"),
            home: Path::new("/nonexistent-home"),
            global_config: Path::new("/nonexistent-config"),
            project: true,
            global: true,
        })
        .unwrap();
        assert_eq!(files.iter().map(|f| f.content.as_str()).collect::<Vec<_>>(), vec!["pkg", "root"]);
        let nested = nested_for_read(&sub.join("x.rs"), false, &tmp.join("repo"));
        assert_eq!(nested, vec![resolve(&sub).join("AGENTS.md")]);
        std::fs::remove_dir_all(&tmp).ok();
    }
}

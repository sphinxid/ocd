//! External-directory approval and target resolution (`core/src/file-access.ts`).

use std::path::{Path, PathBuf};

use oc_schema::Metadata;

use crate::host::PermissionError;
use crate::tool::ToolContext;
use crate::util;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    File,
    Directory,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExternalDirectory {
    /// Lexical directory used as the approval boundary.
    pub directory: PathBuf,
    pub resource: String,
    pub save: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Target {
    pub absolute: PathBuf,
    /// Location-relative for internal paths, absolute for external paths.
    pub resource: String,
    pub external_directory: Option<ExternalDirectory>,
}

/// `FSUtil.windowsPath`.
pub fn windows_path(input: &str) -> String {
    if !cfg!(windows) {
        return input.to_string();
    }
    let re = [
        regex::Regex::new(r"^/([a-zA-Z]):(?:[\\/]|$)").unwrap(),
        regex::Regex::new(r"^/([a-zA-Z])(?:/|$)").unwrap(),
        regex::Regex::new(r"^/cygdrive/([a-zA-Z])(?:/|$)").unwrap(),
        regex::Regex::new(r"^/mnt/([a-zA-Z])(?:/|$)").unwrap(),
    ];
    let mut value = input.to_string();
    for r in &re {
        value = r.replace(&value, |c: &regex::Captures| format!("{}:/", c[1].to_uppercase())).into_owned();
    }
    value
}

/// `FileAccess.resolvePath(directory, input)`: `~` expansion + lexical resolution.
pub fn resolve_path(directory: &Path, input: &str, home: &Path) -> PathBuf {
    let normalized = windows_path(input);
    if normalized == "~" {
        return util::normalize(home);
    }
    if normalized.starts_with("~/") || (cfg!(windows) && normalized.starts_with("~\\")) {
        return util::normalize(&home.join(&normalized[2..]));
    }
    util::resolve(directory, &normalized)
}

/// `Project.root`: nearest ancestor containing `.git` or `.hg`.
pub fn project_root(start: &Path) -> Option<PathBuf> {
    let mut current = start.to_path_buf();
    loop {
        for marker in [".git", ".hg"] {
            if current.join(marker).exists() {
                return Some(current);
            }
        }
        match current.parent() {
            Some(parent) if parent != current => current = parent.to_path_buf(),
            _ => return None,
        }
    }
}

fn is_fs_root(path: &Path) -> bool {
    path.parent().is_none()
}

/// `FileAccess.resolve({path, kind?})`.
pub fn resolve(ctx: &ToolContext, path: &str, kind: Option<Kind>) -> Target {
    resolve_in(&ctx.directory, &ctx.worktree, &ctx.global.home, path, kind)
}

pub fn resolve_in(directory: &Path, worktree: &Path, home: &Path, path: &str, kind: Option<Kind>) -> Target {
    let absolute = resolve_path(directory, path, home);
    let worktree = util::normalize(worktree);
    let internal = util::contains(directory, &absolute) || (!is_fs_root(&worktree) && util::contains(&worktree, &absolute));
    if internal {
        let rel = util::relative(directory, &absolute);
        return Target {
            resource: util::slash(if rel.is_empty() { "." } else { &rel }),
            absolute,
            external_directory: None,
        };
    }
    let is_dir = match kind {
        Some(Kind::Directory) => true,
        Some(Kind::File) => false,
        None => std::fs::metadata(&absolute).map(|m| m.is_dir()).unwrap_or(false),
    };
    let boundary = if is_dir { absolute.clone() } else { util::dirname(&absolute) };
    let save_root = project_root(&boundary).unwrap_or_else(|| boundary.clone());
    Target {
        resource: util::slash(&absolute.to_string_lossy()),
        external_directory: Some(ExternalDirectory {
            resource: util::slash(&boundary.join("*").to_string_lossy()),
            save: util::slash(&save_root.join("*").to_string_lossy()),
            directory: boundary,
        }),
        absolute,
    }
}

/// `authorizeExternal(targets, ctx, metadata?)`.
pub async fn authorize_external(ctx: &ToolContext, targets: &[&Target], metadata: Option<Metadata>) -> Result<(), PermissionError> {
    let mut external: Vec<&ExternalDirectory> = Vec::new();
    for target in targets {
        if let Some(dir) = &target.external_directory {
            if !external.iter().any(|e| e.resource == dir.resource) {
                external.push(dir);
            }
        }
    }
    if external.is_empty() {
        return Ok(());
    }
    let request = ctx.permission(
        "external_directory",
        external.iter().map(|e| e.resource.clone()).collect(),
        Some(external.iter().map(|e| e.save.clone()).collect()),
        metadata,
    );
    ctx.host.ask_permission(request).await
}

/// `authorizeRead(file, ctx, {siblingOf}?)`.
pub async fn authorize_read(ctx: &ToolContext, file: &str, sibling_of: Option<&Target>) -> Result<Target, PermissionError> {
    let target = resolve(ctx, file, sibling_of.map(|_| Kind::File));
    let sibling = sibling_of.is_some_and(|s| util::dirname(&target.absolute) == util::dirname(&s.absolute));
    if !sibling {
        authorize_external(ctx, &[&target], None).await?;
    }
    ctx.host
        .ask_permission(ctx.permission("read", vec![target.resource.clone()], Some(vec!["*".into()]), None))
        .await?;
    Ok(target)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn internal_and_external() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("project");
        std::fs::create_dir_all(root.join("src")).unwrap();
        let home = dir.path().join("home");
        let t = resolve_in(&root, &root, &home, "src/a.ts", None);
        assert_eq!(t.resource, "src/a.ts");
        assert!(t.external_directory.is_none());
        let t = resolve_in(&root, &root, &home, ".", None);
        assert_eq!(t.resource, ".");
        let outside = dir.path().join("other");
        std::fs::create_dir_all(outside.join(".git")).unwrap();
        std::fs::create_dir_all(outside.join("deep")).unwrap();
        let t = resolve_in(&root, &root, &home, &outside.join("deep/file.txt").to_string_lossy(), Some(Kind::File));
        let ext = t.external_directory.unwrap();
        assert_eq!(ext.resource, format!("{}/deep/*", outside.to_string_lossy()));
        assert_eq!(ext.save, format!("{}/*", outside.to_string_lossy()));
        let t = resolve_in(&root, &root, &home, &outside.join("deep").to_string_lossy(), None);
        assert_eq!(t.external_directory.unwrap().resource, format!("{}/deep/*", outside.to_string_lossy()));
        let t = resolve_in(&root, &root, &home, "~/x", None);
        assert_eq!(t.absolute, home.join("x"));
    }

    #[test]
    fn worktree_internal() {
        let root = Path::new("/repo/sub");
        let t = resolve_in(root, Path::new("/repo"), Path::new("/home"), "../other/file", Some(Kind::File));
        assert_eq!(t.resource, "../other/file");
        assert!(t.external_directory.is_none());
        let t = resolve_in(root, Path::new("/"), Path::new("/home"), "/etc/x", Some(Kind::File));
        assert!(t.external_directory.is_some());
    }
}

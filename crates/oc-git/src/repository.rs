//! Port of `packages/core/src/repository.ts`: remote repository references and cache paths.

use std::path::{Path, PathBuf};

use once_cell::sync::Lazy;
use regex::Regex;
use serde::{Deserialize, Serialize};

use crate::paths;

/// A parsed reference. `protocol == Some("file:")` marks a local file repository.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Reference {
    pub host: String,
    pub path: String,
    pub segments: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub owner: Option<String>,
    pub repo: String,
    pub remote: String,
    pub label: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub protocol: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum RepositoryError {
    /// `RepositoryInvalidReferenceError`
    #[error("{message}")]
    InvalidReference { repository: String, message: String },
    /// `RepositoryUnsupportedLocalRepositoryError`
    #[error("{message}")]
    UnsupportedLocalRepository { repository: String, message: String },
    /// `RepositoryInvalidBranchError`
    #[error("{message}")]
    InvalidBranch { branch: String, message: String },
}

impl RepositoryError {
    pub fn tag(&self) -> &'static str {
        match self {
            RepositoryError::InvalidReference { .. } => "RepositoryInvalidReferenceError",
            RepositoryError::UnsupportedLocalRepository { .. } => "RepositoryUnsupportedLocalRepositoryError",
            RepositoryError::InvalidBranch { .. } => "RepositoryInvalidBranchError",
        }
    }
}

pub const INVALID_REFERENCE_MESSAGE: &str =
    "Repository must be a git URL, host/path reference, or GitHub owner/repo shorthand";
pub const LOCAL_REPOSITORY_MESSAGE: &str = "Local file repositories are not supported";
pub const INVALID_BRANCH_MESSAGE: &str =
    "Branch must contain only alphanumeric characters, /, _, ., and -, and cannot start with - or contain ..";

static GITHUB_PREFIXED: Lazy<Regex> = Lazy::new(|| Regex::new(r"^github:([^/\s]+)/([^/\s]+)$").unwrap());
static SCP: Lazy<Regex> = Lazy::new(|| Regex::new(r"^(?:[^@/\s]+@)?([^:/\s]+):(.+)$").unwrap());
static BRANCH: Lazy<Regex> = Lazy::new(|| Regex::new(r"^[A-Za-z0-9/_.-]+$").unwrap());
static UNSAFE_SEGMENT: Lazy<Regex> = Lazy::new(|| Regex::new(r"[\s/\\]").unwrap());

pub fn parse(input: &str) -> Option<Reference> {
    let cleaned = normalize_input(input);
    if cleaned.is_empty() {
        return None;
    }
    if let Some(captures) = GITHUB_PREFIXED.captures(&cleaned) {
        return build_remote("github.com", vec![captures[1].to_string(), captures[2].to_string()], None, None);
    }
    if !cleaned.contains("://") {
        // `.+` in JS does not cross line terminators; inputs are already single-line in practice.
        if let Some(captures) = SCP.captures(&cleaned) {
            return build_remote(&captures[1], parts(&captures[2]), Some(cleaned.clone()), None);
        }
        let direct = parts(&cleaned);
        if direct.len() >= 2 && host_like(&direct[0]) {
            return build_remote(&direct[0].clone(), direct[1..].to_vec(), None, None);
        }
        if direct.len() == 2 {
            return build_remote("github.com", direct, None, None);
        }
    }
    let url = url::Url::parse(&cleaned).ok()?;
    let protocol = format!("{}:", url.scheme());
    if url.scheme() == "file" {
        return build_file(&url, &cleaned);
    }
    let host = match (url.host_str(), url.port()) {
        (Some(host), Some(port)) => format!("{host}:{port}"),
        (Some(host), None) => host.to_string(),
        (None, _) => String::new(),
    };
    let segments = parts(url.path());
    let remote = if host == "github.com" { github_remote(&segments.join("/")) } else { cleaned.clone() };
    build_remote(&host, segments, Some(remote), Some(protocol))
}

pub fn parse_remote(input: &str) -> Result<Reference, RepositoryError> {
    let Some(reference) = parse(input) else {
        return Err(RepositoryError::InvalidReference {
            repository: input.to_string(),
            message: INVALID_REFERENCE_MESSAGE.to_string(),
        });
    };
    if is_file(&reference) {
        return Err(RepositoryError::UnsupportedLocalRepository {
            repository: input.to_string(),
            message: LOCAL_REPOSITORY_MESSAGE.to_string(),
        });
    }
    Ok(reference)
}

pub fn validate_branch(branch: &str) -> Result<(), RepositoryError> {
    if BRANCH.is_match(branch) && !branch.starts_with('-') && !branch.contains("..") {
        return Ok(());
    }
    Err(RepositoryError::InvalidBranch { branch: branch.to_string(), message: INVALID_BRANCH_MESSAGE.to_string() })
}

pub fn is_file(reference: &Reference) -> bool {
    reference.protocol.as_deref() == Some("file:")
}

pub fn is_remote(reference: &Reference) -> bool {
    !is_file(reference)
}

/// `encodeURIComponent`.
pub fn encode_uri_component(input: &str) -> String {
    let mut out = String::new();
    for byte in input.as_bytes() {
        let c = *byte as char;
        if c.is_ascii_alphanumeric() || "-_.!~*'()".contains(c) {
            out.push(c);
        } else {
            out.push_str(&format!("%{byte:02X}"));
        }
    }
    out
}

/// Checkouts are keyed by remote and branch: `<root>/<host parts>/<segments>[@<encoded branch>]`.
pub fn cache_path(root: &Path, reference: &Reference, branch: Option<&str>) -> PathBuf {
    let mut base = root.to_path_buf();
    for part in reference.host.split(':') {
        base.push(part);
    }
    for segment in &reference.segments {
        base.push(segment);
    }
    let base = paths::normalize(&base);
    match branch.filter(|b| !b.is_empty()) {
        Some(branch) => PathBuf::from(format!("{}@{}", base.display(), encode_uri_component(branch))),
        None => base,
    }
}

pub fn cache_identity(reference: &Reference) -> String {
    format!("{}/{}", reference.host, reference.path)
}

pub fn same(left: &Reference, right: &Reference) -> bool {
    cache_identity(left) == cache_identity(right)
}

fn normalize_input(input: &str) -> String {
    let mut value = input.trim();
    if let Some(rest) = value.strip_prefix("git+") {
        value = rest;
    }
    let value = match value.find('#') {
        Some(index) => &value[..index],
        None => value,
    };
    value.trim_end_matches('/').to_string()
}

fn trim_git_suffix(input: &str) -> String {
    input.strip_suffix(".git").unwrap_or(input).to_string()
}

fn parts(input: &str) -> Vec<String> {
    input.split('/').map(|item| trim_git_suffix(item.trim())).filter(|item| !item.is_empty()).collect()
}

fn safe_host(input: &str) -> bool {
    !input.is_empty() && !input.starts_with('-') && input.split(':').all(safe_segment)
}

fn safe_segment(input: &str) -> bool {
    input != "." && input != ".." && !input.contains(':') && !UNSAFE_SEGMENT.is_match(input)
}

fn host_like(input: &str) -> bool {
    input.contains('.') || input.contains(':') || input == "localhost"
}

fn github_remote(pathname: &str) -> String {
    let base = std::env::var("OPENCODE_REPO_CLONE_GITHUB_BASE_URL").ok().filter(|v| !v.is_empty());
    let Some(base) = base else { return format!("https://github.com/{pathname}.git") };
    let base = if base.ends_with('/') { base } else { format!("{base}/") };
    url::Url::parse(&base)
        .and_then(|url| url.join(&format!("{pathname}.git")))
        .map(|url| url.to_string())
        .unwrap_or_else(|_| format!("https://github.com/{pathname}.git"))
}

fn build_remote(
    host: &str,
    segments: Vec<String>,
    remote: Option<String>,
    protocol: Option<String>,
) -> Option<Reference> {
    let segments: Vec<String> =
        segments.iter().map(|s| trim_git_suffix(s)).filter(|s| !s.is_empty()).collect();
    if !safe_host(host) || segments.is_empty() || segments.iter().any(|s| !safe_segment(s)) {
        return None;
    }
    let repository_path = segments.join("/");
    let host = host.to_lowercase();
    let remote = remote.unwrap_or_else(|| {
        if host == "github.com" {
            github_remote(&repository_path)
        } else {
            format!("https://{host}/{repository_path}.git")
        }
    });
    let label = if host == "github.com" && segments.len() == 2 {
        repository_path.clone()
    } else {
        format!("{host}/{repository_path}")
    };
    Some(Reference {
        owner: if segments.len() == 2 { Some(segments[0].clone()) } else { None },
        repo: segments[segments.len() - 1].clone(),
        host,
        path: repository_path,
        segments,
        remote,
        label,
        protocol,
    })
}

fn build_file(url: &url::Url, remote: &str) -> Option<Reference> {
    let file_path = paths::normalize(&url.to_file_path().ok()?);
    let text = file_path.to_string_lossy().into_owned();
    let segments: Vec<String> = text.split(['/', '\\']).filter(|s| !s.is_empty()).map(str::to_string).collect();
    let last = segments.last()?.clone();
    Some(Reference {
        host: "file".to_string(),
        path: text.clone(),
        segments: segments.iter().map(|s| s.strip_suffix(':').unwrap_or(s).to_string()).collect(),
        owner: None,
        repo: trim_git_suffix(&last),
        remote: remote.to_string(),
        label: text,
        protocol: Some("file:".to_string()),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn err(input: &str) -> bool {
        matches!(parse_remote(input), Err(RepositoryError::InvalidReference { .. }))
    }

    #[test]
    fn github_shorthand_and_cache_path() {
        let reference = parse_remote("owner/repo").unwrap();
        assert_eq!(reference.host, "github.com");
        assert_eq!(reference.path, "owner/repo");
        assert_eq!(reference.segments, vec!["owner", "repo"]);
        assert_eq!(reference.owner.as_deref(), Some("owner"));
        assert_eq!(reference.repo, "repo");
        assert_eq!(reference.remote, "https://github.com/owner/repo.git");
        assert_eq!(reference.label, "owner/repo");
        let root = Path::new("/cache");
        assert_eq!(cache_path(root, &reference, None), PathBuf::from("/cache/github.com/owner/repo"));
        assert_eq!(cache_path(root, &reference, Some("main")), PathBuf::from("/cache/github.com/owner/repo@main"));
        assert_eq!(
            cache_path(root, &reference, Some("feature/x")),
            PathBuf::from("/cache/github.com/owner/repo@feature%2Fx")
        );
        assert_eq!(cache_identity(&reference), "github.com/owner/repo");
    }

    #[test]
    fn host_path_and_scp() {
        let gitlab = parse_remote("gitlab.com/group/repo").unwrap();
        assert_eq!(gitlab.host, "gitlab.com");
        assert_eq!(gitlab.remote, "https://gitlab.com/group/repo.git");
        assert_eq!(gitlab.label, "gitlab.com/group/repo");
        let scp = parse_remote("git@github.com:owner/repo.git").unwrap();
        assert_eq!(scp.path, "owner/repo");
        assert_eq!(scp.remote, "git@github.com:owner/repo.git");
        assert_eq!(scp.label, "owner/repo");
    }

    #[test]
    fn host_with_port() {
        let reference = parse_remote("ssh://git@example.com:2222/owner/repo").unwrap();
        assert_eq!(cache_path(Path::new("/cache"), &reference, None), PathBuf::from("/cache/example.com/2222/owner/repo"));
        assert_eq!(reference.protocol.as_deref(), Some("ssh:"));
    }

    #[test]
    fn file_references() {
        let url = url::Url::from_file_path("/tmp/x/repo.git").unwrap();
        let reference = parse(url.as_str()).unwrap();
        assert_eq!(reference.host, "file");
        assert_eq!(reference.label, "/tmp/x/repo.git");
        assert_eq!(reference.repo, "repo");
        assert!(is_file(&reference) && !is_remote(&reference));
        assert!(matches!(parse_remote(url.as_str()), Err(RepositoryError::UnsupportedLocalRepository { .. })));
    }

    #[test]
    fn rejects_unsafe_references_and_branches() {
        assert!(err("not-a-repo"));
        assert!(err("git@github.com:../../../etc/passwd"));
        assert!(validate_branch("feature/docs.v1").is_ok());
        assert!(validate_branch("-bad").is_err());
        assert!(validate_branch("bad..branch").is_err());
        assert!(validate_branch("bad branch").is_err());
        for input in ["..:repo", "git@..:repo", "../owner/repo", "ssh://../repo", "ssh://..:22/repo", "https://%2e%2e/owner/repo", ".:repo"] {
            assert!(err(input), "{input}");
        }
    }

    #[test]
    fn identity_independent_of_spelling() {
        let shorthand = parse_remote("owner/repo").unwrap();
        assert!(same(&shorthand, &parse_remote("https://github.com/owner/repo.git").unwrap()));
        assert!(same(&shorthand, &parse_remote("github.com/owner/repo").unwrap()));
        assert!(same(&shorthand, &parse_remote("git+https://github.com/owner/repo/#readme").unwrap()));
    }

    #[test]
    fn encodes_like_encode_uri_component() {
        assert_eq!(encode_uri_component("a b/é~*"), "a%20b%2F%C3%A9~*");
    }
}

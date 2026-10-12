#![allow(dead_code)]

use std::path::{Path, PathBuf};
use std::process::Command;

pub fn git(cwd: &Path, args: &[&str]) -> String {
    let output = Command::new("git").args(args).current_dir(cwd).output().expect("git runs");
    assert!(
        output.status.success(),
        "git {:?} failed: {}",
        args,
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).into_owned()
}

pub fn git_status(cwd: &Path, args: &[&str]) -> i32 {
    Command::new("git").args(args).current_dir(cwd).output().expect("git runs").status.code().unwrap_or(-1)
}

/// `fixture/git.ts` `initRepo`.
pub fn init_repo(directory: &Path) {
    std::fs::create_dir_all(directory).unwrap();
    git(directory, &["init", "-q"]);
    git(directory, &["config", "core.fsmonitor", "false"]);
    git(directory, &["config", "commit.gpgsign", "false"]);
    git(directory, &["config", "user.email", "test@opencode.test"]);
    git(directory, &["config", "user.name", "Test"]);
    git(directory, &["commit", "-q", "--allow-empty", "-m", "root"]);
}

/// `snapshot.test.ts` `initGit`.
pub fn init_git(directory: &Path, commit: bool) {
    git(directory, &["init", "-q"]);
    git(directory, &["-c", "core.fsmonitor=false", "add", "."]);
    if commit {
        git(
            directory,
            &["-c", "user.email=test@opencode.test", "-c", "user.name=Test", "commit", "-q", "--no-gpg-sign", "-m", "initial"],
        );
    }
}

pub fn commit_all(directory: &Path, message: &str) {
    git(directory, &["add", "."]);
    git(directory, &["commit", "-q", "-m", message]);
}

pub fn write(path: impl AsRef<Path>, content: impl AsRef<[u8]>) {
    let path = path.as_ref();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).unwrap();
    }
    std::fs::write(path, content).unwrap();
}

pub fn read(path: impl AsRef<Path>) -> String {
    std::fs::read_to_string(path).unwrap().replace("\r\n", "\n")
}

pub fn real(path: &Path) -> PathBuf {
    std::fs::canonicalize(path).unwrap()
}

pub fn tmp() -> tempfile::TempDir {
    tempfile::tempdir().unwrap()
}

pub struct Remote {
    pub root: PathBuf,
    pub source: PathBuf,
    pub origin: PathBuf,
    pub remote: String,
}

/// `fixture/git.ts` `gitRemote`.
pub fn git_remote(root: &Path) -> Remote {
    let origin = root.join("origin.git");
    let source = root.join("source");
    git(root, &["init", "-q", "--bare", origin.to_str().unwrap()]);
    git(root, &["init", "-q", source.to_str().unwrap()]);
    git(&source, &["config", "user.email", "test@example.com"]);
    git(&source, &["config", "user.name", "Test"]);
    git(&source, &["config", "commit.gpgsign", "false"]);
    write(source.join("README.md"), "one\n");
    git(&source, &["add", "README.md"]);
    git(&source, &["commit", "-q", "-m", "initial"]);
    git(&source, &["branch", "-M", "main"]);
    let remote = url::Url::from_file_path(&origin).unwrap().to_string();
    git(&source, &["remote", "add", "origin", &remote]);
    git(&source, &["push", "-q", "-u", "origin", "main"]);
    git(root, &["--git-dir", origin.to_str().unwrap(), "symbolic-ref", "HEAD", "refs/heads/main"]);
    Remote { root: root.to_path_buf(), source, origin, remote }
}

pub fn commit_readme(source: &Path, content: &str, message: &str) {
    write(source.join("README.md"), content);
    git(source, &["add", "README.md"]);
    git(source, &["commit", "-q", "-m", message]);
    git(source, &["push", "-q", "origin", "HEAD"]);
}

pub fn branch(source: &Path, name: &str, content: &str) {
    git(source, &["checkout", "-q", "-b", name]);
    write(source.join("README.md"), content);
    git(source, &["add", "README.md"]);
    git(source, &["commit", "-q", "-m", name]);
    git(source, &["push", "-q", "-u", "origin", name]);
    git(source, &["checkout", "-q", "main"]);
}

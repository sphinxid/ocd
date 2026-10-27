//! Embeds the web UI archive (upstream `virtual:opencode-app-assets`) when one has been built.
//!
//! Source: `OCD_WEB_UI_ARCHIVE` (build-time env) or `web-ui/app-archive.bin` next to this file,
//! as written by `scripts/build-web-ui.sh`. The archive is not committed; without it the binary
//! embeds nothing and serves no web UI (like an upstream build made with `--skip-web-ui`).
//!
//! Also sets the ocd build version (see [`build_version`]).
use std::path::{Path, PathBuf};
use std::process::Command;

fn main() {
    let manifest = PathBuf::from(std::env::var_os("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR"));
    let out = PathBuf::from(std::env::var_os("OUT_DIR").expect("OUT_DIR")).join("web_ui_archive.rs");
    println!("cargo:rerun-if-env-changed=OCD_WEB_UI_ARCHIVE");
    let dir = manifest.join("web-ui");
    // The directory is committed (with its .gitignore), so watching it never forces a rebuild.
    println!("cargo:rerun-if-changed={}", dir.display());
    let archive = match std::env::var_os("OCD_WEB_UI_ARCHIVE").filter(|v| !v.is_empty()) {
        Some(path) => {
            let path = PathBuf::from(path);
            println!("cargo:rerun-if-changed={}", path.display());
            Some(path)
        }
        None => Some(dir.join("app-archive.bin")).filter(|p| p.is_file()),
    };
    let code = match archive.as_deref().map(|p| (p, check(p))) {
        Some((path, Ok(()))) => {
            let path = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
            format!("pub static ARCHIVE: &[u8] = include_bytes!({:?});\n", path.to_string_lossy())
        }
        Some((path, Err(e))) => {
            println!("cargo:warning=not embedding the web UI archive {}: {e}", path.display());
            "pub static ARCHIVE: &[u8] = &[];\n".to_string()
        }
        None => "pub static ARCHIVE: &[u8] = &[];\n".to_string(),
    };
    std::fs::write(&out, code).expect("write web_ui_archive.rs");
    build_version(&manifest);
}

/// `OCD_BUILD_VERSION` for `ocd --version`. The release workflow sets it to `<tag without v>+<commit>`; a build
/// without it takes the same from `git describe`, so a local build of a tagged commit reports what the release
/// does. Commits after the tag add their count (`0.2.0+3.abc1234`); with no `v*` tag or no git it stays unset.
fn build_version(manifest: &Path) {
    println!("cargo:rerun-if-env-changed=OCD_BUILD_VERSION");
    if std::env::var_os("OCD_BUILD_VERSION").is_some() {
        return;
    }
    let git = |args: &[&str]| -> Option<String> {
        let out = Command::new("git").args(args).current_dir(manifest).output().ok()?;
        out.status.success().then(|| String::from_utf8_lossy(&out.stdout).trim().to_string())
    };
    // Describe again when HEAD moves (checkout or commit) or the tags change.
    let watch = |path: PathBuf| {
        if path.exists() {
            println!("cargo:rerun-if-changed={}", path.display());
        }
    };
    if let Some(dir) = git(&["rev-parse", "--absolute-git-dir"]) {
        watch(PathBuf::from(dir).join("HEAD"));
    }
    if let Some(common) = git(&["rev-parse", "--path-format=absolute", "--git-common-dir"]) {
        let common = PathBuf::from(common);
        if let Some(head) = git(&["symbolic-ref", "-q", "HEAD"]) {
            watch(common.join(head));
        }
        watch(common.join("packed-refs"));
        watch(common.join("refs/tags"));
    }
    let Some(described) = git(&["describe", "--tags", "--match", "v[0-9]*", "--long", "--abbrev=7"]) else {
        return;
    };
    // `v0.2.0-3-gabc1234`
    let mut parts = described.rsplitn(3, '-');
    let (Some(hash), Some(count), Some(tag)) = (parts.next(), parts.next(), parts.next()) else {
        return;
    };
    let (hash, tag) = (hash.strip_prefix('g').unwrap_or(hash), tag.strip_prefix('v').unwrap_or(tag));
    let version = if count == "0" { format!("{tag}+{hash}") } else { format!("{tag}+{count}.{hash}") };
    println!("cargo:rustc-env=OCD_BUILD_VERSION={version}");
}

/// The archive must decode: `[u32 LE indexLen][JSON {"path":[offset,size]}]`, entries within the body.
fn check(path: &Path) -> Result<(), String> {
    let data = std::fs::read(path).map_err(|e| e.to_string())?;
    if data.len() < 4 {
        return Err("truncated".into());
    }
    let len = u32::from_le_bytes([data[0], data[1], data[2], data[3]]) as usize;
    let body = 4usize.checked_add(len).filter(|&b| b <= data.len()).ok_or("index is truncated")?;
    let index: std::collections::HashMap<String, (usize, usize)> =
        serde_json::from_slice(&data[4..body]).map_err(|e| format!("bad index: {e}"))?;
    if index.values().any(|&(offset, size)| body + offset + size > data.len()) {
        return Err("entries exceed the archive".into());
    }
    Ok(())
}

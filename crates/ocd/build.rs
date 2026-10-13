//! Embeds the web UI archive (upstream `virtual:opencode-app-assets`) when one has been built.
//!
//! Source: `OCD_WEB_UI_ARCHIVE` (build-time env) or `web-ui/app-archive.bin` next to this file,
//! as written by `scripts/build-web-ui.sh`. The archive is not committed; without it the binary
//! embeds nothing and serves no web UI (like an upstream build made with `--skip-web-ui`).
use std::path::{Path, PathBuf};

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

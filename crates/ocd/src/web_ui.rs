//! Locating the web UI that `ocd serve` (all modes: foreground, `--stdio`, `--service`) serves for
//! non-API paths (upstream `packages/cli/src/app-assets.ts` + `services/web-ui.ts`).
//!
//! Upstream embeds the archive in the binary (`virtual:opencode-app-assets`); a build made with
//! `--skip-web-ui` embeds an empty one and answers 404 for every non-API path. ocd looks, in order:
//!
//! 1. `OCD_WEB_UI` (runtime env): an archive file or an unpacked `packages/app/dist` directory
//!    (upstream's local builds read `packages/app/dist`). A bad value is a startup error.
//! 2. The archive embedded at compile time (`crates/ocd/build.rs`, from
//!    `scripts/build-web-ui.sh`).
//! 3. `app-archive.bin` next to the executable.
//! 4. Nothing: an empty web UI (404s), like upstream `--skip-web-ui`.

use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use bytes::Bytes;
use oc_server::webui::{WebUi, WebUiSource};

mod embedded {
    include!(concat!(env!("OUT_DIR"), "/web_ui_archive.rs"));
}

/// Runtime override: archive file or asset directory.
pub const ENV: &str = "OCD_WEB_UI";
/// Archive file name looked up next to the executable.
pub const FILE: &str = "app-archive.bin";

/// Where the served assets came from.
#[derive(Debug, Clone, PartialEq)]
pub enum Origin {
    Env(PathBuf),
    Embedded,
    Beside(PathBuf),
    Missing,
}

/// `FSUtil.mimeType`: npm `mime-types` lookup, `application/octet-stream` when unknown.
fn mime(name: &str) -> String {
    oc_tools::mime::lookup(Path::new(name))
}

/// The web UI for this process.
pub fn load() -> anyhow::Result<WebUiSource> {
    let exe = std::env::current_exe().ok();
    let (ui, origin) = locate(std::env::var_os(ENV).as_deref(), embedded::ARCHIVE, exe.as_deref())?;
    tracing::debug!(?origin, assets = ui.len(), "web ui");
    Ok(WebUiSource::Loaded(Arc::new(ui)))
}

/// Resolve the web UI from an env override, the embedded archive and the executable's path.
pub fn locate(env: Option<&OsStr>, embedded: &'static [u8], exe: Option<&Path>) -> anyhow::Result<(WebUi, Origin)> {
    if let Some(value) = env.filter(|v| !v.is_empty()) {
        let path = PathBuf::from(value);
        let ui = WebUi::load(&path).map_err(|e| anyhow::anyhow!("{ENV}={}: {e:#}", path.display()))?;
        return Ok((ui.with_mime(mime), Origin::Env(path)));
    }
    if !embedded.is_empty() {
        match WebUi::from_archive(Bytes::from_static(embedded)) {
            Ok(ui) => return Ok((ui.with_mime(mime), Origin::Embedded)),
            Err(e) => tracing::warn!("embedded web UI archive is unreadable: {e:#}"),
        }
    }
    if let Some(path) = exe.and_then(Path::parent).map(|dir| dir.join(FILE)).filter(|p| p.is_file()) {
        match WebUi::load(&path) {
            Ok(ui) => return Ok((ui.with_mime(mime), Origin::Beside(path))),
            Err(e) => tracing::warn!("web UI archive {} is unreadable: {e:#}", path.display()),
        }
    }
    Ok((WebUi::empty().with_mime(mime), Origin::Missing))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn archive(entries: &[(&str, &[u8])]) -> Vec<u8> {
        let compressed: Vec<(&str, Vec<u8>)> = entries
            .iter()
            .map(|(k, v)| {
                let mut out = Vec::new();
                brotli::BrotliCompress(&mut &v[..], &mut out, &Default::default()).unwrap();
                (*k, out)
            })
            .collect();
        let refs: Vec<(&str, &[u8])> = compressed.iter().map(|(k, v)| (*k, v.as_slice())).collect();
        WebUi::encode_archive(&refs)
    }

    fn leak(v: Vec<u8>) -> &'static [u8] {
        Box::leak(v.into_boxed_slice())
    }

    #[test]
    fn embedded_archive_decodes() {
        // build.rs only embeds an archive whose index decodes; it holds the app shell.
        if !embedded::ARCHIVE.is_empty() {
            let ui = WebUi::from_archive(Bytes::from_static(embedded::ARCHIVE)).unwrap();
            assert!(ui.is_empty() || ui.contains("index.html"));
        }
    }

    #[test]
    fn nothing_found_serves_an_empty_ui() {
        let dir = tempfile::tempdir().unwrap();
        let (ui, origin) = locate(None, &[], Some(&dir.path().join("ocd"))).unwrap();
        assert_eq!(origin, Origin::Missing);
        assert!(ui.is_empty());
    }

    #[test]
    fn embedded_archive_wins_over_a_file_beside_the_binary() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(FILE), archive(&[("index.html", b"beside"), ("a.js", b"x")])).unwrap();
        let embedded = leak(archive(&[("index.html", b"embedded")]));
        let (ui, origin) = locate(None, embedded, Some(&dir.path().join("ocd"))).unwrap();
        assert_eq!(origin, Origin::Embedded);
        assert_eq!(ui.len(), 1);
        // An empty embedded archive (upstream `--skip-web-ui`) still counts as embedded.
        let (ui, origin) = locate(None, leak(WebUi::encode_archive(&[])), Some(&dir.path().join("ocd"))).unwrap();
        assert_eq!(origin, Origin::Embedded);
        assert!(ui.is_empty());
    }

    #[test]
    fn falls_back_to_the_archive_beside_the_binary() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join(FILE);
        std::fs::write(&file, archive(&[("index.html", b"beside"), ("_assets/a.js", b"x")])).unwrap();
        let (ui, origin) = locate(None, &[], Some(&dir.path().join("ocd"))).unwrap();
        assert_eq!(origin, Origin::Beside(file.clone()));
        assert!(ui.contains("_assets/a.js"));
        // An unreadable file is skipped with a warning.
        std::fs::write(&file, b"\xff\xff\xff\xff{").unwrap();
        let (_, origin) = locate(None, &[], Some(&dir.path().join("ocd"))).unwrap();
        assert_eq!(origin, Origin::Missing);
    }

    #[test]
    fn env_override_takes_an_archive_or_a_directory() {
        let dir = tempfile::tempdir().unwrap();
        let dist = dir.path().join("dist");
        std::fs::create_dir_all(dist.join("_assets")).unwrap();
        std::fs::write(dist.join("index.html"), "<html></html>").unwrap();
        std::fs::write(dist.join("_assets/a.js"), "x").unwrap();
        std::fs::write(dist.join("_assets/a.js.map"), "{}").unwrap();
        let embedded = leak(archive(&[("index.html", b"embedded")]));
        let (ui, origin) = locate(Some(dist.as_os_str()), embedded, None).unwrap();
        assert_eq!(origin, Origin::Env(dist.clone()));
        assert!(ui.contains("_assets/a.js"));
        assert!(!ui.contains("_assets/a.js.map"));

        let file = dir.path().join("ui.bin");
        std::fs::write(&file, archive(&[("index.html", b"env")])).unwrap();
        let (ui, origin) = locate(Some(file.as_os_str()), embedded, None).unwrap();
        assert_eq!(origin, Origin::Env(file));
        assert_eq!(ui.len(), 1);

        // An explicit override that cannot be read is an error, not a silent fallback.
        let missing = dir.path().join("missing.bin");
        let err = locate(Some(missing.as_os_str()), embedded, None).unwrap_err();
        assert!(err.to_string().starts_with(&format!("{ENV}={}", missing.display())), "{err}");
        // An empty value is ignored.
        let (_, origin) = locate(Some(OsStr::new("")), embedded, None).unwrap();
        assert_eq!(origin, Origin::Embedded);
    }
}

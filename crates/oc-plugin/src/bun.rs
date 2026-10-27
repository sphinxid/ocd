//! Bun runtime acquisition for the plugin sidecar.
//!
//! Resolution order:
//! 1. `OCD_BUN_PATH` (ocd's own override), then `OPENCODE_BUN_PATH` (upstream's name): used as is.
//! 2. Otherwise the pinned Bun [`VERSION`] release archive for this platform is downloaded from
//!    GitHub (`OCD_BUN_MIRROR` replaces `https://github.com/oven-sh/bun/releases/download`),
//!    verified against the pinned SHA-256 of the archive, and installed at
//!    `<Global.bin>/bun/<version>-<sha256[0..16]>/bun[.exe]` (0700 directories, atomic rename,
//!    re-verified on every use through a `.sha256` stamp of the executable).

use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};

/// Bun version upstream pins (`packageManager: bun@1.4.4`).
pub const VERSION: &str = "1.4.4";
pub const DEFAULT_MIRROR: &str = "https://github.com/oven-sh/bun/releases/download";

/// A release archive and its SHA-256 (from the release's `SHASUMS256.txt`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Archive {
    pub name: &'static str,
    pub sha256: &'static str,
}

/// Pinned archives of bun-v1.4.4.
pub const ARCHIVES: &[Archive] = &[
    Archive { name: "bun-darwin-aarch64", sha256: "c240d7f0ccb90c263a849fcdb09b7ee62630d921348ed6aafda86b9d577bc510" },
    Archive { name: "bun-darwin-x64", sha256: "50a3491f73abc2fe81673171a85762d3bc2cc1680ccecadd983de99adf77977e" },
    Archive { name: "bun-darwin-x64-baseline", sha256: "d4a155adfbfd09155eb3ba05c9e3a544f39297d7a68616d3c7759f67ab5c47dc" },
    Archive { name: "bun-linux-aarch64", sha256: "baa6bb36f51d6e58561e02bbea825038a9df454f673d6d776e8bbf86f8c7850e" },
    Archive { name: "bun-linux-aarch64-musl", sha256: "57f01b02ac7113c61c935f28f38c2eab849e3c970dab21f8b640a87b1ef65009" },
    Archive { name: "bun-linux-x64", sha256: "bf8ccbfb2b0b80159e0f9a6d15c55317add8077534dbc6130a3edb52fccb395c" },
    Archive { name: "bun-linux-x64-baseline", sha256: "e0446eb55995772eb9894bebae9a6be368d985a8b9271ee124486f9946eedd76" },
    Archive { name: "bun-linux-x64-musl", sha256: "a5a8ccc5e74dcb883fe974f125a7bdb3b41e5195ed1b870e73f17effd3eb2b2d" },
    Archive { name: "bun-linux-x64-musl-baseline", sha256: "6e9c37098b293920aed28bfb51ab240e26d23bfdd9bbfecb3ad428ac166549bc" },
    Archive { name: "bun-windows-aarch64", sha256: "2d57a0eec0cb44d95c918c3f9734f60455a1306ae5085d76fc68f44e3ee30d09" },
    Archive { name: "bun-windows-x64", sha256: "cd2c0fccc78f3ede1f50ca827a15f063eda13d4d150a3d0674f4f6e323d864a5" },
    Archive { name: "bun-windows-x64-baseline", sha256: "0f20ce6674d4f2dbcc8c5d59fdf99f7ba45f8c3b367e0f5781cbfa30a1c78d99" },
];

/// Archive name for a target (`os` / `arch` as in `std::env::consts`).
pub fn archive_name(os: &str, arch: &str, musl: bool, baseline: bool) -> Option<String> {
    let os = match os {
        "macos" | "darwin" => "darwin",
        "linux" => "linux",
        "windows" => "windows",
        _ => return None,
    };
    let arch = match arch {
        "aarch64" | "arm64" => "aarch64",
        "x86_64" | "x64" => "x64",
        _ => return None,
    };
    let mut name = format!("bun-{os}-{arch}");
    if os == "linux" && musl {
        name.push_str("-musl");
    }
    if arch == "x64" && baseline {
        name.push_str("-baseline");
    }
    Some(name)
}

fn baseline_cpu() -> bool {
    #[cfg(target_arch = "x86_64")]
    {
        !std::arch::is_x86_feature_detected!("avx2")
    }
    #[cfg(not(target_arch = "x86_64"))]
    {
        false
    }
}

/// The archive for the running platform.
pub fn current_archive() -> Option<&'static Archive> {
    let name = archive_name(std::env::consts::OS, std::env::consts::ARCH, cfg!(target_env = "musl"), baseline_cpu())?;
    ARCHIVES.iter().find(|a| a.name == name)
}

fn executable_name() -> &'static str {
    if cfg!(windows) { "bun.exe" } else { "bun" }
}

/// The env override, if any (`OCD_BUN_PATH`, then `OPENCODE_BUN_PATH`).
pub fn env_override() -> Option<PathBuf> {
    ["OCD_BUN_PATH", "OPENCODE_BUN_PATH"]
        .into_iter()
        .find_map(|key| std::env::var_os(key).filter(|v| !v.is_empty()))
        .map(PathBuf::from)
}

pub fn sha256_hex(bytes: &[u8]) -> String {
    hex::encode(Sha256::digest(bytes))
}

/// Resolves (downloading when needed) the Bun executable.
#[derive(Clone)]
pub struct BunResolver {
    /// `<cache>/bin` (`Global.bin`).
    pub bin: PathBuf,
    pub http: reqwest::Client,
    pub mirror: String,
}

impl BunResolver {
    pub fn new(bin: PathBuf) -> Self {
        let mirror = std::env::var("OCD_BUN_MIRROR")
            .ok()
            .filter(|v| !v.trim().is_empty())
            .unwrap_or_else(|| DEFAULT_MIRROR.to_string());
        Self { bin, http: oc_util::http::client(), mirror }
    }

    pub async fn resolve(&self) -> Result<PathBuf, String> {
        if let Some(path) = env_override() {
            return Ok(path);
        }
        let archive = current_archive().ok_or_else(|| {
            format!("No pinned Bun {VERSION} build for {}-{}; set OCD_BUN_PATH", std::env::consts::OS, std::env::consts::ARCH)
        })?;
        self.install(archive).await
    }

    pub fn install_dir(&self, archive: &Archive) -> PathBuf {
        self.bin.join("bun").join(format!("{VERSION}-{}", &archive.sha256[..16]))
    }

    /// Installs `archive` unless a verified copy is present.
    pub async fn install(&self, archive: &Archive) -> Result<PathBuf, String> {
        let directory = self.install_dir(archive);
        let destination = directory.join(executable_name());
        if verified(&destination) {
            return Ok(destination);
        }
        let url = format!("{}/bun-v{VERSION}/{}.zip", self.mirror.trim_end_matches('/'), archive.name);
        tracing::info!(url = %url, "downloading bun for the plugin host");
        let response = self.http.get(&url).send().await.map_err(|e| format!("Failed to download Bun: {e}"))?;
        if !response.status().is_success() {
            return Err(format!("Failed to download Bun: HTTP {} ({url})", response.status().as_u16()));
        }
        let bytes = response.bytes().await.map_err(|e| format!("Failed to download Bun: {e}"))?;
        if sha256_hex(&bytes) != archive.sha256 {
            return Err(format!("Downloaded Bun archive checksum mismatch: {url}"));
        }
        let entry = format!("{}/{}", archive.name, executable_name());
        let executable = crate::zip::extract(&bytes, &entry)?.ok_or_else(|| format!("Bun archive did not contain {entry}"))?;
        install_bytes(&directory, &destination, &executable)?;
        Ok(destination)
    }
}

fn stamp(path: &Path) -> PathBuf {
    path.with_extension("sha256")
}

/// The executable exists and still matches the digest recorded when it was installed.
fn verified(path: &Path) -> bool {
    let (Ok(bytes), Ok(expected)) = (std::fs::read(path), std::fs::read_to_string(stamp(path))) else { return false };
    sha256_hex(&bytes) == expected.trim()
}

fn private_dir(path: &Path) -> Result<(), String> {
    let mut builder = std::fs::DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(0o700);
    }
    builder.create(path).map_err(|e| format!("{}: {e}", path.display()))
}

/// Writes an executable atomically together with its digest stamp.
pub fn install_bytes(directory: &Path, destination: &Path, bytes: &[u8]) -> Result<(), String> {
    private_dir(directory)?;
    let temporary = directory.join(format!(".{}.{}.tmp", std::process::id(), rand::random::<u64>()));
    let result = (|| -> Result<(), String> {
        std::fs::write(&temporary, bytes).map_err(|e| e.to_string())?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&temporary, std::fs::Permissions::from_mode(0o755)).map_err(|e| e.to_string())?;
        }
        std::fs::rename(&temporary, destination).map_err(|e| e.to_string())?;
        std::fs::write(stamp(destination), sha256_hex(bytes)).map_err(|e| e.to_string())
    })();
    let _ = std::fs::remove_file(&temporary);
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn archive_names() {
        assert_eq!(archive_name("linux", "x86_64", false, false).as_deref(), Some("bun-linux-x64"));
        assert_eq!(archive_name("linux", "x86_64", true, true).as_deref(), Some("bun-linux-x64-musl-baseline"));
        assert_eq!(archive_name("macos", "aarch64", false, true).as_deref(), Some("bun-darwin-aarch64"));
        assert_eq!(archive_name("windows", "x86_64", false, false).as_deref(), Some("bun-windows-x64"));
        assert_eq!(archive_name("freebsd", "x86_64", false, false), None);
        for name in ["bun-linux-x64", "bun-darwin-aarch64", "bun-windows-x64", "bun-linux-aarch64-musl"] {
            assert!(ARCHIVES.iter().any(|a| a.name == name), "{name}");
        }
        assert!(ARCHIVES.iter().all(|a| a.sha256.len() == 64));
    }

    #[test]
    fn install_and_verify() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("x").join("bun");
        install_bytes(target.parent().unwrap(), &target, b"binary").unwrap();
        assert!(verified(&target));
        std::fs::write(&target, b"tampered").unwrap();
        assert!(!verified(&target));
    }
}

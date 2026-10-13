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

/// Bun version upstream pins (`packageManager: bun@1.4.2`).
pub const VERSION: &str = "1.4.2";
pub const DEFAULT_MIRROR: &str = "https://github.com/oven-sh/bun/releases/download";

/// A release archive and its SHA-256 (from the release's `SHASUMS256.txt`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Archive {
    pub name: &'static str,
    pub sha256: &'static str,
}

/// Pinned archives of bun-v1.4.2.
pub const ARCHIVES: &[Archive] = &[
    Archive { name: "bun-darwin-aarch64", sha256: "90987a3a16d7db556d886ac3d551e7b6d3edf0a1cf43acaed622e8676be1d12f" },
    Archive { name: "bun-darwin-x64", sha256: "80520d7e17526308c9185d261679ac6d27798d3803a0e9f7ff9121ab8affb012" },
    Archive { name: "bun-darwin-x64-baseline", sha256: "bad5bbd6cf14d0980d115f5954c9ff904df619d5e994d2da1ffccd3f316300b0" },
    Archive { name: "bun-linux-aarch64", sha256: "54328bbc2d9c8e0c9f892c544d66c57a83b84139e34909e5ee81758f1ac8fda7" },
    Archive { name: "bun-linux-aarch64-musl", sha256: "71760b6c8ea30623b81a4907cb815d48e2ea266f2e73e751534a44a0607950df" },
    Archive { name: "bun-linux-x64", sha256: "36368faef7527875d5ffa52e53cd48021741f2a83eb6208a8dd64068d422a913" },
    Archive { name: "bun-linux-x64-baseline", sha256: "c678040f14fe0440eb839d37cbd0ce4c051a32da72806ac97de6a6aab6bf728f" },
    Archive { name: "bun-linux-x64-musl", sha256: "4835eca59d6da70f4674f5642f6e459dcadab773695b2ed9922d131057989742" },
    Archive { name: "bun-linux-x64-musl-baseline", sha256: "76e1db84e98f22f78de0a87e309bfbbf297732847f9720db36750646c85c8c18" },
    Archive { name: "bun-windows-aarch64", sha256: "a7a16b876a305fd1029c66dbd27007b4f6112ae896532f675878731a21e50cfd" },
    Archive { name: "bun-windows-x64", sha256: "ce4c17497b2f29712a99d3d53f028de28cd42e3bacb8589599e7f000e49b6405" },
    Archive { name: "bun-windows-x64-baseline", sha256: "78c221c2376f79731ccf4e4af0b3bb46d81fefa3296c5abee09ad8a1b21e68c6" },
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

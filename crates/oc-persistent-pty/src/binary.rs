//! `opencode-pty` executable resolution (`persistent-pty/binary.bun.ts`).
//!
//! Upstream's compiled build embeds the platform binary from npm `@opencode/pty-<suffix>@0.3.3`
//! as `{path, version, sha256}` and installs it into
//! `<global.bin>/opencode-pty/<version>-<sha256[0..16]>/opencode-pty[.exe]`. The Rust port cannot embed
//! it, so the same bytes are fetched from the npm registry tarball (pinned sha512 `integrity`
//! and pinned binary sha256) and installed with exactly the same layout and validation.
//! `OPENCODE_PTY_BIN` overrides everything; unsupported platforms fall back to `opencode-pty`
//! on `PATH`. Since 0.3.1 Windows has binaries too (`opencode-pty.exe`, served over named pipes).

use std::io::Read;
use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256, Sha512};

/// `available`: `opencode-pty` ships binaries for every platform since 0.3.1.
pub const AVAILABLE: bool = true;

/// `available` (`true` since 2.1).
pub fn available() -> bool {
    AVAILABLE
}

/// Pinned `@opencode/pty` version (`packages/core/package.json:108`; the package moved from the
/// `@opencode-ai` scope with 0.3.3).
pub const VERSION: &str = "0.3.3";
pub const DEFAULT_REGISTRY: &str = "https://registry.npmjs.org";
/// Name of the executable (Windows only launches executables with their extension).
pub const EXECUTABLE: &str = if cfg!(windows) { "opencode-pty.exe" } else { "opencode-pty" };
/// Path of the executable inside this platform's package tarball.
pub const TARBALL_ENTRY: &str = if cfg!(windows) { "package/bin/opencode-pty.exe" } else { "package/bin/opencode-pty" };

/// One `@opencode/pty-<suffix>@0.3.3` platform package.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PlatformPackage {
    pub suffix: &'static str,
    /// npm `dist.integrity` of the tarball.
    pub integrity: &'static str,
    /// sha256 of `bin/opencode-pty[.exe]` (what upstream's build embeds as `sha256`).
    pub sha256: &'static str,
}

/// `bun.lock` integrity of each 0.3.3 tarball and the sha256 of its installed binary.
pub const PACKAGES: &[PlatformPackage] = &[
    PlatformPackage {
        suffix: "darwin-arm64",
        integrity: "sha512-1q9sROkkbGQaG2rxyKT6hW/EN1aDUiST8h6WmQdt1dbgvo3WaYDDjaEWmgwhm7Fhhca+E9WruTfxfVBBDm4Ezg==",
        sha256: "6d557e7eb38a24c3c24e822e141de0c7d8dc38aa3ee4d86a0892377e02bcb922",
    },
    PlatformPackage {
        suffix: "darwin-x64",
        integrity: "sha512-dfphCXfoaXGAa1i2P52U7QelcJoANTWimCvuy8xPNPlazrb7ANP+flzQxcy7f6rP/hnbzGLYUMxU0CQocMjwqA==",
        sha256: "ee28fbb73cdca800be11e5a382d8fcb3bac7c2d7ddc475dda4a292f67f65bafb",
    },
    PlatformPackage {
        suffix: "linux-arm64-gnu",
        integrity: "sha512-MyO+3wIkv6hveEY6Jyd7jgk9PRp1J4y+u0Y5FfUg1SPFUhrFTx9yrHXbSDVbAe7mLmMYu8KhRn4+plk8JXZjyg==",
        sha256: "9c17168b174c720aaee2b65239ed2057e54e885e0df513302ccc9ee5c0aa6a5c",
    },
    PlatformPackage {
        suffix: "linux-arm64-musl",
        integrity: "sha512-L0+x0xald7PKwYcckE8NOFef/FpGQ2k13iunGlHJFIN1gxDpI1XtZQEjASIe4w3vq1nB49L7SnbghzMxUKNtOg==",
        sha256: "5338ed1696942d92f56f5dceeae277e177f875ac703446411fa34609ee4d78c1",
    },
    PlatformPackage {
        suffix: "linux-x64-gnu",
        integrity: "sha512-j1nPB/8nwrJm20s5Gz9Q/8qPQyYowkQNXeLUaz2bilvB5L1Wl8OxOSk39WpcEZYp4gkBNsuguNm1zSZ+++RsKQ==",
        sha256: "375334d9e20ffd7f7cd111eaaed7802d87ce13c9c15608ec42942b2b5da2e1b7",
    },
    PlatformPackage {
        suffix: "linux-x64-musl",
        integrity: "sha512-dNPmBprzTiM0u18YBsvC4hXRwi2/SGDbvaZvyfr9VooaC/49YIkKbz9hOVzIry8Q8uaWvXagCiYSQwIdPIfSeA==",
        sha256: "713933d2e69dc0a03a9bd84dbde88e2826f5d6001eee518ff85be4d0b5a0cf7f",
    },
    PlatformPackage {
        suffix: "win32-arm64",
        integrity: "sha512-1v2e97XLZIZein/E7WGyfHOcB1S71pgKp7yq6yCe3OtbDtm5WFnp9mQB+jU2ns1d4ATTp2bJauh7rHFsqaJgeA==",
        sha256: "713b453733994e2c99d69a023e8a15d4f7eb43a2d2c92ba85f052751d6b73ce1",
    },
    PlatformPackage {
        suffix: "win32-x64",
        integrity: "sha512-V7B4i/2gQxqHNBVt9q1EpJhIHX+UgPJxle6+XzAkUgwOo/75ncUUbn0hsqBy/tqVMhDSA4n5BHQ32U+X7aYI0w==",
        sha256: "99181ba38119fa912852ee6bb3884acb434ef778030dc7adc12ba3507407e98b",
    },
];

/// Package suffix for a target (`resolveOpencodePty`, `cli/script/opencode-pty.ts:21-29`):
/// darwin/linux/win32 × arm64/x64, Linux adds `-gnu`/`-musl`.
pub fn suffix_for(os: &str, arch: &str, musl: bool) -> Option<String> {
    let platform = match os {
        "macos" | "darwin" => "darwin",
        "linux" => "linux",
        "windows" | "win32" => "win32",
        _ => return None,
    };
    let arch = match arch {
        "aarch64" | "arm64" => "arm64",
        "x86_64" | "x64" => "x64",
        _ => return None,
    };
    Some(if platform == "linux" {
        format!("{platform}-{arch}-{}", if musl { "musl" } else { "gnu" })
    } else {
        format!("{platform}-{arch}")
    })
}

/// The platform package for this build target (like upstream, chosen per build target).
pub fn current_package() -> Option<&'static PlatformPackage> {
    let suffix = suffix_for(std::env::consts::OS, std::env::consts::ARCH, cfg!(target_env = "musl"))?;
    PACKAGES.iter().find(|p| p.suffix == suffix)
}

/// `<registry>/@opencode/pty-<suffix>/-/pty-<suffix>-<version>.tgz`.
pub fn tarball_url(registry: &str, suffix: &str, version: &str) -> String {
    format!("{}/@opencode/pty-{suffix}/-/pty-{suffix}-{version}.tgz", registry.trim_end_matches('/'))
}

/// npm `integrity` string (`sha512-<base64>`) of some bytes.
pub fn integrity_of(bytes: &[u8]) -> String {
    use base64::Engine;
    format!("sha512-{}", base64::engine::general_purpose::STANDARD.encode(Sha512::digest(bytes)))
}

pub fn sha256_hex(bytes: &[u8]) -> String {
    hex::encode(Sha256::digest(bytes))
}

/// Install directory for a binary (`<bin>/opencode-pty/<version>-<sha256[0..16]>/opencode-pty[.exe]`).
pub fn install_path(bin: &Path, version: &str, sha256: &str) -> PathBuf {
    bin.join("opencode-pty").join(format!("{version}-{}", &sha256[..16.min(sha256.len())])).join(EXECUTABLE)
}

/// Resolves (and if needed downloads + installs) the daemon executable.
#[derive(Clone)]
pub struct BinaryResolver {
    /// `<cache>/bin` (`Global.bin`).
    pub bin: PathBuf,
    pub http: reqwest::Client,
    /// npm registry base; defaults to `$npm_config_registry` or the public registry.
    pub registry: String,
}

impl BinaryResolver {
    pub fn new(bin: PathBuf, http: reqwest::Client) -> Self {
        let registry = std::env::var("npm_config_registry")
            .ok()
            .filter(|v| !v.trim().is_empty())
            .unwrap_or_else(|| DEFAULT_REGISTRY.to_string());
        Self { bin, http, registry }
    }

    /// `resolveBinary(bin)`.
    pub async fn resolve(&self) -> Result<String, String> {
        if let Some(path) = std::env::var("OPENCODE_PTY_BIN").ok().filter(|v| !v.is_empty()) {
            return Ok(path);
        }
        if !AVAILABLE {
            return Ok("opencode-pty".into());
        }
        let Some(package) = current_package() else { return Ok("opencode-pty".into()) };
        self.install(package).await.map(|p| p.to_string_lossy().into_owned())
    }

    /// `install(bin, {version, sha256})`, fetching the bytes from npm when not cached.
    pub async fn install(&self, package: &PlatformPackage) -> Result<PathBuf, String> {
        let root = self.bin.join("opencode-pty");
        private_directory(&root)?;
        let directory = root.join(format!("{VERSION}-{}", &package.sha256[..16]));
        private_directory(&directory)?;
        let destination = directory.join(EXECUTABLE);
        if validate_if_present(&destination, package.sha256)? {
            return Ok(destination);
        }
        let bytes = self.download(package).await?;
        if sha256_hex(&bytes) != package.sha256 {
            return Err("Downloaded opencode-pty checksum mismatch".into());
        }
        write_atomic(&directory, &destination, &bytes, package.sha256)?;
        validate(&destination, package.sha256)
    }

    /// Download the platform tarball, verify its integrity, and extract `bin/opencode-pty[.exe]`.
    pub async fn download(&self, package: &PlatformPackage) -> Result<Vec<u8>, String> {
        let url = tarball_url(&self.registry, package.suffix, VERSION);
        tracing::info!(url = %url, "downloading opencode-pty");
        let response = self.http.get(&url).send().await.map_err(|e| e.to_string())?;
        if !response.status().is_success() {
            return Err(format!("StatusCode: non 2xx status code ({} GET {url})", response.status().as_u16()));
        }
        let tarball = response.bytes().await.map_err(|e| e.to_string())?;
        if integrity_of(&tarball) != package.integrity {
            return Err(format!("opencode-pty package integrity mismatch: {url}"));
        }
        extract_entry(&tarball, TARBALL_ENTRY)?.ok_or_else(|| format!("opencode-pty package did not contain {TARBALL_ENTRY}"))
    }
}

fn write_atomic(directory: &Path, destination: &Path, bytes: &[u8], sha256: &str) -> Result<(), String> {
    use std::io::Write;
    let temporary = directory.join(format!("opencode-pty.{}.{}.tmp", std::process::id(), uuid::Uuid::new_v4()));
    let result = (|| -> Result<(), String> {
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o700);
        }
        let mut file = options.open(&temporary).map_err(|e| e.to_string())?;
        file.write_all(bytes).map_err(|e| e.to_string())?;
        file.sync_all().map_err(|e| e.to_string())?;
        drop(file);
        set_mode(&temporary, 0o755)?;
        if let Err(error) = std::fs::rename(&temporary, destination)
            && !validate_if_present(destination, sha256)? {
                return Err(error.to_string());
            }
        Ok(())
    })();
    let _ = std::fs::remove_file(&temporary);
    result
}

#[cfg(unix)]
fn set_mode(path: &Path, mode: u32) -> Result<(), String> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)).map_err(|e| e.to_string())
}

#[cfg(not(unix))]
fn set_mode(_path: &Path, _mode: u32) -> Result<(), String> {
    Ok(())
}

#[cfg(unix)]
fn owned_by_other(meta: &std::fs::Metadata) -> bool {
    use std::os::unix::fs::MetadataExt;
    // SAFETY: getuid has no preconditions.
    meta.uid() != unsafe { libc::getuid() }
}

#[cfg(not(unix))]
fn owned_by_other(_meta: &std::fs::Metadata) -> bool {
    false
}

/// `privateDirectory`.
pub fn private_directory(directory: &Path) -> Result<(), String> {
    let mut builder = std::fs::DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(0o700);
    }
    builder.create(directory).map_err(|e| e.to_string())?;
    let info = std::fs::symlink_metadata(directory).map_err(|e| e.to_string())?;
    if !info.is_dir() || info.file_type().is_symlink() {
        return Err(format!("Unsafe opencode-pty directory: {}", directory.display()));
    }
    if owned_by_other(&info) {
        return Err(format!("opencode-pty directory is owned by another user: {}", directory.display()));
    }
    set_mode(directory, 0o700)
}

/// `validateIfPresent`: `Ok(false)` when missing.
pub fn validate_if_present(file: &Path, expected: &str) -> Result<bool, String> {
    match std::fs::symlink_metadata(file) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
        _ => validate(file, expected).map(|_| true),
    }
}

/// `validate`: regular non-symlink file owned by us with the expected sha256; chmod 755.
pub fn validate(file: &Path, expected: &str) -> Result<PathBuf, String> {
    let info = std::fs::symlink_metadata(file).map_err(|e| e.to_string())?;
    if !info.is_file() || info.file_type().is_symlink() {
        return Err(format!("Unsafe opencode-pty executable: {}", file.display()));
    }
    if owned_by_other(&info) {
        return Err(format!("opencode-pty executable is owned by another user: {}", file.display()));
    }
    let bytes = std::fs::read(file).map_err(|e| e.to_string())?;
    if sha256_hex(&bytes) != expected {
        return Err(format!("Cached opencode-pty checksum mismatch: {}", file.display()));
    }
    set_mode(file, 0o755)?;
    Ok(file.to_path_buf())
}

/// Gunzip a `.tgz` and return the contents of `entry` (exact path match).
pub fn extract_entry(tgz: &[u8], entry: &str) -> Result<Option<Vec<u8>>, String> {
    let mut tar = Vec::new();
    flate2::read::GzDecoder::new(tgz).read_to_end(&mut tar).map_err(|e| format!("invalid opencode-pty package: {e}"))?;
    Ok(tar_entries(&tar)?.into_iter().find(|(name, _)| name == entry).map(|(_, data)| data.to_vec()))
}

fn parse_octal(field: &[u8]) -> Result<u64, String> {
    // GNU base-256 encoding for large sizes.
    if field.first().is_some_and(|b| b & 0x80 != 0) {
        let mut value: u64 = (field[0] & 0x7f) as u64;
        for b in &field[1..] {
            value = value.checked_mul(256).ok_or("tar size overflow")? + *b as u64;
        }
        return Ok(value);
    }
    let text: String = field.iter().take_while(|b| **b != 0).map(|b| *b as char).collect();
    let text = text.trim();
    if text.is_empty() {
        return Ok(0);
    }
    u64::from_str_radix(text, 8).map_err(|_| "invalid tar header".to_string())
}

fn cstr(field: &[u8]) -> String {
    let end = field.iter().position(|b| *b == 0).unwrap_or(field.len());
    String::from_utf8_lossy(&field[..end]).into_owned()
}

/// Minimal ustar/pax/GNU reader: `(path, data)` of regular files.
pub fn tar_entries(tar: &[u8]) -> Result<Vec<(String, &[u8])>, String> {
    let mut out = Vec::new();
    let mut offset = 0usize;
    let mut long_name: Option<String> = None;
    let mut pax_path: Option<String> = None;
    while offset + 512 <= tar.len() {
        let header = &tar[offset..offset + 512];
        if header.iter().all(|b| *b == 0) {
            break;
        }
        let size = parse_octal(&header[124..136])? as usize;
        let kind = header[156];
        let data_start = offset + 512;
        let data_end = data_start.checked_add(size).filter(|e| *e <= tar.len()).ok_or("truncated tar entry")?;
        let data = &tar[data_start..data_end];
        let mut name = cstr(&header[0..100]);
        if &header[257..262] == b"ustar" {
            let prefix = cstr(&header[345..500]);
            if !prefix.is_empty() {
                name = format!("{prefix}/{name}");
            }
        }
        match kind {
            b'L' => long_name = Some(cstr(data)),
            b'x' => {
                let text = String::from_utf8_lossy(data);
                for record in text.split('\n') {
                    if let Some((_, kv)) = record.split_once(' ')
                        && let Some(path) = kv.strip_prefix("path=") {
                            pax_path = Some(path.to_string());
                        }
                }
            }
            b'g' => {}
            b'0' | 0 | b'7' => {
                let path = pax_path.take().or_else(|| long_name.take()).unwrap_or(name);
                out.push((path, data));
            }
            _ => {
                pax_path = None;
                long_name = None;
            }
        }
        offset = data_start + size.div_ceil(512) * 512;
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tar_header(name: &str, size: usize, kind: u8) -> Vec<u8> {
        let mut h = vec![0u8; 512];
        h[..name.len()].copy_from_slice(name.as_bytes());
        let size = format!("{size:011o}\0");
        h[124..136].copy_from_slice(size.as_bytes());
        h[156] = kind;
        h[257..263].copy_from_slice(b"ustar\0");
        h
    }

    fn tar_with(entries: &[(&str, &[u8])]) -> Vec<u8> {
        let mut tar = Vec::new();
        for (name, data) in entries {
            tar.extend(tar_header(name, data.len(), b'0'));
            tar.extend_from_slice(data);
            tar.resize(tar.len().div_ceil(512) * 512, 0);
        }
        tar.extend(vec![0u8; 1024]);
        tar
    }

    #[test]
    fn suffixes() {
        assert_eq!(suffix_for("linux", "x86_64", false).as_deref(), Some("linux-x64-gnu"));
        assert_eq!(suffix_for("linux", "aarch64", true).as_deref(), Some("linux-arm64-musl"));
        assert_eq!(suffix_for("macos", "aarch64", false).as_deref(), Some("darwin-arm64"));
        assert_eq!(suffix_for("macos", "x86_64", true).as_deref(), Some("darwin-x64"));
        assert_eq!(suffix_for("windows", "x86_64", false).as_deref(), Some("win32-x64"));
        assert_eq!(suffix_for("windows", "aarch64", false).as_deref(), Some("win32-arm64"));
        assert_eq!(suffix_for("freebsd", "x86_64", false), None);
        assert_eq!(suffix_for("linux", "riscv64", false), None);
        for p in PACKAGES {
            assert!(p.integrity.starts_with("sha512-"));
            assert_eq!(p.sha256.len(), 64);
        }
    }

    #[test]
    fn urls_and_paths() {
        assert_eq!(
            tarball_url("https://registry.npmjs.org/", "linux-x64-gnu", VERSION),
            "https://registry.npmjs.org/@opencode/pty-linux-x64-gnu/-/pty-linux-x64-gnu-0.3.3.tgz"
        );
        assert_eq!(
            install_path(Path::new("/c/bin"), VERSION, PACKAGES[4].sha256),
            PathBuf::from("/c/bin/opencode-pty/0.3.3-375334d9e20ffd7f").join(EXECUTABLE)
        );
        assert_eq!(PACKAGES.len(), 8);
    }

    #[test]
    fn integrity_format() {
        assert_eq!(
            integrity_of(b""),
            "sha512-z4PhNX7vuL3xVChQ1m2AB9Yg5AULVxXcg/SpIdNs6c5H0NE8XYXysP+DGNKHfuwvY7kxvUdBeoGlODJ6+SfaPg=="
        );
    }

    #[test]
    fn tar_extraction() {
        let tar = tar_with(&[("package/LICENSE", b"MIT"), (TARBALL_ENTRY, b"\x7fELFbinary")]);
        let entries = tar_entries(&tar).unwrap();
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[1].0, TARBALL_ENTRY);
        let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
        std::io::Write::write_all(&mut gz, &tar).unwrap();
        let tgz = gz.finish().unwrap();
        assert_eq!(extract_entry(&tgz, TARBALL_ENTRY).unwrap().unwrap(), b"\x7fELFbinary");
        assert_eq!(extract_entry(&tgz, "package/missing").unwrap(), None);
        assert!(extract_entry(b"not gzip", TARBALL_ENTRY).is_err());
    }

    #[test]
    fn tar_long_names() {
        let long = format!("package/{}/file", "d".repeat(120));
        let mut tar = Vec::new();
        tar.extend(tar_header("././@LongLink", long.len() + 1, b'L'));
        let mut name = long.clone().into_bytes();
        name.push(0);
        tar.extend_from_slice(&name);
        tar.resize(tar.len().div_ceil(512) * 512, 0);
        tar.extend(tar_header("truncated", 2, b'0'));
        tar.extend_from_slice(b"hi");
        tar.resize(tar.len().div_ceil(512) * 512, 0);
        let entries = tar_entries(&tar).unwrap();
        assert_eq!(entries, vec![(long, &b"hi"[..])]);
    }

    #[cfg(unix)]
    #[test]
    fn install_layout_and_validation() {
        let root = std::env::temp_dir().join(format!("oc-pty-bin-{}", uuid::Uuid::new_v4()));
        let bytes = b"#!/bin/sh\necho hi\n";
        let sha = sha256_hex(bytes);
        let directory = root.join("opencode-pty").join(format!("{VERSION}-{}", &sha[..16]));
        private_directory(&directory).unwrap();
        let destination = directory.join("opencode-pty");
        assert!(!validate_if_present(&destination, &sha).unwrap());
        write_atomic(&directory, &destination, bytes, &sha).unwrap();
        assert!(validate_if_present(&destination, &sha).unwrap());
        assert_eq!(destination, install_path(&root, VERSION, &sha));
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(std::fs::metadata(&destination).unwrap().permissions().mode() & 0o777, 0o755);
        assert_eq!(std::fs::metadata(&directory).unwrap().permissions().mode() & 0o777, 0o700);
        let err = validate(&destination, &"0".repeat(64)).unwrap_err();
        assert_eq!(err, format!("Cached opencode-pty checksum mismatch: {}", destination.display()));
        // Symlinked directories are refused.
        let link = root.join("link");
        std::os::unix::fs::symlink(&directory, &link).unwrap();
        assert_eq!(private_directory(&link).unwrap_err(), format!("Unsafe opencode-pty directory: {}", link.display()));
        let _ = std::fs::remove_dir_all(&root);
    }
}

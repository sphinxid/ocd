//! Unit tests for `release.rs`, against the local GitHub stand-in of `tests/common/releases.rs`.

use super::*;

#[path = "../../tests/common/releases.rs"]
#[allow(dead_code)]
mod mock;

use mock::{MockGitHub, Publish, Reply};

fn v(s: &str) -> Version {
    Version::parse(s).unwrap_or_else(|| panic!("{s} is a version"))
}

fn source(api: &str, build: Option<&str>) -> Source {
    Source { api: api.to_string(), token: None, installed: Installed::from_build(build), disabled: false, exe: None }
}

fn global(root: &Path) -> Global {
    let g = |d: &str| root.join(d);
    Global {
        home: g("home"),
        data: g("data"),
        cache: g("cache"),
        config: g("config"),
        state: g("state"),
        tmp: g("tmp"),
        bin: g("cache/bin"),
        log: g("data/log"),
        repos: g("data/repos"),
    }
}

#[test]
fn versions_parse_like_upstream() {
    assert_eq!(v("1.2.3"), Version { major: 1, minor: 2, patch: 3, pre: vec![] });
    assert_eq!(v("v1.2.3"), v("1.2.3"));
    assert_eq!(v(" v0.2.0+abc1234 "), v("0.2.0"));
    assert_eq!(v("1.0.0-beta.2").pre, vec!["beta", "2"]);
    for bad in ["", "1.2", "1.2.3.4", "01.2.3", "1.2.3-", "1.2.3-01", "1.2.3+", "x1.2.3", "20261027.124153", "1.2.3-a..b"] {
        assert_eq!(Version::parse(bad), None, "{bad:?}");
    }
    assert_eq!(v("1.2.3-rc.1").to_string(), "1.2.3-rc.1");
}

#[test]
fn versions_order_like_semver() {
    let ordered = ["0.1.0", "0.1.1", "0.2.0-alpha", "0.2.0-alpha.1", "0.2.0-alpha.beta", "0.2.0-beta.2", "0.2.0-beta.11", "0.2.0", "0.10.0", "1.0.0"];
    for pair in ordered.windows(2) {
        assert!(v(pair[0]) < v(pair[1]), "{} < {}", pair[0], pair[1]);
    }
    assert_eq!(v("1.0.0+a").cmp(&v("1.0.0+b")), std::cmp::Ordering::Equal);
}

#[test]
fn installed_builds() {
    assert_eq!(Installed::from_build(Some("0.2.0+abc1234")), Installed::Release { version: v("0.2.0"), build: "0.2.0+abc1234".into() });
    // Commits after the tag count as the tag.
    assert_eq!(Installed::from_build(Some("0.2.0+3.abc1234")), Installed::Release { version: v("0.2.0"), build: "0.2.0+3.abc1234".into() });
    assert_eq!(Installed::from_build(Some("20261027.124153+abc1234")), Installed::Local { build: Some("20261027.124153+abc1234".into()) });
    assert_eq!(Installed::from_build(None), Installed::Local { build: None });
    assert_eq!(Installed::from_build(Some("")), Installed::Local { build: None });
}

#[test]
fn checksums_from_sha256sum_output() {
    let sums = "ab8913f08969fe3e250a2cdbfaa29478da069fbce9b642a7320278ae8ed7f939  ocd-v0.1.1-darwin-arm64.tar.gz\n\
                A16D2D909E919CAC4564A968A469E98855F6CB2CB45BE27428AFA9BEA178B3DE *ocd-v0.1.1-linux-x64.tar.gz\n\
                short  ocd-v0.1.1-windows-x64.zip\n";
    assert_eq!(
        checksum_for(sums, "ocd-v0.1.1-linux-x64.tar.gz").as_deref(),
        Some("a16d2d909e919cac4564a968a469e98855f6cb2cb45be27428afa9bea178b3de")
    );
    assert_eq!(checksum_for(sums, "ocd-v0.1.1-darwin-arm64.tar.gz").map(|s| s.len()), Some(64));
    assert_eq!(checksum_for(sums, "ocd-v0.1.1-windows-x64.zip"), None);
    assert_eq!(checksum_for(sums, "ocd-v0.1.1-linux-x64.tar"), None);
}

#[test]
fn archive_and_entry_names() {
    assert_eq!(archive_name("v0.2.0", "linux-x64"), "ocd-v0.2.0-linux-x64.tar.gz");
    assert_eq!(archive_name("v0.2.0", "windows-x64"), "ocd-v0.2.0-windows-x64.zip");
    assert_eq!(binary_entry("v0.2.0", "darwin-arm64"), "ocd-v0.2.0-darwin-arm64/ocd");
    assert_eq!(binary_entry("v0.2.0", "windows-x64"), "ocd-v0.2.0-windows-x64/ocd.exe");
    assert_eq!(tag_of("0.2.0"), "v0.2.0");
    assert_eq!(tag_of("v0.2.0"), "v0.2.0");
}

#[test]
fn extracts_the_binary_from_tar_gz_and_zip() {
    for platform in ["linux-x64", "darwin-arm64", "windows-x64"] {
        let archive = mock::archive("v0.2.0", platform, b"BINARY");
        assert_eq!(extract(&archive, "v0.2.0", platform).unwrap(), b"BINARY", "{platform}");
        let err = extract(&archive, "v0.3.0", platform).unwrap_err();
        assert!(err.contains("is missing from the archive"), "{err}");
    }
    assert!(extract(b"not an archive", "v0.2.0", "linux-x64").unwrap_err().starts_with("not a gzip archive"));
}

/// The real v0.1.1 Linux archive and its `SHA256SUMS`, when downloaded to `$OCD_SCRATCH/releases/v0.1.1/`.
#[test]
fn real_release_archive() {
    let Some(dir) = std::env::var_os("OCD_SCRATCH").map(|s| PathBuf::from(s).join("releases/v0.1.1")) else { return };
    let (Ok(archive), Ok(sums)) = (std::fs::read(dir.join("ocd-v0.1.1-linux-x64.tar.gz")), std::fs::read_to_string(dir.join("SHA256SUMS")))
    else {
        eprintln!("skipping: no release archive in {}", dir.display());
        return;
    };
    let expected = checksum_for(&sums, "ocd-v0.1.1-linux-x64.tar.gz").unwrap();
    assert_eq!(sha256_hex(&archive), expected);
    let binary = extract(&archive, "v0.1.1", "linux-x64").unwrap();
    assert_eq!(&binary[..4], b"\x7fELF");
    assert!(binary.len() > 10_000_000);
}

#[test]
fn errors_format_like_upstream() {
    let e = UpgradeError {
        title: "Could not check for ocd updates".into(),
        detail: "error sending request".into(),
        command: Some("ocd upgrade 0.2.0".into()),
        retry: "Check your network, then run ocd upgrade again.".into(),
    };
    assert_eq!(
        format_error(&e, 80),
        "Could not check for ocd updates\n\nerror sending request\n\nCommand  ocd upgrade 0.2.0\nRetry    Check your network, then run ocd upgrade again."
    );
    let long = UpgradeError { command: None, retry: "word ".repeat(20), ..e };
    let text = format_error(&long, 40);
    let retry: Vec<&str> = text.lines().skip(3).collect();
    assert!(retry[0].starts_with("Retry    word") && retry.len() > 1, "{text}");
    assert!(retry[1..].iter().all(|l| l.starts_with("         word")), "{text}");
    assert!(retry.iter().all(|l| l.len() <= 9 + 24), "{text}");
}

#[test]
fn cache_freshness() {
    let day = CHECK_INTERVAL.as_millis() as i64;
    let hour = RETRY_INTERVAL.as_millis() as i64;
    let ok = CheckCache { checked: 1_000_000, latest: Some("v0.1.1".into()), failed: false };
    assert!(ok.fresh(1_000_000 + day - 1));
    assert!(!ok.fresh(1_000_000 + day));
    assert!(!ok.fresh(999_999), "a clock that went back checks again");
    let failed = CheckCache { failed: true, ..ok };
    assert!(failed.fresh(1_000_000 + hour - 1));
    assert!(!failed.fresh(1_000_000 + hour));
}

#[tokio::test]
async fn fetch_latest_and_tags() {
    let gh = MockGitHub::start().await;
    gh.publish("v0.2.0", b"x", Publish { latest: true, ..Default::default() });
    gh.publish("v0.1.0", b"y", Publish::default());
    let src = source(&gh.api, Some("0.1.0+abc"));
    let latest = fetch(&src, None, Duration::from_secs(5)).await.unwrap();
    assert_eq!((latest.tag.as_str(), latest.version.to_string()), ("v0.2.0", "0.2.0".to_string()));
    assert!(latest.page.ends_with("/releases/tag/v0.2.0"));
    assert!(latest.asset("SHA256SUMS").is_some());
    let headers = gh.headers(&gh.latest_path());
    let header = |k: &str| headers.iter().find(|(n, _)| n == k).map(|(_, v)| v.clone());
    assert!(header("user-agent").is_some_and(|ua| ua.starts_with("ocd/")), "{headers:?}");
    assert_eq!(header("accept").as_deref(), Some("application/vnd.github+json"));
    assert_eq!(header("authorization"), None);
    assert_eq!(fetch(&src, Some("0.1.0"), Duration::from_secs(5)).await.unwrap().tag, "v0.1.0");
    assert_eq!(fetch(&src, Some("v0.1.0"), Duration::from_secs(5)).await.unwrap().tag, "v0.1.0");
    // A token goes along when there is one.
    let with_token = Source { token: Some("t0ken".into()), ..src.clone() };
    fetch(&with_token, None, Duration::from_secs(5)).await.unwrap();
    assert!(gh.headers(&gh.latest_path()).contains(&("authorization".into(), "Bearer t0ken".into())));

    let missing = fetch(&src, Some("9.9.9"), Duration::from_secs(5)).await.unwrap_err();
    assert_eq!(missing.title, "ocd v9.9.9 was not found");
    assert_eq!(missing.detail, "There is no release tagged v9.9.9 in sphinxid/ocd.");
}

#[tokio::test]
async fn fetch_errors() {
    let gh = MockGitHub::start().await;
    let src = source(&gh.api, Some("0.1.0+abc"));
    let none = fetch(&src, None, Duration::from_secs(5)).await.unwrap_err();
    assert_eq!(none.title, "No ocd release found");
    assert_eq!(none.detail, "sphinxid/ocd has no published release yet.");

    gh.set(&gh.latest_path(), Reply::status(403).header("x-ratelimit-remaining", "0").header("x-ratelimit-reset", "1793114029"));
    let limited = fetch(&src, None, Duration::from_secs(5)).await.unwrap_err();
    assert_eq!(limited.title, "GitHub API rate limit reached");
    assert!(limited.detail.contains(" until ") && limited.detail.ends_with("(HTTP 403)."), "{}", limited.detail);
    assert!(limited.retry.contains("GITHUB_TOKEN"));
    gh.set(&gh.latest_path(), Reply::status(429));
    assert_eq!(fetch(&src, None, Duration::from_secs(5)).await.unwrap_err().title, "GitHub API rate limit reached");

    gh.set(&gh.latest_path(), Reply::status(403));
    let forbidden = fetch(&src, None, Duration::from_secs(5)).await.unwrap_err();
    assert_eq!((forbidden.title.as_str(), forbidden.detail.as_str()), (CHECK_TITLE, "The GitHub API returned HTTP 403."));

    gh.set(&gh.latest_path(), Reply::json(serde_json::json!({ "tag_name": "nightly" })));
    let bad = fetch(&src, None, Duration::from_secs(5)).await.unwrap_err();
    assert_eq!((bad.title.as_str(), bad.detail.as_str()), ("Could not read the ocd release information", "The release tag nightly is not a version."));

    // Nothing listens on the port any more.
    let closed = {
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        format!("http://{}/repos/sphinxid/ocd", l.local_addr().unwrap())
    };
    let down = fetch(&source(&closed, None), None, Duration::from_secs(5)).await.unwrap_err();
    assert_eq!(down.title, CHECK_TITLE);
    assert_eq!(down.retry, RETRY_NETWORK);
    assert!(!down.detail.is_empty());
}

#[tokio::test]
async fn download_checks_the_archive() {
    let Some(platform) = PLATFORM else { return };
    let gh = MockGitHub::start().await;
    let src = source(&gh.api, Some("0.1.0+abc"));
    let get = |tag: &'static str| {
        let src = src.clone();
        async move { fetch(&src, Some(tag), Duration::from_secs(5)).await.unwrap() }
    };

    gh.publish("v0.2.0", b"NEW", Publish::default());
    assert_eq!(download(&get("v0.2.0").await, platform).await.unwrap(), b"NEW");

    gh.publish("v0.3.0", b"NEW", Publish { bad_checksum: true, ..Default::default() });
    let e = download(&get("v0.3.0").await, platform).await.unwrap_err();
    assert_eq!(e.title, format!("Checksum mismatch for ocd-v0.3.0-{platform}.{}", if platform.starts_with("windows") { "zip" } else { "tar.gz" }));
    assert!(e.detail.ends_with("Nothing was installed."));

    gh.publish("v0.4.0", b"NEW", Publish { no_checksum_line: true, ..Default::default() });
    let e = download(&get("v0.4.0").await, platform).await.unwrap_err();
    assert_eq!(e.title, "ocd 0.4.0 cannot be verified");
    assert!(e.detail.starts_with("SHA256SUMS has no entry for ocd-v0.4.0-"), "{}", e.detail);

    gh.publish("v0.5.0", b"NEW", Publish { no_archive: true, ..Default::default() });
    let e = download(&get("v0.5.0").await, platform).await.unwrap_err();
    assert_eq!(e.title, format!("ocd 0.5.0 has no download for {platform}"));
    assert!(e.retry.contains("/releases/tag/v0.5.0"), "{}", e.retry);

    // GitHub's own digest must agree too.
    let mut r = get("v0.2.0").await;
    let name = archive_name("v0.2.0", platform);
    r.assets.iter_mut().find(|a| a.name == name).unwrap().digest = Some(format!("sha256:{}", "0".repeat(64)));
    assert!(download(&r, platform).await.unwrap_err().title.starts_with("Checksum mismatch"));

    // No SHA256SUMS at all.
    r.assets.retain(|a| a.name != "SHA256SUMS");
    assert_eq!(download(&r, platform).await.unwrap_err().detail, "The release has no SHA256SUMS file.");
}

#[cfg(unix)]
fn write_exe(path: &Path, body: &[u8], mode: u32) {
    use std::os::unix::fs::PermissionsExt;
    std::fs::write(path, body).unwrap();
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)).unwrap();
}

#[cfg(unix)]
#[tokio::test]
async fn replace_swaps_a_copied_binary_after_checking_it() {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().unwrap();
    let exe = dir.path().join("ocd");
    write_exe(&exe, b"#!/bin/sh\necho old\n", 0o750);

    // A binary that reports another version is refused, and nothing changes.
    let wrong = mock::fake_binary("0.3.0+abc1234", None);
    let e = replace(&exe, &wrong, &v("0.2.0")).await.unwrap_err();
    assert_eq!(e.title, "The downloaded ocd 0.2.0 does not run on this system");
    assert!(e.detail.contains("not version 0.2.0"), "{}", e.detail);
    assert_eq!(std::fs::read(&exe).unwrap(), b"#!/bin/sh\necho old\n");
    assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 1, "the staged file is removed");

    // One that does not start either.
    let e = replace(&exe, b"\x7fELFgarbage", &v("0.2.0")).await.unwrap_err();
    assert!(e.detail.contains("could not run it") || e.detail.contains("exited"), "{}", e.detail);
    assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 1);

    let good = mock::fake_binary("0.2.0+abc1234", None);
    replace(&exe, &good, &v("0.2.0")).await.unwrap();
    assert_eq!(std::fs::read(&exe).unwrap(), good);
    assert_eq!(std::fs::metadata(&exe).unwrap().permissions().mode() & 0o7777, 0o750, "same permissions");
    assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 1);
}

#[cfg(unix)]
#[test]
fn unwritable_directories_are_refused_up_front() {
    use std::os::unix::fs::PermissionsExt;
    if unsafe { libc::geteuid() } == 0 {
        return; // root writes anywhere
    }
    let dir = tempfile::tempdir().unwrap();
    let bin = dir.path().join("bin");
    std::fs::create_dir(&bin).unwrap();
    let exe = bin.join("ocd");
    write_exe(&exe, b"x", 0o755);
    assert!(check_writable(&exe, "https://example.test/r").is_ok());
    std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o555)).unwrap();
    let e = check_writable(&exe, "https://example.test/r").unwrap_err();
    std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).unwrap();
    assert_eq!(e.title, format!("Cannot replace {}", exe.display()));
    assert!(e.detail.starts_with(&format!("{} is not writable by this user", bin.display())), "{}", e.detail);
    assert_eq!(e.retry, "Run sudo ocd upgrade, or download the release from https://example.test/r and replace the binary yourself.");
    assert_eq!(std::fs::read_dir(&bin).unwrap().count(), 1);
}

#[tokio::test]
async fn the_check_runs_once_a_day() {
    let gh = MockGitHub::start().await;
    gh.publish("v0.2.0", b"x", Publish { latest: true, ..Default::default() });
    let dir = tempfile::tempdir().unwrap();
    let g = global(dir.path());
    let src = source(&gh.api, Some("0.1.0+abc"));
    assert_eq!(latest_cached(&g, &src).await, Some(v("0.2.0")));
    assert_eq!(latest_cached(&g, &src).await, Some(v("0.2.0")));
    assert_eq!(gh.hits(&gh.latest_path()), 1);
    let cache = CheckCache::read(&cache_file(&g)).unwrap();
    assert_eq!((cache.latest.as_deref(), cache.failed), (Some("v0.2.0"), false));

    // A day later it looks again.
    CheckCache { checked: cache.checked - CHECK_INTERVAL.as_millis() as i64, ..cache.clone() }.write(&cache_file(&g));
    gh.publish("v0.3.0", b"x", Publish { latest: true, ..Default::default() });
    assert_eq!(latest_cached(&g, &src).await, Some(v("0.3.0")));
    assert_eq!(gh.hits(&gh.latest_path()), 2);

    // A failed lookup keeps the last answer and tries again an hour later, not on every start.
    let stale = CheckCache::read(&cache_file(&g)).unwrap();
    CheckCache { checked: stale.checked - CHECK_INTERVAL.as_millis() as i64, ..stale }.write(&cache_file(&g));
    gh.set(&gh.latest_path(), Reply::status(500));
    assert_eq!(latest_cached(&g, &src).await, Some(v("0.3.0")));
    assert_eq!(latest_cached(&g, &src).await, Some(v("0.3.0")));
    assert_eq!(gh.hits(&gh.latest_path()), 3);
    assert!(CheckCache::read(&cache_file(&g)).unwrap().failed);
}

#[tokio::test]
async fn notify_or_install_by_policy() {
    let Some(_) = PLATFORM else { return };
    let gh = MockGitHub::start().await;
    gh.publish("v0.2.0", &mock_binary("0.2.0+abc1234"), Publish { latest: true, ..Default::default() });
    let dir = tempfile::tempdir().unwrap();
    let g = global(dir.path());
    std::fs::create_dir_all(&g.config).unwrap();
    let exe = dir.path().join("bin").join(if cfg!(windows) { "ocd.exe" } else { "ocd" });
    std::fs::create_dir_all(exe.parent().unwrap()).unwrap();
    std::fs::write(&exe, b"old").unwrap();
    let src = Source { exe: Some(exe.clone()), ..source(&gh.api, Some("0.1.0+abc")) };

    // Default: notify only.
    assert_eq!(inspect(&g, &src).await, Some(Notice { policy: UpdatePolicy::Notify, version: v("0.2.0") }));
    assert_eq!(run(&g, &src, |_| panic!("no install")).await, Some(Outcome::Available("0.2.0".into())));
    assert_eq!(std::fs::read(&exe).unwrap(), b"old");
    std::fs::write(g.config.join("opencode.json"), r#"{ "autoupdate": "notify" }"#).unwrap();
    assert_eq!(run(&g, &src, |_| panic!("no install")).await, Some(Outcome::Available("0.2.0".into())));

    // Already current, newer than the latest, or a local build: nothing.
    assert_eq!(inspect(&g, &source(&gh.api, Some("0.2.0+3.def5678"))).await, None);
    assert_eq!(inspect(&g, &source(&gh.api, Some("0.3.0+abc"))).await, None);
    let hits = gh.hits(&gh.latest_path());
    assert_eq!(inspect(&g, &source(&gh.api, Some("20261027.124153+abc1234"))).await, None);
    assert_eq!(inspect(&g, &source(&gh.api, None)).await, None);
    // Disabled by the environment or by `autoupdate: false`: no lookup at all.
    let _ = std::fs::remove_file(cache_file(&g));
    assert_eq!(inspect(&g, &Source { disabled: true, ..src.clone() }).await, None);
    std::fs::write(g.config.join("opencode.json"), r#"{ "autoupdate": false }"#).unwrap();
    assert_eq!(inspect(&g, &src).await, None);
    assert_eq!(gh.hits(&gh.latest_path()), hits);

    // `autoupdate: true` installs.
    #[cfg(unix)]
    {
        std::fs::write(g.config.join("opencode.json"), r#"{ "autoupdate": true }"#).unwrap();
        let mut announced = None;
        assert_eq!(run(&g, &src, |v| announced = Some(v.to_string())).await, Some(Outcome::Installed("0.2.0".into())));
        assert_eq!(announced.as_deref(), Some("0.2.0"));
        assert_eq!(std::fs::read(&exe).unwrap(), mock_binary("0.2.0+abc1234"));
        // The `update` key wins over `autoupdate`.
        std::fs::write(g.config.join("opencode.json"), r#"{ "autoupdate": true, "update": "notify" }"#).unwrap();
        assert_eq!(inspect(&g, &src).await.map(|n| n.policy), Some(UpdatePolicy::Notify));
    }
}

#[tokio::test]
async fn check_for_the_update_dialog() {
    let gh = MockGitHub::start().await;
    gh.publish("v0.2.0", b"x", Publish { latest: true, ..Default::default() });
    assert_eq!(check(&source(&gh.api, Some("0.1.0+abc"))).await.unwrap(), Some(Outcome::Available("0.2.0".into())));
    assert_eq!(check(&source(&gh.api, Some("0.2.0+abc"))).await.unwrap(), None);
    assert_eq!(check(&source(&gh.api, Some("20261027.124153+abc"))).await.unwrap(), Some(Outcome::Unavailable(LOCAL_BUILD.into())));
    gh.set(&gh.latest_path(), Reply::status(500));
    assert_eq!(check(&source(&gh.api, Some("0.1.0+abc"))).await.unwrap_err().detail, "The GitHub API returned HTTP 500.");
}

fn mock_binary(build: &str) -> Vec<u8> {
    #[cfg(unix)]
    return mock::fake_binary(build, None);
    #[cfg(not(unix))]
    return build.as_bytes().to_vec();
}

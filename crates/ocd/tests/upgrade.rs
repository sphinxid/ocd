//! `ocd upgrade` end to end: a copy of the built binary (a hard link where possible) in a temporary directory
//! upgrades itself from a local stand-in for GitHub (`common::releases`, through `OCD_TEST_RELEASES_API`, test
//! only). `OCD_TEST_BUILD_VERSION` (test only, honoured only together with the API override) sets the build the
//! copy reports. The "new" binaries are shell scripts that answer `--version` like a release and run the real
//! binary for everything else, so the tests run on Unix only.
#![cfg(unix)]

mod common;

use std::path::{Path, PathBuf};
use std::process::Output;

use common::releases::{MockGitHub, Publish, fake_binary};

struct Case {
    env: common::Env,
    gh: MockGitHub,
    exe: PathBuf,
}

/// The built binary at `path`: a hard link (renaming over it never touches the original), else a copy.
fn place_binary(path: &Path) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    let built = Path::new(env!("CARGO_BIN_EXE_ocd"));
    if std::fs::hard_link(built, path).is_err() {
        std::fs::copy(built, path).unwrap();
    }
}

impl Case {
    async fn new() -> Case {
        let env = common::Env::new();
        let exe = env.path("bin/ocd");
        place_binary(&exe);
        Case { env, gh: MockGitHub::start().await, exe }
    }

    fn run_as(&self, build: Option<&str>, args: &[&str]) -> Output {
        let c = self.env.command(args);
        let mut command = std::process::Command::new(&self.exe);
        command.args(args).current_dir(self.env.path("work"));
        for (k, v) in c.get_envs() {
            match v {
                Some(v) => command.env(k, v),
                None => command.env_remove(k),
            };
        }
        command
            .env("OCD_TEST_RELEASES_API", &self.gh.api)
            .env_remove("GITHUB_TOKEN")
            .env_remove("GH_TOKEN")
            .stdin(std::process::Stdio::null());
        match build {
            Some(b) => command.env("OCD_TEST_BUILD_VERSION", b),
            None => command.env_remove("OCD_TEST_BUILD_VERSION"),
        };
        tokio::task::block_in_place(|| command.output().unwrap())
    }

    fn run(&self, build: &str, args: &[&str]) -> Output {
        self.run_as(Some(build), args)
    }

    fn binary(&self) -> Vec<u8> {
        std::fs::read(&self.exe).unwrap()
    }

    fn untouched(&self) -> bool {
        std::fs::metadata(&self.exe).unwrap().len() == std::fs::metadata(env!("CARGO_BIN_EXE_ocd")).unwrap().len()
            && std::fs::read_dir(self.exe.parent().unwrap()).unwrap().count() == 1
    }

    fn downloads(&self) -> usize {
        self.gh.hits_under("/sphinxid/ocd/releases/download/")
    }
}

fn out(o: &Output) -> String {
    format!("{}{}", common::text(&o.stdout), common::text(&o.stderr))
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn same_version_is_already_installed() {
    let c = Case::new().await;
    c.gh.publish("v0.2.0", &fake_binary("0.2.0+abc1234", None), Publish { latest: true, ..Default::default() });
    for build in ["0.2.0+abc1234", "0.2.0+3.def5678"] {
        let o = c.run(build, &["upgrade"]);
        assert_eq!(o.status.code(), Some(0), "{}", out(&o));
        let text = common::text(&o.stdout);
        assert!(text.starts_with("┌  Upgrade\n"), "{text}");
        assert!(text.contains("▲  ocd upgrade skipped: 0.2.0 is already installed\n"), "{text}");
        assert!(text.ends_with("└  Done\n\n"), "{text}");
    }
    // `update` and `-m` too; the method is ignored.
    let o = c.run("0.2.0+abc1234", &["update", "v0.2.0", "-m", "npm"]);
    assert_eq!(o.status.code(), Some(0), "{}", out(&o));
    assert!(common::text(&o.stdout).contains("▲  ocd installs only from its GitHub releases; ignoring --method npm\n"));
    assert!(common::text(&o.stdout).contains("0.2.0 is already installed"));
    assert_eq!(c.downloads(), 0);
    assert!(c.untouched());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_newer_release_replaces_the_binary() {
    let c = Case::new().await;
    let new = fake_binary("0.2.0+abc1234", None);
    c.gh.publish("v0.2.0", &new, Publish { latest: true, ..Default::default() });
    let o = c.run("0.1.0+fedcba9", &["upgrade"]);
    assert_eq!(o.status.code(), Some(0), "{}", out(&o));
    let text = common::text(&o.stdout);
    assert!(text.contains("●  From 0.1.0+fedcba9 → 0.2.0\n"), "{text}");
    assert!(text.contains("◇  Upgrade complete\n"), "{text}");
    assert!(!text.contains("background service"), "no service runs: {text}");
    assert!(text.ends_with("└  Done\n\n"), "{text}");
    assert_eq!(c.binary(), new);
    use std::os::unix::fs::PermissionsExt;
    assert_eq!(std::fs::metadata(&c.exe).unwrap().permissions().mode() & 0o111, 0o111);
    assert_eq!(std::fs::read_dir(c.exe.parent().unwrap()).unwrap().count(), 1, "no staged files left");
    // The original build is untouched (the copy was replaced, not written into).
    assert!(std::fs::metadata(env!("CARGO_BIN_EXE_ocd")).unwrap().len() > new.len() as u64);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_older_latest_is_skipped_but_an_explicit_target_downgrades() {
    let c = Case::new().await;
    let old = fake_binary("0.2.0+abc1234", None);
    c.gh.publish("v0.2.0", &old, Publish { latest: true, ..Default::default() });
    let o = c.run("0.3.0+abc1234", &["upgrade"]);
    assert_eq!(o.status.code(), Some(0), "{}", out(&o));
    assert!(
        common::text(&o.stdout).contains("▲  ocd upgrade skipped: 0.3.0 is already installed, newer than the latest release (0.2.0)\n"),
        "{}",
        out(&o)
    );
    assert!(c.untouched());
    let o = c.run("0.3.0+abc1234", &["upgrade", "0.2.0"]);
    assert_eq!(o.status.code(), Some(0), "{}", out(&o));
    assert!(common::text(&o.stdout).contains("●  From 0.3.0+abc1234 → 0.2.0\n"), "{}", out(&o));
    assert_eq!(c.binary(), old);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_local_build_asks_first() {
    let c = Case::new().await;
    let new = fake_binary("0.2.0+abc1234", None);
    c.gh.publish("v0.2.0", &new, Publish { latest: true, ..Default::default() });
    let o = c.run("20261027.124153+abc1234", &["upgrade"]);
    assert_eq!(o.status.code(), Some(1), "{}", out(&o));
    let text = common::text(&o.stdout);
    assert!(text.contains("▲  This ocd is a local build (20261027.124153+abc1234), not a release.\n"), "{text}");
    assert!(text.contains("■  Pass --yes to replace a local build without an interactive terminal.\n"), "{text}");
    assert!(text.ends_with("└  Failed\n\n"), "{text}");
    assert!(c.untouched());
    // No build version at all.
    let o = c.run("", &["upgrade"]);
    assert_eq!(o.status.code(), Some(1));
    assert!(common::text(&o.stdout).contains("▲  This ocd is a local build, not a release.\n"), "{}", out(&o));
    assert_eq!(c.downloads(), 0);

    let o = c.run("20261027.124153+abc1234", &["upgrade", "--yes"]);
    assert_eq!(o.status.code(), Some(0), "{}", out(&o));
    assert!(common::text(&o.stdout).contains("●  From 20261027.124153+abc1234 → 0.2.0\n"), "{}", out(&o));
    assert_eq!(c.binary(), new);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_bad_checksum_installs_nothing() {
    let c = Case::new().await;
    c.gh.publish("v0.2.0", &fake_binary("0.2.0+abc1234", None), Publish { latest: true, bad_checksum: true, ..Default::default() });
    let o = c.run("0.1.0+abc1234", &["upgrade"]);
    assert_eq!(o.status.code(), Some(1), "{}", out(&o));
    let text = common::text(&o.stdout);
    assert!(text.contains("■  Upgrade failed\n"), "{text}");
    assert!(text.contains("■  Checksum mismatch for ocd-v0.2.0-"), "{text}");
    assert!(text.replace("\n│  ", " ").contains("Nothing was installed."), "{text}");
    assert!(text.contains("Retry    Run ocd upgrade again."), "{text}");
    assert!(text.ends_with("└  Failed\n\n"), "{text}");
    assert!(c.untouched());

    c.gh.publish("v0.2.0", &fake_binary("0.2.0+abc1234", None), Publish { latest: true, no_checksum_line: true, ..Default::default() });
    let o = c.run("0.1.0+abc1234", &["upgrade"]);
    assert_eq!(o.status.code(), Some(1), "{}", out(&o));
    assert!(common::text(&o.stdout).contains("■  ocd 0.2.0 cannot be verified\n"), "{}", out(&o));
    assert!(common::text(&o.stdout).contains("SHA256SUMS has no entry for ocd-v0.2.0-"), "{}", out(&o));
    assert!(c.untouched());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_release_without_this_platform() {
    let c = Case::new().await;
    c.gh.publish("v0.2.0", b"x", Publish { latest: true, no_archive: true, ..Default::default() });
    let o = c.run("0.1.0+abc1234", &["upgrade"]);
    assert_eq!(o.status.code(), Some(1), "{}", out(&o));
    let platform = common::releases::PLATFORM.unwrap();
    assert!(common::text(&o.stdout).contains(&format!("■  ocd 0.2.0 has no download for {platform}\n")), "{}", out(&o));
    assert!(c.untouched());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_binary_that_does_not_run_is_not_installed() {
    let c = Case::new().await;
    c.gh.publish("v0.2.0", &fake_binary("0.1.9+abc1234", None), Publish { latest: true, ..Default::default() });
    let o = c.run("0.1.0+abc1234", &["upgrade"]);
    assert_eq!(o.status.code(), Some(1), "{}", out(&o));
    assert!(common::text(&o.stdout).contains("■  The downloaded ocd 0.2.0 does not run on this system\n"), "{}", out(&o));
    assert!(c.untouched());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn unknown_versions_and_tags() {
    let c = Case::new().await;
    let o = c.run("0.1.0+abc1234", &["upgrade"]);
    assert_eq!(o.status.code(), Some(1), "{}", out(&o));
    assert!(common::text(&o.stdout).contains("■  No ocd release found\n"), "{}", out(&o));
    let o = c.run("0.1.0+abc1234", &["upgrade", "9.9.9"]);
    assert_eq!(o.status.code(), Some(1), "{}", out(&o));
    let text = common::text(&o.stdout);
    assert!(text.contains("■  ocd v9.9.9 was not found\n│  \n│  There is no release tagged v9.9.9 in sphinxid/ocd.\n"), "{text}");
    let o = c.run("0.1.0+abc1234", &["upgrade", "latest"]);
    assert_eq!(o.status.code(), Some(1), "{}", out(&o));
    assert!(common::text(&o.stdout).contains("■  Invalid version: latest\n"), "{}", out(&o));
    assert!(c.untouched());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_unwritable_directory_fails_before_downloading() {
    use std::os::unix::fs::PermissionsExt;
    if unsafe { libc::geteuid() } == 0 {
        return;
    }
    let c = Case::new().await;
    c.gh.publish("v0.2.0", &fake_binary("0.2.0+abc1234", None), Publish { latest: true, ..Default::default() });
    let bin = c.exe.parent().unwrap().to_path_buf();
    std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o555)).unwrap();
    let o = c.run("0.1.0+abc1234", &["upgrade"]);
    std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).unwrap();
    assert_eq!(o.status.code(), Some(1), "{}", out(&o));
    let text = common::text(&o.stdout);
    assert!(text.contains(&format!("■  Cannot replace {}\n", c.exe.display())), "{text}");
    assert!(text.contains("is not writable by this user"), "{text}");
    assert!(text.contains("Retry    Run sudo ocd upgrade, or download the release from"), "{text}");
    assert!(!text.contains("Upgrading"), "fails before the download starts: {text}");
    assert_eq!(c.downloads(), 0);
    assert!(c.untouched());
}

fn registration(env: &common::Env) -> Option<serde_json::Value> {
    let dir = env.path("state/opencode");
    std::fs::read_dir(dir)
        .ok()?
        .flatten()
        .find(|e| {
            let n = e.file_name().to_string_lossy().into_owned();
            n.starts_with("service") && n.ends_with(".json")
        })
        .and_then(|e| serde_json::from_str(&std::fs::read_to_string(e.path()).ok()?).ok())
}

fn alive(pid: i64) -> bool {
    unsafe { libc::kill(pid as i32, 0) == 0 }
}

/// Stops the test's background service, even on failure.
struct StopService<'a>(&'a common::Env);

impl Drop for StopService<'_> {
    fn drop(&mut self) {
        let _ = self.0.command(&["service", "stop"]).output();
    }
}

fn free_port() -> u16 {
    loop {
        let port = std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
        if ![49374, 49375, 28894, 28895].contains(&port) {
            return port;
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_running_service_is_restarted_on_the_new_binary() {
    let c = Case::new().await;
    let _stop = StopService(&c.env);
    let port = free_port().to_string();
    assert_eq!(c.run("0.1.0+abc1234", &["service", "set", "port", &port]).status.code(), Some(0));
    let started = c.run("0.1.0+abc1234", &["service", "start"]);
    assert_eq!(started.status.code(), Some(0), "{}", out(&started));
    let url = common::text(&started.stdout).trim().to_string();
    assert!(url.ends_with(&format!(":{port}")), "{url}");
    let before = registration(&c.env).expect("registered")["pid"].as_i64().unwrap();
    assert!(alive(before));

    // The new release answers --version and runs the real binary otherwise.
    let real = c.env.path("real/ocd");
    place_binary(&real);
    c.gh.publish("v0.2.0", &fake_binary("0.2.0+abc1234", Some(&real)), Publish { latest: true, ..Default::default() });
    let o = c.run("0.1.0+abc1234", &["upgrade"]);
    assert_eq!(o.status.code(), Some(0), "{}", out(&o));
    let text = common::text(&o.stdout);
    assert!(text.contains("◇  Upgrade complete\n"), "{text}");
    assert!(text.contains("◇  Background service restarted\n"), "{text}");
    let after = registration(&c.env).expect("registered again")["pid"].as_i64().unwrap();
    assert_ne!(before, after);
    assert!(alive(after));
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    while alive(before) && std::time::Instant::now() < deadline {
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    assert!(!alive(before), "the old service process is gone");
    let status = c.run("0.2.0+abc1234", &["service", "status"]);
    assert_eq!(common::text(&status.stdout).trim(), url);
    // It runs from the replaced path.
    let cmdline = std::fs::read(format!("/proc/{after}/cmdline")).unwrap_or_default();
    if !cmdline.is_empty() {
        let cmdline = String::from_utf8_lossy(&cmdline).replace('\0', " ");
        assert!(cmdline.contains(&real.display().to_string()) || cmdline.contains(&c.exe.display().to_string()), "{cmdline}");
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_stopped_service_stays_stopped() {
    let c = Case::new().await;
    c.gh.publish("v0.2.0", &fake_binary("0.2.0+abc1234", None), Publish { latest: true, ..Default::default() });
    let o = c.run("0.1.0+abc1234", &["upgrade"]);
    assert_eq!(o.status.code(), Some(0), "{}", out(&o));
    assert!(!common::text(&o.stdout).contains("background service"), "{}", out(&o));
    assert!(registration(&c.env).is_none());
}

/// The real v0.1.1 release replayed from `$OCD_SCRATCH/releases/v0.1.1/` (`release.json` as GitHub's API returned
/// it, `SHA256SUMS` and the Linux x86_64 archive): the lookup, checksum, extraction and `--version` check of the
/// real binary, then the swap. Skipped when the files are not there.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_real_v0_1_1_release() {
    let Some(dir) = std::env::var_os("OCD_SCRATCH").map(|s| PathBuf::from(s).join("releases/v0.1.1")) else { return };
    let name = "ocd-v0.1.1-linux-x64.tar.gz";
    let (Ok(json), Ok(sums), Ok(archive)) =
        (std::fs::read_to_string(dir.join("release.json")), std::fs::read(dir.join("SHA256SUMS")), std::fs::read(dir.join(name)))
    else {
        eprintln!("skipping: no release files in {}", dir.display());
        return;
    };
    if common::releases::PLATFORM != Some("linux-x64") {
        return;
    }
    let c = Case::new().await;
    let mut release: serde_json::Value = serde_json::from_str(&json).unwrap();
    for asset in release["assets"].as_array_mut().unwrap() {
        let file = asset["name"].as_str().unwrap().to_string();
        let path = c.gh.download_path("v0.1.1", &file);
        asset["browser_download_url"] = format!("{}{path}", c.gh.root).into();
        match file.as_str() {
            "SHA256SUMS" => c.gh.set(&path, common::releases::Reply::bytes(sums.clone())),
            f if f == name => c.gh.set(&path, common::releases::Reply::bytes(archive.clone())),
            _ => {}
        }
    }
    c.gh.set(&c.gh.latest_path(), common::releases::Reply::json(release));
    let o = c.run("0.1.0+abc1234", &["upgrade"]);
    assert_eq!(o.status.code(), Some(0), "{}", out(&o));
    assert!(common::text(&o.stdout).contains("●  From 0.1.0+abc1234 → 0.1.1\n"), "{}", out(&o));
    assert_eq!(c.gh.hits(&c.gh.download_path("v0.1.1", name)), 1);
    let installed = std::fs::read(&c.exe).unwrap();
    assert_eq!(&installed[..4], b"\x7fELF");
    assert!(installed.len() > 50_000_000);
    // The installed binary is the real v0.1.1 (its own --version, isolated like the upgrade's check).
    let version = c.env.command(&["--version"]).get_envs().fold(std::process::Command::new(&c.exe), |mut cmd, (k, v)| {
        match v {
            Some(v) => cmd.env(k, v),
            None => cmd.env_remove(k),
        };
        cmd
    });
    let mut version = version;
    let v = tokio::task::block_in_place(|| version.arg("--version").current_dir(c.env.path("work")).output().unwrap());
    assert!(common::text(&v.stdout).starts_with("ocd v0.1.1+"), "{}", out(&v));
}

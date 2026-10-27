#![allow(dead_code)]
//! Shared helpers: run the built binary with isolated XDG directories.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

pub struct Env {
    pub root: tempfile::TempDir,
}

impl Env {
    pub fn new() -> Env {
        let root = tempfile::tempdir().unwrap();
        for d in ["home", "data", "config", "state", "cache", "work"] {
            std::fs::create_dir_all(root.path().join(d)).unwrap();
        }
        Env { root }
    }

    pub fn path(&self, p: &str) -> PathBuf {
        self.root.path().join(p)
    }

    pub fn command(&self, args: &[&str]) -> Command {
        let mut c = Command::new(env!("CARGO_BIN_EXE_ocd"));
        c.args(args)
            .current_dir(self.path("work"))
            .env("HOME", self.path("home"))
            .env("XDG_DATA_HOME", self.path("data"))
            .env("XDG_CONFIG_HOME", self.path("config"))
            .env("XDG_STATE_HOME", self.path("state"))
            .env("XDG_CACHE_HOME", self.path("cache"))
            .env("PWD", self.path("work"))
            .env("OPENCODE_DISABLE_MODELS_FETCH", "1")
            .env("OPENCODE_DISABLE_AUTOUPDATE", "1")
            .env_remove("OPENCODE_TEST_HOME")
            .env_remove("OPENCODE_CONFIG_DIR")
            .env_remove("OPENCODE_PASSWORD")
            .env_remove("OPENCODE_SERVER_PASSWORD")
            .env_remove("NO_COLOR")
            .env_remove("OPENCODE_PRINT_LOGS")
            .stdin(std::process::Stdio::null());
        c
    }

    pub fn run(&self, args: &[&str]) -> Output {
        self.command(args).output().unwrap()
    }
}

pub fn text(b: &[u8]) -> String {
    String::from_utf8_lossy(b).into_owned()
}

pub fn exists(p: &Path) -> bool {
    p.exists()
}

/// What `ocd --version` prints: the ocd build (`OCD_BUILD_VERSION`, always set by build.rs unless set empty) and
/// the opencode version.
pub fn version_line() -> String {
    match option_env!("OCD_BUILD_VERSION") {
        Some(build) if !build.is_empty() => format!("ocd v{build} (opencode {})\n", env!("CARGO_PKG_VERSION")),
        _ => format!("ocd v{}\n", env!("CARGO_PKG_VERSION")),
    }
}

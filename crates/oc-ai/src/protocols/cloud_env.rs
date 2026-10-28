//! The environment the cloud credential chains read (`process.env`, the home directory) and the
//! process-wide state the AWS SDK and google-auth-library keep in module globals.
//!
//! Production code uses [`Env::process`]: the live process environment and one shared state.
//! Tests build an [`Env::from_map`] with their own variables (`HOME`, `AWS_CONFIG_FILE`,
//! `CLOUDSDK_CONFIG`, endpoint overrides, ...) and a fresh state, so they never read the user's real
//! files and can run in parallel.

use parking_lot::Mutex;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, LazyLock};
use std::time::Instant;

/// Module-global state of the SDKs (one per process, or per test environment).
#[derive(Default)]
pub struct State {
    /// `@aws-sdk/token-providers` `lastRefreshAttemptTime` (SSO-OIDC refreshes at most every 30 s).
    pub sso_last_refresh: Mutex<Option<Instant>>,
    /// `@smithy/core/config` `negativeCacheUntil` (IMDS region lookups, 60 s).
    pub imds_region_negative_until: Mutex<Option<Instant>>,
    /// `multipleCredentialSourceWarningEmitted` of `@aws-sdk/credential-provider-node`.
    pub multiple_sources_warned: AtomicBool,
    /// `gcp-metadata` `cachedIsAvailableResponse`.
    pub gce_available: tokio::sync::OnceCell<bool>,
    /// `gcp-metadata` `gcpResidencyCache`.
    pub gcp_residency: Mutex<Option<bool>>,
}

impl State {
    /// Sets the flag and returns whether it was already set.
    pub fn warn_once(flag: &AtomicBool) -> bool {
        flag.swap(true, Ordering::SeqCst)
    }
}

static PROCESS_STATE: LazyLock<Arc<State>> = LazyLock::new(|| Arc::new(State::default()));

/// Variable lookup plus the SDK state that goes with it.
#[derive(Clone)]
pub struct Env {
    vars: Option<Arc<HashMap<String, String>>>,
    state: Arc<State>,
}

impl std::fmt::Debug for Env {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Env").field("process", &self.vars.is_none()).finish()
    }
}

impl Env {
    /// The live process environment.
    pub fn process() -> Env {
        Env { vars: None, state: PROCESS_STATE.clone() }
    }

    /// A fixed set of variables with its own state (tests).
    pub fn from_map<K: Into<String>, V: Into<String>>(vars: impl IntoIterator<Item = (K, V)>) -> Env {
        Env { vars: Some(Arc::new(vars.into_iter().map(|(k, v)| (k.into(), v.into())).collect())), state: Arc::new(State::default()) }
    }

    /// `process.env[name]` (`Some("")` for a variable set to the empty string).
    pub fn var(&self, name: &str) -> Option<String> {
        match &self.vars {
            Some(map) => map.get(name).cloned(),
            None => std::env::var(name).ok(),
        }
    }

    /// The variable when it is set and not empty (a truthy JS string).
    pub fn non_empty(&self, name: &str) -> Option<String> {
        self.var(name).filter(|v| !v.is_empty())
    }

    pub fn state(&self) -> &State {
        &self.state
    }

    /// `os.homedir()` on Unix (`$HOME`, then the password database).
    pub fn os_home(&self) -> PathBuf {
        if cfg!(windows) {
            if let Some(profile) = self.non_empty("USERPROFILE") {
                return PathBuf::from(profile);
            }
        } else if let Some(home) = self.non_empty("HOME") {
            return PathBuf::from(home);
        }
        if self.vars.is_some() {
            return PathBuf::new();
        }
        dirs::home_dir().unwrap_or_default()
    }

    /// `getHomeDir` of `@smithy/core/config`: `HOME`, `USERPROFILE`, `HOMEDRIVE` + `HOMEPATH`, then `os.homedir()`.
    pub fn aws_home(&self) -> PathBuf {
        if let Some(home) = self.non_empty("HOME") {
            return PathBuf::from(home);
        }
        if let Some(profile) = self.non_empty("USERPROFILE") {
            return PathBuf::from(profile);
        }
        if let Some(path) = self.non_empty("HOMEPATH") {
            let drive = self.var("HOMEDRIVE").unwrap_or_else(|| format!("C:{}", std::path::MAIN_SEPARATOR));
            return PathBuf::from(format!("{drive}{path}"));
        }
        self.os_home()
    }

    /// Gives a child process this environment (`{ ...process.env, ...extra }`).
    pub fn apply(&self, command: &mut tokio::process::Command) {
        if let Some(map) = &self.vars {
            command.env_clear();
            if !map.contains_key("PATH")
                && let Ok(path) = std::env::var("PATH")
            {
                command.env("PATH", path);
            }
            command.envs(map.iter());
        }
    }
}

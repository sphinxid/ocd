//! Global paths (`util/src/global.ts`, `util/src/global-roots.ts`).
//! XDG-based on every OS, matching the original.

use std::path::{Path, PathBuf};

const APP: &str = "opencode";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Global {
    pub home: PathBuf,
    pub data: PathBuf,
    pub cache: PathBuf,
    pub config: PathBuf,
    pub state: PathBuf,
    pub tmp: PathBuf,
    pub bin: PathBuf,
    pub log: PathBuf,
    pub repos: PathBuf,
}

fn env_nonempty(key: &str) -> Option<PathBuf> {
    std::env::var_os(key).filter(|v| !v.is_empty()).map(PathBuf::from)
}

fn os_home() -> PathBuf {
    dirs::home_dir().unwrap_or_else(|| PathBuf::from("/"))
}

impl Global {
    /// Paths without side effects. `config` honors `OPENCODE_CONFIG_DIR`.
    pub fn resolve() -> Self {
        let home = os_home();
        let data = env_nonempty("XDG_DATA_HOME").unwrap_or_else(|| home.join(".local/share")).join(APP);
        let cache = env_nonempty("XDG_CACHE_HOME").unwrap_or_else(|| home.join(".cache")).join(APP);
        let config = env_nonempty("OPENCODE_CONFIG_DIR")
            .unwrap_or_else(|| env_nonempty("XDG_CONFIG_HOME").unwrap_or_else(|| home.join(".config")).join(APP));
        let state = env_nonempty("XDG_STATE_HOME").unwrap_or_else(|| home.join(".local/state")).join(APP);
        let tmp = std::env::temp_dir().join(APP);
        Global {
            home: std::env::var_os("OPENCODE_TEST_HOME").map(PathBuf::from).unwrap_or(home),
            bin: cache.join("bin"),
            log: data.join("log"),
            repos: data.join("repos"),
            data,
            cache,
            config,
            state,
            tmp,
        }
    }

    /// Resolve, create the standard directories and canonicalize `tmp` (`global.ts` acquire).
    pub fn acquire() -> std::io::Result<Self> {
        let mut g = Self::resolve();
        for dir in [&g.data, &g.config, &g.state, &g.log, &g.bin, &g.repos, &g.tmp] {
            std::fs::create_dir_all(dir)?;
        }
        g.tmp = std::fs::canonicalize(&g.tmp)?;
        Ok(g)
    }

    /// Database path (`cli/src/database-path.ts`).
    pub fn database(&self, channel: &str) -> PathBuf {
        let filename = std::env::var("OPENCODE_DB").ok().unwrap_or_else(|| {
            let disabled = matches!(std::env::var("OPENCODE_DISABLE_CHANNEL_DB").as_deref(), Ok("1") | Ok("true"));
            if crate::version::is_release_channel(channel) || channel == "prod" || disabled {
                "opencode.db".to_string()
            } else {
                format!("opencode-{}.db", crate::version::sanitize(channel))
            }
        });
        if filename == ":memory:" {
            return PathBuf::from(":memory:");
        }
        let p = Path::new(&filename);
        if p.is_absolute() { p.to_path_buf() } else { self.data.join(p) }
    }

    /// Log file (`observability.ts` `file(local, channel)`).
    pub fn log_file(&self, channel: &str) -> PathBuf {
        if channel == "local" {
            self.log.join(format!("opencode-{}.log", crate::version::sanitize(channel)))
        } else {
            self.log.join("opencode.log")
        }
    }

    pub fn locks(&self) -> PathBuf {
        self.state.join("locks")
    }
}

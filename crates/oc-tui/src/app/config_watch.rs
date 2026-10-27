//! Live reload of the TUI config (`ConfigProvider`'s `safeWatch` in `config/index.tsx`): when
//! `cli.json` changes on disk, its settings apply at once (keybinds, theme, everything read from
//! the config). Checked about once a second; `OPENCODE_FILEWATCHER_DISABLE` (or
//! `OPENCODE_DISABLE_FILEWATCHER`) turns it off.

use super::App;
use crate::config::Config;
use crate::theme::Mode;
use std::time::{Duration, Instant, SystemTime};

const INTERVAL: Duration = Duration::from_millis(1000);

#[derive(Clone, Debug, Default)]
pub struct ConfigWatch {
    /// The file's modification time when last checked (`None`: missing).
    pub mtime: Option<SystemTime>,
    pub checked: Option<Instant>,
}

fn disabled() -> bool {
    let v = std::env::var("OPENCODE_FILEWATCHER_DISABLE").or_else(|_| std::env::var("OPENCODE_DISABLE_FILEWATCHER")).unwrap_or_default();
    v == "1" || v.eq_ignore_ascii_case("true")
}

impl App {
    /// Reload the config when its file changed since the last check.
    pub fn watch_config(&mut self) {
        if self.config_watch.checked.is_some_and(|c| c.elapsed() < INTERVAL) || disabled() {
            return;
        }
        let first = self.config_watch.checked.is_none();
        self.config_watch.checked = Some(Instant::now());
        let mtime = std::fs::metadata(&self.config.path).and_then(|m| m.modified()).ok();
        if mtime == self.config_watch.mtime {
            return;
        }
        self.config_watch.mtime = mtime;
        if !first {
            let fresh = Config::load_from(self.config.path.clone());
            self.apply_config(fresh);
        }
    }

    /// Take a reloaded config: keybinds, the theme and its mode follow it.
    pub fn apply_config(&mut self, mut fresh: Config) {
        fresh.terminal_suspend = self.config.terminal_suspend;
        if fresh.raw == self.config.raw {
            return;
        }
        let old = std::mem::replace(&mut self.config, fresh);
        self.keybinds = self.config.keybinds();
        if old.theme_mode() != self.config.theme_mode() {
            self.themes.lock = Mode::parse(self.config.theme_mode());
            if let Some(m) = self.themes.lock {
                self.themes.set_mode(m);
            }
        }
        if old.theme_name() != self.config.theme_name() {
            let name = self.config.theme_name().unwrap_or(crate::theme::DEFAULT_THEME).to_string();
            self.themes.set(&name);
        }
        self.reload_theme();
        self.dirty = true;
    }
}

#[cfg(test)]
mod tests {
    use super::super::tests::app;
    use super::*;
    use serde_json::json;

    #[test]
    fn edits_on_disk_apply_live() {
        let dir = std::env::temp_dir().join(format!("oc-tui-config-watch-{}", crate::util::id_body(false)));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("cli.json");
        std::fs::write(&path, r#"{ "animations": false }"#).unwrap();
        let mut a = app();
        a.config = Config::load_from(path.clone());
        a.watch_config();
        assert!(!a.config.animations());
        std::fs::write(&path, r#"{ "animations": true, "keybinds": { "command.palette.show": "ctrl+k" }, "theme": { "name": "tokyonight" } }"#).unwrap();
        // A different modification time, whatever the file system's resolution.
        let later = SystemTime::now() + Duration::from_secs(5);
        std::fs::File::options().write(true).open(&path).unwrap().set_modified(later).unwrap();
        a.config_watch.checked = Some(Instant::now() - INTERVAL);
        a.watch_config();
        assert!(a.config.animations());
        assert_eq!(a.keybinds.shortcut("command.palette.show"), "ctrl+k");
        assert_eq!(a.config.theme_name(), Some("tokyonight"));
        assert_eq!(a.config.raw, json!({ "animations": true, "keybinds": { "command.palette.show": "ctrl+k" }, "theme": { "name": "tokyonight" } }));
        let _ = std::fs::remove_dir_all(&dir);
    }
}

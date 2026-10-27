//! Attention: desktop notifications (terminal escape sequences) and sounds (spec §7.7).

use crate::app::App;

fn normalize(s: &str, max: usize) -> String {
    let s = crate::util::locale::strip_ansi(s);
    let s: String = s.replace(['\n', '\r'], " ").chars().filter(|c| !c.is_control()).collect();
    crate::util::locale::truncate(&s, max)
}

/// Notification escape: OSC 777 (rxvt/foot/ghostty/WezTerm) plus OSC 9 (iTerm2/kitty).
pub fn notification_sequence(title: &str, message: &str) -> String {
    let title = normalize(title, 80);
    let message = normalize(message, 240);
    format!("\x1b]777;notify;{title};{message}\x07\x1b]9;{message}\x07")
}

fn play(path: &str, volume: f64) {
    let vol = volume.clamp(0.0, 1.0);
    let candidates: Vec<(&str, Vec<String>)> = if cfg!(target_os = "macos") {
        vec![("afplay", vec!["-v".into(), format!("{vol}"), path.into()])]
    } else {
        vec![
            ("paplay", vec![format!("--volume={}", (vol * 65536.0) as u32), path.into()]),
            ("ffplay", vec!["-nodisp".into(), "-autoexit".into(), "-loglevel".into(), "quiet".into(), "-volume".into(), format!("{}", (vol * 100.0) as u32), path.into()]),
            ("aplay", vec!["-q".into(), path.into()]),
        ]
    };
    for (cmd, args) in candidates {
        if std::process::Command::new(cmd)
            .args(&args)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .is_ok()
        {
            return;
        }
    }
}

/// Raise attention for `kind` (`default|question|permission|error|done|subagent_done`).
pub fn notify(app: &mut App, kind: &str, message: &str, title: Option<&str>) {
    // The `opencode.notifications` built-in, unless disabled in /plugins.
    if !app.builtin_active("opencode.notifications") {
        return;
    }
    let notifications = app.config.notifications();
    let sound = app.config.sound();
    if !notifications && !sound {
        return;
    }
    let child = app.session().is_some_and(|s| s.parent_id.is_some());
    if notifications && !child && app.focused == Some(false) {
        let seq = notification_sequence(title.unwrap_or("OpenCode"), message);
        use std::io::Write;
        let _ = std::io::stdout().write_all(seq.as_bytes());
        let _ = std::io::stdout().flush();
    }
    if sound {
        let path = app
            .config
            .get(&format!("attention.sounds.{kind}"))
            .or_else(|| app.config.get("attention.sounds.default"))
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());
        match path {
            Some(p) => play(&p, app.config.volume()),
            None => {
                // Built-in sound packs ship as mp3 assets upstream; fall back to the terminal bell.
                use std::io::Write;
                let _ = std::io::stdout().write_all(b"\x07");
                let _ = std::io::stdout().flush();
            }
        }
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn sequence() {
        let s = super::notification_sequence("Title", "line\nbreak \x1b[31mred\x1b[0m");
        assert_eq!(s, "\x1b]777;notify;Title;line break red\x07\x1b]9;line break red\x07");
    }
}

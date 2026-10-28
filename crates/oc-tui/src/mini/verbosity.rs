//! Verbosity presets over the transcript knobs (`src/mini/verbosity.ts`) and `resolveMiniSettings`
//! (`runtime.boot.ts`).

use super::types::{MiniSettingChange, MiniSettings, Verbosity};
use serde_json::Value;

const LEVELS: [Verbosity; 3] = [Verbosity::Quiet, Verbosity::Default, Verbosity::Everything];

/// `(thinking, tools, shell_output, turn_summary, footer)`
fn preset(level: Verbosity) -> [bool; 5] {
    match level {
        Verbosity::Quiet => [false, false, false, false, false],
        Verbosity::Default => [true, false, false, true, true],
        Verbosity::Everything => [true, true, true, true, true],
    }
}

fn knobs(s: &MiniSettings) -> [bool; 5] {
    [s.thinking, s.tools, s.shell_output, s.turn_summary, s.footer]
}

/// `verbosityPreset(level)` applied to settings.
pub fn apply_preset(settings: &MiniSettings, level: Verbosity) -> MiniSettings {
    let [thinking, tools, shell_output, turn_summary, footer] = preset(level);
    MiniSettings { thinking, tools, shell_output, turn_summary, footer, ..settings.clone() }
}

/// `verbosityLabel(level)`; `None` is "Custom".
pub fn verbosity_label(level: Option<Verbosity>) -> &'static str {
    match level {
        Some(Verbosity::Quiet) => "Quiet",
        Some(Verbosity::Default) => "Default",
        Some(Verbosity::Everything) => "Everything",
        None => "Custom",
    }
}

pub fn verbosity_value(level: Verbosity) -> &'static str {
    match level {
        Verbosity::Quiet => "quiet",
        Verbosity::Default => "default",
        Verbosity::Everything => "everything",
    }
}

/// `matchMiniVerbosity(settings)`; `None` is "custom".
pub fn match_mini_verbosity(settings: &MiniSettings) -> Option<Verbosity> {
    LEVELS.into_iter().find(|l| preset(*l) == knobs(settings))
}

fn distance(settings: &MiniSettings, level: Verbosity) -> usize {
    knobs(settings).iter().zip(preset(level)).filter(|(a, b)| **a != *b).count()
}

fn nearest(settings: &MiniSettings) -> usize {
    let mut best = 0;
    for (index, level) in LEVELS.iter().enumerate() {
        if distance(settings, *level) < distance(settings, LEVELS[best]) {
            best = index;
        }
    }
    best
}

/// `cycleMiniVerbosity(settings, direction)`: a clamped slider.
pub fn cycle_mini_verbosity(settings: &MiniSettings, direction: i32) -> Verbosity {
    let index = match match_mini_verbosity(settings) {
        Some(level) => LEVELS.iter().position(|l| *l == level).unwrap() as i32,
        None => nearest(settings) as i32,
    };
    let next = index + direction;
    LEVELS[next.clamp(0, LEVELS.len() as i32 - 1) as usize]
}

/// `verbosityChange(settings, direction)`.
pub fn verbosity_change(settings: &MiniSettings, direction: i32) -> Option<MiniSettingChange> {
    let value = cycle_mini_verbosity(settings, direction);
    if match_mini_verbosity(settings) == Some(value) {
        return None;
    }
    Some(MiniSettingChange::Verbosity(value))
}

/// `applyMiniSettingChange(settings, change)`.
pub fn apply_mini_setting_change(settings: &MiniSettings, change: &MiniSettingChange) -> MiniSettings {
    let mut s = settings.clone();
    match change {
        MiniSettingChange::Verbosity(level) => return apply_preset(settings, *level),
        MiniSettingChange::Thinking(v) => s.thinking = *v,
        MiniSettingChange::Tools(v) => s.tools = *v,
        MiniSettingChange::ShellOutput(v) => s.shell_output = *v,
        MiniSettingChange::TurnSummary(v) => s.turn_summary = *v,
        MiniSettingChange::Footer(v) => s.footer = *v,
        MiniSettingChange::Splash(v) => s.splash = *v,
        MiniSettingChange::WorkSpinner(v) => s.work_spinner = v.clone(),
        MiniSettingChange::Mono(v) => s.mono = *v,
    }
    s
}

/// The `mini.*` config writes for a change (`onMiniSettingChange` in `runtime.ts`).
pub fn change_writes(change: &MiniSettingChange) -> Vec<(&'static str, Value)> {
    let show = |v: bool| Value::String(super::types::show(v).into());
    match change {
        MiniSettingChange::Verbosity(level) => {
            let [thinking, tools, shell_output, turn_summary, footer] = preset(*level);
            vec![("thinking", show(thinking)), ("tools", show(tools)), ("shell_output", show(shell_output)), ("turn_summary", show(turn_summary)), ("footer", show(footer))]
        }
        MiniSettingChange::Thinking(v) => vec![("thinking", show(*v))],
        MiniSettingChange::Tools(v) => vec![("tools", show(*v))],
        MiniSettingChange::ShellOutput(v) => vec![("shell_output", show(*v))],
        MiniSettingChange::TurnSummary(v) => vec![("turn_summary", show(*v))],
        MiniSettingChange::Footer(v) => vec![("footer", show(*v))],
        MiniSettingChange::Splash(v) => vec![("splash", show(*v))],
        MiniSettingChange::WorkSpinner(v) => vec![("work_spinner", Value::String(v.clone()))],
        MiniSettingChange::Mono(v) => vec![("mono", Value::Bool(*v))],
    }
}

/// `Config.MiniWorkSpinner.literals` (order matters for cycling).
pub const WORK_SPINNER_NAMES: [&str; 14] = [
    "block-soft-slide",
    "block-soft-sweep",
    "block-low-comet",
    "block-low-duet",
    "block-shuttle",
    "block-bridge",
    "block-squeeze",
    "small-toggle",
    "square-toggle",
    "grow-shrink",
    "quadrant-orbit",
    "crosshatch",
    "density-wave",
    "seed",
];

/// `resolveMiniSettings(config)` from the resolved `mini` config object.
pub fn resolve_mini_settings(mini: Option<&Value>) -> MiniSettings {
    let get = |k: &str| mini.and_then(|m| m.get(k));
    let show = |k: &str, default: bool| match get(k).and_then(|v| v.as_str()) {
        Some("show") => true,
        Some("hide") => false,
        _ => default,
    };
    let d = MiniSettings::default();
    MiniSettings {
        thinking: show("thinking", d.thinking),
        tools: show("tools", d.tools),
        shell_output: show("shell_output", d.shell_output),
        turn_summary: show("turn_summary", d.turn_summary),
        footer: show("footer", d.footer),
        splash: show("splash", d.splash),
        work_spinner: get("work_spinner").and_then(|v| v.as_str()).filter(|s| WORK_SPINNER_NAMES.contains(s)).map(|s| s.to_string()).unwrap_or(d.work_spinner),
        mono: get("mono").and_then(|v| v.as_bool()).unwrap_or(d.mono),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn default_settings_match_default_preset() {
        assert_eq!(match_mini_verbosity(&resolve_mini_settings(None)), Some(Verbosity::Default));
    }

    #[test]
    fn presets_leave_splash_spinner_and_mono() {
        let current = resolve_mini_settings(Some(&json!({"splash": "hide", "work_spinner": "seed", "mono": true})));
        let quiet = apply_mini_setting_change(&current, &MiniSettingChange::Verbosity(Verbosity::Quiet));
        assert_eq!(quiet, apply_preset(&current, Verbosity::Quiet));
        assert!(!quiet.splash);
        assert!(!quiet.footer);
        assert!(apply_mini_setting_change(&current, &MiniSettingChange::Verbosity(Verbosity::Everything)).mono);
        assert!(apply_mini_setting_change(&current, &MiniSettingChange::Thinking(true)).thinking);
    }

    #[test]
    fn knobs_mark_custom() {
        let louder = apply_mini_setting_change(&resolve_mini_settings(None), &MiniSettingChange::Tools(true));
        assert_eq!(match_mini_verbosity(&louder), None);
        assert_eq!(verbosity_label(None), "Custom");
        assert_eq!(match_mini_verbosity(&apply_mini_setting_change(&louder, &MiniSettingChange::Verbosity(Verbosity::Quiet))), Some(Verbosity::Quiet));
    }

    #[test]
    fn cycles_like_clamped_slider() {
        let s = resolve_mini_settings(None);
        assert_eq!(cycle_mini_verbosity(&s, 1), Verbosity::Everything);
        assert_eq!(cycle_mini_verbosity(&s, -1), Verbosity::Quiet);
        assert_eq!(cycle_mini_verbosity(&apply_preset(&s, Verbosity::Quiet), -1), Verbosity::Quiet);
        assert_eq!(cycle_mini_verbosity(&apply_preset(&s, Verbosity::Everything), 1), Verbosity::Everything);
        assert_eq!(verbosity_change(&apply_preset(&s, Verbosity::Quiet), -1), None);
        assert_eq!(verbosity_change(&s, 1), Some(MiniSettingChange::Verbosity(Verbosity::Everything)));
        assert_eq!(verbosity_label(Some(Verbosity::Quiet)), "Quiet");
        assert_eq!(verbosity_label(Some(Verbosity::Everything)), "Everything");
    }

    #[test]
    fn custom_moves_to_nearest() {
        let custom = apply_mini_setting_change(&resolve_mini_settings(None), &MiniSettingChange::Tools(true));
        assert_eq!(match_mini_verbosity(&custom), None);
        assert_eq!(cycle_mini_verbosity(&custom, 1), Verbosity::Everything);
        assert_eq!(cycle_mini_verbosity(&custom, -1), Verbosity::Quiet);
    }
}

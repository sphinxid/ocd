//! Keymap: key-string parsing (`@opentui/keymap` default parser), binding lookup with user
//! overrides, crossterm event normalization, shortcut formatting and leader/sequence state.

pub mod defs;

use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use serde_json::Value;
use std::collections::HashMap;

/// One stroke of a key sequence.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Default)]
pub struct Stroke {
    pub name: String,
    pub ctrl: bool,
    pub shift: bool,
    pub meta: bool,
    pub super_: bool,
    pub hyper: bool,
    /// `<leader>` token.
    pub leader: bool,
}

impl Stroke {
    pub fn key(name: &str) -> Stroke {
        Stroke { name: name.to_string(), ..Default::default() }
    }
    pub fn ctrl(name: &str) -> Stroke {
        Stroke { name: name.to_string(), ctrl: true, ..Default::default() }
    }

    /// Whether `self` (from a binding) matches the actual event stroke `ev`.
    pub fn matches(&self, ev: &Stroke) -> bool {
        if self.name != ev.name
            || self.ctrl != ev.ctrl
            || self.meta != ev.meta
            || self.super_ != ev.super_
            || self.hyper != ev.hyper
        {
            return false;
        }
        if self.shift == ev.shift {
            return true;
        }
        // Symbols typed with shift (e.g. `?`) arrive with or without the modifier depending on the
        // terminal protocol; treat them as equivalent.
        let single = self.name.chars().count() == 1 && !self.name.chars().next().unwrap().is_ascii_alphanumeric();
        single
    }

    pub fn is_char(&self) -> Option<char> {
        if self.ctrl || self.meta || self.super_ || self.leader {
            return None;
        }
        let mut cs = self.name.chars();
        let c = cs.next()?;
        if cs.next().is_some() {
            return if self.name == "space" { Some(' ') } else { None };
        }
        Some(if self.shift { c.to_ascii_uppercase() } else { c })
    }
}

pub type Sequence = Vec<Stroke>;

const NAMED: &[&str] = &[
    "pagedown", "pageup", "backspace", "linefeed", "backquote", "backslash", "semicolon", "leftbracket",
    "rightbracket", "escape", "return", "insert", "delete", "period", "comma", "equal", "minus", "slash", "quote",
    "space", "clear", "right", "enter", "left", "home", "plus", "down", "end", "tab", "up", "lt", "gt",
];

fn named_alias(name: &str) -> String {
    match name {
        "enter" => "return".into(),
        "esc" => "escape".into(),
        "pgdown" => "pagedown".into(),
        "pgup" => "pageup".into(),
        "lt" => "<".into(),
        "gt" => ">".into(),
        "plus" => "+".into(),
        "minus" => "-".into(),
        "equal" => "=".into(),
        "comma" => ",".into(),
        "period" => ".".into(),
        "slash" => "/".into(),
        "backslash" => "\\".into(),
        "semicolon" => ";".into(),
        "quote" => "'".into(),
        "backquote" => "`".into(),
        "leftbracket" => "[".into(),
        "rightbracket" => "]".into(),
        other => other.to_string(),
    }
}

/// Expand whole-word aliases (`enter`, `esc`, `pgdown`, `pgup`).
fn expand_aliases(s: &str) -> String {
    let re = regex::Regex::new(r"(?i)(^|[+,\s>])(enter|esc|pgdown|pgup)($|[+,\s<])").unwrap();
    let mut out = s.to_string();
    // Apply repeatedly to handle adjacent matches.
    loop {
        let next = re
            .replace_all(&out, |c: &regex::Captures| {
                let word = match c[2].to_lowercase().as_str() {
                    "enter" => "return",
                    "esc" => "escape",
                    "pgdown" => "pagedown",
                    _ => "pageup",
                };
                format!("{}{}{}", &c[1], word, &c[3])
            })
            .to_string();
        if next == out {
            return out;
        }
        out = next;
    }
}

/// Parse a single (comma-free) key string into a sequence.
pub fn parse_sequence(input: &str) -> Result<Sequence, String> {
    let s = expand_aliases(input.trim());
    if s.is_empty() {
        return Err(format!("Invalid key sequence \"{input}\""));
    }
    if s == "+" {
        return Ok(vec![Stroke::key("+")]);
    }
    let chars: Vec<char> = s.chars().collect();
    let mut i = 0;
    let mut out = Vec::new();
    while i < chars.len() {
        if chars[i] == '<' {
            if let Some(end) = chars[i..].iter().position(|c| *c == '>') {
                let token: String = chars[i + 1..i + end].iter().collect();
                if token.eq_ignore_ascii_case("leader") {
                    out.push(Stroke { leader: true, ..Default::default() });
                    i += end + 1;
                    continue;
                }
                return Err(format!("Unknown token <{token}>"));
            }
        }
        if chars[i] == ' ' {
            out.push(Stroke::key("space"));
            i += 1;
            continue;
        }
        let mut stroke = Stroke::default();
        // Modifiers.
        loop {
            let rest: String = chars[i..].iter().collect::<String>().to_lowercase();
            let mut matched = false;
            for (m, len) in [
                ("ctrl+", 5),
                ("control+", 8),
                ("shift+", 6),
                ("meta+", 5),
                ("alt+", 4),
                ("option+", 7),
                ("super+", 6),
                ("hyper+", 6),
            ] {
                if rest.starts_with(m) && chars.len() > i + len {
                    match m {
                        "ctrl+" | "control+" => stroke.ctrl = true,
                        "shift+" => stroke.shift = true,
                        "meta+" | "alt+" | "option+" => stroke.meta = true,
                        "super+" => stroke.super_ = true,
                        _ => stroke.hyper = true,
                    }
                    i += len;
                    matched = true;
                    break;
                }
            }
            if !matched {
                break;
            }
        }
        // Key: longest named key at position, else f-keys, else single char.
        let rest: String = chars[i..].iter().collect::<String>();
        let lower = rest.to_lowercase();
        let mut name: Option<(String, usize)> = None;
        if let Some(caps) = regex::Regex::new(r"^f([1-9][0-9]?)").unwrap().captures(&lower) {
            let len = caps[0].len();
            // Only treat as function key when it is not followed by more letters.
            if !lower[len..].chars().next().is_some_and(|c| c.is_ascii_alphabetic()) {
                name = Some((caps[0].to_string(), len));
            }
        }
        if name.is_none() {
            let mut best: Option<&str> = None;
            for n in NAMED {
                if lower.starts_with(n) && best.is_none_or(|b| n.len() > b.len()) {
                    best = Some(n);
                }
            }
            if let Some(b) = best {
                name = Some((named_alias(b), b.chars().count()));
            }
        }
        let (key, len) = match name {
            Some(n) => n,
            None => {
                let c = chars[i];
                (c.to_lowercase().to_string(), 1)
            }
        };
        stroke.name = key;
        out.push(stroke);
        i += len;
    }
    Ok(out)
}

/// Parse a binding string with comma alternatives.
pub fn parse_bindings(s: &str) -> Result<Vec<Sequence>, String> {
    let mut out = Vec::new();
    for part in s.split(',') {
        let p = part.trim();
        if p.is_empty() {
            return Err(format!(
                "Invalid key sequence \"{s}\": comma-separated bindings cannot contain empty entries"
            ));
        }
        out.push(parse_sequence(p)?);
    }
    Ok(out)
}

/// Normalize a crossterm key event into a stroke. Returns `None` for releases.
pub fn stroke_from_event(ev: &KeyEvent) -> Option<Stroke> {
    if ev.kind == KeyEventKind::Release {
        return None;
    }
    let m = ev.modifiers;
    let mut s = Stroke {
        ctrl: m.contains(KeyModifiers::CONTROL),
        shift: m.contains(KeyModifiers::SHIFT),
        meta: m.contains(KeyModifiers::ALT) || m.contains(KeyModifiers::META),
        super_: m.contains(KeyModifiers::SUPER),
        hyper: m.contains(KeyModifiers::HYPER),
        ..Default::default()
    };
    s.name = match ev.code {
        KeyCode::Enter => "return".into(),
        KeyCode::Esc => "escape".into(),
        KeyCode::Tab => "tab".into(),
        KeyCode::BackTab => {
            s.shift = true;
            "tab".into()
        }
        KeyCode::Backspace => "backspace".into(),
        KeyCode::Delete => "delete".into(),
        KeyCode::Insert => "insert".into(),
        KeyCode::Home => "home".into(),
        KeyCode::End => "end".into(),
        KeyCode::PageUp => "pageup".into(),
        KeyCode::PageDown => "pagedown".into(),
        KeyCode::Up => "up".into(),
        KeyCode::Down => "down".into(),
        KeyCode::Left => "left".into(),
        KeyCode::Right => "right".into(),
        KeyCode::F(n) => format!("f{n}"),
        KeyCode::Char(' ') => "space".into(),
        KeyCode::Char(c) => {
            if c.is_ascii_uppercase() {
                s.shift = true;
                c.to_ascii_lowercase().to_string()
            } else {
                // Shift is implied by the produced symbol for non-letters.
                if !c.is_ascii_alphanumeric() && !s.ctrl && !s.meta {
                    s.shift = false;
                }
                c.to_lowercase().to_string()
            }
        }
        _ => return None,
    };
    Some(s)
}

fn key_display(name: &str) -> String {
    match name {
        "return" => "enter".into(),
        "up" => "↑".into(),
        "down" => "↓".into(),
        "left" => "←".into(),
        "right" => "→".into(),
        "pageup" => "pgup".into(),
        "pagedown" => "pgdn".into(),
        "delete" => "del".into(),
        other => other.to_string(),
    }
}

/// Format one stroke: modifiers in fixed order ctrl, shift, alt, super, hyper.
pub fn format_stroke(s: &Stroke, leader: &str) -> String {
    if s.leader {
        return leader.to_string();
    }
    let mut parts: Vec<String> = Vec::new();
    if s.ctrl {
        parts.push("ctrl".into());
    }
    if s.shift {
        parts.push("shift".into());
    }
    if s.meta {
        parts.push("alt".into());
    }
    if s.super_ {
        parts.push("super".into());
    }
    if s.hyper {
        parts.push("hyper".into());
    }
    parts.push(key_display(&s.name));
    parts.join("+")
}

pub fn format_sequence(seq: &Sequence, leader: &str) -> String {
    seq.iter().map(|s| format_stroke(s, leader)).collect::<Vec<_>>().join(" ")
}

/// Resolved keybinds: defaults merged with user overrides.
#[derive(Clone, Debug)]
pub struct Keybinds {
    map: HashMap<String, Vec<Sequence>>,
    pub leader: Stroke,
    pub leader_text: String,
    pub leader_timeout_ms: u64,
}

fn binding_strings(v: &Value) -> Vec<String> {
    match v {
        Value::Bool(false) => vec![],
        Value::String(s) if s == "none" => vec![],
        Value::String(s) => vec![s.clone()],
        Value::Array(items) => items.iter().flat_map(binding_strings).collect(),
        Value::Object(o) => match o.get("key") {
            Some(Value::String(k)) => vec![k.clone()],
            Some(Value::Object(ks)) => vec![stroke_object_string(ks)],
            _ => {
                if o.contains_key("name") {
                    vec![stroke_object_string(o)]
                } else {
                    vec![]
                }
            }
        },
        _ => vec![],
    }
}

fn stroke_object_string(o: &serde_json::Map<String, Value>) -> String {
    let mut parts = Vec::new();
    for m in ["ctrl", "shift", "meta", "super", "hyper"] {
        if o.get(m).and_then(|v| v.as_bool()) == Some(true) {
            parts.push(if m == "meta" { "alt".to_string() } else { m.to_string() });
        }
    }
    parts.push(o.get("name").and_then(|v| v.as_str()).unwrap_or("").to_string());
    parts.join("+")
}

impl Keybinds {
    /// Build from user overrides (`keybinds` object of cli.json).
    pub fn new(overrides: Option<&Value>, leader_timeout: u64, terminal_suspend: bool) -> Keybinds {
        let mut map = HashMap::new();
        let empty = serde_json::Map::new();
        let over = overrides.and_then(|o| o.as_object()).unwrap_or(&empty);
        for (id, default, _) in defs::DEFINITIONS {
            let mut strings = match over.get(*id) {
                Some(v) => binding_strings(v),
                None => binding_strings(&Value::String(default.to_string())),
            };
            if !terminal_suspend {
                if *id == "terminal.suspend" {
                    strings.clear();
                }
                if *id == "input.undo" && !over.contains_key("input.undo") {
                    let mut items = vec!["ctrl+z".to_string()];
                    for s in default.split(',') {
                        if !items.iter().any(|x| x == s) {
                            items.push(s.to_string());
                        }
                    }
                    strings = vec![items.join(",")];
                }
            }
            let mut seqs = Vec::new();
            for s in strings {
                if let Ok(mut parsed) = parse_bindings(&s) {
                    seqs.append(&mut parsed);
                }
            }
            map.insert(id.to_string(), seqs);
        }
        let leader_text = match over.get("leader") {
            Some(v) => binding_strings(v).into_iter().next().unwrap_or_else(|| "ctrl+x".into()),
            None => "ctrl+x".into(),
        };
        let leader = parse_sequence(&leader_text)
            .ok()
            .and_then(|s| s.into_iter().next())
            .unwrap_or_else(|| Stroke::ctrl("x"));
        Keybinds { map, leader, leader_text, leader_timeout_ms: leader_timeout }
    }

    pub fn get(&self, id: &str) -> &[Sequence] {
        self.map.get(id).map(|v| v.as_slice()).unwrap_or(&[])
    }

    /// Replace the `<leader>` token with the leader key stroke.
    fn expand(&self, seq: &Sequence) -> Sequence {
        seq.iter().map(|s| if s.leader { self.leader.clone() } else { s.clone() }).collect()
    }

    /// `shortcuts.get(id)`: the first binding formatted, or "".
    pub fn shortcut(&self, id: &str) -> String {
        self.get(id).first().map(|s| format_sequence(s, &self.leader_text)).unwrap_or_default()
    }

    /// `shortcuts.all(id)`: all bindings joined by ", " (deduplicated).
    pub fn shortcut_all(&self, id: &str) -> String {
        let mut out: Vec<String> = Vec::new();
        for s in self.get(id) {
            let f = format_sequence(s, &self.leader_text);
            if !out.contains(&f) {
                out.push(f);
            }
        }
        out.join(", ")
    }

    /// Match state of `pending` (already typed strokes) against a command's bindings.
    pub fn match_command(&self, id: &str, pending: &[Stroke]) -> Match {
        let mut result = Match::None;
        for seq in self.get(id) {
            let seq = self.expand(seq);
            if seq.len() < pending.len() {
                continue;
            }
            if seq.iter().zip(pending).all(|(b, e)| b.matches(e)) {
                if seq.len() == pending.len() {
                    return Match::Full;
                }
                result = Match::Prefix;
            }
        }
        result
    }

    pub fn is_leader(&self, s: &Stroke) -> bool {
        self.leader.matches(s)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Match {
    None,
    Prefix,
    Full,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_basic() {
        let s = parse_sequence("ctrl+x").unwrap();
        assert_eq!(s, vec![Stroke::ctrl("x")]);
        let s = parse_sequence("<leader>n").unwrap();
        assert!(s[0].leader);
        assert_eq!(s[1].name, "n");
        let s = parse_sequence("gg").unwrap();
        assert_eq!(s.len(), 2);
        let s = parse_sequence("<leader>return").unwrap();
        assert_eq!(s[1].name, "return");
        let s = parse_sequence("pgup").unwrap();
        assert_eq!(s[0].name, "pageup");
        let s = parse_sequence("enter").unwrap();
        assert_eq!(s[0].name, "return");
        let s = parse_sequence("alt+shift+down").unwrap();
        assert!(s[0].meta && s[0].shift && s[0].name == "down");
        let s = parse_sequence("ctrl+-").unwrap();
        assert_eq!(s[0].name, "-");
        let s = parse_sequence("shift+?").unwrap();
        assert_eq!(s[0].name, "?");
        let s = parse_sequence("f2").unwrap();
        assert_eq!(s[0].name, "f2");
        let s = parse_sequence("ctrl+alt+[").unwrap();
        assert!(s[0].ctrl && s[0].meta && s[0].name == "[");
        let s = parse_sequence("shift+g").unwrap();
        assert!(s[0].shift && s[0].name == "g");
        assert!(parse_bindings("a,,b").is_err());
        assert!(parse_sequence("<foo>").is_err());
    }

    #[test]
    fn format() {
        let kb = Keybinds::new(None, 2000, true);
        assert_eq!(kb.shortcut("session.new"), "ctrl+x n");
        assert_eq!(kb.shortcut("app.exit"), "ctrl+c");
        assert_eq!(kb.shortcut_all("session.tab.next"), "ctrl+tab, alt+↓");
        assert_eq!(kb.shortcut("prompt.queue"), "ctrl+x enter");
        assert_eq!(kb.shortcut("session.page.up"), "pgup");
        assert_eq!(kb.shortcut("agent.cycle"), "shift+tab");
        assert_eq!(kb.shortcut("command.palette.show"), "ctrl+p");
        assert_eq!(kb.shortcut("session.tab.next_unread"), "shift+alt+↓");
        assert_eq!(kb.shortcut("theme.switch"), "");
        assert_eq!(kb.shortcut("diff.first"), "g g");
    }

    #[test]
    fn overrides_and_platform() {
        let over = serde_json::json!({"session.new": "none", "leader": "ctrl+a", "model.list": ["<leader>m", "f9"]});
        let kb = Keybinds::new(Some(&over), 2000, false);
        assert_eq!(kb.get("session.new").len(), 0);
        assert_eq!(kb.shortcut_all("model.list"), "ctrl+a m, f9");
        assert_eq!(kb.get("terminal.suspend").len(), 0);
        assert_eq!(kb.shortcut_all("input.undo"), "ctrl+z, ctrl+-, super+z");
    }

    #[test]
    fn matching() {
        let kb = Keybinds::new(None, 2000, true);
        let leader = Stroke::ctrl("x");
        assert_eq!(kb.match_command("session.new", &[leader.clone()]), Match::Prefix);
        assert_eq!(kb.match_command("session.new", &[leader, Stroke::key("n")]), Match::Full);
        let ev = stroke_from_event(&KeyEvent::new(KeyCode::Char('?'), KeyModifiers::SHIFT)).unwrap();
        assert_eq!(kb.match_command("diff.help", &[ev]), Match::Full);
        let ev = stroke_from_event(&KeyEvent::new(KeyCode::Char('G'), KeyModifiers::SHIFT)).unwrap();
        assert_eq!(kb.match_command("diff.last", &[ev]), Match::Full);
        assert_eq!(defs::DEFINITIONS.len(), 244);
    }
}

//! Port of `src/util/locale.ts` plus small formatting helpers.

use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

/// Display width of a string (terminal cells).
pub fn width(s: &str) -> usize {
    UnicodeWidthStr::width(s)
}

/// Uppercase the first letter of each `\b\w` word.
pub fn titlecase(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut prev_word = false;
    for c in s.chars() {
        let is_word = c.is_alphanumeric() || c == '_';
        if is_word && !prev_word {
            out.extend(c.to_uppercase());
        } else {
            out.push(c);
        }
        prev_word = is_word;
    }
    out
}

/// `Locale.number`: 1.2K / 3.4M / integer.
pub fn number(n: f64) -> String {
    // 999950+ would round to "1000.0K".
    if n >= 999_950.0 {
        format!("{:.1}M", n / 1_000_000.0)
    } else if n >= 1000.0 {
        format!("{:.1}K", n / 1000.0)
    } else {
        js_number(n)
    }
}

/// `String.prototype.localeCompare` approximation: case-insensitive first, lowercase before uppercase on ties.
pub fn compare(a: &str, b: &str) -> std::cmp::Ordering {
    a.to_lowercase().cmp(&b.to_lowercase()).then_with(|| b.cmp(a))
}

/// JS `Number.prototype.toString` for typical values.
pub fn js_number(n: f64) -> String {
    if n.fract() == 0.0 && n.abs() < 1e21 {
        format!("{}", n as i64)
    } else {
        let s = format!("{}", n);
        s
    }
}

/// `Locale.duration(ms)`.
pub fn duration(ms: i64) -> String {
    if ms < 1000 {
        return format!("{ms}ms");
    }
    if ms < 60_000 {
        return format!("{:.1}s", ms as f64 / 1000.0);
    }
    if ms < 3_600_000 {
        let m = ms / 60_000;
        let s = (ms % 60_000) / 1000;
        return format!("{m}m {s}s");
    }
    if ms < 86_400_000 {
        let h = ms / 3_600_000;
        let m = (ms % 3_600_000) / 60_000;
        return format!("{h}h {m}m");
    }
    let d = ms / 86_400_000;
    let h = (ms % 86_400_000) / 3_600_000;
    format!("{d}d {h}h")
}

/// `Locale.truncate`: by UTF-16-ish char count (we use chars).
pub fn truncate(s: &str, len: usize) -> String {
    let count = s.chars().count();
    if count <= len {
        return s.to_string();
    }
    let keep: String = s.chars().take(len.saturating_sub(1)).collect();
    format!("{keep}…")
}

pub fn truncate_left(s: &str, len: usize) -> String {
    let count = s.chars().count();
    if count <= len {
        return s.to_string();
    }
    let skip = count - (len.saturating_sub(1));
    let tail: String = s.chars().skip(skip).collect();
    format!("…{tail}")
}

pub fn truncate_middle(s: &str, max: usize) -> String {
    if max == 0 {
        return String::new();
    }
    let count = s.chars().count();
    if count <= max {
        return s.to_string();
    }
    let keep_start = (max.saturating_sub(1) + 1) / 2;
    let keep_end = max.saturating_sub(1) / 2;
    let start: String = s.chars().take(keep_start).collect();
    if keep_end == 0 {
        return format!("{start}…");
    }
    let end: String = s.chars().skip(count - keep_end).collect();
    format!("{start}…{end}")
}

pub fn graphemes(s: &str) -> Vec<&str> {
    UnicodeSegmentation::graphemes(s, true).collect()
}

pub fn take_width(s: &str, w: usize) -> String {
    if w == 0 {
        return String::new();
    }
    if width(s) <= w {
        return s.to_string();
    }
    let mut out = String::new();
    let mut used = 0;
    for g in graphemes(s) {
        let n = width(g);
        if used + n > w {
            break;
        }
        out.push_str(g);
        used += n;
    }
    out
}

pub fn take_end_width(s: &str, w: usize) -> String {
    if w == 0 {
        return String::new();
    }
    if width(s) <= w {
        return s.to_string();
    }
    let gs = graphemes(s);
    let mut out: Vec<&str> = Vec::new();
    let mut used = 0;
    for g in gs.iter().rev() {
        let n = width(g);
        if used + n > w {
            break;
        }
        out.push(g);
        used += n;
    }
    out.reverse();
    out.concat()
}

pub fn truncate_width(s: &str, w: usize) -> String {
    if w == 0 {
        return String::new();
    }
    if width(s) <= w {
        return s.to_string();
    }
    if w == 1 {
        return "…".into();
    }
    format!("{}…", take_width(s, w - 1))
}

/// Middle truncate by display width.
pub fn truncate_middle_width(s: &str, max: usize) -> String {
    if width(s) <= max {
        return s.to_string();
    }
    if max <= 1 {
        return take_width("…", max);
    }
    let keep_start = max / 2;
    let keep_end = max - 1 - keep_start;
    format!("{}…{}", take_width(s, keep_start), take_end_width(s, keep_end))
}

/// JS `toLocaleString()` for integers (en-US grouping).
pub fn grouped(n: i64) -> String {
    let neg = n < 0;
    let digits = n.unsigned_abs().to_string();
    let mut out = String::new();
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i) % 3 == 0 {
            out.push(',');
        }
        out.push(c);
    }
    if neg { format!("-{out}") } else { out }
}

/// Intl USD currency format: `$1,234.56`.
pub fn usd(v: f64) -> String {
    let cents = (v * 100.0).round() as i64;
    let whole = cents / 100;
    let frac = (cents % 100).abs();
    format!("${}.{:02}", grouped(whole), frac)
}

/// `Locale.time` short style, e.g. `3:04 PM`.
pub fn time(ms: i64) -> String {
    use chrono::{Local, TimeZone};
    match Local.timestamp_millis_opt(ms).single() {
        Some(dt) => dt.format("%-I:%M %p").to_string(),
        None => String::new(),
    }
}

/// `freeLimitReset`: when a free allowance resets, `retry_after` ms after `base`: a time when that
/// is today and under a day away, else a date and time; nothing beyond 7 days.
pub fn free_limit_reset(retry_after: Option<f64>, base: i64, now: i64) -> Option<String> {
    let delay = retry_after.filter(|d| d.is_finite() && *d >= 0.0 && *d <= 7.0 * 86_400_000.0)?;
    let at = base + delay as i64;
    if date_string(at) == date_string(now) && at - now < 86_400_000 { Some(time(at)) } else { Some(datetime(at)) }
}

/// `Locale.datetime`: `time · date`.
pub fn datetime(ms: i64) -> String {
    use chrono::{Local, TimeZone};
    match Local.timestamp_millis_opt(ms).single() {
        Some(dt) => format!("{} · {}", time(ms), dt.format("%-m/%-d/%Y")),
        None => String::new(),
    }
}

/// JS `Date.toDateString()`, e.g. `Mon Oct 12 2026`.
pub fn date_string(ms: i64) -> String {
    use chrono::{Local, TimeZone};
    match Local.timestamp_millis_opt(ms).single() {
        Some(dt) => dt.format("%a %b %d %Y").to_string(),
        None => String::new(),
    }
}

/// ISO-8601 string as produced by `Date.toISOString()`.
pub fn iso(ms: i64) -> String {
    use chrono::{TimeZone, Utc};
    match Utc.timestamp_millis_opt(ms).single() {
        Some(dt) => dt.format("%Y-%m-%dT%H:%M:%S%.3fZ").to_string(),
        None => String::new(),
    }
}

/// Collapse all whitespace runs to single spaces and trim.
pub fn collapse_ws(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

pub fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// Middle-elide a file path to `max` cells (port of `truncateFilePath`).
pub fn truncate_file_path(value: &str, max: usize) -> String {
    if max == 0 {
        return String::new();
    }
    if width(value) <= max {
        return value.to_string();
    }
    let sep = if !value.contains('/') && value.contains('\\') { '\\' } else { '/' };
    let root = if value.starts_with('/') { "/" } else { "" };
    let source = &value[root.len()..];
    let segments: Vec<&str> = source.split(sep).filter(|s| !s.is_empty()).collect();
    let basename = segments.last().copied().unwrap_or(value);
    if segments.len() < 2 {
        let rw = width(root);
        if rw >= max {
            return take_width(root, max);
        }
        return format!("{root}{}", truncate_basename(basename, max - rw));
    }
    let prefix = format!("{root}…{sep}");
    let pw = width(&prefix);
    if max <= pw {
        return take_width(&prefix, max);
    }
    let bw = max - pw;
    let compact = truncate_basename(basename, bw);
    if compact != basename {
        return format!("{prefix}{compact}");
    }
    let mut selected: Vec<String> = vec![basename.to_string()];
    let sw = 1;
    let mut w = width(&prefix) + width(basename);
    for i in (0..segments.len() - 1).rev() {
        let seg = segments[i];
        let next = width(seg) + sw;
        if w + next > max {
            let available = max as isize - w as isize - sw as isize;
            if available > 1 {
                selected.insert(0, format!("{}…", take_width(seg, (available - 1) as usize)));
            }
            break;
        }
        selected.insert(0, seg.to_string());
        w += next;
    }
    format!("{prefix}{}", selected.join(&sep.to_string()))
}

fn truncate_basename(value: &str, max: usize) -> String {
    if width(value) <= max {
        return value.to_string();
    }
    if max <= 1 {
        return take_width("…", max);
    }
    let dot = value.rfind('.').filter(|d| *d > 0);
    let ext = dot.map(|d| &value[d..]).unwrap_or("");
    let ew = width(ext);
    if ew >= max {
        return format!("…{}", take_end_width(ext, max - 1));
    }
    let stem = dot.map(|d| &value[..d]).unwrap_or(value);
    format!("{}…{}", take_width(stem, max - ew - 1), ext)
}

/// Replace `$HOME` prefix by `~`.
pub fn abbreviate_home(path: &str) -> String {
    if let Some(home) = dirs::home_dir() {
        let h = home.to_string_lossy().to_string();
        if !h.is_empty() && h != "/" {
            if path == h {
                return "~".into();
            }
            if let Some(rest) = path.strip_prefix(&format!("{h}/")) {
                return format!("~/{rest}");
            }
        }
    }
    path.to_string()
}

pub fn basename(path: &str) -> &str {
    path.trim_end_matches('/').rsplit('/').next().unwrap_or(path)
}

/// Strip ANSI escape sequences.
pub fn strip_ansi(s: &str) -> String {
    static RE: once_cell::sync::Lazy<regex::Regex> = once_cell::sync::Lazy::new(|| {
        regex::Regex::new(r"\x1b\[[0-?]*[ -/]*[@-~]|\x1b\][^\x07\x1b]*(?:\x07|\x1b\\)|\x1b[@-Z\\-_]").unwrap()
    });
    RE.replace_all(s, "").to_string()
}

/// "Session title" fallback check: `^(New session|Child session) - <ISO>$`.
pub fn is_default_title(title: &str) -> bool {
    static RE: once_cell::sync::Lazy<regex::Regex> = once_cell::sync::Lazy::new(|| {
        regex::Regex::new(r"^(New session - |Child session - )\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}\.\d{3}Z$").unwrap()
    });
    RE.is_match(title)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn truncate_middle_narrow_widths() {
        let cases = [(0, ""), (1, "…"), (2, "a…"), (3, "a…j"), (4, "ab…j"), (9, "abcd…ghij"), (10, "abcdefghij")];
        for (max, want) in cases {
            assert_eq!(truncate_middle("abcdefghij", max), want, "max {max}");
        }
    }

    #[test]
    fn durations() {
        assert_eq!(duration(999), "999ms");
        assert_eq!(duration(2400), "2.4s");
        assert_eq!(duration(72_000), "1m 12s");
        assert_eq!(duration(3_700_000), "1h 1m");
        assert_eq!(duration(90_000_000), "1d 1h");
    }

    #[test]
    fn numbers() {
        assert_eq!(number(120300.0), "120.3K");
        assert_eq!(number(2_100_000.0), "2.1M");
        assert_eq!(number(999.0), "999");
        assert_eq!(grouped(46912), "46,912");
        assert_eq!(grouped(1234567), "1,234,567");
        assert_eq!(usd(0.123), "$0.12");
        assert_eq!(usd(1234.5), "$1,234.50");
    }

    #[test]
    fn truncation() {
        assert_eq!(truncate("hello world", 6), "hello…");
        assert_eq!(truncate("hi", 6), "hi");
        assert_eq!(truncate_left("hello world", 6), "…world");
        assert_eq!(truncate_middle("abcdefghij", 5), "ab…ij");
        assert_eq!(truncate_width("hello", 1), "…");
        assert_eq!(take_width("hello", 3), "hel");
        assert_eq!(titlecase("build agent"), "Build Agent");
        assert_eq!(titlecase("general"), "General");
    }

    #[test]
    fn file_path() {
        assert_eq!(truncate_file_path("/a/b/c.ts", 20), "/a/b/c.ts");
        assert_eq!(truncate_file_path("/home/user/project/src/file.ts", 20), "/…/proj…/src/file.ts");
        assert_eq!(truncate_file_path("/home/user/project/src/averyveryverylongname.ts", 16), "/…/averyvery….ts");
        assert_eq!(truncate_file_path("src/components/x.tsx", 12), "…/com…/x.tsx");
    }

    #[test]
    fn default_title() {
        assert!(is_default_title("New session - 2026-01-01T00:00:00.000Z"));
        assert!(!is_default_title("Fix bug"));
    }
}

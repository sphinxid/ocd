//! `opencode stats` (`handlers/stats.ts`, `util/src/activity-calendar.ts`).

use std::time::Duration;

use chrono::{Datelike, Local, NaiveDate, TimeZone};
use oc_util::Global;
use serde_json::{Value, json};

use super::{cwd, pretty, report, server_args};
use crate::cli::parse::Input;
use crate::client::Req;
use crate::connection;
use crate::term::{EOL, write_stdout};

pub struct Range {
    pub from: Option<i64>,
    pub to: i64,
    pub label: String,
}

fn local_midnight(y: i32, m: u32, d: u32) -> i64 {
    Local.with_ymd_and_hms(y, m, d, 0, 0, 0).earliest().map(|t| t.timestamp_millis()).unwrap_or(0)
}

pub fn stats_range(days: Option<i64>, year: Option<i64>, all: bool, now: chrono::DateTime<Local>) -> Range {
    let to = now.timestamp_millis() + 1;
    if all {
        return Range { from: None, to, label: "all time".into() };
    }
    if let Some(days) = days {
        let today = now.date_naive();
        let from_date = today - chrono::Days::new((days - 1).max(0) as u64);
        let from = local_midnight(from_date.year(), from_date.month(), from_date.day());
        let label = if days == 0 || days == 1 { "today".to_string() } else { format!("last {days} days") };
        return Range { from: Some(from), to, label };
    }
    let current = now.year() as i64;
    let year = year.unwrap_or(current);
    Range {
        from: Some(local_midnight(year as i32, 1, 1)),
        to: if year == current { to } else { local_midnight(year as i32 + 1, 1, 1) },
        label: if year == current { format!("{year} so far") } else { year.to_string() },
    }
}

/// `Intl.DateTimeFormat().resolvedOptions().timeZone || "UTC"`
pub fn timezone() -> String {
    if let Ok(tz) = std::env::var("TZ") {
        let tz = tz.trim_start_matches(':').to_string();
        if !tz.is_empty() {
            return tz;
        }
    }
    if let Ok(link) = std::fs::read_link("/etc/localtime") {
        let s = link.display().to_string();
        if let Some(i) = s.find("zoneinfo/") {
            return s[i + 9..].to_string();
        }
    }
    if let Ok(s) = std::fs::read_to_string("/etc/timezone") {
        let s = s.trim();
        if !s.is_empty() {
            return s.to_string();
        }
    }
    "UTC".into()
}

pub async fn run(global: &Global, input: &Input) -> anyhow::Result<i32> {
    let inner = async {
        let days = input.int("days");
        let year = input.int("year");
        let all = input.flag("all");
        if [days.is_some(), year.is_some(), all].iter().filter(|b| **b).count() > 1 {
            anyhow::bail!("--days, --year, and --all cannot be combined");
        }
        let server = connection::resolve(global, server_args(input)).await?;
        let client = server.client();
        let url = server.endpoint.url.clone();
        let reach = |e: crate::client::ClientError| -> anyhow::Error {
            if e.is_transport() { anyhow::anyhow!("Could not reach server at {url}") } else { e.into() }
        };
        let range = stats_range(days, year, all, Local::now());
        let project = input.str("project");
        let project_id = match project.as_deref() {
            Some(".") => client
                .call("location.get", Req::new().location(&cwd()).timeout(Duration::from_secs(30)))
                .await
                .map_err(reach)?
                .pointer("/project/id")
                .cloned(),
            Some(p) => Some(json!(p)),
            None => None,
        };
        let (models, tools, cost, full, as_json) =
            (input.flag("models"), input.flag("tools"), input.flag("cost"), input.flag("full"), input.flag("json"));
        let details = models || tools || cost || full;
        let mode = if as_json || tools || full {
            "detail"
        } else if details {
            "none"
        } else {
            "summary"
        };
        let mut req = Req::new();
        if let Some(f) = range.from {
            req = req.query("from", json!(f));
        }
        req = req.query("to", json!(range.to));
        if let Some(p) = project_id {
            req = req.query("project", p);
        }
        req = req.query("timezone", json!(timezone())).query("tools", json!(mode)).timeout(Duration::from_secs(30));
        let stats = client.call("experimental.session.stats", req).await.map_err(reach)?.get("data").cloned().unwrap_or(Value::Null);
        let output = if as_json {
            pretty(&stats)
        } else {
            render(
                &stats,
                &Options {
                    label: range.label,
                    scope: match project.as_deref() {
                        None => "all projects",
                        Some(".") => "current project",
                        Some(_) => "selected project",
                    }
                    .into(),
                    models: models || full,
                    tools: tools || full,
                    cost: cost || full,
                    limit: input.int("limit").unwrap_or(5).max(1) as usize,
                    color: crate::term::stdout_is_tty() && std::env::var_os("NO_COLOR").is_none(),
                    width: crate::term::columns().unwrap_or(80),
                },
            )
        };
        write_stdout(&format!("{output}{EOL}"));
        Ok(())
    };
    Ok(match inner.await {
        Ok(()) => 0,
        Err(e) => report(e),
    })
}

pub struct Options {
    pub label: String,
    pub scope: String,
    pub models: bool,
    pub tools: bool,
    pub cost: bool,
    pub limit: usize,
    pub color: bool,
    pub width: usize,
}

struct Palette {
    primary: &'static str,
    activity: [&'static str; 4],
}

fn palette() -> Palette {
    let bg = std::env::var("COLORFGBG").ok().and_then(|v| v.rsplit(';').next().and_then(|s| s.trim().parse::<f64>().ok()));
    match bg {
        Some(b) if b >= 7.0 => Palette {
            primary: "38;2;59;125;216",
            activity: ["38;2;153;169;192", "38;2;122;155;200", "38;2;90;140;208", "38;2;59;125;216"],
        },
        Some(_) => Palette {
            primary: "38;2;250;178;131",
            activity: ["38;2;117;99;87", "38;2;161;125;102", "38;2;206;152;116", "38;2;250;178;131"],
        },
        None => Palette { primary: "36", activity: ["2;36", "36", "1;36", "1;96"] },
    }
}

fn style(v: &str, code: &str, color: bool) -> String {
    if color { format!("\x1b[{code}m{v}\x1b[0m") } else { v.to_string() }
}

fn trim_decimal(v: f64) -> String {
    let s = format!("{v:.1}");
    s.strip_suffix(".0").map(String::from).unwrap_or(s)
}

/// `toLocaleString("en-US")` for integers.
pub fn grouped(n: i64) -> String {
    let s = n.abs().to_string();
    let mut out = String::new();
    for (i, c) in s.chars().enumerate() {
        if i > 0 && (s.len() - i) % 3 == 0 {
            out.push(',');
        }
        out.push(c);
    }
    if n < 0 { format!("-{out}") } else { out }
}

pub fn format_number(v: f64) -> String {
    if v >= 1e9 {
        format!("{}b", trim_decimal(v / 1e9))
    } else if v >= 1e6 {
        format!("{}m", trim_decimal(v / 1e6))
    } else if v >= 1e3 {
        format!("{}k", trim_decimal(v / 1e3))
    } else {
        grouped(v.round() as i64)
    }
}

pub fn format_percent(v: f64) -> String {
    if v >= 10.0 { format!("{v:.1}%") } else { format!("{v:.2}%") }
}

fn format_duration(v: f64) -> String {
    if v < 1000.0 { format!("{}ms", v.round() as i64) } else { format!("{}s", trim_decimal(v / 1000.0)) }
}

fn n(v: &Value, p: &str) -> f64 {
    v.pointer(p).and_then(Value::as_f64).unwrap_or(0.0)
}

fn total_tokens(t: &Value) -> f64 {
    n(t, "/input") + n(t, "/output") + n(t, "/reasoning") + n(t, "/cache/read") + n(t, "/cache/write")
}

fn metric(v: f64, noun: &str, p: &Palette, color: bool) -> String {
    format!("{} {noun}{}", style(&format_number(v), &format!("1;{}", p.primary), color), if v == 1.0 { "" } else { "s" })
}

fn truncate(v: &str, w: usize) -> String {
    let units: Vec<u16> = v.encode_utf16().collect();
    if units.len() <= w { v.to_string() } else { format!("{}…", String::from_utf16_lossy(&units[..w.saturating_sub(1)])) }
}

fn pad_start(s: &str, w: usize) -> String {
    format!("{}{s}", " ".repeat(w.saturating_sub(s.encode_utf16().count())))
}

fn table_row(a: &str, b: &str, c: &str, d: &str) -> String {
    format!("{}{}{}{}", super::auth::pad(&truncate(a, 34), 34), pad_start(b, 10), pad_start(c, 12), pad_start(d, 12))
}

pub fn render(stats: &Value, o: &Options) -> String {
    let p = palette();
    let primary = format!("1;{}", p.primary);
    let tools = stats.get("tools").cloned().unwrap_or(Value::Null);
    let tool_totals = (tools.get("mode").and_then(Value::as_str) != Some("none")).then(|| tools.get("totals").cloned().unwrap_or(Value::Null));
    let terminal = tool_totals.as_ref().map(|t| n(t, "/succeeded") + n(t, "/failed")).unwrap_or(0.0);
    let rate = match &tool_totals {
        Some(t) if terminal > 0.0 => Some(n(t, "/succeeded") / terminal * 100.0),
        _ => None,
    };
    let sessions = n(stats, "/sessions");
    let subagents = n(stats, "/subagents");
    let mut session_line = metric(sessions, "session", &p, o.color);
    if subagents > 0.0 {
        session_line.push_str(&format!(" · {}", metric(subagents, "subagent", &p, o.color)));
    }
    let tool_summary = match (&tool_totals, rate) {
        (None, _) => "tool stats unavailable".to_string(),
        (_, None) => "no tool calls".to_string(),
        (_, Some(r)) => format!("{} tool success", style(&format_percent(r), &primary, o.color)),
    };
    let details = o.models || o.tools || o.cost;
    let empty = sessions == 0.0 && n(stats, "/prompts") == 0.0 && n(stats, "/steps") == 0.0;
    let heading = format!(
        "{} {}",
        style("ocd stats", &primary, o.color),
        style(&format!("· {} · {}", o.label, o.scope), "2", o.color)
    );
    let mut lines: Vec<String> = if details {
        vec![style(&format!("{} · {}", o.label, o.scope), "2", o.color)]
    } else if empty {
        vec![heading, String::new(), style("no activity in this range", "2", o.color), String::new(), style("opencode.ai", "2", o.color)]
    } else {
        let streak = n(stats, "/streak");
        let mut l = vec![heading, String::new()];
        l.extend(render_activity(stats, o.color, o.width, &p));
        l.push(String::new());
        l.push(session_line);
        l.push(format!(
            "{} · {} · {}",
            metric(n(stats, "/prompts"), "prompt", &p, o.color),
            metric(n(stats, "/steps"), "step", &p, o.color),
            metric(total_tokens(stats.get("tokens").unwrap_or(&Value::Null)), "token", &p, o.color)
        ));
        l.push(format!(
            "{tool_summary} · {} · best streak {} day{}",
            metric(n(stats, "/activeDays"), "active day", &p, o.color),
            style(&(streak as i64).to_string(), &primary, o.color),
            if streak == 1.0 { "" } else { "s" }
        ));
        l.push(String::new());
        l.push(style("opencode.ai", "2", o.color));
        l
    };
    if o.cost {
        lines.push(String::new());
        let input = n(stats, "/tokens/input") + n(stats, "/tokens/cache/read") + n(stats, "/tokens/cache/write");
        let cached = if input == 0.0 { 0.0 } else { n(stats, "/tokens/cache/read") / input * 100.0 };
        let row = |l: &str, v: String| format!("  {l:<20}{v}");
        lines.push("COST & TOKENS".into());
        lines.push(row("cost", format!("${:.2}", n(stats, "/cost"))));
        lines.push(row("input", format_number(n(stats, "/tokens/input"))));
        lines.push(row("output", format_number(n(stats, "/tokens/output"))));
        lines.push(row("reasoning", format_number(n(stats, "/tokens/reasoning"))));
        lines.push(row("cache read", format_number(n(stats, "/tokens/cache/read"))));
        lines.push(row("cache write", format_number(n(stats, "/tokens/cache/write"))));
        lines.push(row("cached input", format_percent(cached)));
    }
    if o.models {
        lines.push(String::new());
        let models = stats.get("models").and_then(Value::as_array).cloned().unwrap_or_default();
        if models.is_empty() {
            lines.extend(["MODELS".to_string(), "  no model usage".to_string()]);
        } else {
            let shown = &models[..models.len().min(o.limit)];
            let more = models.len() - shown.len();
            lines.push("MODELS".into());
            let label = |m: &Value| {
                let variant = m.pointer("/model/variant").and_then(Value::as_str).filter(|v| !v.is_empty());
                format!(
                    "{}/{}{}",
                    m.pointer("/model/providerID").and_then(Value::as_str).unwrap_or(""),
                    m.pointer("/model/id").and_then(Value::as_str).unwrap_or(""),
                    variant.map(|v| format!("#{v}")).unwrap_or_default()
                )
            };
            if o.width < 68 {
                for m in shown {
                    lines.push(truncate(&label(m), o.width));
                    lines.push(format!(
                        "  {} tokens · {} steps · ${:.2}",
                        format_number(total_tokens(m.get("tokens").unwrap_or(&Value::Null))),
                        format_number(n(m, "/steps")),
                        n(m, "/cost")
                    ));
                }
            } else {
                lines.push(table_row("model", "tokens", "steps", "cost"));
                for m in shown {
                    lines.push(table_row(
                        &label(m),
                        &format_number(total_tokens(m.get("tokens").unwrap_or(&Value::Null))),
                        &format_number(n(m, "/steps")),
                        &format!("${:.2}", n(m, "/cost")),
                    ));
                }
            }
            if more > 0 {
                lines.push(String::new());
                lines.push(format!("+{} more model{}", grouped(more as i64), if more == 1 { "" } else { "s" }));
            }
        }
    }
    if o.tools {
        lines.push(String::new());
        if tools.get("mode").and_then(Value::as_str) != Some("detail") {
            lines.extend(["TOOL RELIABILITY".to_string(), "  tool details unavailable".to_string()]);
        } else {
            let usage = tools.get("usage").and_then(Value::as_array).cloned().unwrap_or_default();
            if usage.is_empty() {
                lines.extend(["TOOL RELIABILITY".to_string(), "  no tool calls".to_string()]);
            } else {
                let shown = &usage[..usage.len().min(o.limit)];
                let more = usage.len() - shown.len();
                lines.push("TOOL RELIABILITY".into());
                let err = |t: &Value| {
                    let terminal = n(t, "/succeeded") + n(t, "/failed");
                    if terminal == 0.0 { "-".to_string() } else { format_percent(n(t, "/failed") / terminal * 100.0) }
                };
                let p50 = |t: &Value| t.get("durationP50").and_then(Value::as_f64).map(format_duration).unwrap_or_else(|| "-".into());
                let name = |t: &Value| t.get("name").and_then(Value::as_str).unwrap_or("").to_string();
                if o.width < 68 {
                    for t in shown {
                        lines.push(truncate(&name(t), o.width));
                        lines.push(format!("  {} calls · {} error · {} p50", format_number(n(t, "/calls")), err(t), p50(t)));
                    }
                } else {
                    lines.push(table_row("tool", "calls", "error", "p50"));
                    for t in shown {
                        lines.push(table_row(&name(t), &format_number(n(t, "/calls")), &err(t), &p50(t)));
                    }
                }
                lines.push(String::new());
                lines.push(format!(
                    "{} finished calls · {} unfinished",
                    format_number(n(&tools, "/totals/succeeded") + n(&tools, "/totals/failed")),
                    format_number(n(&tools, "/totals/unfinished"))
                ));
                if more > 0 {
                    lines.push(format!("+{} more tool{}", grouped(more as i64), if more == 1 { "" } else { "s" }));
                }
            }
        }
    }
    lines.join(EOL)
}

fn paint_activity(level: i64, color: bool, p: &Palette) -> String {
    let glyph = ["·", "░", "▒", "▓", "█"][level as usize];
    if !color {
        return glyph.into();
    }
    if level == 0 {
        return format!("\x1b[2m{glyph}\x1b[22m");
    }
    format!("\x1b[{}m{glyph}\x1b[39m", p.activity[level as usize - 1])
}

pub struct Calendar {
    pub weeks: Vec<[i64; 7]>,
    pub months: Vec<(String, usize)>,
    pub clipped: bool,
}

fn ordinal(ms: i64) -> i64 {
    let t = Local.timestamp_millis_opt(ms).earliest().unwrap_or_else(Local::now);
    let d = t.date_naive();
    d.signed_duration_since(NaiveDate::from_ymd_opt(1970, 1, 1).unwrap()).num_days()
}

fn date_of(ordinal: i64) -> NaiveDate {
    NaiveDate::from_ymd_opt(1970, 1, 1).unwrap() + chrono::Duration::days(ordinal)
}

/// `activityCalendar({activity, from, to, maxWeeks})`
pub fn activity_calendar(activity: &[(String, i64)], from_ms: i64, to_ms: i64, max_weeks: i64) -> Calendar {
    let from = ordinal(from_ms);
    let to = ordinal(to_ms - 1);
    let weekday = date_of(from).weekday().num_days_from_monday() as i64;
    let monday = from - weekday;
    let total = if to_ms <= from_ms { 0 } else { (to - monday).div_euclid(7) + 1 };
    let count = total.min(53).min(max_weeks.max(1));
    let start = monday + (total - count).max(0) * 7;
    let values: std::collections::HashMap<&str, i64> = activity.iter().map(|(d, s)| (d.as_str(), *s)).collect();
    let mut levels: Vec<i64> = values.values().copied().filter(|s| *s > 0).collect();
    levels.sort();
    levels.dedup();
    let weeks: Vec<[i64; 7]> = (0..count)
        .map(|w| {
            let mut row = [0i64; 7];
            for (day, cell) in row.iter_mut().enumerate() {
                let date = start + w * 7 + day as i64;
                let key = date_of(date).format("%Y-%m-%d").to_string();
                let steps = values.get(key.as_str()).copied().unwrap_or(0);
                *cell = if date < from || date > to {
                    -1
                } else if steps == 0 {
                    0
                } else {
                    let idx = levels.iter().position(|l| *l == steps).map(|i| i as i64).unwrap_or(-1);
                    (((idx + 1) as f64 / levels.len() as f64 * 4.0).ceil() as i64).max(1)
                };
            }
            row
        })
        .collect();
    let mut months: Vec<(usize, String)> = vec![];
    for w in 0..count {
        let label = date_of((start + w * 7 + 3).max(from).min(to)).format("%b").to_string();
        if months.last().is_none_or(|(_, l)| *l != label) {
            months.push((w as usize, label));
        }
    }
    let months = months
        .iter()
        .enumerate()
        .map(|(i, (week, label))| (label.clone(), months.get(i + 1).map(|m| m.0).unwrap_or(count as usize) - week))
        .collect();
    Calendar { weeks, months, clipped: count < total }
}

fn render_activity(stats: &Value, color: bool, width: usize, p: &Palette) -> Vec<String> {
    let activity: Vec<(String, i64)> = stats
        .get("activity")
        .and_then(Value::as_array)
        .map(|a| a.iter().map(|d| (d.get("date").and_then(Value::as_str).unwrap_or("").to_string(), n(d, "/steps") as i64)).collect())
        .unwrap_or_default();
    let cal = activity_calendar(&activity, n(stats, "/range/from") as i64, n(stats, "/range/to") as i64, width as i64 - 4);
    let last = cal.months.len().saturating_sub(1);
    let months: String = cal
        .months
        .iter()
        .enumerate()
        .map(|(i, (label, span))| {
            let l = if i == last || label.len() <= *span { label.as_str() } else { "" };
            format!("{l:<span$}", span = *span)
        })
        .collect::<String>()
        .trim_end()
        .to_string();
    let primary = format!("1;{}", p.primary);
    let mut out = vec![
        style(&if cal.clipped { format!("activity · last {} weeks", cal.weeks.len()) } else { "activity".into() }, &primary, color),
        format!("   {}", style(&months, "2", color)),
    ];
    for (day, label) in ["Mo", "Tu", "We", "Th", "Fr", "Sa", "Su"].iter().enumerate() {
        let cells: String = cal.weeks.iter().map(|w| if w[day] < 0 { " ".to_string() } else { paint_activity(w[day], color, p) }).collect();
        out.push(format!("{} {cells}", style(label, "2", color)));
        if day < 6 {
            out.push(String::new());
        }
    }
    out.push(String::new());
    out.push(format!(
        "   {} {} {}",
        style("less", "2", color),
        (0..5).map(|l| paint_activity(l, color, p)).collect::<String>(),
        style("more", "2", color)
    ));
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn numbers() {
        assert_eq!(format_number(18500.0), "18.5k");
        assert_eq!(format_number(1234.0), "1.2k");
        assert_eq!(format_number(999.4), "999");
        assert_eq!(format_number(2_000_000.0), "2m");
        assert_eq!(format_percent(32.258), "32.3%");
        assert_eq!(format_percent(3.0), "3.00%");
        assert_eq!(grouped(1234567), "1,234,567");
    }

    #[test]
    fn empty_card() {
        let stats = json!({"sessions": 0, "prompts": 0, "steps": 0, "subagents": 0, "tokens": {}, "tools": {"mode": "summary", "totals": {}}});
        let o = Options { label: "2026 so far".into(), scope: "all projects".into(), models: false, tools: false, cost: false, limit: 5, color: false, width: 80 };
        assert_eq!(render(&stats, &o), ["ocd stats · 2026 so far · all projects", "", "no activity in this range", "", "opencode.ai"].join(EOL));
    }
}

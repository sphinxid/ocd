//! Usage statistics route (`/stats`, `src/feature-plugins/system/stats.tsx`).

use super::{App, Route};
use crate::theme::{Theme, tint};
use crate::ui::text::*;
use crate::util::locale::width as sw;
use chrono::{Datelike, Local, NaiveDate, TimeZone};
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use serde_json::Value;

#[derive(Clone, Debug, Default)]
pub struct StatsState {
    pub data: Option<Value>,
    pub error: bool,
    pub return_route: Option<Route>,
}

fn digit(c: char) -> Option<[&'static str; 5]> {
    Some(match c {
        '0' => ["111", "101", "101", "101", "111"],
        '1' => ["010", "110", "010", "010", "111"],
        '2' => ["111", "001", "111", "100", "111"],
        '3' => ["111", "001", "111", "001", "111"],
        '4' => ["101", "101", "111", "001", "001"],
        '5' => ["111", "100", "111", "001", "111"],
        '6' => ["111", "100", "111", "101", "111"],
        '7' => ["111", "001", "010", "010", "010"],
        '8' => ["111", "101", "111", "101", "111"],
        '9' => ["111", "101", "111", "001", "111"],
        '.' => ["0", "0", "0", "0", "1"],
        'K' => ["101", "110", "100", "110", "101"],
        'M' => ["10001", "11011", "10101", "10001", "10001"],
        'B' => ["110", "101", "110", "101", "110"],
        'T' => ["111", "010", "010", "010", "010"],
        _ => return None,
    })
}

/// `Intl.NumberFormat("en-US", {notation: "compact", maximumFractionDigits: 1})`.
pub fn stats_number(v: f64) -> String {
    let (div, suffix) = if v >= 1e12 {
        (1e12, "T")
    } else if v >= 1e9 {
        (1e9, "B")
    } else if v >= 1e6 {
        (1e6, "M")
    } else if v >= 1e3 {
        (1e3, "K")
    } else {
        (1.0, "")
    };
    let x = ((v / div) * 10.0).round() / 10.0;
    let s = if x.fract() == 0.0 { format!("{}", x as i64) } else { format!("{x:.1}") };
    format!("{s}{suffix}")
}

pub struct Calendar {
    pub weeks: Vec<Vec<i32>>,
    pub months: Vec<(String, usize)>,
    pub clipped: bool,
}

fn ordinal(ms: i64) -> i64 {
    let d = Local.timestamp_millis_opt(ms).single().map(|d| d.date_naive()).unwrap_or_default();
    d.num_days_from_ce() as i64
}

fn date_of(ord: i64) -> NaiveDate {
    NaiveDate::from_num_days_from_ce_opt(ord as i32).unwrap_or_default()
}

/// Port of `activityCalendar`.
pub fn activity_calendar(activity: &[(String, i64)], from_ms: i64, to_ms: i64, max_weeks: i64) -> Calendar {
    let from = ordinal(from_ms);
    let to = ordinal(to_ms - 1);
    let monday = from - date_of(from).weekday().num_days_from_monday() as i64;
    let total = if to_ms <= from_ms { 0 } else { (to - monday) / 7 + 1 };
    let count = total.min(53).min(max_weeks.max(1));
    let start = monday + (total - count).max(0) * 7;
    let values: std::collections::HashMap<&str, i64> = activity.iter().map(|(d, s)| (d.as_str(), *s)).collect();
    let mut levels: Vec<i64> = values.values().copied().filter(|s| *s > 0).collect();
    levels.sort();
    levels.dedup();
    let weeks: Vec<Vec<i32>> = (0..count)
        .map(|w| {
            (0..7)
                .map(|d| {
                    let date = start + w * 7 + d;
                    let key = date_of(date).format("%Y-%m-%d").to_string();
                    let steps = values.get(key.as_str()).copied().unwrap_or(0);
                    if date < from || date > to {
                        -1
                    } else if steps == 0 {
                        0
                    } else {
                        let idx = levels.iter().position(|l| *l == steps).unwrap_or(0) + 1;
                        (((idx as f64 / levels.len() as f64) * 4.0).ceil() as i32).max(1)
                    }
                })
                .collect()
        })
        .collect();
    let mut months: Vec<(usize, String)> = Vec::new();
    for w in 0..count {
        let label = date_of((start + w * 7 + 3).clamp(from, to)).format("%b").to_string();
        if months.last().is_none_or(|(_, l)| *l != label) {
            months.push((w as usize, label));
        }
    }
    let spans: Vec<(String, usize)> = months
        .iter()
        .enumerate()
        .map(|(i, (w, l))| (l.clone(), months.get(i + 1).map(|(n, _)| *n).unwrap_or(count as usize) - w))
        .collect();
    Calendar { weeks, months: spans, clipped: count < total }
}

fn date_range(from: i64, to: i64) -> String {
    let a = Local.timestamp_millis_opt(from).single().unwrap_or_default();
    let b = Local.timestamp_millis_opt(from.max(to - 1)).single().unwrap_or_default();
    if a.year() == b.year() && a.month() == b.month() {
        return a.format("%b %Y").to_string();
    }
    if a.year() == b.year() {
        return format!("{}\u{2009}–\u{2009}{}", a.format("%b"), b.format("%b %Y"));
    }
    format!("{}\u{2009}–\u{2009}{}", a.format("%b %Y"), b.format("%b %Y"))
}

impl App {
    pub fn open_stats(&mut self) {
        if matches!(&self.route, Route::Plugin(n) if n == "stats") {
            return;
        }
        self.stats = Some(StatsState { return_route: Some(self.route.clone()), ..Default::default() });
        self.dialogs.clear();
        self.route = Route::Plugin("stats".into());
        let c = self.client.clone();
        let from = Local.with_ymd_and_hms(Local::now().year(), 1, 1, 0, 0, 0).single().map(|d| d.timestamp_millis()).unwrap_or(0);
        let tz = std::env::var("TZ").ok().filter(|t| !t.is_empty()).unwrap_or_else(|| "UTC".into());
        self.spawn(async move { c.session_stats(from, &tz).await }, |app, r| {
            if let Some(s) = app.stats.as_mut() {
                match r {
                    Ok(v) => s.data = Some(v),
                    Err(_) => s.error = true,
                }
            }
            app.dirty = true;
        });
    }

    pub fn close_stats(&mut self) {
        let back = self.stats.take().and_then(|s| s.return_route).unwrap_or(Route::Home);
        self.route = back;
        self.dirty = true;
    }

    pub fn draw_stats(&mut self, buf: &mut Buffer, area: Rect, t: &Theme) {
        fill(buf, area, t.background.base.tui());
        let Some(st) = self.stats.clone() else { return };
        let center = |buf: &mut Buffer, y: u16, segs: Vec<Seg>| {
            let w: usize = segs.iter().map(|s| s.width()).sum();
            let x = area.x + (area.width.saturating_sub(w as u16)) / 2;
            put_segs(buf, x, y, &segs, area.width as usize);
        };
        if st.error {
            center(buf, area.y + area.height / 2, vec![seg("Could not load stats. Reopen /stats to try again.", fg(t.text.feedback.error.base))]);
            return;
        }
        let Some(data) = st.data else {
            center(buf, area.y + area.height / 2, vec![seg("Gathering your stats…", fg(t.text.muted))]);
            return;
        };
        let lines = self.stats_poster(t, &data);
        let h = lines.len() as u16;
        let y0 = area.y + area.height.saturating_sub(h) / 2;
        let w = lines.iter().map(|l| l.width()).max().unwrap_or(0) as u16;
        let x0 = area.x + area.width.saturating_sub(w) / 2;
        draw_lines(buf, Rect::new(x0, y0.max(area.y + 1), w.min(area.width), h.min(area.height)), &lines, 0, "");
    }

    pub fn stats_poster(&self, t: &Theme, data: &Value) -> Vec<SLine> {
        let width = 12.max(110.min((self.size.0 as usize).saturating_sub(8)));
        let compact = self.size.1 < 38;
        let tok = data.get("tokens").cloned().unwrap_or(Value::Null);
        let n = |k: &str| tok.get(k).and_then(|v| v.as_f64()).unwrap_or(0.0);
        let total = n("input") + n("output") + n("reasoning") + tok.pointer("/cache/read").and_then(|v| v.as_f64()).unwrap_or(0.0) + tok.pointer("/cache/write").and_then(|v| v.as_f64()).unwrap_or(0.0);
        let num = |k: &str| data.get(k).and_then(|v| v.as_f64()).unwrap_or(0.0);
        let from = data.pointer("/range/from").and_then(|v| v.as_f64()).unwrap_or(0.0) as i64;
        let to = data.pointer("/range/to").and_then(|v| v.as_f64()).unwrap_or(0.0) as i64;
        let center = |segs: Vec<Seg>| -> SLine {
            let w: usize = segs.iter().map(|s| s.width()).sum();
            let mut v = vec![seg(" ".repeat(width.saturating_sub(w) / 2), fg(t.text.base))];
            v.extend(segs);
            SLine::new(v)
        };
        let gap = if compact { 1 } else { 2 };
        let mut out = Vec::new();
        let title = "ocd / stats";
        let dates = date_range(from, to);
        if width < 44 {
            out.push(SLine::new(vec![seg(title, bold(fg(t.text.base)))]));
            out.push(SLine::new(vec![seg(dates, fg(t.text.muted))]));
        } else {
            out.push(SLine::new(vec![seg(title, bold(fg(t.text.base))), seg(" ".repeat(width.saturating_sub(sw(title) + sw(&dates))), fg(t.text.base)), seg(dates, fg(t.text.muted))]));
        }
        let push_gap = |out: &mut Vec<SLine>| {
            for _ in 0..gap {
                out.push(SLine::blank());
            }
        };
        if !compact {
            push_gap(&mut out);
            for l in crate::ui::view_helpers::logo(t, 100, 40) {
                let mut segs = vec![seg(" ".repeat(width.saturating_sub(39) / 2), fg(t.text.base))];
                segs.extend(l.segs);
                out.push(SLine::new(segs));
            }
        }
        push_gap(&mut out);
        let number = stats_number(total);
        let letters: Option<Vec<[&str; 5]>> = number.chars().map(digit).collect();
        let large = letters.as_ref().is_some_and(|ls| ls.iter().map(|c| c[0].len() * 2 + 2).sum::<usize>().saturating_sub(2) <= width);
        if let (true, Some(ls)) = (large, letters) {
            for row in 0..5 {
                let text = ls.iter().map(|c| c[row].replace('1', "██").replace('0', "  ")).collect::<Vec<_>>().join("  ");
                out.push(center(vec![seg(text, fg(t.text.base))]));
            }
        } else {
            out.push(center(vec![seg(number, bold(fg(t.text.base)))]));
        }
        out.push(SLine::blank());
        out.push(center(vec![seg("TOKENS", fg(t.text.muted))]));
        push_gap(&mut out);
        let activity: Vec<(String, i64)> = data
            .get("activity")
            .and_then(|a| a.as_array())
            .map(|a| a.iter().filter_map(|x| Some((x.get("date")?.as_str()?.to_string(), x.get("steps")?.as_i64()?))).collect())
            .unwrap_or_default();
        let cal = activity_calendar(&activity, from, to, ((width - 4) / 2) as i64);
        let shades = [t.text.muted, tint(t.background.base, t.categorical(0, 200), 0.3), tint(t.background.base, t.categorical(0, 200), 0.5), tint(t.background.base, t.categorical(0, 200), 0.75), tint(t.background.base, t.categorical(0, 200), 1.0)];
        let cal_w = 4 + cal.weeks.len() * 2;
        let pad_cal = width.saturating_sub(cal_w) / 2;
        let header: String = cal.months.iter().map(|(l, span)| format!("{:<w$}", if l.len() <= span * 2 { l.as_str() } else { "" }, w = span * 2)).collect();
        out.push(SLine::new(vec![seg(" ".repeat(pad_cal), fg(t.text.base)), seg(format!("    {header}"), fg(t.text.muted))]));
        for (di, day) in ["M", "T", "W", "T", "F", "S", "S"].iter().enumerate() {
            let mut segs = vec![seg(" ".repeat(pad_cal), fg(t.text.base)), seg(format!("{day}   "), fg(t.text.muted))];
            for w in &cal.weeks {
                let level = w[di];
                let text = if level < 0 { "  " } else if level == 0 { "· " } else { "■ " };
                segs.push(seg(text, fg(shades[level.max(0) as usize])));
            }
            out.push(SLine::new(segs));
        }
        if cal.clipped {
            out.push(center(vec![seg(format!("Your last {} weeks", cal.weeks.len()), fg(t.text.muted))]));
        }
        push_gap(&mut out);
        let metrics = [("best streak", num("streak")), ("active days", num("activeDays")), ("sessions", num("sessions"))];
        let col = width / metrics.len();
        let mut values: Vec<Seg> = Vec::new();
        let mut labels: Vec<Seg> = Vec::new();
        for (label, v) in metrics {
            let value = format!("{}{}", stats_number(v), if label == "best streak" { " days" } else { "" });
            let vw = sw(&value);
            let lw = sw(label);
            values.push(seg(" ".repeat(col.saturating_sub(vw) / 2), fg(t.text.base)));
            values.push(seg(value, bold(fg(t.text.base))));
            values.push(seg(" ".repeat(col.saturating_sub(vw) - col.saturating_sub(vw) / 2), fg(t.text.base)));
            labels.push(seg(" ".repeat(col.saturating_sub(lw) / 2), fg(t.text.base)));
            labels.push(seg(label, fg(t.text.muted)));
            labels.push(seg(" ".repeat(col.saturating_sub(lw) - col.saturating_sub(lw) / 2), fg(t.text.base)));
        }
        out.push(SLine::new(values));
        out.push(SLine::new(labels));
        push_gap(&mut out);
        out.push(SLine::new(vec![seg(" ".repeat(width.saturating_sub(11)), fg(t.text.base)), seg("opencode.ai", fg(t.text.base))]));
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compact_numbers() {
        assert_eq!(stats_number(999.0), "999");
        assert_eq!(stats_number(1234.0), "1.2K");
        assert_eq!(stats_number(12000.0), "12K");
        assert_eq!(stats_number(1_500_000.0), "1.5M");
        assert_eq!(stats_number(2e9), "2B");
    }

    #[test]
    fn calendar_shape() {
        let from = Local.with_ymd_and_hms(2026, 1, 1, 0, 0, 0).unwrap().timestamp_millis();
        let to = Local.with_ymd_and_hms(2026, 1, 15, 0, 0, 0).unwrap().timestamp_millis();
        let cal = activity_calendar(&[("2026-01-02".into(), 5), ("2026-01-05".into(), 10)], from, to, 53);
        assert_eq!(cal.weeks.len(), 3);
        // 2026-01-01 is a Thursday: Monday-Wednesday of week 0 are out of range.
        assert_eq!(cal.weeks[0][0], -1);
        assert_eq!(cal.weeks[0][4], 2);
        assert_eq!(cal.weeks[1][0], 4);
        assert_eq!(cal.months[0].0, "Jan");
        assert!(!cal.clipped);
    }
}

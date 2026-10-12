//! `SessionStats.get` (`core/src/session/stats.ts`) for `GET /api/experimental/session/stats`.

use std::collections::{BTreeMap, HashSet};

use chrono::{Datelike, TimeZone};
use indexmap::IndexMap;
use oc_storage::Database;
use rusqlite::types::Value as Sql;
use serde_json::{Value, json};

const WINDOW: i64 = 31 * 24 * 60 * 60 * 1_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolMode {
    None,
    Summary,
    Detail,
}

pub struct Input {
    pub from: Option<i64>,
    pub to: Option<i64>,
    pub project: Option<String>,
    pub timezone: chrono_tz::Tz,
    pub tools: ToolMode,
}

/// `Intl.DateTimeFormat` time zone validation (case-insensitive IANA names).
pub fn parse_timezone(tz: &str) -> Option<chrono_tz::Tz> {
    tz.parse::<chrono_tz::Tz>().ok().or_else(|| chrono_tz::Tz::from_str_insensitive(tz).ok())
}

/// `SessionStats.InvalidRangeError`.
#[derive(Debug)]
pub struct InvalidRange;

#[derive(Default, Clone, Copy)]
struct Tokens {
    input: f64,
    output: f64,
    reasoning: f64,
    read: f64,
    write: f64,
}

impl Tokens {
    fn add(&mut self, o: &Tokens) {
        self.input += o.input;
        self.output += o.output;
        self.reasoning += o.reasoning;
        self.read += o.read;
        self.write += o.write;
    }
    fn total(&self) -> f64 {
        self.input + self.output + self.reasoning + self.read + self.write
    }
    fn json(&self) -> Value {
        json!({"input": self.input, "output": self.output, "reasoning": self.reasoning, "cache": {"read": self.read, "write": self.write}})
    }
}

#[derive(Default, Clone, Copy)]
struct ToolTotals {
    calls: i64,
    succeeded: i64,
    failed: i64,
    unfinished: i64,
}

impl ToolTotals {
    fn add_status(&mut self, status: Option<&str>) {
        self.calls += 1;
        match status {
            Some("completed") => self.succeeded += 1,
            Some("error") => self.failed += 1,
            _ => self.unfinished += 1,
        }
    }
    fn json(&self) -> Value {
        json!({"calls": self.calls, "succeeded": self.succeeded, "failed": self.failed, "unfinished": self.unfinished})
    }
}

fn windows(from: i64, to: i64) -> Vec<(i64, i64)> {
    let n = ((to - from) as f64 / WINDOW as f64).ceil().max(0.0) as i64;
    (0..n).map(|i| (from + i * WINDOW, to.min(from + (i + 1) * WINDOW))).collect()
}

fn date_key(tz: &chrono_tz::Tz, ms: i64) -> String {
    match tz.timestamp_millis_opt(ms).single() {
        Some(d) => format!("{:04}-{:02}-{:02}", d.year(), d.month(), d.day()),
        None => String::new(),
    }
}

fn day_ordinal(day: &str) -> i64 {
    chrono::NaiveDate::parse_from_str(day, "%Y-%m-%d").map(|d| d.num_days_from_ce() as i64).unwrap_or(0)
}

fn longest_streak(days: &[String]) -> i64 {
    let mut current = 0;
    let mut longest = 0;
    for (i, day) in days.iter().enumerate() {
        current = if i > 0 && day_ordinal(day) - day_ordinal(&days[i - 1]) == 1 { current + 1 } else { 1 };
        longest = longest.max(current);
    }
    longest
}

fn median(values: &mut [f64]) -> Option<f64> {
    if values.is_empty() {
        return None;
    }
    values.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let mid = values.len() / 2;
    Some(if values.len().is_multiple_of(2) { (values[mid - 1] + values[mid]) / 2.0 } else { values[mid] })
}

const FORK_FILTER: &str = "(session.fork_session_id IS NULL OR message.time_created >= session.time_created)";

/// `SessionStats.get`.
pub fn get(db: &Database, input: Input) -> anyhow::Result<Result<Value, InvalidRange>> {
    let to = input.to.unwrap_or_else(crate::util::now_ms);
    if input.from.is_some_and(|f| f >= to) {
        return Ok(Err(InvalidRange));
    }
    let project_sql = if input.project.is_some() { " AND session.project_id = ?" } else { "" };
    let with_project = |mut args: Vec<Sql>| {
        if let Some(p) = &input.project {
            args.push(Sql::Text(p.clone()));
        }
        args
    };
    let from = match input.from {
        Some(f) => f,
        None => db
            .with(|c| {
                c.query_row(
                    &format!(
                        "SELECT min(message.time_created) FROM session_message AS message JOIN session_v2 AS session ON session.id = message.session_id WHERE message.type IN ('user', 'assistant') AND message.time_created < ? AND {FORK_FILTER}{project_sql}"
                    ),
                    rusqlite::params_from_iter(with_project(vec![Sql::Integer(to)]).iter()),
                    |r| r.get::<_, Option<i64>>(0),
                )
            })?
            .unwrap_or(to),
    };
    let ranges = windows(from, to);
    let mut sessions = HashSet::new();
    let mut subagents = HashSet::new();
    let mut activity: BTreeMap<String, i64> = BTreeMap::new();
    let mut models: IndexMap<String, (Value, i64, Tokens, f64)> = IndexMap::new();
    let mut tools: IndexMap<String, (ToolTotals, Vec<f64>)> = IndexMap::new();
    let (mut prompts, mut steps, mut tokens, mut cost) = (0i64, 0i64, Tokens::default(), 0f64);
    let mut tool_totals = ToolTotals::default();

    for (rf, rt) in &ranges {
        let sql = format!(
            "SELECT message.session_id, session.parent_id, message.type, message.time_created,
                json_extract(message.data, '$.model.providerID'), json_extract(message.data, '$.model.id'),
                json_extract(message.data, '$.model.variant'), json_extract(message.data, '$.tokens.input'),
                json_extract(message.data, '$.tokens.output'), json_extract(message.data, '$.tokens.reasoning'),
                json_extract(message.data, '$.tokens.cache.read'), json_extract(message.data, '$.tokens.cache.write'),
                json_extract(message.data, '$.cost')
             FROM session_message AS message JOIN session_v2 AS session ON session.id = message.session_id
             WHERE message.type IN ('user', 'assistant') AND message.time_created >= ? AND message.time_created < ? AND {FORK_FILTER}{project_sql}"
        );
        type Row = (String, Option<String>, String, i64, Option<String>, Option<String>, Option<String>, [Option<f64>; 5], Option<f64>);
        let rows: Vec<Row> = db.with(|c| {
            let mut s = c.prepare(&sql)?;
            
            s
                .query_map(rusqlite::params_from_iter(with_project(vec![Sql::Integer(*rf), Sql::Integer(*rt)]).iter()), |r| {
                    Ok((
                        r.get(0)?,
                        r.get(1)?,
                        r.get(2)?,
                        r.get(3)?,
                        r.get(4)?,
                        r.get(5)?,
                        r.get(6)?,
                        [r.get(7)?, r.get(8)?, r.get(9)?, r.get(10)?, r.get(11)?],
                        r.get(12)?,
                    ))
                })?
                .collect()
        })?;
        for (sid, parent, kind, created, provider, model, variant, t, c) in rows {
            if parent.is_none() {
                sessions.insert(sid);
            } else {
                subagents.insert(sid);
            }
            if kind == "user" {
                if parent.is_none() {
                    prompts += 1;
                }
                continue;
            }
            steps += 1;
            let row_tokens = Tokens {
                input: t[0].unwrap_or(0.0),
                output: t[1].unwrap_or(0.0),
                reasoning: t[2].unwrap_or(0.0),
                read: t[3].unwrap_or(0.0),
                write: t[4].unwrap_or(0.0),
            };
            tokens.add(&row_tokens);
            cost += c.unwrap_or(0.0);
            *activity.entry(date_key(&input.timezone, created)).or_default() += 1;
            let (Some(provider), Some(model)) = (provider.filter(|p| !p.is_empty()), model.filter(|m| !m.is_empty())) else { continue };
            let key = format!("{provider}/{model}#{}", variant.clone().unwrap_or_default());
            let entry = models.entry(key).or_insert_with(|| {
                let mut r = json!({"id": model, "providerID": provider});
                if let Some(v) = variant.filter(|v| !v.is_empty()) {
                    r["variant"] = json!(v);
                }
                (r, 0, Tokens::default(), 0.0)
            });
            entry.1 += 1;
            entry.3 += c.unwrap_or(0.0);
            entry.2.add(&row_tokens);
        }
    }

    if input.tools != ToolMode::None {
        for (rf, rt) in &ranges {
            let base = format!(
                "FROM session_message AS message JOIN session_v2 AS session ON session.id = message.session_id, json_each(message.data, '$.content') AS content
                 WHERE message.type = 'assistant' AND message.time_created >= ? AND message.time_created < ? AND {FORK_FILTER} AND json_extract(content.value, '$.type') = 'tool'{project_sql}"
            );
            let args = with_project(vec![Sql::Integer(*rf), Sql::Integer(*rt)]);
            if input.tools == ToolMode::Summary {
                let sql = format!(
                    "WITH calls AS MATERIALIZED (SELECT json_extract(content.value, '$.state.status') AS status {base})
                     SELECT count(*), count(*) FILTER (WHERE status = 'completed'), count(*) FILTER (WHERE status = 'error'),
                            count(*) FILTER (WHERE status IS NULL OR status NOT IN ('completed', 'error')) FROM calls"
                );
                let row: (i64, i64, i64, i64) = db.with(|c| {
                    c.query_row(&sql, rusqlite::params_from_iter(args.iter()), |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))
                })?;
                tool_totals.calls += row.0;
                tool_totals.succeeded += row.1;
                tool_totals.failed += row.2;
                tool_totals.unfinished += row.3;
                continue;
            }
            let sql = format!(
                "SELECT json_extract(content.value, '$.name'), json_extract(content.value, '$.state.status'),
                    CASE WHEN json_extract(content.value, '$.time.completed') IS NULL THEN NULL
                         ELSE json_extract(content.value, '$.time.completed') - coalesce(json_extract(content.value, '$.time.ran'), json_extract(content.value, '$.time.created')) END
                 {base}"
            );
            let rows: Vec<(Option<String>, Option<String>, Option<f64>)> = db.with(|c| {
                let mut s = c.prepare(&sql)?;
                
                s.query_map(rusqlite::params_from_iter(args.iter()), |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?.collect()
            })?;
            for (name, status, duration) in rows {
                tool_totals.add_status(status.as_deref());
                let Some(name) = name.filter(|n| !n.is_empty()) else { continue };
                let entry = tools.entry(name).or_default();
                entry.0.add_status(status.as_deref());
                if let Some(d) = duration {
                    entry.1.push(d);
                }
            }
        }
    }

    // Compaction usage recorded on the event log.
    let ids: Vec<String> = db.with(|c| {
        let mut s = c.prepare(if input.project.is_some() { "SELECT id FROM session_v2 WHERE project_id = ?" } else { "SELECT id FROM session_v2" })?;
        
        match &input.project {
            Some(p) => s.query_map([p], |r| r.get(0))?.collect(),
            None => s.query_map([], |r| r.get(0))?.collect(),
        }
    })?;
    for batch in ids.chunks(500) {
        let placeholders = vec!["?"; batch.len()].join(",");
        let sql = format!(
            "SELECT data FROM event WHERE aggregate_id IN ({placeholders}) AND type = 'session.usage.recorded.1' AND json_extract(data, '$.source') = 'compaction' AND created >= ? AND created < ?"
        );
        let mut args: Vec<Sql> = batch.iter().map(|i| Sql::Text(i.clone())).collect();
        args.push(Sql::Integer(from));
        args.push(Sql::Integer(to));
        let rows: Vec<String> = db.with(|c| {
            let mut s = c.prepare(&sql)?;
            
            s.query_map(rusqlite::params_from_iter(args.iter()), |r| r.get(0))?.collect()
        })?;
        for data in rows {
            let Ok(v) = serde_json::from_str::<Value>(&data) else { continue };
            let (Some(t), Some(c)) = (v.get("tokens"), v.get("cost").and_then(Value::as_f64)) else { continue };
            let n = |p: &str| t.pointer(p).and_then(Value::as_f64);
            let (Some(i), Some(o), Some(r), Some(cr), Some(cw)) = (n("/input"), n("/output"), n("/reasoning"), n("/cache/read"), n("/cache/write")) else {
                continue;
            };
            tokens.add(&Tokens { input: i, output: o, reasoning: r, read: cr, write: cw });
            cost += c;
        }
    }

    let days: Vec<(String, i64)> = activity.into_iter().collect();
    let day_keys: Vec<String> = days.iter().map(|d| d.0.clone()).collect();
    let tools_json = match input.tools {
        ToolMode::None => json!({"mode": "none"}),
        ToolMode::Summary => json!({"mode": "summary", "totals": tool_totals.json()}),
        ToolMode::Detail => {
            let mut usage: Vec<(String, ToolTotals, Vec<f64>)> = tools.into_iter().map(|(n, (t, d))| (n, t, d)).collect();
            usage.sort_by_key(|u| std::cmp::Reverse(u.1.calls));
            let usage: Vec<Value> = usage
                .into_iter()
                .map(|(name, t, mut d)| {
                    let mut v = json!({"name": name, "calls": t.calls, "succeeded": t.succeeded, "failed": t.failed, "unfinished": t.unfinished});
                    if let Some(m) = median(&mut d) {
                        v["durationP50"] = json!(m);
                    }
                    v
                })
                .collect();
            json!({"mode": "detail", "totals": tool_totals.json(), "usage": usage})
        }
    };
    let mut model_list: Vec<(Value, i64, Tokens, f64)> = models.into_values().collect();
    model_list.sort_by(|a, b| b.2.total().partial_cmp(&a.2.total()).unwrap_or(std::cmp::Ordering::Equal));
    Ok(Ok(json!({
        "range": {"from": from, "to": to},
        "sessions": sessions.len(),
        "subagents": subagents.len(),
        "prompts": prompts,
        "steps": steps,
        "tokens": tokens.json(),
        "cost": cost,
        "tools": tools_json,
        "activeDays": days.len(),
        "streak": longest_streak(&day_keys),
        "activity": days.iter().map(|(date, steps)| json!({"date": date, "steps": steps})).collect::<Vec<_>>(),
        "models": model_list.into_iter().map(|(model, steps, t, c)| json!({"model": model, "steps": steps, "tokens": t.json(), "cost": c})).collect::<Vec<_>>(),
    })))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn streaks_and_windows() {
        let days: Vec<String> = ["2026-01-01", "2026-01-02", "2026-01-04", "2026-01-05", "2026-01-06"].iter().map(|s| s.to_string()).collect();
        assert_eq!(longest_streak(&days), 3);
        assert_eq!(longest_streak(&[]), 0);
        assert_eq!(windows(0, WINDOW * 2 + 1).len(), 3);
        assert_eq!(windows(5, 5).len(), 0);
        assert_eq!(median(&mut [3.0, 1.0, 2.0, 10.0]), Some(2.5));
    }

    #[test]
    fn timezone_keys() {
        let tz = parse_timezone("America/New_York").unwrap();
        // 2026-01-01T03:00:00Z is still Dec 31 in New York.
        assert_eq!(date_key(&tz, 1_767_236_400_000), "2025-12-31");
        assert_eq!(date_key(&chrono_tz::UTC, 1_767_236_400_000), "2026-01-01");
        assert!(parse_timezone("Not/AZone").is_none());
        assert!(parse_timezone("utc").is_some());
    }
}

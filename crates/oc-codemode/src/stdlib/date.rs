//! `Date`: construction, `now`/`parse`/`UTC`, the local and UTC getters and setters, and the string forms. Port of
//! `stdlib/date.ts`.
//!
//! Upstream delegates the calendar arithmetic to the host `Date`; here it is the ECMAScript time-value algebra
//! (MakeDay/MakeTime/TimeClip, YearFromTime, ...) on `f64`, with chrono consulted only for the host's local-time
//! offset. `Date.parse` follows the host engine (Bun ships V8's date parser): ES5 ISO strings first, then the legacy
//! heuristics for RFC 2822, `toString`, `M/D/Y`, and month-name forms.

use std::rc::Rc;

use chrono::{DateTime, Local, LocalResult, Offset, TimeZone};

use crate::interp::coerce::{coerce_to_number, coerce_to_string, obj_to_string, to_primitive, Hint};
use crate::interp::errors::{range_error, JsResult};
use crate::interp::function::{arg, constructor, method, methods, prototype_from, receiver, Method, NResult, NativeFn};
use crate::interp::{Rt, Site};
use crate::object::{Obj, ObjKind};
use crate::stdlib::number::to_number;
use crate::value::Value;

// ---------------------------------------------------------------------------------------------------------------
// Time-value algebra (ECMA-262 §21.4.1).

const MS_PER_SECOND: f64 = 1000.0;
const MS_PER_MINUTE: f64 = 60_000.0;
const MS_PER_HOUR: f64 = 3_600_000.0;
const MS_PER_DAY: f64 = 86_400_000.0;
/// The largest time value magnitude: 100,000,000 days.
const MAX_TIME: f64 = 8.64e15;

fn day(t: f64) -> f64 {
    (t / MS_PER_DAY).floor()
}

fn time_within_day(t: f64) -> f64 {
    t.rem_euclid(MS_PER_DAY)
}

fn is_leap(y: f64) -> bool {
    y % 4.0 == 0.0 && (y % 100.0 != 0.0 || y % 400.0 == 0.0)
}

fn day_from_year(y: f64) -> f64 {
    365.0 * (y - 1970.0) + ((y - 1969.0) / 4.0).floor() - ((y - 1901.0) / 100.0).floor()
        + ((y - 1601.0) / 400.0).floor()
}

fn time_from_year(y: f64) -> f64 {
    MS_PER_DAY * day_from_year(y)
}

fn year_from_time(t: f64) -> f64 {
    let mut y = (t / (MS_PER_DAY * 365.2425)).floor() + 1970.0;
    while time_from_year(y) > t {
        y -= 1.0;
    }
    while time_from_year(y + 1.0) <= t {
        y += 1.0;
    }
    y
}

/// Days before the first of each month in a common year.
const CUMULATIVE_DAYS: [f64; 13] = [0.0, 31.0, 59.0, 90.0, 120.0, 151.0, 181.0, 212.0, 243.0, 273.0, 304.0, 334.0, 365.0];

fn month_start(month: usize, leap: bool) -> f64 {
    CUMULATIVE_DAYS[month] + if leap && month >= 2 { 1.0 } else { 0.0 }
}

/// The calendar fields of a finite time value.
#[derive(Debug, Clone, Copy)]
pub struct Fields {
    pub year: f64,
    /// 0-based.
    pub month: f64,
    pub date: f64,
    pub hours: f64,
    pub minutes: f64,
    pub seconds: f64,
    pub ms: f64,
    /// 0 = Sunday.
    pub weekday: f64,
}

pub fn fields(t: f64) -> Fields {
    let year = year_from_time(t);
    let leap = is_leap(year);
    let in_year = day(t) - day_from_year(year);
    let mut month = 0;
    while month < 11 && in_year >= month_start(month + 1, leap) {
        month += 1;
    }
    Fields {
        year,
        month: month as f64,
        date: in_year - month_start(month, leap) + 1.0,
        hours: (t / MS_PER_HOUR).floor().rem_euclid(24.0),
        minutes: (t / MS_PER_MINUTE).floor().rem_euclid(60.0),
        seconds: (t / MS_PER_SECOND).floor().rem_euclid(60.0),
        ms: t.rem_euclid(MS_PER_SECOND),
        weekday: (day(t) + 4.0).rem_euclid(7.0),
    }
}

fn integer(x: f64) -> f64 {
    x.trunc() + 0.0
}

pub fn make_time(hour: f64, min: f64, sec: f64, ms: f64) -> f64 {
    if !(hour.is_finite() && min.is_finite() && sec.is_finite() && ms.is_finite()) {
        return f64::NAN;
    }
    integer(hour) * MS_PER_HOUR + integer(min) * MS_PER_MINUTE + integer(sec) * MS_PER_SECOND + integer(ms)
}

pub fn make_day(year: f64, month: f64, date: f64) -> f64 {
    if !(year.is_finite() && month.is_finite() && date.is_finite()) {
        return f64::NAN;
    }
    let (y, m, dt) = (integer(year), integer(month), integer(date));
    let ym = y + (m / 12.0).floor();
    if !ym.is_finite() {
        return f64::NAN;
    }
    let mn = m.rem_euclid(12.0) as usize;
    day_from_year(ym) + month_start(mn, is_leap(ym)) + dt - 1.0
}

pub fn make_date(day: f64, time: f64) -> f64 {
    if !(day.is_finite() && time.is_finite()) {
        return f64::NAN;
    }
    let tv = day * MS_PER_DAY + time;
    if tv.is_finite() { tv } else { f64::NAN }
}

pub fn time_clip(t: f64) -> f64 {
    if !t.is_finite() || t.abs() > MAX_TIME {
        return f64::NAN;
    }
    integer(t)
}

// ---------------------------------------------------------------------------------------------------------------
// The host's local time zone (chrono reads `TZ` / the system zone like the host engine does).

/// chrono's calendar ends near year ±262143; times beyond it shift by whole 400-year Gregorian cycles, which keep the
/// calendar (and so the zone's recurring rules) identical.
const CHRONO_LIMIT_MS: f64 = 8.0e15;
const MS_PER_CYCLE: f64 = 146_097.0 * MS_PER_DAY;

fn chrono_seconds(t: f64) -> i64 {
    let mut t = t;
    if t > CHRONO_LIMIT_MS {
        t -= ((t - CHRONO_LIMIT_MS) / MS_PER_CYCLE).ceil() * MS_PER_CYCLE;
    } else if t < -CHRONO_LIMIT_MS {
        t += ((-CHRONO_LIMIT_MS - t) / MS_PER_CYCLE).ceil() * MS_PER_CYCLE;
    }
    (t / MS_PER_SECOND).floor() as i64
}

/// Outside the time-value range the host looks offsets up at an equivalent time: the same month, day, and time in a
/// year between 2008 and 2037 with the same leap-ness and weekday of January 1.
fn equivalent_time(t: f64) -> f64 {
    let days = day(t);
    let within = t - days * MS_PER_DAY;
    let f = fields(t);
    let jan1 = day_from_year(f.year);
    let weekday = (jan1 + 4.0).rem_euclid(7.0);
    let recent = if is_leap(f.year) { 1956.0 } else { 1967.0 } + (weekday * 12.0) % 28.0;
    let year = 2008.0 + (recent + 3.0 * 28.0 - 2008.0) % 28.0;
    let new_days = day_from_year(year) + month_start(f.month as usize, is_leap(year)) + f.date - 1.0;
    new_days * MS_PER_DAY + within
}

/// The instant the host consults for an offset: whole milliseconds, moved to an equivalent time when out of range.
fn offset_lookup_time(t: f64) -> f64 {
    let t = t.trunc();
    if t.abs() <= MAX_TIME { t } else { equivalent_time(t) }
}

/// Local minus UTC, in milliseconds, at the UTC time value `t`.
fn offset_at_utc(t: f64) -> f64 {
    match DateTime::from_timestamp(chrono_seconds(offset_lookup_time(t)), 0) {
        Some(dt) => Local.offset_from_utc_datetime(&dt.naive_utc()).fix().local_minus_utc() as f64 * MS_PER_SECOND,
        None => 0.0,
    }
}

/// Local minus UTC, in milliseconds, for a local time value. Repeated wall times take the earlier instant and skipped
/// ones the offset before the transition, like the host engine.
fn offset_at_local(t: f64) -> f64 {
    let t = offset_lookup_time(t);
    let Some(dt) = DateTime::from_timestamp(chrono_seconds(t), 0) else { return 0.0 };
    match Local.offset_from_local_datetime(&dt.naive_utc()) {
        LocalResult::Single(o) => o.fix().local_minus_utc() as f64 * MS_PER_SECOND,
        LocalResult::Ambiguous(a, b) => a.fix().local_minus_utc().max(b.fix().local_minus_utc()) as f64 * MS_PER_SECOND,
        LocalResult::None => offset_at_utc(t - MS_PER_DAY),
    }
}

/// Whether the host converts between local and UTC time for this value (within a day of the time-value range).
fn convertible(t: f64) -> bool {
    t.is_finite() && t.abs() <= MAX_TIME + MS_PER_DAY
}

/// LocalTime(t).
pub fn local_time(t: f64) -> f64 {
    if convertible(t) { t + offset_at_utc(t) } else { t }
}

/// UTC(t) for a local time value.
pub fn utc(t: f64) -> f64 {
    if !t.is_finite() {
        return f64::NAN;
    }
    if convertible(t) { t - offset_at_local(t) } else { t }
}

fn zone_id() -> String {
    if let Ok(tz) = std::env::var("TZ") {
        let tz = tz.trim_start_matches(':');
        if !tz.is_empty() {
            return tz.rsplit_once("zoneinfo/").map(|(_, z)| z).unwrap_or(tz).to_string();
        }
    }
    if let Ok(text) = std::fs::read_to_string("/etc/timezone") {
        let text = text.trim();
        if !text.is_empty() {
            return text.to_string();
        }
    }
    if let Ok(target) = std::fs::read_link("/etc/localtime") {
        let target = target.to_string_lossy().to_string();
        if let Some((_, z)) = target.rsplit_once("zoneinfo/") {
            return z.to_string();
        }
    }
    "UTC".into()
}

/// ICU's long display names for common zones: (zones, standard name, daylight name).
const ZONE_NAMES: &[(&[&str], &str, &str)] = &[
    (
        &[
            "UTC", "Etc/UTC", "Etc/UCT", "UCT", "Etc/Universal", "Universal", "Etc/Zulu", "Zulu", "GMT", "Etc/GMT",
            "Etc/GMT0", "GMT0", "Etc/GMT+0", "Etc/GMT-0", "GMT+0", "GMT-0", "Etc/Greenwich", "Greenwich",
        ],
        "Coordinated Universal Time",
        "Coordinated Universal Time",
    ),
    (&["America/New_York", "America/Toronto", "America/Detroit", "US/Eastern", "EST5EDT"], "Eastern Standard Time", "Eastern Daylight Time"),
    (&["America/Chicago", "America/Mexico_City", "America/Winnipeg", "US/Central", "CST6CDT"], "Central Standard Time", "Central Daylight Time"),
    (&["America/Denver", "America/Phoenix", "America/Edmonton", "US/Mountain", "MST7MDT"], "Mountain Standard Time", "Mountain Daylight Time"),
    (&["America/Los_Angeles", "America/Vancouver", "US/Pacific", "PST8PDT"], "Pacific Standard Time", "Pacific Daylight Time"),
    (&["America/Anchorage"], "Alaska Standard Time", "Alaska Daylight Time"),
    (&["America/Halifax"], "Atlantic Standard Time", "Atlantic Daylight Time"),
    (&["Pacific/Honolulu", "US/Hawaii"], "Hawaii-Aleutian Standard Time", "Hawaii-Aleutian Daylight Time"),
    (&["America/Sao_Paulo"], "Brasilia Standard Time", "Brasilia Summer Time"),
    (&["America/Argentina/Buenos_Aires", "America/Buenos_Aires"], "Argentina Standard Time", "Argentina Summer Time"),
    (&["Europe/London"], "Greenwich Mean Time", "British Summer Time"),
    (&["Europe/Dublin"], "Greenwich Mean Time", "Irish Standard Time"),
    (&["Europe/Lisbon"], "Western European Standard Time", "Western European Summer Time"),
    (
        &[
            "Europe/Berlin", "Europe/Paris", "Europe/Amsterdam", "Europe/Madrid", "Europe/Rome", "Europe/Zurich",
            "Europe/Stockholm", "Europe/Warsaw", "Europe/Vienna", "Europe/Brussels", "Europe/Prague", "Europe/Oslo",
            "Europe/Copenhagen", "Europe/Budapest", "CET",
        ],
        "Central European Standard Time",
        "Central European Summer Time",
    ),
    (
        &["Europe/Kiev", "Europe/Kyiv", "Europe/Athens", "Europe/Helsinki", "Europe/Bucharest", "Africa/Cairo", "EET"],
        "Eastern European Standard Time",
        "Eastern European Summer Time",
    ),
    (&["Europe/Moscow"], "Moscow Standard Time", "Moscow Summer Time"),
    (&["Africa/Johannesburg"], "South Africa Standard Time", "South Africa Standard Time"),
    (&["Africa/Lagos"], "West Africa Standard Time", "West Africa Summer Time"),
    (&["Asia/Jerusalem", "Asia/Tel_Aviv"], "Israel Standard Time", "Israel Daylight Time"),
    (&["Asia/Riyadh"], "Arabian Standard Time", "Arabian Daylight Time"),
    (&["Asia/Dubai"], "Gulf Standard Time", "Gulf Standard Time"),
    (&["Asia/Tehran"], "Iran Standard Time", "Iran Daylight Time"),
    (&["Asia/Karachi"], "Pakistan Standard Time", "Pakistan Summer Time"),
    (&["Asia/Kolkata", "Asia/Calcutta"], "India Standard Time", "India Standard Time"),
    (&["Asia/Kathmandu", "Asia/Katmandu"], "Nepal Time", "Nepal Time"),
    (&["Asia/Dhaka"], "Bangladesh Standard Time", "Bangladesh Summer Time"),
    (&["Asia/Bangkok", "Asia/Ho_Chi_Minh", "Asia/Saigon"], "Indochina Time", "Indochina Time"),
    (&["Asia/Jakarta", "Asia/Pontianak"], "Western Indonesia Time", "Western Indonesia Time"),
    (&["Asia/Makassar"], "Central Indonesia Time", "Central Indonesia Time"),
    (&["Asia/Jayapura"], "Eastern Indonesia Time", "Eastern Indonesia Time"),
    (&["Asia/Shanghai", "Asia/Chongqing", "PRC"], "China Standard Time", "China Daylight Time"),
    (&["Asia/Hong_Kong"], "Hong Kong Standard Time", "Hong Kong Summer Time"),
    (&["Asia/Taipei"], "Taipei Standard Time", "Taipei Daylight Time"),
    (&["Asia/Singapore"], "Singapore Standard Time", "Singapore Standard Time"),
    (&["Asia/Kuala_Lumpur"], "Malaysia Time", "Malaysia Time"),
    (&["Asia/Manila"], "Philippine Standard Time", "Philippine Summer Time"),
    (&["Asia/Tokyo", "Japan"], "Japan Standard Time", "Japan Daylight Time"),
    (&["Asia/Seoul"], "Korean Standard Time", "Korean Daylight Time"),
    (&["Australia/Sydney", "Australia/Melbourne", "Australia/Canberra"], "Australian Eastern Standard Time", "Australian Eastern Daylight Time"),
    (&["Australia/Brisbane"], "Australian Eastern Standard Time", "Australian Eastern Daylight Time"),
    (&["Pacific/Auckland"], "New Zealand Standard Time", "New Zealand Daylight Time"),
    (&["Australia/Lord_Howe"], "Lord Howe Standard Time", "Lord Howe Daylight Time"),
    (&["Australia/Adelaide"], "Australian Central Standard Time", "Australian Central Daylight Time"),
    (&["Australia/Perth"], "Australian Western Standard Time", "Australian Western Daylight Time"),
];

/// The parenthesized zone name of `toString`/`toTimeString`: ICU's long name, or its `GMT+hh:mm` fallback.
fn zone_name(t: f64, offset_ms: f64) -> String {
    let id = zone_id();
    if let Some((_, standard, daylight)) = ZONE_NAMES.iter().find(|(ids, _, _)| ids.contains(&id.as_str())) {
        let year = year_from_time(t);
        // Daylight time is an offset above the year's standard one, sampled inside the time-value range.
        let sample = |month: f64| offset_at_utc(make_date(make_day(year, month, 1.0), 0.0).clamp(-MAX_TIME, MAX_TIME));
        let (january, july) = (sample(0.0), sample(6.0));
        return if offset_ms > january.min(july) { daylight.to_string() } else { standard.to_string() };
    }
    let minutes = (offset_ms / MS_PER_MINUTE).trunc() as i64;
    if minutes == 0 {
        return "GMT".into();
    }
    let sign = if minutes < 0 { '-' } else { '+' };
    format!("GMT{sign}{:02}:{:02}", minutes.abs() / 60, minutes.abs() % 60)
}

// ---------------------------------------------------------------------------------------------------------------
// String forms.

const WEEKDAYS: [&str; 7] = ["Sun", "Mon", "Tue", "Wed", "Thu", "Fri", "Sat"];
const MONTHS: [&str; 12] = ["Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec"];

/// JavaScriptCore's year in `toString`-style output: at least four digits, signed when negative.
fn display_year(year: f64) -> String {
    let y = year as i64;
    if y < 0 { format!("-{:04}", -y) } else { format!("{y:04}") }
}

/// `Date.prototype.toISOString` of a finite time value.
pub fn iso_string(t: f64) -> String {
    let f = fields(t);
    let y = f.year as i64;
    let year = if (0..=9999).contains(&y) {
        format!("{y:04}")
    } else if y < 0 {
        format!("-{:06}", -y)
    } else {
        format!("+{y:06}")
    };
    format!(
        "{year}-{:02}-{:02}T{:02}:{:02}:{:02}.{:03}Z",
        f.month as i64 + 1,
        f.date as i64,
        f.hours as i64,
        f.minutes as i64,
        f.seconds as i64,
        f.ms as i64
    )
}

/// The deterministic string form of a Date: ISO 8601 or "Invalid Date".
pub fn to_display(t: f64) -> String {
    if t.is_finite() { iso_string(t) } else { "Invalid Date".into() }
}

fn date_string(t: f64) -> String {
    if !t.is_finite() {
        return "Invalid Date".into();
    }
    let f = fields(local_time(t));
    format!(
        "{} {} {:02} {}",
        WEEKDAYS[f.weekday as usize],
        MONTHS[f.month as usize],
        f.date as i64,
        display_year(f.year)
    )
}

fn time_string(t: f64) -> String {
    if !t.is_finite() {
        return "Invalid Date".into();
    }
    let offset = offset_at_utc(t);
    let f = fields(t + offset);
    let minutes = (offset / MS_PER_MINUTE).trunc() as i64;
    let sign = if minutes < 0 { '-' } else { '+' };
    format!(
        "{:02}:{:02}:{:02} GMT{sign}{:02}{:02} ({})",
        f.hours as i64,
        f.minutes as i64,
        f.seconds as i64,
        minutes.abs() / 60,
        minutes.abs() % 60,
        zone_name(t, offset)
    )
}

fn utc_string(t: f64) -> String {
    if !t.is_finite() {
        return "Invalid Date".into();
    }
    let f = fields(t);
    format!(
        "{}, {:02} {} {} {:02}:{:02}:{:02} GMT",
        WEEKDAYS[f.weekday as usize],
        f.date as i64,
        MONTHS[f.month as usize],
        display_year(f.year),
        f.hours as i64,
        f.minutes as i64,
        f.seconds as i64
    )
}

/// ICU en-US in UTC: `M/D/Y` with the proleptic Gregorian year of era (1 BC for year 0).
fn locale_date(f: &Fields) -> String {
    let year = if f.year <= 0.0 { 1.0 - f.year } else { f.year };
    format!("{}/{}/{}", f.month as i64 + 1, f.date as i64, year as i64)
}

fn locale_time(f: &Fields) -> String {
    let h = f.hours as i64;
    let hour12 = if h % 12 == 0 { 12 } else { h % 12 };
    format!("{hour12}:{:02}:{:02} {}", f.minutes as i64, f.seconds as i64, if h < 12 { "AM" } else { "PM" })
}

fn locale_string(name: &str, t: f64) -> String {
    if !t.is_finite() {
        return "Invalid Date".into();
    }
    let f = fields(t);
    match name {
        "toLocaleDateString" => locale_date(&f),
        "toLocaleTimeString" => locale_time(&f),
        _ => format!("{}, {}", locale_date(&f), locale_time(&f)),
    }
}

// ---------------------------------------------------------------------------------------------------------------
// Date.parse: V8's DateParser (as shipped by Bun), on Latin-1 characters.

const NONE: i32 = i32::MAX;
const MAX_SIGNIFICANT_DIGITS: usize = 9;

fn between(x: i32, lo: i32, hi: i32) -> bool {
    (x as i64) >= lo as i64 && (x as i64) <= hi as i64
}

/// Only ASCII white space separates tokens: the parser sees UTF-8 bytes, so even U+00A0 is part of a word.
fn is_white_space(c: u32) -> bool {
    matches!(c, 0x09 | 0x0B | 0x0C | 0x20)
}

fn is_white_space_or_line_terminator(c: u32) -> bool {
    is_white_space(c) || matches!(c, 0x0A | 0x0D)
}

struct Reader<'a> {
    s: &'a [u16],
    index: usize,
    ch: u32,
}

impl<'a> Reader<'a> {
    fn new(s: &'a [u16]) -> Self {
        let mut r = Reader { s, index: 0, ch: 0 };
        r.next();
        r
    }
    fn next(&mut self) {
        self.ch = self.s.get(self.index).map(|&c| c as u32).unwrap_or(0);
        self.index += 1;
    }
    fn is_digit(&self) -> bool {
        (b'0' as u32..=b'9' as u32).contains(&self.ch)
    }
    fn read_unsigned_numeral(&mut self) -> i32 {
        let mut n: i32 = 0;
        let mut i = 0;
        while self.ch == b'0' as u32 {
            self.next();
        }
        while self.is_digit() {
            if i < MAX_SIGNIFICANT_DIGITS {
                n = n * 10 + (self.ch - b'0' as u32) as i32;
            }
            i += 1;
            self.next();
        }
        n
    }
    fn is_alpha_or_above(&self) -> bool {
        self.ch >= b'A' as u32
    }
    /// A word (characters >= 'A'), lower-cased into a three-character prefix; returns its length.
    fn read_word(&mut self, prefix: &mut [u32; 3]) -> usize {
        let mut len = 0;
        while self.is_alpha_or_above() && !is_white_space(self.ch) {
            if len < 3 {
                prefix[len] = self.ch | 0x20;
            }
            len += 1;
            self.next();
        }
        len
    }
    fn skip(&mut self, c: u8) -> bool {
        if self.ch == c as u32 {
            self.next();
            return true;
        }
        false
    }
    fn skip_parentheses(&mut self) -> bool {
        if self.ch != b'(' as u32 {
            return false;
        }
        let mut balance = 0;
        loop {
            if self.ch == b')' as u32 {
                balance -= 1;
            } else if self.ch == b'(' as u32 {
                balance += 1;
            }
            self.next();
            if !(balance > 0 && self.ch != 0) {
                break;
            }
        }
        true
    }
}

#[derive(Clone, Copy, PartialEq, Debug)]
enum Kw {
    Invalid,
    MonthName,
    TimeZoneName,
    TimeSeparator,
    AmPm,
}

#[derive(Clone, Copy, PartialEq, Debug)]
enum Tok {
    Invalid,
    Unknown,
    Number { value: i32, len: usize },
    Symbol(u8),
    WhiteSpace,
    End,
    Keyword { kind: Kw, value: i32, len: usize },
}

impl Tok {
    fn is_number(self) -> bool {
        matches!(self, Tok::Number { .. })
    }
    fn number(self) -> i32 {
        match self {
            Tok::Number { value, .. } => value,
            _ => 0,
        }
    }
    fn is_fixed_length_number(self, n: usize) -> bool {
        matches!(self, Tok::Number { len, .. } if len == n)
    }
    fn is_symbol(self, c: u8) -> bool {
        self == Tok::Symbol(c)
    }
    fn is_sign(self) -> bool {
        matches!(self, Tok::Symbol(b'+') | Tok::Symbol(b'-'))
    }
    fn sign(self) -> i32 {
        if self == Tok::Symbol(b'-') { -1 } else { 1 }
    }
    fn is_keyword_kind(self, kind: Kw) -> bool {
        matches!(self, Tok::Keyword { kind: k, .. } if k == kind)
    }
    fn is_keyword_z(self) -> bool {
        matches!(self, Tok::Keyword { kind: Kw::TimeZoneName, value: 0, len: 1 })
    }
}

const KEYWORDS: &[([u8; 3], Kw, i32)] = &[
    (*b"jan", Kw::MonthName, 1),
    (*b"feb", Kw::MonthName, 2),
    (*b"mar", Kw::MonthName, 3),
    (*b"apr", Kw::MonthName, 4),
    (*b"may", Kw::MonthName, 5),
    (*b"jun", Kw::MonthName, 6),
    (*b"jul", Kw::MonthName, 7),
    (*b"aug", Kw::MonthName, 8),
    (*b"sep", Kw::MonthName, 9),
    (*b"oct", Kw::MonthName, 10),
    (*b"nov", Kw::MonthName, 11),
    (*b"dec", Kw::MonthName, 12),
    (*b"am\0", Kw::AmPm, 0),
    (*b"pm\0", Kw::AmPm, 12),
    (*b"ut\0", Kw::TimeZoneName, 0),
    (*b"utc", Kw::TimeZoneName, 0),
    (*b"z\0\0", Kw::TimeZoneName, 0),
    (*b"gmt", Kw::TimeZoneName, 0),
    (*b"cdt", Kw::TimeZoneName, -5),
    (*b"cst", Kw::TimeZoneName, -6),
    (*b"edt", Kw::TimeZoneName, -4),
    (*b"est", Kw::TimeZoneName, -5),
    (*b"mdt", Kw::TimeZoneName, -6),
    (*b"mst", Kw::TimeZoneName, -7),
    (*b"pdt", Kw::TimeZoneName, -7),
    (*b"pst", Kw::TimeZoneName, -8),
    (*b"t\0\0", Kw::TimeSeparator, 0),
];

fn lookup_keyword(prefix: &[u32; 3], len: usize) -> (Kw, i32) {
    for (word, kind, value) in KEYWORDS {
        let matches = word.iter().zip(prefix.iter()).all(|(&w, &p)| w as u32 == p);
        if matches && (len <= 3 || *kind == Kw::MonthName) {
            return (*kind, *value);
        }
    }
    (Kw::Invalid, 0)
}

struct Scanner<'a> {
    input: Reader<'a>,
    next: Tok,
}

impl<'a> Scanner<'a> {
    fn new(s: &'a [u16]) -> Self {
        let mut scanner = Scanner { input: Reader::new(s), next: Tok::End };
        scanner.next = scanner.scan();
        scanner
    }
    fn next(&mut self) -> Tok {
        let result = self.next;
        self.next = self.scan();
        result
    }
    fn peek(&self) -> Tok {
        self.next
    }
    fn skip_symbol(&mut self, c: u8) -> bool {
        if self.next.is_symbol(c) {
            self.next = self.scan();
            return true;
        }
        false
    }
    fn scan(&mut self) -> Tok {
        let input = &mut self.input;
        let start = input.index;
        if input.ch == 0 {
            return Tok::End;
        }
        if input.is_digit() {
            let value = input.read_unsigned_numeral();
            return Tok::Number { value, len: input.index - start };
        }
        for c in [b':', b'-', b'+', b'.', b')'] {
            if input.skip(c) {
                return Tok::Symbol(c);
            }
        }
        if input.is_alpha_or_above() && !is_white_space(input.ch) {
            let mut prefix = [0u32; 3];
            let len = input.read_word(&mut prefix);
            let (kind, value) = lookup_keyword(&prefix, len);
            return Tok::Keyword { kind, value, len };
        }
        if is_white_space_or_line_terminator(input.ch) {
            input.next();
            return Tok::WhiteSpace;
        }
        if input.skip_parentheses() {
            return Tok::Unknown;
        }
        input.next();
        Tok::Unknown
    }
}

struct TimeZoneComposer {
    sign: i32,
    hour: i32,
    minute: i32,
}

impl TimeZoneComposer {
    fn set(&mut self, offset_in_hours: i32) {
        self.sign = if offset_in_hours < 0 { -1 } else { 1 };
        self.hour = offset_in_hours * self.sign;
        self.minute = 0;
    }
    fn set_sign(&mut self, sign: i32) {
        self.sign = if sign < 0 { -1 } else { 1 };
    }
    fn is_expecting(&self, n: i32) -> bool {
        self.hour != NONE && self.minute == NONE && is_minute(n)
    }
    fn is_utc(&self) -> bool {
        self.hour == 0 && self.minute == 0
    }
    fn is_empty(&self) -> bool {
        self.hour == NONE
    }
    /// The UTC offset in seconds, `Some(None)` without a zone, or `None` when it cannot be represented.
    fn write(&mut self) -> Option<Option<i32>> {
        if self.sign == NONE {
            return Some(None);
        }
        if self.hour == NONE {
            self.hour = 0;
        }
        if self.minute == NONE {
            self.minute = 0;
        }
        let total = (self.hour as u32).wrapping_mul(3600).wrapping_add((self.minute as u32).wrapping_mul(60));
        if total > i32::MAX as u32 {
            return None;
        }
        let total = total as i32;
        Some(Some(if self.sign < 0 { -total } else { total }))
    }
}

fn is_minute(x: i32) -> bool {
    between(x, 0, 59)
}
fn is_hour(x: i32) -> bool {
    between(x, 0, 23)
}
fn is_second(x: i32) -> bool {
    between(x, 0, 59)
}
fn is_millisecond(x: i32) -> bool {
    between(x, 0, 999)
}
fn is_month(x: i32) -> bool {
    between(x, 1, 12)
}
fn is_day(x: i32) -> bool {
    between(x, 1, 31)
}

struct TimeComposer {
    comp: [i32; 4],
    index: usize,
    hour_offset: i32,
}

impl TimeComposer {
    fn is_empty(&self) -> bool {
        self.index == 0
    }
    fn is_expecting(&self, n: i32) -> bool {
        (self.index == 1 && is_minute(n)) || (self.index == 2 && is_second(n)) || (self.index == 3 && is_millisecond(n))
    }
    fn add(&mut self, n: i32) -> bool {
        if self.index < 4 {
            self.comp[self.index] = n;
            self.index += 1;
            true
        } else {
            false
        }
    }
    fn add_final(&mut self, n: i32) -> bool {
        if !self.add(n) {
            return false;
        }
        while self.index < 4 {
            self.comp[self.index] = 0;
            self.index += 1;
        }
        true
    }
    fn write(&mut self) -> Option<[i32; 4]> {
        while self.index < 4 {
            self.comp[self.index] = 0;
            self.index += 1;
        }
        let [mut hour, minute, second, ms] = self.comp;
        if self.hour_offset != NONE {
            if !between(hour, 0, 12) {
                return None;
            }
            hour %= 12;
            hour += self.hour_offset;
        }
        if !is_hour(hour) || !is_minute(minute) || !is_second(second) || !is_millisecond(ms) {
            // A 24th hour is allowed if minutes, seconds, and milliseconds are 0.
            if hour != 24 || minute != 0 || second != 0 || ms != 0 {
                return None;
            }
        }
        Some([hour, minute, second, ms])
    }
}

struct DayComposer {
    comp: [i32; 3],
    index: usize,
    named_month: i32,
    is_iso_date: bool,
}

impl DayComposer {
    fn is_empty(&self) -> bool {
        self.index == 0
    }
    fn add(&mut self, n: i32) -> bool {
        if self.index < 3 {
            self.comp[self.index] = n;
            self.index += 1;
            true
        } else {
            false
        }
    }
    /// (year, 0-based month, day).
    fn write(&mut self) -> Option<(i32, i32, i32)> {
        if self.index < 1 {
            return None;
        }
        while self.index < 3 {
            self.comp[self.index] = 1;
            self.index += 1;
        }
        let mut year = 0;
        let month;
        let day;
        if self.named_month == NONE {
            if self.is_iso_date || (self.index == 3 && !is_day(self.comp[0])) {
                year = self.comp[0];
                month = self.comp[1];
                day = self.comp[2];
            } else {
                month = self.comp[0];
                day = self.comp[1];
                if self.index == 3 {
                    year = self.comp[2];
                }
            }
        } else {
            month = self.named_month;
            if self.index == 1 {
                day = self.comp[0];
            } else if !is_day(self.comp[0]) {
                year = self.comp[0];
                day = self.comp[1];
            } else {
                day = self.comp[0];
                year = self.comp[1];
            }
        }
        if !self.is_iso_date {
            if between(year, 0, 49) {
                year += 2000;
            } else if between(year, 50, 99) {
                year += 1900;
            }
        }
        if !is_month(month) || !is_day(day) {
            return None;
        }
        Some((year, month - 1, day))
    }
}

fn read_milliseconds(token: Tok) -> i32 {
    let Tok::Number { value, len } = token else { return 0 };
    let mut number = value;
    let mut length = len;
    if length < 3 {
        if length == 1 {
            number *= 100;
        } else if length == 2 {
            number *= 10;
        }
    } else if length > 3 {
        if length > MAX_SIGNIFICANT_DIGITS {
            length = MAX_SIGNIFICANT_DIGITS;
        }
        let mut factor = 1;
        while length > 3 {
            factor *= 10;
            length -= 1;
        }
        number /= factor;
    }
    number
}

/// The ES5 date-time string format; returns the token the legacy parser continues with (`End` when complete,
/// `Invalid` when the string can no longer be a date).
fn parse_es5(scanner: &mut Scanner, day: &mut DayComposer, time: &mut TimeComposer, tz: &mut TimeZoneComposer) -> Tok {
    if scanner.peek().is_sign() {
        let sign_token = scanner.next();
        if !scanner.peek().is_fixed_length_number(6) {
            return sign_token;
        }
        let sign = sign_token.sign();
        let year = scanner.next().number();
        if sign < 0 && year == 0 {
            return sign_token;
        }
        day.add(sign * year);
    } else if scanner.peek().is_fixed_length_number(4) {
        let year = scanner.next().number();
        day.add(year);
    } else {
        return scanner.next();
    }
    if scanner.skip_symbol(b'-') {
        if !scanner.peek().is_fixed_length_number(2) || !is_month(scanner.peek().number()) {
            return scanner.next();
        }
        let month = scanner.next().number();
        day.add(month);
        if scanner.skip_symbol(b'-') {
            if !scanner.peek().is_fixed_length_number(2) || !is_day(scanner.peek().number()) {
                return scanner.next();
            }
            let date = scanner.next().number();
            day.add(date);
        }
    }
    if !scanner.peek().is_keyword_kind(Kw::TimeSeparator) {
        if scanner.peek() != Tok::End {
            return scanner.next();
        }
    } else {
        scanner.next();
        if !scanner.peek().is_fixed_length_number(2) || !between(scanner.peek().number(), 0, 24) {
            return Tok::Invalid;
        }
        let hour_is_24 = scanner.peek().number() == 24;
        let hour = scanner.next().number();
        time.add(hour);
        if !scanner.skip_symbol(b':') {
            return Tok::Invalid;
        }
        if !scanner.peek().is_fixed_length_number(2)
            || !is_minute(scanner.peek().number())
            || (hour_is_24 && scanner.peek().number() > 0)
        {
            return Tok::Invalid;
        }
        let minute = scanner.next().number();
        time.add(minute);
        if scanner.skip_symbol(b':') {
            if !scanner.peek().is_fixed_length_number(2)
                || !is_second(scanner.peek().number())
                || (hour_is_24 && scanner.peek().number() > 0)
            {
                return Tok::Invalid;
            }
            let second = scanner.next().number();
            time.add(second);
            if scanner.skip_symbol(b'.') {
                if !scanner.peek().is_number() || (hour_is_24 && scanner.peek().number() > 0) {
                    return Tok::Invalid;
                }
                let ms = read_milliseconds(scanner.next());
                time.add(ms);
            }
        }
        if scanner.peek().is_keyword_z() {
            scanner.next();
            tz.set(0);
        } else if scanner.peek().is_symbol(b'+') || scanner.peek().is_symbol(b'-') {
            let sign = scanner.next().sign();
            tz.set_sign(sign);
            if scanner.peek().is_fixed_length_number(4) {
                let hourmin = scanner.next().number();
                let (hour, min) = (hourmin / 100, hourmin % 100);
                if !is_hour(hour) || !is_minute(min) {
                    return Tok::Invalid;
                }
                tz.hour = hour;
                tz.minute = min;
            } else {
                if !scanner.peek().is_fixed_length_number(2) || !is_hour(scanner.peek().number()) {
                    return Tok::Invalid;
                }
                tz.hour = scanner.next().number();
                if !scanner.skip_symbol(b':') {
                    return Tok::Invalid;
                }
                if !scanner.peek().is_fixed_length_number(2) || !is_minute(scanner.peek().number()) {
                    return Tok::Invalid;
                }
                tz.minute = scanner.next().number();
            }
        }
        if scanner.peek() != Tok::End {
            return Tok::Invalid;
        }
    }
    // Date-only forms without an offset are UTC; date-time forms are local.
    if tz.is_empty() && time.is_empty() {
        tz.set(0);
    }
    day.is_iso_date = true;
    Tok::End
}

/// The parsed fields: year, 0-based month, day, hour, minute, second, millisecond, and the UTC offset in seconds.
type Parsed = ((i32, i32, i32), [i32; 4], Option<i32>);

fn parse_fields(units: &[u16]) -> Option<Parsed> {
    let mut scanner = Scanner::new(units);
    let mut tz = TimeZoneComposer { sign: NONE, hour: NONE, minute: NONE };
    let mut time = TimeComposer { comp: [0; 4], index: 0, hour_offset: NONE };
    let mut day = DayComposer { comp: [0; 3], index: 0, named_month: NONE, is_iso_date: false };

    let next_unhandled = parse_es5(&mut scanner, &mut day, &mut time, &mut tz);
    if next_unhandled == Tok::Invalid {
        return None;
    }
    let mut has_read_number = !day.is_empty();
    let mut token = next_unhandled;
    while token != Tok::End {
        match token {
            Tok::Number { value: n, .. } => {
                has_read_number = true;
                if scanner.skip_symbol(b':') {
                    if scanner.skip_symbol(b':') {
                        // n + "::"
                        if !time.is_empty() {
                            return None;
                        }
                        time.add(n);
                        time.add(0);
                    } else {
                        // n + ":"
                        if !time.add(n) {
                            return None;
                        }
                        if scanner.peek().is_symbol(b'.') {
                            scanner.next();
                        }
                    }
                } else if scanner.skip_symbol(b'.') && time.is_expecting(n) {
                    time.add(n);
                    if !scanner.peek().is_number() {
                        return None;
                    }
                    let ms = read_milliseconds(scanner.next());
                    if ms < 0 {
                        return None;
                    }
                    time.add_final(ms);
                } else if tz.is_expecting(n) {
                    tz.minute = n;
                } else if time.is_expecting(n) {
                    time.add_final(n);
                    // Require end, white space, "Z", "+" or "-" immediately after finalizing time.
                    let peek = scanner.peek();
                    if peek != Tok::End && peek != Tok::WhiteSpace && !peek.is_keyword_z() && !peek.is_sign() {
                        return None;
                    }
                } else {
                    if !day.add(n) {
                        return None;
                    }
                    scanner.skip_symbol(b'-');
                }
            }
            Tok::Keyword { kind, value, .. } => {
                if kind == Kw::AmPm && !time.is_empty() {
                    time.hour_offset = value;
                } else if kind == Kw::MonthName {
                    day.named_month = value;
                    scanner.skip_symbol(b'-');
                } else if kind == Kw::TimeZoneName && has_read_number {
                    tz.set(value);
                } else {
                    // Garbage words are illegal once a number has been read, and the first number must be
                    // separated from garbage words.
                    if has_read_number || scanner.peek().is_number() {
                        return None;
                    }
                }
            }
            t if t.is_sign() && (tz.is_utc() || !time.is_empty()) => {
                // A UTC offset (only after UTC or a time); the number may be empty.
                tz.set_sign(t.sign());
                let mut n = 0;
                let mut length = 0;
                if scanner.peek().is_number() {
                    if let Tok::Number { value, len } = scanner.next() {
                        n = value;
                        length = len;
                    }
                }
                has_read_number = true;
                if scanner.peek().is_symbol(b':') {
                    tz.hour = n;
                    tz.minute = NONE;
                } else if length == 1 || length == 2 {
                    tz.hour = n;
                    tz.minute = 0;
                } else if length == 3 || length == 4 {
                    tz.hour = n / 100;
                    tz.minute = n % 100;
                } else {
                    return None;
                }
            }
            t if (t.is_sign() || t.is_symbol(b')')) && has_read_number => return None,
            _ => {}
        }
        token = scanner.next();
    }
    let date = day.write()?;
    let time = time.write()?;
    let offset = tz.write()?;
    Some((date, time, offset))
}

/// `Date.parse` of a string's code units. The host hands the parser the string's UTF-8 bytes read as Latin-1, so any
/// non-ASCII character (a lone surrogate included) is a run of word characters.
pub fn parse(units: &[u16]) -> f64 {
    let bytes: Vec<u16> = String::from_utf16_lossy(units).bytes().map(u16::from).collect();
    let Some(((year, month, date), [hour, minute, second, ms], offset)) = parse_fields(&bytes) else {
        return f64::NAN;
    };
    let day = make_day(year as f64, month as f64, date as f64);
    let time = make_time(hour as f64, minute as f64, second as f64, ms as f64);
    let mut tv = make_date(day, time);
    match offset {
        None => {
            if !(tv.abs() <= MAX_TIME + 10.0 * MS_PER_DAY) {
                return f64::NAN;
            }
            tv = utc(tv);
        }
        Some(seconds) => {
            tv -= seconds as f64 * MS_PER_SECOND;
            if !(tv.abs() <= MAX_TIME) {
                return f64::NAN;
            }
        }
    }
    time_clip(tv)
}

// ---------------------------------------------------------------------------------------------------------------
// Construction and the built-in.

fn now() -> f64 {
    let since = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH);
    match since {
        Ok(d) => d.as_millis() as f64,
        Err(e) => -(e.duration().as_millis() as f64),
    }
}

/// `new Date(y, m[, d[, h[, min[, s[, ms]]]]])` or `Date.UTC(...)` from already-converted numbers.
fn from_components(parts: &[f64]) -> f64 {
    let get = |i: usize, default: f64| parts.get(i).copied().unwrap_or(default);
    let y = get(0, f64::NAN);
    let year = if y.is_nan() {
        y
    } else {
        let yi = integer(y);
        if (0.0..=99.0).contains(&yi) { 1900.0 + yi } else { y }
    };
    let day = make_day(year, get(1, 0.0), get(2, 1.0));
    let time = make_time(get(3, 0.0), get(4, 0.0), get(5, 0.0), get(6, 0.0));
    make_date(day, time)
}

/// ToPrimitive(number) then ToNumber of each argument, in order, stopping at the first throw.
async fn numbers(rt: &Rt, site: Site, values: Vec<Value>) -> JsResult<Vec<f64>> {
    let mut out = Vec::with_capacity(values.len());
    for v in values {
        let p = to_primitive(rt, &v, Hint::Number, site).await?;
        out.push(to_number(&p)?);
    }
    Ok(out)
}

/// Runs `body` on the numeric values of `args`, synchronously when no argument needs program code.
fn with_numbers(rt: &Rt, site: Site, args: Vec<Value>, body: impl FnOnce(Vec<f64>) -> JsResult<Value> + 'static) -> NResult {
    if !args.iter().any(|v| matches!(v, Value::Object(_))) {
        return NResult::Ready(args.iter().map(to_number).collect::<JsResult<Vec<f64>>>().and_then(body));
    }
    let rt = rt.clone();
    NResult::future(async move { body(numbers(&rt, site, args).await?) })
}

fn new_date(proto: &Obj, time: f64) -> Value {
    Value::Object(Obj::new(Some(proto.clone()), ObjKind::Date(std::cell::Cell::new(time))))
}

fn construct_date(rt: &Rt, site: Site, args: Vec<Value>, proto: Obj) -> NResult {
    match args.len() {
        0 => NResult::Ready(Ok(new_date(&proto, now()))),
        1 => {
            let value = args.into_iter().next().unwrap();
            if let Value::Object(o) = &value
                && let ObjKind::Date(t) = o.kind()
            {
                return NResult::Ready(Ok(new_date(&proto, t.get())));
            }
            let finish = move |p: Value| -> JsResult<Value> {
                let time = match &p {
                    Value::String(s) => parse(s.units()),
                    other => time_clip(number_of(other)?),
                };
                Ok(new_date(&proto, time))
            };
            if !matches!(value, Value::Object(_)) {
                return NResult::Ready(finish(value));
            }
            let rt = rt.clone();
            NResult::future(async move { finish(to_primitive(&rt, &value, Hint::Default, site).await?) })
        }
        _ => {
            let parts: Vec<Value> = args.into_iter().take(7).collect();
            with_numbers(rt, site, parts, move |n| Ok(new_date(&proto, time_clip(utc(from_components(&n))))))
        }
    }
}

/// ToNumber of a primitive (opaque values such as functions pass ToPrimitive unchanged and read as NaN).
fn number_of(value: &Value) -> JsResult<f64> {
    match value {
        Value::Object(_) | Value::Tool(_) => Ok(coerce_to_number(value)),
        other => to_number(other),
    }
}

fn this_date(this: &Value, name: &str) -> JsResult<Obj> {
    receiver(this, &format!("Date.prototype.{name}"), |o| matches!(o.kind(), ObjKind::Date(_)))
}

fn time_of(o: &Obj) -> f64 {
    o.date_time().unwrap_or(f64::NAN)
}

fn set_time_of(o: &Obj, t: f64) {
    if let ObjKind::Date(cell) = o.kind() {
        cell.set(t);
    }
}

fn getter(name: &'static str) -> Method {
    method(name, 0, move |_, _, this, _| {
        NResult::Ready(this_date(&this, name).map(|o| Value::Number(get_component(name, time_of(&o)))))
    })
}

fn get_component(name: &str, t: f64) -> f64 {
    if !t.is_finite() {
        return f64::NAN;
    }
    if name == "getTimezoneOffset" {
        // Whole minutes, like the host (local mean time offsets have seconds).
        return ((t - local_time(t)) / MS_PER_MINUTE).trunc() + 0.0;
    }
    let (utc_name, t) = match name.strip_prefix("getUTC") {
        Some(rest) => (rest, t),
        None => (name.strip_prefix("get").unwrap_or(name), local_time(t)),
    };
    let f = fields(t);
    match utc_name {
        "FullYear" => f.year,
        "Month" => f.month,
        "Date" => f.date,
        "Day" => f.weekday,
        "Hours" => f.hours,
        "Minutes" => f.minutes,
        "Seconds" => f.seconds,
        "Milliseconds" => f.ms,
        _ => f64::NAN,
    }
}

/// The host setter `name` applied to the time value `t` with the converted arguments (only those passed, at most the
/// setter's length); returns the new, clipped time value.
pub fn apply_setter(name: &str, t: f64, args: &[f64]) -> f64 {
    let arg = |i: usize| args.get(i).copied().unwrap_or(f64::NAN);
    let opt = |i: usize, current: f64| args.get(i).copied().unwrap_or(current);
    if name == "setTime" {
        return time_clip(arg(0));
    }
    let local = !name.starts_with("setUTC");
    let component = name.trim_start_matches("setUTC").trim_start_matches("set");
    let base = if component == "FullYear" && t.is_nan() {
        0.0
    } else if local {
        local_time(t)
    } else {
        t
    };
    if base.is_nan() {
        return f64::NAN;
    }
    let f = fields(base);
    let new = match component {
        "Milliseconds" => make_date(day(base), make_time(f.hours, f.minutes, f.seconds, arg(0))),
        "Seconds" => make_date(day(base), make_time(f.hours, f.minutes, arg(0), opt(1, f.ms))),
        "Minutes" => make_date(day(base), make_time(f.hours, arg(0), opt(1, f.seconds), opt(2, f.ms))),
        "Hours" => make_date(day(base), make_time(arg(0), opt(1, f.minutes), opt(2, f.seconds), opt(3, f.ms))),
        "Date" => make_date(make_day(f.year, f.month, arg(0)), time_within_day(base)),
        "Month" => make_date(make_day(f.year, arg(0), opt(1, f.date)), time_within_day(base)),
        "FullYear" => make_date(make_day(arg(0), opt(1, f.month), opt(2, f.date)), time_within_day(base)),
        _ => f64::NAN,
    };
    time_clip(if local { utc(new) } else { new })
}

fn setter(name: &'static str, length: usize) -> Method {
    method(name, length, move |rt, site, this, args| {
        let target = match this_date(&this, name) {
            Ok(o) => o,
            Err(e) => return NResult::err(e),
        };
        // Native setters read the current time before argument coercion, whose callbacks may mutate the Date.
        let hosted = time_of(&target);
        let values: Vec<Value> = args.into_iter().take(length).collect();
        with_numbers(rt, site, values, move |values| {
            // Every setter but setTime and setFullYear leaves an invalid Date untouched and answers NaN.
            if hosted.is_nan() && name != "setTime" && !name.ends_with("FullYear") {
                return Ok(Value::Number(f64::NAN));
            }
            let t = apply_setter(name, hosted, &values);
            set_time_of(&target, t);
            Ok(Value::Number(t))
        })
    })
}

fn formatter(name: &'static str, format: fn(f64) -> String) -> Method {
    method(name, 0, move |_, _, this, _| {
        NResult::Ready(this_date(&this, name).map(|o| Value::from(format(time_of(&o)))))
    })
}

const GETTERS: [&str; 17] = [
    "getFullYear",
    "getMonth",
    "getDate",
    "getDay",
    "getHours",
    "getMinutes",
    "getSeconds",
    "getMilliseconds",
    "getUTCFullYear",
    "getUTCMonth",
    "getUTCDate",
    "getUTCDay",
    "getUTCHours",
    "getUTCMinutes",
    "getUTCSeconds",
    "getUTCMilliseconds",
    "getTimezoneOffset",
];

const SETTERS: [(&str, usize); 15] = [
    ("setTime", 1),
    ("setMilliseconds", 1),
    ("setUTCMilliseconds", 1),
    ("setSeconds", 2),
    ("setUTCSeconds", 2),
    ("setMinutes", 3),
    ("setUTCMinutes", 3),
    ("setHours", 4),
    ("setUTCHours", 4),
    ("setDate", 1),
    ("setUTCDate", 1),
    ("setMonth", 2),
    ("setUTCMonth", 2),
    ("setFullYear", 3),
    ("setUTCFullYear", 3),
];

pub fn date_global(rt: &Rt) -> Value {
    let b = &rt.builtins;
    let proto = b.date.clone();
    let ctor = constructor(
        &b.function,
        &proto,
        "Date",
        7,
        NativeFn {
            // ISO instead of the host's locale string: date strings are deterministic and must not leak the host
            // timezone.
            call: Rc::new(|_, _, _, _| NResult::ok(iso_string(now()))),
            construct: Some(Rc::new(|rt, site, args, new_target| {
                let proto = prototype_from(&new_target, &rt.builtins.date);
                construct_date(rt, site, args, proto)
            })),
            callback: true,
        },
    );
    methods(
        &b.function,
        &ctor,
        vec![
            method("now", 0, |_, _, _, _| NResult::ok(now())),
            method("parse", 1, |_, _, _, args| NResult::ok(parse(coerce_to_string(&arg(&args, 0)).units()))),
            method("UTC", 7, |rt, site, _, args| {
                let parts: Vec<Value> = args.into_iter().take(7).collect();
                with_numbers(rt, site, parts, |n| Ok(Value::Number(time_clip(from_components(&n)))))
            }),
        ],
    );

    let mut table = vec![
        method("getTime", 0, |_, _, this, _| NResult::Ready(this_date(&this, "getTime").map(|o| Value::Number(time_of(&o))))),
        method("valueOf", 0, |_, _, this, _| NResult::Ready(this_date(&this, "valueOf").map(|o| Value::Number(time_of(&o))))),
        method("toISOString", 0, |_, _, this, _| {
            NResult::Ready(this_date(&this, "toISOString").and_then(|o| {
                let t = time_of(&o);
                if !t.is_finite() {
                    return Err(range_error("Invalid time value."));
                }
                Ok(Value::from(iso_string(t)))
            }))
        }),
        method("toJSON", 1, |_, _, this, _| {
            NResult::Ready(this_date(&this, "toJSON").map(|o| {
                let t = time_of(&o);
                if t.is_finite() { Value::from(iso_string(t)) } else { Value::Null }
            }))
        }),
        method("toString", 0, |_, _, this, _| {
            NResult::Ready(this_date(&this, "toString").map(|o| Value::from(obj_to_string(&o))))
        }),
        formatter("toDateString", date_string),
        formatter("toTimeString", time_string),
        formatter("toUTCString", utc_string),
        formatter("toGMTString", utc_string),
        formatter("toLocaleString", |t| locale_string("toLocaleString", t)),
        formatter("toLocaleDateString", |t| locale_string("toLocaleDateString", t)),
        formatter("toLocaleTimeString", |t| locale_string("toLocaleTimeString", t)),
    ];
    table.extend(GETTERS.iter().map(|&name| getter(name)));
    table.extend(SETTERS.iter().map(|&(name, length)| setter(name, length)));
    methods(&b.function, &proto, table);
    Value::Object(ctor)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(s: &str) -> f64 {
        parse(&s.encode_utf16().collect::<Vec<u16>>())
    }

    #[test]
    fn calendar_round_trips() {
        for t in [0.0, -1.0, 951782400000.0, -62198755200000.0, 8.64e15, -8.64e15, 253402300800000.0] {
            let f = fields(t);
            let back = make_date(make_day(f.year, f.month, f.date), make_time(f.hours, f.minutes, f.seconds, f.ms));
            assert_eq!(back, t);
        }
        assert_eq!(iso_string(-62198755200000.0), "-000001-01-01T00:00:00.000Z");
        assert_eq!(iso_string(8.64e15), "+275760-09-13T00:00:00.000Z");
        assert_eq!(utc_string(0.0), "Thu, 01 Jan 1970 00:00:00 GMT");
    }

    #[test]
    fn parses_like_v8() {
        assert_eq!(p("2020-01-01T00:00:00.123Z"), 1577836800123.0);
        assert_eq!(p("Wed, 01 Jan 2020 00:00:00 +0100"), 1577833200000.0);
        assert_eq!(p("Jan 1 2020 10:00 GMT+9960"), 1577512800000.0);
        assert!(p("2020-01-01T23:59:60Z").is_nan());
        assert_eq!(p("2021-02-29"), 1614556800000.0);
    }
}

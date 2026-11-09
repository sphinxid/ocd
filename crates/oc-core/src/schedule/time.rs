//! The Effect 4.0.6 time helpers the schedule service relies on, ported to behave exactly like upstream running on
//! Bun 1.4.4 (whose JavaScriptCore parses `new Date(string)` with V8's date parser and resolves zones with ICU):
//!
//! - time zones: `DateTime.zoneMakeNamed`, `zoneMakeLocal`, `zoneToString`, `zoneFromString`;
//! - `Schema.DurationFromString` + `Duration.toMillis` ([`duration_millis`]);
//! - schedule.ts `parseAt` (`DateTime.make` / `DateTime.makeZoned(…, {adjustForTimeZone: true})`) ([`parse_at`]);
//! - `Cron.parse` / `Cron.next` ([`cron_parse`], [`Cron::next`]);
//! - `DateTime.formatIso` and `DateTime.formatIsoOffset(DateTime.setZone(time, zone))`.
//!
//! Zone data comes from chrono-tz instead of ICU; the two agree for current rules, but historic local-mean-time
//! offsets (before ~1900) can differ.

use chrono::{Offset, TimeZone as _};
use chrono_tz::Tz;

const SECOND_MS: i64 = 1_000;
const MINUTE_MS: i64 = 60_000;
const HOUR_MS: i64 = 3_600_000;
const DAY_MS: i64 = 86_400_000;
/// ECMAScript time value range (±100,000,000 days).
const MAX_TIME: i64 = 8_640_000_000_000_000;
/// `minEpochMillis` / `maxEpochMillis` of Effect's `makeZonedUnsafe`.
const MIN_EPOCH: i64 = -MAX_TIME + 12 * HOUR_MS;
const MAX_EPOCH: i64 = MAX_TIME - 14 * HOUR_MS;

// ---------------------------------------------------------------------------------------------------------------
// Time zones
// ---------------------------------------------------------------------------------------------------------------

/// `DateTime.TimeZone`: an ICU-backed named zone (an IANA id, or an ECMA-402 offset id such as `+05:00`), or a fixed
/// offset in milliseconds.
#[derive(Clone, Debug, PartialEq)]
pub enum Zone {
    Named { id: String, data: NamedData },
    Offset(i64),
}

/// The rules behind a named zone.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum NamedData {
    Tz(Tz),
    /// An offset time zone id (`+05:00`), in milliseconds.
    Fixed(i64),
}

/// Ids ICU accepts that the tz database (and chrono-tz) dropped, with the zone they follow.
const ICU_ONLY: [(&str, &str); 2] = [("US/Pacific-New", "America/Los_Angeles"), ("Canada/East-Saskatchewan", "America/Regina")];

/// ECMA-402 offset time zone ids: `±HH`, `±HHMM` or `±HH:MM` (hours 00-23, minutes 00-59).
fn intl_offset_id(tz: &str) -> Option<i64> {
    let b = tz.as_bytes();
    let sign = match b.first()? {
        b'+' => 1,
        b'-' => -1,
        _ => return None,
    };
    let digits = |s: &[u8]| s.len() == 2 && s.iter().all(u8::is_ascii_digit);
    let rest = &b[1..];
    let (h, m) = match rest.len() {
        2 => (&rest[0..2], &b"00"[..]),
        4 => (&rest[0..2], &rest[2..4]),
        5 if rest[2] == b':' => (&rest[0..2], &rest[3..5]),
        _ => return None,
    };
    if !digits(h) || !digits(m) {
        return None;
    }
    let num = |s: &[u8]| ((s[0] - b'0') * 10 + (s[1] - b'0')) as i64;
    let (h, m) = (num(h), num(m));
    if h > 23 || m > 59 {
        return None;
    }
    Some(sign * (h * 60 + m) * MINUTE_MS)
}

/// `DateTime.zoneMakeNamed`: the zone ICU resolves `tz` to (case-insensitively), named by ICU's case-corrected id.
pub fn zone_make_named(tz: &str) -> Option<Zone> {
    if let Some(offset) = intl_offset_id(tz) {
        return Some(Zone::Named { id: offset_to_string(offset), data: NamedData::Fixed(offset) });
    }
    if let Some((id, target)) = ICU_ONLY.iter().find(|(id, _)| id.eq_ignore_ascii_case(tz)) {
        let tz: Tz = target.parse().ok()?;
        return Some(Zone::Named { id: id.to_string(), data: NamedData::Tz(tz) });
    }
    let tz = Tz::from_str_insensitive(tz).ok()?;
    Some(Zone::Named { id: tz.name().to_string(), data: NamedData::Tz(tz) })
}

/// `DateTime.zoneMakeLocal`: the process's default zone as ICU (under Bun) resolves it: `TZ` (a leading `:` is
/// ignored; empty or unknown means UTC), else `/etc/localtime`, canonicalized the way ICU names its default zone
/// (links resolve to their target, `Etc/UTC` and friends to `UTC`).
pub fn zone_make_local() -> Zone {
    let id = match std::env::var("TZ") {
        Ok(value) => local_id(value.strip_prefix(':').unwrap_or(&value)),
        Err(_) => std::fs::read_link("/etc/localtime")
            .ok()
            .and_then(|path| {
                let path = path.to_string_lossy().into_owned();
                path.find("zoneinfo/").map(|i| path[i + "zoneinfo/".len()..].to_string())
            })
            .and_then(|id| local_id_opt(&id))
            .unwrap_or_else(|| "UTC".to_string()),
    };
    zone_make_named(&id).unwrap_or(Zone::Named { id: "UTC".into(), data: NamedData::Tz(Tz::UTC) })
}

fn local_id(tz: &str) -> String {
    local_id_opt(tz).unwrap_or_else(|| "UTC".to_string())
}

fn local_id_opt(tz: &str) -> Option<String> {
    let Some(Zone::Named { id, data: NamedData::Tz(_) }) = zone_make_named(tz) else { return None };
    if let Some((_, target)) = ICU_ONLY.iter().find(|(name, _)| *name == id) {
        return Some(target.to_string());
    }
    Some(LOCAL_CANONICAL.iter().find(|(name, _)| *name == id).map(|(_, target)| target.to_string()).unwrap_or(id))
}

/// `DateTime.zoneToString`.
pub fn zone_to_string(zone: &Zone) -> String {
    match zone {
        Zone::Named { id, .. } => id.clone(),
        Zone::Offset(offset) => offset_to_string(*offset),
    }
}

/// `DateTime.zoneFromString` (what `Cron.parse` does with a string zone): `[GMT]±…` is a fixed offset
/// (`±HH:MM`/`±HHMM` at the end), anything else a named zone.
pub fn zone_from_string(zone: &str) -> Option<Zone> {
    let rest = zone.strip_prefix("GMT").unwrap_or(zone);
    if rest.starts_with('+') || rest.starts_with('-') {
        return parse_offset(zone).map(Zone::Offset);
    }
    zone_make_named(zone)
}

/// `parseOffset`: `/([+-])(\d{2}):?(\d{2})$/` in milliseconds.
fn parse_offset(s: &str) -> Option<i64> {
    let b = s.as_bytes();
    let d = |i: usize| b.get(i).filter(|c| c.is_ascii_digit()).map(|c| (c - b'0') as i64);
    let n = b.len();
    let sign = |c: u8| match c {
        b'+' => Some(1),
        b'-' => Some(-1),
        _ => None,
    };
    let (sign, h1) = if n >= 6 && b[n - 3] == b':' && sign(b[n - 6]).is_some() {
        (sign(b[n - 6])?, n - 5)
    } else if n >= 5 && sign(b[n - 5]).is_some() {
        (sign(b[n - 5])?, n - 4)
    } else {
        return None;
    };
    let hours = d(h1)? * 10 + d(h1 + 1)?;
    let minutes = d(n - 2)? * 10 + d(n - 1)?;
    Some(sign * (hours * 60 + minutes) * MINUTE_MS)
}

/// `offsetToString`: `±HH:MM`, minutes rounded.
fn offset_to_string(offset: i64) -> String {
    let abs = offset.abs();
    let mut hours = abs / HOUR_MS;
    // Math.round on a non-negative value: halves round up.
    let mut minutes = ((abs % HOUR_MS) * 2 + MINUTE_MS) / (2 * MINUTE_MS);
    if minutes == 60 {
        hours += 1;
        minutes = 0;
    }
    format!("{}{hours:02}:{minutes:02}", if offset < 0 { "-" } else { "+" })
}

fn tz_offset(tz: Tz, utc: i64) -> i64 {
    // chrono covers ±262,143 years; clamp the rest of the ECMAScript range onto its ends.
    let utc = utc.clamp(-8_000_000_000_000_000, 8_000_000_000_000_000);
    match chrono::DateTime::from_timestamp_millis(utc) {
        Some(at) => tz.offset_from_utc_datetime(&at.naive_utc()).fix().local_minus_utc() as i64 * SECOND_MS,
        None => 0,
    }
}

/// The zone's offset at a UTC instant, to the second (what `toDate` reads from ICU's parts).
fn true_offset(zone: &Zone, utc: i64) -> i64 {
    match zone {
        Zone::Offset(offset) | Zone::Named { data: NamedData::Fixed(offset), .. } => *offset,
        Zone::Named { data: NamedData::Tz(tz), .. } => tz_offset(*tz, utc),
    }
}

/// `calculateNamedOffset`: ICU's `longOffset` name at `utc` parsed back; one with seconds (local mean time) does not
/// parse, so the offset in effect at `adjusted` read as an instant is used instead.
fn named_offset(zone: &Zone, utc: i64, adjusted: i64) -> i64 {
    let offset = true_offset(zone, utc);
    if offset % MINUTE_MS != 0 { true_offset(zone, adjusted) } else { offset }
}

/// `toDate` of a zoned value: its wall clock as UTC fields.
fn to_date(zone: &Zone, epoch: i64) -> i64 {
    epoch + true_offset(zone, epoch)
}

/// `makeZonedFromAdjusted(adjusted, zone, "compatible")`: the instant whose wall clock is `adjusted`; a time in a
/// gap moves forward, a repeated time takes its first occurrence.
fn from_adjusted(adjusted: i64, zone: &Zone) -> i64 {
    if let Zone::Offset(offset) = zone {
        return adjusted - offset;
    }
    let before = named_offset(zone, adjusted - DAY_MS, adjusted);
    let after = named_offset(zone, adjusted + DAY_MS, adjusted);
    if before == after {
        return adjusted - before;
    }
    if before < after {
        if named_offset(zone, adjusted - after, adjusted) == after {
            return adjusted - after;
        }
        return adjusted - before;
    }
    if named_offset(zone, adjusted - before, adjusted) == before {
        return adjusted - before;
    }
    adjusted - after
}

// ---------------------------------------------------------------------------------------------------------------
// ECMAScript Date arithmetic
// ---------------------------------------------------------------------------------------------------------------

fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = (m + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

fn civil_from_days(z: i64) -> (i64, i64, i64) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    (if m <= 2 { yoe + era * 400 + 1 } else { yoe + era * 400 }, m, d)
}

/// `MakeDay(year, month, date)` with a 0-based, possibly out-of-range month.
fn make_day(year: i64, month: i64, date: i64) -> i64 {
    let y = year + month.div_euclid(12);
    let m = month.rem_euclid(12);
    days_from_civil(y, m + 1, 1) + date - 1
}

/// `TimeClip`: `None` stands for NaN.
fn time_clip(t: i128) -> Option<i64> {
    (t.abs() <= MAX_TIME as i128).then_some(t as i64)
}

/// A time value's UTC fields.
#[derive(Clone, Copy, Debug)]
struct Parts {
    year: i64,
    /// 0-based.
    month: i64,
    date: i64,
    hours: i64,
    minutes: i64,
    seconds: i64,
    ms: i64,
    weekday: i64,
}

fn parts(t: i64) -> Parts {
    let day = t.div_euclid(DAY_MS);
    let time = t.rem_euclid(DAY_MS);
    let (year, month, date) = civil_from_days(day);
    Parts {
        year,
        month: month - 1,
        date,
        hours: time / HOUR_MS,
        minutes: time / MINUTE_MS % 60,
        seconds: time / SECOND_MS % 60,
        ms: time % SECOND_MS,
        weekday: (day + 4).rem_euclid(7),
    }
}

/// The `Date.prototype.setUTC*` family: fields left `None` keep their current value.
#[derive(Default)]
struct Set {
    year: Option<i64>,
    month: Option<i64>,
    date: Option<i64>,
    hours: Option<i64>,
    minutes: Option<i64>,
    seconds: Option<i64>,
    ms: Option<i64>,
}

fn set(t: i64, s: Set) -> Option<i64> {
    let p = parts(t);
    let day = make_day(s.year.unwrap_or(p.year), s.month.unwrap_or(p.month), s.date.unwrap_or(p.date)) as i128;
    let time = s.hours.unwrap_or(p.hours) as i128 * HOUR_MS as i128
        + s.minutes.unwrap_or(p.minutes) as i128 * MINUTE_MS as i128
        + s.seconds.unwrap_or(p.seconds) as i128 * SECOND_MS as i128
        + s.ms.unwrap_or(p.ms) as i128;
    time_clip(day * DAY_MS as i128 + time)
}

/// `Date.prototype.toISOString`.
fn iso_string(t: i64) -> String {
    let p = parts(t);
    let year =
        if (0..=9999).contains(&p.year) { format!("{:04}", p.year) } else { format!("{}{:06}", if p.year < 0 { "-" } else { "+" }, p.year.abs()) };
    format!("{year}-{:02}-{:02}T{:02}:{:02}:{:02}.{:03}Z", p.month + 1, p.date, p.hours, p.minutes, p.seconds, p.ms)
}

/// `DateTime.formatIso` of a UTC instant.
pub fn format_iso(ms: i64) -> String {
    iso_string(ms)
}

/// `DateTime.formatIsoOffset(DateTime.setZone(DateTime.makeUnsafe(ms), zone))`, e.g. `2026-10-27T09:00:00.000-04:00`.
pub fn format_iso_offset(ms: i64, zone: &Zone) -> String {
    let wall = to_date(zone, ms);
    let iso = iso_string(wall);
    format!("{}{}", &iso[..iso.len() - 1], offset_to_string(wall - ms))
}

// ---------------------------------------------------------------------------------------------------------------
// `new Date(string)` (Bun: V8's date parser over the UTF-8 bytes)
// ---------------------------------------------------------------------------------------------------------------

/// ECMAScript `WhiteSpace` and `LineTerminator` (what `\s` and `String.prototype.trim` match).
fn js_space(c: char) -> bool {
    matches!(
        c,
        '\t' | '\n' | '\u{0B}' | '\u{0C}' | '\r' | ' ' | '\u{A0}' | '\u{1680}' | '\u{2000}'
            ..='\u{200A}' | '\u{2028}' | '\u{2029}' | '\u{202F}' | '\u{205F}' | '\u{3000}' | '\u{FEFF}'
    )
}

/// The bytes Bun hands the date parser: whitespace folded to ASCII spaces (only NBSP for Latin-1 strings), UTF-8.
fn parser_bytes(input: &str) -> Vec<u8> {
    if input.is_ascii() {
        return input.as_bytes().to_vec();
    }
    let wide = input.chars().any(|c| c as u32 > 0xFF);
    input
        .chars()
        .map(|c| {
            let fold = if wide {
                matches!(
                    c,
                    '\t' | '\u{0B}' | '\u{0C}' | ' ' | '\u{A0}' | '\u{FEFF}' | '\u{1680}' | '\u{2000}'
                        ..='\u{200A}' | '\u{202F}' | '\u{205F}' | '\u{3000}'
                )
            } else {
                c == '\u{A0}'
            };
            if fold { ' ' } else { c }
        })
        .collect::<String>()
        .into_bytes()
}

const K_NONE: i32 = i32::MAX;

struct Reader<'a> {
    buf: &'a [u8],
    index: usize,
    ch: u32,
}

impl<'a> Reader<'a> {
    fn new(buf: &'a [u8]) -> Self {
        let mut reader = Reader { buf, index: 0, ch: 0 };
        reader.next();
        reader
    }
    fn next(&mut self) {
        self.ch = self.buf.get(self.index).map(|&b| b as u32).unwrap_or(0);
        self.index += 1;
    }
    fn digit(&self) -> bool {
        (b'0' as u32..=b'9' as u32).contains(&self.ch)
    }
    fn white_space_char(&self) -> bool {
        matches!(self.ch, 0x09 | 0x0B | 0x0C | 0x20 | 0xA0)
    }
    fn alpha_or_above(&self) -> bool {
        self.ch >= b'A' as u32
    }
    fn read_unsigned_numeral(&mut self) -> i32 {
        let mut n = 0i32;
        let mut i = 0;
        while self.ch == b'0' as u32 {
            self.next();
        }
        while self.digit() {
            if i < 9 {
                n = n * 10 + (self.ch - b'0' as u32) as i32;
            }
            i += 1;
            self.next();
        }
        n
    }
    fn read_word(&mut self) -> ([u32; 3], i32) {
        let mut prefix = [0u32; 3];
        let mut len = 0;
        while self.alpha_or_above() && !self.white_space_char() {
            if len < 3 {
                prefix[len as usize] = self.ch | 0x20;
            }
            self.next();
            len += 1;
        }
        (prefix, len)
    }
    fn skip(&mut self, c: u8) -> bool {
        if self.ch == c as u32 {
            self.next();
            true
        } else {
            false
        }
    }
    fn skip_white_space(&mut self) -> bool {
        if self.white_space_char() || self.ch == 0x0A || self.ch == 0x0D {
            self.next();
            true
        } else {
            false
        }
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
    Month,
    Zone,
    Separator,
    AmPm,
}

#[derive(Clone, Copy, PartialEq, Debug)]
enum Tok {
    Invalid,
    Unknown,
    WhiteSpace,
    End,
    Number { value: i32, len: i32 },
    Symbol(u8),
    Keyword { kind: Kw, value: i32, len: i32 },
}

impl Tok {
    fn fixed(self, n: i32) -> bool {
        matches!(self, Tok::Number { len, .. } if len == n)
    }
    fn number(self) -> i32 {
        match self {
            Tok::Number { value, .. } => value,
            _ => 0,
        }
    }
    fn sign(self) -> Option<i32> {
        match self {
            Tok::Symbol(b'+') => Some(1),
            Tok::Symbol(b'-') => Some(-1),
            _ => None,
        }
    }
    fn keyword_z(self) -> bool {
        matches!(self, Tok::Keyword { kind: Kw::Zone, len: 1, value: 0 })
    }
}

const KEYWORDS: [([u8; 3], Kw, i32); 27] = [
    (*b"jan", Kw::Month, 1),
    (*b"feb", Kw::Month, 2),
    (*b"mar", Kw::Month, 3),
    (*b"apr", Kw::Month, 4),
    (*b"may", Kw::Month, 5),
    (*b"jun", Kw::Month, 6),
    (*b"jul", Kw::Month, 7),
    (*b"aug", Kw::Month, 8),
    (*b"sep", Kw::Month, 9),
    (*b"oct", Kw::Month, 10),
    (*b"nov", Kw::Month, 11),
    (*b"dec", Kw::Month, 12),
    (*b"am\0", Kw::AmPm, 0),
    (*b"pm\0", Kw::AmPm, 12),
    (*b"ut\0", Kw::Zone, 0),
    (*b"utc", Kw::Zone, 0),
    (*b"z\0\0", Kw::Zone, 0),
    (*b"gmt", Kw::Zone, 0),
    (*b"cdt", Kw::Zone, -5),
    (*b"cst", Kw::Zone, -6),
    (*b"edt", Kw::Zone, -4),
    (*b"est", Kw::Zone, -5),
    (*b"mdt", Kw::Zone, -6),
    (*b"mst", Kw::Zone, -7),
    (*b"pdt", Kw::Zone, -7),
    (*b"pst", Kw::Zone, -8),
    (*b"t\0\0", Kw::Separator, 0),
];

fn keyword(prefix: [u32; 3], len: i32) -> (Kw, i32) {
    for (word, kind, value) in KEYWORDS {
        if (0..3).all(|j| prefix[j] == word[j] as u32) && (len <= 3 || kind == Kw::Month) {
            return (kind, value);
        }
    }
    (Kw::Invalid, 0)
}

struct Scanner<'a> {
    reader: Reader<'a>,
    next: Tok,
}

impl<'a> Scanner<'a> {
    fn new(buf: &'a [u8]) -> Self {
        let mut reader = Reader::new(buf);
        let next = Self::scan(&mut reader);
        Scanner { reader, next }
    }
    fn scan(r: &mut Reader) -> Tok {
        let pre = r.index;
        if r.ch == 0 {
            return Tok::End;
        }
        if r.digit() {
            let value = r.read_unsigned_numeral();
            return Tok::Number { value, len: (r.index - pre) as i32 };
        }
        for symbol in *b":-+.)" {
            if r.skip(symbol) {
                return Tok::Symbol(symbol);
            }
        }
        if r.alpha_or_above() && !r.white_space_char() {
            let (prefix, len) = r.read_word();
            let (kind, value) = keyword(prefix, len);
            return Tok::Keyword { kind, value, len };
        }
        if r.skip_white_space() {
            return Tok::WhiteSpace;
        }
        if r.skip_parentheses() {
            return Tok::Unknown;
        }
        r.next();
        Tok::Unknown
    }
    fn next(&mut self) -> Tok {
        let result = self.next;
        self.next = Self::scan(&mut self.reader);
        result
    }
    fn peek(&self) -> Tok {
        self.next
    }
    fn skip_symbol(&mut self, symbol: u8) -> bool {
        if self.next == Tok::Symbol(symbol) {
            self.next = Self::scan(&mut self.reader);
            true
        } else {
            false
        }
    }
}

fn between(x: i32, lo: i32, hi: i32) -> bool {
    (lo..=hi).contains(&x)
}

struct DayComposer {
    comp: [i32; 3],
    index: usize,
    named_month: i32,
    iso: bool,
}

impl DayComposer {
    fn add(&mut self, n: i32) -> bool {
        if self.index < 3 {
            self.comp[self.index] = n;
            self.index += 1;
            true
        } else {
            false
        }
    }
    fn write(&mut self) -> Option<(i64, i64, i64)> {
        if self.index < 1 {
            return None;
        }
        while self.index < 3 {
            self.comp[self.index] = 1;
            self.index += 1;
        }
        let mut year = 0;
        let (month, day);
        let c = self.comp;
        if self.named_month == K_NONE {
            if self.iso || (self.index == 3 && !between(c[0], 1, 31)) {
                year = c[0];
                month = c[1];
                day = c[2];
            } else {
                month = c[0];
                day = c[1];
                if self.index == 3 {
                    year = c[2];
                }
            }
        } else {
            month = self.named_month;
            if self.index == 1 {
                day = c[0];
            } else if !between(c[0], 1, 31) {
                year = c[0];
                day = c[1];
            } else {
                day = c[0];
                year = c[1];
            }
        }
        if !self.iso {
            if between(year, 0, 49) {
                year += 2000;
            } else if between(year, 50, 99) {
                year += 1900;
            }
        }
        if !(-(1 << 30)..(1 << 30)).contains(&(year as i64)) || !between(month, 1, 12) || !between(day, 1, 31) {
            return None;
        }
        Some((year as i64, month as i64 - 1, day as i64))
    }
}

struct TimeComposer {
    comp: [i32; 4],
    index: usize,
    hour_offset: i32,
}

impl TimeComposer {
    fn empty(&self) -> bool {
        self.index == 0
    }
    fn expecting(&self, n: i32) -> bool {
        (self.index == 1 && between(n, 0, 59)) || (self.index == 2 && between(n, 0, 59)) || (self.index == 3 && between(n, 0, 999))
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
    fn write(&mut self) -> Option<(i64, i64, i64, i64)> {
        while self.index < 4 {
            self.comp[self.index] = 0;
            self.index += 1;
        }
        let [mut hour, minute, second, ms] = self.comp;
        if self.hour_offset != K_NONE {
            if !between(hour, 0, 12) {
                return None;
            }
            hour %= 12;
            hour += self.hour_offset;
        }
        if (!between(hour, 0, 23) || !between(minute, 0, 59) || !between(second, 0, 59) || !between(ms, 0, 999))
            && (hour != 24 || minute != 0 || second != 0 || ms != 0)
        {
            return None;
        }
        Some((hour as i64, minute as i64, second as i64, ms as i64))
    }
}

struct ZoneComposer {
    sign: i32,
    hour: i32,
    minute: i32,
}

impl ZoneComposer {
    fn set(&mut self, offset_hours: i32) {
        self.sign = if offset_hours < 0 { -1 } else { 1 };
        self.hour = offset_hours * self.sign;
        self.minute = 0;
    }
    fn set_sign(&mut self, sign: i32) {
        self.sign = if sign < 0 { -1 } else { 1 };
    }
    fn expecting(&self, n: i32) -> bool {
        self.hour != K_NONE && self.minute == K_NONE && between(n, 0, 59)
    }
    fn utc(&self) -> bool {
        self.hour == 0 && self.minute == 0
    }
    fn empty(&self) -> bool {
        self.hour == K_NONE
    }
    /// `Some(None)` when there is no zone (local time).
    fn write(&mut self) -> Option<Option<i64>> {
        if self.sign == K_NONE {
            return Some(None);
        }
        if self.hour == K_NONE {
            self.hour = 0;
        }
        if self.minute == K_NONE {
            self.minute = 0;
        }
        let total = self.hour as u32 as u64 * 3600 + self.minute as u32 as u64 * 60;
        let total = total as u32 as u64;
        if total > (1 << 30) - 1 {
            return None;
        }
        Some(Some(if self.sign < 0 { -(total as i64) } else { total as i64 }))
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
        if length > 9 {
            length = 9;
        }
        let mut factor = 1;
        loop {
            factor *= 10;
            length -= 1;
            if length <= 3 {
                break;
            }
        }
        number /= factor;
    }
    number
}

/// `ParseES5DateTime`: the first token it leaves to the legacy parser, `End`, or `Invalid` (no date at all).
fn parse_es5(s: &mut Scanner, day: &mut DayComposer, time: &mut TimeComposer, tz: &mut ZoneComposer) -> Tok {
    if let Some(sign) = s.peek().sign() {
        let sign_token = s.next();
        if !s.peek().fixed(6) {
            return sign_token;
        }
        let year = s.next().number();
        if sign < 0 && year == 0 {
            return sign_token;
        }
        day.add(sign * year);
    } else if s.peek().fixed(4) {
        day.add(s.next().number());
    } else {
        return s.next();
    }
    if s.skip_symbol(b'-') {
        if !s.peek().fixed(2) || !between(s.peek().number(), 1, 12) {
            return s.next();
        }
        day.add(s.next().number());
        if s.skip_symbol(b'-') {
            if !s.peek().fixed(2) || !between(s.peek().number(), 1, 31) {
                return s.next();
            }
            day.add(s.next().number());
        }
    }
    if !matches!(s.peek(), Tok::Keyword { kind: Kw::Separator, .. }) {
        if s.peek() != Tok::End {
            return s.next();
        }
    } else {
        s.next();
        if !s.peek().fixed(2) || !between(s.peek().number(), 0, 24) {
            return Tok::Invalid;
        }
        let hour_is_24 = s.peek().number() == 24;
        time.add(s.next().number());
        if !s.skip_symbol(b':') {
            return Tok::Invalid;
        }
        if !s.peek().fixed(2) || !between(s.peek().number(), 0, 59) || (hour_is_24 && s.peek().number() > 0) {
            return Tok::Invalid;
        }
        time.add(s.next().number());
        if s.skip_symbol(b':') {
            if !s.peek().fixed(2) || !between(s.peek().number(), 0, 59) || (hour_is_24 && s.peek().number() > 0) {
                return Tok::Invalid;
            }
            time.add(s.next().number());
            if s.skip_symbol(b'.') {
                if !matches!(s.peek(), Tok::Number { .. }) || (hour_is_24 && s.peek().number() > 0) {
                    return Tok::Invalid;
                }
                time.add(read_milliseconds(s.next()));
            }
        }
        if s.peek().keyword_z() {
            s.next();
            tz.set(0);
        } else if let Some(sign) = s.peek().sign() {
            s.next();
            tz.set_sign(sign);
            if s.peek().fixed(4) {
                let hourmin = s.next().number();
                let (hour, min) = (hourmin / 100, hourmin % 100);
                if !between(hour, 0, 23) || !between(min, 0, 59) {
                    return Tok::Invalid;
                }
                tz.hour = hour;
                tz.minute = min;
            } else {
                if !s.peek().fixed(2) || !between(s.peek().number(), 0, 23) {
                    return Tok::Invalid;
                }
                tz.hour = s.next().number();
                if !s.skip_symbol(b':') {
                    return Tok::Invalid;
                }
                if !s.peek().fixed(2) || !between(s.peek().number(), 0, 59) {
                    return Tok::Invalid;
                }
                tz.minute = s.next().number();
            }
        }
        if s.peek() != Tok::End {
            return Tok::Invalid;
        }
    }
    if tz.empty() && time.empty() {
        tz.set(0);
    }
    day.iso = true;
    Tok::End
}

/// `DateParser::Parse` + Bun's `ParseDateTimeString` + `TimeClip`: the time value `new Date(input)` holds, or `None`
/// for an invalid date. Strings without a zone are local time.
fn js_date_parse(input: &str) -> Option<i64> {
    let bytes = parser_bytes(input);
    let mut s = Scanner::new(&bytes);
    let mut tz = ZoneComposer { sign: K_NONE, hour: K_NONE, minute: K_NONE };
    let mut time = TimeComposer { comp: [0; 4], index: 0, hour_offset: K_NONE };
    let mut day = DayComposer { comp: [0; 3], index: 0, named_month: K_NONE, iso: false };
    let mut token = parse_es5(&mut s, &mut day, &mut time, &mut tz);
    if token == Tok::Invalid {
        return None;
    }
    let mut has_read_number = day.index != 0;
    while token != Tok::End {
        match token {
            Tok::Number { value: n, .. } => {
                has_read_number = true;
                if s.skip_symbol(b':') {
                    if s.skip_symbol(b':') {
                        if !time.empty() {
                            return None;
                        }
                        time.add(n);
                        time.add(0);
                    } else {
                        if !time.add(n) {
                            return None;
                        }
                        if s.peek() == Tok::Symbol(b'.') {
                            s.next();
                        }
                    }
                } else if s.skip_symbol(b'.') && time.expecting(n) {
                    time.add(n);
                    if !matches!(s.peek(), Tok::Number { .. }) {
                        return None;
                    }
                    let ms = read_milliseconds(s.next());
                    if ms < 0 {
                        return None;
                    }
                    time.add_final(ms);
                } else if tz.expecting(n) {
                    tz.minute = n;
                } else if time.expecting(n) {
                    time.add_final(n);
                    let peek = s.peek();
                    if peek != Tok::End && peek != Tok::WhiteSpace && !peek.keyword_z() && peek.sign().is_none() {
                        return None;
                    }
                } else {
                    if !day.add(n) {
                        return None;
                    }
                    s.skip_symbol(b'-');
                }
            }
            Tok::Keyword { kind, value, .. } => {
                if kind == Kw::AmPm && !time.empty() {
                    time.hour_offset = value;
                } else if kind == Kw::Month {
                    day.named_month = value;
                    s.skip_symbol(b'-');
                } else if kind == Kw::Zone && has_read_number {
                    tz.set(value);
                } else {
                    if has_read_number {
                        return None;
                    }
                    if matches!(s.peek(), Tok::Number { .. }) {
                        return None;
                    }
                }
            }
            Tok::Symbol(_) if token.sign().is_some() && (tz.utc() || !time.empty()) => {
                tz.set_sign(token.sign().unwrap_or(1));
                let (mut n, mut length) = (0, 0);
                if let Tok::Number { value, len } = s.peek() {
                    s.next();
                    n = value;
                    length = len;
                }
                has_read_number = true;
                if s.peek() == Tok::Symbol(b':') {
                    tz.hour = n;
                    tz.minute = K_NONE;
                } else if length == 2 || length == 1 {
                    tz.hour = n;
                    tz.minute = 0;
                } else if length == 4 || length == 3 {
                    tz.hour = n / 100;
                    tz.minute = n % 100;
                } else {
                    return None;
                }
            }
            Tok::Symbol(c) if (token.sign().is_some() || c == b')') && has_read_number => return None,
            _ => {}
        }
        token = s.next();
    }
    // The date is written first, then the time, then the zone; any failure makes the whole parse fail.
    let (year, month, date) = day.write()?;
    let (h, m, sec, ms) = time.write()?;
    let offset = tz.write()?;
    let value = make_day(year, month, date) as i128 * DAY_MS as i128 + (h * HOUR_MS + m * MINUTE_MS + sec * SECOND_MS + ms) as i128;
    let value = match offset {
        None => {
            if value.abs() > (MAX_TIME + 30 * DAY_MS) as i128 {
                return None;
            }
            let wall = value as i64;
            wall as i128 - (wall - from_adjusted(wall, &zone_make_local())) as i128
        }
        Some(seconds) => {
            let utc = value - seconds as i128 * SECOND_MS as i128;
            if utc.abs() > MAX_TIME as i128 {
                return None;
            }
            utc
        }
    };
    time_clip(value)
}

// ---------------------------------------------------------------------------------------------------------------
// DateTime.make / makeZoned and schedule.ts parseAt
// ---------------------------------------------------------------------------------------------------------------

/// `hasZone` of `DateTime.makeUnsafe`.
fn has_zone(input: &str) -> bool {
    let b = input.as_bytes();
    let n = b.len();
    let digits = |r: std::ops::Range<usize>| r.clone().all(|i| b[i].is_ascii_digit());
    let sign = |i: usize| b[i] == b'+' || b[i] == b'-';
    input.contains('Z')
        || input.contains("GMT")
        || (n >= 3 && sign(n - 3) && digits(n - 2..n))
        || (n >= 5 && sign(n - 5) && digits(n - 4..n))
        || (n >= 6 && sign(n - 6) && digits(n - 5..n - 3) && b[n - 3] == b':' && digits(n - 2..n))
        || input.ends_with(']')
        || input.ends_with(['z', 'Z'])
        || (n >= 3 && input.is_char_boundary(n - 3) && input[n - 3..].eq_ignore_ascii_case("gmt"))
}

/// `DateTime.makeUnsafe(string)`: a string without a zone is read as UTC.
fn make_from_string(input: &str) -> Option<i64> {
    if has_zone(input) { js_date_parse(input) } else { js_date_parse(&format!("{input}Z")) }
}

/// `/\d(?:z|[+-]\d{2}:?\d{2})$/i`
fn ends_with_offset(input: &str) -> bool {
    let b = input.as_bytes();
    let n = b.len();
    let digit = |i: usize| b[i].is_ascii_digit();
    let sign = |i: usize| b[i] == b'+' || b[i] == b'-';
    (n >= 2 && (b[n - 1] == b'z' || b[n - 1] == b'Z') && digit(n - 2))
        || (n >= 6 && digit(n - 6) && sign(n - 5) && digit(n - 4) && digit(n - 3) && digit(n - 2) && digit(n - 1))
        || (n >= 7 && digit(n - 7) && sign(n - 6) && digit(n - 5) && digit(n - 4) && b[n - 3] == b':' && digit(n - 2) && digit(n - 1))
}

/// schedule.ts `parseAt`: an instant with an offset (`DateTime.make`), or a wall-clock time in `zone`
/// (`DateTime.makeZoned(input, {timeZone: zone, adjustForTimeZone: true})`), as epoch milliseconds.
pub fn parse_at(input: &str, zone: &Zone) -> Option<i64> {
    if ends_with_offset(input) {
        return js_date_parse(input);
    }
    let adjusted = make_from_string(input)?;
    if !(MIN_EPOCH..=MAX_EPOCH).contains(&adjusted) {
        return None;
    }
    Some(from_adjusted(adjusted, zone))
}

// ---------------------------------------------------------------------------------------------------------------
// Durations
// ---------------------------------------------------------------------------------------------------------------

/// `Schema.decodeUnknownOption(Schema.DurationFromString)` then `Duration.toMillis`: `"90 seconds"`,
/// `"1.5 hours"`, `"-2 days"`, `"Infinity"`, …
pub fn duration_millis(value: &str) -> Option<f64> {
    match value {
        "Infinity" => return Some(f64::INFINITY),
        "-Infinity" => return Some(f64::NEG_INFINITY),
        _ => {}
    }
    // /^(-?\d+(?:\.\d+)?)\s+(nanos?|micros?|millis?|seconds?|minutes?|hours?|days?|weeks?)$/
    let b = value.as_bytes();
    let mut i = usize::from(b.first() == Some(&b'-'));
    let start = i;
    while i < b.len() && b[i].is_ascii_digit() {
        i += 1;
    }
    if i == start {
        return None;
    }
    if b.get(i) == Some(&b'.') && b.get(i + 1).is_some_and(u8::is_ascii_digit) {
        i += 1;
        while i < b.len() && b[i].is_ascii_digit() {
            i += 1;
        }
    }
    let number = &value[..i];
    let rest = &value[i..];
    let unit = rest.trim_start_matches(js_space);
    if unit.len() == rest.len() {
        return None;
    }
    let scale: i128 = match unit {
        "nano" | "nanos" => 1,
        "micro" | "micros" => 1_000,
        "milli" | "millis" => 1_000_000,
        "second" | "seconds" => 1_000_000_000,
        "minute" | "minutes" => 60_000_000_000,
        "hour" | "hours" => 3_600_000_000_000,
        "day" | "days" => 86_400_000_000_000,
        "week" | "weeks" => 604_800_000_000_000,
        _ => return None,
    };
    let nanos_to_millis = |nanos: i128| if nanos == 0 { 0.0 } else { nanos as f64 / 1e6 };
    if scale < 1_000_000 {
        return Some(match parse_nanos(number, scale) {
            Some(nanos) => nanos_to_millis(nanos),
            None => number.parse::<f64>().ok()? * scale as f64 / 1e6,
        });
    }
    let millis_scale = (scale / 1_000_000) as f64;
    let millis = number.parse::<f64>().ok()? * millis_scale;
    if !millis.is_finite() {
        return Some(millis);
    }
    let Some(nanos) = parse_nanos(number, scale) else { return Some(if millis == 0.0 { 0.0 } else { millis }) };
    if millis.fract() == 0.0 && millis.abs() < 1e38 && (millis as i128).checked_mul(1_000_000) == Some(nanos) {
        return Some(if millis == 0.0 { 0.0 } else { millis });
    }
    Some(nanos_to_millis(nanos))
}

/// `parseNanos(input, scale)`: the exact nanoseconds, rounding a fraction half away from zero; `None` past i128.
fn parse_nanos(input: &str, scale: i128) -> Option<i128> {
    let negative = input.starts_with('-');
    let unsigned = input.trim_start_matches('-');
    let Some((whole, fraction)) = unsigned.split_once('.') else {
        let value: i128 = unsigned.parse().ok()?;
        return value.checked_mul(scale).map(|v| if negative { -v } else { v });
    };
    let fraction_scale = 10i128.checked_pow(fraction.len() as u32)?;
    let scaled = whole.parse::<i128>().ok()?.checked_mul(fraction_scale)?.checked_add(fraction.parse::<i128>().ok()?)?.checked_mul(scale)?;
    let rounded = scaled / fraction_scale + i128::from((scaled % fraction_scale) * 2 >= fraction_scale);
    Some(if negative { -rounded } else { rounded })
}

// ---------------------------------------------------------------------------------------------------------------
// Cron
// ---------------------------------------------------------------------------------------------------------------

/// `Cron.Cron`: the allowed values of each field (empty means every value), sorted.
#[derive(Clone, Debug, PartialEq)]
pub struct Cron {
    pub tz: Option<Zone>,
    pub seconds: Vec<u32>,
    pub minutes: Vec<u32>,
    pub hours: Vec<u32>,
    pub days: Vec<u32>,
    pub months: Vec<u32>,
    pub weekdays: Vec<u32>,
    pub and: bool,
    first: Bounds,
    next: Tables,
}

#[derive(Clone, Debug, PartialEq)]
struct Bounds {
    second: i64,
    minute: i64,
    hour: i64,
    day: i64,
    /// 0-based.
    month: i64,
    weekday: i64,
}

#[derive(Clone, Debug, PartialEq)]
struct Tables {
    second: Vec<Option<i64>>,
    minute: Vec<Option<i64>>,
    hour: Vec<Option<i64>>,
    day: Vec<Option<i64>>,
    month: Vec<Option<i64>>,
    weekday: Vec<Option<i64>>,
}

/// `makeLookupTable(size, "next")`: the smallest value at or above each index.
fn lookup_next(size: usize, values: &[u32]) -> Vec<Option<i64>> {
    (0..size).map(|i| values.iter().find(|&&v| v as usize >= i).map(|&v| v as i64)).collect()
}

struct Segment {
    min: u32,
    max: u32,
    aliases: &'static [(&'static str, u32)],
    weekday: bool,
}

const MONTH_ALIASES: [(&str, u32); 12] = [
    ("jan", 1),
    ("feb", 2),
    ("mar", 3),
    ("apr", 4),
    ("may", 5),
    ("jun", 6),
    ("jul", 7),
    ("aug", 8),
    ("sep", 9),
    ("oct", 10),
    ("nov", 11),
    ("dec", 12),
];
const WEEKDAY_ALIASES: [(&str, u32); 7] = [("sun", 0), ("mon", 1), ("tue", 2), ("wed", 3), ("thu", 4), ("fri", 5), ("sat", 6)];

/// A parsed number: `Number(field)` for `^\d+$` (or an alias), NaN otherwise.
fn decimal(field: &str) -> f64 {
    if !field.is_empty() && field.bytes().all(|b| b.is_ascii_digit()) { field.parse().unwrap_or(f64::NAN) } else { f64::NAN }
}

fn alias_or_value(field: &str, aliases: &[(&str, u32)]) -> f64 {
    let lower = field.to_lowercase();
    match aliases.iter().find(|(name, _)| *name == lower) {
        Some((_, value)) => *value as f64,
        None => decimal(field),
    }
}

fn split_step(input: &str) -> (&str, Option<f64>) {
    match input.find('/') {
        Some(i) => (&input[..i], Some(decimal(&input[i + 1..]))),
        None => (input, None),
    }
}

fn integer(n: f64) -> bool {
    n.is_finite() && n.fract() == 0.0
}

/// `parseSegment`: (values, wildcard) or the parse error message.
fn parse_segment(input: &str, options: &Segment) -> Result<(Vec<u32>, bool), String> {
    let mut values: Vec<u32> = Vec::new();
    let fields: Vec<&str> = input.split(',').collect();
    let first = split_step(fields[0]);
    let wildcard = first.0 == "*";
    let collect = !(wildcard && (first.1.is_none() || first.1 == Some(1.0)));
    let (min, max) = (options.min as f64, options.max as f64);
    let mut add = |value: f64| {
        if collect {
            let value = value as u32;
            let value = if options.weekday && value == 7 { 0 } else { value };
            if !values.contains(&value) {
                values.push(value);
            }
        }
    };
    for (index, field) in fields.iter().enumerate() {
        let (raw, step) = if index == 0 { first } else { split_step(field) };
        if let Some(step) = step {
            if !integer(step) {
                return Err("Expected step value to be a positive integer".into());
            }
            if step < 1.0 {
                return Err("Expected step value to be greater than 0".into());
            }
            if step > max {
                return Err(format!("Expected step value to be less than or equal to {}", options.max));
            }
        }
        let by = step.unwrap_or(1.0);
        if raw == "*" {
            if index == 0 && (step.is_none() || step == Some(1.0)) {
                continue;
            }
            let mut i = min;
            while i <= max {
                add(i);
                i += by;
            }
            continue;
        }
        let (left, right) = match raw.find('-') {
            Some(i) => (alias_or_value(&raw[..i], options.aliases), Some(alias_or_value(&raw[i + 1..], options.aliases))),
            None => (alias_or_value(raw, options.aliases), None),
        };
        if !integer(left) {
            return Err("Expected a positive integer".into());
        }
        if left < min || left > max {
            return Err(format!("Expected a value between {} and {}", options.min, options.max));
        }
        match right {
            None => {
                let end = if step.is_none() { left } else { max };
                let mut i = left;
                while i <= end {
                    add(i);
                    i += by;
                }
            }
            Some(right) => {
                if !integer(right) {
                    return Err("Expected a positive integer".into());
                }
                if right < min || right > max {
                    return Err(format!("Expected a value between {} and {}", options.min, options.max));
                }
                if left > right {
                    return Err("Invalid value range".into());
                }
                let mut i = left;
                while i <= right {
                    add(i);
                    i += by;
                }
            }
        }
    }
    values.sort_unstable();
    Ok((values, wildcard))
}

/// `Cron.parse(expression, zone)`: `Err` carries the `CronParseError` message.
pub fn cron_parse(expression: &str, zone: Option<&Zone>) -> Result<Cron, String> {
    parse_cron(expression, Ok(zone.cloned()))
}

/// `Cron.parse(expression, zoneString)` (as `Cron.parseUnsafe(row.when.cron, row.tz)` calls it): the zone goes
/// through [`zone_from_string`].
pub fn cron_parse_tz(expression: &str, zone: &str) -> Result<Cron, String> {
    parse_cron(expression, zone_from_string(zone).map(Some).ok_or(()))
}

fn parse_cron(expression: &str, zone: Result<Option<Zone>, ()>) -> Result<Cron, String> {
    let mut segments: Vec<&str> = expression.trim_matches(js_space).split(js_space).filter(|s| !s.is_empty()).collect();
    if segments.len() != 5 && segments.len() != 6 {
        return Err("Invalid number of segments in cron expression".into());
    }
    if segments.len() == 5 {
        segments.insert(0, "0");
    }
    let tz = zone.map_err(|_| "Invalid time zone in cron expression".to_string())?;
    let plain = |min, max| Segment { min, max, aliases: &[], weekday: false };
    let seconds = parse_segment(segments[0], &plain(0, 59))?;
    let minutes = parse_segment(segments[1], &plain(0, 59))?;
    let hours = parse_segment(segments[2], &plain(0, 23))?;
    let days = parse_segment(segments[3], &plain(1, 31))?;
    let months = parse_segment(segments[4], &Segment { min: 1, max: 12, aliases: &MONTH_ALIASES, weekday: false })?;
    let weekdays = parse_segment(segments[5], &Segment { min: 0, max: 7, aliases: &WEEKDAY_ALIASES, weekday: true })?;
    let and = (days.1 || weekdays.1) && !days.0.is_empty() && !weekdays.0.is_empty();
    let first_or = |v: &[u32], d: i64| v.first().map(|&x| x as i64).unwrap_or(d);
    let first = Bounds {
        second: first_or(&seconds.0, 0),
        minute: first_or(&minutes.0, 0),
        hour: first_or(&hours.0, 0),
        day: first_or(&days.0, 1),
        month: first_or(&months.0, 1) - 1,
        weekday: first_or(&weekdays.0, 0),
    };
    let next = Tables {
        second: lookup_next(60, &seconds.0),
        minute: lookup_next(60, &minutes.0),
        hour: lookup_next(24, &hours.0),
        day: lookup_next(32, &days.0),
        month: lookup_next(13, &months.0),
        weekday: lookup_next(7, &weekdays.0),
    };
    Ok(Cron { tz, seconds: seconds.0, minutes: minutes.0, hours: hours.0, days: days.0, months: months.0, weekdays: weekdays.0, and, first, next })
}

fn days_in_month(t: i64) -> i64 {
    let p = parts(t);
    let last = make_day(p.year, p.month + 1, 0);
    civil_from_days(last).2
}

impl Cron {
    /// `cron.seconds.size`.
    pub fn seconds_len(&self) -> usize {
        self.seconds.len()
    }

    /// `Cron.next(cron, new Date(from))`: the first matching instant strictly after `from` (whole seconds), or
    /// `None` where upstream throws (no match within its search budget, or out of range).
    pub fn next(&self, from: i64) -> Option<i64> {
        let zone = match &self.tz {
            Some(zone) => zone.clone(),
            None => Zone::Offset(true_offset(&zone_make_local(), from)),
        };
        if !(MIN_EPOCH..=MAX_EPOCH).contains(&from) {
            return None;
        }
        let start = from;
        let utc = matches!(&zone, Zone::Named { id, .. } if id == "UTC");
        let in_range = |t: i64| (MIN_EPOCH..=MAX_EPOCH).contains(&t).then_some(t);
        let adjust_dst = |current: i64| -> Option<i64> {
            if utc {
                return Some(current);
            }
            let current = in_range(current)?;
            let adjusted = to_date(&zone, from_adjusted(current, &zone));
            let drift = current - adjusted;
            if drift > 0 { time_clip(current as i128 + drift as i128) } else { Some(current) }
        };
        let before_start = |current: i64| -> Option<bool> {
            if utc {
                return Some(false);
            }
            Some(from_adjusted(in_range(current)?, &zone) <= start)
        };
        let first = &self.first;
        let table = &self.next;
        let mut cur = to_date(&zone, start);
        let p = parts(cur);
        cur = set(cur, Set { seconds: Some(p.seconds + 1), ms: Some(0), ..Default::default() })?;
        let mut i = 0;
        loop {
            if i >= 10_000 {
                return None;
            }
            i += 1;
            let p = parts(cur);
            if !self.seconds.is_empty() {
                match table.second[p.seconds as usize] {
                    None => {
                        cur = adjust_dst(set(cur, Set { minutes: Some(p.minutes + 1), seconds: Some(first.second), ..Default::default() })?)?;
                        continue;
                    }
                    Some(next) if next > p.seconds => {
                        cur = adjust_dst(set(cur, Set { seconds: Some(next), ..Default::default() })?)?;
                        continue;
                    }
                    _ => {}
                }
            }
            if !self.minutes.is_empty() {
                match table.minute[p.minutes as usize] {
                    None => {
                        cur = adjust_dst(set(
                            cur,
                            Set { hours: Some(p.hours + 1), minutes: Some(first.minute), seconds: Some(first.second), ..Default::default() },
                        )?)?;
                        continue;
                    }
                    Some(next) if next > p.minutes => {
                        cur = adjust_dst(set(cur, Set { minutes: Some(next), seconds: Some(first.second), ..Default::default() })?)?;
                        continue;
                    }
                    _ => {}
                }
            }
            if !self.hours.is_empty() {
                match table.hour[p.hours as usize] {
                    None => {
                        let moved = set(cur, Set { date: Some(p.date + 1), ..Default::default() })?;
                        cur = adjust_dst(set(
                            moved,
                            Set { hours: Some(first.hour), minutes: Some(first.minute), seconds: Some(first.second), ..Default::default() },
                        )?)?;
                        continue;
                    }
                    Some(next) if next > p.hours => {
                        cur = adjust_dst(set(
                            cur,
                            Set { hours: Some(next), minutes: Some(first.minute), seconds: Some(first.second), ..Default::default() },
                        )?)?;
                        continue;
                    }
                    _ => {}
                }
            }
            let start_of_day = |t: i64, add: i64| -> Option<i64> {
                let moved = set(t, Set { date: Some(parts(t).date + add), ..Default::default() })?;
                set(moved, Set { hours: Some(first.hour), minutes: Some(first.minute), seconds: Some(first.second), ..Default::default() })
            };
            if !self.weekdays.is_empty() || !self.days.is_empty() {
                if self.and {
                    let day_ok = self.days.is_empty() || self.days.contains(&(p.date as u32));
                    let weekday_ok = self.weekdays.is_empty() || self.weekdays.contains(&(p.weekday as u32));
                    if !day_ok || !weekday_ok {
                        cur = adjust_dst(start_of_day(cur, 1)?)?;
                        continue;
                    }
                } else {
                    let mut a = i64::MAX;
                    let mut b = i64::MAX;
                    if !self.weekdays.is_empty() {
                        a = match table.weekday[p.weekday as usize] {
                            None => 7 - p.weekday + first.weekday,
                            Some(next) => next - p.weekday,
                        };
                    }
                    if !self.days.is_empty() && a != 0 {
                        let dim = days_in_month(cur);
                        b = match table.day[p.date as usize] {
                            None => dim - p.date + first.day,
                            Some(next) if next > dim => dim - p.date + first.day,
                            Some(next) => next - p.date,
                        };
                    }
                    let add = a.min(b);
                    if add != 0 {
                        cur = adjust_dst(start_of_day(cur, add)?)?;
                        continue;
                    }
                }
            }
            if !self.months.is_empty() {
                let month = p.month + 1;
                // `clampBoundaryDay` going forward: the first allowed day when only days of the month are set.
                let boundary_day = if !self.days.is_empty() && self.weekdays.is_empty() { first.day } else { 1 };
                match table.month[month as usize] {
                    None => {
                        let moved = set(cur, Set { year: Some(p.year + 1), ..Default::default() })?;
                        let moved = set(moved, Set { month: Some(first.month), date: Some(boundary_day), ..Default::default() })?;
                        cur = adjust_dst(set(
                            moved,
                            Set { hours: Some(first.hour), minutes: Some(first.minute), seconds: Some(first.second), ..Default::default() },
                        )?)?;
                        continue;
                    }
                    Some(next) if next > month => {
                        let moved = set(cur, Set { month: Some(next - 1), date: Some(boundary_day), ..Default::default() })?;
                        cur = adjust_dst(set(
                            moved,
                            Set { hours: Some(first.hour), minutes: Some(first.minute), seconds: Some(first.second), ..Default::default() },
                        )?)?;
                        continue;
                    }
                    _ => {}
                }
            }
            if before_start(cur)? {
                cur = set(cur, Set { seconds: Some(p.seconds + 1), ..Default::default() })?;
                i = 0;
                continue;
            }
            return Some(from_adjusted(cur, &zone));
        }
    }
}

/// `Intl` (Bun's ICU) names the default zone by its canonical id: links resolve to their target and the UTC aliases
/// to `UTC`. Generated by running Bun with `TZ=<id>` for every chrono-tz id.
const LOCAL_CANONICAL: [(&str, &str); 152] = [
    ("Africa/Asmera", "Africa/Asmara"),
    ("Africa/Timbuktu", "Africa/Bamako"),
    ("America/Argentina/ComodRivadavia", "America/Argentina/Catamarca"),
    ("America/Atka", "America/Adak"),
    ("America/Buenos_Aires", "America/Argentina/Buenos_Aires"),
    ("America/Catamarca", "America/Argentina/Catamarca"),
    ("America/Coral_Harbour", "America/Atikokan"),
    ("America/Cordoba", "America/Argentina/Cordoba"),
    ("America/Ensenada", "America/Tijuana"),
    ("America/Fort_Wayne", "America/Indiana/Indianapolis"),
    ("America/Godthab", "America/Nuuk"),
    ("America/Indianapolis", "America/Indiana/Indianapolis"),
    ("America/Jujuy", "America/Argentina/Jujuy"),
    ("America/Knox_IN", "America/Indiana/Knox"),
    ("America/Louisville", "America/Kentucky/Louisville"),
    ("America/Mendoza", "America/Argentina/Mendoza"),
    ("America/Montreal", "America/Toronto"),
    ("America/Nipigon", "America/Toronto"),
    ("America/Pangnirtung", "America/Iqaluit"),
    ("America/Porto_Acre", "America/Rio_Branco"),
    ("America/Rainy_River", "America/Winnipeg"),
    ("America/Rosario", "America/Argentina/Cordoba"),
    ("America/Santa_Isabel", "America/Tijuana"),
    ("America/Shiprock", "America/Denver"),
    ("America/Thunder_Bay", "America/Toronto"),
    ("America/Virgin", "America/St_Thomas"),
    ("America/Yellowknife", "America/Edmonton"),
    ("Antarctica/South_Pole", "Antarctica/McMurdo"),
    ("Asia/Ashkhabad", "Asia/Ashgabat"),
    ("Asia/Calcutta", "Asia/Kolkata"),
    ("Asia/Choibalsan", "Asia/Ulaanbaatar"),
    ("Asia/Chongqing", "Asia/Shanghai"),
    ("Asia/Chungking", "Asia/Shanghai"),
    ("Asia/Dacca", "Asia/Dhaka"),
    ("Asia/Harbin", "Asia/Shanghai"),
    ("Asia/Istanbul", "Europe/Istanbul"),
    ("Asia/Kashgar", "Asia/Urumqi"),
    ("Asia/Katmandu", "Asia/Kathmandu"),
    ("Asia/Macao", "Asia/Macau"),
    ("Asia/Rangoon", "Asia/Yangon"),
    ("Asia/Saigon", "Asia/Ho_Chi_Minh"),
    ("Asia/Tel_Aviv", "Asia/Jerusalem"),
    ("Asia/Thimbu", "Asia/Thimphu"),
    ("Asia/Ujung_Pandang", "Asia/Makassar"),
    ("Asia/Ulan_Bator", "Asia/Ulaanbaatar"),
    ("Atlantic/Faeroe", "Atlantic/Faroe"),
    ("Atlantic/Jan_Mayen", "Arctic/Longyearbyen"),
    ("Australia/ACT", "Australia/Sydney"),
    ("Australia/Canberra", "Australia/Sydney"),
    ("Australia/Currie", "Australia/Hobart"),
    ("Australia/LHI", "Australia/Lord_Howe"),
    ("Australia/NSW", "Australia/Sydney"),
    ("Australia/North", "Australia/Darwin"),
    ("Australia/Queensland", "Australia/Brisbane"),
    ("Australia/South", "Australia/Adelaide"),
    ("Australia/Tasmania", "Australia/Hobart"),
    ("Australia/Victoria", "Australia/Melbourne"),
    ("Australia/West", "Australia/Perth"),
    ("Australia/Yancowinna", "Australia/Broken_Hill"),
    ("Brazil/Acre", "America/Rio_Branco"),
    ("Brazil/DeNoronha", "America/Noronha"),
    ("Brazil/East", "America/Sao_Paulo"),
    ("Brazil/West", "America/Manaus"),
    ("CET", "Europe/Brussels"),
    ("CST6CDT", "America/Chicago"),
    ("Canada/Atlantic", "America/Halifax"),
    ("Canada/Central", "America/Winnipeg"),
    ("Canada/Eastern", "America/Toronto"),
    ("Canada/Mountain", "America/Edmonton"),
    ("Canada/Newfoundland", "America/St_Johns"),
    ("Canada/Pacific", "America/Vancouver"),
    ("Canada/Saskatchewan", "America/Regina"),
    ("Canada/Yukon", "America/Whitehorse"),
    ("Chile/Continental", "America/Santiago"),
    ("Chile/EasterIsland", "Pacific/Easter"),
    ("Cuba", "America/Havana"),
    ("EET", "Europe/Athens"),
    ("EST", "America/Panama"),
    ("EST5EDT", "America/New_York"),
    ("Egypt", "Africa/Cairo"),
    ("Eire", "Europe/Dublin"),
    ("Etc/GMT", "UTC"),
    ("Etc/GMT+0", "UTC"),
    ("Etc/GMT-0", "UTC"),
    ("Etc/GMT0", "UTC"),
    ("Etc/Greenwich", "UTC"),
    ("Etc/UCT", "UTC"),
    ("Etc/UTC", "UTC"),
    ("Etc/Universal", "UTC"),
    ("Etc/Zulu", "UTC"),
    ("Europe/Belfast", "Europe/London"),
    ("Europe/Kiev", "Europe/Kyiv"),
    ("Europe/Nicosia", "Asia/Nicosia"),
    ("Europe/Tiraspol", "Europe/Chisinau"),
    ("Europe/Uzhgorod", "Europe/Kyiv"),
    ("Europe/Zaporozhye", "Europe/Kyiv"),
    ("GB", "Europe/London"),
    ("GB-Eire", "Europe/London"),
    ("GMT", "UTC"),
    ("GMT+0", "UTC"),
    ("GMT-0", "UTC"),
    ("GMT0", "UTC"),
    ("Greenwich", "UTC"),
    ("HST", "Pacific/Honolulu"),
    ("Hongkong", "Asia/Hong_Kong"),
    ("Iceland", "Atlantic/Reykjavik"),
    ("Iran", "Asia/Tehran"),
    ("Israel", "Asia/Jerusalem"),
    ("Jamaica", "America/Jamaica"),
    ("Japan", "Asia/Tokyo"),
    ("Kwajalein", "Pacific/Kwajalein"),
    ("Libya", "Africa/Tripoli"),
    ("MET", "Europe/Brussels"),
    ("MST", "America/Phoenix"),
    ("MST7MDT", "America/Denver"),
    ("Mexico/BajaNorte", "America/Tijuana"),
    ("Mexico/BajaSur", "America/Mazatlan"),
    ("Mexico/General", "America/Mexico_City"),
    ("NZ", "Pacific/Auckland"),
    ("NZ-CHAT", "Pacific/Chatham"),
    ("Navajo", "America/Denver"),
    ("PRC", "Asia/Shanghai"),
    ("PST8PDT", "America/Los_Angeles"),
    ("Pacific/Enderbury", "Pacific/Kanton"),
    ("Pacific/Johnston", "Pacific/Honolulu"),
    ("Pacific/Ponape", "Pacific/Pohnpei"),
    ("Pacific/Samoa", "Pacific/Pago_Pago"),
    ("Pacific/Truk", "Pacific/Chuuk"),
    ("Pacific/Yap", "Pacific/Chuuk"),
    ("Poland", "Europe/Warsaw"),
    ("Portugal", "Europe/Lisbon"),
    ("ROC", "Asia/Taipei"),
    ("ROK", "Asia/Seoul"),
    ("Singapore", "Asia/Singapore"),
    ("Turkey", "Europe/Istanbul"),
    ("UCT", "UTC"),
    ("US/Alaska", "America/Anchorage"),
    ("US/Aleutian", "America/Adak"),
    ("US/Arizona", "America/Phoenix"),
    ("US/Central", "America/Chicago"),
    ("US/East-Indiana", "America/Indiana/Indianapolis"),
    ("US/Eastern", "America/New_York"),
    ("US/Hawaii", "Pacific/Honolulu"),
    ("US/Indiana-Starke", "America/Indiana/Knox"),
    ("US/Michigan", "America/Detroit"),
    ("US/Mountain", "America/Denver"),
    ("US/Pacific", "America/Los_Angeles"),
    ("US/Samoa", "Pacific/Pago_Pago"),
    ("Universal", "UTC"),
    ("W-SU", "Europe/Moscow"),
    ("WET", "Europe/Lisbon"),
    ("Zulu", "UTC"),
];

/// Expected values were produced by Effect 4.0.6 on Bun 1.4.4 (`TZ=UTC`); the full comparison corpus (≈75,000
/// checks across several `TZ` settings, including a seeded random sweep of cron expressions, zones and instants)
/// matched exactly.
#[cfg(test)]
mod tests {
    use super::*;

    fn named(id: &str) -> Zone {
        zone_make_named(id).unwrap()
    }

    #[test]
    fn durations() {
        let cases: [(&str, Option<f64>); 22] = [
            ("90 seconds", Some(90_000.0)),
            ("1.5 hours", Some(5_400_000.0)),
            ("0.5 minutes", Some(30_000.0)),
            ("-1 days", Some(-86_400_000.0)),
            ("-0 seconds", Some(0.0)),
            ("1 week", Some(604_800_000.0)),
            ("1.5 millis", Some(1.5)),
            ("1.0000001 seconds", Some(1000.0001)),
            ("0.0000001 seconds", Some(0.0001)),
            ("3.333333333333 nanos", Some(3e-6)),
            ("1.4999999 nanos", Some(1e-6)),
            ("-2.5 micros", Some(-0.0025)),
            ("5\u{3000}minutes", Some(300_000.0)),
            ("1\u{FEFF}hours", Some(3_600_000.0)),
            ("Infinity", Some(f64::INFINITY)),
            ("1\u{85}hours", None),
            ("5minutes", None),
            ("5 Minutes", None),
            ("5 mins", None),
            ("5. minutes", None),
            ("+5 minutes", None),
            ("infinity", None),
        ];
        for (input, want) in cases {
            assert_eq!(duration_millis(input), want, "{input:?}");
        }
    }

    #[test]
    fn zones() {
        let named_cases = [
            ("utc", Some("UTC")),
            ("AMERICA/NEW_YORK", Some("America/New_York")),
            ("US/Eastern", Some("US/Eastern")),
            ("etc/gmt-14", Some("Etc/GMT-14")),
            ("US/Pacific-New", Some("US/Pacific-New")),
            ("+0500", Some("+05:00")),
            ("+05", Some("+05:00")),
            ("-00:00", Some("+00:00")),
            ("-23", Some("-23:00")),
            ("+5", None),
            ("+24:00", None),
            ("GMT+05:00", None),
            (" UTC", None),
            ("Etc/GMT+13", None),
            ("Factory", None),
            ("", None),
        ];
        for (input, want) in named_cases {
            assert_eq!(zone_make_named(input).map(|z| zone_to_string(&z)).as_deref(), want, "{input:?}");
        }
        let string_cases = [
            ("+05", None),
            ("-23", None),
            ("+24:00", Some("+24:00")),
            ("+05:60", Some("+06:00")),
            ("+99:99", Some("+100:39")),
            ("GMT-0800", Some("-08:00")),
            ("GMT+5", None),
            ("x+05:00", None),
            ("-0000", Some("+00:00")),
            ("Europe/Kiev", Some("Europe/Kiev")),
        ];
        for (input, want) in string_cases {
            assert_eq!(zone_from_string(input).map(|z| zone_to_string(&z)).as_deref(), want, "{input:?}");
        }
    }

    #[test]
    fn dates() {
        let cases: [(&str, Option<i64>); 26] = [
            ("2026-10-27T09:00Z", Some(1793091600000)),
            ("2026-10-27t09:00Z", Some(1793091600000)),
            ("2026-10-27 09:00Z", Some(1793091600000)),
            ("2026-10-27T09:00:00.1234567891234Z", Some(1793091600123)),
            ("2026-10-27T24:00Z", Some(1793145600000)),
            ("2026-02-29T00:00Z", Some(1772323200000)),
            ("2026-10-27T09:00-0530", Some(1793111400000)),
            ("2026-10-27 09:00+02", Some(1793084400000)),
            ("2026/10/27 09:00Z", Some(1793091600000)),
            ("Tue, 27 Oct 2026 09:00:00 +0200", Some(1793084400000)),
            ("Oct 27, 2026 9:00 AM GMT", Some(1793091600000)),
            ("(x) 2026-10-27 09:00Z", Some(1793091600000)),
            ("+002026-10-27T09:00Z", Some(1793091600000)),
            ("-000001-01-01T00:00Z", Some(-62198755200000)),
            ("+275760-09-13T00:00Z", Some(8640000000000000)),
            ("2026Z", Some(1767225600000)),
            ("2026-10-27T09:00+02", None),
            ("2026-10-27T9:00Z", None),
            ("2026-10-27T24:01Z", None),
            ("2026-10-27T09:00:60Z", None),
            ("2026-10-27T09:00:00.Z", None),
            ("-000000-01-01T00:00Z", None),
            ("+275760-09-14T00:00Z", None),
            ("Jan 5 2026 13:00 pm GMT", None),
            ("2026-10-27T0900Z", None),
            ("tomorrowZ", None),
        ];
        for (input, want) in cases {
            assert_eq!(js_date_parse(input), want, "{input:?}");
        }
    }

    #[test]
    fn parse_at_times() {
        let cases: [(&str, &str, Option<i64>); 24] = [
            ("2026-10-27T09:00", "America/New_York", Some(1793106000000)),
            ("2026-10-27", "America/New_York", Some(1793073600000)),
            ("2026-10-27 9:5", "America/New_York", Some(1793106300000)),
            ("2026-10-27T09:00:00.000+05:30", "America/New_York", Some(1793071800000)),
            ("2026-10-27 09:00+02", "America/New_York", Some(1793098800000)),
            ("2026-10-27T09:00z", "Europe/Berlin", Some(1793091600000)),
            ("2026-10-27T09:00GMT", "Europe/Berlin", None),
            ("2026-10-27T09:00[America/New_York]", "Europe/Berlin", None),
            ("2026-10-27T9:00", "UTC", None),
            // New York springs forward at 02:00 on 2026-03-08 and falls back at 02:00 on 2026-11-01.
            ("2026-03-08T01:59", "America/New_York", Some(1772953140000)),
            ("2026-03-08T02:30", "America/New_York", Some(1772955000000)),
            ("2026-03-08T03:00", "America/New_York", Some(1772953200000)),
            ("2026-11-01T01:30", "America/New_York", Some(1793511000000)),
            ("2026-11-01T02:00", "America/New_York", Some(1793516400000)),
            // Berlin: 2026-03-29 02:00 -> 03:00, 2026-10-25 03:00 -> 02:00.
            ("2026-03-29T02:30", "Europe/Berlin", Some(1774747800000)),
            ("2026-10-25T02:30", "Europe/Berlin", Some(1792888200000)),
            // Lord Howe moves by 30 minutes.
            ("2026-04-05T01:30", "Australia/Lord_Howe", Some(1775313000000)),
            ("2026-10-04T02:15", "Australia/Lord_Howe", Some(1791042300000)),
            ("Oct 27 2026", "Asia/Kolkata", Some(1793039400000)),
            ("10/27/2026 9:00 PM", "Asia/Kolkata", None),
            ("2026-1-5 09:00", "UTC", Some(1767603600000)),
            ("2026-10-27T09:00", "+05:30", Some(1793071800000)),
            ("2026-10-27T09:00", "Pacific/Chatham", Some(1793042100000)),
            ("tomorrow", "UTC", None),
        ];
        for (input, zone, want) in cases {
            assert_eq!(parse_at(input, &named(zone)), want, "{input:?} in {zone}");
        }
        assert_eq!(parse_at("2026-10-27T09:00", &Zone::Offset(-14_400_000)), Some(1793106000000));
    }

    #[test]
    fn formatting() {
        assert_eq!(format_iso(0), "1970-01-01T00:00:00.000Z");
        assert_eq!(format_iso(-62198755200001), "-000002-12-31T23:59:59.999Z");
        assert_eq!(format_iso(253402300800000), "+010000-01-01T00:00:00.000Z");
        let cases = [
            (1793512800000, "America/New_York", "2026-11-01T01:00:00.000-05:00"),
            (1793509200000, "America/New_York", "2026-11-01T01:00:00.000-04:00"),
            (1774746000000, "Europe/Berlin", "2026-03-29T03:00:00.000+02:00"),
            (1793091600000, "Asia/Kolkata", "2026-10-27T14:30:00.000+05:30"),
            (1793091600000, "+05:30", "2026-10-27T14:30:00.000+05:30"),
            (0, "UTC", "1970-01-01T00:00:00.000+00:00"),
        ];
        for (ms, zone, want) in cases {
            assert_eq!(format_iso_offset(ms, &named(zone)), want);
        }
        assert_eq!(format_iso_offset(1793091600000, &Zone::Offset(30_000)), "2026-10-27T09:00:30.000+00:01");
        assert_eq!(format_iso_offset(1793091600000, &Zone::Offset(-2_700_000)), "2026-10-27T08:15:00.000-00:45");
    }

    #[test]
    fn cron_errors() {
        let cases = [
            ("0 9 * *", "Invalid number of segments in cron expression"),
            ("", "Invalid number of segments in cron expression"),
            ("61 * * * *", "Expected a value between 0 and 59"),
            ("0 0 0 * *", "Expected a value between 1 and 31"),
            ("0 0 * * 8", "Expected a value between 0 and 7"),
            ("5-1 * * * *", "Invalid value range"),
            ("*/0 * * * *", "Expected step value to be greater than 0"),
            ("*/61 * * * *", "Expected step value to be less than or equal to 59"),
            ("*/x * * * *", "Expected step value to be a positive integer"),
            ("0 12 L * *", "Expected a positive integer"),
            ("1,,2 * * * *", "Expected a positive integer"),
            ("0 9 * * -1", "Expected a positive integer"),
        ];
        for (expr, want) in cases {
            assert_eq!(cron_parse(expr, Some(&named("UTC"))).err().as_deref(), Some(want), "{expr:?}");
        }
        assert_eq!(cron_parse_tz("0 9 * * *", "Nope/Zone").err().as_deref(), Some("Invalid time zone in cron expression"));
        assert_eq!(cron_parse_tz("bad", "Nope/Zone").err().as_deref(), Some("Invalid number of segments in cron expression"));
        assert_eq!(cron_parse("*/10 * * * * *", None).map(|c| c.seconds_len()), Ok(6));
        assert_eq!(cron_parse("* 0 9 * * *", None).map(|c| c.seconds_len()), Ok(0));
        assert_eq!(cron_parse("0 9 * * 1-5", None).map(|c| c.seconds_len()), Ok(1));
        assert_eq!(cron_parse("0 9 */2 * 1", None).map(|c| c.and), Ok(true));
        assert_eq!(cron_parse("0 9 1-7 * 1", None).map(|c| c.and), Ok(false));
    }

    fn sequence(expr: &str, zone: &str, from: i64) -> Vec<Option<i64>> {
        let cron = cron_parse(expr, Some(&named(zone))).unwrap();
        let mut at = from;
        let mut out = Vec::new();
        for _ in 0..4 {
            let next = cron.next(at);
            out.push(next);
            match next {
                Some(n) => at = n,
                None => break,
            }
        }
        out
    }

    #[test]
    fn cron_next() {
        let ny = "America/New_York";
        // 2026-03-08T03:00Z (Saturday evening in New York, the night clocks spring forward).
        let spring = 1772938800000;
        // 2026-11-01T04:00Z (midnight in New York, the night clocks fall back).
        let fall = 1793505600000;
        let s = |v: &[i64]| v.iter().map(|&x| Some(x)).collect::<Vec<_>>();
        assert_eq!(sequence("0 9 * * 1-5", ny, spring), s(&[1773061200000, 1773147600000, 1773234000000, 1773320400000]));
        assert_eq!(sequence("30 2 * * *", ny, spring), s(&[1772955000000, 1773037800000, 1773124200000, 1773210600000]));
        assert_eq!(sequence("0 */2 * * *", ny, spring), s(&[1772946000000, 1772953200000, 1772956800000, 1772964000000]));
        assert_eq!(sequence("30 1 * * *", ny, fall), s(&[1793511000000, 1793601000000, 1793687400000, 1793773800000]));
        assert_eq!(sequence("0 0 * * * *", ny, fall), s(&[1793509200000, 1793516400000, 1793520000000, 1793523600000]));
        assert_eq!(sequence("0 0/30 * * * *", ny, fall), s(&[1793507400000, 1793509200000, 1793511000000, 1793516400000]));
        assert_eq!(sequence("0 9 31 * *", ny, spring), s(&[1774962000000, 1780232400000, 1785502800000, 1788181200000]));
        assert_eq!(sequence("0 9 29 2 *", ny, spring), s(&[1835445600000, 1961676000000, 2087906400000, 2214136800000]));
        assert_eq!(sequence("0 0 30 2 *", ny, spring), vec![None]);
        assert_eq!(sequence("0 9 * * 0,7", ny, fall), s(&[1793541600000, 1794146400000, 1794751200000, 1795356000000]));
        assert_eq!(sequence("0 12 1,15 * *", ny, fall), s(&[1793552400000, 1794762000000, 1796144400000, 1797354000000]));
        assert_eq!(sequence("0 9 */2 * 1", ny, spring), s(&[1773061200000, 1774270800000, 1776085200000, 1777294800000]));
        assert_eq!(sequence("*/10 * * * * *", ny, spring), s(&[1772938810000, 1772938820000, 1772938830000, 1772938840000]));
        assert_eq!(sequence("0 9 * * sun", "Australia/Lord_Howe", spring), s(&[1773525600000, 1774130400000, 1774735200000, 1775341800000]));
        assert_eq!(sequence("0 9 * JAN *", ny, spring), s(&[1798812000000, 1798898400000, 1798984800000, 1799071200000]));
        // A string zone, as stored on the row.
        let cron = cron_parse_tz("0 2 * * *", "-05:00").unwrap();
        assert_eq!(cron.next(spring), Some(1772953200000));
    }
}

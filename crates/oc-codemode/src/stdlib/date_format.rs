//! `Date.prototype.toLocaleString`, `toLocaleDateString` and `toLocaleTimeString` with their locales and options, as
//! Bun 1.4.4's `Intl.DateTimeFormat` gives them (2.4.1, #64938): ECMA-402's option processing with Bun's messages,
//! time zones from the IANA database (`chrono-tz`), and the patterns and names of [`super::intl_data`] (generated from
//! Bun's ICU/CLDR data by `tests/recorder/intl-data.ts`) for twelve locales. A locale without data formats as the
//! first one with its language, else as `en-US` (Bun's default); an option set without a pattern of its own is
//! composed from its date and time halves.

use chrono::{Offset, TimeZone};
use chrono_tz::OffsetComponents;

use super::date::{fields, Fields, MS_PER_SECOND};
use super::intl::{bool_option, prefixed, string_option};
use super::intl_data::{Locale, KEYS as PATTERN_KEYS, LOCALES, PATTERNS};
use crate::interp::coerce::{coerce_to_number, coerce_to_string};
use crate::interp::errors::{range_error, type_error, JsResult};
use crate::value::Value;

/// The options upstream reads, in its order (`dateTimeOptions` in psychoscript's `stdlib/date.ts`).
pub const DATE_TIME_OPTIONS: [&str; 20] = [
    "localeMatcher",
    "calendar",
    "numberingSystem",
    "hour12",
    "hourCycle",
    "timeZone",
    "weekday",
    "era",
    "year",
    "month",
    "day",
    "dayPeriod",
    "hour",
    "minute",
    "second",
    "fractionalSecondDigits",
    "timeZoneName",
    "formatMatcher",
    "dateStyle",
    "timeStyle",
];

/// The option codes of a pattern key, in `intl-data.ts`'s `KEYS` order.
const KEYS: [(&str, &[&str]); 14] = [
    ("weekday", &["narrow", "short", "long"]),
    ("era", &["narrow", "short", "long"]),
    ("year", &["numeric", "2-digit"]),
    ("month", &["numeric", "2-digit", "narrow", "short", "long"]),
    ("day", &["numeric", "2-digit"]),
    ("hour", &["numeric", "2-digit"]),
    ("minute", &["numeric", "2-digit"]),
    ("second", &["numeric", "2-digit"]),
    ("fractionalSecondDigits", &["1", "2", "3"]),
    ("timeZoneName", &["short", "long", "shortOffset", "longOffset", "shortGeneric", "longGeneric"]),
    ("hc", &["t", "f", "h11", "h12", "h23", "h24"]),
    ("dayPeriod", &["narrow", "short", "long"]),
    ("dateStyle", &["full", "long", "medium", "short"]),
    ("timeStyle", &["full", "long", "medium", "short"]),
];

const DATE_KEYS: [&str; 5] = ["weekday", "era", "year", "month", "day"];
const TIME_KEYS: [&str; 7] = ["hour", "minute", "second", "fractionalSecondDigits", "timeZoneName", "hc", "dayPeriod"];

/// A time zone: an IANA zone, a fixed offset (`+05:30`), or the host's local zone.
enum Zone {
    Iana(chrono_tz::Tz),
    Fixed(i32),
    Local,
}

impl Zone {
    fn parse(name: &str) -> Option<Zone> {
        if let Some(seconds) = fixed_offset(name) {
            return Some(Zone::Fixed(seconds));
        }
        let upper = name.to_ascii_uppercase();
        if matches!(upper.as_str(), "UTC" | "GMT" | "ETC/UTC" | "ETC/GMT" | "Z" | "UCT" | "ETC/UCT" | "ETC/ZULU") {
            return Some(Zone::Iana(chrono_tz::UTC));
        }
        chrono_tz::Tz::from_str_insensitive(name).ok().map(Zone::Iana)
    }

    /// The zone's offset in seconds and whether daylight time is in effect, at UTC time `t`.
    fn offset(&self, t: f64) -> (i32, bool) {
        // The IANA tables end; far in the future the zone's current rules apply, so look the offset up at an
        // equivalent time (same month, day and time in a year between 2008 and 2037 with the same calendar).
        const YEAR_2400: f64 = 13_574_563_200_000.0;
        let lookup = if t > YEAR_2400 { super::date::equivalent_time(t) } else { t };
        let Some(utc) = chrono::DateTime::from_timestamp(super::date::chrono_seconds(lookup), 0) else {
            return (0, false);
        };
        match self {
            Zone::Iana(tz) => {
                let o = tz.offset_from_utc_datetime(&utc.naive_utc());
                (o.fix().local_minus_utc(), !o.dst_offset().is_zero())
            }
            Zone::Fixed(s) => (*s, false),
            Zone::Local => ((super::date::local_time(t) - t) as i32 / 1000, false),
        }
    }

    fn name(&self) -> Option<&'static str> {
        match self {
            Zone::Iana(tz) => Some(tz.name()),
            _ => None,
        }
    }
}

/// `+05:30`, `-08`, `+0530`: a fixed offset.
fn fixed_offset(name: &str) -> Option<i32> {
    let (sign, rest) = match name.as_bytes().first()? {
        b'+' => (1, &name[1..]),
        b'-' => (-1, &name[1..]),
        _ => return None,
    };
    let digits: String = rest.chars().filter(|c| *c != ':').collect();
    if !digits.bytes().all(|b| b.is_ascii_digit()) || !(digits.len() == 2 || digits.len() == 4) {
        return None;
    }
    if rest.contains(':') && rest.len() != 5 {
        return None;
    }
    let hours: i32 = digits[..2].parse().ok()?;
    let minutes: i32 = if digits.len() == 4 { digits[2..].parse().ok()? } else { 0 };
    if hours > 23 || minutes > 59 {
        return None;
    }
    Some(sign * (hours * 3600 + minutes * 60))
}

/// The host's local zone, as `Date` reads it.
fn local_zone() -> Zone {
    Zone::parse(&super::date::zone_id()).unwrap_or(Zone::Local)
}

/// The locale with data for the first requested tag: the tag itself, else one with its language; else `en-US`.
fn resolve_locale(tags: &[String]) -> &'static Locale {
    for tag in tags {
        let base = tag.split("-u-").next().unwrap_or(tag);
        if let Some(l) = LOCALES.iter().find(|l| l.tag.eq_ignore_ascii_case(base)) {
            return l;
        }
        let language = base.split('-').next().unwrap_or("");
        if let Some(l) = LOCALES.iter().find(|l| l.tag.split('-').next() == Some(language)) {
            return l;
        }
    }
    &LOCALES[0]
}

/// The formatted date, or "Invalid Date" for NaN (before any option is checked, as ECMA-402 does).
pub fn format(t: f64, method: &str, tags: &[String], options: &[Value]) -> JsResult<String> {
    if t.is_nan() {
        return Ok("Invalid Date".into());
    }
    let get = |name: &str| -> Value {
        DATE_TIME_OPTIONS.iter().position(|k| *k == name).and_then(|i| options.get(i).cloned()).unwrap_or_default()
    };
    // CreateDateTimeFormat's order.
    string_option(&get("localeMatcher"), "localeMatcher", &["lookup", "best fit"], "")?;
    for (name, what) in [("calendar", "calendar"), ("numberingSystem", "numbering system")] {
        let value = get(name);
        if !value.is_undefined() && !well_formed_type(&coerce_to_string(&value).to_rust()) {
            return Err(range_error(format!("{name} is not a well-formed {what} value")));
        }
    }
    let hour12 = bool_option(&get("hour12"));
    let hour_cycle = enum_option(&get("hourCycle"), "hourCycle", &["h11", "h12", "h23", "h24"])?;
    let zone = match get("timeZone") {
        Value::Undefined => local_zone(),
        value => {
            let name = coerce_to_string(&value).to_rust();
            Zone::parse(&name).ok_or_else(|| range_error(format!("invalid time zone: {name}")))?
        }
    };
    let mut chosen: Vec<(&str, String)> = Vec::new();
    for (name, allowed) in [
        ("weekday", &["narrow", "short", "long"][..]),
        ("era", &["narrow", "short", "long"]),
        ("year", &["2-digit", "numeric"]),
        ("month", &["2-digit", "numeric", "narrow", "short", "long"]),
        ("day", &["2-digit", "numeric"]),
        ("dayPeriod", &["narrow", "short", "long"]),
        ("hour", &["2-digit", "numeric"]),
        ("minute", &["2-digit", "numeric"]),
        ("second", &["2-digit", "numeric"]),
    ] {
        if let Some(v) = enum_option(&get(name), name, allowed)? {
            chosen.push((name, v));
        }
    }
    match get("fractionalSecondDigits") {
        Value::Undefined => {}
        value => {
            let n = coerce_to_number(&value).floor();
            if !(1.0..=3.0).contains(&n) {
                return Err(range_error("fractionalSecondDigits is out of range"));
            }
            chosen.push(("fractionalSecondDigits", (n as u32).to_string()));
        }
    }
    let zone_names = ["short", "long", "shortOffset", "longOffset", "shortGeneric", "longGeneric"];
    if let Some(v) = enum_option(&get("timeZoneName"), "timeZoneName", &zone_names)? {
        chosen.push(("timeZoneName", v));
    }
    string_option(&get("formatMatcher"), "formatMatcher", &["basic", "best fit"], "")?;
    let styles = ["full", "long", "medium", "short"];
    let date_style = enum_option(&get("dateStyle"), "dateStyle", &styles)?;
    let time_style = enum_option(&get("timeStyle"), "timeStyle", &styles)?;
    if (date_style.is_some() || time_style.is_some()) && !chosen.is_empty() {
        return Err(type_error("dateStyle and timeStyle may not be used with other DateTimeFormat options"));
    }
    let (required, defaults) = match method {
        "toLocaleDateString" => ("date", "date"),
        "toLocaleTimeString" => ("time", "time"),
        _ => ("any", "all"),
    };
    if required == "date" && time_style.is_some() {
        return Err(type_error("timeStyle is specified while formatting date is requested"));
    }
    if required == "time" && date_style.is_some() {
        return Err(type_error("dateStyle is specified while formatting time is requested"));
    }
    let has = |names: &[&str]| chosen.iter().any(|(n, _)| names.contains(n));
    let mut need_defaults = date_style.is_none() && time_style.is_none();
    if matches!(required, "date" | "any") && has(&["weekday", "year", "month", "day"]) {
        need_defaults = false;
    }
    if matches!(required, "time" | "any") && has(&["dayPeriod", "hour", "minute", "second", "fractionalSecondDigits"]) {
        need_defaults = false;
    }
    if need_defaults {
        if matches!(defaults, "date" | "all") {
            for n in ["year", "month", "day"] {
                chosen.push((n, "numeric".into()));
            }
        }
        if matches!(defaults, "time" | "all") {
            for n in ["hour", "minute", "second"] {
                chosen.push((n, "numeric".into()));
            }
        }
    }
    let mut options: Vec<(&str, String)> = chosen;
    if let Some(s) = date_style {
        options.push(("dateStyle", s));
    }
    if let Some(s) = time_style {
        options.push(("timeStyle", s));
    }
    // hour12 wins over hourCycle; either matters only with an hour.
    if options.iter().any(|(n, _)| *n == "hour" || *n == "timeStyle") {
        match (hour12, hour_cycle) {
            (Some(true), _) => options.push(("hc", "t".into())),
            (Some(false), _) => options.push(("hc", "f".into())),
            (None, Some(c)) => options.push(("hc", c)),
            (None, None) => {}
        }
    }
    let locale = resolve_locale(tags);
    let pattern = pattern_for(locale, &options);
    let (offset, dst) = zone.offset(t);
    let local = fields(t + offset as f64 * MS_PER_SECOND);
    Ok(render(locale, &pattern, &local, &zone, offset, dst))
}

/// An enumerated string option with Bun's message (`hour must be "2-digit" or "numeric"`).
fn enum_option(value: &Value, name: &str, allowed: &[&str]) -> JsResult<Option<String>> {
    if value.is_undefined() {
        return Ok(None);
    }
    let text = coerce_to_string(value).to_rust();
    if allowed.contains(&text.as_str()) {
        return Ok(Some(text));
    }
    let quoted: Vec<String> = allowed.iter().map(|a| format!("\"{a}\"")).collect();
    let list = if quoted.len() == 2 {
        format!("{} or {}", quoted[0], quoted[1])
    } else {
        format!("{}, or {}", quoted[..quoted.len() - 1].join(", "), quoted[quoted.len() - 1])
    };
    Err(range_error(prefixed("", &format!("{name} must be {list}"))))
}

/// A `type` subtag sequence (`gregory`, `latn`): 3 to 8 alphanumerics per subtag.
fn well_formed_type(value: &str) -> bool {
    !value.is_empty() && value.split('-').all(|s| (3..=8).contains(&s.len()) && s.bytes().all(|b| b.is_ascii_alphanumeric()))
}

fn key_of(options: &[(&str, String)], only: Option<&[&str]>) -> String {
    KEYS.iter()
        .map(|(name, values)| {
            let value = options.iter().find(|(n, _)| n == name).map(|(_, v)| v.as_str());
            match value {
                Some(v) if only.is_none_or(|o| o.contains(name)) => {
                    values.iter().position(|x| *x == v).map(|i| (b'a' + i as u8) as char).unwrap_or('-')
                }
                _ => '-',
            }
        })
        .collect()
}

fn lookup(locale: &Locale, key: &str) -> Option<&'static str> {
    let at = PATTERN_KEYS.binary_search(&key).ok()?;
    match locale.patterns[at] {
        u16::MAX => None,
        i => Some(PATTERNS[i as usize]),
    }
}

/// The pattern for the options: its own, else its date and time halves joined as the locale joins its defaults.
fn pattern_for(locale: &Locale, options: &[(&str, String)]) -> String {
    if let Some(p) = lookup(locale, &key_of(options, None)) {
        return p.to_string();
    }
    let composed = compose(locale, options);
    // A zone name the composed pattern lost goes after it, as most locales put it.
    match options.iter().find(|(n, _)| *n == "timeZoneName") {
        Some((_, kind)) if !composed.contains("{z:") => format!("{composed} {{z:{kind}}}"),
        _ => composed,
    }
}

fn compose(locale: &Locale, options: &[(&str, String)]) -> String {
    let date = lookup(locale, &key_of(options, Some(&DATE_KEYS)));
    let mut time_options: Vec<(&str, String)> = options.to_vec();
    if !options.iter().any(|(n, _)| *n == "hour") {
        time_options.retain(|(n, _)| *n != "hc");
    }
    let time = lookup(locale, &key_of(&time_options, Some(&TIME_KEYS)))
        .or_else(|| lookup(locale, &key_of(&time_options, Some(&["hour", "minute", "second", "hc"]))));
    match (date, time) {
        (Some(d), Some(t)) => {
            // The defaults' joint: `{date}, {time}`.
            let default_date = lookup(locale, "--aaa---------").unwrap_or("");
            let default_both = lookup(locale, "--aaaaaa------").unwrap_or("");
            let default_time = lookup(locale, "-----aaa------").unwrap_or("");
            let joint = default_both
                .strip_prefix(default_date)
                .and_then(|rest| rest.strip_suffix(default_time))
                .unwrap_or(", ");
            format!("{d}{joint}{t}")
        }
        (Some(d), None) => d.to_string(),
        (None, Some(t)) => t.to_string(),
        (None, None) => lookup(locale, "--aaaaaa------").unwrap_or("{M}/{d}/{y}, {h}:{mm}:{ss} {a}").to_string(),
    }
}

fn pad2(n: i64) -> String {
    format!("{n:02}")
}

/// A zone name of one of the six types, from the locale's names or its offset format.
fn zone_name(locale: &Locale, zone: &Zone, kind: &str, offset: i32, dst: bool) -> String {
    let index = match kind {
        "short" => Some(0),
        "long" => Some(1),
        "shortGeneric" => Some(2),
        "longGeneric" => Some(3),
        _ => None,
    };
    if let (Some(i), Some(name)) = (index, zone.name()) {
        let english = if locale.tag.starts_with("en-") { super::intl_data::EN_ZONES } else { &[] };
        if let Some((_, names)) = locale.zones.iter().chain(english.iter()).find(|(z, _)| *z == name) {
            return names[i][usize::from(dst)].to_string();
        }
    }
    if offset == 0 {
        return locale.gmt_zero.to_string();
    }
    // The locale's offset format from its examples: `GMT+5:30` (short) and `GMT+05:30` (long) at +5:30, and the
    // minus sign of `GMT-5`.
    let short = locale.offsets[0];
    let prefix_end = short.find(|c: char| c == '+').unwrap_or(short.len());
    let prefix = &short[..prefix_end];
    let minus = locale.offsets[1][prefix_end..].chars().next().unwrap_or('-');
    let sign = if offset < 0 { minus.to_string() } else { "+".to_string() };
    let total = offset.unsigned_abs();
    let (h, m) = (total / 3600, total % 3600 / 60);
    let long = matches!(kind, "long" | "longOffset" | "longGeneric");
    if long {
        format!("{prefix}{sign}{h:02}:{m:02}")
    } else if m == 0 {
        format!("{prefix}{sign}{h}")
    } else {
        format!("{prefix}{sign}{h}:{m:02}")
    }
}

fn render(locale: &Locale, pattern: &str, f: &Fields, zone: &Zone, offset: i32, dst: bool) -> String {
    // `long:f` / `long:s`: the width, and the format or standalone form ICU chose for the pattern.
    let width = |w: &str| match w.split(':').next().unwrap_or("") {
        "narrow" => 0,
        "short" => 1,
        _ => 2,
    };
    let form = |w: &str| usize::from(w.ends_with(":s"));
    let year = if f.year <= 0.0 { 1.0 - f.year } else { f.year } as i64;
    let hours = f.hours as i64;
    let mut out = String::new();
    let mut rest = pattern;
    while !rest.is_empty() {
        if let Some(r) = rest.strip_prefix("{{") {
            out.push('{');
            rest = r;
            continue;
        }
        if let Some(r) = rest.strip_prefix("}}") {
            out.push('}');
            rest = r;
            continue;
        }
        if let Some(r) = rest.strip_prefix('{') {
            let end = r.find('}').unwrap_or(r.len());
            let token = &r[..end];
            rest = r.get(end + 1..).unwrap_or("");
            let (name, arg) = token.split_once(':').unwrap_or((token, ""));
            let text = match (name, arg) {
                ("M", w) if !w.is_empty() => locale.months[form(w)][width(w)][f.month as usize].to_string(),
                ("E", w) => locale.weekdays[form(w)][width(w)][f.weekday as usize].to_string(),
                ("G", w) => locale.eras[width(w)][usize::from(f.year > 0.0)].to_string(),
                ("z", kind) => zone_name(locale, zone, kind, offset, dst),
                ("B", w) => {
                    let noon = hours == 12 && f.minutes == 0.0 && f.seconds == 0.0 && f.ms == 0.0;
                    locale.day_periods[width(w)][if noon { 24 } else { hours as usize }].to_string()
                }
                (name, _) => match name {
                "y" => year.to_string(),
                "yy" => pad2(year % 100),
                "M" => (f.month as i64 + 1).to_string(),
                "MM" => pad2(f.month as i64 + 1),
                "d" => (f.date as i64).to_string(),
                "dd" => pad2(f.date as i64),
                "h" => (if hours % 12 == 0 { 12 } else { hours % 12 }).to_string(),
                "hh" => pad2(if hours % 12 == 0 { 12 } else { hours % 12 }),
                "K" => (hours % 12).to_string(),
                "KK" => pad2(hours % 12),
                "H" => hours.to_string(),
                "HH" => pad2(hours),
                "k" => (if hours == 0 { 24 } else { hours }).to_string(),
                "kk" => pad2(if hours == 0 { 24 } else { hours }),
                "m" => (f.minutes as i64).to_string(),
                "mm" => pad2(f.minutes as i64),
                "s" => (f.seconds as i64).to_string(),
                "ss" => pad2(f.seconds as i64),
                "S" | "SS" | "SSS" => format!("{:03}", f.ms as i64)[..name.len()].to_string(),
                "a" => locale.am_pm[usize::from(hours >= 12)].to_string(),
                "B" => String::new(),
                _ => String::new(),
                },
            };
            out.push_str(&text);
            continue;
        }
        let next = rest.find(['{', '}']).unwrap_or(rest.len());
        let next = if next == 0 { 1 } else { next };
        out.push_str(&rest[..next]);
        rest = &rest[next..];
    }
    out
}

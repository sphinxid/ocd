//! `Date.parse(string)` as upstream's runtime (Bun) evaluates it: the ES date-time string format
//! first, then the lenient legacy format (V8's `DateParser`, which JavaScriptCore in Bun matches for
//! the inputs checked in the tests). Used where upstream parses free-form strings such as Copilot
//! model versions (`core/src/github-copilot/models.ts`), so release dates (and model ordering) agree.
//!
//! Strings without a time zone in the legacy format, and ES date-time forms without an offset, are
//! local time, as in JavaScript.

const NONE: i64 = i64::MAX;

#[derive(Debug, Clone, Copy, PartialEq)]
enum Token {
    End,
    Number { value: i64, length: usize },
    Symbol(u16),
    WhiteSpace,
    Keyword { kind: Keyword, value: i64, length: usize },
    Unknown,
    Invalid,
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum Keyword {
    Invalid,
    MonthName,
    TimeZoneName,
    TimeSeparator,
    AmPm,
}

impl Token {
    fn is_number(&self) -> bool {
        matches!(self, Token::Number { .. })
    }
    fn number(&self) -> i64 {
        match self {
            Token::Number { value, .. } => *value,
            _ => 0,
        }
    }
    fn is_fixed_length_number(&self, n: usize) -> bool {
        matches!(self, Token::Number { length, .. } if *length == n)
    }
    fn is_symbol(&self, c: char) -> bool {
        *self == Token::Symbol(c as u16)
    }
    fn is_ascii_sign(&self) -> bool {
        self.is_symbol('+') || self.is_symbol('-')
    }
    fn ascii_sign(&self) -> i64 {
        if self.is_symbol('-') { -1 } else { 1 }
    }
    fn is_keyword_type(&self, kind: Keyword) -> bool {
        matches!(self, Token::Keyword { kind: k, .. } if *k == kind)
    }
    fn is_keyword_z(&self) -> bool {
        matches!(self, Token::Keyword { kind: Keyword::TimeZoneName, length: 1, value: 0 })
    }
}

/// `IsWhiteSpaceOrLineTerminator`.
fn is_white_space(c: u16) -> bool {
    matches!(c, 0x09 | 0x0b | 0x0c | 0x20 | 0xa0 | 0x1680 | 0x2000..=0x200a | 0x2028 | 0x2029 | 0x202f | 0x205f | 0x3000 | 0xfeff | 0x0a | 0x0d)
}

/// Keyword table: three-letter prefix, kind, value. Words longer than the prefix only match months.
const KEYWORDS: [(&[u8; 3], Keyword, i64); 26] = [
    (b"jan", Keyword::MonthName, 1),
    (b"feb", Keyword::MonthName, 2),
    (b"mar", Keyword::MonthName, 3),
    (b"apr", Keyword::MonthName, 4),
    (b"may", Keyword::MonthName, 5),
    (b"jun", Keyword::MonthName, 6),
    (b"jul", Keyword::MonthName, 7),
    (b"aug", Keyword::MonthName, 8),
    (b"sep", Keyword::MonthName, 9),
    (b"oct", Keyword::MonthName, 10),
    (b"nov", Keyword::MonthName, 11),
    (b"dec", Keyword::MonthName, 12),
    (b"am\0", Keyword::AmPm, 0),
    (b"pm\0", Keyword::AmPm, 12),
    (b"ut\0", Keyword::TimeZoneName, 0),
    (b"utc", Keyword::TimeZoneName, 0),
    (b"z\0\0", Keyword::TimeZoneName, 0),
    (b"gmt", Keyword::TimeZoneName, 0),
    (b"cdt", Keyword::TimeZoneName, -5),
    (b"cst", Keyword::TimeZoneName, -6),
    (b"edt", Keyword::TimeZoneName, -4),
    (b"est", Keyword::TimeZoneName, -5),
    (b"mdt", Keyword::TimeZoneName, -6),
    (b"mst", Keyword::TimeZoneName, -7),
    (b"pdt", Keyword::TimeZoneName, -7),
    (b"pst", Keyword::TimeZoneName, -8),
];

fn keyword(prefix: [u16; 3], length: usize) -> (Keyword, i64) {
    for (word, kind, value) in KEYWORDS.iter().chain([(b"t\0\0", Keyword::TimeSeparator, 0)].iter()) {
        if word.iter().zip(prefix).all(|(a, b)| *a as u16 == b) && (length <= 3 || *kind == Keyword::MonthName) {
            return (*kind, *value);
        }
    }
    (Keyword::Invalid, 0)
}

struct Scanner {
    chars: Vec<u16>,
    pos: usize,
    next: Token,
}

impl Scanner {
    fn new(text: &str) -> Self {
        let mut scanner = Scanner { chars: text.encode_utf16().collect(), pos: 0, next: Token::End };
        scanner.next = scanner.scan();
        scanner
    }

    fn ch(&self) -> u16 {
        self.chars.get(self.pos).copied().unwrap_or(0)
    }

    fn at_end(&self) -> bool {
        self.pos >= self.chars.len()
    }

    fn scan(&mut self) -> Token {
        let start = self.pos;
        if self.at_end() {
            return Token::End;
        }
        let c = self.ch();
        if (b'0' as u16..=b'9' as u16).contains(&c) {
            // Cap the value at nine significant digits; longer numerals are skipped.
            let (mut value, mut digits) = (0i64, 0);
            while !self.at_end() && (b'0' as u16..=b'9' as u16).contains(&self.ch()) {
                if digits < 9 {
                    value = value * 10 + (self.ch() - b'0' as u16) as i64;
                }
                digits += 1;
                self.pos += 1;
            }
            return Token::Number { value, length: self.pos - start };
        }
        for symbol in [':', '-', '+', '.', ')'] {
            if c == symbol as u16 {
                self.pos += 1;
                return Token::Symbol(c);
            }
        }
        if c >= b'A' as u16 && !is_white_space(c) {
            let mut prefix = [0u16; 3];
            let mut length = 0;
            while !self.at_end() && self.ch() >= b'A' as u16 && !is_white_space(self.ch()) {
                if length < 3 {
                    let ch = self.ch();
                    prefix[length] = if (b'A' as u16..=b'Z' as u16).contains(&ch) { ch | 0x20 } else { ch };
                }
                length += 1;
                self.pos += 1;
            }
            let (kind, value) = keyword(prefix, length);
            return Token::Keyword { kind, value, length };
        }
        if is_white_space(c) {
            self.pos += 1;
            return Token::WhiteSpace;
        }
        if c == b'(' as u16 {
            let mut balance = 0;
            loop {
                match self.ch() {
                    0x29 => balance -= 1,
                    0x28 => balance += 1,
                    _ => {}
                }
                self.pos += 1;
                if !(balance > 0 && !self.at_end()) {
                    break;
                }
            }
            return Token::Unknown;
        }
        self.pos += 1;
        Token::Unknown
    }

    fn next(&mut self) -> Token {
        let token = self.next;
        self.next = self.scan();
        token
    }

    fn peek(&self) -> Token {
        self.next
    }

    fn skip_symbol(&mut self, c: char) -> bool {
        if self.next.is_symbol(c) {
            self.next();
            return true;
        }
        false
    }
}

fn between(x: i64, lo: i64, hi: i64) -> bool {
    x >= lo && x <= hi
}
fn is_month(x: i64) -> bool {
    between(x, 1, 12)
}
fn is_day(x: i64) -> bool {
    between(x, 1, 31)
}
fn is_hour(x: i64) -> bool {
    between(x, 0, 23)
}
fn is_minute(x: i64) -> bool {
    between(x, 0, 59)
}
fn is_second(x: i64) -> bool {
    between(x, 0, 59)
}
fn is_hour12(x: i64) -> bool {
    between(x, 0, 12)
}
fn is_millisecond(x: i64) -> bool {
    between(x, 0, 999)
}

#[derive(Default)]
struct Day {
    comp: [i64; 3],
    index: usize,
    named_month: Option<i64>,
    iso: bool,
}

impl Day {
    fn add(&mut self, n: i64) -> bool {
        if self.index == 3 {
            return false;
        }
        self.comp[self.index] = n;
        self.index += 1;
        true
    }

    fn write(&mut self) -> Option<(i64, i64, i64)> {
        if self.index < 1 {
            return None;
        }
        while self.index < 3 {
            self.comp[self.index] = 1;
            self.index += 1;
        }
        let (mut year, month, day);
        match self.named_month {
            None => {
                if self.iso || !is_day(self.comp[0]) {
                    (year, month, day) = (self.comp[0], self.comp[1], self.comp[2]);
                } else {
                    (year, month, day) = (self.comp[2], self.comp[0], self.comp[1]);
                }
            }
            Some(named) => {
                month = named;
                if !is_day(self.comp[0]) {
                    (year, day) = (self.comp[0], self.comp[1]);
                } else {
                    (day, year) = (self.comp[0], self.comp[1]);
                }
            }
        }
        if !self.iso {
            if between(year, 0, 49) {
                year += 2000;
            } else if between(year, 50, 99) {
                year += 1900;
            }
        }
        if !between(year, -(1 << 30), (1 << 30) - 1) || !is_month(month) || !is_day(day) {
            return None;
        }
        Some((year, month - 1, day))
    }
}

struct Time {
    comp: [i64; 4],
    index: usize,
    hour_offset: i64,
}

impl Time {
    fn new() -> Self {
        Time { comp: [0; 4], index: 0, hour_offset: NONE }
    }
    fn is_empty(&self) -> bool {
        self.index == 0
    }
    fn is_expecting(&self, n: i64) -> bool {
        (self.index == 1 && is_minute(n)) || (self.index == 2 && is_second(n)) || (self.index == 3 && is_millisecond(n))
    }
    fn add(&mut self, n: i64) -> bool {
        if self.index < 4 {
            self.comp[self.index] = n;
            self.index += 1;
            true
        } else {
            false
        }
    }
    fn add_final(&mut self, n: i64) -> bool {
        if !self.add(n) {
            return false;
        }
        while self.index < 4 {
            self.comp[self.index] = 0;
            self.index += 1;
        }
        true
    }
    fn write(&mut self) -> Option<[i64; 4]> {
        while self.index < 4 {
            self.comp[self.index] = 0;
            self.index += 1;
        }
        let [mut hour, minute, second, ms] = self.comp;
        if self.hour_offset != NONE {
            if !is_hour12(hour) {
                return None;
            }
            hour %= 12;
            hour += self.hour_offset;
        }
        if (!is_hour(hour) || !is_minute(minute) || !is_second(second) || !is_millisecond(ms))
            && (hour != 24 || minute != 0 || second != 0 || ms != 0)
        {
            return None;
        }
        Some([hour, minute, second, ms])
    }
}

struct Zone {
    sign: i64,
    hour: i64,
    minute: i64,
}

impl Zone {
    fn new() -> Self {
        Zone { sign: NONE, hour: NONE, minute: NONE }
    }
    fn set(&mut self, offset_hours: i64) {
        self.sign = if offset_hours < 0 { -1 } else { 1 };
        self.hour = offset_hours * self.sign;
        self.minute = 0;
    }
    fn is_expecting(&self, n: i64) -> bool {
        self.hour != NONE && self.minute == NONE && is_minute(n)
    }
    fn is_utc(&self) -> bool {
        self.hour == 0 && self.minute == 0
    }
    fn is_empty(&self) -> bool {
        self.sign == NONE
    }
    /// UTC offset in seconds; `None` = local time. `Err` = invalid.
    fn write(&mut self) -> Result<Option<i64>, ()> {
        if self.sign == NONE {
            return Ok(None);
        }
        if self.hour == NONE {
            self.hour = 0;
        }
        if self.minute == NONE {
            self.minute = 0;
        }
        let total = self.hour as u64 * 3600 + self.minute as u64 * 60;
        if total > (1u64 << 30) - 1 {
            return Err(());
        }
        Ok(Some(if self.sign < 0 { -(total as i64) } else { total as i64 }))
    }
}

/// `ReadMilliseconds`: the first three digits of a fraction, scaled to milliseconds.
fn read_milliseconds(token: Token) -> i64 {
    let Token::Number { value, length } = token else { return 0 };
    let mut number = value;
    if length == 1 {
        number * 100
    } else if length == 2 {
        number * 10
    } else if length == 3 {
        number
    } else {
        // More than three digits: keep the first three (the value holds at most nine).
        let mut digits = length.min(9);
        while digits > 3 {
            number /= 10;
            digits -= 1;
        }
        number
    }
}

/// The ES date-time string part; returns the first token it did not handle.
fn parse_es5(scanner: &mut Scanner, day: &mut Day, time: &mut Time, tz: &mut Zone) -> Token {
    if scanner.peek().is_ascii_sign() {
        let sign_token = scanner.next();
        if !scanner.peek().is_fixed_length_number(6) {
            return sign_token;
        }
        let sign = sign_token.ascii_sign();
        let year = scanner.next().number();
        if sign < 0 && year == 0 {
            return sign_token;
        }
        day.add(sign * year);
    } else if scanner.peek().is_fixed_length_number(4) {
        day.add(scanner.next().number());
    } else {
        return scanner.next();
    }
    if scanner.skip_symbol('-') {
        if !scanner.peek().is_fixed_length_number(2) || !is_month(scanner.peek().number()) {
            return scanner.next();
        }
        day.add(scanner.next().number());
        if scanner.skip_symbol('-') {
            if !scanner.peek().is_fixed_length_number(2) || !is_day(scanner.peek().number()) {
                return scanner.next();
            }
            day.add(scanner.next().number());
        }
    }
    if !scanner.peek().is_keyword_type(Keyword::TimeSeparator) {
        if scanner.peek() != Token::End {
            return scanner.next();
        }
    } else {
        scanner.next();
        if !scanner.peek().is_fixed_length_number(2) || !between(scanner.peek().number(), 0, 24) {
            return Token::Invalid;
        }
        let hour_is_24 = scanner.peek().number() == 24;
        time.add(scanner.next().number());
        if !scanner.skip_symbol(':') {
            return Token::Invalid;
        }
        if !scanner.peek().is_fixed_length_number(2) || !is_minute(scanner.peek().number()) || (hour_is_24 && scanner.peek().number() > 0) {
            return Token::Invalid;
        }
        time.add(scanner.next().number());
        if scanner.skip_symbol(':') {
            if !scanner.peek().is_fixed_length_number(2) || !is_second(scanner.peek().number()) || (hour_is_24 && scanner.peek().number() > 0) {
                return Token::Invalid;
            }
            time.add(scanner.next().number());
            if scanner.skip_symbol('.') {
                if !scanner.peek().is_number() || (hour_is_24 && scanner.peek().number() > 0) {
                    return Token::Invalid;
                }
                time.add(read_milliseconds(scanner.next()));
            }
        }
        if scanner.peek().is_keyword_z() {
            scanner.next();
            tz.set(0);
        } else if scanner.peek().is_symbol('+') || scanner.peek().is_symbol('-') {
            tz.sign = if scanner.next().is_symbol('+') { 1 } else { -1 };
            if scanner.peek().is_fixed_length_number(4) {
                let hourmin = scanner.next().number();
                let (hour, min) = (hourmin / 100, hourmin % 100);
                if !is_hour(hour) || !is_minute(min) {
                    return Token::Invalid;
                }
                tz.hour = hour;
                tz.minute = min;
            } else {
                if !scanner.peek().is_fixed_length_number(2) || !is_hour(scanner.peek().number()) {
                    return Token::Invalid;
                }
                tz.hour = scanner.next().number();
                if !scanner.skip_symbol(':') {
                    return Token::Invalid;
                }
                if !scanner.peek().is_fixed_length_number(2) || !is_minute(scanner.peek().number()) {
                    return Token::Invalid;
                }
                tz.minute = scanner.next().number();
            }
        }
        if scanner.peek() != Token::End {
            return Token::Invalid;
        }
    }
    // Date-only forms without an offset are UTC; date-time forms are local.
    if tz.is_empty() && time.is_empty() {
        tz.set(0);
    }
    day.iso = true;
    Token::End
}

/// Parsed fields: (year, month0, day, [h, m, s, ms], UTC offset seconds or local).
fn parse_fields(text: &str) -> Option<(i64, i64, i64, [i64; 4], Option<i64>)> {
    let mut scanner = Scanner::new(text);
    let (mut day, mut time, mut tz) = (Day::default(), Time::new(), Zone::new());
    let mut token = parse_es5(&mut scanner, &mut day, &mut time, &mut tz);
    if token == Token::Invalid {
        return None;
    }
    let mut has_read_number = day.index > 0;
    while token != Token::End {
        match token {
            Token::Number { value: n, .. } => {
                has_read_number = true;
                if scanner.skip_symbol(':') {
                    if scanner.skip_symbol(':') {
                        if !time.is_empty() {
                            return None;
                        }
                        time.add(n);
                        time.add(0);
                    } else {
                        if !time.add(n) {
                            return None;
                        }
                        if scanner.peek().is_symbol('.') {
                            scanner.next();
                        }
                    }
                } else if scanner.skip_symbol('.') && time.is_expecting(n) {
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
                    let peek = scanner.peek();
                    if peek != Token::End && peek != Token::WhiteSpace && !peek.is_keyword_z() && !peek.is_ascii_sign() {
                        return None;
                    }
                } else {
                    if !day.add(n) {
                        return None;
                    }
                    scanner.skip_symbol('-');
                }
            }
            Token::Keyword { kind, value, .. } => {
                if kind == Keyword::AmPm && !time.is_empty() {
                    time.hour_offset = value;
                } else if kind == Keyword::MonthName {
                    day.named_month = Some(value);
                    scanner.skip_symbol('-');
                } else if kind == Keyword::TimeZoneName && has_read_number {
                    tz.set(value);
                } else {
                    // Garbage words are illegal once a number has been read, and the first number
                    // must be separated from garbage words.
                    if has_read_number || scanner.peek().is_number() {
                        return None;
                    }
                }
            }
            t if t.is_ascii_sign() && (tz.is_utc() || !time.is_empty()) => {
                tz.sign = t.ascii_sign();
                let (mut n, mut length) = (0, 0);
                if scanner.peek().is_number() {
                    let next = scanner.next();
                    if let Token::Number { value, length: l } = next {
                        n = value;
                        length = l;
                    }
                }
                has_read_number = true;
                if scanner.peek().is_symbol(':') {
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
            t if (t.is_ascii_sign() || t.is_symbol(')')) && has_read_number => return None,
            _ => {}
        }
        token = scanner.next();
    }
    let (year, month, d) = day.write()?;
    let clock = time.write()?;
    let offset = tz.write().ok()?;
    Some((year, month, d, clock, offset))
}

/// `MakeDay` (ES 21.4.1.28) for integral inputs.
fn make_day(year: i64, month: i64, date: i64) -> f64 {
    let ym = year as f64 + (month as f64 / 12.0).floor();
    let mn = month.rem_euclid(12) as u32;
    let Some(first) = chrono::NaiveDate::from_ymd_opt(ym as i32, mn + 1, 1) else { return f64::NAN };
    let days = (first - chrono::NaiveDate::from_ymd_opt(1970, 1, 1).unwrap()).num_days() as f64;
    days + date as f64 - 1.0
}

/// Local wall-clock milliseconds → UTC milliseconds (`LocalTime` inverse).
fn local_to_utc(local_ms: f64) -> f64 {
    use chrono::{Local, TimeZone};
    let secs = (local_ms / 1000.0).floor();
    let nanos = ((local_ms - secs * 1000.0) * 1_000_000.0) as u32;
    let Some(naive) = chrono::DateTime::from_timestamp(secs as i64, nanos).map(|d| d.naive_utc()) else { return f64::NAN };
    match Local.from_local_datetime(&naive) {
        chrono::LocalResult::Single(d) | chrono::LocalResult::Ambiguous(d, _) => d.timestamp_millis() as f64,
        chrono::LocalResult::None => {
            // Skipped wall-clock time (DST gap): use the offset in effect just before it.
            let before = naive - chrono::Duration::hours(1);
            match Local.from_local_datetime(&before) {
                chrono::LocalResult::Single(d) | chrono::LocalResult::Ambiguous(d, _) => d.timestamp_millis() as f64 + 3_600_000.0,
                chrono::LocalResult::None => f64::NAN,
            }
        }
    }
}

/// `Date.parse(text)`: epoch milliseconds, `None` for `NaN`.
pub fn parse(text: &str) -> Option<f64> {
    parse_with(text, local_to_utc)
}

fn parse_with(text: &str, local_to_utc: fn(f64) -> f64) -> Option<f64> {
    const MAX_TIME_MS: f64 = 8.64e15;
    let (year, month, day, [h, m, s, ms], offset) = parse_fields(text)?;
    let date = make_day(year, month, day) * 86_400_000.0 + (h * 3_600_000 + m * 60_000 + s * 1000 + ms) as f64;
    let value = match offset {
        None => {
            if date.abs() > MAX_TIME_MS + 864_000_000.0 {
                return None;
            }
            local_to_utc(date)
        }
        Some(seconds) => date - seconds as f64 * 1000.0,
    };
    (value.is_finite() && value.abs() <= MAX_TIME_MS).then_some(value)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn utc(local: f64) -> f64 {
        local
    }

    /// Values Bun 1.4.2 returns with `TZ=UTC` for strings without an explicit zone.
    #[test]
    fn matches_bun() {
        let cases: &[(&str, Option<f64>)] = &[
            ("grok-4.6", Some(986515200000.0)),
            ("gpt-5-mini", None),
            ("2025-10-01", Some(1759276800000.0)),
            ("claude-sonnet-4.6", Some(986515200000.0)),
            ("gpt-4o-2024-11-20", None),
            ("4.6", Some(986515200000.0)),
            ("2025", Some(1735689600000.0)),
            ("2025-10", Some(1759276800000.0)),
            ("2025-10-01T00:00:00", Some(1759276800000.0)),
            ("2025-10-01T00:00:00Z", Some(1759276800000.0)),
            ("10/01/2025", Some(1759276800000.0)),
            ("Oct 1 2025", Some(1759276800000.0)),
            ("x 2025", Some(1735689600000.0)),
            ("1", Some(978307200000.0)),
            ("12", Some(1007164800000.0)),
            ("gemini-3.5-flash", None),
            ("o3-mini", None),
            ("v1", None),
            ("2026-03-05-preview", None),
            ("2025-10-01 12:00", Some(1759320000000.0)),
            ("gpt-4.1", Some(986083200000.0)),
            ("claude-3.5", Some(983750400000.0)),
            ("a 1", Some(978307200000.0)),
            ("001", Some(978307200000.0)),
        ];
        for (text, expected) in cases {
            assert_eq!(parse_with(text, utc), *expected, "{text}");
        }
    }

    /// Differential check against a corpus of `<json string>\t<Date.parse result or NaN>` lines
    /// produced by Bun with `TZ=UTC` (`OCD_JS_DATE_CORPUS=<file> cargo test -p oc-ai js_date -- --ignored`).
    #[test]
    #[ignore]
    fn corpus() {
        let Ok(path) = std::env::var("OCD_JS_DATE_CORPUS") else { return };
        let text = std::fs::read_to_string(path).unwrap();
        let mut failures = vec![];
        for line in text.lines() {
            let (input, expected) = line.split_once('\t').unwrap();
            let input: String = serde_json::from_str(input).unwrap();
            let expected = if expected == "NaN" { None } else { Some(expected.parse::<f64>().unwrap()) };
            let actual = parse_with(&input, utc);
            if actual != expected {
                failures.push(format!("{input:?}: expected {expected:?}, got {actual:?}"));
            }
        }
        assert!(failures.is_empty(), "{} mismatches:\n{}", failures.len(), failures.join("\n"));
    }
}

//! `src/prompt/parse.ts`: the file line-range suffix of an `@` mention query, shared by the full TUI and mini.

/// A `#start[-end]` suffix: `end` only when it is greater than `start`.
pub type LineRange = (u64, Option<u64>);

/// `parseFileLineRange` (2.2.5, #60336): the last `#` starts a range only when it is not the first character and
/// the rest is `^(\d+)(?:-(\d*))?$`; otherwise the whole input is the base (so `report#2026.md` stays a path).
pub fn parse_file_line_range(input: &str) -> (String, Option<LineRange>) {
    let Some(hash) = input.rfind('#').filter(|h| *h > 0) else { return (input.to_string(), None) };
    let rest = &input[hash + 1..];
    let (start, end) = match rest.split_once('-') {
        Some((a, b)) => (a, Some(b)),
        None => (rest, None),
    };
    let digits = |s: &str| !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit());
    if !digits(start) || end.is_some_and(|e| !e.is_empty() && !digits(e)) {
        return (input.to_string(), None);
    }
    let number = |s: &str| s.parse::<u64>().unwrap_or(u64::MAX);
    let start = number(start);
    let end = end.filter(|e| !e.is_empty()).map(number).filter(|e| start < *e);
    (input[..hash].to_string(), Some((start, end)))
}

/// `stripFileLineRange`.
pub fn strip_file_line_range(input: &str) -> String {
    parse_file_line_range(input).0
}

/// `${path}#${start}${end ? `-${end}` : ""}`.
pub fn with_line_range(path: &str, range: LineRange) -> String {
    format!("{path}#{}{}", range.0, range.1.map(|e| format!("-{e}")).unwrap_or_default())
}

/// The `?start=…&end=…` search params `pathToFileURL` gets for a range.
pub fn line_range_query(range: LineRange) -> String {
    format!("?start={}{}", range.0, range.1.map(|e| format!("&end={e}")).unwrap_or_default())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(input: &str) -> (String, Option<LineRange>) {
        parse_file_line_range(input)
    }

    fn base(s: &str) -> (String, Option<LineRange>) {
        (s.to_string(), None)
    }

    fn ranged(s: &str, start: u64, end: Option<u64>) -> (String, Option<LineRange>) {
        (s.to_string(), Some((start, end)))
    }

    // parse.test.ts (2.2.5): "preserves file line-range parsing semantics"
    #[test]
    fn preserves_file_line_range_parsing_semantics() {
        assert_eq!(p("src/app.ts#7"), ranged("src/app.ts", 7, None));
        assert_eq!(p("src/app.ts#12-20"), ranged("src/app.ts", 12, Some(20)));
        assert_eq!(p("src/app.ts#12-"), ranged("src/app.ts", 12, None));
        assert_eq!(p("src/app.ts#12-12"), ranged("src/app.ts", 12, None));
        assert_eq!(p("src/app.ts#bad"), base("src/app.ts#bad"));
        assert_eq!(p("src/app.ts#"), base("src/app.ts#"));
        assert_eq!(p("report#2026.md"), base("report#2026.md"));
        assert_eq!(p("#notes.md"), base("#notes.md"));
        assert_eq!(p("#3"), base("#3"));
        assert_eq!(p("report#2026.md#3"), ranged("report#2026.md", 3, None));
        assert_eq!(p("report#2026.md#3-10"), ranged("report#2026.md", 3, Some(10)));
        assert_eq!(p("#notes.md#3"), ranged("#notes.md", 3, None));
        assert_eq!(p("src/app.ts"), base("src/app.ts"));
        let stripped: Vec<String> = ["report#2026.md", "#notes.md", "report#2026.md#3", "src/app.ts#12-20"].iter().map(|s| strip_file_line_range(s)).collect();
        assert_eq!(stripped, ["report#2026.md", "#notes.md", "report#2026.md", "src/app.ts"]);
    }

    #[test]
    fn rejects_suffixes_the_regex_does_not_match() {
        assert_eq!(p("a.ts#1-2-3"), base("a.ts#1-2-3"));
        assert_eq!(p("a.ts#-3"), base("a.ts#-3"));
        assert_eq!(p("a.ts#1-x"), base("a.ts#1-x"));
        assert_eq!(p("a.ts#007"), ranged("a.ts", 7, None));
        assert_eq!(with_line_range("a.ts", (3, Some(10))), "a.ts#3-10");
        assert_eq!(line_range_query((3, None)), "?start=3");
    }
}

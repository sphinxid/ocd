//! Prompt state helpers (`src/mini/prompt.shared.ts`, `prompt.editor.ts`, `prompt/mention.ts` realign,
//! `prompt/content.ts`): the history ring, prompt text helpers and editor-offset measurement.

use super::types::{MentionRange, PromptPart, RunPrompt};
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

const HISTORY_LIMIT: usize = 200;

/// Display width of one grapheme as the editor counts it (tabs are 2 cells, control characters 0).
fn grapheme_width(g: &str) -> usize {
    if g == "\t" {
        return 2;
    }
    if g.chars().all(|c| c.is_control()) {
        return 0;
    }
    UnicodeWidthStr::width(g)
}

/// Cell width of a single line.
pub fn line_width(line: &str) -> usize {
    line.graphemes(true).map(grapheme_width).sum()
}

/// `measureEditText(text)`: the cells of each line plus one per line break (LF, CR or CRLF).
pub fn measure_edit_text(text: &str) -> usize {
    let mut offset = 0;
    let mut rest = text;
    loop {
        match rest.find(['\r', '\n']) {
            None => return offset + line_width(rest),
            Some(i) => {
                offset += line_width(&rest[..i]) + 1;
                let skip = if rest[i..].starts_with("\r\n") { 2 } else { 1 };
                rest = &rest[i + skip..];
            }
        }
    }
}

#[derive(Clone, Debug, PartialEq, Default)]
pub struct PromptHistory {
    pub items: Vec<RunPrompt>,
    pub index: Option<usize>,
    pub draft: String,
}

#[derive(Clone, Debug, PartialEq)]
pub struct PromptMove {
    pub state: PromptHistory,
    pub text: Option<String>,
    pub cursor: Option<usize>,
    pub apply: bool,
}

/// `promptCopy`: text, parts, mode and command (no message id or delivery).
pub fn prompt_copy(prompt: &RunPrompt) -> RunPrompt {
    RunPrompt { message_id: None, text: prompt.text.clone(), parts: prompt.parts.clone(), delivery: None, shell: prompt.shell, command: prompt.command.clone() }
}

fn shift(r: &MentionRange, offset: usize) -> MentionRange {
    MentionRange { start: r.start + offset, end: r.end + offset, value: r.value.clone() }
}

/// `promptAppend(prompt, following)`: part ranges shift by the measured offset.
pub fn prompt_append(prompt: &RunPrompt, following: &RunPrompt) -> RunPrompt {
    let text = if prompt.text.is_empty() { String::new() } else { format!("{}\n\n", prompt.text) };
    let offset = measure_edit_text(&text);
    let command = if prompt.text.is_empty() { following.command.clone() } else { prompt.command.clone() };
    let mut parts = prompt.parts.clone();
    parts.extend(following.parts.iter().map(|part| match part {
        PromptPart::File { url, filename, mime, description, source } => PromptPart::File {
            url: url.clone(),
            filename: filename.clone(),
            mime: mime.clone(),
            description: description.clone(),
            source: source.as_ref().map(|(p, r)| (p.clone(), shift(r, offset))),
        },
        PromptPart::Agent { name, source } => PromptPart::Agent { name: name.clone(), source: source.as_ref().map(|r| shift(r, offset)) },
        PromptPart::Skill { id, source } => PromptPart::Skill { id: id.clone(), source: source.as_ref().map(|r| shift(r, offset)) },
    }));
    RunPrompt { text: text + &following.text, parts, command, ..Default::default() }
}

/// `promptSame`.
pub fn prompt_same(a: &RunPrompt, b: &RunPrompt) -> bool {
    a.shell == b.shell && a.text == b.text && a.parts == b.parts && a.command == b.command
}

pub const EXIT_COMMANDS: [&str; 3] = ["exit", "quit", "q"];

pub fn is_exit_command(input: &str) -> bool {
    let text = input.trim().to_lowercase();
    text == ":q" || EXIT_COMMANDS.iter().any(|n| text == format!("/{n}"))
}

pub fn is_new_command(input: &str) -> bool {
    input.trim().to_lowercase() == "/new"
}

pub fn is_compact_command(input: &str) -> bool {
    input.trim().to_lowercase() == "/compact"
}

fn has_file(p: &RunPrompt) -> bool {
    p.parts.iter().any(|x| x.is_file())
}

/// `createPromptHistory(items)`.
pub fn create_prompt_history(items: &[RunPrompt]) -> PromptHistory {
    let mut next: Vec<RunPrompt> = Vec::new();
    for item in items.iter().filter(|i| !i.text.trim().is_empty() || has_file(i)).map(prompt_copy) {
        if next.last().is_some_and(|l| prompt_same(l, &item)) {
            continue;
        }
        next.push(item);
    }
    let start = next.len().saturating_sub(HISTORY_LIMIT);
    PromptHistory { items: next.split_off(start), index: None, draft: String::new() }
}

/// `pushPromptHistory(state, prompt)`.
pub fn push_prompt_history(state: &PromptHistory, prompt: &RunPrompt) -> PromptHistory {
    if prompt.text.trim().is_empty() && !has_file(prompt) {
        return state.clone();
    }
    let next = prompt_copy(prompt);
    if state.items.last().is_some_and(|l| prompt_same(l, &next)) {
        return PromptHistory { items: state.items.clone(), index: None, draft: String::new() };
    }
    let mut items = state.items.clone();
    items.push(next);
    let start = items.len().saturating_sub(HISTORY_LIMIT);
    PromptHistory { items: items.split_off(start), index: None, draft: String::new() }
}

/// `movePromptHistory(state, dir, text, cursor)`.
pub fn move_prompt_history(state: &PromptHistory, dir: i32, text: &str, cursor: usize) -> PromptMove {
    let none = || PromptMove { state: state.clone(), text: None, cursor: None, apply: false };
    if state.items.is_empty() {
        return none();
    }
    if dir == -1 && cursor != 0 {
        return none();
    }
    if dir == 1 && cursor != measure_edit_text(text) {
        return none();
    }
    let Some(index) = state.index else {
        if dir == 1 {
            return none();
        }
        let idx = state.items.len() - 1;
        return PromptMove {
            state: PromptHistory { items: state.items.clone(), index: Some(idx), draft: text.to_string() },
            text: Some(state.items[idx].text.clone()),
            cursor: Some(0),
            apply: true,
        };
    };
    let idx = index as i64 + dir as i64;
    if idx < 0 {
        return none();
    }
    let idx = idx as usize;
    if idx >= state.items.len() {
        return PromptMove {
            state: PromptHistory { items: state.items.clone(), index: None, draft: state.draft.clone() },
            text: Some(state.draft.clone()),
            cursor: Some(measure_edit_text(&state.draft)),
            apply: true,
        };
    }
    PromptMove {
        state: PromptHistory { items: state.items.clone(), index: Some(idx), draft: state.draft.clone() },
        text: Some(state.items[idx].text.clone()),
        cursor: Some(if dir == -1 { 0 } else { measure_edit_text(&state.items[idx].text) }),
        apply: true,
    }
}

/// `resolveEditorSlashValue(text)`: `/editor args` opens the editor with `args`.
pub fn resolve_editor_slash_value(text: &str) -> String {
    match super::util::parse_slash_head(text) {
        Some(head) if head.name.to_lowercase() == "editor" => head.arguments,
        _ => text.to_string(),
    }
}

/// `normalizePromptContent`: drop one trailing newline from single-line editor content.
pub fn normalize_prompt_content(content: &str) -> String {
    if let Some(body) = content.strip_suffix("\r\n") {
        return if !body.contains('\n') && !body.contains('\r') { body.to_string() } else { content.to_string() };
    }
    if let Some(body) = content.strip_suffix('\n') {
        return if !body.contains('\n') && !body.contains('\r') { body.to_string() } else { content.to_string() };
    }
    content.to_string()
}

#[derive(Clone)]
struct Candidate {
    start: usize,
    end: usize,
    offset: usize,
}

fn mention_ranges(content: &str, text: &str) -> Vec<Candidate> {
    let units: Vec<u16> = content.encode_utf16().collect();
    let needle: Vec<u16> = text.encode_utf16().collect();
    let mut out = Vec::new();
    if needle.is_empty() {
        return out;
    }
    let mut from = 0;
    while from + needle.len() <= units.len() {
        let Some(pos) = (from..=units.len() - needle.len()).find(|i| units[*i..*i + needle.len()] == needle[..]) else { break };
        let prefix = String::from_utf16_lossy(&units[..pos]);
        out.push(Candidate { start: pos, end: pos + needle.len(), offset: measure_edit_text(&prefix) });
        from = pos + needle.len();
    }
    out
}

#[derive(Clone)]
struct Solution {
    matches: usize,
    cost: i64,
    pairs: Vec<(usize, Candidate)>,
}

fn assign_mentions(items: &[(usize, MentionRange)], candidates: &[Candidate]) -> Vec<(usize, Candidate)> {
    let mut ordered: Vec<(usize, MentionRange)> = items.to_vec();
    ordered.sort_by(|a, b| a.1.start.cmp(&b.1.start).then(a.0.cmp(&b.0)));
    let mut memo: std::collections::HashMap<(usize, usize), Solution> = std::collections::HashMap::new();
    fn solve(i: usize, c: usize, ordered: &[(usize, MentionRange)], candidates: &[Candidate], memo: &mut std::collections::HashMap<(usize, usize), Solution>) -> Solution {
        if i >= ordered.len() || c >= candidates.len() {
            return Solution { matches: 0, cost: 0, pairs: vec![] };
        }
        if let Some(s) = memo.get(&(i, c)) {
            return s.clone();
        }
        let tail = solve(i + 1, c + 1, ordered, candidates, memo);
        let current = &ordered[i];
        let range = &candidates[c];
        let mut pairs = vec![(current.0, range.clone())];
        pairs.extend(tail.pairs.iter().cloned());
        let matched = Solution { matches: tail.matches + 1, cost: tail.cost + (range.offset as i64 - current.1.start as i64).abs(), pairs };
        let options = [matched, solve(i + 1, c, ordered, candidates, memo), solve(i, c + 1, ordered, candidates, memo)];
        let mut best = options[0].clone();
        for next in options.iter().skip(1) {
            if next.matches != best.matches {
                if next.matches > best.matches {
                    best = next.clone();
                }
            } else if next.cost < best.cost {
                best = next.clone();
            }
        }
        memo.insert((i, c), best.clone());
        best
    }
    solve(0, 0, &ordered, candidates, &mut memo).pairs
}

/// `realignPromptMentions(content, mentions)`.
pub fn realign_prompt_mentions(content: &str, mentions: &[Option<MentionRange>]) -> Vec<Option<MentionRange>> {
    let mut protected: Vec<(usize, usize)> = Vec::new();
    let mut aligned: Vec<Option<MentionRange>> = mentions.iter().map(|m| m.clone().filter(|m| m.value.is_empty())).collect();
    let mut groups: indexmap::IndexMap<String, Vec<(usize, MentionRange)>> = indexmap::IndexMap::new();
    for (index, m) in mentions.iter().enumerate() {
        if let Some(m) = m.as_ref().filter(|m| !m.value.is_empty()) {
            groups.entry(m.value.clone()).or_default().push((index, m.clone()));
        }
    }
    let mut entries: Vec<(String, Vec<(usize, MentionRange)>)> = groups.into_iter().collect();
    entries.sort_by(|(l, _), (r, _)| r.encode_utf16().count().cmp(&l.encode_utf16().count()).then_with(|| crate::util::locale::compare(l, r)));
    for (text, items) in entries {
        let candidates = mention_ranges(content, &text);
        let available: Vec<Candidate> = candidates.iter().filter(|c| !protected.iter().any(|(s, e)| c.start < *e && c.end > *s)).cloned().collect();
        for (index, candidate) in assign_mentions(&items, &available) {
            aligned[index] = Some(MentionRange { start: candidate.offset, end: candidate.offset + measure_edit_text(&text), value: text.clone() });
        }
        protected.extend(candidates.iter().map(|c| (c.start, c.end)));
    }
    aligned
}

fn part_mention(part: &PromptPart) -> Option<MentionRange> {
    match part {
        PromptPart::File { source, .. } => source.as_ref().map(|(_, r)| r.clone()),
        PromptPart::Agent { source, .. } | PromptPart::Skill { source, .. } => source.clone(),
    }
}

/// `realignEditorPromptParts(content, parts)`: parts whose mention text disappeared are dropped.
pub fn realign_editor_prompt_parts(content: &str, parts: &[PromptPart]) -> Vec<PromptPart> {
    let matches = realign_prompt_mentions(content, &parts.iter().map(part_mention).collect::<Vec<_>>());
    parts
        .iter()
        .enumerate()
        .filter_map(|(index, part)| {
            let Some(mention) = part_mention(part).filter(|m| !m.value.is_empty()) else { return Some(part.clone()) };
            let _ = mention;
            let m = matches[index].clone()?;
            Some(match part {
                PromptPart::Agent { name, .. } => PromptPart::Agent { name: name.clone(), source: Some(m) },
                PromptPart::Skill { id, .. } => PromptPart::Skill { id: id.clone(), source: Some(m) },
                PromptPart::File { url, filename, mime, description, source } => PromptPart::File {
                    url: url.clone(),
                    filename: filename.clone(),
                    mime: mime.clone(),
                    description: description.clone(),
                    source: source.as_ref().map(|(p, _)| (p.clone(), m)),
                },
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(text: &str) -> RunPrompt {
        RunPrompt::text(text)
    }

    #[test]
    fn filters_blank_and_dedupes() {
        let out = create_prompt_history(&[p("   "), p("one"), p("one"), p("two"), p("one")]);
        assert_eq!(out.items.iter().map(|i| i.text.as_str()).collect::<Vec<_>>(), ["one", "two", "one"]);
        assert_eq!(out.index, None);
        assert_eq!(out.draft, "");
    }

    #[test]
    fn push_dedupes_latest() {
        let base = create_prompt_history(&[p("one")]);
        let texts = |h: PromptHistory| h.items.iter().map(|i| i.text.clone()).collect::<Vec<_>>();
        assert_eq!(texts(push_prompt_history(&base, &p("   "))), ["one"]);
        assert_eq!(texts(push_prompt_history(&base, &p("one"))), ["one"]);
        assert_eq!(texts(push_prompt_history(&base, &p("two"))), ["one", "two"]);
    }

    #[test]
    fn moves_through_history() {
        let (wide, draft) = ("中文👨‍👩‍👧\n\t", "草稿👨‍👩‍👧\u{c}");
        let base = create_prompt_history(&[p("one"), p(wide)]);
        let m = move_prompt_history(&base, -1, draft, 1);
        assert!(!m.apply);
        assert_eq!(m.state, base);
        let up = move_prompt_history(&base, -1, draft, 0);
        assert!(up.apply);
        assert_eq!((up.text.as_deref(), up.cursor, up.state.index, up.state.draft.as_str()), (Some(wide), Some(0), Some(1), draft));
        let older = move_prompt_history(&up.state, -1, wide, 0);
        assert_eq!((older.apply, older.text.as_deref(), older.cursor, older.state.index), (true, Some("one"), Some(0), Some(0)));
        let stay = move_prompt_history(&older.state, 1, "one", 2);
        assert!(!stay.apply);
        let newer = move_prompt_history(&older.state, 1, "one", measure_edit_text("one"));
        assert_eq!((newer.apply, newer.text.as_deref(), newer.cursor, newer.state.index), (true, Some(wide), Some(measure_edit_text(wide)), Some(1)));
        let back = move_prompt_history(&newer.state, 1, wide, measure_edit_text(wide));
        assert_eq!((back.apply, back.text.as_deref(), back.cursor, back.state.index), (true, Some(draft), Some(measure_edit_text(draft)), None));
    }

    #[test]
    fn commands() {
        assert!(is_exit_command("/exit"));
        assert!(is_exit_command(" /Quit "));
        assert!(!is_exit_command("/quit now"));
        assert!(is_new_command("/new"));
        assert!(is_new_command(" /NEW "));
        assert!(!is_new_command("/new now"));
        assert!(is_compact_command("/compact"));
        assert!(is_compact_command(" /COMPACT "));
        assert!(!is_compact_command("/summarize"));
    }

    #[test]
    fn append_shifts_ranges() {
        let first = RunPrompt { parts: vec![PromptPart::Agent { name: "plan".into(), source: Some(MentionRange { start: 0, end: 4, value: "日本".into() }) }], ..p("日本👨‍👩‍👧\t") };
        let next = RunPrompt {
            message_id: Some("m-1".into()),
            parts: vec![
                PromptPart::File { url: "file:///a.ts".into(), filename: None, mime: None, description: None, source: Some(("".into(), MentionRange { start: 0, end: 5, value: "@a.ts".into() })) },
                PromptPart::Skill { id: "x".into(), source: Some(MentionRange { start: 6, end: 8, value: "/x".into() }) },
            ],
            ..p("@a.ts /x")
        };
        let out = prompt_append(&first, &next);
        let offset = measure_edit_text("日本👨‍👩‍👧\t\n\n");
        assert_eq!(out.text, "日本👨‍👩‍👧\t\n\n@a.ts /x");
        assert_eq!(
            out.parts,
            vec![
                PromptPart::Agent { name: "plan".into(), source: Some(MentionRange { start: 0, end: 4, value: "日本".into() }) },
                PromptPart::File { url: "file:///a.ts".into(), filename: None, mime: None, description: None, source: Some(("".into(), MentionRange { start: offset, end: offset + 5, value: "@a.ts".into() })) },
                PromptPart::Skill { id: "x".into(), source: Some(MentionRange { start: offset + 6, end: offset + 8, value: "/x".into() }) },
            ]
        );
        assert_eq!(prompt_append(&p(""), &p("next")), p("next"));
    }

    #[test]
    fn editor_slash_value() {
        assert_eq!(resolve_editor_slash_value("/editor"), "");
        assert_eq!(resolve_editor_slash_value("/editor draft message"), "draft message");
        assert_eq!(resolve_editor_slash_value("/editor first line\nsecond line"), "first line\nsecond line");
    }

    fn file_part() -> PromptPart {
        PromptPart::File {
            url: "file:///src/app.ts".into(),
            filename: Some("src/app.ts".into()),
            mime: Some("text/plain".into()),
            description: None,
            source: Some(("src/app.ts".into(), MentionRange { start: 0, end: 11, value: "@src/app.ts".into() })),
        }
    }

    fn agent_part() -> PromptPart {
        PromptPart::Agent { name: "helper".into(), source: Some(MentionRange { start: 12, end: 19, value: "@helper".into() }) }
    }

    #[test]
    fn realigns_parts_after_editing() {
        let out = realign_editor_prompt_parts("Please check @helper before @src/app.ts", &[file_part(), agent_part()]);
        assert_eq!(
            out,
            vec![
                PromptPart::File {
                    url: "file:///src/app.ts".into(),
                    filename: Some("src/app.ts".into()),
                    mime: Some("text/plain".into()),
                    description: None,
                    source: Some(("src/app.ts".into(), MentionRange { start: 28, end: 39, value: "@src/app.ts".into() })),
                },
                PromptPart::Agent { name: "helper".into(), source: Some(MentionRange { start: 13, end: 20, value: "@helper".into() }) },
            ]
        );
        let out = realign_editor_prompt_parts("Only @helper remains", &[file_part(), agent_part()]);
        assert_eq!(out, vec![PromptPart::Agent { name: "helper".into(), source: Some(MentionRange { start: 5, end: 12, value: "@helper".into() }) }]);
    }

    #[test]
    fn realign_uses_editor_offsets() {
        let part = PromptPart::Agent { name: "助手".into(), source: Some(MentionRange { start: 0, end: 5, value: "@助手".into() }) };
        let start = measure_edit_text("\t\u{c}中文👨‍👩‍👧\r\n");
        let out = realign_editor_prompt_parts("\t\u{c}中文👨‍👩‍👧\r\n@助手", &[part]);
        assert_eq!(out, vec![PromptPart::Agent { name: "助手".into(), source: Some(MentionRange { start, end: start + 5, value: "@助手".into() }) }]);
    }
}

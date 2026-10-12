//! Golden tests generated from the upstream TypeScript implementation, plus
//! ports of the large-input tests in `core/test/tool-webfetch.test.ts` and
//! `core/test/tool-html-markdown-budget.test.ts`.

use super::{MAX_MARKDOWN_BYTES, convert_html_to_markdown, extract_text_from_html};

#[path = "golden.rs"]
mod golden;

const MAX_RESPONSE_BYTES: usize = 5 * 1024 * 1024;
const BUDGET: usize = MAX_MARKDOWN_BYTES as usize - 64 * 1024;

#[test]
fn golden_cases_match_upstream() {
    let mut failures = Vec::new();
    for (name, kind, input, expected) in golden::CASES {
        let actual = if *kind == "md" { convert_html_to_markdown(input) } else { extract_text_from_html(input) };
        if actual != *expected {
            failures.push(format!("{name}:\n  expected {expected:?}\n  actual   {actual:?}"));
        }
    }
    assert!(failures.is_empty(), "{} golden mismatches:\n{}", failures.len(), failures.join("\n"));
}

#[test]
fn golden_case_count() {
    assert!(golden::CASES.len() >= 40);
}

#[test]
fn spec_example() {
    let html = "<html><head><title>T</title></head><body><h1>Hello <em>World</em></h1><p>Para with <a href=\"http://x.com/a b\" title=\"t\">link</a> and <code>x`y</code>.</p><ul><li>one</li><li>two<ol start=\"3\"><li>three</li></ol></li></ul><pre><code class=\"language-js\">let a = 1;\n</code></pre><table><tr><th>A</th><th>B</th></tr><tr><td>1</td><td>2|3</td></tr></table><blockquote><p>q1</p><p>q2</p></blockquote><img src=\"i.png\" alt=\"alt]\"><hr><dl><dt>T</dt><dd>D</dd></dl><script>bad()</script><p>1. not list # x *y*</p></body></html>";
    assert_eq!(
        convert_html_to_markdown(html),
        "# Hello *World*\n\nPara with [link](http://x.com/a%20b \"t\") and ``x`y``.\n\n- one\n\n- two\n\n  3. three\n\n```js\nlet a = 1;\n```\n\n| A | B |\n| --- | --- |\n| 1 | 2\\|3 |\n\n> q1\n\n> q2\n\n![alt\\]](i.png)\n\n---\n\n**T**\n: D\n\n1\\. not list \\# x \\*y\\*"
    );
}

#[test]
fn fits_inline_code_to_its_emitted_boundaries() {
    let cases: &[(&str, &str, usize, &str)] = &[
        ("discarded trailing backtick after ASCII", "x`", 7, "``x``"),
        ("discarded trailing backtick after Unicode", "😀`", 10, "``😀``"),
        ("discarded trailing backtick with spare room", "x`", 9, "``x``"),
        ("retained trailing backtick", "x`", 10, "`` x` ``"),
        ("new trailing backtick from an internal run", "x`y", 8, "``x``"),
        ("leading backtick without padding room", "`x", 8, ""),
        ("leading backtick alone fits", "`x", 9, "`` ` ``"),
        ("leading backtick with payload fits", "`x", 10, "`` `x ``"),
        ("all backticks truncated", "``", 11, "``` ` ```"),
        ("all backticks fit", "``", 12, "``` `` ```"),
        ("mixed internal runs truncated", "a``b`c", 11, "```a```"),
        ("Unicode code point cannot fit", "😀`", 9, ""),
        ("ordinary payload cannot fit", "x", 3, ""),
        ("spaces cannot fit", "   ", 4, ""),
        ("space-only prefix fits", "   ", 5, "` `"),
        ("truncated prefix becomes space-only", " x", 6, "` `"),
        ("discarded trailing space", "x ", 5, "`x`"),
    ];
    for (name, content, spare, expected) in cases {
        let prefix = "x".repeat(MAX_RESPONSE_BYTES - 64 * 1024 - spare);
        let html = format!("<p>{prefix}<code>{content}</code></p>");
        assert!(html.len() <= MAX_RESPONSE_BYTES);
        let output = convert_html_to_markdown(&html);
        assert_eq!(&output[..prefix.len()], prefix, "{name}");
        assert_eq!(&output[prefix.len()..], *expected, "{name}");
        assert!(output.len() <= MAX_RESPONSE_BYTES, "{name}");
    }
}

#[test]
fn finishes_bounded_code_conversion() {
    let cases: &[(&str, usize, &str, &str, bool)] = &[
        ("exhausted budget", BUDGET, "x", "", false),
        ("one byte short of an empty fence", BUDGET - 13, "x", "", false),
        ("small fitting block", 64, "x", "\n\n```\nx\n```", false),
        ("last payload byte fits", BUDGET - 15, "x", "\n\n```\nx\n```", false),
        ("only an empty fence fits", BUDGET - 14, "x", "\n\n```\n\n```", false),
        ("Unicode payload truncates at a code point", BUDGET - 18, "😀é", "\n\n```\n😀\n```", false),
        ("quoted payload fits", BUDGET - 23, "x", "\n\n> ```\n> x\n> ```", true),
        ("only an empty quoted fence fits", BUDGET - 22, "x", "\n\n> ```\n> \n> ```", true),
        ("one byte short of an empty quoted fence", BUDGET - 21, "x", "", true),
    ];
    for (name, count, payload, suffix, quoted) in cases {
        let code = format!("<pre>{payload}</pre>");
        let body = if *quoted { format!("<blockquote>{code}</blockquote>") } else { code };
        let html = format!("<p>{}</p>{body}", "x".repeat(*count));
        assert!(html.len() <= MAX_MARKDOWN_BYTES as usize);
        let output = convert_html_to_markdown(&html);
        assert!(output.len() <= MAX_MARKDOWN_BYTES as usize, "{name}");
        let expected = format!("{}{suffix}", "x".repeat((*count).min(BUDGET - 2)));
        assert!(output == expected, "{name}: tail {:?}", &output[output.len().saturating_sub(40)..]);
    }
}

#[test]
fn deterministic_and_bounded_for_malformed_input_across_chunks() {
    let html = format!("<main><p>{}</main></p></unknown>", "visible &amp; text ".repeat(4_096));
    let first = convert_html_to_markdown(&html);
    assert_eq!(convert_html_to_markdown(&html), first);
    assert!(first.starts_with("visible & text visible & text"));
    assert!(first.encode_utf16().count() <= html.encode_utf16().count());
}

#[test]
fn defaults_to_production_byte_budget() {
    let output = convert_html_to_markdown(&"x".repeat(MAX_RESPONSE_BYTES));
    assert_eq!(output.len(), MAX_RESPONSE_BYTES - 64 * 1024);
}

#[test]
fn preserves_utf8_boundaries_at_content_limit() {
    for character in ["x", "\u{e9}", "\u{1f600}"] {
        let fitting = format!("aa{}", character.repeat((BUDGET - 2) / character.len()));
        assert!(convert_html_to_markdown(&fitting) == fitting, "fitting {character}");
        let truncated = convert_html_to_markdown(&format!("{fitting}{character}"));
        assert!(truncated == fitting, "truncated {character}");
    }
}

#[test]
fn bounds_deeply_nested_lists_quotes_and_fragmented_fences() {
    let lists = format!("{}{}", "<ul><li>item".repeat(2_000), "</li></ul>".repeat(2_000));
    let quotes = format!("{}{}", "<blockquote><p>item".repeat(2_000), "</p></blockquote>".repeat(2_000));
    let code = format!("<pre>{}</pre>", "` x ".repeat(4_096));
    let utf16 = |s: &str| s.encode_utf16().count();
    assert!(utf16(&convert_html_to_markdown(&lists)) < utf16(&lists) * 4);
    assert!(utf16(&convert_html_to_markdown(&quotes)) < utf16(&quotes) * 4);
    let _ = convert_html_to_markdown(&code);
    assert_eq!(
        convert_html_to_markdown(&format!(
            "{}safe<script><b>bad</b>&amp;</script><p>tail &amp;</p>",
            "<div>".repeat(20_000)
        )),
        "safe tail &"
    );
}

#[test]
fn deep_nesting_fallback_inside_suppressed_subtree() {
    let html = format!("<div hidden>{}hidden text</div>visible", "<span>".repeat(10_050));
    assert_eq!(convert_html_to_markdown(&html), "visible");
}

#[test]
fn extract_text_upstream_case() {
    let html = "<h1>Hello</h1><script>bad()</script><p>world <strong>wide</strong> <product-name>today</product-name></p><style>.bad {}</style>";
    assert_eq!(extract_text_from_html(html), "Helloworld wide today");
}

#[test]
fn chunk_boundary_surrogate_pairs_rejoin() {
    // A surrogate pair straddling the 64Ki UTF-16 unit chunk boundary.
    let html = format!("{}😀 tail", "y".repeat(64 * 1024 - 1));
    let output = convert_html_to_markdown(&html);
    assert!(output.ends_with("😀 tail"));
}

//! Parity tests for `String`, `String.prototype`, and `RegExp`: each program's result JSON must equal what upstream
//! `@opencode/codemode` (on Bun/JavaScriptCore) printed for it. Cases are ported from the upstream suites
//! (`string-core-test262`, `string-search-test262`, `string-regexp-test262`, `string-date-methods-test262`,
//! `regexp-math-test262`, `stdlib`, `parity`, `callbacks`, `errors`, `limits`), plus edge cases checked against
//! upstream: coercion order, invalid-pattern reasons, `lastIndex`/sticky/global semantics, named groups and `d`
//! indices, substitution patterns, `RegExp.escape`, lone surrogates, and `localeCompare` ordering.

use std::sync::Arc;

use oc_codemode::codemode::{CodeMode, ExecutionLimits};
use oc_codemode::host::NoTools;

fn normalize(json: &str) -> String {
    let value: serde_json::Value = serde_json::from_str(json).expect("expected JSON");
    serde_json::to_string(&value).unwrap()
}

/// Runs every `(origin, program, expected result JSON)` case and reports all mismatches at once.
fn check(cases: &[(&str, &str, &str)]) {
    let runtime = CodeMode::new(Vec::new(), Arc::new(NoTools), Vec::new(), ExecutionLimits::default()).unwrap();
    let mut failures = Vec::new();
    for (origin, code, expected) in cases {
        let result = runtime.execute_blocking(code);
        let actual = normalize(&serde_json::to_string(&result.to_json()).unwrap());
        let expected = normalize(expected);
        if actual != expected {
            failures.push(format!("[{origin}]\n  program: {code}\n  expected: {expected}\n  actual:   {actual}"));
        }
    }
    assert!(failures.is_empty(), "{} of {} cases differ:\n\n{}", failures.len(), cases.len(), failures.join("\n\n"));
}
#[test]
fn string_core_test262() {
    check(&[
        (
            "string-core-test262.test: test/built-ins/String/prototype/toLowerCase/S15.5.4.16_A2_T1.js",
            r##"return [{ label: "#1 direct value", value: "Hello, WoRlD!".toLowerCase() },{ label: "#2 String value", value: "Hello, WoRlD!".toLowerCase() }]"##,
            r##"{"ok":true,"value":[{"label":"#1 direct value","value":"hello, world!"},{"label":"#2 String value","value":"hello, world!"}],"toolCalls":[]}"##,
        ),
        (
            "string-core-test262.test: test/built-ins/String/prototype/toLowerCase/special_casing.js",
            r#"return [{ label: "103 SpecialCasing mappings", value: "ßİﬀﬁﬂﬃﬄﬅﬆևﬓﬔﬕﬖﬗŉΐΰǰẖẗẘẙẚὐὒὔὖᾶῆῒΐῖῗῢΰῤῦῧῶᾀᾁᾂᾃᾄᾅᾆᾇᾈᾉᾊᾋᾌᾍᾎᾏᾐᾑᾒᾓᾔᾕᾖᾗᾘᾙᾚᾛᾜᾝᾞᾟᾠᾡᾢᾣᾤᾥᾦᾧᾨᾩᾪᾫᾬᾭᾮᾯᾳᾼῃῌῳῼᾲᾴῂῄῲῴᾷῇῷ".toLowerCase() }]"#,
            r#"{"ok":true,"value":[{"label":"103 SpecialCasing mappings","value":"ßi̇ﬀﬁﬂﬃﬄﬅﬆևﬓﬔﬕﬖﬗŉΐΰǰẖẗẘẙẚὐὒὔὖᾶῆῒΐῖῗῢΰῤῦῧῶᾀᾁᾂᾃᾄᾅᾆᾇᾀᾁᾂᾃᾄᾅᾆᾇᾐᾑᾒᾓᾔᾕᾖᾗᾐᾑᾒᾓᾔᾕᾖᾗᾠᾡᾢᾣᾤᾥᾦᾧᾠᾡᾢᾣᾤᾥᾦᾧᾳᾳῃῃῳῳᾲᾴῂῄῲῴᾷῇῷ"}],"toolCalls":[]}"#,
        ),
        (
            "string-core-test262.test: test/built-ins/String/prototype/toLowerCase/special_casing_conditional.js",
            r#"return [{ label: "single sigma", value: "Σ".toLowerCase() },{ label: "preceded by cased", value: "AΣ".toLowerCase() },{ label: "preceded by supplementary cased", value: "𝒢Σ".toLowerCase() },{ label: "preceded by full stop", value: "A.Σ".toLowerCase() },{ label: "preceded by soft hyphen", value: "A­Σ".toLowerCase() },{ label: "preceded by combining mark", value: "A𝉂Σ".toLowerCase() },{ label: "preceded by uncased combining mark", value: "ͅΣ".toLowerCase() },{ label: "preceded by cased and combining mark", value: "ᾼΣ".toLowerCase() },{ label: "followed by cased", value: "AΣB".toLowerCase() },{ label: "followed by supplementary cased", value: "AΣ𝒢".toLowerCase() },{ label: "followed by full stop and cased", value: "AΣ.b".toLowerCase() },{ label: "followed by soft hyphen and cased", value: "AΣ­B".toLowerCase() },{ label: "followed by combining mark and cased", value: "AΣ𝉂B".toLowerCase() },{ label: "followed by uncased combining mark", value: "AΣͅ".toLowerCase() },{ label: "followed by combining mark and cased Greek", value: "AΣͅΑ".toLowerCase() }]"#,
            r#"{"ok":true,"value":[{"label":"single sigma","value":"σ"},{"label":"preceded by cased","value":"aς"},{"label":"preceded by supplementary cased","value":"𝒢ς"},{"label":"preceded by full stop","value":"a.ς"},{"label":"preceded by soft hyphen","value":"a­ς"},{"label":"preceded by combining mark","value":"a𝉂ς"},{"label":"preceded by uncased combining mark","value":"ͅσ"},{"label":"preceded by cased and combining mark","value":"ᾳς"},{"label":"followed by cased","value":"aσb"},{"label":"followed by supplementary cased","value":"aσ𝒢"},{"label":"followed by full stop and cased","value":"aσ.b"},{"label":"followed by soft hyphen and cased","value":"aσ­b"},{"label":"followed by combining mark and cased","value":"aσ𝉂b"},{"label":"followed by uncased combining mark","value":"aςͅ"},{"label":"followed by combining mark and cased Greek","value":"aσͅα"}],"toolCalls":[]}"#,
        ),
        (
            "string-core-test262.test: test/built-ins/String/prototype/toLowerCase/Final_Sigma_U180E.js",
            r#"return [{ label: "preceded by U+180E", value: "A᠎Σ".toLowerCase() },{ label: "preceded by U+180E and followed by cased", value: "A᠎ΣB".toLowerCase() },{ label: "followed by U+180E", value: "AΣ᠎".toLowerCase() },{ label: "followed by U+180E and cased", value: "AΣ᠎B".toLowerCase() },{ label: "surrounded by U+180E", value: "A᠎Σ᠎".toLowerCase() },{ label: "surrounded by U+180E and followed by cased", value: "A᠎Σ᠎B".toLowerCase() }]"#,
            r#"{"ok":true,"value":[{"label":"preceded by U+180E","value":"a᠎ς"},{"label":"preceded by U+180E and followed by cased","value":"a᠎σb"},{"label":"followed by U+180E","value":"aς᠎"},{"label":"followed by U+180E and cased","value":"aσ᠎b"},{"label":"surrounded by U+180E","value":"a᠎ς᠎"},{"label":"surrounded by U+180E and followed by cased","value":"a᠎σ᠎b"}],"toolCalls":[]}"#,
        ),
        (
            "string-core-test262.test: test/built-ins/String/prototype/toLowerCase/supplementary_plane.js",
            r#"return [{ label: "40 Deseret mappings", value: "𐐀𐐁𐐂𐐃𐐄𐐅𐐆𐐇𐐈𐐉𐐊𐐋𐐌𐐍𐐎𐐏𐐐𐐑𐐒𐐓𐐔𐐕𐐖𐐗𐐘𐐙𐐚𐐛𐐜𐐝𐐞𐐟𐐠𐐡𐐢𐐣𐐤𐐥𐐦𐐧".toLowerCase() }]"#,
            r#"{"ok":true,"value":[{"label":"40 Deseret mappings","value":"𐐨𐐩𐐪𐐫𐐬𐐭𐐮𐐯𐐰𐐱𐐲𐐳𐐴𐐵𐐶𐐷𐐸𐐹𐐺𐐻𐐼𐐽𐐾𐐿𐑀𐑁𐑂𐑃𐑄𐑅𐑆𐑇𐑈𐑉𐑊𐑋𐑌𐑍𐑎𐑏"}],"toolCalls":[]}"#,
        ),
        (
            "string-core-test262.test: test/built-ins/String/prototype/toUpperCase/S15.5.4.18_A2_T1.js",
            r##"return [{ label: "#1 direct value", value: "Hello, WoRlD!".toUpperCase() },{ label: "#2 String value", value: "Hello, WoRlD!".toUpperCase() }]"##,
            r##"{"ok":true,"value":[{"label":"#1 direct value","value":"HELLO, WORLD!"},{"label":"#2 String value","value":"HELLO, WORLD!"}],"toolCalls":[]}"##,
        ),
        (
            "string-core-test262.test: test/built-ins/String/prototype/toUpperCase/special_casing.js",
            r#"return [{ label: "103 SpecialCasing mappings", value: "ßİﬀﬁﬂﬃﬄﬅﬆևﬓﬔﬕﬖﬗŉΐΰǰẖẗẘẙẚὐὒὔὖᾶῆῒΐῖῗῢΰῤῦῧῶᾀᾁᾂᾃᾄᾅᾆᾇᾈᾉᾊᾋᾌᾍᾎᾏᾐᾑᾒᾓᾔᾕᾖᾗᾘᾙᾚᾛᾜᾝᾞᾟᾠᾡᾢᾣᾤᾥᾦᾧᾨᾩᾪᾫᾬᾭᾮᾯᾳᾼῃῌῳῼᾲᾴῂῄῲῴᾷῇῷ".toUpperCase() }]"#,
            r#"{"ok":true,"value":[{"label":"103 SpecialCasing mappings","value":"SSİFFFIFLFFIFFLSTSTԵՒՄՆՄԵՄԻՎՆՄԽʼNΪ́Ϋ́J̌H̱T̈W̊Y̊AʾΥ̓Υ̓̀Υ̓́Υ̓͂Α͂Η͂Ϊ̀Ϊ́Ι͂Ϊ͂Ϋ̀Ϋ́Ρ̓Υ͂Ϋ͂Ω͂ἈΙἉΙἊΙἋΙἌΙἍΙἎΙἏΙἈΙἉΙἊΙἋΙἌΙἍΙἎΙἏΙἨΙἩΙἪΙἫΙἬΙἭΙἮΙἯΙἨΙἩΙἪΙἫΙἬΙἭΙἮΙἯΙὨΙὩΙὪΙὫΙὬΙὭΙὮΙὯΙὨΙὩΙὪΙὫΙὬΙὭΙὮΙὯΙΑΙΑΙΗΙΗΙΩΙΩΙᾺΙΆΙῊΙΉΙῺΙΏΙΑ͂ΙΗ͂ΙΩ͂Ι"}],"toolCalls":[]}"#,
        ),
        (
            "string-core-test262.test: test/built-ins/String/prototype/toUpperCase/supplementary_plane.js",
            r#"return [{ label: "40 Deseret mappings", value: "𐐨𐐩𐐪𐐫𐐬𐐭𐐮𐐯𐐰𐐱𐐲𐐳𐐴𐐵𐐶𐐷𐐸𐐹𐐺𐐻𐐼𐐽𐐾𐐿𐑀𐑁𐑂𐑃𐑄𐑅𐑆𐑇𐑈𐑉𐑊𐑋𐑌𐑍𐑎𐑏".toUpperCase() }]"#,
            r#"{"ok":true,"value":[{"label":"40 Deseret mappings","value":"𐐀𐐁𐐂𐐃𐐄𐐅𐐆𐐇𐐈𐐉𐐊𐐋𐐌𐐍𐐎𐐏𐐐𐐑𐐒𐐓𐐔𐐕𐐖𐐗𐐘𐐙𐐚𐐛𐐜𐐝𐐞𐐟𐐠𐐡𐐢𐐣𐐤𐐥𐐦𐐧"}],"toolCalls":[]}"#,
        ),
        (
            "string-core-test262.test: test/built-ins/String/prototype/trim/15.5.4.20-3-1.js",
            r#"return [{ label: "upstream assertion", value: "\n\r  ".trim() }]"#,
            r#"{"ok":true,"value":[{"label":"upstream assertion","value":""}],"toolCalls":[]}"#,
        ),
        (
            "string-core-test262.test: test/built-ins/String/prototype/trim/15.5.4.20-3-2.js",
            r#"return [{ label: "upstream assertion", value: "\t\n\u000b\f\r                  　﻿".trim() }]"#,
            r#"{"ok":true,"value":[{"label":"upstream assertion","value":""}],"toolCalls":[]}"#,
        ),
        (
            "string-core-test262.test: test/built-ins/String/prototype/trim/15.5.4.20-3-3.js",
            r#"return [{ label: "upstream assertion", value: "\t\n\u000b\f\r                  　﻿\n\r  ".trim() }]"#,
            r#"{"ok":true,"value":[{"label":"upstream assertion","value":""}],"toolCalls":[]}"#,
        ),
        (
            "string-core-test262.test: test/built-ins/String/prototype/trim/15.5.4.20-3-4.js",
            r#"return [{ label: "upstream assertion", value: "\t\n\u000b\f\r                  　﻿\n\r  abc".trim() }]"#,
            r#"{"ok":true,"value":[{"label":"upstream assertion","value":"abc"}],"toolCalls":[]}"#,
        ),
        (
            "string-core-test262.test: test/built-ins/String/prototype/trim/15.5.4.20-3-5.js",
            r#"return [{ label: "upstream assertion", value: "abc\t\n\u000b\f\r                  　﻿\n\r  ".trim() }]"#,
            r#"{"ok":true,"value":[{"label":"upstream assertion","value":"abc"}],"toolCalls":[]}"#,
        ),
        (
            "string-core-test262.test: test/built-ins/String/prototype/trim/15.5.4.20-3-6.js",
            r#"return [{ label: "upstream assertion", value: "\t\n\u000b\f\r                  　﻿\n\r  abc\t\n\u000b\f\r                  　﻿\n\r  ".trim() }]"#,
            r#"{"ok":true,"value":[{"label":"upstream assertion","value":"abc"}],"toolCalls":[]}"#,
        ),
        (
            "string-core-test262.test: test/built-ins/String/prototype/trim/15.5.4.20-3-7.js",
            r#"return [{ label: "upstream assertion", value: "ab\t\n\u000b\f\r                  　﻿\n\r  cd".trim() }]"#,
            r#"{"ok":true,"value":[{"label":"upstream assertion","value":"ab\t\n\u000b\f\r                  　﻿\n\r  cd"}],"toolCalls":[]}"#,
        ),
        (
            "string-core-test262.test: test/built-ins/String/prototype/trim/15.5.4.20-3-8.js",
            r#"return [{ label: "upstream assertion", value: "\u0000\u0000".trim() }]"#,
            r#"{"ok":true,"value":[{"label":"upstream assertion","value":"\u0000\u0000"}],"toolCalls":[]}"#,
        ),
        (
            "string-core-test262.test: test/built-ins/String/prototype/trim/15.5.4.20-3-9.js",
            r#"return [{ label: "upstream assertion", value: "\u0000".trim() }]"#,
            r#"{"ok":true,"value":[{"label":"upstream assertion","value":"\u0000"}],"toolCalls":[]}"#,
        ),
        (
            "string-core-test262.test: test/built-ins/String/prototype/trim/15.5.4.20-3-11.js",
            r#"return [{ label: "upstream assertion", value: "\u0000\u0000abc".trim() }]"#,
            r#"{"ok":true,"value":[{"label":"upstream assertion","value":"\u0000\u0000abc"}],"toolCalls":[]}"#,
        ),
        (
            "string-core-test262.test: test/built-ins/String/prototype/trim/15.5.4.20-3-12.js",
            r#"return [{ label: "upstream assertion", value: "abc\u0000\u0000".trim() }]"#,
            r#"{"ok":true,"value":[{"label":"upstream assertion","value":"abc\u0000\u0000"}],"toolCalls":[]}"#,
        ),
        (
            "string-core-test262.test: test/built-ins/String/prototype/trim/15.5.4.20-3-13.js",
            r#"return [{ label: "upstream assertion", value: "\u0000\u0000abc\u0000\u0000".trim() }]"#,
            r#"{"ok":true,"value":[{"label":"upstream assertion","value":"\u0000\u0000abc\u0000\u0000"}],"toolCalls":[]}"#,
        ),
        (
            "string-core-test262.test: test/built-ins/String/prototype/trim/15.5.4.20-3-14.js",
            r#"return [{ label: "upstream assertion", value: "a\u0000\u0000bc".trim() }]"#,
            r#"{"ok":true,"value":[{"label":"upstream assertion","value":"a\u0000\u0000bc"}],"toolCalls":[]}"#,
        ),
        (
            "string-core-test262.test: test/built-ins/String/prototype/trim/15.5.4.20-4-1.js",
            r#"return [{ label: "upstream assertion", value: "\ta bc \t".trim() }]"#,
            r#"{"ok":true,"value":[{"label":"upstream assertion","value":"a bc"}],"toolCalls":[]}"#,
        ),
        (
            "string-core-test262.test: test/built-ins/String/prototype/trim/15.5.4.20-4-2.js",
            r#"return [{ label: "upstream assertion", value: " \tabc \t".trim() }]"#,
            r#"{"ok":true,"value":[{"label":"upstream assertion","value":"abc"}],"toolCalls":[]}"#,
        ),
        (
            "string-core-test262.test: test/built-ins/String/prototype/trim/15.5.4.20-4-3.js",
            r#"return [{ label: "upstream assertion", value: "\tabc".trim() }]"#,
            r#"{"ok":true,"value":[{"label":"upstream assertion","value":"abc"}],"toolCalls":[]}"#,
        ),
        (
            "string-core-test262.test: test/built-ins/String/prototype/trim/15.5.4.20-4-4.js",
            r#"return [{ label: "upstream assertion", value: "\u000babc".trim() }]"#,
            r#"{"ok":true,"value":[{"label":"upstream assertion","value":"abc"}],"toolCalls":[]}"#,
        ),
        (
            "string-core-test262.test: test/built-ins/String/prototype/trim/15.5.4.20-4-5.js",
            r#"return [{ label: "upstream assertion", value: "\fabc".trim() }]"#,
            r#"{"ok":true,"value":[{"label":"upstream assertion","value":"abc"}],"toolCalls":[]}"#,
        ),
        (
            "string-core-test262.test: test/built-ins/String/prototype/trim/15.5.4.20-4-6.js",
            r#"return [{ label: "upstream assertion", value: " abc".trim() }]"#,
            r#"{"ok":true,"value":[{"label":"upstream assertion","value":"abc"}],"toolCalls":[]}"#,
        ),
        (
            "string-core-test262.test: test/built-ins/String/prototype/trim/15.5.4.20-4-8.js",
            r#"return [{ label: "upstream assertion", value: " abc".trim() }]"#,
            r#"{"ok":true,"value":[{"label":"upstream assertion","value":"abc"}],"toolCalls":[]}"#,
        ),
        (
            "string-core-test262.test: test/built-ins/String/prototype/trim/15.5.4.20-4-10.js",
            r#"return [{ label: "upstream assertion", value: "﻿abc".trim() }]"#,
            r#"{"ok":true,"value":[{"label":"upstream assertion","value":"abc"}],"toolCalls":[]}"#,
        ),
        (
            "string-core-test262.test: test/built-ins/String/prototype/trim/15.5.4.20-4-11.js",
            r#"return [{ label: "upstream assertion", value: "abc\t".trim() }]"#,
            r#"{"ok":true,"value":[{"label":"upstream assertion","value":"abc"}],"toolCalls":[]}"#,
        ),
        (
            "string-core-test262.test: test/built-ins/String/prototype/trim/15.5.4.20-4-12.js",
            r#"return [{ label: "upstream assertion", value: "abc\u000b".trim() }]"#,
            r#"{"ok":true,"value":[{"label":"upstream assertion","value":"abc"}],"toolCalls":[]}"#,
        ),
        (
            "string-core-test262.test: test/built-ins/String/prototype/trim/15.5.4.20-4-13.js",
            r#"return [{ label: "upstream assertion", value: "abc\f".trim() }]"#,
            r#"{"ok":true,"value":[{"label":"upstream assertion","value":"abc"}],"toolCalls":[]}"#,
        ),
        (
            "string-core-test262.test: test/built-ins/String/prototype/trim/15.5.4.20-4-14.js",
            r#"return [{ label: "upstream assertion", value: "abc ".trim() }]"#,
            r#"{"ok":true,"value":[{"label":"upstream assertion","value":"abc"}],"toolCalls":[]}"#,
        ),
        (
            "string-core-test262.test: test/built-ins/String/prototype/trim/15.5.4.20-4-16.js",
            r#"return [{ label: "upstream assertion", value: "abc ".trim() }]"#,
            r#"{"ok":true,"value":[{"label":"upstream assertion","value":"abc"}],"toolCalls":[]}"#,
        ),
        (
            "string-core-test262.test: test/built-ins/String/prototype/trim/15.5.4.20-4-18.js",
            r#"return [{ label: "upstream assertion", value: "abc﻿".trim() }]"#,
            r#"{"ok":true,"value":[{"label":"upstream assertion","value":"abc"}],"toolCalls":[]}"#,
        ),
        (
            "string-core-test262.test: test/built-ins/String/prototype/trim/15.5.4.20-4-19.js",
            r#"return [{ label: "upstream assertion", value: "\tabc\t".trim() }]"#,
            r#"{"ok":true,"value":[{"label":"upstream assertion","value":"abc"}],"toolCalls":[]}"#,
        ),
        (
            "string-core-test262.test: test/built-ins/String/prototype/trim/15.5.4.20-4-20.js",
            r#"return [{ label: "upstream assertion", value: "\u000babc\u000b".trim() }]"#,
            r#"{"ok":true,"value":[{"label":"upstream assertion","value":"abc"}],"toolCalls":[]}"#,
        ),
        (
            "string-core-test262.test: test/built-ins/String/prototype/trim/15.5.4.20-4-21.js",
            r#"return [{ label: "upstream assertion", value: "\fabc\f".trim() }]"#,
            r#"{"ok":true,"value":[{"label":"upstream assertion","value":"abc"}],"toolCalls":[]}"#,
        ),
        (
            "string-core-test262.test: test/built-ins/String/prototype/trim/15.5.4.20-4-22.js",
            r#"return [{ label: "upstream assertion", value: " abc ".trim() }]"#,
            r#"{"ok":true,"value":[{"label":"upstream assertion","value":"abc"}],"toolCalls":[]}"#,
        ),
        (
            "string-core-test262.test: test/built-ins/String/prototype/trim/15.5.4.20-4-24.js",
            r#"return [{ label: "upstream assertion", value: " abc ".trim() }]"#,
            r#"{"ok":true,"value":[{"label":"upstream assertion","value":"abc"}],"toolCalls":[]}"#,
        ),
        (
            "string-core-test262.test: test/built-ins/String/prototype/trim/15.5.4.20-4-27.js",
            r#"return [{ label: "upstream assertion", value: "\t\t".trim() }]"#,
            r#"{"ok":true,"value":[{"label":"upstream assertion","value":""}],"toolCalls":[]}"#,
        ),
        (
            "string-core-test262.test: test/built-ins/String/prototype/trim/15.5.4.20-4-28.js",
            r#"return [{ label: "upstream assertion", value: "\u000b\u000b".trim() }]"#,
            r#"{"ok":true,"value":[{"label":"upstream assertion","value":""}],"toolCalls":[]}"#,
        ),
        (
            "string-core-test262.test: test/built-ins/String/prototype/trim/15.5.4.20-4-29.js",
            r#"return [{ label: "upstream assertion", value: "\f\f".trim() }]"#,
            r#"{"ok":true,"value":[{"label":"upstream assertion","value":""}],"toolCalls":[]}"#,
        ),
        (
            "string-core-test262.test: test/built-ins/String/prototype/trim/15.5.4.20-4-30.js",
            r#"return [{ label: "upstream assertion", value: "  ".trim() }]"#,
            r#"{"ok":true,"value":[{"label":"upstream assertion","value":""}],"toolCalls":[]}"#,
        ),
        (
            "string-core-test262.test: test/built-ins/String/prototype/trim/15.5.4.20-4-32.js",
            r#"return [{ label: "upstream assertion", value: "  ".trim() }]"#,
            r#"{"ok":true,"value":[{"label":"upstream assertion","value":""}],"toolCalls":[]}"#,
        ),
        (
            "string-core-test262.test: test/built-ins/String/prototype/trim/15.5.4.20-4-34.js",
            r#"return [{ label: "upstream assertion", value: "﻿﻿".trim() }]"#,
            r#"{"ok":true,"value":[{"label":"upstream assertion","value":""}],"toolCalls":[]}"#,
        ),
        (
            "string-core-test262.test: test/built-ins/String/prototype/trim/15.5.4.20-4-35.js",
            r#"return [{ label: "upstream assertion", value: "ab\tc".trim() }]"#,
            r#"{"ok":true,"value":[{"label":"upstream assertion","value":"ab\tc"}],"toolCalls":[]}"#,
        ),
        (
            "string-core-test262.test: test/built-ins/String/prototype/trim/15.5.4.20-4-36.js",
            r#"return [{ label: "upstream assertion", value: "ab\u000bc".trim() }]"#,
            r#"{"ok":true,"value":[{"label":"upstream assertion","value":"ab\u000bc"}],"toolCalls":[]}"#,
        ),
        (
            "string-core-test262.test: test/built-ins/String/prototype/trim/15.5.4.20-4-37.js",
            r#"return [{ label: "upstream assertion", value: "ab\fc".trim() }]"#,
            r#"{"ok":true,"value":[{"label":"upstream assertion","value":"ab\fc"}],"toolCalls":[]}"#,
        ),
        (
            "string-core-test262.test: test/built-ins/String/prototype/trim/15.5.4.20-4-38.js",
            r#"return [{ label: "upstream assertion", value: "ab c".trim() }]"#,
            r#"{"ok":true,"value":[{"label":"upstream assertion","value":"ab c"}],"toolCalls":[]}"#,
        ),
        (
            "string-core-test262.test: test/built-ins/String/prototype/trim/15.5.4.20-4-39.js",
            r#"return [{ label: "upstream assertion", value: "abc".trim() }]"#,
            r#"{"ok":true,"value":[{"label":"upstream assertion","value":"abc"}],"toolCalls":[]}"#,
        ),
        (
            "string-core-test262.test: test/built-ins/String/prototype/trim/15.5.4.20-4-40.js",
            r#"return [{ label: "upstream assertion", value: "ab c".trim() }]"#,
            r#"{"ok":true,"value":[{"label":"upstream assertion","value":"ab c"}],"toolCalls":[]}"#,
        ),
        (
            "string-core-test262.test: test/built-ins/String/prototype/trim/15.5.4.20-4-41.js",
            r#"return [{ label: "upstream assertion", value: "ab​c".trim() }]"#,
            r#"{"ok":true,"value":[{"label":"upstream assertion","value":"ab​c"}],"toolCalls":[]}"#,
        ),
        (
            "string-core-test262.test: test/built-ins/String/prototype/trim/15.5.4.20-4-42.js",
            r#"return [{ label: "upstream assertion", value: "ab﻿c".trim() }]"#,
            r#"{"ok":true,"value":[{"label":"upstream assertion","value":"ab﻿c"}],"toolCalls":[]}"#,
        ),
        (
            "string-core-test262.test: test/built-ins/String/prototype/trim/15.5.4.20-4-43.js",
            r#"return [{ label: "upstream assertion", value: "\nabc".trim() }]"#,
            r#"{"ok":true,"value":[{"label":"upstream assertion","value":"abc"}],"toolCalls":[]}"#,
        ),
        (
            "string-core-test262.test: test/built-ins/String/prototype/trim/15.5.4.20-4-44.js",
            r#"return [{ label: "upstream assertion", value: "\rabc".trim() }]"#,
            r#"{"ok":true,"value":[{"label":"upstream assertion","value":"abc"}],"toolCalls":[]}"#,
        ),
        (
            "string-core-test262.test: test/built-ins/String/prototype/trim/15.5.4.20-4-45.js",
            r#"return [{ label: "upstream assertion", value: " abc".trim() }]"#,
            r#"{"ok":true,"value":[{"label":"upstream assertion","value":"abc"}],"toolCalls":[]}"#,
        ),
        (
            "string-core-test262.test: test/built-ins/String/prototype/trim/15.5.4.20-4-46.js",
            r#"return [{ label: "upstream assertion", value: " abc".trim() }]"#,
            r#"{"ok":true,"value":[{"label":"upstream assertion","value":"abc"}],"toolCalls":[]}"#,
        ),
        (
            "string-core-test262.test: test/built-ins/String/prototype/trim/15.5.4.20-4-47.js",
            r#"return [{ label: "upstream assertion", value: "abc\n".trim() }]"#,
            r#"{"ok":true,"value":[{"label":"upstream assertion","value":"abc"}],"toolCalls":[]}"#,
        ),
        (
            "string-core-test262.test: test/built-ins/String/prototype/trim/15.5.4.20-4-48.js",
            r#"return [{ label: "upstream assertion", value: "abc\r".trim() }]"#,
            r#"{"ok":true,"value":[{"label":"upstream assertion","value":"abc"}],"toolCalls":[]}"#,
        ),
        (
            "string-core-test262.test: test/built-ins/String/prototype/trim/15.5.4.20-4-49.js",
            r#"return [{ label: "upstream assertion", value: "abc ".trim() }]"#,
            r#"{"ok":true,"value":[{"label":"upstream assertion","value":"abc"}],"toolCalls":[]}"#,
        ),
        (
            "string-core-test262.test: test/built-ins/String/prototype/trim/15.5.4.20-4-50.js",
            r#"return [{ label: "upstream assertion", value: "abc ".trim() }]"#,
            r#"{"ok":true,"value":[{"label":"upstream assertion","value":"abc"}],"toolCalls":[]}"#,
        ),
        (
            "string-core-test262.test: test/built-ins/String/prototype/trim/15.5.4.20-4-51.js",
            r#"return [{ label: "upstream assertion", value: "\nabc\n".trim() }]"#,
            r#"{"ok":true,"value":[{"label":"upstream assertion","value":"abc"}],"toolCalls":[]}"#,
        ),
        (
            "string-core-test262.test: test/built-ins/String/prototype/trim/15.5.4.20-4-52.js",
            r#"return [{ label: "upstream assertion", value: "\rabc\r".trim() }]"#,
            r#"{"ok":true,"value":[{"label":"upstream assertion","value":"abc"}],"toolCalls":[]}"#,
        ),
        (
            "string-core-test262.test: test/built-ins/String/prototype/trim/15.5.4.20-4-53.js",
            r#"return [{ label: "upstream assertion", value: " abc ".trim() }]"#,
            r#"{"ok":true,"value":[{"label":"upstream assertion","value":"abc"}],"toolCalls":[]}"#,
        ),
        (
            "string-core-test262.test: test/built-ins/String/prototype/trim/15.5.4.20-4-54.js",
            r#"return [{ label: "upstream assertion", value: " abc ".trim() }]"#,
            r#"{"ok":true,"value":[{"label":"upstream assertion","value":"abc"}],"toolCalls":[]}"#,
        ),
        (
            "string-core-test262.test: test/built-ins/String/prototype/trim/15.5.4.20-4-55.js",
            r#"return [{ label: "upstream assertion", value: "\n\n".trim() }]"#,
            r#"{"ok":true,"value":[{"label":"upstream assertion","value":""}],"toolCalls":[]}"#,
        ),
        (
            "string-core-test262.test: test/built-ins/String/prototype/trim/15.5.4.20-4-56.js",
            r#"return [{ label: "upstream assertion", value: "\r\r".trim() }]"#,
            r#"{"ok":true,"value":[{"label":"upstream assertion","value":""}],"toolCalls":[]}"#,
        ),
        (
            "string-core-test262.test: test/built-ins/String/prototype/trim/15.5.4.20-4-57.js",
            r#"return [{ label: "upstream assertion", value: "  ".trim() }]"#,
            r#"{"ok":true,"value":[{"label":"upstream assertion","value":""}],"toolCalls":[]}"#,
        ),
        (
            "string-core-test262.test: test/built-ins/String/prototype/trim/15.5.4.20-4-58.js",
            r#"return [{ label: "upstream assertion", value: "  ".trim() }]"#,
            r#"{"ok":true,"value":[{"label":"upstream assertion","value":""}],"toolCalls":[]}"#,
        ),
        (
            "string-core-test262.test: test/built-ins/String/prototype/trim/15.5.4.20-4-59.js",
            r#"return [{ label: "upstream assertion", value: "            abc".trim() }]"#,
            r#"{"ok":true,"value":[{"label":"upstream assertion","value":"abc"}],"toolCalls":[]}"#,
        ),
        (
            "string-core-test262.test: test/built-ins/String/prototype/trim/15.5.4.20-4-60.js",
            r#"return [{ label: "upstream assertion", value: "    ".trim() }]"#,
            r#"{"ok":true,"value":[{"label":"upstream assertion","value":""}],"toolCalls":[]}"#,
        ),
        (
            "string-core-test262.test: test/built-ins/String/prototype/trim/u180e.js",
            r#"return [{ label: "trailing U+180E", value: "_᠎".trim() },{ label: "only U+180E", value: "᠎".trim() },{ label: "leading U+180E", value: "᠎_".trim() }]"#,
            r#"{"ok":true,"value":[{"label":"trailing U+180E","value":"_᠎"},{"label":"only U+180E","value":"᠎"},{"label":"leading U+180E","value":"᠎_"}],"toolCalls":[]}"#,
        ),
        (
            "string-core-test262.test: test/built-ins/String/prototype/trimStart/this-value-whitespace.js",
            r#"return [{ label: "all whitespace", value: "\t\n\u000b\f\r                　  ﻿a\t\n\u000b\f\r                　  ﻿b\t\n\u000b\f\r                　  ﻿".trimStart() }]"#,
            r#"{"ok":true,"value":[{"label":"all whitespace","value":"a\t\n\u000b\f\r                　  ﻿b\t\n\u000b\f\r                　  ﻿"}],"toolCalls":[]}"#,
        ),
        (
            "string-core-test262.test: test/built-ins/String/prototype/trimStart/this-value-line-terminator.js",
            r#"return [{ label: "all line terminators", value: "\n\r  a\n\r  b\n\r  ".trimStart() }]"#,
            r#"{"ok":true,"value":[{"label":"all line terminators","value":"a\n\r  b\n\r  "}],"toolCalls":[]}"#,
        ),
        (
            "string-core-test262.test: test/built-ins/String/prototype/trimEnd/this-value-whitespace.js",
            r#"return [{ label: "all whitespace", value: "\t\n\u000b\f\r                　  ﻿a\t\n\u000b\f\r                　  ﻿b\t\n\u000b\f\r                　  ﻿".trimEnd() }]"#,
            r#"{"ok":true,"value":[{"label":"all whitespace","value":"\t\n\u000b\f\r                　  ﻿a\t\n\u000b\f\r                　  ﻿b"}],"toolCalls":[]}"#,
        ),
        (
            "string-core-test262.test: test/built-ins/String/prototype/trimEnd/this-value-line-terminator.js",
            r#"return [{ label: "all line terminators", value: "\n\r  a\n\r  b\n\r  ".trimEnd() }]"#,
            r#"{"ok":true,"value":[{"label":"all line terminators","value":"\n\r  a\n\r  b"}],"toolCalls":[]}"#,
        ),
        (
            "string-core-test262.test: test/built-ins/String/prototype/repeat/repeat-string-n-times.js",
            r#"return [{ label: "repeat once", value: "abc".repeat(1) },{ label: "repeat three times", value: "abc".repeat(3) },{ label: "repeat 10000 times length", value: ".".repeat(10000).length }]"#,
            r#"{"ok":true,"value":[{"label":"repeat once","value":"abc"},{"label":"repeat three times","value":"abcabcabc"},{"label":"repeat 10000 times length","value":10000}],"toolCalls":[]}"#,
        ),
        (
            "string-core-test262.test: test/built-ins/String/prototype/repeat/empty-string-returns-empty.js",
            r#"return [{ label: "count 1", value: "".repeat(1) },{ label: "count 3", value: "".repeat(3) },{ label: "maximum 32-bit count", value: "".repeat(4294967295) }]"#,
            r#"{"ok":true,"value":[{"label":"count 1","value":""},{"label":"count 3","value":""},{"label":"maximum 32-bit count","value":""}],"toolCalls":[]}"#,
        ),
        (
            "string-core-test262.test: test/built-ins/String/prototype/repeat/count-is-zero-returns-empty-string.js",
            r#"return [{ label: "zero", value: "foo".repeat(0) }]"#,
            r#"{"ok":true,"value":[{"label":"zero","value":""}],"toolCalls":[]}"#,
        ),
        (
            "string-core-test262.test: test/built-ins/String/prototype/repeat/count-coerced-to-zero-returns-empty-string.js",
            r#"return [{ label: "fraction truncates to zero", value: "abc".repeat(0.9) }]"#,
            r#"{"ok":true,"value":[{"label":"fraction truncates to zero","value":""}],"toolCalls":[]}"#,
        ),
        (
            "string-core-test262.test: test/built-ins/String/prototype/padStart/fill-string-empty.js",
            r#"return [{ label: "empty fill", value: "abc".padStart(5, "") }]"#,
            r#"{"ok":true,"value":[{"label":"empty fill","value":"abc"}],"toolCalls":[]}"#,
        ),
        (
            "string-core-test262.test: test/built-ins/String/prototype/padStart/fill-string-omitted.js",
            r#"return [{ label: "omitted fill", value: "abc".padStart(5) },{ label: "undefined fill", value: "abc".padStart(5, undefined) }]"#,
            r#"{"ok":true,"value":[{"label":"omitted fill","value":"  abc"},{"label":"undefined fill","value":"  abc"}],"toolCalls":[]}"#,
        ),
        (
            "string-core-test262.test: test/built-ins/String/prototype/padStart/max-length-not-greater-than-string.js",
            r#"return [{ label: "NaN", value: "abc".padStart(NaN, "def") },{ label: "negative infinity", value: "abc".padStart(-Infinity, "def") },{ label: "zero", value: "abc".padStart(0, "def") },{ label: "negative one", value: "abc".padStart(-1, "def") },{ label: "equal length", value: "abc".padStart(3, "def") },{ label: "fraction truncates", value: "abc".padStart(3.9999, "def") }]"#,
            r#"{"ok":true,"value":[{"label":"NaN","value":"abc"},{"label":"negative infinity","value":"abc"},{"label":"zero","value":"abc"},{"label":"negative one","value":"abc"},{"label":"equal length","value":"abc"},{"label":"fraction truncates","value":"abc"}],"toolCalls":[]}"#,
        ),
        (
            "string-core-test262.test: test/built-ins/String/prototype/padEnd/fill-string-empty.js",
            r#"return [{ label: "empty fill", value: "abc".padEnd(5, "") }]"#,
            r#"{"ok":true,"value":[{"label":"empty fill","value":"abc"}],"toolCalls":[]}"#,
        ),
        (
            "string-core-test262.test: test/built-ins/String/prototype/padEnd/fill-string-omitted.js",
            r#"return [{ label: "omitted fill", value: "abc".padEnd(5) },{ label: "undefined fill", value: "abc".padEnd(5, undefined) }]"#,
            r#"{"ok":true,"value":[{"label":"omitted fill","value":"abc  "},{"label":"undefined fill","value":"abc  "}],"toolCalls":[]}"#,
        ),
        (
            "string-core-test262.test: test/built-ins/String/prototype/padEnd/max-length-not-greater-than-string.js",
            r#"return [{ label: "NaN", value: "abc".padEnd(NaN, "def") },{ label: "negative infinity", value: "abc".padEnd(-Infinity, "def") },{ label: "zero", value: "abc".padEnd(0, "def") },{ label: "negative one", value: "abc".padEnd(-1, "def") },{ label: "equal length", value: "abc".padEnd(3, "def") },{ label: "fraction truncates", value: "abc".padEnd(3.9999, "def") }]"#,
            r#"{"ok":true,"value":[{"label":"NaN","value":"abc"},{"label":"negative infinity","value":"abc"},{"label":"zero","value":"abc"},{"label":"negative one","value":"abc"},{"label":"equal length","value":"abc"},{"label":"fraction truncates","value":"abc"}],"toolCalls":[]}"#,
        ),
        (
            "string-core-test262.test: test/built-ins/String/prototype/charAt/S15.5.4.4_A1_T4.js",
            r#"return [{ label: "omitted position", value: "lego".charAt() }]"#,
            r#"{"ok":true,"value":[{"label":"omitted position","value":"l"}],"toolCalls":[]}"#,
        ),
        (
            "string-core-test262.test: test/built-ins/String/prototype/charAt/S15.5.4.4_A1_T7.js",
            r#"return [{ label: "undefined position", value: "lego".charAt(undefined) }]"#,
            r#"{"ok":true,"value":[{"label":"undefined position","value":"l"}],"toolCalls":[]}"#,
        ),
        (
            "string-core-test262.test: test/built-ins/String/prototype/charAt/S15.5.4.4_A1_T8.js",
            r#"return [{ label: "undefined position", value: "42".charAt(undefined) }]"#,
            r#"{"ok":true,"value":[{"label":"undefined position","value":"4"}],"toolCalls":[]}"#,
        ),
        (
            "string-core-test262.test: test/built-ins/String/prototype/charAt/S15.5.4.4_A4_T1.js",
            r#"return [{ label: "position 0", value: "ABCABC".charAt(0) },{ label: "position 1", value: "ABCABC".charAt(1) },{ label: "position 2", value: "ABCABC".charAt(2) },{ label: "position 3", value: "ABCABC".charAt(3) },{ label: "position 4", value: "ABCABC".charAt(4) },{ label: "position 5", value: "ABCABC".charAt(5) }]"#,
            r#"{"ok":true,"value":[{"label":"position 0","value":"A"},{"label":"position 1","value":"B"},{"label":"position 2","value":"C"},{"label":"position 3","value":"A"},{"label":"position 4","value":"B"},{"label":"position 5","value":"C"}],"toolCalls":[]}"#,
        ),
        (
            "string-core-test262.test: test/built-ins/String/prototype/charAt/S15.5.4.4_A4_T2.js",
            r#"return [{ label: "position -2", value: "ABCABC".charAt(-2) },{ label: "position -1", value: "ABCABC".charAt(-1) }]"#,
            r#"{"ok":true,"value":[{"label":"position -2","value":""},{"label":"position -1","value":""}],"toolCalls":[]}"#,
        ),
        (
            "string-core-test262.test: test/built-ins/String/prototype/charAt/S15.5.4.4_A4_T3.js",
            r#"return [{ label: "position 6", value: "ABCABC".charAt(6) },{ label: "position 7", value: "ABCABC".charAt(7) }]"#,
            r#"{"ok":true,"value":[{"label":"position 6","value":""},{"label":"position 7","value":""}],"toolCalls":[]}"#,
        ),
        (
            "string-core-test262.test: test/built-ins/String/prototype/charAt/S9.4_A1.js",
            r#"return [{ label: "NaN position", value: "abc".charAt(NaN) }]"#,
            r#"{"ok":true,"value":[{"label":"NaN position","value":"a"}],"toolCalls":[]}"#,
        ),
        (
            "string-core-test262.test: test/built-ins/String/prototype/charAt/S9.4_A2.js",
            r#"return [{ label: "positive zero", value: "abc".charAt(0) },{ label: "negative zero", value: "abc".charAt(-0) }]"#,
            r#"{"ok":true,"value":[{"label":"positive zero","value":"a"},{"label":"negative zero","value":"a"}],"toolCalls":[]}"#,
        ),
        (
            "string-core-test262.test: test/built-ins/String/prototype/charAt/pos-rounding.js",
            r#"return [{ label: "-0.99999", value: "abc".charAt(-0.99999) },{ label: "-0.00001", value: "abc".charAt(-0.00001) },{ label: "0.00001", value: "abc".charAt(0.00001) },{ label: "0.99999", value: "abc".charAt(0.99999) },{ label: "1.00001", value: "abc".charAt(1.00001) },{ label: "1.99999", value: "abc".charAt(1.99999) }]"#,
            r#"{"ok":true,"value":[{"label":"-0.99999","value":"a"},{"label":"-0.00001","value":"a"},{"label":"0.00001","value":"a"},{"label":"0.99999","value":"a"},{"label":"1.00001","value":"b"},{"label":"1.99999","value":"b"}],"toolCalls":[]}"#,
        ),
        (
            "string-core-test262.test: test/built-ins/String/prototype/charCodeAt/S15.5.4.5_A1_T4.js",
            r#"return [{ label: "omitted position", value: "smart".charCodeAt() }]"#,
            r#"{"ok":true,"value":[{"label":"omitted position","value":115}],"toolCalls":[]}"#,
        ),
        (
            "string-core-test262.test: test/built-ins/String/prototype/charCodeAt/S15.5.4.5_A1_T7.js",
            r#"return [{ label: "undefined position", value: "lego".charCodeAt(undefined) }]"#,
            r#"{"ok":true,"value":[{"label":"undefined position","value":108}],"toolCalls":[]}"#,
        ),
        (
            "string-core-test262.test: test/built-ins/String/prototype/charCodeAt/S15.5.4.5_A1_T8.js",
            r#"return [{ label: "undefined position", value: "42".charCodeAt(undefined) }]"#,
            r#"{"ok":true,"value":[{"label":"undefined position","value":52}],"toolCalls":[]}"#,
        ),
        (
            "string-core-test262.test: test/built-ins/String/prototype/charCodeAt/pos-rounding.js",
            r#"return [{ label: "-0.99999", value: "abc".charCodeAt(-0.99999) },{ label: "-0.00001", value: "abc".charCodeAt(-0.00001) },{ label: "0.00001", value: "abc".charCodeAt(0.00001) },{ label: "0.99999", value: "abc".charCodeAt(0.99999) },{ label: "1.00001", value: "abc".charCodeAt(1.00001) },{ label: "1.99999", value: "abc".charCodeAt(1.99999) }]"#,
            r#"{"ok":true,"value":[{"label":"-0.99999","value":97},{"label":"-0.00001","value":97},{"label":"0.00001","value":97},{"label":"0.99999","value":97},{"label":"1.00001","value":98},{"label":"1.99999","value":98}],"toolCalls":[]}"#,
        ),
        (
            "string-core-test262.test: test/built-ins/String/prototype/codePointAt/return-single-code-unit.js",
            r#"return [{ label: "a", value: "abc".codePointAt(0) },{ label: "b", value: "abc".codePointAt(1) },{ label: "c", value: "abc".codePointAt(2) },{ label: "ordinary BMP", value: "ꪪ뮻".codePointAt(0) },{ label: "before high-surrogate range", value: "퟿ꪪ".codePointAt(0) },{ label: "low surrogate", value: "\udc00ꪪ".codePointAt(0) },{ label: "trailing D800", value: "123\ud800".codePointAt(3) },{ label: "trailing DAAA", value: "123\udaaa".codePointAt(3) },{ label: "trailing DBFF", value: "123\udbff".codePointAt(3) }]"#,
            r#"{"ok":true,"value":[{"label":"a","value":97},{"label":"b","value":98},{"label":"c","value":99},{"label":"ordinary BMP","value":43690},{"label":"before high-surrogate range","value":55295},{"label":"low surrogate","value":56320},{"label":"trailing D800","value":55296},{"label":"trailing DAAA","value":55978},{"label":"trailing DBFF","value":56319}],"toolCalls":[]}"#,
        ),
        (
            "string-core-test262.test: test/built-ins/String/prototype/codePointAt/return-first-code-unit.js",
            r#"return [{ label: "D800 before DBFF", value: "\ud800\udbff".codePointAt(0) },{ label: "D800 before E000", value: "\ud800".codePointAt(0) },{ label: "DAAA before DBFF", value: "\udaaa\udbff".codePointAt(0) },{ label: "DAAA before E000", value: "\udaaa".codePointAt(0) },{ label: "DBFF before DBFF", value: "\udbff\udbff".codePointAt(0) },{ label: "DBFF before E000", value: "\udbff".codePointAt(0) },{ label: "D800 before NUL", value: "\ud800\u0000".codePointAt(0) },{ label: "D800 before FFFF", value: "\ud800￿".codePointAt(0) },{ label: "DAAA before NUL", value: "\udaaa\u0000".codePointAt(0) },{ label: "DAAA before FFFF", value: "\udaaa￿".codePointAt(0) },{ label: "DBFF before FFFF", value: "\udbff￿".codePointAt(0) }]"#,
            r#"{"ok":true,"value":[{"label":"D800 before DBFF","value":55296},{"label":"D800 before E000","value":55296},{"label":"DAAA before DBFF","value":55978},{"label":"DAAA before E000","value":55978},{"label":"DBFF before DBFF","value":56319},{"label":"DBFF before E000","value":56319},{"label":"D800 before NUL","value":55296},{"label":"D800 before FFFF","value":55296},{"label":"DAAA before NUL","value":55978},{"label":"DAAA before FFFF","value":55978},{"label":"DBFF before FFFF","value":56319}],"toolCalls":[]}"#,
        ),
        (
            "string-core-test262.test: test/built-ins/String/prototype/codePointAt/return-utf16-decode.js",
            r#"return [{ label: "U+10000", value: "𐀀".codePointAt(0) },{ label: "U+101D0", value: "𐇐".codePointAt(0) },{ label: "U+103FF", value: "𐏿".codePointAt(0) },{ label: "U+BA800", value: "򺠀".codePointAt(0) },{ label: "U+BA9D0", value: "򺧐".codePointAt(0) },{ label: "U+BABFF", value: "򺯿".codePointAt(0) },{ label: "U+10FC00", value: "􏰀".codePointAt(0) },{ label: "U+10FDD0", value: "􏷐".codePointAt(0) },{ label: "U+10FFFF", value: "􏿿".codePointAt(0) }]"#,
            r#"{"ok":true,"value":[{"label":"U+10000","value":65536},{"label":"U+101D0","value":66000},{"label":"U+103FF","value":66559},{"label":"U+BA800","value":763904},{"label":"U+BA9D0","value":764368},{"label":"U+BABFF","value":764927},{"label":"U+10FC00","value":1113088},{"label":"U+10FDD0","value":1113552},{"label":"U+10FFFF","value":1114111}],"toolCalls":[]}"#,
        ),
        (
            "string-core-test262.test: test/built-ins/String/prototype/codePointAt/return-code-unit-coerced-position.js",
            r#"return [{ label: "NaN", value: "𐀀".codePointAt(NaN) },{ label: "undefined", value: "𐀀".codePointAt(undefined) }]"#,
            r#"{"ok":true,"value":[{"label":"NaN","value":65536},{"label":"undefined","value":65536}],"toolCalls":[]}"#,
        ),
        (
            "string-core-test262.test: test/built-ins/String/prototype/codePointAt/returns-undefined-on-position-less-than-zero.js",
            r#"return [{ label: "negative one", value: "abc".codePointAt(-1) === undefined },{ label: "negative infinity", value: "abc".codePointAt(-Infinity) === undefined }]"#,
            r#"{"ok":true,"value":[{"label":"negative one","value":true},{"label":"negative infinity","value":true}],"toolCalls":[]}"#,
        ),
        (
            "string-core-test262.test: test/built-ins/String/prototype/codePointAt/returns-undefined-on-position-equal-or-more-than-size.js",
            r#"return [{ label: "equal to size", value: "abc".codePointAt(3) === undefined },{ label: "greater than size", value: "abc".codePointAt(4) === undefined },{ label: "positive infinity", value: "abc".codePointAt(Infinity) === undefined }]"#,
            r#"{"ok":true,"value":[{"label":"equal to size","value":true},{"label":"greater than size","value":true},{"label":"positive infinity","value":true}],"toolCalls":[]}"#,
        ),
        (
            "string-core-test262.test: test/built-ins/String/prototype/concat/S15.5.4.6_A1_T4.js",
            r#"return [{ label: "no arguments", value: "lego".concat() }]"#,
            r#"{"ok":true,"value":[{"label":"no arguments","value":"lego"}],"toolCalls":[]}"#,
        ),
        (
            "string-core-test262.test: test/built-ins/String/prototype/toString/string-primitive.js",
            r#"return [{ label: "empty string", value: "".toString() },{ label: "non-empty string", value: "str".toString() }]"#,
            r#"{"ok":true,"value":[{"label":"empty string","value":""},{"label":"non-empty string","value":"str"}],"toolCalls":[]}"#,
        ),
        (
            "string-core-test262.test: test/built-ins/String/prototype/normalize/return-normalized-string.js",
            r#"return [{ label: "NFC short", value: "ẛ̣".normalize("NFC") },{ label: "NFD short", value: "ẛ̣".normalize("NFD") },{ label: "NFKC short", value: "ẛ̣".normalize("NFKC") },{ label: "NFKD short", value: "ẛ̣".normalize("NFKD") },{ label: "NFC long", value: "Å⫝̸क़Ω̈́".normalize("NFC") },{ label: "NFD long", value: "Å⫝̸क़Ω̈́".normalize("NFD") },{ label: "NFKC long", value: "Å⫝̸क़Ω̈́".normalize("NFKC") },{ label: "NFKD long", value: "Å⫝̸क़Ω̈́".normalize("NFKD") }]"#,
            r#"{"ok":true,"value":[{"label":"NFC short","value":"ẛ̣"},{"label":"NFD short","value":"ẛ̣"},{"label":"NFKC short","value":"ṩ"},{"label":"NFKD short","value":"ṩ"},{"label":"NFC long","value":"Å⫝̸क़Ω̈́"},{"label":"NFD long","value":"Å⫝̸क़Ω̈́"},{"label":"NFKC long","value":"Å⫝̸क़Ω̈́"},{"label":"NFKD long","value":"Å⫝̸क़Ω̈́"}],"toolCalls":[]}"#,
        ),
        (
            "string-core-test262.test: test/built-ins/String/prototype/normalize/return-normalized-string-using-default-parameter.js",
            r#"return [{ label: "omitted", value: "Å⫝̸क़Ω̈́".normalize() },{ label: "undefined", value: "Å⫝̸क़Ω̈́".normalize(undefined) }]"#,
            r#"{"ok":true,"value":[{"label":"omitted","value":"Å⫝̸क़Ω̈́"},{"label":"undefined","value":"Å⫝̸क़Ω̈́"}],"toolCalls":[]}"#,
        ),
        (
            "string-core-test262.test: test/built-ins/String/prototype/normalize/form-is-not-valid-throws.js",
            r#"return [{ label: "bar", value: (() => { try { "foo".normalize("bar"); return false } catch (error) { return error instanceof RangeError } })() },{ label: "NFC1", value: (() => { try { "foo".normalize("NFC1"); return false } catch (error) { return error instanceof RangeError } })() }]"#,
            r#"{"ok":true,"value":[{"label":"bar","value":true},{"label":"NFC1","value":true}],"toolCalls":[]}"#,
        ),
        (
            "string-core-test262.test: test/built-ins/String/prototype/localeCompare/15.5.4.9_CE.js",
            r#"return [{ label: "D70", value: "ö".localeCompare("ö") },{ label: "reordered diaeresis", value: "ạ̈".localeCompare("ạ̈") },{ label: "reordered marks", value: "ạ̈".localeCompare("ạ̈") },{ label: "precomposed dot below", value: "ạ̈".localeCompare("ạ̈") },{ label: "breve after diaeresis", value: "ä̆".localeCompare("ä̆") },{ label: "diaeresis after breve", value: "ă̈".localeCompare("ă̈") },{ label: "Hangul", value: "퓛".localeCompare("퓛") },{ label: "angstrom compatibility", value: "Å".localeCompare("Å") },{ label: "angstrom decomposed", value: "Å".localeCompare("Å") },{ label: "reordered horn and dot", value: "x̛̣".localeCompare("x̛̣") },{ label: "Vietnamese precomposed 1", value: "ự".localeCompare("ự") },{ label: "Vietnamese decomposed", value: "ự".localeCompare("ự") },{ label: "Vietnamese precomposed 2", value: "ự".localeCompare("ự") },{ label: "Vietnamese reordered", value: "ự".localeCompare("ự") },{ label: "cedilla", value: "Ç".localeCompare("Ç") },{ label: "q reordered", value: "q̣̇".localeCompare("q̣̇") },{ label: "Hangul syllable", value: "가".localeCompare("가") },{ label: "ohm", value: "Ω".localeCompare("Ω") },{ label: "angstrom", value: "Å".localeCompare("Å") },{ label: "circumflex", value: "ô".localeCompare("ô") },{ label: "s with marks", value: "ṩ".localeCompare("ṩ") },{ label: "d composed plus dot", value: "ḍ̇".localeCompare("ḍ̇") },{ label: "d two precompositions", value: "ḍ̇".localeCompare("ḍ̇") }]"#,
            r#"{"ok":true,"value":[{"label":"D70","value":0},{"label":"reordered diaeresis","value":0},{"label":"reordered marks","value":0},{"label":"precomposed dot below","value":0},{"label":"breve after diaeresis","value":0},{"label":"diaeresis after breve","value":0},{"label":"Hangul","value":0},{"label":"angstrom compatibility","value":0},{"label":"angstrom decomposed","value":0},{"label":"reordered horn and dot","value":0},{"label":"Vietnamese precomposed 1","value":0},{"label":"Vietnamese decomposed","value":0},{"label":"Vietnamese precomposed 2","value":0},{"label":"Vietnamese reordered","value":0},{"label":"cedilla","value":0},{"label":"q reordered","value":0},{"label":"Hangul syllable","value":0},{"label":"ohm","value":0},{"label":"angstrom","value":0},{"label":"circumflex","value":0},{"label":"s with marks","value":0},{"label":"d composed plus dot","value":0},{"label":"d two precompositions","value":0}],"toolCalls":[]}"#,
        ),
        (
            "string-core-test262.test: test/built-ins/String/fromCharCode/S15.5.3.2_A2.js",
            r#"return [{ label: "no arguments", value: String.fromCharCode() }]"#,
            r#"{"ok":true,"value":[{"label":"no arguments","value":""}],"toolCalls":[]}"#,
        ),
        (
            "string-core-test262.test: test/built-ins/String/fromCharCode/S15.5.3.2_A3_T1.js",
            r#"return [{ label: "ABBA", value: String.fromCharCode(65, 66, 66, 65) }]"#,
            r#"{"ok":true,"value":[{"label":"ABBA","value":"ABBA"}],"toolCalls":[]}"#,
        ),
        (
            "string-core-test262.test: test/built-ins/String/fromCharCode/S9.7_A1.js",
            r#"return [{ label: "NaN", value: String.fromCharCode(NaN).charCodeAt(0) },{ label: "zero", value: String.fromCharCode(0).charCodeAt(0) },{ label: "negative zero", value: String.fromCharCode(-0).charCodeAt(0) },{ label: "positive infinity", value: String.fromCharCode(Infinity).charCodeAt(0) },{ label: "negative infinity", value: String.fromCharCode(-Infinity).charCodeAt(0) }]"#,
            r#"{"ok":true,"value":[{"label":"NaN","value":0},{"label":"zero","value":0},{"label":"negative zero","value":0},{"label":"positive infinity","value":0},{"label":"negative infinity","value":0}],"toolCalls":[]}"#,
        ),
        (
            "string-core-test262.test: test/built-ins/String/fromCharCode/S9.7_A2.1.js",
            r#"return [{ label: "0", value: String.fromCharCode(0).charCodeAt(0) },{ label: "1", value: String.fromCharCode(1).charCodeAt(0) },{ label: "-1", value: String.fromCharCode(-1).charCodeAt(0) },{ label: "65535", value: String.fromCharCode(65535).charCodeAt(0) },{ label: "65534", value: String.fromCharCode(65534).charCodeAt(0) },{ label: "65536", value: String.fromCharCode(65536).charCodeAt(0) },{ label: "4294967295", value: String.fromCharCode(4294967295).charCodeAt(0) },{ label: "4294967294", value: String.fromCharCode(4294967294).charCodeAt(0) },{ label: "4294967296", value: String.fromCharCode(4294967296).charCodeAt(0) }]"#,
            r#"{"ok":true,"value":[{"label":"0","value":0},{"label":"1","value":1},{"label":"-1","value":65535},{"label":"65535","value":65535},{"label":"65534","value":65534},{"label":"65536","value":0},{"label":"4294967295","value":65535},{"label":"4294967294","value":65534},{"label":"4294967296","value":0}],"toolCalls":[]}"#,
        ),
        (
            "string-core-test262.test: test/built-ins/String/fromCharCode/S9.7_A2.2.js",
            r#"return [{ label: "-32767", value: String.fromCharCode(-32767).charCodeAt(0) },{ label: "-32768", value: String.fromCharCode(-32768).charCodeAt(0) },{ label: "-32769", value: String.fromCharCode(-32769).charCodeAt(0) },{ label: "-65535", value: String.fromCharCode(-65535).charCodeAt(0) },{ label: "-65536", value: String.fromCharCode(-65536).charCodeAt(0) },{ label: "-65537", value: String.fromCharCode(-65537).charCodeAt(0) },{ label: "65535", value: String.fromCharCode(65535).charCodeAt(0) },{ label: "65536", value: String.fromCharCode(65536).charCodeAt(0) },{ label: "65537", value: String.fromCharCode(65537).charCodeAt(0) },{ label: "131071", value: String.fromCharCode(131071).charCodeAt(0) },{ label: "131072", value: String.fromCharCode(131072).charCodeAt(0) },{ label: "131073", value: String.fromCharCode(131073).charCodeAt(0) }]"#,
            r#"{"ok":true,"value":[{"label":"-32767","value":32769},{"label":"-32768","value":32768},{"label":"-32769","value":32767},{"label":"-65535","value":1},{"label":"-65536","value":0},{"label":"-65537","value":65535},{"label":"65535","value":65535},{"label":"65536","value":0},{"label":"65537","value":1},{"label":"131071","value":65535},{"label":"131072","value":0},{"label":"131073","value":1}],"toolCalls":[]}"#,
        ),
        (
            "string-core-test262.test: test/built-ins/String/fromCharCode/S9.7_A3.2_T1.js",
            r#"return [{ label: "positive fraction", value: String.fromCharCode(1.2345).charCodeAt(0) },{ label: "negative fraction", value: String.fromCharCode(-5.4321).charCodeAt(0) }]"#,
            r#"{"ok":true,"value":[{"label":"positive fraction","value":1},{"label":"negative fraction","value":65531}],"toolCalls":[]}"#,
        ),
        (
            "string-core-test262.test: test/built-ins/String/fromCodePoint/arguments-is-empty.js",
            r#"return [{ label: "no arguments", value: String.fromCodePoint() }]"#,
            r#"{"ok":true,"value":[{"label":"no arguments","value":""}],"toolCalls":[]}"#,
        ),
        (
            "string-core-test262.test: test/built-ins/String/fromCodePoint/return-string-value.js",
            r#"return [{ label: "NUL", value: String.fromCodePoint(0) },{ label: "asterisk", value: String.fromCodePoint(42) },{ label: "AZ", value: String.fromCodePoint(65, 90) },{ label: "Cyrillic", value: String.fromCodePoint(1028) },{ label: "hex supplementary", value: String.fromCodePoint(194564) },{ label: "decimal supplementary", value: String.fromCodePoint(194564) },{ label: "mixed supplementary", value: String.fromCodePoint(119558, 97, 119559) },{ label: "maximum code point", value: String.fromCodePoint(1114111) }]"#,
            r#"{"ok":true,"value":[{"label":"NUL","value":"\u0000"},{"label":"asterisk","value":"*"},{"label":"AZ","value":"AZ"},{"label":"Cyrillic","value":"Є"},{"label":"hex supplementary","value":"你"},{"label":"decimal supplementary","value":"你"},{"label":"mixed supplementary","value":"𝌆a𝌇"},{"label":"maximum code point","value":"􏿿"}],"toolCalls":[]}"#,
        ),
        (
            "string-core-test262.test: test/built-ins/String/fromCodePoint/argument-is-not-integer.js",
            r#"return [{ label: "fraction", value: (() => { try { String.fromCodePoint(3.14); return false } catch (error) { return error instanceof RangeError } })() },{ label: "fraction after valid", value: (() => { try { String.fromCodePoint(42, 3.14); return false } catch (error) { return error instanceof RangeError } })() }]"#,
            r#"{"ok":true,"value":[{"label":"fraction","value":true},{"label":"fraction after valid","value":true}],"toolCalls":[]}"#,
        ),
        (
            "string-core-test262.test: test/built-ins/String/fromCodePoint/number-is-out-of-range.js",
            r#"return [{ label: "negative one", value: (() => { try { String.fromCodePoint(-1); return false } catch (error) { return error instanceof RangeError } })() },{ label: "negative after valid", value: (() => { try { String.fromCodePoint(1, -1); return false } catch (error) { return error instanceof RangeError } })() },{ label: "above maximum", value: (() => { try { String.fromCodePoint(1114112); return false } catch (error) { return error instanceof RangeError } })() },{ label: "infinity", value: (() => { try { String.fromCodePoint(Infinity); return false } catch (error) { return error instanceof RangeError } })() }]"#,
            r#"{"ok":true,"value":[{"label":"negative one","value":true},{"label":"negative after valid","value":true},{"label":"above maximum","value":true},{"label":"infinity","value":true}],"toolCalls":[]}"#,
        ),
    ]);
}

#[test]
fn string_search_test262() {
    check(&[
        (
            "string-search-test262.test: test/built-ins/String/prototype/split/call-split-l-0-instance-is-string-hello.js",
            r#"const result = "hello".split("l", 0); return [result.length, result[0] === undefined]"#,
            r#"{"ok":true,"value":[0,true],"toolCalls":[]}"#,
        ),
        (
            "string-search-test262.test: test/built-ins/String/prototype/split/call-split-l-1-instance-is-string-hello.js",
            r#"const result = "hello".split("l", 1); return [result.length, result[0]]"#,
            r#"{"ok":true,"value":[1,"he"],"toolCalls":[]}"#,
        ),
        (
            "string-search-test262.test: test/built-ins/String/prototype/split/call-split-l-2-instance-is-string-hello.js",
            r#"const result = "hello".split("l", 2); return [result.length, result[0], result[1]]"#,
            r#"{"ok":true,"value":[2,"he",""],"toolCalls":[]}"#,
        ),
        (
            "string-search-test262.test: test/built-ins/String/prototype/split/call-split-l-3-instance-is-string-hello.js",
            r#"const result = "hello".split("l", 3); return [result.length, result[0], result[1], result[2]]"#,
            r#"{"ok":true,"value":[3,"he","","o"],"toolCalls":[]}"#,
        ),
        (
            "string-search-test262.test: test/built-ins/String/prototype/split/call-split-l-4-instance-is-string-hello.js",
            r#"const result = "hello".split("l", 4); return [result.length, result[0], result[1], result[2]]"#,
            r#"{"ok":true,"value":[3,"he","","o"],"toolCalls":[]}"#,
        ),
        (
            "string-search-test262.test: test/built-ins/String/prototype/split/call-split-l-na-n-instance-is-string-hello.js",
            r#"const result = "hello".split("l", NaN); return [result.length, result[0] === undefined]"#,
            r#"{"ok":true,"value":[0,true],"toolCalls":[]}"#,
        ),
        (
            "string-search-test262.test: test/built-ins/String/prototype/split/call-split-l-instance-is-string-hello.js",
            r#"const result = "hello".split("l"); return [result.length, result[0], result[1], result[2]]"#,
            r#"{"ok":true,"value":[3,"he","","o"],"toolCalls":[]}"#,
        ),
        (
            "string-search-test262.test: test/built-ins/String/prototype/split/call-split-ll-instance-is-string-hello.js",
            r#"const result = "hello".split("ll"); return [result.length, result[0], result[1]]"#,
            r#"{"ok":true,"value":[2,"he","o"],"toolCalls":[]}"#,
        ),
        (
            "string-search-test262.test: test/built-ins/String/prototype/split/call-split-h-instance-is-string-hello.js",
            r#"const result = "hello".split("h"); return [result.length, result[0], result[1]]"#,
            r#"{"ok":true,"value":[2,"","ello"],"toolCalls":[]}"#,
        ),
        (
            "string-search-test262.test: test/built-ins/String/prototype/split/call-split-hello-instance-is-string-hello.js",
            r#"const result = "hello".split("hello"); return [result.length, result[0], result[1]]"#,
            r#"{"ok":true,"value":[2,"",""],"toolCalls":[]}"#,
        ),
        (
            "string-search-test262.test: test/built-ins/String/prototype/split/call-split-hellothere-instance-is-string-hello.js",
            r#"const result = "hello".split("hellothere"); return [result.length, result[0]]"#,
            r#"{"ok":true,"value":[1,"hello"],"toolCalls":[]}"#,
        ),
        (
            "string-search-test262.test: test/built-ins/String/prototype/split/call-split-o-instance-is-string-hello.js",
            r#"const result = "hello".split("o"); return [result.length, result[0], result[1]]"#,
            r#"{"ok":true,"value":[2,"hell",""],"toolCalls":[]}"#,
        ),
        (
            "string-search-test262.test: test/built-ins/String/prototype/split/call-split-x-instance-is-string-hello.js",
            r#"const result = "hello".split("x"); return [result.length, result[0]]"#,
            r#"{"ok":true,"value":[1,"hello"],"toolCalls":[]}"#,
        ),
        (
            "string-search-test262.test: test/built-ins/String/prototype/split/call-split-x-instance-is-empty-string.js",
            r#"const result = "".split("x"); return [result.length, result[0]]"#,
            r#"{"ok":true,"value":[1,""],"toolCalls":[]}"#,
        ),
        (
            "string-search-test262.test: test/built-ins/String/prototype/split/call-split-4-instance-is-string-one-1-two-2-four-4.js",
            r#"const result = "one-1 two-2 four-4".split("-4"); return [result.length, result[0], result[1]]"#,
            r#"{"ok":true,"value":[2,"one-1 two-2 four",""],"toolCalls":[]}"#,
        ),
        (
            "string-search-test262.test: test/built-ins/String/prototype/split/call-split-on-instance-is-string-one-1-two-2-four-4.js",
            r#"const result = "one-1 two-2 four-4".split("on"); return [result.length, result[0], result[1]]"#,
            r#"{"ok":true,"value":[2,"","e-1 two-2 four-4"],"toolCalls":[]}"#,
        ),
        (
            "string-search-test262.test: test/built-ins/String/prototype/split/call-split-instance-is-string-one-two-three-four-five.js",
            r#"const result = "one two three four five".split(" "); return [result.length, ...result]"#,
            r#"{"ok":true,"value":[5,"one","two","three","four","five"],"toolCalls":[]}"#,
        ),
        (
            "string-search-test262.test: test/built-ins/String/prototype/split/call-split-instance-is-string-one-two-three.js",
            r#"const result = "one two three".split(""); return [result[0], result[1], result[11], result[12]]"#,
            r#"{"ok":true,"value":["o","n","e","e"],"toolCalls":[]}"#,
        ),
        (
            "string-search-test262.test: test/built-ins/String/prototype/split/call-split-instance-is-string.js",
            r#"const result = " ".split(" "); return [result.length, result[0], result[1]]"#,
            r#"{"ok":true,"value":[2,"",""],"toolCalls":[]}"#,
        ),
        (
            "string-search-test262.test: test/built-ins/String/prototype/split/instance-is-string-one-two-three-four-five.js",
            r#"const result = "one,two,three,four,five".split(); return [result.length, result[0]]"#,
            r#"{"ok":true,"value":[1,"one,two,three,four,five"],"toolCalls":[]}"#,
        ),
        (
            "string-search-test262.test: test/built-ins/String/prototype/split/instance-is-string.js",
            r#"const result = " ".split(); return [result.length, result[0]]"#,
            r#"{"ok":true,"value":[1," "],"toolCalls":[]}"#,
        ),
        (
            "string-search-test262.test: test/built-ins/String/prototype/split/separator-colon-instance-is-string-one-1-two-2-four-4.js",
            r#"const result = "one-1,two-2,four-4".split(":"); return [result.length, result[0]]"#,
            r#"{"ok":true,"value":[1,"one-1,two-2,four-4"],"toolCalls":[]}"#,
        ),
        (
            "string-search-test262.test: test/built-ins/String/prototype/split/separator-comma-instance-is-string-one-two-three-four-five.js",
            r#"const result = "one,two,three,four,five".split(","); return [result.length, ...result]"#,
            r#"{"ok":true,"value":[5,"one","two","three","four","five"],"toolCalls":[]}"#,
        ),
        (
            "string-search-test262.test: test/built-ins/String/prototype/split/separator-empty-string-instance-is-string.js",
            r#"const result = " ".split(""); return [result.length, result[0]]"#,
            r#"{"ok":true,"value":[1," "],"toolCalls":[]}"#,
        ),
        (
            "string-search-test262.test: test/built-ins/String/prototype/split/call-split-without-arguments-and-instance-is-empty-string.js",
            r#"const result = "".split(); return [result.length, result[0]]"#,
            r#"{"ok":true,"value":[1,""],"toolCalls":[]}"#,
        ),
        (
            "string-search-test262.test: test/built-ins/String/prototype/split/separator-undef.js",
            r#"const result = "undefined is not a function".split(); return [Array.isArray(result), result.length, result[0]]"#,
            r#"{"ok":true,"value":[true,1,"undefined is not a function"],"toolCalls":[]}"#,
        ),
        (
            "string-search-test262.test: test/built-ins/String/prototype/slice/S15.5.4.13_A1_T6.js",
            r#"return ["undefined".slice(undefined, 3)]"#,
            r#"{"ok":true,"value":["und"],"toolCalls":[]}"#,
        ),
        (
            "string-search-test262.test: test/built-ins/String/prototype/slice/S15.5.4.13_A1_T14.js",
            r#"return ["report".slice(undefined)]"#,
            r#"{"ok":true,"value":["report"],"toolCalls":[]}"#,
        ),
        (
            "string-search-test262.test: test/built-ins/String/prototype/slice/S15.5.4.13_A2_T1.js",
            r#"return [typeof "this is a string object".slice()]"#,
            r#"{"ok":true,"value":["string"],"toolCalls":[]}"#,
        ),
        (
            "string-search-test262.test: test/built-ins/String/prototype/slice/S15.5.4.13_A2_T2.js",
            r#"return ["this is a string object".slice(NaN, Infinity)]"#,
            r#"{"ok":true,"value":["this is a string object"],"toolCalls":[]}"#,
        ),
        (
            "string-search-test262.test: test/built-ins/String/prototype/slice/S15.5.4.13_A2_T3.js",
            r#"return ["".slice(1, 0)]"#,
            r#"{"ok":true,"value":[""],"toolCalls":[]}"#,
        ),
        (
            "string-search-test262.test: test/built-ins/String/prototype/slice/S15.5.4.13_A2_T4.js",
            r#"return ["this is a string object".slice(Infinity, NaN)]"#,
            r#"{"ok":true,"value":[""],"toolCalls":[]}"#,
        ),
        (
            "string-search-test262.test: test/built-ins/String/prototype/slice/S15.5.4.13_A2_T5.js",
            r#"return ["this is a string object".slice(Infinity, Infinity)]"#,
            r#"{"ok":true,"value":[""],"toolCalls":[]}"#,
        ),
        (
            "string-search-test262.test: test/built-ins/String/prototype/slice/S15.5.4.13_A2_T6.js",
            r#"return ["this is a string object".slice(-0.01, 0)]"#,
            r#"{"ok":true,"value":[""],"toolCalls":[]}"#,
        ),
        (
            "string-search-test262.test: test/built-ins/String/prototype/slice/S15.5.4.13_A2_T7.js",
            r#"const text = "this is a string object"; return [text.slice(text.length, text.length)]"#,
            r#"{"ok":true,"value":[""],"toolCalls":[]}"#,
        ),
        (
            "string-search-test262.test: test/built-ins/String/prototype/slice/S15.5.4.13_A2_T8.js",
            r#"const text = "this is a string object"; return [text.slice(text.length + 1, 0)]"#,
            r#"{"ok":true,"value":[""],"toolCalls":[]}"#,
        ),
        (
            "string-search-test262.test: test/built-ins/String/prototype/slice/S15.5.4.13_A2_T9.js",
            r#"return ["this is a string object".slice(-Infinity, -Infinity)]"#,
            r#"{"ok":true,"value":[""],"toolCalls":[]}"#,
        ),
        (
            "string-search-test262.test: test/built-ins/String/prototype/substring/S15.5.4.15_A1_T6.js",
            r#"return ["undefined".substring(undefined, 3)]"#,
            r#"{"ok":true,"value":["und"],"toolCalls":[]}"#,
        ),
        (
            "string-search-test262.test: test/built-ins/String/prototype/substring/S15.5.4.15_A1_T14.js",
            r#"return ["report".substring(undefined)]"#,
            r#"{"ok":true,"value":["report"],"toolCalls":[]}"#,
        ),
        (
            "string-search-test262.test: test/built-ins/String/prototype/substring/S15.5.4.15_A2_T1.js",
            r#"return [typeof "this is a string object".substring()]"#,
            r#"{"ok":true,"value":["string"],"toolCalls":[]}"#,
        ),
        (
            "string-search-test262.test: test/built-ins/String/prototype/substring/S15.5.4.15_A2_T2.js",
            r#"return ["this is a string object".substring(NaN, Infinity)]"#,
            r#"{"ok":true,"value":["this is a string object"],"toolCalls":[]}"#,
        ),
        (
            "string-search-test262.test: test/built-ins/String/prototype/substring/S15.5.4.15_A2_T3.js",
            r#"return ["".substring(1, 0)]"#,
            r#"{"ok":true,"value":[""],"toolCalls":[]}"#,
        ),
        (
            "string-search-test262.test: test/built-ins/String/prototype/substring/S15.5.4.15_A2_T4.js",
            r#"return ["this is a string object".substring(Infinity, NaN)]"#,
            r#"{"ok":true,"value":["this is a string object"],"toolCalls":[]}"#,
        ),
        (
            "string-search-test262.test: test/built-ins/String/prototype/substring/S15.5.4.15_A2_T5.js",
            r#"return ["this is a string object".substring(Infinity, Infinity)]"#,
            r#"{"ok":true,"value":[""],"toolCalls":[]}"#,
        ),
        (
            "string-search-test262.test: test/built-ins/String/prototype/substring/S15.5.4.15_A2_T6.js",
            r#"return ["this is a string object".substring(-0.01, 0)]"#,
            r#"{"ok":true,"value":[""],"toolCalls":[]}"#,
        ),
        (
            "string-search-test262.test: test/built-ins/String/prototype/substring/S15.5.4.15_A2_T7.js",
            r#"const text = "this is a string object"; return [text.substring(text.length, text.length)]"#,
            r#"{"ok":true,"value":[""],"toolCalls":[]}"#,
        ),
        (
            "string-search-test262.test: test/built-ins/String/prototype/substring/S15.5.4.15_A2_T8.js",
            r#"const text = "this is a string object"; return [text.substring(text.length + 1, 0)]"#,
            r#"{"ok":true,"value":["this is a string object"],"toolCalls":[]}"#,
        ),
        (
            "string-search-test262.test: test/built-ins/String/prototype/substring/S15.5.4.15_A2_T9.js",
            r#"return ["this is a string object".substring(-Infinity, -Infinity)]"#,
            r#"{"ok":true,"value":[""],"toolCalls":[]}"#,
        ),
        (
            "string-search-test262.test: test/built-ins/String/prototype/substring/S15.5.4.15_A2_T10.js",
            r#"return ["this_is_a_string object".substring(0, 8)]"#,
            r#"{"ok":true,"value":["this_is_"],"toolCalls":[]}"#,
        ),
        (
            "string-search-test262.test: test/built-ins/String/prototype/includes/String.prototype.includes_FailMissingLetter.js",
            r#"return ["word".includes("a", 0)]"#,
            r#"{"ok":true,"value":[false],"toolCalls":[]}"#,
        ),
        (
            "string-search-test262.test: test/built-ins/String/prototype/includes/String.prototype.includes_SuccessNoLocation.js",
            r#"return ["word".includes("w")]"#,
            r#"{"ok":true,"value":[true],"toolCalls":[]}"#,
        ),
        (
            "string-search-test262.test: test/built-ins/String/prototype/includes/String.prototype.includes_FailBadLocation.js",
            r#"return ["word".includes("w", 5)]"#,
            r#"{"ok":true,"value":[false],"toolCalls":[]}"#,
        ),
        (
            "string-search-test262.test: test/built-ins/String/prototype/includes/String.prototype.includes_FailLocation.js",
            r#"return ["word".includes("o", 3)]"#,
            r#"{"ok":true,"value":[false],"toolCalls":[]}"#,
        ),
        (
            "string-search-test262.test: test/built-ins/String/prototype/includes/String.prototype.includes_Success.js",
            r#"return ["word".includes("w", 0)]"#,
            r#"{"ok":true,"value":[true],"toolCalls":[]}"#,
        ),
        (
            "string-search-test262.test: test/built-ins/String/prototype/includes/searchstring-found-with-position.js",
            r#"const text = "The future is cool!"; return [text.includes("The future", 0), text.includes(" is ", 1), text.includes("cool!", 10)]"#,
            r#"{"ok":true,"value":[true,true,true],"toolCalls":[]}"#,
        ),
        (
            "string-search-test262.test: test/built-ins/String/prototype/includes/searchstring-found-without-position.js",
            r#"const text = "The future is cool!"; return [text.includes("The future"), text.includes("is cool!"), text.includes(text)]"#,
            r#"{"ok":true,"value":[true,true,true],"toolCalls":[]}"#,
        ),
        (
            "string-search-test262.test: test/built-ins/String/prototype/includes/searchstring-not-found-with-position.js",
            r#"const text = "The future is cool!"; return [text.includes("The future", 1), text.includes(text, 1)]"#,
            r#"{"ok":true,"value":[false,false],"toolCalls":[]}"#,
        ),
        (
            "string-search-test262.test: test/built-ins/String/prototype/includes/searchstring-not-found-without-position.js",
            r#"const text = "The future is cool!"; return [text.includes("Flash"), text.includes("FUTURE")]"#,
            r#"{"ok":true,"value":[false,false],"toolCalls":[]}"#,
        ),
        (
            "string-search-test262.test: test/built-ins/String/prototype/includes/return-false-with-out-of-bounds-position.js",
            r#"const text = "The future is cool!"; return [
      text.includes("!", text.length + 1), text.includes("!", 100), text.includes("!", Infinity), text.includes("!", text.length),
    ]"#,
            r#"{"ok":true,"value":[false,false,false,false],"toolCalls":[]}"#,
        ),
        (
            "string-search-test262.test: test/built-ins/String/prototype/includes/return-true-if-searchstring-is-empty.js",
            r#"const text = "The future is cool!"; return [text.includes("", text.length), text.includes(""), text.includes("", Infinity)]"#,
            r#"{"ok":true,"value":[true,true,true],"toolCalls":[]}"#,
        ),
        (
            "string-search-test262.test: test/built-ins/String/prototype/includes/coerced-values-of-position.js",
            r#"const text = "The future is cool!"; return [
      text.includes("The future", NaN), text.includes("The future", undefined), text.includes("The future", 0.4),
      text.includes("The future", -1), text.includes("The future", 1.4),
    ]"#,
            r#"{"ok":true,"value":[true,true,true,true,false],"toolCalls":[]}"#,
        ),
        (
            "string-search-test262.test: test/built-ins/String/prototype/startsWith/searchstring-found-with-position.js",
            r#"const text = "The future is cool!"; return [text.startsWith("The future", 0), text.startsWith("future", 4), text.startsWith(" is cool!", 10)]"#,
            r#"{"ok":true,"value":[true,true,true],"toolCalls":[]}"#,
        ),
        (
            "string-search-test262.test: test/built-ins/String/prototype/startsWith/searchstring-found-without-position.js",
            r#"const text = "The future is cool!"; return [text.startsWith("The "), text.startsWith("The future"), text.startsWith(text)]"#,
            r#"{"ok":true,"value":[true,true,true],"toolCalls":[]}"#,
        ),
        (
            "string-search-test262.test: test/built-ins/String/prototype/startsWith/searchstring-not-found-with-position.js",
            r#"const text = "The future is cool!"; return [text.startsWith("The future", 1), text.startsWith(text, 1)]"#,
            r#"{"ok":true,"value":[false,false],"toolCalls":[]}"#,
        ),
        (
            "string-search-test262.test: test/built-ins/String/prototype/startsWith/searchstring-not-found-without-position.js",
            r#"const text = "The future is cool!"; return [text.startsWith("Flash"), text.startsWith("THE FUTURE"), text.startsWith("future is cool!")]"#,
            r#"{"ok":true,"value":[false,false,false],"toolCalls":[]}"#,
        ),
        (
            "string-search-test262.test: test/built-ins/String/prototype/startsWith/out-of-bounds-position.js",
            r#"const text = "The future is cool!"; return [
      text.startsWith("!", text.length), text.startsWith("!", 100), text.startsWith("!", Infinity),
      text.startsWith("The future", -1), text.startsWith("The future", -Infinity),
    ]"#,
            r#"{"ok":true,"value":[false,false,false,true,true],"toolCalls":[]}"#,
        ),
        (
            "string-search-test262.test: test/built-ins/String/prototype/startsWith/return-true-if-searchstring-is-empty.js",
            r#"const text = "The future is cool!"; return [text.startsWith(""), text.startsWith("", text.length), text.startsWith("", Infinity)]"#,
            r#"{"ok":true,"value":[true,true,true],"toolCalls":[]}"#,
        ),
        (
            "string-search-test262.test: test/built-ins/String/prototype/startsWith/coerced-values-of-position.js",
            r#"const text = "The future is cool!"; return [
      text.startsWith("The future", NaN), text.startsWith("The future", undefined),
      text.startsWith("The future", 0.4), text.startsWith("The future", 1.4),
    ]"#,
            r#"{"ok":true,"value":[true,true,true,false],"toolCalls":[]}"#,
        ),
        (
            "string-search-test262.test: test/built-ins/String/prototype/endsWith/String.prototype.endsWith_Success.js",
            r#"return ["word".endsWith("d")]"#,
            r#"{"ok":true,"value":[true],"toolCalls":[]}"#,
        ),
        (
            "string-search-test262.test: test/built-ins/String/prototype/endsWith/String.prototype.endsWith_Success_2.js",
            r#"return ["word".endsWith("d", 4)]"#,
            r#"{"ok":true,"value":[true],"toolCalls":[]}"#,
        ),
        (
            "string-search-test262.test: test/built-ins/String/prototype/endsWith/String.prototype.endsWith_Success_3.js",
            r#"return ["word".endsWith("d", 25)]"#,
            r#"{"ok":true,"value":[true],"toolCalls":[]}"#,
        ),
        (
            "string-search-test262.test: test/built-ins/String/prototype/endsWith/String.prototype.endsWith_Success_4.js",
            r#"return ["word".endsWith("r", 3)]"#,
            r#"{"ok":true,"value":[true],"toolCalls":[]}"#,
        ),
        (
            "string-search-test262.test: test/built-ins/String/prototype/endsWith/String.prototype.endsWith_Fail.js",
            r#"return ["word".endsWith("r")]"#,
            r#"{"ok":true,"value":[false],"toolCalls":[]}"#,
        ),
        (
            "string-search-test262.test: test/built-ins/String/prototype/endsWith/String.prototype.endsWith_Fail_2.js",
            r#"return ["word".endsWith("d", 3)]"#,
            r#"{"ok":true,"value":[false],"toolCalls":[]}"#,
        ),
        (
            "string-search-test262.test: test/built-ins/String/prototype/endsWith/searchstring-found-with-position.js",
            r#"const text = "The future is cool!"; return [text.endsWith("The future", 10), text.endsWith("future", 10), text.endsWith(" is cool!", text.length)]"#,
            r#"{"ok":true,"value":[true,true,true],"toolCalls":[]}"#,
        ),
        (
            "string-search-test262.test: test/built-ins/String/prototype/endsWith/searchstring-found-without-position.js",
            r#"const text = "The future is cool!"; return [text.endsWith("cool!"), text.endsWith("!"), text.endsWith(text)]"#,
            r#"{"ok":true,"value":[true,true,true],"toolCalls":[]}"#,
        ),
        (
            "string-search-test262.test: test/built-ins/String/prototype/endsWith/searchstring-not-found-with-position.js",
            r#"const text = "The future is cool!"; return [text.endsWith("is cool!", text.length - 1), text.endsWith("!", 1)]"#,
            r#"{"ok":true,"value":[false,false],"toolCalls":[]}"#,
        ),
        (
            "string-search-test262.test: test/built-ins/String/prototype/endsWith/searchstring-not-found-without-position.js",
            r#"const text = "The future is cool!"; return [text.endsWith("is Flash!"), text.endsWith("IS COOL!"), text.endsWith("The future")]"#,
            r#"{"ok":true,"value":[false,false,false],"toolCalls":[]}"#,
        ),
        (
            "string-search-test262.test: test/built-ins/String/prototype/endsWith/return-false-if-search-start-is-less-than-zero.js",
            r#"return ["web".endsWith("w", 0), "Bob".endsWith("  Bob")]"#,
            r#"{"ok":true,"value":[false,false],"toolCalls":[]}"#,
        ),
        (
            "string-search-test262.test: test/built-ins/String/prototype/endsWith/return-true-if-searchstring-is-empty.js",
            r#"const text = "The future is cool!"; return [
      text.endsWith(""), text.endsWith("", text.length), text.endsWith("", Infinity),
      text.endsWith("", -1), text.endsWith("", -Infinity),
    ]"#,
            r#"{"ok":true,"value":[true,true,true,true,true],"toolCalls":[]}"#,
        ),
        (
            "string-search-test262.test: test/built-ins/String/prototype/endsWith/coerced-values-of-position.js",
            r#"const text = "The future is cool!"; return [
      text.endsWith("", NaN), text.endsWith("", undefined), text.endsWith("The future", 10.4),
    ]"#,
            r#"{"ok":true,"value":[true,true,true],"toolCalls":[]}"#,
        ),
        (
            "string-search-test262.test: test/built-ins/String/prototype/indexOf/S15.5.4.7_A2_T1.js",
            r#"return ["abcd".indexOf("abcdab")]"#,
            r#"{"ok":true,"value":[-1],"toolCalls":[]}"#,
        ),
        (
            "string-search-test262.test: test/built-ins/String/prototype/indexOf/S15.5.4.7_A2_T2.js",
            r#"return ["abcd".indexOf("abcdab", 0)]"#,
            r#"{"ok":true,"value":[-1],"toolCalls":[]}"#,
        ),
        (
            "string-search-test262.test: test/built-ins/String/prototype/indexOf/S15.5.4.7_A2_T3.js",
            r#"return ["abcd".indexOf("abcdab", 99)]"#,
            r#"{"ok":true,"value":[-1],"toolCalls":[]}"#,
        ),
        (
            "string-search-test262.test: test/built-ins/String/prototype/indexOf/S15.5.4.7_A2_T4.js",
            r#"return ["abcd".indexOf("abcdab", NaN)]"#,
            r#"{"ok":true,"value":[-1],"toolCalls":[]}"#,
        ),
        (
            "string-search-test262.test: test/built-ins/String/prototype/indexOf/S15.5.4.7_A3_T1.js",
            r#"return ["$$abcdabcd".indexOf("ab", NaN)]"#,
            r#"{"ok":true,"value":[2],"toolCalls":[]}"#,
        ),
        (
            "string-search-test262.test: test/built-ins/String/prototype/indexOf/S15.5.4.7_A3_T3.js",
            r#"return ["$$abcdabcd".indexOf("ab", -Infinity)]"#,
            r#"{"ok":true,"value":[2],"toolCalls":[]}"#,
        ),
        (
            "string-search-test262.test: test/built-ins/String/prototype/indexOf/position-tointeger.js",
            r#"return [
      "aaaa".indexOf("aa", 0), "aaaa".indexOf("aa", 1), "aaaa".indexOf("aa", -0.9),
      "aaaa".indexOf("aa", 0.9), "aaaa".indexOf("aa", 1.9), "aaaa".indexOf("aa", NaN),
      "aaaa".indexOf("aa", Infinity), "aaaa".indexOf("aa", undefined),
      "aaaa".indexOf("aa", 2), "aaaa".indexOf("aa", 2.9),
    ]"#,
            r#"{"ok":true,"value":[0,1,0,0,1,0,-1,0,2,2],"toolCalls":[]}"#,
        ),
        (
            "string-search-test262.test: test/built-ins/String/prototype/indexOf/searchstring-tostring.js",
            r#"return ["foo".indexOf(""), "__foo__".indexOf("foo")]"#,
            r#"{"ok":true,"value":[0,2],"toolCalls":[]}"#,
        ),
        (
            "string-search-test262.test: test/built-ins/String/prototype/lastIndexOf/not-a-substring.js",
            r#"return ["abc".lastIndexOf("d")]"#,
            r#"{"ok":true,"value":[-1],"toolCalls":[]}"#,
        ),
    ]);
}

#[test]
fn string_regexp_test262() {
    check(&[
        (
            "string-regexp-test262.test: test/built-ins/String/prototype/split/separator-regexp.js",
            r#"
      return [
        "x".split(/^/), "x".split(/$/), "x".split(/.?/), "x".split(/.*/), "x".split(/.+/),
        "x".split(/.*?/), "x".split(/.{1}/), "x".split(/.{1,}/), "x".split(/.{1,2}/),
        "x".split(/()/), "x".split(/./), "x".split(/(?:)/), "x".split(/(...)/),
        "x".split(/(|)/), "x".split(/[]/), "x".split(/[^]/), "x".split(/[.-.]/),
        "x".split(/\0/), "x".split(/\b/), "x".split(/\B/), "x".split(/\d/),
        "x".split(/\D/), "x".split(/\n/), "x".split(/\r/), "x".split(/\s/),
        "x".split(/\S/), "x".split(/\v/), "x".split(/\w/), "x".split(/\W/),
      ]
    "#,
            r#"{"ok":true,"value":[["x"],["x"],["",""],["",""],["",""],["x"],["",""],["",""],["",""],["x"],["",""],["x"],["x"],["x"],["x"],["",""],["x"],["x"],["x"],["x"],["x"],["",""],["x"],["x"],["x"],["",""],["x"],["",""],["x"]],"toolCalls":[]}"#,
        ),
        (
            "string-regexp-test262.test: test/built-ins/String/prototype/split/arguments-are-regexp-s-and-3-and-instance-is-string-a-b-c-de-f.js",
            r#"return "a b c de f".split(/\s/, 3)"#,
            r#"{"ok":true,"value":["a","b","c"],"toolCalls":[]}"#,
        ),
        (
            "string-regexp-test262.test: test/built-ins/String/prototype/split/argument-is-regexp-s-and-instance-is-string-a-b-c-de-f.js",
            r#"return "a b c de f".split(/\s/)"#,
            r#"{"ok":true,"value":["a","b","c","de","f"],"toolCalls":[]}"#,
        ),
        (
            "string-regexp-test262.test: test/built-ins/String/prototype/split/argument-is-regexp-d-and-instance-is-string-dfe23iu-34-65.js",
            r#"return "dfe23iu 34 =+65--".split(/\d+/)"#,
            r#"{"ok":true,"value":["dfe","iu "," =+","--"],"toolCalls":[]}"#,
        ),
        (
            "string-regexp-test262.test: test/built-ins/String/prototype/split/argument-is-regexp-reg-exp-d-and-instance-is-string-dfe23iu-34-65.js",
            r#"return "dfe23iu 34 =+65--".split(new RegExp("\\d+"))"#,
            r#"{"ok":true,"value":["dfe","iu "," =+","--"],"toolCalls":[]}"#,
        ),
        (
            "string-regexp-test262.test: test/built-ins/String/prototype/split/argument-is-regexp-a-z-and-instance-is-string-abc.js",
            r#"return "abc".split(/[a-z]/)"#,
            r#"{"ok":true,"value":["","","",""],"toolCalls":[]}"#,
        ),
        (
            "string-regexp-test262.test: test/built-ins/String/prototype/split/argument-is-reg-exp-a-z-and-instance-is-string-abc.js",
            r#"return "abc".split(new RegExp("[a-z]"))"#,
            r#"{"ok":true,"value":["","","",""],"toolCalls":[]}"#,
        ),
        (
            "string-regexp-test262.test: test/built-ins/String/prototype/split/arguments-are-regexp-l-and-undefined-and-instance-is-string-hello.js",
            r#"return "hello".split(/l/, undefined)"#,
            r#"{"ok":true,"value":["he","","o"],"toolCalls":[]}"#,
        ),
        (
            "string-regexp-test262.test: test/built-ins/String/prototype/split/arguments-are-regexp-l-and-0-and-instance-is-string-hello.js",
            r#"return "hello".split(/l/, 0)"#,
            r#"{"ok":true,"value":[],"toolCalls":[]}"#,
        ),
        (
            "string-regexp-test262.test: test/built-ins/String/prototype/split/arguments-are-regexp-l-and-1-and-instance-is-string-hello.js",
            r#"return "hello".split(/l/, 1)"#,
            r#"{"ok":true,"value":["he"],"toolCalls":[]}"#,
        ),
        (
            "string-regexp-test262.test: test/built-ins/String/prototype/split/arguments-are-regexp-l-and-2-and-instance-is-string-hello.js",
            r#"return "hello".split(/l/, 2)"#,
            r#"{"ok":true,"value":["he",""],"toolCalls":[]}"#,
        ),
        (
            "string-regexp-test262.test: test/built-ins/String/prototype/split/arguments-are-regexp-l-and-3-and-instance-is-string-hello.js",
            r#"return "hello".split(/l/, 3)"#,
            r#"{"ok":true,"value":["he","","o"],"toolCalls":[]}"#,
        ),
        (
            "string-regexp-test262.test: test/built-ins/String/prototype/split/arguments-are-regexp-l-and-4-and-instance-is-string-hello.js",
            r#"return "hello".split(/l/, 4)"#,
            r#"{"ok":true,"value":["he","","o"],"toolCalls":[]}"#,
        ),
        (
            "string-regexp-test262.test: test/built-ins/String/prototype/split/argument-is-regexp-l-and-instance-is-string-hello.js",
            r#"return "hello".split(/l/)"#,
            r#"{"ok":true,"value":["he","","o"],"toolCalls":[]}"#,
        ),
        (
            "string-regexp-test262.test: test/built-ins/String/prototype/split/argument-is-new-reg-exp-and-instance-is-string-hello.js",
            r#"return "hello".split(new RegExp())"#,
            r#"{"ok":true,"value":["h","e","l","l","o"],"toolCalls":[]}"#,
        ),
        (
            "string-regexp-test262.test: test/built-ins/String/prototype/split/arguments-are-new-reg-exp-and-0-and-instance-is-string-hello.js",
            r#"return "hello".split(new RegExp(), 0)"#,
            r#"{"ok":true,"value":[],"toolCalls":[]}"#,
        ),
        (
            "string-regexp-test262.test: test/built-ins/String/prototype/split/arguments-are-new-reg-exp-and-1-and-instance-is-string-hello.js",
            r#"return "hello".split(new RegExp(), 1)"#,
            r#"{"ok":true,"value":["h"],"toolCalls":[]}"#,
        ),
        (
            "string-regexp-test262.test: test/built-ins/String/prototype/split/arguments-are-new-reg-exp-and-2-and-instance-is-string-hello.js",
            r#"return "hello".split(new RegExp(), 2)"#,
            r#"{"ok":true,"value":["h","e"],"toolCalls":[]}"#,
        ),
        (
            "string-regexp-test262.test: test/built-ins/String/prototype/split/arguments-are-new-reg-exp-and-3-and-instance-is-string-hello.js",
            r#"return "hello".split(new RegExp(), 3)"#,
            r#"{"ok":true,"value":["h","e","l"],"toolCalls":[]}"#,
        ),
        (
            "string-regexp-test262.test: test/built-ins/String/prototype/split/arguments-are-new-reg-exp-and-4-and-instance-is-string-hello.js",
            r#"return "hello".split(new RegExp(), 4)"#,
            r#"{"ok":true,"value":["h","e","l","l"],"toolCalls":[]}"#,
        ),
        (
            "string-regexp-test262.test: test/built-ins/String/prototype/split/arguments-are-new-reg-exp-and-undefined-and-instance-is-string-hello.js",
            r#"return "hello".split(new RegExp(), undefined)"#,
            r#"{"ok":true,"value":["h","e","l","l","o"],"toolCalls":[]}"#,
        ),
        (
            "string-regexp-test262.test: test/built-ins/String/prototype/split/call-split-2-instance-is-string-one-two-three-four-five.js",
            r#"return "one two three four five".split(/ /, 2)"#,
            r#"{"ok":true,"value":["one","two"],"toolCalls":[]}"#,
        ),
        (
            "string-regexp-test262.test: test/built-ins/String/prototype/split/separator-regexp-comma-instance-is-string-one-1-two-2-four-4.js",
            r#"return "one-1,two-2,four-4".split(/,/)"#,
            r#"{"ok":true,"value":["one-1","two-2","four-4"],"toolCalls":[]}"#,
        ),
        (
            "string-regexp-test262.test: test/built-ins/String/prototype/split/argument-is-regexp-x-and-instance-is-string-a-b-c-de-f.js",
            r#"return "a b c de f".split(/X/)"#,
            r#"{"ok":true,"value":["a b c de f"],"toolCalls":[]}"#,
        ),
        (
            "string-regexp-test262.test: test/built-ins/String/prototype/replace/regexp-capture-by-index.js",
            r#"
      const str = "foo-x-bar"
      const patterns = ["x", /x/, /(x)/, /(x)($^)?/, /((((((((((x))))))))))/]
      const replacements = ["|$0|", "|$00|", "|$000|", "|$1|", "|$01|", "|$010|", "|$2|", "|$02|", "|$020|", "|$10|", "|$100|", "|$20|", "|$200|"]
      return replacements.flatMap((replacement) => patterns.map((pattern) => str.replace(pattern, replacement)))
    "#,
            r#"{"ok":true,"value":["foo-|$0|-bar","foo-|$0|-bar","foo-|$0|-bar","foo-|$0|-bar","foo-|$0|-bar","foo-|$00|-bar","foo-|$00|-bar","foo-|$00|-bar","foo-|$00|-bar","foo-|$00|-bar","foo-|$000|-bar","foo-|$000|-bar","foo-|$000|-bar","foo-|$000|-bar","foo-|$000|-bar","foo-|$1|-bar","foo-|$1|-bar","foo-|x|-bar","foo-|x|-bar","foo-|x|-bar","foo-|$01|-bar","foo-|$01|-bar","foo-|x|-bar","foo-|x|-bar","foo-|x|-bar","foo-|$010|-bar","foo-|$010|-bar","foo-|x0|-bar","foo-|x0|-bar","foo-|x0|-bar","foo-|$2|-bar","foo-|$2|-bar","foo-|$2|-bar","foo-||-bar","foo-|x|-bar","foo-|$02|-bar","foo-|$02|-bar","foo-|$02|-bar","foo-||-bar","foo-|x|-bar","foo-|$020|-bar","foo-|$020|-bar","foo-|$020|-bar","foo-|0|-bar","foo-|x0|-bar","foo-|$10|-bar","foo-|$10|-bar","foo-|x0|-bar","foo-|x0|-bar","foo-|x|-bar","foo-|$100|-bar","foo-|$100|-bar","foo-|x00|-bar","foo-|x00|-bar","foo-|x0|-bar","foo-|$20|-bar","foo-|$20|-bar","foo-|$20|-bar","foo-|0|-bar","foo-|x0|-bar","foo-|$200|-bar","foo-|$200|-bar","foo-|$200|-bar","foo-|00|-bar","foo-|x00|-bar"],"toolCalls":[]}"#,
        ),
        (
            "string-regexp-test262.test: test/built-ins/String/prototype/replace/S15.5.4.11_A1_T17.js",
            r#"return "asdf".replace(new RegExp(undefined, "g"), "1")"#,
            r#"{"ok":true,"value":"1a1s1d1f1","toolCalls":[]}"#,
        ),
        (
            "string-regexp-test262.test: test/built-ins/String/prototype/replace/S15.5.4.11_A2_T1.js",
            r#"return "She sells seashells by the seashore.".replace(/sh/g, "sch")"#,
            r#"{"ok":true,"value":"She sells seaschells by the seaschore.","toolCalls":[]}"#,
        ),
        (
            "string-regexp-test262.test: test/built-ins/String/prototype/replace/S15.5.4.11_A2_T2.js",
            r#"return "She sells seashells by the seashore.".replace(/sh/g, "$$sch")"#,
            r#"{"ok":true,"value":"She sells sea$schells by the sea$schore.","toolCalls":[]}"#,
        ),
        (
            "string-regexp-test262.test: test/built-ins/String/prototype/replace/S15.5.4.11_A2_T3.js",
            r#"return "She sells seashells by the seashore.".replace(/sh/g, "$&sch")"#,
            r#"{"ok":true,"value":"She sells seashschells by the seashschore.","toolCalls":[]}"#,
        ),
        (
            "string-regexp-test262.test: test/built-ins/String/prototype/replace/S15.5.4.11_A2_T4.js",
            r#"return "She sells seashells by the seashore.".replace(/sh/g, "$`sch")"#,
            r#"{"ok":true,"value":"She sells seaShe sells seaschells by the seaShe sells seashells by the seaschore.","toolCalls":[]}"#,
        ),
        (
            "string-regexp-test262.test: test/built-ins/String/prototype/replace/S15.5.4.11_A2_T5.js",
            r#"return "She sells seashells by the seashore.".replace(/sh/g, "$'sch")"#,
            r#"{"ok":true,"value":"She sells seaells by the seashore.schells by the seaore.schore.","toolCalls":[]}"#,
        ),
        (
            "string-regexp-test262.test: test/built-ins/String/prototype/replace/S15.5.4.11_A2_T6.js",
            r#"return "She sells seashells by the seashore.".replace(/sh/, "sch")"#,
            r#"{"ok":true,"value":"She sells seaschells by the seashore.","toolCalls":[]}"#,
        ),
        (
            "string-regexp-test262.test: test/built-ins/String/prototype/replace/S15.5.4.11_A2_T7.js",
            r#"return "She sells seashells by the seashore.".replace(/sh/, "$$sch")"#,
            r#"{"ok":true,"value":"She sells sea$schells by the seashore.","toolCalls":[]}"#,
        ),
        (
            "string-regexp-test262.test: test/built-ins/String/prototype/replace/S15.5.4.11_A2_T8.js",
            r#"return "She sells seashells by the seashore.".replace(/sh/, "$&sch")"#,
            r#"{"ok":true,"value":"She sells seashschells by the seashore.","toolCalls":[]}"#,
        ),
        (
            "string-regexp-test262.test: test/built-ins/String/prototype/replace/S15.5.4.11_A2_T9.js",
            r#"return "She sells seashells by the seashore.".replace(/sh/, "$`sch")"#,
            r#"{"ok":true,"value":"She sells seaShe sells seaschells by the seashore.","toolCalls":[]}"#,
        ),
        (
            "string-regexp-test262.test: test/built-ins/String/prototype/replace/S15.5.4.11_A2_T10.js",
            r#"return "She sells seashells by the seashore.".replace(/sh/, "$'sch")"#,
            r#"{"ok":true,"value":"She sells seaells by the seashore.schells by the seashore.","toolCalls":[]}"#,
        ),
        (
            "string-regexp-test262.test: test/built-ins/String/prototype/replace/S15.5.4.11_A3_T1.js",
            r#"return "uid=31".replace(/(uid=)(\d+)/, "$1115")"#,
            r#"{"ok":true,"value":"uid=115","toolCalls":[]}"#,
        ),
        (
            "string-regexp-test262.test: test/built-ins/String/prototype/replace/S15.5.4.11_A3_T3.js",
            r#"return "uid=31".replace(/(uid=)(\d+)/, "$11A15")"#,
            r#"{"ok":true,"value":"uid=1A15","toolCalls":[]}"#,
        ),
        (
            "string-regexp-test262.test: test/built-ins/String/prototype/replace/S15.5.4.11_A5_T1.js",
            r#"return "aaaaaaaaaa,aaaaaaaaaaaaaaa".replace(/^(a+)\1*,\1+$/, "$1")"#,
            r#"{"ok":true,"value":"aaaaa","toolCalls":[]}"#,
        ),
        (
            "string-regexp-test262.test: test/built-ins/String/prototype/replaceAll/searchValue-replacer-RegExp-call.js",
            r#"
      return [
        "abc abc abc".replaceAll(new RegExp("b", "g"), "z"),
        "abc abc abc".replaceAll(new RegExp("b", "gy"), "z"),
        "abc abc abc".replaceAll(new RegExp("b", "giy"), "z"),
        "No Uppercase!".replaceAll(new RegExp("[A-Z]", "g"), ""),
        "No Uppercase?".replaceAll(new RegExp("[A-Z]", "gy"), ""),
        "NO UPPERCASE!".replaceAll(new RegExp("[A-Z]", "gy"), ""),
        "abcabcabcabc".replaceAll(new RegExp("a(b)(ca)", "g"), "$2-$1"),
        "abcabcabcabc".replaceAll(new RegExp("(a(.))", "g"), "$1$2$3"),
        "aabacadaeafagahaiajakalamano a azaya".replaceAll(new RegExp("(((((((((((((a(.).).).).).).).).))))))", "g"), "($10)-($12)-($1)"),
        "abcba".replaceAll(new RegExp("b", "g"), "$'"),
        "abcba".replaceAll(new RegExp("b", "g"), "$`"),
        "abcba".replaceAll(new RegExp("(?<named>b)", "g"), "($<named>)"),
        "abcba".replaceAll(new RegExp("(?<named>b)", "g"), "($<named)"),
        "abcba".replaceAll(new RegExp("(?<named>b)", "g"), "($<unnamed>)"),
        "abcabcabcabc".replaceAll(new RegExp("a(b)(ca)", "g"), "($$)"),
        "abcabcabcabc".replaceAll(new RegExp("a(b)(ca)", "g"), "($)"),
        "abcabcabcabc".replaceAll(new RegExp("a(b)(ca)", "g"), "($$$$)"),
        "abcabcabcabc".replaceAll(new RegExp("a(b)(ca)", "g"), "($$$)"),
        "abcabcabcabc".replaceAll(new RegExp("a(b)(ca)", "g"), "($$&)"),
        "abcabcabcabc".replaceAll(new RegExp("a(b)(ca)", "g"), "($$1)"),
        "abcabcabcabc".replaceAll(new RegExp("a(b)(ca)", "g"), "($$`)"),
        "abcabcabcabc".replaceAll(new RegExp("a(b)(ca)", "g"), "($$')"),
        "abcabcabcabc".replaceAll(new RegExp("a(?<z>b)(ca)", "g"), "($$<z>)"),
        "abcabcabcabc".replaceAll(new RegExp("a(b)(ca)", "g"), "($&)"),
      ]
    "#,
            r#"{"ok":true,"value":["azc azc azc","abc abc abc","abc abc abc","o ppercase!","o Uppercase?"," UPPERCASE!","ca-bbcca-bbc","abb$3cabb$3cabb$3cabb$3c","(aabaca)-(aaba)-(aabacadaea)f(agahai)-(agah)-(agahaiajak)(alaman)-(alam)-(alamano a )azaya","acbacaa","aacabca","a(b)c(b)a","a($<named)c($<named)a","a()c()a","($)bc($)bc","($)bc($)bc","($$)bc($$)bc","($$)bc($$)bc","($&)bc($&)bc","($1)bc($1)bc","($`)bc($`)bc","($')bc($')bc","($<z>)bc($<z>)bc","(abca)bc(abca)bc"],"toolCalls":[]}"#,
        ),
        (
            "string-regexp-test262.test: test/built-ins/String/prototype/replaceAll/searchValue-empty-string.js",
            r#"return ["aab c  \nx".replaceAll("", "_"), "a".replaceAll("", "_")]"#,
            r#"{"ok":true,"value":["_a_a_b_ _c_ _ _\n_x_","_a_"],"toolCalls":[]}"#,
        ),
        (
            "string-regexp-test262.test: test/built-ins/String/prototype/replaceAll/searchValue-empty-string-this-empty-string.js",
            r#"return "".replaceAll("", "abc")"#,
            r#"{"ok":true,"value":"abc","toolCalls":[]}"#,
        ),
        (
            "string-regexp-test262.test: test/built-ins/String/prototype/replaceAll/replaceValue-value-replaces-string.js",
            r#"return ["aaab a a aac".replaceAll("aa", "z"), "aaab a a aac".replaceAll("aa", "a"), "aaab a a aac".replaceAll("a", "a"), "aaab a a aac".replaceAll("a", "z")]"#,
            r#"{"ok":true,"value":["zab a a zc","aab a a ac","aaab a a aac","zzzb z z zzc"],"toolCalls":[]}"#,
        ),
        (
            "string-regexp-test262.test: test/built-ins/String/prototype/replaceAll/getSubstitution-0x0024.js",
            r#"
      const str = "Ninguém é igual a ninguém. Todo o ser humano é um estranho ímpar."
      return [str.replaceAll("ninguém", "$"), str.replaceAll("é", "$"), str.replaceAll("é", "$ -"), str.replaceAll("é", "$$$")]
    "#,
            r#"{"ok":true,"value":["Ninguém é igual a $. Todo o ser humano é um estranho ímpar.","Ningu$m $ igual a ningu$m. Todo o ser humano $ um estranho ímpar.","Ningu$ -m $ - igual a ningu$ -m. Todo o ser humano $ - um estranho ímpar.","Ningu$$m $$ igual a ningu$$m. Todo o ser humano $$ um estranho ímpar."],"toolCalls":[]}"#,
        ),
        (
            "string-regexp-test262.test: test/built-ins/String/prototype/replaceAll/getSubstitution-0x0024-0x0024.js",
            r#"
      const str = "Ninguém é igual a ninguém. Todo o ser humano é um estranho ímpar."
      return [str.replaceAll("ninguém", "$$"), str.replaceAll("é", "$$"), str.replaceAll("é", "$$ -"), str.replaceAll("é", "$$&"), str.replaceAll("é", "$$$"), str.replaceAll("é", "$$$$")]
    "#,
            r#"{"ok":true,"value":["Ninguém é igual a $. Todo o ser humano é um estranho ímpar.","Ningu$m $ igual a ningu$m. Todo o ser humano $ um estranho ímpar.","Ningu$ -m $ - igual a ningu$ -m. Todo o ser humano $ - um estranho ímpar.","Ningu$&m $& igual a ningu$&m. Todo o ser humano $& um estranho ímpar.","Ningu$$m $$ igual a ningu$$m. Todo o ser humano $$ um estranho ímpar.","Ningu$$m $$ igual a ningu$$m. Todo o ser humano $$ um estranho ímpar."],"toolCalls":[]}"#,
        ),
        (
            "string-regexp-test262.test: test/built-ins/String/prototype/replaceAll/getSubstitution-0x0024-0x0026.js",
            r#"
      const str = "Ninguém é igual a ninguém. Todo o ser humano é um estranho ímpar."
      return [str.replaceAll("ninguém", "$&"), str.replaceAll("ninguém", "($&)"), str.replaceAll("é", "($&)"), str.replaceAll("é", "($&) $&")]
    "#,
            r#"{"ok":true,"value":["Ninguém é igual a ninguém. Todo o ser humano é um estranho ímpar.","Ninguém é igual a (ninguém). Todo o ser humano é um estranho ímpar.","Ningu(é)m (é) igual a ningu(é)m. Todo o ser humano (é) um estranho ímpar.","Ningu(é) ém (é) é igual a ningu(é) ém. Todo o ser humano (é) é um estranho ímpar."],"toolCalls":[]}"#,
        ),
        (
            "string-regexp-test262.test: test/built-ins/String/prototype/replaceAll/getSubstitution-0x0024-0x0060.js",
            r#"
      const str = "Ninguém é igual a ninguém. Todo o ser humano é um estranho ímpar."
      return [str.replaceAll("ninguém", "$`"), str.replaceAll("Ninguém", "$`"), str.replaceAll("ninguém", "($`)"), str.replaceAll("é", "($`)")]
    "#,
            r#"{"ok":true,"value":["Ninguém é igual a Ninguém é igual a . Todo o ser humano é um estranho ímpar."," é igual a ninguém. Todo o ser humano é um estranho ímpar.","Ninguém é igual a (Ninguém é igual a ). Todo o ser humano é um estranho ímpar.","Ningu(Ningu)m (Ninguém ) igual a ningu(Ninguém é igual a ningu)m. Todo o ser humano (Ninguém é igual a ninguém. Todo o ser humano ) um estranho ímpar."],"toolCalls":[]}"#,
        ),
        (
            "string-regexp-test262.test: test/built-ins/String/prototype/replaceAll/getSubstitution-0x0024-0x0027.js",
            r#"
      const str = "Ninguém é igual a ninguém. Todo o ser humano é um estranho ímpar."
      return [str.replaceAll("ninguém", "$'"), str.replaceAll(".", "--- $'"), str.replaceAll("é", "($')")]
    "#,
            r#"{"ok":true,"value":["Ninguém é igual a . Todo o ser humano é um estranho ímpar.. Todo o ser humano é um estranho ímpar.","Ninguém é igual a ninguém---  Todo o ser humano é um estranho ímpar. Todo o ser humano é um estranho ímpar--- ","Ningu(m é igual a ninguém. Todo o ser humano é um estranho ímpar.)m ( igual a ninguém. Todo o ser humano é um estranho ímpar.) igual a ningu(m. Todo o ser humano é um estranho ímpar.)m. Todo o ser humano ( um estranho ímpar.) um estranho ímpar."],"toolCalls":[]}"#,
        ),
        (
            "string-regexp-test262.test: test/built-ins/String/prototype/replaceAll/getSubstitution-0x0024N.js",
            r#"
      const str = "ABC AAA ABC AAA"
      return ["$1", "$2", "$3", "$4", "$5", "$6", "$7", "$8", "$9"].map((replacement) => str.replaceAll("ABC", replacement))
    "#,
            r#"{"ok":true,"value":["$1 AAA $1 AAA","$2 AAA $2 AAA","$3 AAA $3 AAA","$4 AAA $4 AAA","$5 AAA $5 AAA","$6 AAA $6 AAA","$7 AAA $7 AAA","$8 AAA $8 AAA","$9 AAA $9 AAA"],"toolCalls":[]}"#,
        ),
        (
            "string-regexp-test262.test: test/built-ins/String/prototype/replaceAll/getSubstitution-0x0024NN.js",
            r#"
      const str = "aaaaaaaaaaaaaaaa aaaaaaaa aaaaaaaaaaaaaaaa"
      return [str.replaceAll("a", "$11"), str.replaceAll("a", "$29")]
    "#,
            r#"{"ok":true,"value":["$11$11$11$11$11$11$11$11$11$11$11$11$11$11$11$11 $11$11$11$11$11$11$11$11 $11$11$11$11$11$11$11$11$11$11$11$11$11$11$11$11","$29$29$29$29$29$29$29$29$29$29$29$29$29$29$29$29 $29$29$29$29$29$29$29$29 $29$29$29$29$29$29$29$29$29$29$29$29$29$29$29$29"],"toolCalls":[]}"#,
        ),
        (
            "string-regexp-test262.test: test/built-ins/String/prototype/replaceAll/getSubstitution-0x0024-0x003C.js",
            r#"return "aaaaaaaaaaaaaaaa aaaaaaaa aaaaaaaaaaaaaaaa".replaceAll("a", "$<")"#,
            r#"{"ok":true,"value":"$<$<$<$<$<$<$<$<$<$<$<$<$<$<$<$< $<$<$<$<$<$<$<$< $<$<$<$<$<$<$<$<$<$<$<$<$<$<$<$<","toolCalls":[]}"#,
        ),
        (
            "string-regexp-test262.test: test/built-ins/String/prototype/match/S15.5.4.10_A1_T14.js",
            r#"const match = "ABBABABAB77BBAA".match(new RegExp("77")); return [match[0], match.index]"#,
            r#"{"ok":true,"value":["77",9],"toolCalls":[]}"#,
        ),
        (
            "string-regexp-test262.test: test/built-ins/String/prototype/match/S15.5.4.10_A2_T2.js",
            r#"return "343443444".match(/34/g)"#,
            r#"{"ok":true,"value":["34","34","34"],"toolCalls":[]}"#,
        ),
        (
            "string-regexp-test262.test: test/built-ins/String/prototype/match/S15.5.4.10_A2_T3.js",
            r#"return "123456abcde7890".match(/\d{1}/g)"#,
            r#"{"ok":true,"value":["1","2","3","4","5","6","7","8","9","0"],"toolCalls":[]}"#,
        ),
        (
            "string-regexp-test262.test: test/built-ins/String/prototype/match/S15.5.4.10_A2_T4.js",
            r#"return "123456abcde7890".match(/\d{2}/g)"#,
            r#"{"ok":true,"value":["12","34","56","78","90"],"toolCalls":[]}"#,
        ),
        (
            "string-regexp-test262.test: test/built-ins/String/prototype/match/S15.5.4.10_A2_T5.js",
            r#"return "123456abcde7890".match(/\D{2}/g)"#,
            r#"{"ok":true,"value":["ab","cd"],"toolCalls":[]}"#,
        ),
        (
            "string-regexp-test262.test: test/built-ins/String/prototype/match/S15.5.4.10_A2_T6.js",
            r#"const match = "Boston, Mass. 02134".match(/([\d]{5})([- ]?[\d]{4})?$/); return [match[0], match[1], match[2] === undefined, match.length, match.index]"#,
            r#"{"ok":true,"value":["02134","02134",true,3,14],"toolCalls":[]}"#,
        ),
        (
            "string-regexp-test262.test: test/built-ins/String/prototype/match/S15.5.4.10_A2_T7.js",
            r#"return "Boston, Mass. 02134".match(/([\d]{5})([- ]?[\d]{4})?$/g)"#,
            r#"{"ok":true,"value":["02134"],"toolCalls":[]}"#,
        ),
        (
            "string-regexp-test262.test: test/built-ins/String/prototype/match/S15.5.4.10_A2_T8.js",
            r#"const match = "Boston, MA 02134".match(/([\d]{5})([- ]?[\d]{4})?$/); return [match[0], match[1], match[2] === undefined, match.length, match.index]"#,
            r#"{"ok":true,"value":["02134","02134",true,3,11],"toolCalls":[]}"#,
        ),
        (
            "string-regexp-test262.test: test/built-ins/String/prototype/match/S15.5.4.10_A2_T12.js",
            r#"return "Boston, MA 02134".match(/([\d]{5})([- ]?[\d]{4})?$/g)"#,
            r#"{"ok":true,"value":["02134"],"toolCalls":[]}"#,
        ),
        (
            "string-regexp-test262.test: test/built-ins/String/prototype/matchAll/regexp-prototype-matchAll-v-u-flag.js",
            r#"
      const text = "𠮷a𠮷b𠮷"
      const collect = (regex) => {
        const matches = text.matchAll(regex)
        return matches.map((match) => match[0]).concat(matches.map((match) => match.index))
      }
      const empty = text.matchAll(/(?:)/gu)
      const complex = "a𠮷b􏿿c".matchAll(/\P{ASCII}/gu)
      return [
        collect(/𠮷/g),
        collect(/𠮷/gu),
        collect(/\p{Script=Han}/gu),
        collect(/./gu),
        empty.map((match) => match[0]).concat(empty.map((match) => match.index)).length,
        complex.map((match) => match[0]),
      ]
    "#,
            r#"{"ok":true,"value":[["𠮷","𠮷","𠮷",0,3,6],["𠮷","𠮷","𠮷",0,3,6],["𠮷","𠮷","𠮷",0,3,6],["𠮷","a","𠮷","b","𠮷",0,2,3,5,6],12,["𠮷","􏿿"]],"toolCalls":[]}"#,
        ),
        (
            "string-regexp-test262.test: test/built-ins/String/prototype/search/S15.5.4.12_A1_T14.js",
            r#"return "ABBABABAB77BBAA".search(new RegExp("77"))"#,
            r#"{"ok":true,"value":9,"toolCalls":[]}"#,
        ),
        (
            "string-regexp-test262.test: test/built-ins/String/prototype/search/S15.5.4.12_A2_T1.js",
            r#"return "test string".search("string")"#,
            r#"{"ok":true,"value":5,"toolCalls":[]}"#,
        ),
        (
            "string-regexp-test262.test: test/built-ins/String/prototype/search/S15.5.4.12_A2_T2.js",
            r#"return "test string".search("String")"#,
            r#"{"ok":true,"value":-1,"toolCalls":[]}"#,
        ),
        (
            "string-regexp-test262.test: test/built-ins/String/prototype/search/S15.5.4.12_A2_T3.js",
            r#"return "test string".search(/String/i)"#,
            r#"{"ok":true,"value":5,"toolCalls":[]}"#,
        ),
        (
            "string-regexp-test262.test: test/built-ins/String/prototype/search/S15.5.4.12_A2_T4.js",
            r#"return "one two three four five".search(/Four/)"#,
            r#"{"ok":true,"value":-1,"toolCalls":[]}"#,
        ),
        (
            "string-regexp-test262.test: test/built-ins/String/prototype/search/S15.5.4.12_A2_T5.js",
            r#"return "one two three four five".search(/four/)"#,
            r#"{"ok":true,"value":14,"toolCalls":[]}"#,
        ),
        (
            "string-regexp-test262.test: test/built-ins/String/prototype/search/S15.5.4.12_A2_T6.js",
            r#"return "test string".search("notexist")"#,
            r#"{"ok":true,"value":-1,"toolCalls":[]}"#,
        ),
        (
            "string-regexp-test262.test: test/built-ins/String/prototype/search/S15.5.4.12_A2_T7.js",
            r#"return "test string probe".search("string pro")"#,
            r#"{"ok":true,"value":5,"toolCalls":[]}"#,
        ),
        (
            "string-regexp-test262.test: test/built-ins/String/prototype/search/S15.5.4.12_A3_T1.js",
            r#"const text = "power of the power of the power of the great sword"; return [text.search(/the/), text.search(/the/g)]"#,
            r#"{"ok":true,"value":[9,9],"toolCalls":[]}"#,
        ),
        (
            "string-regexp-test262.test: test/built-ins/String/prototype/search/S15.5.4.12_A3_T2.js",
            r#"const text = "power of the power of the power of the great sword"; return [text.search(/of/), text.search(/of/g)]"#,
            r#"{"ok":true,"value":[6,6],"toolCalls":[]}"#,
        ),
    ]);
}

#[test]
fn string_date_methods_test262() {
    check(&[
        (
            "string-date-methods-test262.test: test/annexB/built-ins/String/prototype/substr/length-falsey.js",
            r#"
        return [false, NaN, "", null].flatMap((length) => [0, 1, 2, 3].map((start) => "abc".substr(start, length)))
      "#,
            r#"{"ok":true,"value":["","","","","","","","","","","","","","","",""],"toolCalls":[]}"#,
        ),
        (
            "string-date-methods-test262.test: test/annexB/built-ins/String/prototype/substr/length-negative.js",
            r#"
        return [0, 1, 2, 3].flatMap((start) => [-1, -2, -3, -4].map((length) => "abc".substr(start, length)))
      "#,
            r#"{"ok":true,"value":["","","","","","","","","","","","","","","",""],"toolCalls":[]}"#,
        ),
        (
            "string-date-methods-test262.test: test/annexB/built-ins/String/prototype/substr/length-positive.js",
            r#"
        return [0, 1, 2, 3].map((start) => [1, 2, 3, 4].map((length) => "abc".substr(start, length)))
      "#,
            r#"{"ok":true,"value":[["a","ab","abc","abc"],["b","bc","bc","bc"],["c","c","c","c"],["","","",""]],"toolCalls":[]}"#,
        ),
        (
            "string-date-methods-test262.test: test/annexB/built-ins/String/prototype/substr/length-undef.js",
            r#"
        return [
          "abc".substr(0), "abc".substr(1), "abc".substr(2), "abc".substr(3),
          "abc".substr(0, undefined), "abc".substr(1, undefined), "abc".substr(2, undefined), "abc".substr(3, undefined),
        ]
      "#,
            r#"{"ok":true,"value":["abc","bc","c","","abc","bc","c",""],"toolCalls":[]}"#,
        ),
        (
            "string-date-methods-test262.test: test/annexB/built-ins/String/prototype/substr/start-negative.js",
            r#"
        return ["abc".substr(-1), "abc".substr(-2), "abc".substr(-3), "abc".substr(-4), "abc".substr(-1.1)]
      "#,
            r#"{"ok":true,"value":["c","bc","abc","abc","c"],"toolCalls":[]}"#,
        ),
        (
            "string-date-methods-test262.test: test/built-ins/String/prototype/isWellFormed/returns-boolean.js",
            r#"
        const leadingPoo = "\uD83D"
        const trailingPoo = "\uDCA9"
        const wholePoo = leadingPoo + trailingPoo
        return [
          typeof "".isWellFormed,
          ("a" + leadingPoo + "c" + leadingPoo + "e").isWellFormed(),
          ("a" + trailingPoo + "c" + trailingPoo + "e").isWellFormed(),
          ("a" + trailingPoo + leadingPoo + "d").isWellFormed(),
          "a💩c".isWellFormed(),
          "a\uD83D\uDCA9c".isWellFormed(),
          ("a" + leadingPoo + trailingPoo + "d").isWellFormed(),
          wholePoo.slice(0, 1).isWellFormed(),
          wholePoo.slice(1).isWellFormed(),
          "abc".isWellFormed(),
          "a\u25A8c".isWellFormed(),
        ]
      "#,
            r#"{"ok":true,"value":["function",false,false,false,true,true,true,false,false,true,true],"toolCalls":[]}"#,
        ),
        (
            "string-date-methods-test262.test: test/built-ins/String/prototype/toWellFormed/returns-well-formed-string.js",
            r#"
        const replacementChar = "\uFFFD"
        const leadingPoo = "\uD83D"
        const trailingPoo = "\uDCA9"
        const wholePoo = leadingPoo + trailingPoo
        return [
          typeof "".toWellFormed,
          ("a" + leadingPoo + "c" + leadingPoo + "e").toWellFormed() === "a" + replacementChar + "c" + replacementChar + "e",
          ("a" + trailingPoo + "c" + trailingPoo + "e").toWellFormed() === "a" + replacementChar + "c" + replacementChar + "e",
          ("a" + trailingPoo + leadingPoo + "d").toWellFormed() === "a" + replacementChar + replacementChar + "d",
          "a💩c".toWellFormed() === "a💩c",
          "a\uD83D\uDCA9c".toWellFormed() === "a\uD83D\uDCA9c",
          ("a" + leadingPoo + trailingPoo + "d").toWellFormed() === "a" + wholePoo + "d",
          wholePoo.slice(0, 1).toWellFormed() === replacementChar,
          wholePoo.slice(1).toWellFormed() === replacementChar,
          "abc".toWellFormed() === "abc",
          "a\u25A8c".toWellFormed() === "a\u25A8c",
        ]
      "#,
            r#"{"ok":true,"value":["function",true,true,true,true,true,true,true,true,true,true],"toolCalls":[]}"#,
        ),
    ]);
}

#[test]
fn regexp_math_test262() {
    check(&[
        (
            "regexp-math-test262.test: RegExp.escape encodes syntax, punctuators, whitespace, and surrogates",
            r#"
        return [
          RegExp.escape("1+1"),
          RegExp.escape("foo.bar"),
          RegExp.escape(".*+?^$|()[]{}"),
          RegExp.escape(",-=#&!%:;@~'"),
          RegExp.escape("\t\n\v\f\r"),
          RegExp.escape("\uFEFF \u00A0\u202F"),
          RegExp.escape("\u2028\u2029"),
          RegExp.escape("\uD800"),
          RegExp.escape("\uDFFF"),
        ]
      "#,
            r#"{"ok":true,"value":["\\x31\\+1","\\x66oo\\.bar","\\.\\*\\+\\?\\^\\$\\|\\(\\)\\[\\]\\{\\}","\\x2c\\x2d\\x3d\\x23\\x26\\x21\\x25\\x3a\\x3b\\x40\\x7e\\x27","\\t\\n\\v\\f\\r","\\ufeff\\x20\\xa0\\u202f","\\u2028\\u2029","\\ud800","\\udfff"],"toolCalls":[]}"#,
        ),
        (
            "regexp-math-test262.test: RegExp.escape rejects non-string inputs",
            r#"
        const rejects = (input) => {
          try {
            RegExp.escape(input)
            return false
          } catch (error) {
            return error.name === "TypeError"
          }
        }
        return [rejects(123), rejects({}), rejects([]), rejects(null), rejects(undefined)]
      "#,
            r#"{"ok":true,"value":[true,true,true,true,true],"toolCalls":[]}"#,
        ),
        (
            "regexp-math-test262.test: RegExp.escape preserves ordinary ASCII and Unicode code points",
            r#"
        return [
          RegExp.escape(""),
          RegExp.escape(".a1b2c3D4E5F6"),
          RegExp.escape("_a_1_2"),
          RegExp.escape("hello_world"),
          RegExp.escape("//"),
          RegExp.escape("\u4F60\u597D"),
          RegExp.escape("\u0393\u03B5\u03B9\u03AC \u03C3\u03BF\u03C5"),
          RegExp.escape("\uD835\uDC01"),
        ]
      "#,
            r#"{"ok":true,"value":["","\\.a1b2c3D4E5F6","_a_1_2","\\x68ello_world","\\/\\/","你好","Γειά\\x20σου","𝐁"],"toolCalls":[]}"#,
        ),
        (
            "regexp-math-test262.test: d flag exposes match indices through exec, match, and matchAll",
            r#"
        const match = /(?<a>a)(b)?/d.exec("a")
        const stringMatch = "a".match(/a/d)
        const all = "a a".matchAll(/a/dg)
        const named = /(?<constructor>a)(?<safe>b)/d.exec("ab")
        return [
          /./.hasIndices,
          /./d.hasIndices,
          new RegExp(".", "d").hasIndices,
          match.indices[0],
          match.indices[1],
          match.indices[2],
          match.indices.groups.a,
          stringMatch.indices[0],
          all[0].indices[0],
          all[1].indices[0],
          Object.keys(named.indices.groups),
        ]
      "#,
            r#"{"ok":true,"value":[false,true,true,[0,1],[0,1],null,[0,1],[0,1],[0,1],[2,3],["constructor","safe"]],"toolCalls":[]}"#,
        ),
        (
            "regexp-math-test262.test: match indices preserve captures, Unicode offsets, and groups properties",
            r#"
        const plain = /a/.exec("a")
        const captured = "abcd".match(/b(c)/d)
        const unmatched = "bab".match(/(\w\w)(\W)?/d)
        const nonUnicode = "\uD835\uDC01".match(/./d)
        const unicode = "\uD835\uDC01".match(/./du)
        const noGroups = /./d.exec("a").indices
        return [
          Object.hasOwn(plain, "indices"),
          captured.indices,
          unmatched.indices,
          nonUnicode.indices[0],
          unicode.indices[0],
          Object.hasOwn(noGroups, "groups"),
          noGroups.groups,
        ]
      "#,
            r#"{"ok":true,"value":[false,[[1,3],[2,3]],[[0,2],[0,2],null],[0,1],[0,2],true,null],"toolCalls":[]}"#,
        ),
        (
            "regexp-math-test262.test: match and matchAll preserve named, unmatched, and prototype-named index groups",
            r#"
        const matched = "a".match(/(?<a>a)|(?<x>x)/d).indices.groups
        const all = "a x".matchAll(/(?<a>a)|(?<x>x)/dg)
        const namedMatch = "ab".match(/(?<constructor>a)(?<safe>b)/d).indices.groups
        const namedAll = "ab".matchAll(/(?<constructor>a)(?<safe>b)/dg)[0].indices.groups
        return [
          matched.a,
          matched.x,
          all[0].indices.groups.a,
          all[0].indices.groups.x,
          all[1].indices.groups.a,
          all[1].indices.groups.x,
          Object.keys(namedMatch),
          Object.keys(namedAll),
        ]
      "#,
            r#"{"ok":true,"value":[[0,1],null,[0,1],null,null,[2,3],["constructor","safe"],["constructor","safe"]],"toolCalls":[]}"#,
        ),
        (
            "regexp-math-test262.test: v flag exposes unicodeSets and remains exclusive with u",
            r#"
        const pattern = new RegExp("[a&&a]", "v")
        let rejectsUV = false
        try {
          new RegExp(".", "uv")
        } catch (error) {
          rejectsUV = error.name === "SyntaxError"
        }
        return [pattern.unicodeSets, pattern.unicode, pattern.flags, pattern.test("a"), pattern.test("b"), rejectsUV]
      "#,
            r#"{"ok":true,"value":[true,false,"v",true,false,true],"toolCalls":[]}"#,
        ),
        (
            "regexp-math-test262.test: d and v flags compose",
            r#"
        const pattern = new RegExp("(?<a>[a&&a])", "dv")
        const match = pattern.exec("a")
        return [pattern.hasIndices, pattern.unicodeSets, pattern.flags, match.indices[0], match.indices.groups.a]
      "#,
            r#"{"ok":true,"value":[true,true,"dv",[0,1],[0,1]],"toolCalls":[]}"#,
        ),
    ]);
}

#[test]
fn stdlib() {
    check(&[
        (
            "stdlib.test: literal test",
            r#"return /ab+c/.test("xabbbc")"#,
            r#"{"ok":true,"value":true,"toolCalls":[]}"#,
        ),
        (
            "stdlib.test: literal test",
            r#"return /ab+c/.test("nope")"#,
            r#"{"ok":true,"value":false,"toolCalls":[]}"#,
        ),
        (
            "stdlib.test: exec exposes captures and index",
            r#"const m = /a(b+)/.exec("xxabbc"); return { full: m[0], group: m[1], index: m.index }"#,
            r#"{"ok":true,"value":{"full":"abb","group":"bb","index":2},"toolCalls":[]}"#,
        ),
        (
            "stdlib.test: exec exposes captures and index",
            r#"return /a/.exec("zzz")"#,
            r#"{"ok":true,"value":null,"toolCalls":[]}"#,
        ),
        (
            "stdlib.test: named groups read through",
            r#"const m = /(?<word>[a-z]+)-(?<num>\d+)/.exec("id ab-42"); return m.groups.word + m.groups.num"#,
            r#"{"ok":true,"value":"ab42","toolCalls":[]}"#,
        ),
        (
            "stdlib.test: global exec advances lastIndex across calls",
            r#"
      const r = /\d+/g
      const first = r.exec("a1b22c")
      const second = r.exec("a1b22c")
      return [first[0], second[0]]
    "#,
            r#"{"ok":true,"value":["1","22"],"toolCalls":[]}"#,
        ),
        (
            "stdlib.test: lastIndex is writable and stores a number",
            r#"
        const pattern = /(?:ab|cd)\d?/g
        pattern.lastIndex = "12"
        const stored = [pattern.lastIndex, typeof pattern.lastIndex]
        const match = pattern.exec("aacd2233ab12nm444ab42")
        pattern.lastIndex = 0
        return [stored, match[0], match.index, pattern.lastIndex]
      "#,
            r#"{"ok":true,"value":[[12,"number"],"ab4",17,0],"toolCalls":[]}"#,
        ),
        (
            "stdlib.test: lastIndex is writable and stores a number",
            r#"const re = /a/; return [delete re.lastIndex, re.lastIndex]"#,
            r#"{"ok":true,"value":[true,0],"toolCalls":[]}"#,
        ),
        (
            "stdlib.test: a non-numeric lastIndex runs from 0; non-global exec and test leave it alone",
            r#"
        const pattern = /a/g
        pattern.lastIndex = {}
        const match = pattern.exec("ba")
        pattern.lastIndex = 10
        const missed = pattern.exec("a")
        const plain = /a/
        plain.lastIndex = 5
        return [match.index, pattern.lastIndex, missed, plain.exec("ba").index, plain.test("ba"), plain.lastIndex]
      "#,
            r#"{"ok":true,"value":[1,0,null,1,true,5],"toolCalls":[]}"#,
        ),
        (
            "stdlib.test: String methods read and update lastIndex like exec",
            r#"
        const re = /a/y
        re.exec("aa")
        re.lastIndex = 0
        return ["aa".replace(re, "b"), re.lastIndex]
      "#,
            r#"{"ok":true,"value":["ba",0],"toolCalls":[]}"#,
        ),
        (
            "stdlib.test: split, search, and matchAll leave lastIndex unchanged like JS",
            r#"
        const re = /a/y
        re.lastIndex = 2
        return ["banana".split(re), re.lastIndex]
      "#,
            r#"{"ok":true,"value":[["b","n","n",""],2],"toolCalls":[]}"#,
        ),
        (
            "stdlib.test: split, search, and matchAll leave lastIndex unchanged like JS",
            r#"
        const re = /a/g
        re.lastIndex = 2
        return ["banana".search(re), re.lastIndex]
      "#,
            r#"{"ok":true,"value":[1,2],"toolCalls":[]}"#,
        ),
        (
            "stdlib.test: split, search, and matchAll leave lastIndex unchanged like JS",
            r#"
        const re = /a/y
        re.lastIndex = 2
        return ["banana".search(re), re.lastIndex]
      "#,
            r#"{"ok":true,"value":[-1,2],"toolCalls":[]}"#,
        ),
        (
            "stdlib.test: split, search, and matchAll leave lastIndex unchanged like JS",
            r#"
        const re = /a/g
        re.lastIndex = 2
        return ["banana".matchAll(re).map((m) => m.index), re.lastIndex]
      "#,
            r#"{"ok":true,"value":[[3,5],2],"toolCalls":[]}"#,
        ),
        (
            "stdlib.test: split, search, and matchAll leave lastIndex unchanged like JS",
            r#"
        const re = /a/gy
        re.lastIndex = 1
        return ["banana".matchAll(re).map((m) => m.index), re.lastIndex]
      "#,
            r#"{"ok":true,"value":[[1],1],"toolCalls":[]}"#,
        ),
        (
            "stdlib.test: String methods leave lastIndex untouched without g or y",
            r#"
        const re = /a/
        re.lastIndex = 5
        return ["aaa".replace(re, "b"), "aaa".match(re).index, "aaa".split(re), "aaa".search(re), re.lastIndex]
      "#,
            r#"{"ok":true,"value":["baa",0,["","","",""],0,5],"toolCalls":[]}"#,
        ),
        (
            "stdlib.test: an unmatched string pattern returns null",
            r#"return "abc".match(/\d/)"#,
            r#"{"ok":true,"value":null,"toolCalls":[]}"#,
        ),
        (
            "stdlib.test: matchAll materializes match arrays with captures",
            r#"return "a1b22".matchAll(/(\d+)/g).map((m) => m[1])"#,
            r#"{"ok":true,"value":["1","22"],"toolCalls":[]}"#,
        ),
        (
            "stdlib.test: function replacers receive captures, offsets, input, and named groups",
            r#"
        return "red-blue".replace(
          /(?<left>[a-z]+)-(?<right>[a-z]+)/,
          (match, left, right, offset, input, groups) => groups.right + ":" + groups.left,
        )
      "#,
            r#"{"ok":true,"value":"blue:red","toolCalls":[]}"#,
        ),
        (
            "stdlib.test: function replacers support string searches, zero-length matches, and result coercion",
            r#"return "banana".replace("na", (match, offset, input) => "[" + offset + "]")"#,
            r#"{"ok":true,"value":"ba[2]na","toolCalls":[]}"#,
        ),
        (
            "stdlib.test: function replacers support string searches, zero-length matches, and result coercion",
            r#"return "ab".replaceAll("", (match, offset) => offset)"#,
            r#"{"ok":true,"value":"0a1b2","toolCalls":[]}"#,
        ),
        (
            "stdlib.test: function replacers support string searches, zero-length matches, and result coercion",
            r#"return "😀".replaceAll(/(?:)/gu, (match, offset) => "[" + offset + "]")"#,
            r#"{"ok":true,"value":"[0]😀[2]","toolCalls":[]}"#,
        ),
        (
            "stdlib.test: function replacers support string searches, zero-length matches, and result coercion",
            r#"return "123".replace(/\d/g, (match) => match === "1" ? 7 : match === "2" ? null : { n: 3 })"#,
            r#"{"ok":true,"value":"7null[object Object]","toolCalls":[]}"#,
        ),
        (
            "stdlib.test: replaceAll without the g flag is a catchable error",
            r#"try { "a".replaceAll(/a/, "b"); return "no" } catch { return "caught" }"#,
            r#"{"ok":true,"value":"caught","toolCalls":[]}"#,
        ),
        (
            "stdlib.test: new RegExp constructs from strings; invalid patterns are catchable",
            r#"return new RegExp("a+", "i").test("AAA")"#,
            r#"{"ok":true,"value":true,"toolCalls":[]}"#,
        ),
        (
            "stdlib.test: new RegExp constructs from strings; invalid patterns are catchable",
            r#"try { new RegExp("("); return "no" } catch { return "caught" }"#,
            r#"{"ok":true,"value":"caught","toolCalls":[]}"#,
        ),
        (
            "stdlib.test: new RegExp constructs from strings; invalid patterns are catchable",
            r#"return [/a/ instanceof RegExp, /a/.source]"#,
            r#"{"ok":true,"value":[true,"a"],"toolCalls":[]}"#,
        ),
        (
            "stdlib.test: invalid patterns fail with actionable messages",
            r#"return "abc".match("(")"#,
            r#"{"ok":false,"error":{"kind":"ExecutionFailure","message":"SyntaxError: String.match received the string \"(\", which is not a valid regular expression pattern (missing )). To match special characters like ( ) [ ] { } + * ? . literally, escape them with a backslash (e.g. \"\\\\(\") or test for them with String.includes instead. (line 1, col 8)","location":{"line":1,"column":8}},"toolCalls":[]}"#,
        ),
        (
            "stdlib.test: invalid patterns fail with actionable messages",
            r#"return new RegExp("(")"#,
            r#"{"ok":false,"error":{"kind":"ExecutionFailure","message":"SyntaxError: new RegExp(...) received \"(\", which is not a valid regular expression pattern (missing )). To match special characters like ( ) [ ] { } + * ? . literally, escape them with a backslash (e.g. \"\\\\(\") or test for them with String.includes instead. (line 1, col 8)","location":{"line":1,"column":8}},"toolCalls":[]}"#,
        ),
        (
            "stdlib.test: invalid patterns fail with actionable messages",
            r#"return new RegExp("a", "xz")"#,
            r#"{"ok":false,"error":{"kind":"ExecutionFailure","message":"SyntaxError: new RegExp(...) received invalid flags \"xz\" (Invalid flags supplied to RegExp constructor.). Valid flags are d, g, i, m, s, u, v, and y. (line 1, col 8)","location":{"line":1,"column":8}},"toolCalls":[]}"#,
        ),
        (
            "stdlib.test: missing g-flag errors say how to fix the call",
            r#"return "aa".replaceAll(/a/, "b")"#,
            r#"{"ok":false,"error":{"kind":"ExecutionFailure","message":"TypeError: String.replaceAll requires a regular expression with the global (g) flag: write /a/g, or use String.replace to replace only the first match. (line 1, col 8)","location":{"line":1,"column":8}},"toolCalls":[]}"#,
        ),
        (
            "stdlib.test: missing g-flag errors say how to fix the call",
            r#"return "aa".matchAll(/a/)"#,
            r#"{"ok":false,"error":{"kind":"ExecutionFailure","message":"TypeError: String.matchAll requires a regular expression with the global (g) flag: write /a/g, or use String.match for a single match. (line 1, col 8)","location":{"line":1,"column":8}},"toolCalls":[]}"#,
        ),
        (
            "stdlib.test: any argument is a pattern string, as new RegExp(arg) reads it",
            r#"return ["a42b".match(42)[0], "xnullx".search(null), "abc".match(undefined), [..."1a1".matchAll(1)].length]"#,
            r#"{"ok":true,"value":["42",1,[""],2],"toolCalls":[]}"#,
        ),
        (
            "stdlib.test: source and flags properties read through",
            r#"const r = /ab/gi; return { source: r.source, flags: r.flags, global: r.global }"#,
            r#"{"ok":true,"value":{"source":"ab","flags":"gi","global":true},"toolCalls":[]}"#,
        ),
        (
            "stdlib.test: template interpolation renders the literal form",
            r#"return `${/ab/g}`"#,
            r#"{"ok":true,"value":"/ab/g","toolCalls":[]}"#,
        ),
        (
            "stdlib.test: Object values and entries accept arrays",
            r#"const match = /a/.exec("ba"); return [Object.values(match), Object.entries(match)]"#,
            r#"{"ok":true,"value":[["a",1,"ba"],[["0","a"],["index",1],["input","ba"]]],"toolCalls":[]}"#,
        ),
        (
            "stdlib.test: Object values and entries accept arrays",
            r#"return Object.keys(Object.values({ match: /a/.exec("ba") })[0])"#,
            r#"{"ok":true,"value":["0","index","input"],"toolCalls":[]}"#,
        ),
        (
            "stdlib.test: regexes stay callable through Object.values",
            r#"return Object.values({ r: /ab+/ })[0].test("abb")"#,
            r#"{"ok":true,"value":true,"toolCalls":[]}"#,
        ),
    ]);
}

#[test]
fn parity() {
    check(&[
        (
            "parity.test: localeCompare orders strings for sorting",
            r#"return ["b","a","c"].sort((x, y) => x.localeCompare(y))"#,
            r#"{"ok":true,"value":["a","b","c"],"toolCalls":[]}"#,
        ),
        (
            "parity.test: an invalid normalize form is a clear catchable error",
            r#"try { "x".normalize("nope"); return "no" } catch (e) { return e.message }"#,
            r#"{"ok":true,"value":"String.normalize expects the form \"NFC\", \"NFD\", \"NFKC\", or \"NFKD\" (got \"nope\").","toolCalls":[]}"#,
        ),
        (
            "parity.test: exposes the Annex B string aliases every engine ships",
            r#"return [" x ".trimLeft(), " x ".trimRight(), "abc".substr(1, 1)]"#,
            r#"{"ok":true,"value":["x "," x","b"],"toolCalls":[]}"#,
        ),
        (
            "parity.test: includes and indexOf coerce numbers",
            r#"return "v1.2".includes(1)"#,
            r#"{"ok":true,"value":true,"toolCalls":[]}"#,
        ),
        (
            "parity.test: includes and indexOf coerce numbers",
            r#"return "a2b".indexOf(2)"#,
            r#"{"ok":true,"value":1,"toolCalls":[]}"#,
        ),
        (
            "parity.test: includes and indexOf coerce numbers",
            r#"return "abc".includes("d")"#,
            r#"{"ok":true,"value":false,"toolCalls":[]}"#,
        ),
        (
            "parity.test: slice, repeat, and padStart coerce numeric strings",
            r#"return "abc".slice("1")"#,
            r#"{"ok":true,"value":"bc","toolCalls":[]}"#,
        ),
        (
            "parity.test: slice, repeat, and padStart coerce numeric strings",
            r#"return "ab".repeat("2")"#,
            r#"{"ok":true,"value":"abab","toolCalls":[]}"#,
        ),
        (
            "parity.test: slice, repeat, and padStart coerce numeric strings",
            r#"return "7".padStart("3", 0)"#,
            r#"{"ok":true,"value":"007","toolCalls":[]}"#,
        ),
        (
            "parity.test: split coerces separators but treats undefined as absent",
            r#"return "a1b".split(1)"#,
            r#"{"ok":true,"value":["a","b"],"toolCalls":[]}"#,
        ),
        (
            "parity.test: split coerces separators but treats undefined as absent",
            r#"return "a,b".split(undefined)"#,
            r#"{"ok":true,"value":["a,b"],"toolCalls":[]}"#,
        ),
        (
            "parity.test: split coerces separators but treats undefined as absent",
            r#"return "a,b".split()"#,
            r#"{"ok":true,"value":["a,b"],"toolCalls":[]}"#,
        ),
        (
            "parity.test: split coerces separators but treats undefined as absent",
            r#"return "a,b".split(undefined, 0)"#,
            r#"{"ok":true,"value":[],"toolCalls":[]}"#,
        ),
        (
            "parity.test: split coerces separators but treats undefined as absent",
            r#"return "a,b".split(undefined, 1)"#,
            r#"{"ok":true,"value":["a,b"],"toolCalls":[]}"#,
        ),
        (
            "parity.test: replace coerces search and replacement values",
            r#"return "a1b".replace(1, 2)"#,
            r#"{"ok":true,"value":"a2b","toolCalls":[]}"#,
        ),
        (
            "parity.test: replace coerces search and replacement values",
            r#"return "a1b".replace(1, () => "x")"#,
            r#"{"ok":true,"value":"axb","toolCalls":[]}"#,
        ),
        (
            "parity.test: repeat rejections carry the native RangeError name",
            r#"try { "a".repeat(-1) } catch (e) { return e.name }"#,
            r#"{"ok":true,"value":"RangeError","toolCalls":[]}"#,
        ),
        (
            "parity.test: includes, startsWith, and endsWith reject regular expressions with a TypeError",
            r#"try { "abc".includes(/a/) } catch (e) { return e.name }"#,
            r#"{"ok":true,"value":"TypeError","toolCalls":[]}"#,
        ),
        (
            "parity.test: includes, startsWith, and endsWith reject regular expressions with a TypeError",
            r#"try { "abc".startsWith(/a/) } catch (e) { return e.name }"#,
            r#"{"ok":true,"value":"TypeError","toolCalls":[]}"#,
        ),
        (
            "parity.test: includes, startsWith, and endsWith reject regular expressions with a TypeError",
            r#"try { "abc".endsWith(/a/) } catch (e) { return e.name }"#,
            r#"{"ok":true,"value":"TypeError","toolCalls":[]}"#,
        ),
        (
            "parity.test: opaque runtime references still reject as data errors",
            r#"const f = () => 1; return "abc".includes(f)"#,
            r#"{"ok":false,"error":{"kind":"InvalidDataValue","message":"TypeError: String.includes expects argument 1 to be a data value. (line 1, col 27)","location":{"line":1,"column":27}},"toolCalls":[]}"#,
        ),
        (
            "parity.test: opaque runtime references still reject as data errors",
            r#"const f = () => 1; return "a".replace(f, () => "x")"#,
            r#"{"ok":false,"error":{"kind":"InvalidDataValue","message":"TypeError: String.replace expects argument 1 to be a data value. (line 1, col 27)","location":{"line":1,"column":27}},"toolCalls":[]}"#,
        ),
        (
            "parity.test: behave as an empty pattern like native JS",
            r#"return "abc".search()"#,
            r#"{"ok":true,"value":0,"toolCalls":[]}"#,
        ),
        (
            "parity.test: behave as an empty pattern like native JS",
            r#"const m = "abc".match(); return { first: m[0], index: m.index }"#,
            r#"{"ok":true,"value":{"first":"","index":0},"toolCalls":[]}"#,
        ),
        (
            "parity.test: the tag receives the cooked strings, their raw forms, and the substitutions in order",
            r#"
        const tag = (strings, ...values) => [strings, strings.raw, values, Object.keys(strings)]
        return tag`a${1}b\n${2}c`
      "#,
            r#"{"ok":true,"value":[["a","b\n","c"],["a","b\\n","c"],[1,2],["0","1","2"]],"toolCalls":[]}"#,
        ),
        (
            "parity.test: an invalid escape cooks to undefined and keeps its raw text",
            r#"return ((strings) => [strings[0] === undefined, strings.raw[0]])`\unicode`"#,
            r#"{"ok":true,"value":[true,"\\unicode"],"toolCalls":[]}"#,
        ),
        (
            "parity.test: each site has one template object; different sites differ",
            r#"
        const seen = []
        const tag = (strings) => { seen.push(strings) }
        for (let i = 0; i < 2; i++) tag`x${i}`
        tag`x${0}`
        return [seen[0] === seen[1], seen[0] === seen[2]]
      "#,
            r#"{"ok":true,"value":[true,false],"toolCalls":[]}"#,
        ),
        (
            "parity.test: the tag is read like a callee: members, chained tags, async tags, and the not-a-function error",
            r#"
        const o = { tag: (strings) => strings[0].toUpperCase() }
        const chain = () => chain
        const asyncTag = async (strings, value) => strings[0] + value
        let failure
        try { (1)`x` } catch (error) { failure = error instanceof TypeError }
        return [o.tag`abc`, typeof chain`a``b`, await asyncTag`n=${1}`, failure]
      "#,
            r#"{"ok":true,"value":["ABC","function","n=1",true],"toolCalls":[]}"#,
        ),
        (
            "parity.test: raw is read-only",
            r#"try { ((strings) => { strings.raw = 1 })`a` } catch (error) { return error.name }"#,
            r#"{"ok":true,"value":"TypeError","toolCalls":[]}"#,
        ),
        (
            "parity.test: joins the raw strings with the substitutions",
            r#"return [String.raw`a\n${1}b`, String.raw({ raw: ["x", "y", "z"] }, 1, 2, 3)]"#,
            r#"{"ok":true,"value":["a\\n1b","x1y2z"],"toolCalls":[]}"#,
        ),
        (
            "parity.test: extra substitutions are dropped, missing ones are skipped, and a program object's toString is used",
            r#"
        const shout = { toString() { return "!" } }
        return [String.raw({ raw: ["x", "y"] }, 1, 2), String.raw({ raw: ["x", "y", "z"] }, shout), String.raw({ raw: { length: 0 } })]
      "#,
            r#"{"ok":true,"value":["x1y","x!yz",""],"toolCalls":[]}"#,
        ),
        (
            "parity.test: a template without a raw array is a TypeError",
            r#"String.raw(1)"#,
            r#"{"ok":false,"error":{"kind":"ExecutionFailure","message":"TypeError: String.raw expects a template object with a raw array. (line 1, col 1)","location":{"line":1,"column":1}},"toolCalls":[]}"#,
        ),
        (
            "parity.test: the hint picks the method: + and Number prefer valueOf, template literals and String prefer toString",
            r#"
        const both = { valueOf() { return 1 }, toString() { return "s" } }
        return [both + "", `${both}`, String(both), both * 2, new Error(both).message, [both].join(), [both, 2] + ""]
      "#,
            r#"{"ok":true,"value":["1","s","s",2,"s","s","s,2"],"toolCalls":[]}"#,
        ),
        (
            "parity.test: arrays keep their built-in join form unless the program replaces toString",
            r#"
        const arr = [1, 2]
        const before = [arr + "", [] + [], [1, , 3].join("-"), [1, { toString() { return "q" } }].join("-")]
        arr.toString = () => "x"
        return [...before, arr + "", `${arr}`, String(arr)]
      "#,
            r#"{"ok":true,"value":["1,2","","1--3","1-q","x","x","x"],"toolCalls":[]}"#,
        ),
        (
            "parity.test: functions and other opaque values still reject arithmetic, and an object without a primitive form throws",
            r#"return { valueOf() { return {} }, toString() { return [] } } + 1"#,
            r#"{"ok":false,"error":{"kind":"ExecutionFailure","message":"TypeError: Cannot convert object to primitive value. (line 1, col 8)","location":{"line":1,"column":8}},"toolCalls":[]}"#,
        ),
        (
            "parity.test: split, replace, match, and search convert a plain pattern but keep a RegExp as is",
            r#"
        const s = { toString() { return "b" } }
        return [
          "abc".split(s), "abc".split(/b/, { valueOf() { return 1 } }), "abc".split(undefined, { valueOf() { return undefined } }),
          "abc".replace(s, "X"), "abc".replace(/b/, { toString() { return "R" } }), "abc".replaceAll(s, s),
          "abc".replace(s, (m) => m.toUpperCase()), "abc".match(s)[0], "abcb".matchAll(s).length, "abc".search(s),
        ]
      "#,
            r#"{"ok":true,"value":[["a","c"],["a"],[],"aXc","aRc","abc","aBc","b",2,1],"toolCalls":[]}"#,
        ),
        (
            "parity.test: split, replace, match, and search convert a plain pattern but keep a RegExp as is",
            r#""abc".includes(/b/)"#,
            r#"{"ok":false,"error":{"kind":"ExecutionFailure","message":"TypeError: String.includes cannot take a regular expression; use regex.test(string) or String.search instead. (line 1, col 1)","location":{"line":1,"column":1}},"toolCalls":[]}"#,
        ),
        (
            "parity.test: the receiver converts first, then each consumed argument, in spec order; extra arguments are untouched",
            r#"
        const log = []
        const observer = (name, string, number) => ({
          toString() { log.push("toString:" + name); return string },
          valueOf() { log.push("valueOf:" + name); return number },
        })
        const padded = String.prototype.padStart.call(observer("receiver", {}, "abc"), observer("maxLength", 11, {}), observer("fillString", {}, "def"))
        const extra = "abc".indexOf("b", 1, { valueOf() { throw new Error("extra argument converted") } })
        return [padded, log, extra, String.prototype.trim.call({ toString() { return " abc " } })]
      "#,
            r#"{"ok":true,"value":["defdefdeabc",["toString:receiver","valueOf:receiver","valueOf:maxLength","toString:maxLength","toString:fillString","valueOf:fillString"],1,"abc"],"toolCalls":[]}"#,
        ),
        (
            "parity.test: conversion failures surface and opaque arguments still reject",
            r#""abc".indexOf({ toString() { throw new RangeError("intostr") } })"#,
            r#"{"ok":false,"error":{"kind":"ExecutionFailure","message":"RangeError: intostr"},"toolCalls":[]}"#,
        ),
        (
            "parity.test: conversion failures surface and opaque arguments still reject",
            r#""abc".indexOf(tools.nowhere)"#,
            r#"{"ok":false,"error":{"kind":"InvalidDataValue","message":"TypeError: String.indexOf expects argument 1 to be a data value. (line 1, col 1)","location":{"line":1,"column":1}},"toolCalls":[]}"#,
        ),
        (
            "parity.test: conversion failures surface and opaque arguments still reject",
            r#""abc".indexOf(Promise.resolve("b"))"#,
            r#"{"ok":false,"error":{"kind":"InvalidDataValue","message":"TypeError: String.indexOf expects argument 1 to be a data value. (line 1, col 1)","location":{"line":1,"column":1}},"toolCalls":[]}"#,
        ),
    ]);
}

#[test]
fn limits() {
    check(&[
        (
            "limits.test: strings: repeat, pad, concat, join, +, template, JSON.stringify",
            r#"return "x".repeat(16777216).length"#,
            r#"{"ok":true,"value":16777216,"toolCalls":[]}"#,
        ),
        (
            "limits.test: strings: repeat, pad, concat, join, +, template, JSON.stringify",
            r#"try { "x".repeat(16777217) } catch (e) { throw Error(e.name + ": " + e.message) }"#,
            r#"{"ok":false,"error":{"kind":"ExecutionFailure","message":"Error: RangeError: Invalid string length"},"toolCalls":[]}"#,
        ),
        (
            "limits.test: strings: repeat, pad, concat, join, +, template, JSON.stringify",
            r#"try { "x".padStart(16777217) } catch (e) { throw Error(e.name + ": " + e.message) }"#,
            r#"{"ok":false,"error":{"kind":"ExecutionFailure","message":"Error: RangeError: Invalid string length"},"toolCalls":[]}"#,
        ),
        (
            "limits.test: strings: repeat, pad, concat, join, +, template, JSON.stringify",
            r#"try { "x".padEnd(16777217, "y") } catch (e) { throw Error(e.name + ": " + e.message) }"#,
            r#"{"ok":false,"error":{"kind":"ExecutionFailure","message":"Error: RangeError: Invalid string length"},"toolCalls":[]}"#,
        ),
        (
            "limits.test: strings: repeat, pad, concat, join, +, template, JSON.stringify",
            r#"try { "x".repeat(16777216).concat("y") } catch (e) { throw Error(e.name + ": " + e.message) }"#,
            r#"{"ok":false,"error":{"kind":"ExecutionFailure","message":"Error: RangeError: Invalid string length"},"toolCalls":[]}"#,
        ),
        (
            "limits.test: strings: repeat, pad, concat, join, +, template, JSON.stringify",
            r#"try { ["x".repeat(16777216), "y"].join("") } catch (e) { throw Error(e.name + ": " + e.message) }"#,
            r#"{"ok":false,"error":{"kind":"ExecutionFailure","message":"Error: RangeError: Invalid string length"},"toolCalls":[]}"#,
        ),
        (
            "limits.test: strings: repeat, pad, concat, join, +, template, JSON.stringify",
            r#"try { "x".repeat(16777216) + "y" } catch (e) { throw Error(e.name + ": " + e.message) }"#,
            r#"{"ok":false,"error":{"kind":"ExecutionFailure","message":"Error: RangeError: Invalid string length"},"toolCalls":[]}"#,
        ),
        (
            "limits.test: strings: repeat, pad, concat, join, +, template, JSON.stringify",
            r#"try { `${"x".repeat(16777216)}y` } catch (e) { throw Error(e.name + ": " + e.message) }"#,
            r#"{"ok":false,"error":{"kind":"ExecutionFailure","message":"Error: RangeError: Invalid string length"},"toolCalls":[]}"#,
        ),
        (
            "limits.test: strings: repeat, pad, concat, join, +, template, JSON.stringify",
            r#"try { JSON.stringify("x".repeat(16777216)) } catch (e) { throw Error(e.name + ": " + e.message) }"#,
            r#"{"ok":false,"error":{"kind":"ExecutionFailure","message":"Error: RangeError: Invalid string length"},"toolCalls":[]}"#,
        ),
        (
            "limits.test: arrays: constructor, length, Array.from, concat, flat",
            r#"try { Array(10000000).concat([1]) } catch (e) { throw Error(e.name + ": " + e.message) }"#,
            r#"{"ok":false,"error":{"kind":"ExecutionFailure","message":"Error: RangeError: Invalid array length"},"toolCalls":[]}"#,
        ),
    ]);
}

#[test]
fn callbacks() {
    check(&[
        (
            "callbacks.test: detached method references lose their receiver, like JS",
            r#"return ["a", "z"].filter("abc".includes)"#,
            r#"{"ok":false,"error":{"kind":"ExecutionFailure","message":"TypeError: String.prototype.includes called on null or undefined. (line 1, col 8)","location":{"line":1,"column":8}},"toolCalls":[]}"#,
        ),
        (
            "callbacks.test: error values stringify like JS Error.prototype.toString",
            r#"return String(TypeError("bad"))"#,
            r#"{"ok":true,"value":"TypeError: bad","toolCalls":[]}"#,
        ),
        (
            "callbacks.test: error values stringify like JS Error.prototype.toString",
            r#"return String(Error(""))"#,
            r#"{"ok":true,"value":"Error","toolCalls":[]}"#,
        ),
        (
            "callbacks.test: error values stringify like JS Error.prototype.toString",
            r#"return "a1b2".replace(/\d/, Error)"#,
            r#"{"ok":true,"value":"aError: 1b2","toolCalls":[]}"#,
        ),
        (
            "callbacks.test: Date() without new returns a deterministic ISO string and ignores arguments",
            r#"return "abc".replace(RegExp("b"), "x")"#,
            r#"{"ok":true,"value":"axc","toolCalls":[]}"#,
        ),
        (
            "callbacks.test: RegExp with non-string flags throws a SyntaxError, like JS",
            r#"try { RegExp("a", 0) } catch (e) { throw Error(e.name) }"#,
            r#"{"ok":false,"error":{"kind":"ExecutionFailure","message":"Error: SyntaxError"},"toolCalls":[]}"#,
        ),
        (
            "callbacks.test: built-in references work as replacers",
            r#"return "abc".replace(/b/, JSON.stringify)"#,
            r#"{"ok":true,"value":"a\"b\"c","toolCalls":[]}"#,
        ),
    ]);
}

#[test]
fn edge_cases() {
    check(&[
        (
            "probe s1.txt",
            r#"return String.fromCodePoint(-1)"#,
            r#"{"ok":false,"error":{"kind":"ExecutionFailure","message":"RangeError: Arguments contain a value that is out of range of code points (line 1, col 8)","location":{"line":1,"column":8}},"toolCalls":[]}"#,
        ),
        (
            "probe s1.txt",
            r#"return String.fromCodePoint(1.5)"#,
            r#"{"ok":false,"error":{"kind":"ExecutionFailure","message":"RangeError: Arguments contain a value that is out of range of code points (line 1, col 8)","location":{"line":1,"column":8}},"toolCalls":[]}"#,
        ),
        (
            "probe s1.txt",
            r#"return String.fromCodePoint(0x10FFFF+1)"#,
            r#"{"ok":false,"error":{"kind":"ExecutionFailure","message":"RangeError: Arguments contain a value that is out of range of code points (line 1, col 8)","location":{"line":1,"column":8}},"toolCalls":[]}"#,
        ),
        (
            "probe s1.txt",
            r#"return "abc".normalize("x")"#,
            r#"{"ok":false,"error":{"kind":"ExecutionFailure","message":"RangeError: String.normalize expects the form \"NFC\", \"NFD\", \"NFKC\", or \"NFKD\" (got \"x\"). (line 1, col 8)","location":{"line":1,"column":8}},"toolCalls":[]}"#,
        ),
        (
            "probe s1.txt",
            r#"return "ab".repeat(-1)"#,
            r#"{"ok":false,"error":{"kind":"ExecutionFailure","message":"RangeError: String.repeat expects a finite non-negative count. (line 1, col 8)","location":{"line":1,"column":8}},"toolCalls":[]}"#,
        ),
        (
            "probe s1.txt",
            r#"return "ab".repeat(Infinity)"#,
            r#"{"ok":false,"error":{"kind":"ExecutionFailure","message":"RangeError: String.repeat expects a finite non-negative count. (line 1, col 8)","location":{"line":1,"column":8}},"toolCalls":[]}"#,
        ),
        (
            "probe s1.txt",
            r#"return "ab".repeat(1e9)"#,
            r#"{"ok":false,"error":{"kind":"ExecutionFailure","message":"RangeError: Invalid string length (line 1, col 8)","location":{"line":1,"column":8}},"toolCalls":[]}"#,
        ),
        (
            "probe s1.txt",
            r#"return "a".padStart(2**25)"#,
            r#"{"ok":false,"error":{"kind":"ExecutionFailure","message":"RangeError: Invalid string length (line 1, col 8)","location":{"line":1,"column":8}},"toolCalls":[]}"#,
        ),
        (
            "probe s1.txt",
            r#"return "a".padStart(5, "")"#,
            r#"{"ok":true,"value":"a","toolCalls":[]}"#,
        ),
        (
            "probe s1.txt",
            r#"return ["a","B","b","A","é","e","f","_","1"," ","Z","z","ä"].sort((x,y)=>x.localeCompare(y))"#,
            r#"{"ok":true,"value":[" ","_","1","a","A","ä","b","B","e","é","f","z","Z"],"toolCalls":[]}"#,
        ),
        (
            "probe s1.txt",
            r#"return ["résumé","resume","Resume","résume","a b","ab","a-b","a_b","a.b","10","9","1"].sort((x,y)=>x.localeCompare(y))"#,
            r#"{"ok":true,"value":["1","10","9","a b","a_b","a-b","a.b","ab","resume","Resume","résume","résumé"],"toolCalls":[]}"#,
        ),
        (
            "probe s1.txt",
            r#"return ["ß".toUpperCase(), "İ".toLowerCase(), "ΑΣ".toLowerCase(), "ǅ".toUpperCase(), "ﬁ".toUpperCase()]"#,
            r#"{"ok":true,"value":["SS","i̇","ας","Ǆ","FI"],"toolCalls":[]}"#,
        ),
        (
            "probe s1.txt",
            r#"return "x".codePointAt(5)"#,
            r#"{"ok":true,"value":null,"toolCalls":[]}"#,
        ),
        (
            "probe s1.txt",
            r#"return [..."a😀b"]"#,
            r#"{"ok":true,"value":["a","😀","b"],"toolCalls":[]}"#,
        ),
        (
            "probe s1.txt",
            r#"return "abc".at(-1) + "abc".charAt(9) + "abc".charCodeAt(9)"#,
            r#"{"ok":true,"value":"cNaN","toolCalls":[]}"#,
        ),
        (
            "probe s1.txt",
            r#"return "a,b,,c".split(",", 2)"#,
            r#"{"ok":true,"value":["a","b"],"toolCalls":[]}"#,
        ),
        (
            "probe s1.txt",
            r#"return "abc".split(undefined, 0)"#,
            r#"{"ok":true,"value":[],"toolCalls":[]}"#,
        ),
        (
            "probe s1.txt",
            r#"return "abc".includes(/a/)"#,
            r#"{"ok":false,"error":{"kind":"ExecutionFailure","message":"TypeError: String.includes cannot take a regular expression; use regex.test(string) or String.search instead. (line 1, col 8)","location":{"line":1,"column":8}},"toolCalls":[]}"#,
        ),
        (
            "probe s1.txt",
            r#"return "aXbXc".replace("X", (m, o, s) => [m, o, s].join("|"))"#,
            r#"{"ok":true,"value":"aX|1|aXbXcbXc","toolCalls":[]}"#,
        ),
        (
            "probe s1.txt",
            r#"return "aXbXc".replaceAll(/x/i, "y")"#,
            r#"{"ok":false,"error":{"kind":"ExecutionFailure","message":"TypeError: String.replaceAll requires a regular expression with the global (g) flag: write /x/ig, or use String.replace to replace only the first match. (line 1, col 8)","location":{"line":1,"column":8}},"toolCalls":[]}"#,
        ),
        (
            "probe s2.txt",
            r#"const pats = ["(", ")", "[", "]", "{", "}", "a{", "a{1", "a{1,", "a{2,1}", "*", "+a", "a**", "a??+", "\\", "(?<a>x)(?<a>y)", "(?<a>x)|(?<a>y)", "\\k<a>", "(?<a>x)\\k<b>", "(?<1a>x)", "(?", "(?x)", "(?<", "[b-a]", "\\1(a)", "\\c", "^*", "$*", "\\b*", "(?=a)*", "(?<=a)*", "x{99999999999}", "[\\d-z]", "\\p{L}", "\\u{1F600}", "(?i:a)", "(?-i:a)", "(?i-i:a)", "(?ii:a)"]
return pats.map(p => { try { new RegExp(p); return "ok" } catch (e) { return e.message } })"#,
            r#"{"ok":true,"value":["new RegExp(...) received \"(\", which is not a valid regular expression pattern (missing )). To match special characters like ( ) [ ] { } + * ? . literally, escape them with a backslash (e.g. \"\\\\(\") or test for them with String.includes instead.","new RegExp(...) received \")\", which is not a valid regular expression pattern (unmatched parentheses). To match special characters like ( ) [ ] { } + * ? . literally, escape them with a backslash (e.g. \"\\\\(\") or test for them with String.includes instead.","new RegExp(...) received \"[\", which is not a valid regular expression pattern (missing terminating ] for character class). To match special characters like ( ) [ ] { } + * ? . literally, escape them with a backslash (e.g. \"\\\\(\") or test for them with String.includes instead.","ok","ok","ok","ok","ok","ok","new RegExp(...) received \"a{2,1}\", which is not a valid regular expression pattern (numbers out of order in {} quantifier). To match special characters like ( ) [ ] { } + * ? . literally, escape them with a backslash (e.g. \"\\\\(\") or test for them with String.includes instead.","new RegExp(...) received \"*\", which is not a valid regular expression pattern (nothing to repeat). To match special characters like ( ) [ ] { } + * ? . literally, escape them with a backslash (e.g. \"\\\\(\") or test for them with String.includes instead.","new RegExp(...) received \"+a\", which is not a valid regular expression pattern (nothing to repeat). To match special characters like ( ) [ ] { } + * ? . literally, escape them with a backslash (e.g. \"\\\\(\") or test for them with String.includes instead.","new RegExp(...) received \"a**\", which is not a valid regular expression pattern (nothing to repeat). To match special characters like ( ) [ ] { } + * ? . literally, escape them with a backslash (e.g. \"\\\\(\") or test for them with String.includes instead.","new RegExp(...) received \"a??+\", which is not a valid regular expression pattern (nothing to repeat). To match special characters like ( ) [ ] { } + * ? . literally, escape them with a backslash (e.g. \"\\\\(\") or test for them with String.includes instead.","new RegExp(...) received \"\\\\\", which is not a valid regular expression pattern (\\ at end of pattern). To match special characters like ( ) [ ] { } + * ? . literally, escape them with a backslash (e.g. \"\\\\(\") or test for them with String.includes instead.","new RegExp(...) received \"(?<a>x)(?<a>y)\", which is not a valid regular expression pattern (duplicate group specifier name). To match special characters like ( ) [ ] { } + * ? . literally, escape them with a backslash (e.g. \"\\\\(\") or test for them with String.includes instead.","ok","ok","new RegExp(...) received \"(?<a>x)\\\\k<b>\", which is not a valid regular expression pattern (invalid \\k<> named backreference). To match special characters like ( ) [ ] { } + * ? . literally, escape them with a backslash (e.g. \"\\\\(\") or test for them with String.includes instead.","new RegExp(...) received \"(?<1a>x)\", which is not a valid regular expression pattern (invalid group specifier name). To match special characters like ( ) [ ] { } + * ? . literally, escape them with a backslash (e.g. \"\\\\(\") or test for them with String.includes instead.","new RegExp(...) received \"(?\", which is not a valid regular expression pattern (unrecognized character after (?). To match special characters like ( ) [ ] { } + * ? . literally, escape them with a backslash (e.g. \"\\\\(\") or test for them with String.includes instead.","new RegExp(...) received \"(?x)\", which is not a valid regular expression pattern (unrecognized character after (?). To match special characters like ( ) [ ] { } + * ? . literally, escape them with a backslash (e.g. \"\\\\(\") or test for them with String.includes instead.","new RegExp(...) received \"(?<\", which is not a valid regular expression pattern (invalid group specifier name). To match special characters like ( ) [ ] { } + * ? . literally, escape them with a backslash (e.g. \"\\\\(\") or test for them with String.includes instead.","new RegExp(...) received \"[b-a]\", which is not a valid regular expression pattern (range out of order in character class). To match special characters like ( ) [ ] { } + * ? . literally, escape them with a backslash (e.g. \"\\\\(\") or test for them with String.includes instead.","ok","ok","new RegExp(...) received \"^*\", which is not a valid regular expression pattern (nothing to repeat). To match special characters like ( ) [ ] { } + * ? . literally, escape them with a backslash (e.g. \"\\\\(\") or test for them with String.includes instead.","new RegExp(...) received \"$*\", which is not a valid regular expression pattern (nothing to repeat). To match special characters like ( ) [ ] { } + * ? . literally, escape them with a backslash (e.g. \"\\\\(\") or test for them with String.includes instead.","new RegExp(...) received \"\\\\b*\", which is not a valid regular expression pattern (nothing to repeat). To match special characters like ( ) [ ] { } + * ? . literally, escape them with a backslash (e.g. \"\\\\(\") or test for them with String.includes instead.","ok","new RegExp(...) received \"(?<=a)*\", which is not a valid regular expression pattern (invalid quantifier). To match special characters like ( ) [ ] { } + * ? . literally, escape them with a backslash (e.g. \"\\\\(\") or test for them with String.includes instead.","ok","ok","ok","ok","ok","ok","new RegExp(...) received \"(?i-i:a)\", which is not a valid regular expression pattern (invalid regular expression modifier). To match special characters like ( ) [ ] { } + * ? . literally, escape them with a backslash (e.g. \"\\\\(\") or test for them with String.includes instead.","new RegExp(...) received \"(?ii:a)\", which is not a valid regular expression pattern (invalid regular expression modifier). To match special characters like ( ) [ ] { } + * ? . literally, escape them with a backslash (e.g. \"\\\\(\") or test for them with String.includes instead."],"toolCalls":[]}"#,
        ),
        (
            "probe s2.txt",
            r#"const pats = ["\\p{L}", "\\p{Foo}", "\\P{", "\\u{110000}", "\\u{zz}", "\\x", "\\-", "\\q", "[\\d-z]", "{", "}", "]", "a{1", "\\1", "\\k<a>", "\\c1", "[\\c1]", "\\8", "(?<a>.)\\k<a>", "\\u12", "[a-\\d]", "\\uD83D\\uDE00", "a**"]
return pats.map(p => { try { new RegExp(p, "u"); return "ok" } catch (e) { return e.message } })"#,
            r#"{"ok":true,"value":["ok","new RegExp(...) received \"\\\\p{Foo}\", which is not a valid regular expression pattern (invalid property expression). To match special characters like ( ) [ ] { } + * ? . literally, escape them with a backslash (e.g. \"\\\\(\") or test for them with String.includes instead.","new RegExp(...) received \"\\\\P{\", which is not a valid regular expression pattern (invalid property expression). To match special characters like ( ) [ ] { } + * ? . literally, escape them with a backslash (e.g. \"\\\\(\") or test for them with String.includes instead.","new RegExp(...) received \"\\\\u{110000}\", which is not a valid regular expression pattern (invalid Unicode code point \\u{} escape). To match special characters like ( ) [ ] { } + * ? . literally, escape them with a backslash (e.g. \"\\\\(\") or test for them with String.includes instead.","new RegExp(...) received \"\\\\u{zz}\", which is not a valid regular expression pattern (invalid Unicode code point \\u{} escape). To match special characters like ( ) [ ] { } + * ? . literally, escape them with a backslash (e.g. \"\\\\(\") or test for them with String.includes instead.","new RegExp(...) received \"\\\\x\", which is not a valid regular expression pattern (invalid escaped character for Unicode pattern). To match special characters like ( ) [ ] { } + * ? . literally, escape them with a backslash (e.g. \"\\\\(\") or test for them with String.includes instead.","new RegExp(...) received \"\\\\-\", which is not a valid regular expression pattern (invalid escaped character for Unicode pattern). To match special characters like ( ) [ ] { } + * ? . literally, escape them with a backslash (e.g. \"\\\\(\") or test for them with String.includes instead.","new RegExp(...) received \"\\\\q\", which is not a valid regular expression pattern (invalid escaped character for Unicode pattern). To match special characters like ( ) [ ] { } + * ? . literally, escape them with a backslash (e.g. \"\\\\(\") or test for them with String.includes instead.","new RegExp(...) received \"[\\\\d-z]\", which is not a valid regular expression pattern (invalid range in character class for Unicode pattern). To match special characters like ( ) [ ] { } + * ? . literally, escape them with a backslash (e.g. \"\\\\(\") or test for them with String.includes instead.","new RegExp(...) received \"{\", which is not a valid regular expression pattern (incomplete {} quantifier for Unicode pattern). To match special characters like ( ) [ ] { } + * ? . literally, escape them with a backslash (e.g. \"\\\\(\") or test for them with String.includes instead.","new RegExp(...) received \"}\", which is not a valid regular expression pattern (unmatched ] or } bracket for Unicode pattern). To match special characters like ( ) [ ] { } + * ? . literally, escape them with a backslash (e.g. \"\\\\(\") or test for them with String.includes instead.","new RegExp(...) received \"]\", which is not a valid regular expression pattern (unmatched ] or } bracket for Unicode pattern). To match special characters like ( ) [ ] { } + * ? . literally, escape them with a backslash (e.g. \"\\\\(\") or test for them with String.includes instead.","new RegExp(...) received \"a{1\", which is not a valid regular expression pattern (incomplete {} quantifier for Unicode pattern). To match special characters like ( ) [ ] { } + * ? . literally, escape them with a backslash (e.g. \"\\\\(\") or test for them with String.includes instead.","new RegExp(...) received \"\\\\1\", which is not a valid regular expression pattern (invalid backreference for Unicode pattern). To match special characters like ( ) [ ] { } + * ? . literally, escape them with a backslash (e.g. \"\\\\(\") or test for them with String.includes instead.","new RegExp(...) received \"\\\\k<a>\", which is not a valid regular expression pattern (invalid \\k<> named backreference). To match special characters like ( ) [ ] { } + * ? . literally, escape them with a backslash (e.g. \"\\\\(\") or test for them with String.includes instead.","new RegExp(...) received \"\\\\c1\", which is not a valid regular expression pattern (invalid \\c escape for Unicode pattern). To match special characters like ( ) [ ] { } + * ? . literally, escape them with a backslash (e.g. \"\\\\(\") or test for them with String.includes instead.","new RegExp(...) received \"[\\\\c1]\", which is not a valid regular expression pattern (invalid \\c escape for Unicode pattern). To match special characters like ( ) [ ] { } + * ? . literally, escape them with a backslash (e.g. \"\\\\(\") or test for them with String.includes instead.","new RegExp(...) received \"\\\\8\", which is not a valid regular expression pattern (invalid backreference for Unicode pattern). To match special characters like ( ) [ ] { } + * ? . literally, escape them with a backslash (e.g. \"\\\\(\") or test for them with String.includes instead.","ok","new RegExp(...) received \"\\\\u12\", which is not a valid regular expression pattern (invalid Unicode \\u escape). To match special characters like ( ) [ ] { } + * ? . literally, escape them with a backslash (e.g. \"\\\\(\") or test for them with String.includes instead.","new RegExp(...) received \"[a-\\\\d]\", which is not a valid regular expression pattern (invalid range in character class for Unicode pattern). To match special characters like ( ) [ ] { } + * ? . literally, escape them with a backslash (e.g. \"\\\\(\") or test for them with String.includes instead.","ok","new RegExp(...) received \"a**\", which is not a valid regular expression pattern (nothing to repeat). To match special characters like ( ) [ ] { } + * ? . literally, escape them with a backslash (e.g. \"\\\\(\") or test for them with String.includes instead."],"toolCalls":[]}"#,
        ),
        (
            "probe s2.txt",
            r#"const pats = ["[a&&b]", "[a--b]", "[\\q{abc}]", "[^\\q{ab}]", "[a&&&b]", "[(]", "[a-z&&[aeiou]]", "\\p{RGI_Emoji}", "[\\p{RGI_Emoji}--\\q{x}]", "[a-z--b]", "[[a-z]--b]", "[|]", "[a|]"]
return pats.map(p => { try { const r = new RegExp(p, "v"); return r.test("a") } catch (e) { return e.message } })"#,
            r#"{"ok":true,"value":[false,true,false,"new RegExp(...) received \"[^\\\\q{ab}]\", which is not a valid regular expression pattern (negated class set may contain strings). To match special characters like ( ) [ ] { } + * ? . literally, escape them with a backslash (e.g. \"\\\\(\") or test for them with String.includes instead.","new RegExp(...) received \"[a&&&b]\", which is not a valid regular expression pattern (invalid class set character). To match special characters like ( ) [ ] { } + * ? . literally, escape them with a backslash (e.g. \"\\\\(\") or test for them with String.includes instead.","new RegExp(...) received \"[(]\", which is not a valid regular expression pattern (invalid class set character). To match special characters like ( ) [ ] { } + * ? . literally, escape them with a backslash (e.g. \"\\\\(\") or test for them with String.includes instead.","new RegExp(...) received \"[a-z&&[aeiou]]\", which is not a valid regular expression pattern (invalid operation in class set). To match special characters like ( ) [ ] { } + * ? . literally, escape them with a backslash (e.g. \"\\\\(\") or test for them with String.includes instead.",false,false,"new RegExp(...) received \"[a-z--b]\", which is not a valid regular expression pattern (invalid operation in class set). To match special characters like ( ) [ ] { } + * ? . literally, escape them with a backslash (e.g. \"\\\\(\") or test for them with String.includes instead.",true,"new RegExp(...) received \"[|]\", which is not a valid regular expression pattern (invalid class set character). To match special characters like ( ) [ ] { } + * ? . literally, escape them with a backslash (e.g. \"\\\\(\") or test for them with String.includes instead.","new RegExp(...) received \"[a|]\", which is not a valid regular expression pattern (invalid class set character). To match special characters like ( ) [ ] { } + * ? . literally, escape them with a backslash (e.g. \"\\\\(\") or test for them with String.includes instead."],"toolCalls":[]}"#,
        ),
        (
            "probe s2.txt",
            r#"return ["gg", "x", "uv", "G", " g", "dgimsuy", "ydsmig"].map(f => { try { return new RegExp("a", f).flags } catch (e) { return e.message } })"#,
            r#"{"ok":true,"value":["new RegExp(...) received invalid flags \"gg\" (Invalid flags supplied to RegExp constructor.). Valid flags are d, g, i, m, s, u, v, and y.","new RegExp(...) received invalid flags \"x\" (Invalid flags supplied to RegExp constructor.). Valid flags are d, g, i, m, s, u, v, and y.","new RegExp(...) received invalid flags \"uv\" (Invalid flags supplied to RegExp constructor.). Valid flags are d, g, i, m, s, u, v, and y.","new RegExp(...) received invalid flags \"G\" (Invalid flags supplied to RegExp constructor.). Valid flags are d, g, i, m, s, u, v, and y.","new RegExp(...) received invalid flags \" g\" (Invalid flags supplied to RegExp constructor.). Valid flags are d, g, i, m, s, u, v, and y.","dgimsuy","dgimsy"],"toolCalls":[]}"#,
        ),
        (
            "probe s2.txt",
            r#"return [new RegExp("a", null), 1].map(String)"#,
            r#"{"ok":false,"error":{"kind":"ExecutionFailure","message":"SyntaxError: RegExp flags must be a string of flag characters (e.g. \"g\", \"gi\"), not null. (line 1, col 9)","location":{"line":1,"column":9}},"toolCalls":[]}"#,
        ),
        (
            "probe s2.txt",
            r#"try { new RegExp("a", 1) } catch (e) { return e.message }"#,
            r#"{"ok":true,"value":"RegExp flags must be a string of flag characters (e.g. \"g\", \"gi\"), not number.","toolCalls":[]}"#,
        ),
        (
            "probe s2.txt",
            r#"try { new RegExp("a", {}) } catch (e) { return e.message }"#,
            r#"{"ok":true,"value":"RegExp flags must be a string of flag characters (e.g. \"g\", \"gi\"), not object.","toolCalls":[]}"#,
        ),
        (
            "probe s2.txt",
            r#"try { RegExp("a", undefined).flags } catch (e) { return e.message }"#,
            r#"{"ok":true,"value":null,"toolCalls":[]}"#,
        ),
        (
            "probe s2.txt",
            r#"return [String(new RegExp("/")), String(new RegExp("a/b")), String(new RegExp("[/]")), String(new RegExp("\\/")), String(new RegExp("\n")), String(new RegExp("")), new RegExp("").source, String(new RegExp(" ")), String(/[/]\//), new RegExp("\\\n").source, String(new RegExp("\r\\\r"))]"#,
            r#"{"ok":true,"value":["/\\//","/a\\/b/","/[/]/","/\\//","/\\n/","/(?:)/","(?:)","/\\u2028/","/[/]\\//","\\n","/\\r\\r/"],"toolCalls":[]}"#,
        ),
        (
            "probe s2.txt",
            r#"const r = /a(b)?/g; return [r.source, r.flags, r.global, r.ignoreCase, r.multiline, r.sticky, r.unicode, r.unicodeSets, r.dotAll, r.hasIndices, r.lastIndex, typeof r, r instanceof RegExp, Object.keys(r)]"#,
            r#"{"ok":true,"value":["a(b)?","g",true,false,false,false,false,false,false,false,0,"object",true,[]],"toolCalls":[]}"#,
        ),
        (
            "probe s2.txt",
            r#"const r = /a/g; const s = new RegExp(r); const t = new RegExp(r, "i"); return [String(s), String(t), s === r, RegExp(r) === r]"#,
            r#"{"ok":true,"value":["/a/g","/a/i",false,false],"toolCalls":[]}"#,
        ),
        (
            "probe s2.txt",
            r#"try { return RegExp.prototype.global } catch (e) { return e.message }"#,
            r#"{"ok":true,"value":"RegExp.prototype.global called on incompatible receiver a data object.","toolCalls":[]}"#,
        ),
        (
            "probe s2.txt",
            r#"try { return RegExp.prototype.toString.call({}) } catch (e) { return e.message }"#,
            r#"{"ok":true,"value":"RegExp.prototype.toString called on incompatible receiver a data object.","toolCalls":[]}"#,
        ),
        (
            "probe s2.txt",
            r#"try { return RegExp.prototype.exec.call("a", "a") } catch (e) { return e.message }"#,
            r#"{"ok":true,"value":"RegExp.prototype.exec called on incompatible receiver a string.","toolCalls":[]}"#,
        ),
        (
            "probe s2.txt",
            r#"try { return String(RegExp.prototype) } catch (e) { return e.message }"#,
            r#"{"ok":true,"value":"RegExp.prototype.toString called on incompatible receiver a data object.","toolCalls":[]}"#,
        ),
        (
            "probe s2.txt",
            r#"const r = /a/g; const out = []; while (r.test("aXaXa")) out.push(r.lastIndex); out.push(r.lastIndex); return out"#,
            r#"{"ok":true,"value":[1,3,5,0],"toolCalls":[]}"#,
        ),
        (
            "probe s2.txt",
            r#"const r = /a/y; return [r.test("ba"), r.lastIndex, (r.lastIndex = 1, r.test("ba")), r.lastIndex, r.test("ba"), r.lastIndex]"#,
            r#"{"ok":true,"value":[false,0,true,2,false,0],"toolCalls":[]}"#,
        ),
        (
            "probe s2.txt",
            r#"const r = /a/g; r.lastIndex = 10; const a = r.exec("aaa"); return [a, r.lastIndex]"#,
            r#"{"ok":true,"value":[null,0],"toolCalls":[]}"#,
        ),
        (
            "probe s2.txt",
            r#"const r = /a/g; r.lastIndex = -5; const a = r.exec("aaa"); return [a && a.index, r.lastIndex]"#,
            r#"{"ok":true,"value":[0,1],"toolCalls":[]}"#,
        ),
        (
            "probe s2.txt",
            r#"const r = /a/g; r.lastIndex = "2"; const a = r.exec("aaa"); return [a && a.index, r.lastIndex]"#,
            r#"{"ok":true,"value":[2,3],"toolCalls":[]}"#,
        ),
        (
            "probe s2.txt",
            r#"const r = /a/g; r.lastIndex = 1.7; const a = r.exec("aaa"); return [a && a.index, r.lastIndex]"#,
            r#"{"ok":true,"value":[1,2],"toolCalls":[]}"#,
        ),
        (
            "probe s2.txt",
            r#"const r = /a/; r.lastIndex = 7; const a = r.exec("aaa"); return [a.index, r.lastIndex]"#,
            r#"{"ok":true,"value":[0,7],"toolCalls":[]}"#,
        ),
        (
            "probe s2.txt",
            r#"const r = /a/g; r.lastIndex = NaN; const a = r.exec("baa"); return [a.index, r.lastIndex]"#,
            r#"{"ok":true,"value":[1,2],"toolCalls":[]}"#,
        ),
        (
            "probe s2.txt",
            r#"const r = /a/g; r.lastIndex = "x"; return [r.lastIndex, typeof r.lastIndex]"#,
            r#"{"ok":true,"value":[null,"number"],"toolCalls":[]}"#,
        ),
        (
            "probe s2.txt",
            r#"const m = /(?<year>\d{4})-(?<month>\d{2})(-(?<day>\d\d))?/.exec("on 2024-05!"); return [m, m.index, m.input, m.groups, Object.keys(m), m.length]"#,
            r#"{"ok":true,"value":[["2024-05","2024","05",null,null],3,"on 2024-05!",{"year":"2024","month":"05"},["0","1","2","3","4","index","input","groups"],5],"toolCalls":[]}"#,
        ),
        (
            "probe s2.txt",
            r#"const m = /(?<year>\d{4})-(?<month>\d{2})(-(?<day>\d\d))?/d.exec("on 2024-05!"); return [m.indices, m.indices.groups, Object.keys(m), Object.keys(m.indices)]"#,
            r#"{"ok":true,"value":[[[3,10],[3,7],[8,10],null,null],{"year":[3,7],"month":[8,10]},["0","1","2","3","4","index","input","groups","indices"],["0","1","2","3","4","groups"]],"toolCalls":[]}"#,
        ),
        (
            "probe s2.txt",
            r#"const m = /a(x)?/d.exec("ba"); return [m.indices, m.indices.groups, "groups" in m.indices, m.groups, "groups" in m]"#,
            r#"{"ok":true,"value":[[[1,2],null],null,true,null,false],"toolCalls":[]}"#,
        ),
        (
            "probe s2.txt",
            r#"const m = /(?<a>x)|(?<a>y)/.exec("y"); return [m, m.groups]"#,
            r#"{"ok":true,"value":[["y",null,"y"],{"a":"y"}],"toolCalls":[]}"#,
        ),
        (
            "probe s2.txt",
            r#"return [/(?<a>x)|(?<a>y)/.exec("x").groups, "xy".replace(/(?<a>x)|(?<a>y)/g, "[$<a>]")]"#,
            r#"{"ok":true,"value":[{"a":"x"},"[x][y]"],"toolCalls":[]}"#,
        ),
        (
            "probe s2.txt",
            r#"return ["😀".length, /^.$/.test("😀"), /^.$/u.test("😀"), /^.$/v.test("😀"), "😀x".match(/./gu), "😀x".match(/./g).length]"#,
            r#"{"ok":true,"value":[2,false,true,true,["😀","x"],3],"toolCalls":[]}"#,
        ),
        (
            "probe s2.txt",
            r#"return [/\u{1F600}/u.test("😀"), /[😀]/u.test("😀"), /[\uD83D]/.test("😀"), /\uD83D/u.test("😀"), /^[^x]$/u.test("😀")]"#,
            r#"{"ok":true,"value":[true,true,true,false,true],"toolCalls":[]}"#,
        ),
        (
            "probe s2.txt",
            r#"const r = /(?:)/gu; return "😀😀".replace(r, "-")"#,
            r#"{"ok":true,"value":"-😀-😀-","toolCalls":[]}"#,
        ),
        (
            "probe s2.txt",
            r#"const r = /(?:)/g; return "😀".replace(r, "-").length"#,
            r#"{"ok":true,"value":5,"toolCalls":[]}"#,
        ),
        (
            "probe s2.txt",
            r#"return [/\p{Lu}+/u.exec("abcDEF")[0], /\p{Script=Greek}/u.test("α"), /\P{L}/u.test("a"), /[\p{N}--[0-9]]/v.test("5"), /[\p{N}--[0-9]]/v.test("٥")]"#,
            r#"{"ok":true,"value":["DEF",true,false,false,true],"toolCalls":[]}"#,
        ),
        (
            "probe s2.txt",
            r#"return [/(?<=\$)\d+/.exec("cost $42")[0], /(?<!\$)\b\d+/.exec("cost $42 or 17")[0], /(a)\1/.test("aa"), /\b\w/g.exec("hi there").index]"#,
            r#"{"ok":true,"value":["42","17",true,0],"toolCalls":[]}"#,
        ),
        (
            "probe s2.txt",
            r#"return [/^b/m.exec("a\nb").index, /a.c/s.test("a\nc"), /a.c/.test("a\nc"), /a$/.test("a\n"), /a$/m.test("a\n")]"#,
            r#"{"ok":true,"value":[2,true,false,false,true],"toolCalls":[]}"#,
        ),
        (
            "probe s2.txt",
            r#"return ["a-b_c d".split(/[-_ ]/), "a1b2c3".split(/(\d)/), "abc".split(/(?:)/), "".split(/x/), "".split(/(?:)/), "abc".split(/b/, 1), "a,b,c".split(/,/, 0), "test".split(/(t)/)]"#,
            r#"{"ok":true,"value":[["a","b","c","d"],["a","1","b","2","c","3",""],["a","b","c"],[""],[],["a"],[],["","t","es","t",""]],"toolCalls":[]}"#,
        ),
        (
            "probe s2.txt",
            r#"return ["😀a😀".split(/(?:)/u), "😀a".split("").map(s => s.charCodeAt(0)), "abc".split(/(b)(x)?/), "aXbxc".split(/x/i, 2)]"#,
            r#"{"ok":true,"value":[["😀","a","😀"],[55357,56832,97],["a","b",null,"c"],["a","b"]],"toolCalls":[]}"#,
        ),
        (
            "probe s2.txt",
            r#"return ["abc".split("", 2), "a,b".split(",", -1), "a,b".split(",", 4294967297), "a,b".split(",", NaN), "a".split(",", undefined), "abc".split(undefined), "abc".split(undefined, 0), "abc".split(null)]"#,
            r#"{"ok":true,"value":[["a","b"],["a","b"],["a"],[],["a"],["abc"],[],["abc"]],"toolCalls":[]}"#,
        ),
        (
            "probe s2.txt",
            r#"return "$1-$2".replace(/(\w)/g, "[$1|$2|$0|$00|$01|$10|$&|$`|$'|$$|$<x>|$]")"#,
            r#"{"ok":true,"value":"$[1|$2|$0|$00|1|10|1|$|-$2|$|$<x>|$]-$[2|$2|$0|$00|2|20|2|$1-$||$|$<x>|$]","toolCalls":[]}"#,
        ),
        (
            "probe s2.txt",
            r#"return "abcdefghijk".replace(/(a)(b)(c)(d)(e)(f)(g)(h)(i)(j)(k)/, "$11-$10-$1-$111-$012-$99-$100")"#,
            r#"{"ok":true,"value":"k-j-a-k1-a2-i9-j0","toolCalls":[]}"#,
        ),
        (
            "probe s2.txt",
            r#"return ["abc".replace(/(b)/, "$2"), "abc".replace(/(?<n>b)/, "[$<n>][$<m>][$<n]"), "abc".replace("b", "[$&$`$'$1$<n>]"), "aaa".replaceAll("a", "$&$&"), "abc".replace(/b/, "$")]"#,
            r#"{"ok":true,"value":["a$2c","a[b][][$<n]c","a[bac$1$<n>]c","aaaaaa","a$c"],"toolCalls":[]}"#,
        ),
        (
            "probe s2.txt",
            r#"return ["xyz".replaceAll("", "-"), "aaaa".replaceAll("aa", "b"), "abc".replace("", "-"), "abcabc".replaceAll(/b/g, (m, o, s) => o), "x".replace(/(y)?x/, (m, p1, o, s) => [m, p1, o, s].join(","))]"#,
            r#"{"ok":true,"value":["-x-y-z-","bb","-abc","a1ca4c","x,,0,x"],"toolCalls":[]}"#,
        ),
        (
            "probe s2.txt",
            r#"return "2024-05-06".replace(/(?<y>\d+)-(?<m>\d+)-(?<d>\d+)/, (...args) => JSON.stringify(args))"#,
            r#"{"ok":true,"value":"[\"2024-05-06\",\"2024\",\"05\",\"06\",0,\"2024-05-06\",{\"y\":\"2024\",\"m\":\"05\",\"d\":\"06\"}]","toolCalls":[]}"#,
        ),
        (
            "probe s2.txt",
            r#"return "aXbX".replace(/x/gi, function (m) { return this === undefined })"#,
            r#"{"ok":true,"value":"atruebtrue","toolCalls":[]}"#,
        ),
        (
            "probe s2.txt",
            r#"const re = /a/g; re.lastIndex = 2; const out = "aaa".replace(re, (m, o) => re.lastIndex + ":" + o); return [out, re.lastIndex]"#,
            r#"{"ok":true,"value":["0:00:10:2",0],"toolCalls":[]}"#,
        ),
        (
            "probe s2.txt",
            r#"const re = /a/g; const all = [..."banana".matchAll(re)]; return [all.map(m => [m[0], m.index]), re.lastIndex, all.length]"#,
            r#"{"ok":true,"value":[[["a",1],["a",3],["a",5]],0,3],"toolCalls":[]}"#,
        ),
        (
            "probe s2.txt",
            r#"const re = /a/g; re.lastIndex = 3; const all = "banana".matchAll(re); return [all.map(m => m.index), re.lastIndex, Array.isArray(all)]"#,
            r#"{"ok":true,"value":[[3,5],3,true],"toolCalls":[]}"#,
        ),
        (
            "probe s2.txt",
            r#"try { "a".matchAll(/a/) } catch (e) { return e.message }"#,
            r#"{"ok":true,"value":"String.matchAll requires a regular expression with the global (g) flag: write /a/g, or use String.match for a single match.","toolCalls":[]}"#,
        ),
        (
            "probe s2.txt",
            r#"return ["abc".matchAll("b").map(m => m.index), "a.b".matchAll(".").length, "x".matchAll().length, "".matchAll().map(m => [m[0], m.index])]"#,
            r#"{"ok":true,"value":[[1],3,2,[["",0]]],"toolCalls":[]}"#,
        ),
        (
            "probe s2.txt",
            r#"try { return "a".matchAll("(") } catch (e) { return e.message }"#,
            r#"{"ok":true,"value":"String.matchAll received the string \"(\", which is not a valid regular expression pattern (missing )). To match special characters like ( ) [ ] { } + * ? . literally, escape them with a backslash (e.g. \"\\\\(\") or test for them with String.includes instead.","toolCalls":[]}"#,
        ),
        (
            "probe s2.txt",
            r#"try { return "a".search("[") } catch (e) { return e.message }"#,
            r#"{"ok":true,"value":"String.search received the string \"[\", which is not a valid regular expression pattern (missing terminating ] for character class). To match special characters like ( ) [ ] { } + * ? . literally, escape them with a backslash (e.g. \"\\\\(\") or test for them with String.includes instead.","toolCalls":[]}"#,
        ),
        (
            "probe s2.txt",
            r#"try { return "a".split(/(/) } catch (e) { return e.message }"#,
            r#"{"ok":false,"error":{"kind":"ParseError","message":"Invalid regular expression: /(/: Unterminated group (1:24)"},"toolCalls":[]}"#,
        ),
        (
            "probe s2.txt",
            r#"return ["abc".search(/c/), "abc".search("b"), "abc".search(), "a.c".search("."), "null".search(null), "abc".search(/x/), "a1b".match(1), "abc".match(), "abc".match(undefined)]"#,
            r#"{"ok":true,"value":[2,1,0,0,0,-1,["1"],[""],[""]],"toolCalls":[]}"#,
        ),
        (
            "probe s2.txt",
            r#"const re = /b/g; re.lastIndex = 2; return ["abcb".search(re), re.lastIndex]"#,
            r#"{"ok":true,"value":[1,2],"toolCalls":[]}"#,
        ),
        (
            "probe s2.txt",
            r#"const re = /a/g; re.lastIndex = 2; return ["aaa".match(re), re.lastIndex]"#,
            r#"{"ok":true,"value":[["a","a","a"],0],"toolCalls":[]}"#,
        ),
        (
            "probe s2.txt",
            r#"return ["abc".match(/(?<x>b)/), "abc".match(/(?<x>b)/).groups, "abc".match(/z/), "abab".match(/a/g), "abab".match(/z/g)]"#,
            r#"{"ok":true,"value":[["b","b"],{"x":"b"},null,["a","a"],null],"toolCalls":[]}"#,
        ),
        (
            "probe s2.txt",
            r#"return [RegExp.escape("a.b*c"), RegExp.escape("1abc"), RegExp.escape("_x"), RegExp.escape(" -,=<>#&!%:;@~'`\""), RegExp.escape("\t\n\v\f\r"), RegExp.escape(" ﻿ "), RegExp.escape("😀\uD800"), RegExp.escape("^$\\.*+?()[]{}|/"), RegExp.escape("")]"#,
            r#"{"ok":true,"value":["\\x61\\.b\\*c","\\x31abc","_x","\\x20\\x2d\\x2c\\x3d\\x3c\\x3e\\x23\\x26\\x21\\x25\\x3a\\x3b\\x40\\x7e\\x27\\x60\\x22","\\t\\n\\v\\f\\r","\\u2028\\ufeff\\xa0","😀\\ud800","\\^\\$\\\\\\.\\*\\+\\?\\(\\)\\[\\]\\{\\}\\|\\/",""],"toolCalls":[]}"#,
        ),
        (
            "probe s2.txt",
            r#"try { RegExp.escape(1) } catch (e) { return e.message }"#,
            r#"{"ok":true,"value":"RegExp.escape expects a string.","toolCalls":[]}"#,
        ),
        (
            "probe s2.txt",
            r#"return [RegExp.name, RegExp.length, RegExp.escape.length, RegExp.prototype.exec.length, RegExp.prototype.test.length, RegExp.prototype.toString.length, Object.getOwnPropertyNames(RegExp.prototype)]"#,
            r#"{"ok":false,"error":{"kind":"ExecutionFailure","message":"TypeError: Object.getOwnPropertyNames is not a function. (line 1, col 153)","location":{"line":1,"column":153}},"toolCalls":[]}"#,
        ),
        (
            "probe s2.txt",
            r#"return [String.name, String.length, String.fromCharCode.length, String.raw.length, Object.getOwnPropertyNames(String.prototype), "".replace.length, "".split.length, "".padStart.length, "".concat.length, "".at.length, ""[Symbol.iterator].name]"#,
            r#"{"ok":false,"error":{"kind":"ExecutionFailure","message":"TypeError: Object.getOwnPropertyNames is not a function. (line 1, col 84)","location":{"line":1,"column":84}},"toolCalls":[]}"#,
        ),
        (
            "probe s2.txt",
            r#"return [String(), String(undefined), String(null), String(1.5), String([1, [2, 3]]), String({}), String(/a/g), String({ toString() { return "x" } })]"#,
            r#"{"ok":true,"value":["","undefined","null","1.5","1,2,3","[object Object]","/a/g","x"],"toolCalls":[]}"#,
        ),
        (
            "probe s2.txt",
            r#"try { return new String("a") } catch (e) { return e.message }"#,
            r#"{"ok":true,"value":"new String(...) is not supported; call String(...) without new instead.","toolCalls":[]}"#,
        ),
        (
            "probe s4.txt",
            r#"const log = []; const o = (n, v) => ({ toString() { log.push(n + ".toString"); return v }, valueOf() { log.push(n + ".valueOf"); return v } })
const r = String.prototype.slice.call(o("this", "abcdef"), o("a", 1), o("b", 3)); return [r, log]"#,
            r#"{"ok":true,"value":["bc",["this.toString","a.valueOf","b.valueOf"]],"toolCalls":[]}"#,
        ),
        (
            "probe s4.txt",
            r#"const log = []; const o = (n, v) => ({ toString() { log.push(n + ".toString"); return v }, valueOf() { log.push(n + ".valueOf"); return v } })
const r = "a-b-c".split(o("sep", "-"), o("lim", 2)); return [r, log]"#,
            r#"{"ok":true,"value":[["a","b"],["lim.valueOf","sep.toString"]],"toolCalls":[]}"#,
        ),
        (
            "probe s4.txt",
            r#"const log = []; const o = (n, v) => ({ toString() { log.push(n + ".toString"); return v }, valueOf() { log.push(n + ".valueOf"); return v } })
const r = "abc".replace(o("pat", "b"), o("rep", "X")); return [r, log]"#,
            r#"{"ok":true,"value":["aXc",["pat.toString","rep.toString"]],"toolCalls":[]}"#,
        ),
        (
            "probe s4.txt",
            r#"const log = []; const o = (n, v) => ({ toString() { log.push(n + ".toString"); return v }, valueOf() { log.push(n + ".valueOf"); return v } })
const r = [o("x", "abc").includes(o("s", "b"), o("p", 0)), "abc".padStart(o("n", 5), o("f", "*")), "abc".concat(o("c1", 1), o("c2", 2)), "abc".indexOf(o("i", "c"), o("j", 1)), "x".repeat(o("r", 3)), "abc".localeCompare(o("l", "abd")), "abc".normalize(o("f2", "NFD"))]; return [r, log]"#,
            r#"{"ok":false,"error":{"kind":"ExecutionFailure","message":"TypeError: o(...).includes is not a function. (line 2, col 12)","location":{"line":2,"column":12}},"toolCalls":[]}"#,
        ),
        (
            "probe s4.txt",
            r#"const log = []; const o = (n, v) => ({ toString() { log.push(n + ".toString"); return v }, valueOf() { log.push(n + ".valueOf"); return v } })
return ["a1b".match(o("m", "\\d")), "a1b".search(o("s", "b")), "aXbX".matchAll(o("ma", "X")).length, log]"#,
            r#"{"ok":true,"value":[["1"],2,2,["m.toString","s.toString","ma.toString"]],"toolCalls":[]}"#,
        ),
        (
            "probe s4.txt",
            r#"try { return String.prototype.trim.call(null) } catch (e) { return e.message }"#,
            r#"{"ok":true,"value":"String.prototype.trim called on null or undefined.","toolCalls":[]}"#,
        ),
        (
            "probe s4.txt",
            r#"try { return String.prototype.includes.call(undefined, "a") } catch (e) { return e.message }"#,
            r#"{"ok":true,"value":"String.prototype.includes called on null or undefined.","toolCalls":[]}"#,
        ),
        (
            "probe s4.txt",
            r#"try { return String.prototype[Symbol.iterator].call(null) } catch (e) { return e.message }"#,
            r#"{"ok":true,"value":"String.prototype.[Symbol.iterator] called on null or undefined.","toolCalls":[]}"#,
        ),
        (
            "probe s4.txt",
            r#"return [...String.prototype[Symbol.iterator].call(12)]"#,
            r#"{"ok":true,"value":["1","2"],"toolCalls":[]}"#,
        ),
        (
            "probe s4.txt",
            r#"try { return "abc".indexOf(() => 1) } catch (e) { return [e.name, e.message] }"#,
            r#"{"ok":true,"value":["TypeError","String.indexOf expects argument 1 to be a data value."],"toolCalls":[]}"#,
        ),
        (
            "probe s4.txt",
            r#"try { return "abc".padEnd(5, async () => 1) } catch (e) { return [e.name, e.message] }"#,
            r#"{"ok":true,"value":["TypeError","String.padEnd expects argument 2 to be a data value."],"toolCalls":[]}"#,
        ),
        (
            "probe s4.txt",
            r#"try { return "abc".concat("x", () => 1) } catch (e) { return [e.name, e.message] }"#,
            r#"{"ok":true,"value":["TypeError","String.concat expects argument 2 to be a data value."],"toolCalls":[]}"#,
        ),
        (
            "probe s4.txt",
            r#"try { return "abc".split(",", () => 1) } catch (e) { return [e.name, e.message] }"#,
            r#"{"ok":true,"value":["TypeError","String.split expects argument 2 to be a data value."],"toolCalls":[]}"#,
        ),
        (
            "probe s4.txt",
            r#"try { return "abc".replace(() => 1, "x") } catch (e) { return [e.name, e.message] }"#,
            r#"{"ok":true,"value":["TypeError","String.replace expects argument 1 to be a data value."],"toolCalls":[]}"#,
        ),
        (
            "probe s4.txt",
            r#"try { return "abc".replace("b", tools) } catch (e) { return [e.name, e.message] }"#,
            r#"{"ok":true,"value":["TypeError","String.replace expects argument 2 to be a data value."],"toolCalls":[]}"#,
        ),
        (
            "probe s4.txt",
            r#"try { return "abc".startsWith(/a/) } catch (e) { return e.message }"#,
            r#"{"ok":true,"value":"String.startsWith cannot take a regular expression; use regex.test(string) or String.search instead.","toolCalls":[]}"#,
        ),
        (
            "probe s4.txt",
            r#"try { return "abc".endsWith(/a/, 1) } catch (e) { return e.message }"#,
            r#"{"ok":true,"value":"String.endsWith cannot take a regular expression; use regex.test(string) or String.search instead.","toolCalls":[]}"#,
        ),
        (
            "probe s4.txt",
            r#"return ["abc".includes(1), "a1c".includes(1), "abc".slice("1"), "abc".includes(), "undefined".includes(), "abc".startsWith("b", 1), "abc".endsWith("b", 2), "abc".endsWith("c", 99), "abc".startsWith("", 99), "abc".includes("", 99)]"#,
            r#"{"ok":true,"value":[false,true,"bc",false,true,true,true,true,true,true],"toolCalls":[]}"#,
        ),
        (
            "probe s4.txt",
            r#"return ["abcabc".indexOf("c", -5), "abcabc".indexOf("", 99), "abcabc".lastIndexOf("c"), "abcabc".lastIndexOf("c", 4), "abcabc".lastIndexOf("c", -1), "abcabc".lastIndexOf("", 3), "abcabc".lastIndexOf("", NaN), "abc".lastIndexOf("abcd"), "abcabc".lastIndexOf("a", Infinity), "aaa".lastIndexOf("a", 0.9)]"#,
            r#"{"ok":true,"value":[2,6,5,2,-1,3,6,-1,3,0],"toolCalls":[]}"#,
        ),
        (
            "probe s4.txt",
            r#"return ["abc".slice(-2), "abc".slice(1, -1), "abc".slice(2, 1), "abc".slice(-99, 99), "abc".slice(NaN), "abc".substring(2, 0), "abc".substring(-1, 2), "abc".substring(NaN, 2), "abc".substr(-2), "abc".substr(1, 1), "abc".substr(-5, 2), "abc".substr(1, -1), "abc".substr(1, Infinity), "abc".substr(-Infinity, 2)]"#,
            r#"{"ok":true,"value":["bc","b","","abc","abc","ab","ab","ab","bc","b","ab","","bc","ab"],"toolCalls":[]}"#,
        ),
        (
            "probe s4.txt",
            r#"return ["abc".at(), "abc".at(-4), "abc".at(3), "abc".at("1"), "abc".at(1.9), "abc".at(-0.5), "abc".charAt(-1), "abc".charAt(1.5), "abc".charCodeAt(), "😀".codePointAt(0), "😀".codePointAt(1), "😀".charCodeAt(0), "abc".codePointAt(-1)]"#,
            r#"{"ok":true,"value":["a",null,null,"b","b","a","","b",97,128512,56832,55357,null],"toolCalls":[]}"#,
        ),
        (
            "probe s4.txt",
            r#"return ["ab".repeat(0), "".repeat(5), "ab".repeat(2.9), "ab".repeat("2"), "ab".padStart(5, "xyz"), "ab".padEnd(1), "ab".padStart(NaN), "ab".padStart(5, undefined), "ab".padStart(5, null), "ab".padEnd(4.9, "-")]"#,
            r#"{"ok":true,"value":["","","abab","abab","xyzab","ab","ab","   ab","nulab","ab--"],"toolCalls":[]}"#,
        ),
        (
            "probe s4.txt",
            r#"try { return "ab".repeat(-0.5) } catch (e) { return e.message }"#,
            r#"{"ok":true,"value":"String.repeat expects a finite non-negative count.","toolCalls":[]}"#,
        ),
        (
            "probe s4.txt",
            r#"try { return "".repeat(1e10) } catch (e) { return e.message }"#,
            r#"{"ok":true,"value":"","toolCalls":[]}"#,
        ),
        (
            "probe s4.txt",
            r#"try { return "a".padEnd(Infinity) } catch (e) { return e.message }"#,
            r#"{"ok":true,"value":"Invalid string length","toolCalls":[]}"#,
        ),
        (
            "probe s4.txt",
            r#"try { return "a".repeat(2 ** 24).concat("b") } catch (e) { return e.message }"#,
            r#"{"ok":true,"value":"Invalid string length","toolCalls":[]}"#,
        ),
        (
            "probe s4.txt",
            r#"return "a".repeat(2 ** 24).length"#,
            r#"{"ok":true,"value":16777216,"toolCalls":[]}"#,
        ),
        (
            "probe s4.txt",
            r#"return ["  \t x \n".trim(), " x﻿".trimStart(), "x　 ".trimEnd(), " x ".trimLeft(), " x ".trimRight(), "᠎x".trim().length, "\u0085x".trim().length]"#,
            r#"{"ok":true,"value":["x","x﻿","x","x "," x",2,2],"toolCalls":[]}"#,
        ),
        (
            "probe s4.txt",
            r#"return ["İstanbul".toLowerCase(), "straße".toUpperCase(), "ΌΣΟΣ".toLowerCase(), "ﬀ".toUpperCase(), "ǈ".toLowerCase(), "Ǉ".toLowerCase(), "ß".toLocaleUpperCase("de"), "I".toLocaleLowerCase("tr"), "\uD800A".toLowerCase().length]"#,
            r#"{"ok":true,"value":["i̇stanbul","STRASSE","όσος","FF","ǉ","ǉ","SS","i",2],"toolCalls":[]}"#,
        ),
        (
            "probe s4.txt",
            r#"return ["é".normalize(), "é".normalize("NFD").length, "ﬁ".normalize("NFKC"), "ﬁ".normalize("NFKD"), "Å".normalize("NFC") === "Å", "x".normalize(undefined)]"#,
            r#"{"ok":true,"value":["é",2,"fi","fi",true,"x"],"toolCalls":[]}"#,
        ),
        (
            "probe s4.txt",
            r#"try { return "x".normalize(null) } catch (e) { return e.message }"#,
            r#"{"ok":true,"value":"String.normalize expects the form \"NFC\", \"NFD\", \"NFKC\", or \"NFKD\" (got \"null\").","toolCalls":[]}"#,
        ),
        (
            "probe s4.txt",
            r#"try { return "x".normalize("nfc") } catch (e) { return e.message }"#,
            r#"{"ok":true,"value":"String.normalize expects the form \"NFC\", \"NFD\", \"NFKC\", or \"NFKD\" (got \"nfc\").","toolCalls":[]}"#,
        ),
        (
            "probe s4.txt",
            r#"return ["ab\uD800".isWellFormed(), "ab".isWellFormed(), "😀".isWellFormed(), "a\uDC00b".toWellFormed(), "😀".toWellFormed()]"#,
            r#"{"ok":true,"value":[false,true,true,"a�b","😀"],"toolCalls":[]}"#,
        ),
        (
            "probe s4.txt",
            r#"return [String.fromCharCode(65, 66.9, 65536 + 67, -1, "68", NaN), String.fromCharCode(), String.fromCodePoint(128512, 65), String.fromCodePoint(), String.fromCodePoint("65")]"#,
            r#"{"ok":true,"value":["ABC￿D\u0000","","😀A","","A"],"toolCalls":[]}"#,
        ),
        (
            "probe s4.txt",
            r#"try { return String.fromCodePoint(NaN) } catch (e) { return [e.name, e.message] }"#,
            r#"{"ok":true,"value":["RangeError","Arguments contain a value that is out of range of code points"],"toolCalls":[]}"#,
        ),
        (
            "probe s4.txt",
            r#"try { return String.fromCodePoint(Infinity) } catch (e) { return [e.name, e.message] }"#,
            r#"{"ok":true,"value":["RangeError","Arguments contain a value that is out of range of code points"],"toolCalls":[]}"#,
        ),
        (
            "probe s4.txt",
            r#"return [String.raw`a\n${1}b${2}`, String.raw({ raw: ["x", "y", "z"] }, 1), String.raw({ raw: "abc" }, "-", "+", "!"), String.raw({ raw: [] }), String.raw({ raw: { length: 2, 0: "p", 1: "q" } }, 9), String.raw({ raw: ["a"] }, 1, 2)]"#,
            r#"{"ok":false,"error":{"kind":"ExecutionFailure","message":"TypeError: String.raw expects a template object with a raw array. (line 1, col 76)","location":{"line":1,"column":76}},"toolCalls":[]}"#,
        ),
        (
            "probe s4.txt",
            r#"try { return String.raw({}) } catch (e) { return e.message }"#,
            r#"{"ok":true,"value":"String.raw expects a template object with a raw array.","toolCalls":[]}"#,
        ),
        (
            "probe s4.txt",
            r#"try { return String.raw() } catch (e) { return e.message }"#,
            r#"{"ok":true,"value":"String.raw expects a template object with a raw array.","toolCalls":[]}"#,
        ),
        (
            "probe s4.txt",
            r#"try { return String.raw("abc") } catch (e) { return e.message }"#,
            r#"{"ok":true,"value":"String.raw expects a template object with a raw array.","toolCalls":[]}"#,
        ),
        (
            "probe s4.txt",
            r#"return String.raw({ raw: [{ toString() { return "T" } }, "2"] }, { toString() { return "S" } })"#,
            r#"{"ok":true,"value":"TS2","toolCalls":[]}"#,
        ),
        (
            "probe s4.txt",
            r#"return ["abc".concat(), "a".concat(1, null, undefined, [2, 3], {}), "a".valueOf(), "a".toString(), "abc"[1], "abc".length, [..."héllo"]]"#,
            r#"{"ok":true,"value":["abc","a1nullundefined2,3[object Object]","a","a","b",3,["h","é","l","l","o"]],"toolCalls":[]}"#,
        ),
        (
            "probe s4.txt",
            r#"try { return String.prototype.toString.call(5) } catch (e) { return e.message }"#,
            r#"{"ok":true,"value":"5","toolCalls":[]}"#,
        ),
        (
            "probe s4.txt",
            r#"return [String.prototype.toString.call(5), String.prototype.toUpperCase.call(true), String.prototype.slice.call([1,2,3], 1), String.prototype.at.call({}, 1)]"#,
            r#"{"ok":true,"value":["5","TRUE",",2,3","o"],"toolCalls":[]}"#,
        ),
        (
            "probe s4.txt",
            r#"const it = "a😀"[Symbol.iterator](); return [it.next(), it.next(), it.next(), typeof it[Symbol.iterator], Object.prototype.toString.call(it)]"#,
            r#"{"ok":true,"value":[{"value":"a","done":false},{"value":"😀","done":false},{"done":true},"function","[object Iterator]"],"toolCalls":[]}"#,
        ),
        (
            "probe s4.txt",
            r#"return [..."x".matchAll(/(?<l>x)/dg)].map(m => [m.index, m.input, m.groups, m.indices])"#,
            r#"{"ok":true,"value":[[0,"x",{"l":"x"},[[0,1],[0,1]]]],"toolCalls":[]}"#,
        ),
        (
            "probe s4.txt",
            r#"return ["abc".replace(/b/, () => ({})), "abc".replace(/b/, () => [1, 2]), "abc".replace(/b/, () => undefined), "abc".replace("b", () => null), "abc".replace(/b/, (m) => m + m)]"#,
            r#"{"ok":true,"value":["a[object Object]c","a1,2c","aundefinedc","anullc","abbc"],"toolCalls":[]}"#,
        ),
        (
            "probe s4.txt",
            r#"return ["abc".replace(/b/, async () => "x")]"#,
            r#"{"ok":true,"value":["a[object Promise]c"],"toolCalls":[]}"#,
        ),
        (
            "probe s4.txt",
            r#"const f = (m) => m.toUpperCase(); return ["a-b-c".replaceAll("-", f), "a-b-c".replaceAll(/-/g, f), "a-b".replace(/(?<sep>-)/, (m, p1, off, s, groups) => JSON.stringify(groups))]"#,
            r#"{"ok":true,"value":["a-b-c","a-b-c","a{\"sep\":\"-\"}b"],"toolCalls":[]}"#,
        ),
        (
            "probe s4.txt",
            r#"try { return "abc".replaceAll(/b/, () => 1) } catch (e) { return e.message }"#,
            r#"{"ok":true,"value":"String.replaceAll requires a regular expression with the global (g) flag: write /b/g, or use String.replace to replace only the first match.","toolCalls":[]}"#,
        ),
        (
            "probe s4.txt",
            r#"try { return "abc".replaceAll(/b/y, "x") } catch (e) { return e.message }"#,
            r#"{"ok":true,"value":"String.replaceAll requires a regular expression with the global (g) flag: write /b/yg, or use String.replace to replace only the first match.","toolCalls":[]}"#,
        ),
        (
            "probe s4.txt",
            r#"return "abc".replaceAll(/b/gy, "x")"#,
            r#"{"ok":true,"value":"abc","toolCalls":[]}"#,
        ),
        (
            "probe s4.txt",
            r#"return "a.b.c".replace(".", "$`")"#,
            r#"{"ok":true,"value":"aab.c","toolCalls":[]}"#,
        ),
        (
            "probe s5.txt",
            r#"const pats = ["(?<a>x)|(?<a>y)", "(?<a>x)(?<a>y)", "((?<a>x)|(?<a>y))(?<a>z)", "(?:(?<a>x)|(?<a>y))\\k<a>", "(?<a>x)|(?:(?<a>y)|(?<a>z))", "(?<$>a)", "(?<\\u0061>a)", "(?<a\\u{62}>a)", "(?<😀>a)", "(?<a>a)\\k<a", "\\k<a>(?<a>a)", "\\k<b>(?<a>a)", "(?<a>a)\\k"]
return pats.map(p => { try { return [new RegExp(p).exec("xyz"), new RegExp(p).source] } catch (e) { return e.message } })"#,
            r#"{"ok":true,"value":[[["x","x",null],"(?<a>x)|(?<a>y)"],"new RegExp(...) received \"(?<a>x)(?<a>y)\", which is not a valid regular expression pattern (duplicate group specifier name). To match special characters like ( ) [ ] { } + * ? . literally, escape them with a backslash (e.g. \"\\\\(\") or test for them with String.includes instead.","new RegExp(...) received \"((?<a>x)|(?<a>y))(?<a>z)\", which is not a valid regular expression pattern (duplicate group specifier name). To match special characters like ( ) [ ] { } + * ? . literally, escape them with a backslash (e.g. \"\\\\(\") or test for them with String.includes instead.",[null,"(?:(?<a>x)|(?<a>y))\\k<a>"],[["x","x",null,null],"(?<a>x)|(?:(?<a>y)|(?<a>z))"],[null,"(?<$>a)"],[null,"(?<\\u0061>a)"],[null,"(?<a\\u{62}>a)"],"new RegExp(...) received \"(?<😀>a)\", which is not a valid regular expression pattern (invalid group specifier name). To match special characters like ( ) [ ] { } + * ? . literally, escape them with a backslash (e.g. \"\\\\(\") or test for them with String.includes instead.","new RegExp(...) received \"(?<a>a)\\\\k<a\", which is not a valid regular expression pattern (invalid \\k<> named backreference). To match special characters like ( ) [ ] { } + * ? . literally, escape them with a backslash (e.g. \"\\\\(\") or test for them with String.includes instead.",[null,"\\k<a>(?<a>a)"],"new RegExp(...) received \"\\\\k<b>(?<a>a)\", which is not a valid regular expression pattern (invalid \\k<> named backreference). To match special characters like ( ) [ ] { } + * ? . literally, escape them with a backslash (e.g. \"\\\\(\") or test for them with String.includes instead.","new RegExp(...) received \"(?<a>a)\\\\k\", which is not a valid regular expression pattern (invalid \\k<> named backreference). To match special characters like ( ) [ ] { } + * ? . literally, escape them with a backslash (e.g. \"\\\\(\") or test for them with String.includes instead."],"toolCalls":[]}"#,
        ),
        (
            "probe s5.txt",
            r#"return ["xyzy".replace(/(?<a>x)|(?<a>y)/g, "<$<a>>"), "zx".replace(/(?:(?<a>x)|(?<a>y))/, (...a) => JSON.stringify(a)), /(?:(?<a>x)|(?<a>y))\k<a>/.test("yy")]"#,
            r#"{"ok":true,"value":["<x><y>z<y>","z[\"x\",\"x\",null,1,\"zx\",{\"a\":\"x\"}]",true],"toolCalls":[]}"#,
        ),
        (
            "probe s5.txt",
            r#"return [/(?<a>x)|(?<a>y)/d.exec("y").indices.groups, Object.keys(/(?<b>.)(?<a>.)/.exec("xy").groups)]"#,
            r#"{"ok":true,"value":[{"a":[0,1]},["b","a"]],"toolCalls":[]}"#,
        ),
        (
            "probe s5.txt",
            r#"const pats = ["a{,5}", "a{1,2", "{1}", "x{1}{2}", "x{2,}?", "\\d{2}", "a{0}", "]", "}", "a|*", "(|)", "()", "(?:)", "[]", "[^]", "\\0", "\\00", "\\07", "\\8", "[\\b]", "\\B+", "(?=x)+", "(?!x){2}", "(?<=x)?", "[\\w-a]", "[a-\\w]", "[\\s-\\d]", "\\cA", "\\c", "[\\c]", "\\u{41}", "\\x4", "\\-"]
return pats.map(p => { try { const r = new RegExp(p); return [r.test("a{,5}"), r.source] } catch (e) { return e.message } })"#,
            r#"{"ok":true,"value":[[true,"a{,5}"],[false,"a{1,2"],"new RegExp(...) received \"{1}\", which is not a valid regular expression pattern (nothing to repeat). To match special characters like ( ) [ ] { } + * ? . literally, escape them with a backslash (e.g. \"\\\\(\") or test for them with String.includes instead.","new RegExp(...) received \"x{1}{2}\", which is not a valid regular expression pattern (nothing to repeat). To match special characters like ( ) [ ] { } + * ? . literally, escape them with a backslash (e.g. \"\\\\(\") or test for them with String.includes instead.",[false,"x{2,}?"],[false,"\\d{2}"],[true,"a{0}"],[false,"]"],[true,"}"],"new RegExp(...) received \"a|*\", which is not a valid regular expression pattern (nothing to repeat). To match special characters like ( ) [ ] { } + * ? . literally, escape them with a backslash (e.g. \"\\\\(\") or test for them with String.includes instead.",[true,"(|)"],[true,"()"],[true,"(?:)"],[false,"[]"],[true,"[^]"],[false,"\\0"],[false,"\\00"],[false,"\\07"],[false,"\\8"],[false,"[\\b]"],"new RegExp(...) received \"\\\\B+\", which is not a valid regular expression pattern (nothing to repeat). To match special characters like ( ) [ ] { } + * ? . literally, escape them with a backslash (e.g. \"\\\\(\") or test for them with String.includes instead.",[false,"(?=x)+"],[true,"(?!x){2}"],"new RegExp(...) received \"(?<=x)?\", which is not a valid regular expression pattern (invalid quantifier). To match special characters like ( ) [ ] { } + * ? . literally, escape them with a backslash (e.g. \"\\\\(\") or test for them with String.includes instead.",[true,"[\\w-a]"],[true,"[a-\\w]"],[true,"[\\s-\\d]"],[false,"\\cA"],[false,"\\c"],[false,"[\\c]"],[false,"\\u{41}"],[false,"\\x4"],[false,"\\-"]],"toolCalls":[]}"#,
        ),
        (
            "probe s5.txt",
            r#"const pats = ["(?<a>.)\\k<a>", "a{,5}", "(?=x)+", "[\\w-a]", "\\u{41}", "\\p{Lu}", "\\p{General_Category=Lu}", "\\p{gc=Lu}", "\\p{Script_Extensions=Latin}", "\\p{scx=Grek}", "\\p{ASCII_Hex_Digit}", "\\p{Any}", "\\p{Assigned}", "\\p{ASCII}", "\\p{L&}", "\\p{IsLatin}", "\\p{Lowercase_Letter}", "\\p{lu}", "\\p{Script=Foo}", "\\p{Emoji}", "\\p{RGI_Emoji}", "[\\p{L}-z]", "[a-\\p{L}]"]
return pats.map(p => { try { const r = new RegExp(p, "u"); return r.test("A") } catch (e) { return e.message } })"#,
            r#"{"ok":true,"value":[false,"new RegExp(...) received \"a{,5}\", which is not a valid regular expression pattern (incomplete {} quantifier for Unicode pattern). To match special characters like ( ) [ ] { } + * ? . literally, escape them with a backslash (e.g. \"\\\\(\") or test for them with String.includes instead.","new RegExp(...) received \"(?=x)+\", which is not a valid regular expression pattern (nothing to repeat). To match special characters like ( ) [ ] { } + * ? . literally, escape them with a backslash (e.g. \"\\\\(\") or test for them with String.includes instead.","new RegExp(...) received \"[\\\\w-a]\", which is not a valid regular expression pattern (invalid range in character class for Unicode pattern). To match special characters like ( ) [ ] { } + * ? . literally, escape them with a backslash (e.g. \"\\\\(\") or test for them with String.includes instead.",true,true,true,true,true,false,true,true,true,true,"new RegExp(...) received \"\\\\p{L&}\", which is not a valid regular expression pattern (invalid property expression). To match special characters like ( ) [ ] { } + * ? . literally, escape them with a backslash (e.g. \"\\\\(\") or test for them with String.includes instead.","new RegExp(...) received \"\\\\p{IsLatin}\", which is not a valid regular expression pattern (invalid property expression). To match special characters like ( ) [ ] { } + * ? . literally, escape them with a backslash (e.g. \"\\\\(\") or test for them with String.includes instead.",false,"new RegExp(...) received \"\\\\p{lu}\", which is not a valid regular expression pattern (invalid property expression). To match special characters like ( ) [ ] { } + * ? . literally, escape them with a backslash (e.g. \"\\\\(\") or test for them with String.includes instead.","new RegExp(...) received \"\\\\p{Script=Foo}\", which is not a valid regular expression pattern (invalid property expression). To match special characters like ( ) [ ] { } + * ? . literally, escape them with a backslash (e.g. \"\\\\(\") or test for them with String.includes instead.",false,"new RegExp(...) received \"\\\\p{RGI_Emoji}\", which is not a valid regular expression pattern (invalid property expression). To match special characters like ( ) [ ] { } + * ? . literally, escape them with a backslash (e.g. \"\\\\(\") or test for them with String.includes instead.","new RegExp(...) received \"[\\\\p{L}-z]\", which is not a valid regular expression pattern (invalid range in character class for Unicode pattern). To match special characters like ( ) [ ] { } + * ? . literally, escape them with a backslash (e.g. \"\\\\(\") or test for them with String.includes instead.","new RegExp(...) received \"[a-\\\\p{L}]\", which is not a valid regular expression pattern (invalid range in character class for Unicode pattern). To match special characters like ( ) [ ] { } + * ? . literally, escape them with a backslash (e.g. \"\\\\(\") or test for them with String.includes instead."],"toolCalls":[]}"#,
        ),
        (
            "probe s5.txt",
            r#"const pats = ["[\\p{RGI_Emoji}]", "[^\\p{RGI_Emoji}]", "\\P{RGI_Emoji}", "[\\q{a|bc}--\\q{bc}]", "[[a-z]&&[^aeiou]]", "[a&&b&&c]", "[a&&b--c]", "[a--]", "[--a]", "[&&a]", "[a-]", "[-a]", "[a-z-]", "[(]", "[)]", "[{]", "[}]", "[/]", "[|]", "[\\-]", "[!!]", "[a!!]", "[##]", "[a&b]", "[a~~b]", "[\\p{Lu}&&\\p{Script=Greek}]"]
return pats.map(p => { try { const r = new RegExp(p, "v"); return [r.test("Σ"), r.test("a")] } catch (e) { return e.message } })"#,
            r#"{"ok":true,"value":[[false,false],"new RegExp(...) received \"[^\\\\p{RGI_Emoji}]\", which is not a valid regular expression pattern (negated class set may contain strings). To match special characters like ( ) [ ] { } + * ? . literally, escape them with a backslash (e.g. \"\\\\(\") or test for them with String.includes instead.","new RegExp(...) received \"\\\\P{RGI_Emoji}\", which is not a valid regular expression pattern (negated class set may contain strings). To match special characters like ( ) [ ] { } + * ? . literally, escape them with a backslash (e.g. \"\\\\(\") or test for them with String.includes instead.",[false,true],[false,false],[false,false],"new RegExp(...) received \"[a&&b--c]\", which is not a valid regular expression pattern (invalid operation in class set). To match special characters like ( ) [ ] { } + * ? . literally, escape them with a backslash (e.g. \"\\\\(\") or test for them with String.includes instead.","new RegExp(...) received \"[a--]\", which is not a valid regular expression pattern (invalid class set character). To match special characters like ( ) [ ] { } + * ? . literally, escape them with a backslash (e.g. \"\\\\(\") or test for them with String.includes instead.","new RegExp(...) received \"[--a]\", which is not a valid regular expression pattern (invalid operation in class set). To match special characters like ( ) [ ] { } + * ? . literally, escape them with a backslash (e.g. \"\\\\(\") or test for them with String.includes instead.","new RegExp(...) received \"[&&a]\", which is not a valid regular expression pattern (invalid operation in class set). To match special characters like ( ) [ ] { } + * ? . literally, escape them with a backslash (e.g. \"\\\\(\") or test for them with String.includes instead.","new RegExp(...) received \"[a-]\", which is not a valid regular expression pattern (invalid class set character). To match special characters like ( ) [ ] { } + * ? . literally, escape them with a backslash (e.g. \"\\\\(\") or test for them with String.includes instead.","new RegExp(...) received \"[-a]\", which is not a valid regular expression pattern (invalid class set character). To match special characters like ( ) [ ] { } + * ? . literally, escape them with a backslash (e.g. \"\\\\(\") or test for them with String.includes instead.","new RegExp(...) received \"[a-z-]\", which is not a valid regular expression pattern (invalid class set character). To match special characters like ( ) [ ] { } + * ? . literally, escape them with a backslash (e.g. \"\\\\(\") or test for them with String.includes instead.","new RegExp(...) received \"[(]\", which is not a valid regular expression pattern (invalid class set character). To match special characters like ( ) [ ] { } + * ? . literally, escape them with a backslash (e.g. \"\\\\(\") or test for them with String.includes instead.","new RegExp(...) received \"[)]\", which is not a valid regular expression pattern (invalid class set character). To match special characters like ( ) [ ] { } + * ? . literally, escape them with a backslash (e.g. \"\\\\(\") or test for them with String.includes instead.","new RegExp(...) received \"[{]\", which is not a valid regular expression pattern (invalid class set character). To match special characters like ( ) [ ] { } + * ? . literally, escape them with a backslash (e.g. \"\\\\(\") or test for them with String.includes instead.","new RegExp(...) received \"[}]\", which is not a valid regular expression pattern (invalid class set character). To match special characters like ( ) [ ] { } + * ? . literally, escape them with a backslash (e.g. \"\\\\(\") or test for them with String.includes instead.","new RegExp(...) received \"[/]\", which is not a valid regular expression pattern (invalid class set character). To match special characters like ( ) [ ] { } + * ? . literally, escape them with a backslash (e.g. \"\\\\(\") or test for them with String.includes instead.","new RegExp(...) received \"[|]\", which is not a valid regular expression pattern (invalid class set character). To match special characters like ( ) [ ] { } + * ? . literally, escape them with a backslash (e.g. \"\\\\(\") or test for them with String.includes instead.",[false,false],"new RegExp(...) received \"[!!]\", which is not a valid regular expression pattern (invalid operation in class set). To match special characters like ( ) [ ] { } + * ? . literally, escape them with a backslash (e.g. \"\\\\(\") or test for them with String.includes instead.","new RegExp(...) received \"[a!!]\", which is not a valid regular expression pattern (invalid operation in class set). To match special characters like ( ) [ ] { } + * ? . literally, escape them with a backslash (e.g. \"\\\\(\") or test for them with String.includes instead.","new RegExp(...) received \"[##]\", which is not a valid regular expression pattern (invalid operation in class set). To match special characters like ( ) [ ] { } + * ? . literally, escape them with a backslash (e.g. \"\\\\(\") or test for them with String.includes instead.",[false,true],"new RegExp(...) received \"[a~~b]\", which is not a valid regular expression pattern (invalid operation in class set). To match special characters like ( ) [ ] { } + * ? . literally, escape them with a backslash (e.g. \"\\\\(\") or test for them with String.includes instead.",[true,false]],"toolCalls":[]}"#,
        ),
        (
            "probe s5.txt",
            r#"return [/[\p{RGI_Emoji}--\q{😀}]/v.test("😀"), /^\p{RGI_Emoji}$/v.test("👍🏽"), /^[\q{abc|d}]$/v.test("abc"), "xabcx".match(/[\q{abc|a}]/v)[0], /[\q{AB}]/vi.test("ab")]"#,
            r#"{"ok":true,"value":[false,true,true,"abc",true],"toolCalls":[]}"#,
        ),
        (
            "probe s5.txt",
            r#"const pats = ["(?i:a)b", "(?-i:a)", "(?i-:a)", "(?-:a)", "(?ims:a)", "(?x:a)", "(?i-i:a)", "(?ii:a)", "(?i", "(?i)", "(?i:a", "(?s-m:a)"]
return pats.map(p => { try { const r = new RegExp(p); return [r.test("Ab"), r.test("AB")] } catch (e) { return e.message } })"#,
            r#"{"ok":true,"value":[[true,false],[false,false],[true,true],"new RegExp(...) received \"(?-:a)\", which is not a valid regular expression pattern (invalid regular expression modifier). To match special characters like ( ) [ ] { } + * ? . literally, escape them with a backslash (e.g. \"\\\\(\") or test for them with String.includes instead.",[true,true],"new RegExp(...) received \"(?x:a)\", which is not a valid regular expression pattern (unrecognized character after (?). To match special characters like ( ) [ ] { } + * ? . literally, escape them with a backslash (e.g. \"\\\\(\") or test for them with String.includes instead.","new RegExp(...) received \"(?i-i:a)\", which is not a valid regular expression pattern (invalid regular expression modifier). To match special characters like ( ) [ ] { } + * ? . literally, escape them with a backslash (e.g. \"\\\\(\") or test for them with String.includes instead.","new RegExp(...) received \"(?ii:a)\", which is not a valid regular expression pattern (invalid regular expression modifier). To match special characters like ( ) [ ] { } + * ? . literally, escape them with a backslash (e.g. \"\\\\(\") or test for them with String.includes instead.","new RegExp(...) received \"(?i\", which is not a valid regular expression pattern (missing )). To match special characters like ( ) [ ] { } + * ? . literally, escape them with a backslash (e.g. \"\\\\(\") or test for them with String.includes instead.","new RegExp(...) received \"(?i)\", which is not a valid regular expression pattern (unrecognized character after (?). To match special characters like ( ) [ ] { } + * ? . literally, escape them with a backslash (e.g. \"\\\\(\") or test for them with String.includes instead.","new RegExp(...) received \"(?i:a\", which is not a valid regular expression pattern (missing )). To match special characters like ( ) [ ] { } + * ? . literally, escape them with a backslash (e.g. \"\\\\(\") or test for them with String.includes instead.",[false,false]],"toolCalls":[]}"#,
        ),
        (
            "probe s5.txt",
            r#"return [/(?i:a)b/.test("Ab"), /(?i:a)b/.test("AB"), /(?-i:a)b/i.test("aB"), /(?-i:a)b/i.test("AB"), /(?s:.)/.test("\n"), /(?m:^b)/.test("a\nb")]"#,
            r#"{"ok":true,"value":[true,false,true,false,true,true],"toolCalls":[]}"#,
        ),
        (
            "probe s5.txt",
            r#"const r = /\u{1F600}/gu; r.lastIndex = 1; const m = r.exec("😀😀"); return [m && m.index, r.lastIndex]"#,
            r#"{"ok":true,"value":[2,4],"toolCalls":[]}"#,
        ),
        (
            "probe s5.txt",
            r#"const r = /./gu; r.lastIndex = 1; const m = r.exec("😀x"); return [m && m[0], m && m.index, r.lastIndex]"#,
            r#"{"ok":true,"value":["x",2,3],"toolCalls":[]}"#,
        ),
        (
            "probe s5.txt",
            r#"const r = /./g; r.lastIndex = 1; const m = r.exec("😀x"); return [m && m[0].length, m && m.index, r.lastIndex]"#,
            r#"{"ok":true,"value":[1,1,2],"toolCalls":[]}"#,
        ),
        (
            "probe s5.txt",
            r#"return ["😀".split(/(?:)/).map(s => s.charCodeAt(0)), "a😀b".replace(/(?:)/gu, "-"), "a😀b".match(/(?:)/gu).length, "😀😀".search(/\uDE00/), "😀😀".search(/\uDE00/u)]"#,
            r#"{"ok":true,"value":[[55357,56832],"-a-😀-b-",4,1,-1],"toolCalls":[]}"#,
        ),
        (
            "probe s5.txt",
            r#"return ["aaa".match(/a*?/g), "abc".match(/x*/g), "abc".replace(/x*/g, "-"), "aaa".split(/a*?/), "aaa".split(/a*/), "abc".split(/(?=b)/), "a,b,".split(/,/), ",a,".split(/(,)/)]"#,
            r#"{"ok":true,"value":[["","","",""],["","","",""],"-a-b-c-",["a","a","a"],["",""],["a","bc"],["a","b",""],["",",","a",",",""]],"toolCalls":[]}"#,
        ),
        (
            "probe s5.txt",
            r#"return [/a|ab/.exec("abc")[0], /((a)|(ab))((c)|(bc))/.exec("abc"), /(z)((a+)?(b+)?(c))*/.exec("zaacbbbcac"), /(a*)*/.exec("b"), /(a*)b\1+/.exec("baaaac"), /(?=(a+))/.exec("baaabac"), /(?=(a+))a*b\1/.exec("baaabac"), /(.*?)a(?!(a+)b\2c)\2(.*)/.exec("baaabaac")]"#,
            r#"{"ok":true,"value":["a",["abc","a","a",null,"bc",null,"bc"],["zaacbbbcac","z","ac","a",null,"c"],["",null],["b",""],["","aaa"],["aba","a"],["baaabaac","ba",null,"abaac"]],"toolCalls":[]}"#,
        ),
        (
            "probe s5.txt",
            r#"return [/[^]/.test("\n"), /[]/.test("a"), /\s/.test("﻿"), /\s/.test("᠎"), /\d/u.test("٣"), /\w/.test("é"), /\bé/.test(" é"), /^\S+$/.test(" ")]"#,
            r#"{"ok":true,"value":[true,false,true,false,false,false,false,false],"toolCalls":[]}"#,
        ),
        (
            "probe s5.txt",
            r#"return [/a/gimsuyd.flags, /a/v.flags, /a/v.unicodeSets, /a/v.unicode, new RegExp("a", "yg").flags, /a/.hasIndices]"#,
            r#"{"ok":true,"value":["dgimsuy","v",true,false,"gy",false],"toolCalls":[]}"#,
        ),
        (
            "probe s5.txt",
            r#"return [String(/\//), String(new RegExp("\\/")), /[/]/.source, new RegExp("a\\/b").source, new RegExp("[\\]/]").source, new RegExp("\\\\/").source, new RegExp("[\\\\]/").source]"#,
            r#"{"ok":true,"value":["/\\//","/\\//","[/]","a\\/b","[\\]/]","\\\\\\/","[\\\\]\\/"],"toolCalls":[]}"#,
        ),
        (
            "probe s5.txt",
            r#"const re = /a/g; const s = structuredClone ? 1 : 0; return [re.exec.name, typeof re.exec, Object.getOwnPropertyDescriptor(RegExp.prototype, "flags").get.name]"#,
            r#"{"ok":false,"error":{"kind":"ExecutionFailure","message":"TypeError: Object.getOwnPropertyDescriptor is not a function. (line 1, col 91)","location":{"line":1,"column":91}},"toolCalls":[]}"#,
        ),
        (
            "probe s6.txt",
            r#"return [/[a&b]/v.test("&"), /[&]/v.test("&"), /[a&]/v.test("a"), /[&a]/v.test("a"), /[a&b]/v.test("b"), /[a\&b]/v.test("a"), /[ab&]/v.test("b")]"#,
            r#"{"ok":true,"value":[true,true,true,true,true,true,true],"toolCalls":[]}"#,
        ),
        (
            "probe s7.txt",
            r#"const r = /(?<=a)b/y; r.lastIndex = 1; const r2 = /\bb/y; r2.lastIndex = 1; const r3 = /^b/my; r3.lastIndex = 2; return [r.test("ab"), r2.test("ab"), r2.test(" b"), r3.test("a\nb"), r3.lastIndex]"#,
            r#"{"ok":true,"value":[true,false,false,true,3],"toolCalls":[]}"#,
        ),
        (
            "probe s7.txt",
            r#"const r = /(a)(?<n>b)?/y; r.lastIndex = 2; const m = r.exec("xxab"); return [m, m.index, m.groups, r.lastIndex]"#,
            r#"{"ok":true,"value":[["ab","a","b"],2,{"n":"b"},4],"toolCalls":[]}"#,
        ),
        (
            "probe s7.txt",
            r#"const r = /b$/y; r.lastIndex = 1; return [r.test("ab"), r.lastIndex, (r.lastIndex = 1, r.test("abc"))]"#,
            r#"{"ok":true,"value":[true,2,false],"toolCalls":[]}"#,
        ),
        (
            "probe s7.txt",
            r#"const pats = [/a|b/y, /a?b/y, /(?:x|y)+z/y, /[^a]/y, /\D/y, /\W+/y, /./y, /.?/y, /(a)\1/y, /\1(a)/y, /(?=a)./y, /(?!a)./y, /x*y/y, /a/y, /\x61/y, /\cJ/y, /[\]a]/y, /\//y, /a{2}/y, /a{0}b/y, /(?:)/y, /(?<n>a)\k<n>/y, /é/iy, /É/iy, /[a-z]/iy, /😀/uy, /./uy, /\p{L}/uy, /\u{1F600}/uy, /😀/uy, /[😀]/uy, /k/iuy, /s/iuy, /\0/y, /$/y]
const inputs = ["xab", "xbb", "xxyz", "xba", "x1a", "x  a", "x\na", "aaa", "xaa", "xaab", "xa", "xb", "xxxy", "xa", "xa", "x\n", "x]", "x/", "xaa", "xb", "x", "xaa", "xÉ", "xé", "xQ", "x😀", "x😀", "xé", "x😀", "x😀", "x😀", "xK", "xſ", "x\0", "x"]
return pats.map((p, i) => { p.lastIndex = 1; const m = p.exec(inputs[i]); return [m && m[0], p.lastIndex] })"#,
            r#"{"ok":true,"value":[["a",2],["b",2],["xyz",4],["b",2],[null,0],["  ",3],[null,0],["a",2],["aa",3],["a",2],["a",2],["b",2],["xxy",4],["a",2],["a",2],["\n",2],["]",2],["/",2],["aa",3],["b",2],["",1],["aa",3],["É",2],["é",2],["Q",2],["😀",3],["😀",3],["é",2],["😀",3],["😀",3],["😀",3],["K",2],["ſ",2],["\u0000",2],["",1]],"toolCalls":[]}"#,
        ),
        (
            "probe s7.txt",
            r#"const pats = [/a|b/y, /[^a]/y, /\W+/y, /./y, /x*y/y, /[a-z]/iy, /😀/uy, /./uy, /\d/y]
return pats.map((p) => { const out = []; for (let i = 0; i <= 4; i++) { p.lastIndex = i; out.push(p.test("ab😀1")) } return out })"#,
            r#"{"ok":true,"value":[[true,true,false,false,false],[false,true,true,true,true],[false,false,true,true,false],[true,true,true,true,true],[false,false,false,false,false],[true,true,false,false,false],[false,false,true,false,false],[true,true,true,false,true],[false,false,false,false,true]],"toolCalls":[]}"#,
        ),
        (
            "probe s8.txt",
            r#"const pair = "𝌆"; return [pair.substr(1).charCodeAt(0), pair.substr(0, 1).charCodeAt(0), pair.substr(0, 1).length, "abc".padStart(6, "💩").length, "abc".padStart(6, "💩").charCodeAt(2), "abc".padEnd(6, "💩").charCodeAt(5)]"#,
            r#"{"ok":true,"value":[57094,55348,1,6,55357,55357],"toolCalls":[]}"#,
        ),
        (
            "probe s8.txt",
            r#"const s = "a\ud800b\udc00"; return [s.length, s.isWellFormed(), s.toWellFormed(), s.toUpperCase().charCodeAt(1), s.toLowerCase().length, s.normalize("NFD").charCodeAt(1), [...s].length, s.at(1).charCodeAt(0), s.codePointAt(1), s.codePointAt(3)]"#,
            r#"{"ok":true,"value":[4,false,"a�b�",55296,4,55296,4,55296,55296,56320],"toolCalls":[]}"#,
        ),
        (
            "probe s8.txt",
            r#"const s = "x\ud83d"; return [s.split("").length, s.slice(1).charCodeAt(0), s.concat("\ude00").length, s.concat("\ude00").codePointAt(1), String.fromCharCode(0xd83d, 0xde00), String.fromCodePoint(0xd83d).charCodeAt(0)]"#,
            r#"{"ok":true,"value":[2,55357,3,128512,"😀",55357],"toolCalls":[]}"#,
        ),
        (
            "probe s8.txt",
            r#"return [RegExp.escape("\ud800"), RegExp.escape("😀\udc00x")]"#,
            r#"{"ok":true,"value":["\\ud800","😀\\udc00x"],"toolCalls":[]}"#,
        ),
        (
            "probe s8.txt",
            r#"const r = new RegExp("\ud83d"); return [r.test("😀"), new RegExp("\ud83d", "u").test("😀"), r.source.length, "😀".search(/\ude00/), "😀".replace(/\ud83d/, "x").length]"#,
            r#"{"ok":true,"value":[true,false,1,1,2],"toolCalls":[]}"#,
        ),
        (
            "probe s8.txt",
            r#"return ["\ud83d".localeCompare("\ude00"), "a\ud83d".localeCompare("a"), "\ud83d" < "\ude00"]"#,
            r#"{"ok":true,"value":[-1,1,true],"toolCalls":[]}"#,
        ),
        (
            "probe s9.txt",
            r#"return ["ſ".match(/s/iu), "K".match(/K/iu), /\w/iu.test("ſ"), /å/i.test("Å"), /[a-z]+/i.exec("xHeLLo")[0], /ǅ/i.test("ǆ")]"#,
            r#"{"ok":true,"value":[["ſ"],["K"],true,true,"xHeLLo",true],"toolCalls":[]}"#,
        ),
    ]);
}

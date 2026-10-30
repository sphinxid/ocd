//! `STDLIB_DUMP=<file>` appends every parity case a stdlib test runs as `{"file", "code", "expected"}` JSON lines, so
//! the expectations can be re-checked against upstream (see `tests/recorder/stdlib-check.ts`).
#![allow(dead_code)]

use std::io::Write;

pub fn case(file: &str, code: &str, expected: &str) {
    let Ok(path) = std::env::var("STDLIB_DUMP") else { return };
    // One write per line: tests run in parallel and append to the same file.
    let line = format!("{}\n", serde_json::json!({ "file": file, "code": code, "expected": expected }));
    if let Ok(mut out) = std::fs::OpenOptions::new().create(true).append(true).open(path) {
        let _ = out.write_all(line.as_bytes());
    }
}

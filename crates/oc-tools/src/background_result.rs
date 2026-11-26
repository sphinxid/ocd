//! `core/src/session/background-result.ts` (2.4.1, #64971): background work reports to the session in one
//! `<background-result kind="…" …>` block, whatever its kind (`shell`, `subagent`, `image`, `video`, `codemode`).

/// `BackgroundResult.render(attributes, body)`.
pub fn render(attributes: &str, body: &str) -> String {
    format!("<background-result {attributes}>\n{body}\n</background-result>")
}

#[cfg(test)]
mod tests {
    #[test]
    fn renders_one_block() {
        assert_eq!(super::render("kind=\"shell\" id=\"sh_1\"", "out"), "<background-result kind=\"shell\" id=\"sh_1\">\nout\n</background-result>");
    }
}

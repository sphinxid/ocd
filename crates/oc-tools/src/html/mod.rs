//! HTML → text/markdown conversion for the `webfetch` tool.
//!
//! Ports `packages/core/src/tool/html-markdown.ts` (`convertHTMLToMarkdown`)
//! and `extractTextFromHTML` from `packages/core/src/tool/plugin/webfetch.ts`,
//! on top of a port of htmlparser2 8.0.2's tokenizer and parser (entity
//! decoding per entities 4.5.0) so tokenisation quirks match upstream.

mod entities;
mod entities_data;
mod js;
mod markdown;
mod parser;
mod tokenizer;

#[cfg(test)]
mod tests;

pub use markdown::MAX_MARKDOWN_BYTES;

use parser::{Attributes, Handler};

/// `convertHTMLToMarkdown(html)`.
pub fn convert_html_to_markdown(html: &str) -> String {
    markdown::convert_html_to_markdown(html)
}

/// `extractTextFromHTML(html)`: concatenated text outside
/// `script|style|noscript|iframe|object|embed`, trimmed.
pub fn extract_text_from_html(html: &str) -> String {
    struct Extract {
        text: String,
        skip_depth: usize,
    }
    impl Handler for Extract {
        fn on_open_tag(&mut self, name: &str, _attributes: &Attributes) {
            if self.skip_depth > 0 || matches!(name, "script" | "style" | "noscript" | "iframe" | "object" | "embed") {
                self.skip_depth += 1;
            }
        }
        fn on_text(&mut self, text: &str) {
            if self.skip_depth == 0 {
                self.text.push_str(text);
            }
        }
        fn on_close_tag(&mut self, _name: &str) {
            if self.skip_depth > 0 {
                self.skip_depth -= 1;
            }
        }
    }
    let result = parser::parse(html, usize::MAX, Extract { text: String::new(), skip_depth: 0 });
    js::trim(&result.text).to_string()
}

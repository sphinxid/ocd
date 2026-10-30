//! A port of the acorn 8.15 JavaScript parser.
//!
//! The parser runs with the options upstream `@opencode/codemode` passes to acorn: `ecmaVersion: "latest"`,
//! `sourceType: "script"`, `allowReturnOutsideFunction: true`, `allowAwaitOutsideFunction: true`,
//! `locations: true`. It accepts exactly the programs acorn accepts and fails with acorn's error message and position
//! otherwise, including acorn's validation of regular expression literals.
//!
//! Like acorn, it works on UTF-16 code units: every offset and column (in [`crate::ast::Loc`] and in [`ParseError`])
//! counts UTF-16 code units.
//!
//! The one deliberate difference is nesting depth: where acorn would eventually throw a `RangeError` on node's stack,
//! this parser fails with `Maximum call stack size exceeded (line:column)` once nesting exceeds a fixed limit that is
//! above every depth acorn handles (except member/call chains longer than 10 000 links, which acorn parses
//! iteratively). The parser is recursive and needs up to a few hundred MB of stack at that limit in debug builds
//! (tens of MB in release), so parse on a thread with a large stack, as the interpreter thread does.
//!
//! Module layout follows acorn's sources: `tokenizer` (with `tokentype` and the token contexts), `statement`,
//! `expression`, `lval` (patterns and lvalue checks), `state` (parser state, scopes, utilities), `regexp` (the regular
//! expression validator, with `unicode_data`), `chars`/`id_tables` (character classes), and `node` (internal node
//! shapes and number helpers). `estree` dumps the AST as normalized ESTree JSON for the acorn comparison tests.

mod chars;
mod estree;
mod expression;
mod id_tables;
mod lval;
mod node;
mod regexp;
mod state;
mod statement;
mod tokenizer;
mod tokentype;
mod unicode_data;

use std::fmt;

#[doc(hidden)]
pub use estree::to_estree;

/// A syntax error, as acorn reports it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParseError {
    /// acorn's `SyntaxError.message`: the error text followed by ` (line:column)`.
    pub message: String,
    /// UTF-16 offset of the error (acorn's `err.pos`).
    pub pos: u32,
    /// 1-based line of the error (acorn's `err.loc.line`).
    pub line: u32,
    /// 0-based UTF-16 column of the error (acorn's `err.loc.column`).
    pub column: u32,
    /// The nesting-depth guard fired: upstream overflows the host stack here (a `RangeError`, not a syntax error).
    pub depth: bool,
}

impl fmt::Display for ParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for ParseError {}

/// Parses a script the way upstream codemode's acorn call does.
pub fn parse(source: &str) -> Result<crate::ast::Program, ParseError> {
    state::Parser::new(source).parse_program()
}

//! The tokenizer (acorn's `tokenize.js` and the context tracking of `tokencontext.js`).

use super::chars::{
    EOF_CODE, has_line_break, is_identifier_char, is_identifier_start, is_new_line, is_non_ascii_whitespace,
    next_line_break, push_code_point,
};
use super::node::{big_digits_to_decimal, f64_from_radix_digits};
use super::regexp::validate_regexp_pattern;
use super::state::{PErr, Parser, R, TokVal};
use super::tokentype::{Ctx, TT};
use std::rc::Rc;

/// Every operator's source text, so token values need no allocation.
fn op_text(s: &[u16]) -> &'static str {
    const OPS: &[&str] = &[
        "=", "!", "~", "+", "-", "*", "/", "%", "**", "<", ">", "<=", ">=", "==", "!=", "===", "!==", "<<", ">>",
        ">>>", "&", "|", "^", "&&", "||", "??", "++", "--", "+=", "-=", "*=", "/=", "%=", "**=", "<<=", ">>=", ">>>=",
        "&=", "|=", "^=", "&&=", "||=", "??=", "?", "?.",
    ];
    OPS.iter().find(|op| op.len() == s.len() && op.bytes().zip(s).all(|(a, &b)| a as u16 == b)).copied().unwrap_or("")
}

impl Parser {
    // ## Token-level API

    /// acorn's `next(ignoreEscapeSequenceInKeyword)`.
    pub(super) fn next(&mut self, ignore_escape_in_keyword: bool) -> R<()> {
        if !ignore_escape_in_keyword
            && self.contains_esc
            && let Some(kw) = self.ty.keyword()
        {
            return Err(self.raise(self.start, &format!("Escape sequence in keyword {kw}")));
        }
        self.last_tok_end = self.end;
        self.last_tok_start = self.start;
        self.last_tok_start_loc = self.start_loc;
        self.next_token()
    }

    pub(super) fn cur_context(&self) -> Option<Ctx> {
        self.context.last().copied()
    }

    pub(super) fn next_token(&mut self) -> R<()> {
        let cur = self.cur_context();
        if !cur.is_some_and(Ctx::preserve_space) {
            self.skip_space()?;
        }
        self.start = self.pos;
        self.start_loc = self.cur_position();
        if self.pos >= self.input.len() {
            return self.finish_token(TT::Eof, TokVal::None);
        }
        if cur == Some(Ctx::QTmpl) {
            return self.try_read_template_token();
        }
        let code = self.full_char_code_at_pos();
        self.read_token(code)
    }

    fn read_token(&mut self, code: u32) -> R<()> {
        if is_identifier_start(code, true) || code == 92 {
            return self.read_word();
        }
        self.get_token_from_code(code)
    }

    pub(super) fn full_char_code_at_pos(&self) -> u32 {
        let code = self.cc(self.pos);
        if code <= 0xd7ff || code >= 0xdc00 {
            return code;
        }
        let next = self.cc(self.pos + 1);
        if next <= 0xdbff || next >= 0xe000 {
            // A lone high surrogate cannot occur in input that came from a Rust string.
            return code;
        }
        (code << 10) + next - 0x35fdc00
    }

    fn skip_block_comment(&mut self) -> R<()> {
        let start = self.pos;
        self.pos += 2;
        let Some(end) = super::chars::find(&self.input, &[b'*' as u16, b'/' as u16], self.pos) else {
            return Err(self.raise(self.pos - 2, "Unterminated comment"));
        };
        self.pos = end + 2;
        let mut pos = start;
        while let Some(next_break) = next_line_break(&self.input, pos, self.pos) {
            self.cur_line += 1;
            self.line_start = next_break;
            pos = next_break;
        }
        Ok(())
    }

    pub(super) fn skip_line_comment(&mut self, start_skip: usize) {
        self.pos += start_skip;
        let mut ch = self.cc(self.pos);
        while self.pos < self.input.len() && !is_new_line(ch) {
            self.pos += 1;
            ch = self.cc(self.pos);
        }
    }

    /// Skips whitespace and comments before a token.
    fn skip_space(&mut self) -> R<()> {
        while self.pos < self.input.len() {
            let ch = self.input[self.pos] as u32;
            match ch {
                32 | 160 => self.pos += 1,
                13 | 10 | 8232 | 8233 => {
                    if ch == 13 && self.cc(self.pos + 1) == 10 {
                        self.pos += 1;
                    }
                    self.pos += 1;
                    self.cur_line += 1;
                    self.line_start = self.pos;
                }
                47 => match self.cc(self.pos + 1) {
                    42 => self.skip_block_comment()?,
                    47 => self.skip_line_comment(2),
                    _ => break,
                },
                _ => {
                    if (ch > 8 && ch < 14) || (ch >= 5760 && is_non_ascii_whitespace(ch)) {
                        self.pos += 1;
                    } else {
                        break;
                    }
                }
            }
        }
        Ok(())
    }

    /// Ends the current token: sets its end, type and value, and updates the context.
    pub(super) fn finish_token(&mut self, ty: TT, val: TokVal) -> R<()> {
        self.end = self.pos;
        let prev = self.ty;
        self.ty = ty;
        self.value = val;
        self.update_context(prev);
        Ok(())
    }

    fn finish_op(&mut self, ty: TT, size: usize) -> R<()> {
        let end = (self.pos + size).min(self.input.len());
        let text = op_text(&self.input[self.pos..end]);
        self.pos += size;
        self.finish_token(ty, TokVal::Op(text))
    }

    // ## Context tracking

    fn brace_is_block(&self, prev: TT) -> bool {
        let parent = self.cur_context();
        if parent == Some(Ctx::FExpr) || parent == Some(Ctx::FStat) {
            return true;
        }
        if prev == TT::Colon && (parent == Some(Ctx::BStat) || parent == Some(Ctx::BExpr)) {
            return parent != Some(Ctx::BExpr);
        }
        if prev == TT::Return || (prev == TT::Name && self.expr_allowed) {
            return has_line_break(&self.input, self.last_tok_end, self.start);
        }
        if matches!(prev, TT::Else | TT::Semi | TT::Eof | TT::ParenR | TT::Arrow) {
            return true;
        }
        if prev == TT::BraceL {
            return parent == Some(Ctx::BStat);
        }
        if matches!(prev, TT::Var | TT::Const | TT::Name) {
            return false;
        }
        !self.expr_allowed
    }

    pub(super) fn in_generator_context(&self) -> bool {
        for ctx in self.context.iter().skip(1).rev() {
            if ctx.is_function() {
                return ctx.generator();
            }
        }
        false
    }

    fn update_context(&mut self, prev: TT) {
        let ty = self.ty;
        if ty.is_keyword() && prev == TT::Dot {
            self.expr_allowed = false;
            return;
        }
        match ty {
            TT::ParenR | TT::BraceR => {
                if self.context.len() == 1 {
                    self.expr_allowed = true;
                    return;
                }
                let mut out = self.context.pop();
                if out == Some(Ctx::BStat) && self.cur_context().is_some_and(Ctx::is_function) {
                    out = self.context.pop();
                }
                self.expr_allowed = !out.is_some_and(Ctx::is_expr);
            }
            TT::BraceL => {
                let ctx = if self.brace_is_block(prev) { Ctx::BStat } else { Ctx::BExpr };
                self.context.push(ctx);
                self.expr_allowed = true;
            }
            TT::DollarBraceL => {
                self.context.push(Ctx::BTmpl);
                self.expr_allowed = true;
            }
            TT::ParenL => {
                let statement_parens = matches!(prev, TT::If | TT::For | TT::With | TT::While);
                self.context.push(if statement_parens { Ctx::PStat } else { Ctx::PExpr });
                self.expr_allowed = true;
            }
            TT::IncDec => {}
            TT::Function | TT::Class => {
                let cur = self.cur_context();
                if prev.before_expr()
                    && prev != TT::Else
                    && !(prev == TT::Semi && cur != Some(Ctx::PStat))
                    && !(prev == TT::Return && has_line_break(&self.input, self.last_tok_end, self.start))
                    && !((prev == TT::Colon || prev == TT::BraceL) && cur == Some(Ctx::BStat))
                {
                    self.context.push(Ctx::FExpr);
                } else {
                    self.context.push(Ctx::FStat);
                }
                self.expr_allowed = false;
            }
            TT::Colon => {
                if self.cur_context().is_some_and(Ctx::is_function) {
                    self.context.pop();
                }
                self.expr_allowed = true;
            }
            TT::BackQuote => {
                if self.cur_context() == Some(Ctx::QTmpl) {
                    self.context.pop();
                } else {
                    self.context.push(Ctx::QTmpl);
                }
                self.expr_allowed = false;
            }
            TT::Star => {
                if prev == TT::Function
                    && let Some(last) = self.context.last_mut()
                {
                    *last = if *last == Ctx::FExpr { Ctx::FExprGen } else { Ctx::FGen };
                }
                self.expr_allowed = true;
            }
            TT::Name => {
                let mut allowed = false;
                if prev != TT::Dot {
                    let word = self.word_value();
                    if (word == Some("of") && !self.expr_allowed)
                        || (word == Some("yield") && self.in_generator_context())
                    {
                        allowed = true;
                    }
                }
                self.expr_allowed = allowed;
            }
            _ => self.expr_allowed = ty.before_expr(),
        }
    }

    /// acorn's `overrideContext`.
    pub(super) fn override_context(&mut self, ctx: Ctx) {
        if let Some(last) = self.context.last_mut() {
            *last = ctx;
        }
    }

    // ## Token reading

    fn read_token_dot(&mut self) -> R<()> {
        let next = self.cc(self.pos + 1);
        if (48..=57).contains(&next) {
            return self.read_number(true);
        }
        let next2 = self.cc(self.pos + 2);
        if next == 46 && next2 == 46 {
            self.pos += 3;
            return self.finish_token(TT::Ellipsis, TokVal::None);
        }
        self.pos += 1;
        self.finish_token(TT::Dot, TokVal::None)
    }

    fn read_token_slash(&mut self) -> R<()> {
        let next = self.cc(self.pos + 1);
        if self.expr_allowed {
            self.pos += 1;
            return self.read_regexp();
        }
        if next == 61 {
            return self.finish_op(TT::Assign, 2);
        }
        self.finish_op(TT::Slash, 1)
    }

    fn read_token_mult_modulo_exp(&mut self, code: u32) -> R<()> {
        let mut next = self.cc(self.pos + 1);
        let mut size = 1;
        let mut ty = if code == 42 { TT::Star } else { TT::Modulo };
        if code == 42 && next == 42 {
            size += 1;
            ty = TT::Starstar;
            next = self.cc(self.pos + 2);
        }
        if next == 61 {
            return self.finish_op(TT::Assign, size + 1);
        }
        self.finish_op(ty, size)
    }

    fn read_token_pipe_amp(&mut self, code: u32) -> R<()> {
        let next = self.cc(self.pos + 1);
        if next == code {
            if self.cc(self.pos + 2) == 61 {
                return self.finish_op(TT::Assign, 3);
            }
            return self.finish_op(if code == 124 { TT::LogicalOr } else { TT::LogicalAnd }, 2);
        }
        if next == 61 {
            return self.finish_op(TT::Assign, 2);
        }
        self.finish_op(if code == 124 { TT::BitwiseOr } else { TT::BitwiseAnd }, 1)
    }

    fn read_token_caret(&mut self) -> R<()> {
        if self.cc(self.pos + 1) == 61 {
            return self.finish_op(TT::Assign, 2);
        }
        self.finish_op(TT::BitwiseXor, 1)
    }

    fn read_token_plus_min(&mut self, code: u32) -> R<()> {
        let next = self.cc(self.pos + 1);
        if next == code {
            if next == 45
                && self.cc(self.pos + 2) == 62
                && (self.last_tok_end == 0 || has_line_break(&self.input, self.last_tok_end, self.pos))
            {
                // A `-->` line comment.
                self.skip_line_comment(3);
                self.skip_space()?;
                return self.next_token();
            }
            return self.finish_op(TT::IncDec, 2);
        }
        if next == 61 {
            return self.finish_op(TT::Assign, 2);
        }
        self.finish_op(TT::PlusMin, 1)
    }

    fn read_token_lt_gt(&mut self, code: u32) -> R<()> {
        let next = self.cc(self.pos + 1);
        let mut size = 1;
        if next == code {
            size = if code == 62 && self.cc(self.pos + 2) == 62 { 3 } else { 2 };
            if self.cc(self.pos + size) == 61 {
                return self.finish_op(TT::Assign, size + 1);
            }
            return self.finish_op(TT::BitShift, size);
        }
        if next == 33 && code == 60 && self.cc(self.pos + 2) == 45 && self.cc(self.pos + 3) == 45 {
            // `<!--`, an XML-style comment that is a line comment.
            self.skip_line_comment(4);
            self.skip_space()?;
            return self.next_token();
        }
        if next == 61 {
            size = 2;
        }
        self.finish_op(TT::Relational, size)
    }

    fn read_token_eq_excl(&mut self, code: u32) -> R<()> {
        let next = self.cc(self.pos + 1);
        if next == 61 {
            let size = if self.cc(self.pos + 2) == 61 { 3 } else { 2 };
            return self.finish_op(TT::Equality, size);
        }
        if code == 61 && next == 62 {
            self.pos += 2;
            return self.finish_token(TT::Arrow, TokVal::None);
        }
        self.finish_op(if code == 61 { TT::Eq } else { TT::Prefix }, 1)
    }

    fn read_token_question(&mut self) -> R<()> {
        let next = self.cc(self.pos + 1);
        if next == 46 {
            let next2 = self.cc(self.pos + 2);
            // `NaN` past the end compares false both ways.
            if next2 != EOF_CODE && !(48..=57).contains(&next2) {
                return self.finish_op(TT::QuestionDot, 2);
            }
        }
        if next == 63 {
            if self.cc(self.pos + 2) == 61 {
                return self.finish_op(TT::Assign, 3);
            }
            return self.finish_op(TT::Coalesce, 2);
        }
        self.finish_op(TT::Question, 1)
    }

    fn read_token_number_sign(&mut self) -> R<()> {
        self.pos += 1;
        let code = self.full_char_code_at_pos();
        if is_identifier_start(code, true) || code == 92 {
            let word = self.read_word1()?;
            return self.finish_token(TT::PrivateId, TokVal::Word(word));
        }
        Err(self.unexpected_character(self.pos, code))
    }

    fn unexpected_character(&self, pos: usize, code: u32) -> PErr {
        // `codePointToString(NaN)` yields U+10000.
        let ch = if code == EOF_CODE { '\u{10000}' } else { char::from_u32(code).unwrap_or('\u{fffd}') };
        self.raise(pos, &format!("Unexpected character '{ch}'"))
    }

    fn get_token_from_code(&mut self, code: u32) -> R<()> {
        let simple = match code {
            40 => Some(TT::ParenL),
            41 => Some(TT::ParenR),
            59 => Some(TT::Semi),
            44 => Some(TT::Comma),
            91 => Some(TT::BracketL),
            93 => Some(TT::BracketR),
            123 => Some(TT::BraceL),
            125 => Some(TT::BraceR),
            58 => Some(TT::Colon),
            96 => Some(TT::BackQuote),
            _ => None,
        };
        if let Some(ty) = simple {
            self.pos += 1;
            return self.finish_token(ty, TokVal::None);
        }
        match code {
            46 => self.read_token_dot(),
            48 => {
                let next = self.cc(self.pos + 1);
                match next {
                    120 | 88 => self.read_radix_number(16),
                    111 | 79 => self.read_radix_number(8),
                    98 | 66 => self.read_radix_number(2),
                    _ => self.read_number(false),
                }
            }
            49..=57 => self.read_number(false),
            34 | 39 => self.read_string(code),
            47 => self.read_token_slash(),
            37 | 42 => self.read_token_mult_modulo_exp(code),
            124 | 38 => self.read_token_pipe_amp(code),
            94 => self.read_token_caret(),
            43 | 45 => self.read_token_plus_min(code),
            60 | 62 => self.read_token_lt_gt(code),
            61 | 33 => self.read_token_eq_excl(code),
            63 => self.read_token_question(),
            126 => self.finish_op(TT::Prefix, 1),
            35 => self.read_token_number_sign(),
            _ => Err(self.unexpected_character(self.pos, code)),
        }
    }

    /// Reads a regular expression literal; `self.pos` is just past the opening slash.
    pub(super) fn read_regexp(&mut self) -> R<()> {
        let start = self.pos;
        let mut escaped = false;
        let mut in_class = false;
        loop {
            if self.pos >= self.input.len() {
                return Err(self.raise(start, "Unterminated regular expression"));
            }
            let ch = self.input[self.pos];
            if is_new_line(ch as u32) {
                return Err(self.raise(start, "Unterminated regular expression"));
            }
            if !escaped {
                if ch == b'[' as u16 {
                    in_class = true;
                } else if ch == b']' as u16 && in_class {
                    in_class = false;
                } else if ch == b'/' as u16 && !in_class {
                    break;
                }
                escaped = ch == b'\\' as u16;
            } else {
                escaped = false;
            }
            self.pos += 1;
        }
        let pattern_units = self.input[start..self.pos].to_vec();
        self.pos += 1;
        let flags_start = self.pos;
        let flags = self.read_word1()?;
        if self.contains_esc {
            return Err(self.unexpected_at(flags_start));
        }

        // validateRegExpFlags
        let flag_units: Vec<u16> = flags.encode_utf16().collect();
        let (mut u, mut v) = (false, false);
        for (i, &flag) in flag_units.iter().enumerate() {
            if !"gimuysdv".encode_utf16().any(|f| f == flag) {
                return Err(self.raise(start, "Invalid regular expression flag"));
            }
            if flag_units[i + 1..].contains(&flag) {
                return Err(self.raise(start, "Duplicate regular expression flag"));
            }
            if flag == b'u' as u16 {
                u = true;
            }
            if flag == b'v' as u16 {
                v = true;
            }
        }
        if u && v {
            return Err(self.raise(start, "Invalid regular expression flag"));
        }

        let pattern = String::from_utf16_lossy(&pattern_units);
        if let Err(message) = validate_regexp_pattern(&pattern_units, &flags) {
            return Err(self.raise(start, &format!("Invalid regular expression: /{pattern}/: {message}")));
        }
        self.finish_token(TT::Regexp, TokVal::Regex { pattern: Rc::from(pattern), flags })
    }

    /// acorn's `readInt(radix, len, maybeLegacyOctalNumericLiteral)`; `None` when no (or not exactly `len`)
    /// digits were read.
    fn read_int(&mut self, radix: u32, len: Option<usize>, maybe_legacy_octal: bool) -> R<Option<f64>> {
        let allow_separators = len.is_none();
        let is_legacy_octal = maybe_legacy_octal && self.cc(self.pos) == 48;
        let start = self.pos;
        let mut total = 0f64;
        let mut last_code = 0u32;
        let mut i = 0usize;
        while len.is_none_or(|l| i < l) {
            let code = self.cc(self.pos);
            if allow_separators && code == 95 {
                if is_legacy_octal {
                    return Err(
                        self.raise(self.pos, "Numeric separator is not allowed in legacy octal numeric literals")
                    );
                }
                if last_code == 95 {
                    return Err(self.raise(self.pos, "Numeric separator must be exactly one underscore"));
                }
                if i == 0 {
                    return Err(self.raise(self.pos, "Numeric separator is not allowed at the first of digits"));
                }
                last_code = code;
                i += 1;
                self.pos += 1;
                continue;
            }
            let val = if code == EOF_CODE {
                u32::MAX
            } else if code >= 97 {
                code - 97 + 10
            } else if code >= 65 {
                code - 65 + 10
            } else if (48..=57).contains(&code) {
                code - 48
            } else {
                u32::MAX
            };
            if val >= radix {
                break;
            }
            last_code = code;
            total = total * radix as f64 + val as f64;
            i += 1;
            self.pos += 1;
        }
        if allow_separators && last_code == 95 {
            return Err(self.raise(self.pos - 1, "Numeric separator is not allowed at the last of digits"));
        }
        if self.pos == start || len.is_some_and(|l| self.pos - start != l) {
            return Ok(None);
        }
        Ok(Some(total))
    }

    fn read_radix_number(&mut self, radix: u32) -> R<()> {
        let start = self.pos;
        self.pos += 2;
        let Some(val) = self.read_int(radix, None, false)? else {
            return Err(self.raise(self.start + 2, &format!("Expected number in radix {radix}")));
        };
        if self.cc(self.pos) == 110 {
            let digits: Vec<u32> = self.input[start + 2..self.pos]
                .iter()
                .filter(|&&c| c != b'_' as u16)
                .map(|&c| (c as u8 as char).to_digit(16).unwrap_or(0))
                .collect();
            let decimal = big_digits_to_decimal(&digits, radix);
            self.pos += 1;
            return self.finish_token(TT::Num, TokVal::BigInt(Rc::from(decimal)));
        } else if is_identifier_start(self.full_char_code_at_pos(), true) {
            return Err(self.raise(self.pos, "Identifier directly after number"));
        }
        self.finish_token(TT::Num, TokVal::Num(val))
    }

    fn read_number(&mut self, starts_with_dot: bool) -> R<()> {
        let start = self.pos;
        if !starts_with_dot && self.read_int(10, None, true)?.is_none() {
            return Err(self.raise(start, "Invalid number"));
        }
        let mut octal = self.pos - start >= 2 && self.cc(start) == 48;
        if octal && self.strict {
            return Err(self.raise(start, "Invalid number"));
        }
        let mut next = self.cc(self.pos);
        if !octal && !starts_with_dot && next == 110 {
            let digits: String =
                self.input[start..self.pos].iter().filter(|&&c| c != b'_' as u16).map(|&c| c as u8 as char).collect();
            self.pos += 1;
            if is_identifier_start(self.full_char_code_at_pos(), true) {
                return Err(self.raise(self.pos, "Identifier directly after number"));
            }
            return self.finish_token(TT::Num, TokVal::BigInt(Rc::from(digits)));
        }
        if octal && self.input[start..self.pos].iter().any(|&c| c == b'8' as u16 || c == b'9' as u16) {
            octal = false;
        }
        if next == 46 && !octal {
            self.pos += 1;
            self.read_int(10, None, false)?;
            next = self.cc(self.pos);
        }
        if (next == 69 || next == 101) && !octal {
            self.pos += 1;
            next = self.cc(self.pos);
            if next == 43 || next == 45 {
                self.pos += 1;
            }
            if self.read_int(10, None, false)?.is_none() {
                return Err(self.raise(start, "Invalid number"));
            }
        }
        if is_identifier_start(self.full_char_code_at_pos(), true) {
            return Err(self.raise(self.pos, "Identifier directly after number"));
        }
        let text: String =
            self.input[start..self.pos].iter().filter(|&&c| c != b'_' as u16).map(|&c| c as u8 as char).collect();
        let val = if octal {
            let digits: Vec<u32> = text.bytes().map(|b| (b - b'0') as u32).collect();
            f64_from_radix_digits(&digits, 8)
        } else {
            text.parse::<f64>().unwrap_or(f64::NAN)
        };
        self.finish_token(TT::Num, TokVal::Num(val))
    }

    /// Reads the code point of a `\u` escape (`self.pos` is just past the `u`).
    fn read_code_point(&mut self) -> R<f64> {
        let ch = self.cc(self.pos);
        if ch == 123 {
            self.pos += 1;
            let code_pos = self.pos;
            let len = super::chars::find(&self.input, &[b'}' as u16], self.pos).map(|end| end - self.pos);
            // `indexOf` returning -1 makes the requested length negative: no digit is read and the escape is bad.
            let code = self.read_hex_char(len.unwrap_or(0), len.is_none())?;
            self.pos += 1;
            if code > 1_114_111.0 {
                return Err(self.invalid_string_token(code_pos, "Code point out of bounds"));
            }
            Ok(code)
        } else {
            self.read_hex_char(4, false)
        }
    }

    fn read_string(&mut self, quote: u32) -> R<()> {
        let mut out: Vec<u16> = Vec::new();
        self.pos += 1;
        let mut chunk_start = self.pos;
        loop {
            if self.pos >= self.input.len() {
                return Err(self.raise(self.start, "Unterminated string constant"));
            }
            let ch = self.input[self.pos] as u32;
            if ch == quote {
                break;
            }
            if ch == 92 {
                out.extend_from_slice(&self.input[chunk_start..self.pos]);
                self.read_escaped_char(false, &mut out)?;
                chunk_start = self.pos;
            } else if ch == 0x2028 || ch == 0x2029 {
                self.pos += 1;
                self.cur_line += 1;
                self.line_start = self.pos;
            } else {
                if is_new_line(ch) {
                    return Err(self.raise(self.start, "Unterminated string constant"));
                }
                self.pos += 1;
            }
        }
        out.extend_from_slice(&self.input[chunk_start..self.pos]);
        self.pos += 1;
        self.finish_token(TT::String, TokVal::Str(Rc::from(out)))
    }

    fn try_read_template_token(&mut self) -> R<()> {
        self.in_template_element = true;
        match self.read_tmpl_token() {
            Ok(()) => {}
            Err(PErr::BadTemplateEscape) => self.read_invalid_template_token()?,
            Err(e) => {
                self.in_template_element = false;
                return Err(e);
            }
        }
        self.in_template_element = false;
        Ok(())
    }

    fn invalid_string_token(&self, position: usize, message: &str) -> PErr {
        if self.in_template_element { PErr::BadTemplateEscape } else { self.raise(position, message) }
    }

    fn read_tmpl_token(&mut self) -> R<()> {
        let mut out: Vec<u16> = Vec::new();
        let mut chunk_start = self.pos;
        loop {
            if self.pos >= self.input.len() {
                return Err(self.raise(self.start, "Unterminated template"));
            }
            let ch = self.input[self.pos] as u32;
            if ch == 96 || (ch == 36 && self.cc(self.pos + 1) == 123) {
                if self.pos == self.start && (self.ty == TT::Template || self.ty == TT::InvalidTemplate) {
                    if ch == 36 {
                        self.pos += 2;
                        return self.finish_token(TT::DollarBraceL, TokVal::None);
                    }
                    self.pos += 1;
                    return self.finish_token(TT::BackQuote, TokVal::None);
                }
                out.extend_from_slice(&self.input[chunk_start..self.pos]);
                return self.finish_token(TT::Template, TokVal::Template(Rc::from(out)));
            }
            if ch == 92 {
                out.extend_from_slice(&self.input[chunk_start..self.pos]);
                self.read_escaped_char(true, &mut out)?;
                chunk_start = self.pos;
            } else if is_new_line(ch) {
                out.extend_from_slice(&self.input[chunk_start..self.pos]);
                self.pos += 1;
                match ch {
                    13 | 10 => {
                        if ch == 13 && self.cc(self.pos) == 10 {
                            self.pos += 1;
                        }
                        out.push(10);
                    }
                    _ => out.push(ch as u16),
                }
                self.cur_line += 1;
                self.line_start = self.pos;
                chunk_start = self.pos;
            } else {
                self.pos += 1;
            }
        }
    }

    /// Reads a template token to its end without validating escapes.
    fn read_invalid_template_token(&mut self) -> R<()> {
        while self.pos < self.input.len() {
            let ch = self.input[self.pos];
            match ch {
                92 => self.pos += 1,
                36 | 96 => {
                    if ch == 96 || self.cc(self.pos + 1) == 123 {
                        let raw = Rc::from(&self.input[self.start..self.pos]);
                        return self.finish_token(TT::InvalidTemplate, TokVal::InvalidTemplate(raw));
                    }
                }
                13 | 10 | 0x2028 | 0x2029 => {
                    if ch == 13 && self.cc(self.pos + 1) == 10 {
                        self.pos += 1;
                    }
                    self.cur_line += 1;
                    self.line_start = self.pos + 1;
                }
                _ => {}
            }
            self.pos += 1;
        }
        Err(self.raise(self.start, "Unterminated template"))
    }

    /// Reads an escape sequence (`self.pos` is at the backslash) and appends its value to `out`.
    fn read_escaped_char(&mut self, in_template: bool, out: &mut Vec<u16>) -> R<()> {
        self.pos += 1;
        let ch = self.cc(self.pos);
        self.pos += 1;
        match ch {
            110 => out.push(10),
            114 => out.push(13),
            120 => {
                let code = self.read_hex_char(2, false)?;
                out.push(code as u16);
            }
            117 => {
                let code = self.read_code_point()?;
                push_code_point(out, code as u32);
            }
            116 => out.push(9),
            98 => out.push(8),
            118 => out.push(11),
            102 => out.push(12),
            13 | 10 => {
                if ch == 13 && self.cc(self.pos) == 10 {
                    self.pos += 1;
                }
                self.line_start = self.pos;
                self.cur_line += 1;
            }
            _ => {
                if ch == 56 || ch == 57 {
                    if self.strict {
                        return Err(self.invalid_string_token(self.pos - 1, "Invalid escape sequence"));
                    }
                    if in_template {
                        return Err(
                            self.invalid_string_token(self.pos - 1, "Invalid escape sequence in template string")
                        );
                    }
                }
                if (48..=55).contains(&ch) {
                    let from = self.pos - 1;
                    let mut len = 0;
                    while len < 3
                        && from + len < self.input.len()
                        && (48..=55).contains(&(self.input[from + len] as u32))
                    {
                        len += 1;
                    }
                    let mut octal = self.input[from..from + len].iter().fold(0u32, |acc, &c| acc * 8 + (c as u32 - 48));
                    if octal > 255 {
                        len -= 1;
                        octal /= 8;
                    }
                    self.pos += len - 1;
                    let next = self.cc(self.pos);
                    let is_zero = len == 1 && self.input[from] == b'0' as u16;
                    if (!is_zero || next == 56 || next == 57) && (self.strict || in_template) {
                        let message = if in_template {
                            "Octal literal in template string"
                        } else {
                            "Octal literal in strict mode"
                        };
                        return Err(self.invalid_string_token(self.pos - 1 - len, message));
                    }
                    out.push(octal as u16);
                } else if is_new_line(ch) {
                    // Unicode line terminators after `\` are dropped from the value.
                    self.line_start = self.pos;
                    self.cur_line += 1;
                } else if ch == EOF_CODE {
                    // `String.fromCharCode(NaN)`
                    out.push(0);
                } else {
                    out.push(ch as u16);
                }
            }
        }
        Ok(())
    }

    /// acorn's `readHexChar(len)`. `negative_len` models `indexOf` returning -1 for `\u{` without `}`.
    fn read_hex_char(&mut self, len: usize, negative_len: bool) -> R<f64> {
        let code_pos = self.pos;
        let n = if negative_len { None } else { self.read_int(16, Some(len), false)? };
        match n {
            Some(n) => Ok(n),
            None => Err(self.invalid_string_token(code_pos, "Bad character escape sequence")),
        }
    }

    /// Reads an identifier (handling `\u` escapes) and sets `contains_esc`.
    pub(super) fn read_word1(&mut self) -> R<Rc<str>> {
        self.contains_esc = false;
        let mut word: Vec<u16> = Vec::new();
        let mut first = true;
        let mut chunk_start = self.pos;
        while self.pos < self.input.len() {
            let ch = self.full_char_code_at_pos();
            if is_identifier_char(ch, true) {
                self.pos += if ch <= 0xffff { 1 } else { 2 };
            } else if ch == 92 {
                self.contains_esc = true;
                word.extend_from_slice(&self.input[chunk_start..self.pos]);
                let esc_start = self.pos;
                self.pos += 1;
                if self.cc(self.pos) != 117 {
                    return Err(self.invalid_string_token(self.pos, "Expecting Unicode escape sequence \\uXXXX"));
                }
                self.pos += 1;
                let esc = self.read_code_point()? as u32;
                let ok = if first { is_identifier_start(esc, true) } else { is_identifier_char(esc, true) };
                if !ok {
                    return Err(self.invalid_string_token(esc_start, "Invalid Unicode escape"));
                }
                push_code_point(&mut word, esc);
                chunk_start = self.pos;
            } else {
                break;
            }
            first = false;
        }
        word.extend_from_slice(&self.input[chunk_start..self.pos]);
        Ok(Rc::from(String::from_utf16_lossy(&word)))
    }

    fn read_word(&mut self) -> R<()> {
        let word = self.read_word1()?;
        let ty = TT::from_keyword(&word).unwrap_or(TT::Name);
        self.finish_token(ty, TokVal::Word(word))
    }
}

//! Parser state and shared utilities (acorn's `state.js`, `parseutil.js`, `scope.js`, `location.js`).

use super::ParseError;
use super::chars::{get_line_info, has_line_break, is_identifier_char, is_identifier_start, skip_white_space};
use super::tokentype::{Ctx, TT};
use crate::ast;
use std::collections::HashMap;
use std::rc::Rc;

/// A `{line, column}` position.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(super) struct Pos {
    pub line: u32,
    pub column: u32,
}

/// The value of the current token (acorn's `this.value`).
#[derive(Debug, Clone)]
pub(super) enum TokVal {
    None,
    /// Names, keywords, and private names (without `#`).
    Word(Rc<str>),
    /// Operators: their source text.
    Op(&'static str),
    Num(f64),
    /// A BigInt literal's decimal digits.
    BigInt(Rc<str>),
    Str(Rc<[u16]>),
    Regex {
        pattern: Rc<str>,
        flags: Rc<str>,
    },
    /// Cooked value of a `template` token.
    Template(Rc<[u16]>),
    /// Raw source of an `invalidTemplate` token.
    InvalidTemplate(Rc<[u16]>),
}

/// Internal error: a syntax error, or acorn's `INVALID_TEMPLATE_ESCAPE_ERROR` sentinel.
#[derive(Debug)]
pub(super) enum PErr {
    Syntax(Box<ParseError>),
    BadTemplateEscape,
}

pub(super) type R<T> = Result<T, PErr>;

// Scope flags.
pub(super) const SCOPE_TOP: u32 = 1;
pub(super) const SCOPE_FUNCTION: u32 = 2;
pub(super) const SCOPE_ASYNC: u32 = 4;
pub(super) const SCOPE_GENERATOR: u32 = 8;
pub(super) const SCOPE_ARROW: u32 = 16;
pub(super) const SCOPE_SIMPLE_CATCH: u32 = 32;
pub(super) const SCOPE_SUPER: u32 = 64;
pub(super) const SCOPE_DIRECT_SUPER: u32 = 128;
pub(super) const SCOPE_CLASS_STATIC_BLOCK: u32 = 256;
pub(super) const SCOPE_CLASS_FIELD_INIT: u32 = 512;
pub(super) const SCOPE_VAR: u32 = SCOPE_TOP | SCOPE_FUNCTION | SCOPE_CLASS_STATIC_BLOCK;

pub(super) fn function_flags(is_async: bool, generator: bool) -> u32 {
    SCOPE_FUNCTION | if is_async { SCOPE_ASYNC } else { 0 } | if generator { SCOPE_GENERATOR } else { 0 }
}

// Binding types.
pub(super) const BIND_NONE: u8 = 0;
pub(super) const BIND_VAR: u8 = 1;
pub(super) const BIND_LEXICAL: u8 = 2;
pub(super) const BIND_FUNCTION: u8 = 3;
pub(super) const BIND_SIMPLE_CATCH: u8 = 4;
pub(super) const BIND_OUTSIDE: u8 = 5;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum LabelKind {
    None,
    Loop,
    Switch,
}

#[derive(Debug, Clone)]
pub(super) struct Label {
    pub name: Option<Rc<str>>,
    pub kind: LabelKind,
    pub statement_start: Option<usize>,
}

impl Label {
    pub(super) fn loop_label() -> Label {
        Label { name: None, kind: LabelKind::Loop, statement_start: None }
    }

    pub(super) fn switch_label() -> Label {
        Label { name: None, kind: LabelKind::Switch, statement_start: None }
    }
}

#[derive(Debug, Default)]
pub(super) struct Scope {
    pub flags: u32,
    pub var: Vec<Rc<str>>,
    pub lexical: Vec<Rc<str>>,
    pub functions: Vec<Rc<str>>,
}

#[derive(Debug, Default)]
pub(super) struct PrivateNameScope {
    /// Name to acorn's conflict-tracking state (`"true"`, `"iget"`, `"iset"`, `"sget"`, `"sset"`).
    pub declared: HashMap<Rc<str>, &'static str>,
    /// Used private names: `(name, start)`.
    pub used: Vec<(Rc<str>, usize)>,
}

/// acorn's `DestructuringErrors`.
#[derive(Debug, Clone, Copy)]
pub(super) struct DestructuringErrors {
    pub shorthand_assign: i64,
    pub trailing_comma: i64,
    pub parenthesized_assign: i64,
    pub parenthesized_bind: i64,
    pub double_proto: i64,
}

impl DestructuringErrors {
    pub(super) fn new() -> Self {
        DestructuringErrors {
            shorthand_assign: -1,
            trailing_comma: -1,
            parenthesized_assign: -1,
            parenthesized_bind: -1,
            double_proto: -1,
        }
    }
}

pub(super) struct Parser {
    pub input: Vec<u16>,

    // Tokenizer state.
    pub pos: usize,
    pub line_start: usize,
    pub cur_line: u32,
    pub ty: TT,
    pub value: TokVal,
    pub start: usize,
    pub end: usize,
    pub start_loc: Pos,
    pub last_tok_end: usize,
    pub last_tok_start: usize,
    pub last_tok_start_loc: Pos,
    pub context: Vec<Ctx>,
    pub expr_allowed: bool,
    pub contains_esc: bool,
    pub in_template_element: bool,

    // Parser state.
    pub strict: bool,
    pub potential_arrow_at: i64,
    pub potential_arrow_in_for_await: bool,
    pub yield_pos: usize,
    pub await_pos: usize,
    pub await_ident_pos: usize,
    pub labels: Vec<Label>,
    pub scope_stack: Vec<Scope>,
    pub private_name_stack: Vec<PrivateNameScope>,
    /// Nesting depth guard against native stack exhaustion.
    pub depth: u32,
}

/// Deepest nesting the parser allows before failing with "Maximum call stack size exceeded" (where acorn would run out
/// of JavaScript stack). One unit is a statement, an assignment-level or unary-level expression, a binding pattern,
/// an object literal, or one link of a binary-operator or subscript chain, so the AST depth stays bounded as well.
///
/// This is above every limit acorn reaches on node's default stack (695 nested parentheses, 2929 nested blocks, 4296
/// unary operators, 6958 binary operators), except that acorn parses member/call chains iteratively without limit.
/// Recursion costs up to ~6 KB of native stack per unit in release builds and ~36 KB in debug builds, so callers
/// parse on a thread with a large stack (the interpreter thread has 1 GiB).
const MAX_DEPTH: u32 = 10_000;

impl Parser {
    pub(super) fn new(source: &str) -> Parser {
        let input: Vec<u16> = source.encode_utf16().collect();
        let mut p = Parser {
            input,
            pos: 0,
            line_start: 0,
            cur_line: 1,
            ty: TT::Eof,
            value: TokVal::None,
            start: 0,
            end: 0,
            start_loc: Pos { line: 1, column: 0 },
            last_tok_end: 0,
            last_tok_start: 0,
            last_tok_start_loc: Pos { line: 1, column: 0 },
            context: vec![Ctx::BStat],
            expr_allowed: true,
            contains_esc: false,
            in_template_element: false,
            strict: false,
            potential_arrow_at: -1,
            potential_arrow_in_for_await: false,
            yield_pos: 0,
            await_pos: 0,
            await_ident_pos: 0,
            labels: Vec::new(),
            scope_stack: Vec::new(),
            private_name_stack: Vec::new(),
            depth: 0,
        };
        p.strict = p.strict_directive(0);
        if p.input.len() >= 2 && p.input[0] == b'#' as u16 && p.input[1] == b'!' as u16 {
            p.skip_line_comment(2);
        }
        p.enter_scope(SCOPE_TOP);
        p
    }

    pub(super) fn parse_program(mut self) -> Result<ast::Program, ParseError> {
        let result = self.next_token().and_then(|_| self.parse_top_level());
        match result {
            Ok(program) => Ok(program),
            Err(PErr::Syntax(e)) => Err(*e),
            // Only reachable if a template escape error escaped template tokenizing, which acorn never lets happen.
            Err(PErr::BadTemplateEscape) => Err(*self.raise_err(self.pos, "Unexpected token")),
        }
    }

    // ## Errors

    pub(super) fn raise_err(&self, pos: usize, message: &str) -> Box<ParseError> {
        let (line, column) = get_line_info(&self.input, pos);
        Box::new(ParseError { message: format!("{message} ({line}:{column})"), pos: pos as u32, line, column })
    }

    pub(super) fn raise(&self, pos: usize, message: &str) -> PErr {
        PErr::Syntax(self.raise_err(pos, message))
    }

    pub(super) fn unexpected_at(&self, pos: usize) -> PErr {
        self.raise(pos, "Unexpected token")
    }

    pub(super) fn unexpected(&self) -> PErr {
        self.raise(self.start, "Unexpected token")
    }

    pub(super) fn enter_depth(&mut self) -> R<()> {
        self.depth += 1;
        if self.depth > MAX_DEPTH {
            return Err(self.raise(self.start, "Maximum call stack size exceeded"));
        }
        Ok(())
    }

    pub(super) fn exit_depth(&mut self) {
        self.depth -= 1;
    }

    pub(super) fn cur_position(&self) -> Pos {
        Pos { line: self.cur_line, column: (self.pos - self.line_start) as u32 }
    }

    pub(super) fn loc(&self, start: usize, start_loc: Pos, end: usize) -> ast::Loc {
        ast::Loc { line: start_loc.line, column: start_loc.column, start: start as u32, end: end as u32 }
    }

    /// `finishNode` for a node started at `(start, start_loc)`: ends at the previous token's end.
    pub(super) fn finish_loc(&self, start: usize, start_loc: Pos) -> ast::Loc {
        self.loc(start, start_loc, self.last_tok_end)
    }

    pub(super) fn cc(&self, i: usize) -> u32 {
        match self.input.get(i) {
            Some(&c) => c as u32,
            None => super::chars::EOF_CODE,
        }
    }

    // ## Parser utilities

    pub(super) fn word_value(&self) -> Option<&str> {
        match &self.value {
            TokVal::Word(w) => Some(w),
            _ => None,
        }
    }

    pub(super) fn word_rc(&self) -> Rc<str> {
        match &self.value {
            TokVal::Word(w) => w.clone(),
            _ => Rc::from(""),
        }
    }

    pub(super) fn eat(&mut self, ty: TT) -> R<bool> {
        if self.ty == ty {
            self.next(false)?;
            Ok(true)
        } else {
            Ok(false)
        }
    }

    pub(super) fn is_contextual(&self, name: &str) -> bool {
        self.ty == TT::Name && self.word_value() == Some(name) && !self.contains_esc
    }

    pub(super) fn eat_contextual(&mut self, name: &str) -> R<bool> {
        if !self.is_contextual(name) {
            return Ok(false);
        }
        self.next(false)?;
        Ok(true)
    }

    pub(super) fn can_insert_semicolon(&self) -> bool {
        self.ty == TT::Eof || self.ty == TT::BraceR || has_line_break(&self.input, self.last_tok_end, self.start)
    }

    pub(super) fn insert_semicolon(&self) -> bool {
        self.can_insert_semicolon()
    }

    pub(super) fn semicolon(&mut self) -> R<()> {
        if !self.eat(TT::Semi)? && !self.insert_semicolon() {
            return Err(self.unexpected());
        }
        Ok(())
    }

    pub(super) fn after_trailing_comma(&mut self, ty: TT, not_next: bool) -> R<bool> {
        if self.ty == ty {
            if !not_next {
                self.next(false)?;
            }
            return Ok(true);
        }
        Ok(false)
    }

    pub(super) fn expect(&mut self, ty: TT) -> R<()> {
        if !self.eat(ty)? {
            return Err(self.unexpected());
        }
        Ok(())
    }

    pub(super) fn check_pattern_errors(&self, refd: Option<&DestructuringErrors>, is_assign: bool) -> R<()> {
        let Some(refd) = refd else { return Ok(()) };
        if refd.trailing_comma > -1 {
            return Err(self.raise(refd.trailing_comma as usize, "Comma is not permitted after the rest element"));
        }
        let parens = if is_assign { refd.parenthesized_assign } else { refd.parenthesized_bind };
        if parens > -1 {
            return Err(
                self.raise(parens as usize, if is_assign { "Assigning to rvalue" } else { "Parenthesized pattern" })
            );
        }
        Ok(())
    }

    /// `checkExpressionErrors(refDestructuringErrors, false)`.
    pub(super) fn has_expression_errors(&self, refd: Option<&DestructuringErrors>) -> bool {
        match refd {
            None => false,
            Some(r) => r.shorthand_assign >= 0 || r.double_proto >= 0,
        }
    }

    /// `checkExpressionErrors(refDestructuringErrors, true)`.
    pub(super) fn check_expression_errors(&self, refd: Option<&DestructuringErrors>) -> R<()> {
        let Some(r) = refd else { return Ok(()) };
        if r.shorthand_assign >= 0 {
            return Err(self.raise(
                r.shorthand_assign as usize,
                "Shorthand property assignments are valid only in destructuring patterns",
            ));
        }
        if r.double_proto >= 0 {
            return Err(self.raise(r.double_proto as usize, "Redefinition of __proto__ property"));
        }
        Ok(())
    }

    pub(super) fn check_yield_await_in_default_params(&self) -> R<()> {
        if self.yield_pos != 0 && (self.await_pos == 0 || self.yield_pos < self.await_pos) {
            return Err(self.raise(self.yield_pos, "Yield expression cannot be a default value"));
        }
        if self.await_pos != 0 {
            return Err(self.raise(self.await_pos, "Await expression cannot be a default value"));
        }
        Ok(())
    }

    /// acorn's `strictDirective(start)`.
    pub(super) fn strict_directive(&self, mut start: usize) -> bool {
        let s = &self.input;
        loop {
            start = skip_white_space(s, start);
            // literal = /^(?:'((?:\\[^]|[^'\\])*?)'|"((?:\\[^]|[^"\\])*?)")/
            let Some((content_start, content_end, match_end)) = match_string_literal(s, start) else {
                return false;
            };
            if &s[content_start..content_end] == "use strict".encode_utf16().collect::<Vec<_>>().as_slice() {
                let space_start = match_end;
                let end = skip_white_space(s, space_start);
                let next = s.get(end).copied().map(|c| c as u32);
                if next == Some(b';' as u32) || next == Some(b'}' as u32) {
                    return true;
                }
                if !has_line_break(s, space_start, end) {
                    return false;
                }
                let starts_operator = match next {
                    Some(c) => "(`.[+-/*%<>=,?^&".encode_utf16().any(|x| x as u32 == c),
                    None => false,
                };
                let not_eq = next == Some(b'!' as u32) && s.get(end + 1).copied() == Some(b'=' as u16);
                return !(starts_operator || not_eq);
            }
            start = match_end;
            start = skip_white_space(s, start);
            if s.get(start).copied() == Some(b';' as u16) {
                start += 1;
            }
        }
    }

    /// acorn's `isLet(context)`.
    pub(super) fn is_let(&self, context: Option<&str>) -> bool {
        if !self.is_contextual("let") {
            return false;
        }
        let next = skip_white_space(&self.input, self.pos);
        let mut next_ch = self.cc(next);
        if next_ch == 91 || next_ch == 92 {
            return true;
        }
        if context.is_some() {
            return false;
        }
        if next_ch == 123 || (next_ch > 0xd7ff && next_ch < 0xdc00) {
            return true;
        }
        if is_identifier_start(next_ch, true) {
            let mut pos = next + 1;
            loop {
                next_ch = self.cc(pos);
                if !is_identifier_char(next_ch, true) {
                    break;
                }
                pos += 1;
            }
            if next_ch == 92 || (next_ch > 0xd7ff && next_ch < 0xdc00) {
                return true;
            }
            let ident = &self.input[next..pos];
            let is_rel = ident == [b'i' as u16, b'n' as u16]
                || ident == "instanceof".encode_utf16().collect::<Vec<_>>().as_slice();
            if !is_rel {
                return true;
            }
        }
        false
    }

    /// acorn's `isAsyncFunction()`.
    pub(super) fn is_async_function(&self) -> bool {
        if !self.is_contextual("async") {
            return false;
        }
        let next = skip_white_space(&self.input, self.pos);
        if has_line_break(&self.input, self.pos, next) {
            return false;
        }
        if !self.slice_eq(next, "function") {
            return false;
        }
        if next + 8 == self.input.len() {
            return true;
        }
        let after = self.cc(next + 8);
        !(is_identifier_char(after, true) || (after > 0xd7ff && after < 0xdc00))
    }

    pub(super) fn slice_eq(&self, at: usize, s: &str) -> bool {
        s.encode_utf16().enumerate().all(|(i, c)| self.input.get(at + i) == Some(&c))
    }

    /// acorn's `isUsingKeyword(isAwaitUsing, isFor)`.
    pub(super) fn is_using_keyword(&self, is_await_using: bool, is_for: bool) -> bool {
        if !self.is_contextual(if is_await_using { "await" } else { "using" }) {
            return false;
        }
        let next = skip_white_space(&self.input, self.pos);
        if has_line_break(&self.input, self.pos, next) {
            return false;
        }
        if is_await_using {
            let await_end_pos = next + 5;
            if !self.slice_eq(next, "using") || await_end_pos == self.input.len() {
                return false;
            }
            let after = self.cc(await_end_pos);
            if is_identifier_char(after, true) || (after > 0xd7ff && after < 0xdc00) {
                return false;
            }
            let skip_end = skip_white_space(&self.input, await_end_pos);
            if has_line_break(&self.input, await_end_pos, skip_end) {
                return false;
            }
        }
        if is_for {
            let of_end_pos = next + 2;
            if self.slice_eq(next, "of") {
                if of_end_pos == self.input.len() {
                    return false;
                }
                let after = self.cc(of_end_pos);
                if !is_identifier_char(after, true) && !(after > 0xd7ff && after < 0xdc00) {
                    return false;
                }
            }
        }
        let ch = self.cc(next);
        is_identifier_start(ch, true) || ch == 92
    }

    pub(super) fn is_await_using(&self, is_for: bool) -> bool {
        self.is_using_keyword(true, is_for)
    }

    pub(super) fn is_using(&self, is_for: bool) -> bool {
        self.is_using_keyword(false, is_for)
    }

    // ## Scope

    pub(super) fn enter_scope(&mut self, flags: u32) {
        self.scope_stack.push(Scope { flags, ..Default::default() });
    }

    pub(super) fn exit_scope(&mut self) {
        self.scope_stack.pop();
    }

    pub(super) fn treat_functions_as_var_in_scope(scope: &Scope) -> bool {
        (scope.flags & SCOPE_FUNCTION) != 0 || (scope.flags & SCOPE_TOP) != 0
    }

    pub(super) fn treat_functions_as_var(&self) -> bool {
        Self::treat_functions_as_var_in_scope(self.current_scope())
    }

    pub(super) fn current_scope(&self) -> &Scope {
        self.scope_stack.last().expect("scope stack is never empty")
    }

    pub(super) fn current_var_scope(&self) -> &Scope {
        self.scope_stack
            .iter()
            .rev()
            .find(|s| s.flags & (SCOPE_VAR | SCOPE_CLASS_FIELD_INIT | SCOPE_CLASS_STATIC_BLOCK) != 0)
            .expect("top scope is a var scope")
    }

    pub(super) fn current_this_scope(&self) -> &Scope {
        self.scope_stack
            .iter()
            .rev()
            .find(|s| {
                s.flags & (SCOPE_VAR | SCOPE_CLASS_FIELD_INIT | SCOPE_CLASS_STATIC_BLOCK) != 0
                    && s.flags & SCOPE_ARROW == 0
            })
            .expect("top scope is a this scope")
    }

    pub(super) fn in_generator(&self) -> bool {
        self.current_var_scope().flags & SCOPE_GENERATOR > 0
    }

    pub(super) fn in_async(&self) -> bool {
        self.current_var_scope().flags & SCOPE_ASYNC > 0
    }

    pub(super) fn can_await(&self) -> bool {
        for scope in self.scope_stack.iter().rev() {
            let flags = scope.flags;
            if flags & (SCOPE_CLASS_STATIC_BLOCK | SCOPE_CLASS_FIELD_INIT) != 0 {
                return false;
            }
            if flags & SCOPE_FUNCTION != 0 {
                return flags & SCOPE_ASYNC > 0;
            }
        }
        // allowAwaitOutsideFunction
        true
    }

    pub(super) fn allow_super(&self) -> bool {
        self.current_this_scope().flags & SCOPE_SUPER > 0
    }

    pub(super) fn allow_direct_super(&self) -> bool {
        self.current_this_scope().flags & SCOPE_DIRECT_SUPER > 0
    }

    pub(super) fn allow_new_dot_target(&self) -> bool {
        for scope in self.scope_stack.iter().rev() {
            let flags = scope.flags;
            if flags & (SCOPE_CLASS_STATIC_BLOCK | SCOPE_CLASS_FIELD_INIT) != 0
                || (flags & SCOPE_FUNCTION != 0 && flags & SCOPE_ARROW == 0)
            {
                return true;
            }
        }
        false
    }

    pub(super) fn in_class_static_block(&self) -> bool {
        self.current_var_scope().flags & SCOPE_CLASS_STATIC_BLOCK > 0
    }

    pub(super) fn declare_name(&mut self, name: &Rc<str>, binding_type: u8, pos: usize) -> R<()> {
        let mut redeclared = false;
        if binding_type == BIND_LEXICAL {
            let scope = self.scope_stack.last_mut().expect("scope");
            redeclared = scope.lexical.contains(name) || scope.functions.contains(name) || scope.var.contains(name);
            scope.lexical.push(name.clone());
        } else if binding_type == BIND_SIMPLE_CATCH {
            let scope = self.scope_stack.last_mut().expect("scope");
            scope.lexical.push(name.clone());
        } else if binding_type == BIND_FUNCTION {
            let treat = self.treat_functions_as_var();
            let scope = self.scope_stack.last_mut().expect("scope");
            redeclared = if treat {
                scope.lexical.contains(name)
            } else {
                scope.lexical.contains(name) || scope.var.contains(name)
            };
            scope.functions.push(name.clone());
        } else {
            for i in (0..self.scope_stack.len()).rev() {
                let scope = &self.scope_stack[i];
                if (scope.lexical.contains(name)
                    && !(scope.flags & SCOPE_SIMPLE_CATCH != 0 && scope.lexical.first() == Some(name)))
                    || (!Self::treat_functions_as_var_in_scope(scope) && scope.functions.contains(name))
                {
                    redeclared = true;
                    break;
                }
                let scope = &mut self.scope_stack[i];
                scope.var.push(name.clone());
                if scope.flags & SCOPE_VAR != 0 {
                    break;
                }
            }
        }
        if redeclared {
            return Err(self.raise(pos, &format!("Identifier '{name}' has already been declared")));
        }
        Ok(())
    }
}

/// Matches acorn's directive `literal` regexp at `start`: `(content_start, content_end, match_end)`.
fn match_string_literal(s: &[u16], start: usize) -> Option<(usize, usize, usize)> {
    let quote = *s.get(start)?;
    if quote != b'\'' as u16 && quote != b'"' as u16 {
        return None;
    }
    let mut i = start + 1;
    loop {
        let c = *s.get(i)?;
        if c == quote {
            return Some((start + 1, i, i + 1));
        }
        if c == b'\\' as u16 {
            if i + 1 >= s.len() {
                return None;
            }
            i += 2;
        } else {
            i += 1;
        }
    }
}

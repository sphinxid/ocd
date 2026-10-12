//! Statement parsing (acorn's `statement.js`), including the class syntax the AST only keeps as unsupported.

use super::chars::{has_line_break, skip_white_space};
use super::expression::{FUNC_HANGING_STATEMENT, FUNC_STATEMENT, key_is};
use super::node::{ClassElement, InFor};
use super::state::{
    BIND_LEXICAL, BIND_NONE, BIND_SIMPLE_CATCH, BIND_VAR, DestructuringErrors, Label, LabelKind, Parser,
    PrivateNameScope, R, SCOPE_CLASS_FIELD_INIT, SCOPE_CLASS_STATIC_BLOCK, SCOPE_SIMPLE_CATCH, SCOPE_SUPER,
};
use super::tokentype::TT;
use crate::ast::{
    self, Block, CatchClause, Expr, ForHead, ForInit, Ident, Pat, PropKey, Stmt, SwitchCase, VarDecl, VarDeclarator,
    VarKind,
};
use std::rc::Rc;

/// acorn's statement `context` argument (`"if"`, `"label"`, `"iflabel"`, ...).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct StmtContext {
    base: &'static str,
    label: bool,
}

impl StmtContext {
    const fn of(base: &'static str) -> Option<StmtContext> {
        Some(StmtContext { base, label: false })
    }

    /// `context === "if"` / `context === "label"`.
    fn is_exactly(self, s: &str) -> bool {
        if s == "label" { self.base.is_empty() && self.label } else { self.base == s && !self.label }
    }

    /// The context of a labeled statement's body.
    fn with_label(ctx: Option<StmtContext>) -> StmtContext {
        match ctx {
            Some(c) => StmtContext { base: c.base, label: true },
            None => StmtContext { base: "", label: true },
        }
    }
}

impl Parser {
    pub(super) fn parse_top_level(&mut self) -> R<ast::Program> {
        let mut body = Vec::new();
        while self.ty != TT::Eof {
            body.push(self.parse_statement(None, true)?);
        }
        self.adapt_directive_prologue(&mut body);
        self.next(false)?;
        let loc = ast::Loc { line: 1, column: 0, start: 0, end: self.last_tok_end as u32 };
        Ok(ast::Program { body, loc })
    }

    /// Parses a single statement.
    pub(super) fn parse_statement(&mut self, context: Option<StmtContext>, top_level: bool) -> R<Stmt> {
        self.enter_depth()?;
        let result = self.parse_statement_inner(context, top_level);
        self.exit_depth();
        result
    }

    fn parse_statement_inner(&mut self, context: Option<StmtContext>, top_level: bool) -> R<Stmt> {
        let mut starttype = self.ty;
        let (start, start_loc) = (self.start, self.start_loc);
        let mut kind = None;
        if self.is_let(context.is_some().then_some("")) {
            starttype = TT::Var;
            kind = Some(VarKind::Let);
        }

        match starttype {
            TT::Break | TT::Continue => self.parse_break_continue_statement(starttype == TT::Break),
            TT::Debugger => {
                self.next(false)?;
                self.semicolon()?;
                Ok(Stmt::Unsupported { kind: "DebuggerStatement", loc: self.finish_loc(start, start_loc) })
            }
            TT::Do => {
                self.next(false)?;
                self.labels.push(Label::loop_label());
                let body = self.parse_statement(StmtContext::of("do"), false)?;
                self.labels.pop();
                self.expect(TT::While)?;
                let test = self.parse_paren_expression()?;
                self.eat(TT::Semi)?;
                Ok(Stmt::DoWhile { body: Box::new(body), test, loc: self.finish_loc(start, start_loc) })
            }
            TT::For => self.parse_for_statement(),
            TT::Function => {
                // A function is allowed as the sole body of an if statement or labeled statement (sloppy mode only),
                // but not of a labeled statement that is itself the body of an if statement.
                if let Some(ctx) = context
                    && (self.strict || (!ctx.is_exactly("if") && !ctx.is_exactly("label")))
                {
                    return Err(self.unexpected());
                }
                self.parse_function_statement(start, start_loc, false, context.is_none())
            }
            TT::Class => {
                if context.is_some() {
                    return Err(self.unexpected());
                }
                self.parse_class(true)?;
                Ok(Stmt::Unsupported { kind: "ClassDeclaration", loc: self.finish_loc(start, start_loc) })
            }
            TT::If => {
                self.next(false)?;
                let test = self.parse_paren_expression()?;
                let consequent = self.parse_statement(StmtContext::of("if"), false)?;
                let alternate = if self.eat(TT::Else)? {
                    Some(Box::new(self.parse_statement(StmtContext::of("if"), false)?))
                } else {
                    None
                };
                Ok(Stmt::If {
                    test,
                    consequent: Box::new(consequent),
                    alternate,
                    loc: self.finish_loc(start, start_loc),
                })
            }
            TT::Return => {
                self.next(false)?;
                let argument = if self.eat(TT::Semi)? || self.insert_semicolon() {
                    None
                } else {
                    let arg = self.parse_expression(InFor::No, None)?;
                    self.semicolon()?;
                    Some(arg)
                };
                Ok(Stmt::Return { argument, loc: self.finish_loc(start, start_loc) })
            }
            TT::Switch => self.parse_switch_statement(),
            TT::Throw => {
                self.next(false)?;
                if has_line_break(&self.input, self.last_tok_end, self.start) {
                    return Err(self.raise(self.last_tok_end, "Illegal newline after throw"));
                }
                let argument = self.parse_expression(InFor::No, None)?;
                self.semicolon()?;
                Ok(Stmt::Throw { argument, loc: self.finish_loc(start, start_loc) })
            }
            TT::Try => self.parse_try_statement(),
            TT::Const | TT::Var => {
                let kind = kind.unwrap_or(if self.ty == TT::Const { VarKind::Const } else { VarKind::Var });
                if context.is_some() && kind != VarKind::Var {
                    return Err(self.unexpected());
                }
                self.next(false)?;
                let declarations = self.parse_var(false, kind, false)?;
                self.semicolon()?;
                Ok(Stmt::VarDecl(VarDecl { kind, declarations, loc: self.finish_loc(start, start_loc) }))
            }
            TT::While => {
                self.next(false)?;
                let test = self.parse_paren_expression()?;
                self.labels.push(Label::loop_label());
                let body = self.parse_statement(StmtContext::of("while"), false)?;
                self.labels.pop();
                Ok(Stmt::While { test, body: Box::new(body), loc: self.finish_loc(start, start_loc) })
            }
            TT::With => {
                if self.strict {
                    return Err(self.raise(self.start, "'with' in strict mode"));
                }
                self.next(false)?;
                self.parse_paren_expression()?;
                self.parse_statement(StmtContext::of("with"), false)?;
                Ok(Stmt::Unsupported { kind: "WithStatement", loc: self.finish_loc(start, start_loc) })
            }
            TT::BraceL => Ok(Stmt::Block(self.parse_block(true, false)?)),
            TT::Semi => {
                self.next(false)?;
                Ok(Stmt::Empty(self.finish_loc(start, start_loc)))
            }
            TT::Export | TT::Import => {
                if starttype == TT::Import {
                    let next = skip_white_space(&self.input, self.pos);
                    let next_ch = self.cc(next);
                    if next_ch == 40 || next_ch == 46 {
                        let expr = self.parse_expression(InFor::No, None)?;
                        return self.parse_expression_statement(expr, start, start_loc);
                    }
                }
                if !top_level {
                    return Err(self.raise(self.start, "'import' and 'export' may only appear at the top level"));
                }
                Err(self.raise(self.start, "'import' and 'export' may appear only with 'sourceType: module'"))
            }
            _ => {
                if self.is_async_function() {
                    if context.is_some() {
                        return Err(self.unexpected());
                    }
                    self.next(false)?;
                    return self.parse_function_statement(start, start_loc, true, context.is_none());
                }
                let using_kind = if self.is_await_using(false) {
                    Some(VarKind::AwaitUsing)
                } else if self.is_using(false) {
                    Some(VarKind::Using)
                } else {
                    None
                };
                if let Some(kind) = using_kind {
                    if top_level {
                        return Err(self.raise(
                            self.start,
                            "Using declaration cannot appear in the top level when source type is `script`",
                        ));
                    }
                    if kind == VarKind::AwaitUsing {
                        if !self.can_await() {
                            return Err(self.raise(self.start, "Await using cannot appear outside of async function"));
                        }
                        self.next(false)?;
                    }
                    self.next(false)?;
                    let declarations = self.parse_var(false, kind, false)?;
                    self.semicolon()?;
                    return Ok(Stmt::VarDecl(VarDecl { kind, declarations, loc: self.finish_loc(start, start_loc) }));
                }

                let expr = self.parse_expression(InFor::No, None)?;
                if starttype == TT::Name && matches!(expr, Expr::Ident(_)) && self.eat(TT::Colon)? {
                    let Expr::Ident(id) = expr else { unreachable!("checked above") };
                    return self.parse_labeled_statement(start, start_loc, id, context);
                }
                self.parse_expression_statement(expr, start, start_loc)
            }
        }
    }

    fn parse_break_continue_statement(&mut self, is_break: bool) -> R<Stmt> {
        let (start, start_loc) = (self.start, self.start_loc);
        self.next(false)?;
        let label = if self.eat(TT::Semi)? || self.insert_semicolon() {
            None
        } else if self.ty != TT::Name {
            return Err(self.unexpected());
        } else {
            let label = self.parse_ident(false)?;
            self.semicolon()?;
            Some(label)
        };

        // Verify that there is an actual destination to break or continue to.
        let found = self.labels.iter().any(|lab| {
            let name_matches = match &label {
                None => true,
                Some(l) => lab.name.as_deref() == Some(&*l.name),
            };
            name_matches
                && ((lab.kind != LabelKind::None && (is_break || lab.kind == LabelKind::Loop))
                    || (label.is_some() && is_break))
        });
        if !found {
            let keyword = if is_break { "break" } else { "continue" };
            return Err(self.raise(start, &format!("Unsyntactic {keyword}")));
        }
        let loc = self.finish_loc(start, start_loc);
        Ok(if is_break { Stmt::Break { label, loc } } else { Stmt::Continue { label, loc } })
    }

    fn parse_for_statement(&mut self) -> R<Stmt> {
        let (start, start_loc) = (self.start, self.start_loc);
        self.next(false)?;
        let await_at = if self.can_await() && self.eat_contextual("await")? { Some(self.last_tok_start) } else { None };
        self.labels.push(Label::loop_label());
        self.enter_scope(0);
        self.expect(TT::ParenL)?;
        if self.ty == TT::Semi {
            if let Some(at) = await_at {
                return Err(self.unexpected_at(at));
            }
            return self.parse_for(start, start_loc, None);
        }
        let is_let = self.is_let(None);
        if self.ty == TT::Var || self.ty == TT::Const || is_let {
            let (init_start, init_start_loc) = (self.start, self.start_loc);
            let kind = if is_let {
                VarKind::Let
            } else if self.ty == TT::Const {
                VarKind::Const
            } else {
                VarKind::Var
            };
            self.next(false)?;
            let declarations = self.parse_var(true, kind, false)?;
            let init = VarDecl { kind, declarations, loc: self.finish_loc(init_start, init_start_loc) };
            return self.parse_for_after_init(start, start_loc, init, await_at);
        }
        let starts_with_let = self.is_contextual("let");

        let using_kind = if self.is_using(true) {
            Some(VarKind::Using)
        } else if self.is_await_using(true) {
            Some(VarKind::AwaitUsing)
        } else {
            None
        };
        if let Some(kind) = using_kind {
            let (init_start, init_start_loc) = (self.start, self.start_loc);
            self.next(false)?;
            if kind == VarKind::AwaitUsing {
                self.next(false)?;
            }
            let declarations = self.parse_var(true, kind, false)?;
            let init = VarDecl { kind, declarations, loc: self.finish_loc(init_start, init_start_loc) };
            return self.parse_for_after_init(start, start_loc, init, await_at);
        }

        let contains_esc = self.contains_esc;
        let mut refd = DestructuringErrors::new();
        let init_pos = self.start;
        let init = if await_at.is_some() {
            self.parse_expr_subscripts(Some(&mut refd), InFor::Await)?
        } else {
            self.parse_expression(InFor::Yes, Some(&mut refd))?
        };
        let is_for_of = self.ty != TT::In && self.is_contextual("of");
        if self.ty == TT::In || is_for_of {
            let mut is_await = false;
            if let Some(at) = await_at {
                if self.ty == TT::In {
                    return Err(self.unexpected_at(at));
                }
                is_await = true;
            } else if is_for_of
                && let Expr::Ident(id) = &init
                && id.loc.start as usize == init_pos
                && !contains_esc
                && &*id.name == "async"
            {
                return Err(self.unexpected());
            }
            if starts_with_let && is_for_of {
                return Err(self.raise(
                    init.loc().start as usize,
                    "The left-hand side of a for-of loop may not start with 'let'.",
                ));
            }
            let pat = self.make_assignable(init, false, Some(&refd))?;
            self.check_lval_pattern(&pat, BIND_NONE, None)?;
            return self.parse_for_in(start, start_loc, ForHead::Pat(pat), is_await);
        } else {
            self.check_expression_errors(Some(&refd))?;
        }
        if let Some(at) = await_at {
            return Err(self.unexpected_at(at));
        }
        self.parse_for(start, start_loc, Some(ForInit::Expr(init)))
    }

    fn parse_for_after_init(
        &mut self,
        start: usize,
        start_loc: super::state::Pos,
        init: VarDecl,
        await_at: Option<usize>,
    ) -> R<Stmt> {
        if (self.ty == TT::In || self.is_contextual("of")) && init.declarations.len() == 1 {
            if self.ty == TT::In
                && let Some(at) = await_at
            {
                return Err(self.unexpected_at(at));
            }
            return self.parse_for_in(start, start_loc, ForHead::Var(init), await_at.is_some());
        }
        if let Some(at) = await_at {
            return Err(self.unexpected_at(at));
        }
        self.parse_for(start, start_loc, Some(ForInit::Var(init)))
    }

    fn parse_for(&mut self, start: usize, start_loc: super::state::Pos, init: Option<ForInit>) -> R<Stmt> {
        self.expect(TT::Semi)?;
        let test = if self.ty == TT::Semi { None } else { Some(self.parse_expression(InFor::No, None)?) };
        self.expect(TT::Semi)?;
        let update = if self.ty == TT::ParenR { None } else { Some(self.parse_expression(InFor::No, None)?) };
        self.expect(TT::ParenR)?;
        let body = self.parse_statement(StmtContext::of("for"), false)?;
        self.exit_scope();
        self.labels.pop();
        Ok(Stmt::For { init, test, update, body: Box::new(body), loc: self.finish_loc(start, start_loc) })
    }

    fn parse_for_in(&mut self, start: usize, start_loc: super::state::Pos, init: ForHead, is_await: bool) -> R<Stmt> {
        let is_for_in = self.ty == TT::In;
        self.next(false)?;
        if let ForHead::Var(decl) = &init {
            let first = &decl.declarations[0];
            if first.init.is_some()
                && (!is_for_in || self.strict || decl.kind != VarKind::Var || !matches!(first.id, Pat::Ident(_)))
            {
                let what = if is_for_in { "for-in" } else { "for-of" };
                return Err(self.raise(
                    decl.loc.start as usize,
                    &format!("{what} loop variable declaration may not have an initializer"),
                ));
            }
        }
        let right =
            if is_for_in { self.parse_expression(InFor::No, None)? } else { self.parse_maybe_assign(InFor::No, None)? };
        self.expect(TT::ParenR)?;
        let body = Box::new(self.parse_statement(StmtContext::of("for"), false)?);
        self.exit_scope();
        self.labels.pop();
        let loc = self.finish_loc(start, start_loc);
        Ok(if is_for_in {
            Stmt::ForIn { left: init, right, body, loc }
        } else {
            Stmt::ForOf { left: init, right, body, is_await, loc }
        })
    }

    fn parse_function_statement(
        &mut self,
        start: usize,
        start_loc: super::state::Pos,
        is_async: bool,
        declaration_position: bool,
    ) -> R<Stmt> {
        self.next(false)?;
        let flags = FUNC_STATEMENT | if declaration_position { 0 } else { FUNC_HANGING_STATEMENT };
        let f = self.parse_function(start, start_loc, flags, is_async, InFor::No)?;
        Ok(Stmt::FunctionDecl(f))
    }

    fn parse_switch_statement(&mut self) -> R<Stmt> {
        let (start, start_loc) = (self.start, self.start_loc);
        self.next(false)?;
        let discriminant = self.parse_paren_expression()?;
        let mut cases: Vec<SwitchCase> = Vec::new();
        self.expect(TT::BraceL)?;
        self.labels.push(Label::switch_label());
        self.enter_scope(0);

        // The case being filled: its test, statements and start.
        let mut cur: Option<(Option<Expr>, Vec<Stmt>, usize, super::state::Pos)> = None;
        let mut saw_default = false;
        while self.ty != TT::BraceR {
            if self.ty == TT::Case || self.ty == TT::Default {
                let is_case = self.ty == TT::Case;
                if let Some((test, consequent, s, sl)) = cur.take() {
                    cases.push(SwitchCase { test, consequent, loc: self.finish_loc(s, sl) });
                }
                let (case_start, case_start_loc) = (self.start, self.start_loc);
                self.next(false)?;
                let test = if is_case {
                    Some(self.parse_expression(InFor::No, None)?)
                } else {
                    if saw_default {
                        return Err(self.raise(self.last_tok_start, "Multiple default clauses"));
                    }
                    saw_default = true;
                    None
                };
                cur = Some((test, Vec::new(), case_start, case_start_loc));
                self.expect(TT::Colon)?;
            } else {
                let Some((_, consequent, _, _)) = cur.as_mut() else { return Err(self.unexpected()) };
                let stmt = self.parse_statement(None, false)?;
                consequent.push(stmt);
            }
        }
        self.exit_scope();
        if let Some((test, consequent, s, sl)) = cur.take() {
            cases.push(SwitchCase { test, consequent, loc: self.finish_loc(s, sl) });
        }
        self.next(false)?;
        self.labels.pop();
        Ok(Stmt::Switch { discriminant, cases, loc: self.finish_loc(start, start_loc) })
    }

    fn parse_catch_clause_param(&mut self) -> R<Pat> {
        let param = self.parse_binding_atom()?;
        let simple = matches!(param, Pat::Ident(_));
        self.enter_scope(if simple { SCOPE_SIMPLE_CATCH } else { 0 });
        self.check_lval_pattern(&param, if simple { BIND_SIMPLE_CATCH } else { BIND_LEXICAL }, None)?;
        self.expect(TT::ParenR)?;
        Ok(param)
    }

    fn parse_try_statement(&mut self) -> R<Stmt> {
        let (start, start_loc) = (self.start, self.start_loc);
        self.next(false)?;
        let block = self.parse_block(true, false)?;
        let mut handler = None;
        if self.ty == TT::Catch {
            let (clause_start, clause_start_loc) = (self.start, self.start_loc);
            self.next(false)?;
            let param = if self.eat(TT::ParenL)? {
                Some(self.parse_catch_clause_param()?)
            } else {
                self.enter_scope(0);
                None
            };
            let body = self.parse_block(false, false)?;
            self.exit_scope();
            handler = Some(CatchClause { param, body, loc: self.finish_loc(clause_start, clause_start_loc) });
        }
        let finalizer = if self.eat(TT::Finally)? { Some(self.parse_block(true, false)?) } else { None };
        if handler.is_none() && finalizer.is_none() {
            return Err(self.raise(start, "Missing catch or finally clause"));
        }
        Ok(Stmt::Try { block, handler, finalizer, loc: self.finish_loc(start, start_loc) })
    }

    fn parse_labeled_statement(
        &mut self,
        start: usize,
        start_loc: super::state::Pos,
        label: Ident,
        context: Option<StmtContext>,
    ) -> R<Stmt> {
        let name = label.name.clone();
        if self.labels.iter().any(|l| l.name.as_deref() == Some(&*name)) {
            return Err(self.raise(label.loc.start as usize, &format!("Label '{name}' is already declared")));
        }
        let kind = if self.ty.is_loop() {
            LabelKind::Loop
        } else if self.ty == TT::Switch {
            LabelKind::Switch
        } else {
            LabelKind::None
        };
        let cur_start = self.start;
        for l in self.labels.iter_mut().rev() {
            if l.statement_start == Some(start) {
                // Update information about previous labels on this statement.
                l.statement_start = Some(cur_start);
                l.kind = kind;
            } else {
                break;
            }
        }
        self.labels.push(Label { name: Some(name), kind, statement_start: Some(cur_start) });
        let body = self.parse_statement(Some(StmtContext::with_label(context)), false)?;
        self.labels.pop();
        Ok(Stmt::Labeled { label, body: Box::new(body), loc: self.finish_loc(start, start_loc) })
    }

    fn parse_expression_statement(&mut self, expr: Expr, start: usize, start_loc: super::state::Pos) -> R<Stmt> {
        self.semicolon()?;
        Ok(Stmt::Expr { expr, directive: None, loc: self.finish_loc(start, start_loc) })
    }

    /// Parses a braced block; `exit_strict` leaves strict mode before the closing brace's successor is read.
    pub(super) fn parse_block(&mut self, create_new_lexical_scope: bool, exit_strict: bool) -> R<Block> {
        let (start, start_loc) = (self.start, self.start_loc);
        let mut body = Vec::new();
        self.expect(TT::BraceL)?;
        if create_new_lexical_scope {
            self.enter_scope(0);
        }
        while self.ty != TT::BraceR {
            body.push(self.parse_statement(None, false)?);
        }
        if exit_strict {
            self.strict = false;
        }
        self.next(false)?;
        if create_new_lexical_scope {
            self.exit_scope();
        }
        Ok(Block { body, loc: self.finish_loc(start, start_loc) })
    }

    /// Parses a list of variable declarators.
    fn parse_var(&mut self, is_for: bool, kind: VarKind, allow_missing_initializer: bool) -> R<Vec<VarDeclarator>> {
        let mut declarations = Vec::new();
        loop {
            let (start, start_loc) = (self.start, self.start_loc);
            let id = if matches!(kind, VarKind::Using | VarKind::AwaitUsing) {
                Pat::Ident(self.parse_ident(false)?)
            } else {
                self.parse_binding_atom()?
            };
            self.check_lval_pattern(&id, if kind == VarKind::Var { BIND_VAR } else { BIND_LEXICAL }, None)?;
            let init = if self.eat(TT::Eq)? {
                Some(self.parse_maybe_assign(InFor::from_bool(is_for), None)?)
            } else if !allow_missing_initializer
                && kind == VarKind::Const
                && !(self.ty == TT::In || self.is_contextual("of"))
            {
                return Err(self.unexpected());
            } else if !allow_missing_initializer
                && matches!(kind, VarKind::Using | VarKind::AwaitUsing)
                && self.ty != TT::In
                && !self.is_contextual("of")
            {
                return Err(
                    self.raise(self.last_tok_end, &format!("Missing initializer in {} declaration", kind.as_str()))
                );
            } else if !allow_missing_initializer
                && !matches!(id, Pat::Ident(_))
                && !(is_for && (self.ty == TT::In || self.is_contextual("of")))
            {
                return Err(self.raise(self.last_tok_end, "Complex binding patterns require an initialization value"));
            } else {
                None
            };
            declarations.push(VarDeclarator { id, init, loc: self.finish_loc(start, start_loc) });
            if !self.eat(TT::Comma)? {
                break;
            }
        }
        Ok(declarations)
    }

    // ## Classes (parsed and validated, kept as unsupported nodes)

    /// Parses a class declaration (`is_statement`) or expression; the caller builds the node.
    pub(super) fn parse_class(&mut self, is_statement: bool) -> R<()> {
        self.next(false)?;
        // A class definition is always strict mode code.
        let old_strict = self.strict;
        self.strict = true;

        if self.ty == TT::Name {
            let id = self.parse_ident(false)?;
            if is_statement {
                self.check_lval_ident(&id, BIND_LEXICAL, None)?;
            }
        } else if is_statement {
            return Err(self.unexpected());
        }
        let has_super_class = if self.eat(TT::Extends)? {
            self.parse_expr_subscripts(None, InFor::No)?;
            true
        } else {
            false
        };
        self.private_name_stack.push(PrivateNameScope::default());
        self.expect(TT::BraceL)?;
        let mut had_constructor = false;
        while self.ty != TT::BraceR {
            let Some(element) = self.parse_class_element(has_super_class)? else { continue };
            if element.is_method && element.kind == "constructor" {
                if had_constructor {
                    return Err(self.raise(element.start, "Duplicate constructor in the same class"));
                }
                had_constructor = true;
            } else if let Some(PropKey::Private(key)) = &element.key
                && self.is_private_name_conflicted(&element, key)
            {
                return Err(self
                    .raise(key.loc.start as usize, &format!("Identifier '#{}' has already been declared", key.name)));
            }
        }
        self.strict = old_strict;
        self.next(false)?;
        self.exit_class_body()
    }

    fn is_private_name_conflicted(&mut self, element: &ClassElement, key: &Ident) -> bool {
        let declared = &mut self.private_name_stack.last_mut().expect("class body").declared;
        let next: &'static str = if element.is_method && (element.kind == "get" || element.kind == "set") {
            match (element.is_static, element.kind) {
                (true, "get") => "sget",
                (true, _) => "sset",
                (false, "get") => "iget",
                (false, _) => "iset",
            }
        } else {
            "true"
        };
        let curr = declared.get(&key.name).copied();
        match (curr, next) {
            (Some("iget"), "iset") | (Some("iset"), "iget") | (Some("sget"), "sset") | (Some("sset"), "sget") => {
                declared.insert(key.name.clone(), "true");
                false
            }
            (None, _) => {
                declared.insert(key.name.clone(), next);
                false
            }
            _ => true,
        }
    }

    fn exit_class_body(&mut self) -> R<()> {
        let scope = self.private_name_stack.pop().expect("class body");
        for (name, start) in scope.used {
            if !scope.declared.contains_key(&name) {
                match self.private_name_stack.last_mut() {
                    Some(parent) => parent.used.push((name, start)),
                    None => {
                        return Err(self
                            .raise(start, &format!("Private field '#{name}' must be declared in an enclosing class")));
                    }
                }
            }
        }
        Ok(())
    }

    fn is_class_element_name_start(&self) -> bool {
        matches!(self.ty, TT::Name | TT::PrivateId | TT::Num | TT::String | TT::BracketL) || self.ty.is_keyword()
    }

    fn parse_class_element(&mut self, constructor_allows_super: bool) -> R<Option<ClassElement>> {
        if self.eat(TT::Semi)? {
            return Ok(None);
        }
        let start = self.start;
        let mut key_name: Option<&'static str> = None;
        let mut is_generator = false;
        let mut is_async = false;
        let mut kind: &'static str = "method";
        let mut is_static = false;

        if self.eat_contextual("static")? {
            // A static initialization block.
            if self.eat(TT::BraceL)? {
                self.parse_class_static_block()?;
                return Ok(Some(ClassElement { is_method: false, kind: "", is_static: true, start, key: None }));
            }
            if self.is_class_element_name_start() || self.ty == TT::Star {
                is_static = true;
            } else {
                key_name = Some("static");
            }
        }
        if key_name.is_none() && self.eat_contextual("async")? {
            if (self.is_class_element_name_start() || self.ty == TT::Star) && !self.can_insert_semicolon() {
                is_async = true;
            } else {
                key_name = Some("async");
            }
        }
        if key_name.is_none() && self.eat(TT::Star)? {
            is_generator = true;
        }
        if key_name.is_none() && !is_async && !is_generator {
            let last_value = match self.word_value() {
                Some("get") => "get",
                Some("set") => "set",
                _ => "",
            };
            if self.eat_contextual("get")? || self.eat_contextual("set")? {
                if self.is_class_element_name_start() {
                    kind = last_value;
                } else {
                    key_name = Some(last_value);
                }
            }
        }

        // The element name.
        let key = if let Some(name) = key_name {
            // `async`, `get`, `set` or `static` were not modifiers: the last token is the name.
            PropKey::Ident(Ident {
                name: Rc::from(name),
                loc: self.loc(self.last_tok_start, self.last_tok_start_loc, self.last_tok_end),
            })
        } else if self.ty == TT::PrivateId {
            if self.word_value() == Some("constructor") {
                return Err(self.raise(self.start, "Classes can't have an element named '#constructor'"));
            }
            PropKey::Private(self.parse_private_ident()?)
        } else {
            self.parse_property_name()?
        };
        let key_start = key.loc().start as usize;
        let computed = matches!(key, PropKey::Computed(_));

        // The element value.
        if self.ty == TT::ParenL || kind != "method" || is_generator || is_async {
            let is_constructor = !is_static && !computed && key_is(&key, "constructor");
            let allows_direct_super = is_constructor && constructor_allows_super;
            if is_constructor && kind != "method" {
                return Err(self.raise(key_start, "Constructor can't have get/set modifier"));
            }
            let kind = if is_constructor { "constructor" } else { kind };
            if kind == "constructor" {
                if is_generator {
                    return Err(self.raise(key_start, "Constructor can't be a generator"));
                }
                if is_async {
                    return Err(self.raise(key_start, "Constructor can't be an async method"));
                }
            } else if is_static && key_is(&key, "prototype") {
                return Err(self.raise(key_start, "Classes may not have a static property named prototype"));
            }
            let value = self.parse_method(is_generator, is_async, allows_direct_super)?;
            if kind == "get" && !value.params.is_empty() {
                return Err(self.raise(value.loc.start as usize, "getter should have no params"));
            }
            if kind == "set" && value.params.len() != 1 {
                return Err(self.raise(value.loc.start as usize, "setter should have exactly one param"));
            }
            if kind == "set"
                && let Pat::Rest { loc, .. } = &value.params[0]
            {
                return Err(self.raise(loc.start as usize, "Setter cannot use rest params"));
            }
            Ok(Some(ClassElement { is_method: true, kind, is_static, start, key: Some(key) }))
        } else {
            if key_is(&key, "constructor") {
                return Err(self.raise(key_start, "Classes can't have a field named 'constructor'"));
            } else if is_static && key_is(&key, "prototype") {
                return Err(self.raise(key_start, "Classes can't have a static field named 'prototype'"));
            }
            if self.eat(TT::Eq)? {
                // A separate scope, so that `arguments` in the initializer is an error.
                self.enter_scope(SCOPE_CLASS_FIELD_INIT | SCOPE_SUPER);
                self.parse_maybe_assign(InFor::No, None)?;
                self.exit_scope();
            }
            self.semicolon()?;
            Ok(Some(ClassElement { is_method: false, kind: "field", is_static, start, key: Some(key) }))
        }
    }

    fn parse_class_static_block(&mut self) -> R<()> {
        let old_labels = std::mem::take(&mut self.labels);
        self.enter_scope(SCOPE_CLASS_STATIC_BLOCK | SCOPE_SUPER);
        while self.ty != TT::BraceR {
            self.parse_statement(None, false)?;
        }
        self.next(false)?;
        self.exit_scope();
        self.labels = old_labels;
        Ok(())
    }
}

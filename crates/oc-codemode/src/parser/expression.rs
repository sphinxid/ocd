//! Expression parsing (acorn's `expression.js`).

use super::chars::has_line_break;
use super::node::{ArrowParam, InFor};
use super::state::{
    BIND_NONE, BIND_OUTSIDE, BIND_VAR, DestructuringErrors, Parser, Pos, R, SCOPE_ARROW, SCOPE_DIRECT_SUPER,
    SCOPE_SUPER, SCOPE_VAR, TokVal, function_flags,
};
use super::tokentype::{Ctx, TT};
use crate::ast::{
    AssignOp, BinaryOp, Block, Expr, ExprOrSpread, Function, FunctionBody, FunctionKind, Ident, Lit, LitValue,
    LogicalOp, Member, MemberProp, ObjectMember, Pat, PropKey, PropKind, Property, Stmt, Template, TemplateElement,
    UnaryOp, UpdateOp,
};
use std::collections::HashSet;
use std::rc::Rc;

/// Flags for [`Parser::parse_function`] (acorn's `FUNC_*`).
pub(super) const FUNC_STATEMENT: u8 = 1;
pub(super) const FUNC_HANGING_STATEMENT: u8 = 2;

/// The `__proto__` tracking of acorn's `propHash` (the only part used for `ecmaVersion >= 6`).
#[derive(Default)]
struct PropHash {
    proto: bool,
}

fn utf16_eq(s: &[u16], text: &str) -> bool {
    s.len() == text.len() && s.iter().zip(text.bytes()).all(|(&a, b)| a == b as u16)
}

fn binary_op(op: &str) -> BinaryOp {
    use BinaryOp::*;
    match op {
        "==" => Eq,
        "!=" => NotEq,
        "===" => StrictEq,
        "!==" => StrictNotEq,
        "<" => Lt,
        "<=" => LtEq,
        ">" => Gt,
        ">=" => GtEq,
        "<<" => Shl,
        ">>" => Shr,
        ">>>" => UShr,
        "+" => Add,
        "-" => Sub,
        "*" => Mul,
        "/" => Div,
        "%" => Mod,
        "**" => Exp,
        "|" => BitOr,
        "^" => BitXor,
        "&" => BitAnd,
        "in" => In,
        _ => InstanceOf,
    }
}

fn assign_op(op: &str) -> AssignOp {
    match op {
        "=" => AssignOp::Assign,
        "&&=" => AssignOp::Logical(LogicalOp::And),
        "||=" => AssignOp::Logical(LogicalOp::Or),
        "??=" => AssignOp::Logical(LogicalOp::Nullish),
        _ => AssignOp::Compound(binary_op(&op[..op.len() - 1])),
    }
}

fn unary_op(op: &str) -> UnaryOp {
    match op {
        "-" => UnaryOp::Minus,
        "+" => UnaryOp::Plus,
        "!" => UnaryOp::Not,
        "~" => UnaryOp::BitNot,
        "typeof" => UnaryOp::Typeof,
        "void" => UnaryOp::Void,
        _ => UnaryOp::Delete,
    }
}

fn is_arrow(expr: &Expr) -> bool {
    matches!(expr, Expr::Function(f) if f.kind == FunctionKind::Arrow)
}

fn is_private_field_access(expr: &Expr) -> bool {
    match expr {
        Expr::Member(m) => matches!(m.property, MemberProp::Private(_)),
        Expr::Chain { expression, .. } => is_private_field_access(expression),
        _ => false,
    }
}

fn is_simple_assign_target(expr: &Expr) -> bool {
    matches!(expr, Expr::Ident(_) | Expr::Member(_))
}

/// acorn's `checkKeyName`: a non-computed key that is the identifier or string `name`.
pub(super) fn key_is(key: &PropKey, name: &str) -> bool {
    match key {
        PropKey::Ident(id) => &*id.name == name,
        PropKey::Lit(Lit { value: LitValue::String(s), .. }) => utf16_eq(s, name),
        _ => false,
    }
}

impl Parser {
    /// The text of the current operator or keyword-operator token.
    fn op_value(&self) -> &str {
        match &self.value {
            TokVal::Op(op) => op,
            TokVal::Word(w) => w,
            _ => "",
        }
    }

    // ### Expression parsing

    pub(super) fn parse_expression(&mut self, for_init: InFor, mut refd: Option<&mut DestructuringErrors>) -> R<Expr> {
        let (start, start_loc) = (self.start, self.start_loc);
        let expr = self.parse_maybe_assign(for_init, refd.as_deref_mut())?;
        if self.ty == TT::Comma {
            let mut expressions = vec![expr];
            while self.eat(TT::Comma)? {
                expressions.push(self.parse_maybe_assign(for_init, refd.as_deref_mut())?);
            }
            return Ok(Expr::Sequence { expressions, loc: self.finish_loc(start, start_loc) });
        }
        Ok(expr)
    }

    pub(super) fn parse_maybe_assign(&mut self, for_init: InFor, refd: Option<&mut DestructuringErrors>) -> R<Expr> {
        self.enter_depth()?;
        let result = self.parse_maybe_assign_inner(for_init, refd);
        self.exit_depth();
        result
    }

    fn parse_maybe_assign_inner(&mut self, for_init: InFor, refd: Option<&mut DestructuringErrors>) -> R<Expr> {
        if self.is_contextual("yield") {
            if self.in_generator() {
                return self.parse_yield(for_init);
            }
            // The tokenizer assumed an expression is allowed after `yield`, but this is not that kind of yield.
            self.expr_allowed = false;
        }

        let mut own = DestructuringErrors::new();
        let own_destructuring_errors = refd.is_none();
        let (mut old_paren_assign, mut old_trailing_comma, mut old_double_proto) = (-1, -1, -1);
        let refd: &mut DestructuringErrors = match refd {
            Some(r) => {
                old_paren_assign = r.parenthesized_assign;
                old_trailing_comma = r.trailing_comma;
                old_double_proto = r.double_proto;
                r.parenthesized_assign = -1;
                r.trailing_comma = -1;
                r
            }
            None => &mut own,
        };

        let (start, start_loc) = (self.start, self.start_loc);
        if self.ty == TT::ParenL || self.ty == TT::Name {
            self.potential_arrow_at = self.start as i64;
            self.potential_arrow_in_for_await = for_init == InFor::Await;
        }
        let left = self.parse_maybe_conditional(for_init, Some(refd))?;
        if self.ty.is_assign() {
            let op = assign_op(self.op_value());
            let left_start = left.loc().start as i64;
            let is_eq = self.ty == TT::Eq;
            let left_pat = if is_eq {
                self.make_assignable(left, false, Some(refd))?
            } else {
                // Checked below by `check_lval_simple_expr`.
                match left {
                    Expr::Ident(id) => {
                        self.check_lval_ident(&id, BIND_NONE, None)?;
                        Pat::Ident(id)
                    }
                    Expr::Member(m) => Pat::Member(m),
                    other => {
                        self.check_lval_simple_expr(&other)?;
                        unreachable!("check_lval_simple_expr rejects every other expression")
                    }
                }
            };
            if !own_destructuring_errors {
                refd.parenthesized_assign = -1;
                refd.trailing_comma = -1;
                refd.double_proto = -1;
            }
            if refd.shorthand_assign >= left_start {
                // Reset because the shorthand default was used correctly.
                refd.shorthand_assign = -1;
            }
            if is_eq {
                self.check_lval_pattern(&left_pat, BIND_NONE, None)?;
            }
            self.next(false)?;
            let right = self.parse_maybe_assign(for_init, None)?;
            if old_double_proto > -1 {
                refd.double_proto = old_double_proto;
            }
            return Ok(Expr::Assign {
                op,
                left: Box::new(left_pat),
                right: Box::new(right),
                loc: self.finish_loc(start, start_loc),
            });
        } else if own_destructuring_errors {
            self.check_expression_errors(Some(refd))?;
        }
        if old_paren_assign > -1 {
            refd.parenthesized_assign = old_paren_assign;
        }
        if old_trailing_comma > -1 {
            refd.trailing_comma = old_trailing_comma;
        }
        Ok(left)
    }

    fn parse_maybe_conditional(&mut self, for_init: InFor, mut refd: Option<&mut DestructuringErrors>) -> R<Expr> {
        let (start, start_loc) = (self.start, self.start_loc);
        let expr = self.parse_expr_ops(for_init, refd.as_deref_mut())?;
        if self.has_expression_errors(refd.as_deref()) {
            return Ok(expr);
        }
        if self.eat(TT::Question)? {
            let consequent = self.parse_maybe_assign(InFor::No, None)?;
            self.expect(TT::Colon)?;
            let alternate = self.parse_maybe_assign(for_init, None)?;
            return Ok(Expr::Conditional {
                test: Box::new(expr),
                consequent: Box::new(consequent),
                alternate: Box::new(alternate),
                loc: self.finish_loc(start, start_loc),
            });
        }
        Ok(expr)
    }

    fn parse_expr_ops(&mut self, for_init: InFor, mut refd: Option<&mut DestructuringErrors>) -> R<Expr> {
        let (start, start_loc) = (self.start, self.start_loc);
        let expr = self.parse_maybe_unary(refd.as_deref_mut(), false, false, for_init)?;
        if self.has_expression_errors(refd.as_deref()) {
            return Ok(expr);
        }
        if expr.loc().start as usize == start && is_arrow(&expr) {
            return Ok(expr);
        }
        self.parse_expr_op(expr, start, start_loc, -1, for_init)
    }

    /// Operator-precedence parsing of binary operators.
    fn parse_expr_op(
        &mut self,
        left: Expr,
        left_start: usize,
        left_start_loc: Pos,
        min_prec: i32,
        for_init: InFor,
    ) -> R<Expr> {
        // acorn recurses once per operator; each link of the chain counts towards the nesting limit.
        let depth = self.depth;
        let result = self.parse_expr_op_chain(left, left_start, left_start_loc, min_prec, for_init);
        self.depth = depth;
        result
    }

    fn parse_expr_op_chain(
        &mut self,
        mut left: Expr,
        left_start: usize,
        left_start_loc: Pos,
        min_prec: i32,
        for_init: InFor,
    ) -> R<Expr> {
        loop {
            let Some(prec) = self.ty.binop() else { return Ok(left) };
            if for_init.is_set() && self.ty == TT::In {
                return Ok(left);
            }
            let mut prec = prec as i32;
            if prec <= min_prec {
                return Ok(left);
            }
            self.enter_depth()?;
            let logical = self.ty == TT::LogicalOr || self.ty == TT::LogicalAnd;
            let coalesce = self.ty == TT::Coalesce;
            if coalesce {
                // `??` binds like `&&` so that its right side cannot hold an unparenthesized logical expression.
                prec = TT::LogicalAnd.binop().unwrap_or(2) as i32;
            }
            let op = self.op_value().to_string();
            self.next(false)?;
            let (start, start_loc) = (self.start, self.start_loc);
            let operand = self.parse_maybe_unary(None, false, false, for_init)?;
            let right = self.parse_expr_op(operand, start, start_loc, prec, for_init)?;
            let node = self.build_binary(left_start, left_start_loc, left, right, &op, logical || coalesce)?;
            if (logical && self.ty == TT::Coalesce)
                || (coalesce && (self.ty == TT::LogicalOr || self.ty == TT::LogicalAnd))
            {
                return Err(self.raise(
                    self.start,
                    "Logical expressions and coalesce expressions cannot be mixed. Wrap either by parentheses",
                ));
            }
            left = node;
        }
    }

    fn build_binary(
        &mut self,
        start: usize,
        start_loc: Pos,
        left: Expr,
        right: Expr,
        op: &str,
        logical: bool,
    ) -> R<Expr> {
        if let Expr::PrivateName(id) = &right {
            return Err(
                self.raise(id.loc.start as usize, "Private identifier can only be left side of binary expression")
            );
        }
        let loc = self.finish_loc(start, start_loc);
        let (left, right) = (Box::new(left), Box::new(right));
        Ok(if logical {
            let op = match op {
                "||" => LogicalOp::Or,
                "&&" => LogicalOp::And,
                _ => LogicalOp::Nullish,
            };
            Expr::Logical { op, left, right, loc }
        } else {
            Expr::Binary { op: binary_op(op), left, right, loc }
        })
    }

    /// Parses unary operators, both prefix and postfix.
    pub(super) fn parse_maybe_unary(
        &mut self,
        refd: Option<&mut DestructuringErrors>,
        saw_unary: bool,
        inc_dec: bool,
        for_init: InFor,
    ) -> R<Expr> {
        self.enter_depth()?;
        let result = self.parse_maybe_unary_inner(refd, saw_unary, inc_dec, for_init);
        self.exit_depth();
        result
    }

    fn parse_maybe_unary_inner(
        &mut self,
        mut refd: Option<&mut DestructuringErrors>,
        mut saw_unary: bool,
        inc_dec: bool,
        for_init: InFor,
    ) -> R<Expr> {
        let (start, start_loc) = (self.start, self.start_loc);
        let mut expr;
        if self.is_contextual("await") && self.can_await() {
            expr = self.parse_await(for_init)?;
            saw_unary = true;
        } else if self.ty.prefix() {
            let update = self.ty == TT::IncDec;
            let op = self.op_value().to_string();
            self.next(false)?;
            let argument = self.parse_maybe_unary(None, true, update, for_init)?;
            self.check_expression_errors(refd.as_deref())?;
            if update {
                self.check_lval_simple_expr(&argument)?;
            } else if self.strict && op == "delete" && matches!(argument, Expr::Ident(_)) {
                return Err(self.raise(start, "Deleting local variable in strict mode"));
            } else if op == "delete" && is_private_field_access(&argument) {
                return Err(self.raise(start, "Private fields can not be deleted"));
            } else {
                saw_unary = true;
            }
            let loc = self.finish_loc(start, start_loc);
            expr = if update {
                let op = if op == "++" { UpdateOp::Increment } else { UpdateOp::Decrement };
                Expr::Update { op, prefix: true, argument: Box::new(argument), loc }
            } else {
                Expr::Unary { op: unary_op(&op), argument: Box::new(argument), loc }
            };
        } else if !saw_unary && self.ty == TT::PrivateId {
            if for_init.is_set() || self.private_name_stack.is_empty() {
                return Err(self.unexpected());
            }
            let id = self.parse_private_ident()?;
            // Only valid as the left side of `#x in obj`.
            if self.ty != TT::In {
                return Err(self.unexpected());
            }
            expr = Expr::PrivateName(id);
        } else {
            expr = self.parse_expr_subscripts(refd.as_deref_mut(), for_init)?;
            if self.has_expression_errors(refd.as_deref()) {
                return Ok(expr);
            }
            while self.ty.postfix() && !self.can_insert_semicolon() {
                self.check_lval_simple_expr(&expr)?;
                let op = if self.op_value() == "++" { UpdateOp::Increment } else { UpdateOp::Decrement };
                self.next(false)?;
                expr = Expr::Update {
                    op,
                    prefix: false,
                    argument: Box::new(expr),
                    loc: self.finish_loc(start, start_loc),
                };
            }
        }

        if !inc_dec && self.eat(TT::Starstar)? {
            if saw_unary {
                return Err(self.unexpected_at(self.last_tok_start));
            }
            let right = self.parse_maybe_unary(None, false, false, for_init)?;
            return self.build_binary(start, start_loc, expr, right, "**", false);
        }
        Ok(expr)
    }

    /// Parses call, dot, and `[]`-subscript expressions.
    pub(super) fn parse_expr_subscripts(
        &mut self,
        mut refd: Option<&mut DestructuringErrors>,
        for_init: InFor,
    ) -> R<Expr> {
        let (start, start_loc) = (self.start, self.start_loc);
        let expr = self.parse_expr_atom(refd.as_deref_mut(), for_init, false)?;
        let last_is_paren =
            self.last_tok_end == self.last_tok_start + 1 && self.input[self.last_tok_start] == b')' as u16;
        if is_arrow(&expr) && !last_is_paren {
            return Ok(expr);
        }
        let result = self.parse_subscripts(expr, start, start_loc, false, for_init)?;
        if let (Some(r), Expr::Member(m)) = (refd, &result) {
            let s = m.loc.start as i64;
            if r.parenthesized_assign >= s {
                r.parenthesized_assign = -1;
            }
            if r.parenthesized_bind >= s {
                r.parenthesized_bind = -1;
            }
            if r.trailing_comma >= s {
                r.trailing_comma = -1;
            }
        }
        Ok(result)
    }

    pub(super) fn parse_subscripts(
        &mut self,
        mut base: Expr,
        start: usize,
        start_loc: Pos,
        no_calls: bool,
        for_init: InFor,
    ) -> R<Expr> {
        let maybe_async_arrow = match &base {
            Expr::Ident(id) => {
                &*id.name == "async"
                    && self.last_tok_end == id.loc.end as usize
                    && !self.can_insert_semicolon()
                    && id.loc.end - id.loc.start == 5
                    && self.potential_arrow_at == id.loc.start as i64
            }
            _ => false,
        };
        let mut optional_chained = false;
        // Each link of the chain nests the AST one level deeper and counts towards the nesting limit.
        let depth = self.depth;
        loop {
            self.enter_depth()?;
            let (mut element, changed) =
                self.parse_subscript(base, start, start_loc, no_calls, maybe_async_arrow, optional_chained, for_init)?;
            let optional = match &element {
                Expr::Member(m) => m.optional,
                Expr::Call { optional, .. } => *optional,
                _ => false,
            };
            if optional {
                optional_chained = true;
            }
            if !changed || is_arrow(&element) {
                if optional_chained {
                    element = Expr::Chain { expression: Box::new(element), loc: self.finish_loc(start, start_loc) };
                }
                self.depth = depth;
                return Ok(element);
            }
            base = element;
        }
    }

    fn should_parse_async_arrow(&mut self) -> R<bool> {
        Ok(!self.can_insert_semicolon() && self.eat(TT::Arrow)?)
    }

    /// Parses one subscript; the flag tells whether anything was parsed (acorn's `element === base` test).
    #[allow(clippy::too_many_arguments)]
    fn parse_subscript(
        &mut self,
        base: Expr,
        start: usize,
        start_loc: Pos,
        no_calls: bool,
        maybe_async_arrow: bool,
        optional_chained: bool,
        for_init: InFor,
    ) -> R<(Expr, bool)> {
        let optional = self.eat(TT::QuestionDot)?;
        if no_calls && optional {
            return Err(
                self.raise(self.last_tok_start, "Optional chaining cannot appear in the callee of new expressions")
            );
        }
        let computed = self.eat(TT::BracketL)?;
        if computed || (optional && self.ty != TT::ParenL && self.ty != TT::BackQuote) || self.eat(TT::Dot)? {
            let property = if computed {
                let e = self.parse_expression(InFor::No, None)?;
                self.expect(TT::BracketR)?;
                MemberProp::Computed(Box::new(e))
            } else if self.ty == TT::PrivateId && !matches!(base, Expr::Super(_)) {
                MemberProp::Private(self.parse_private_ident()?)
            } else {
                MemberProp::Ident(self.parse_ident(true)?)
            };
            let loc = self.finish_loc(start, start_loc);
            Ok((Expr::Member(Member { object: Box::new(base), property, optional, loc }), true))
        } else if !no_calls && self.eat(TT::ParenL)? {
            let mut refd = DestructuringErrors::new();
            let (old_yield, old_await, old_await_ident) = (self.yield_pos, self.await_pos, self.await_ident_pos);
            self.yield_pos = 0;
            self.await_pos = 0;
            self.await_ident_pos = 0;
            let expr_list = self.parse_expr_list(TT::ParenR, true, false, Some(&mut refd))?;
            if maybe_async_arrow && !optional && self.should_parse_async_arrow()? {
                self.check_pattern_errors(Some(&refd), false)?;
                self.check_yield_await_in_default_params()?;
                if self.await_ident_pos > 0 {
                    return Err(
                        self.raise(self.await_ident_pos, "Cannot use 'await' as identifier inside an async function")
                    );
                }
                self.yield_pos = old_yield;
                self.await_pos = old_await;
                self.await_ident_pos = old_await_ident;
                let params = expr_list
                    .into_iter()
                    .flatten()
                    .map(|e| match e {
                        ExprOrSpread::Expr(e) => ArrowParam::Expr(e),
                        ExprOrSpread::Spread { argument, loc } => ArrowParam::Spread { argument, loc },
                    })
                    .collect();
                let arrow = self.parse_arrow_expression(start, start_loc, params, true, for_init)?;
                return Ok((arrow, true));
            }
            self.check_expression_errors(Some(&refd))?;
            if old_yield != 0 {
                self.yield_pos = old_yield;
            }
            if old_await != 0 {
                self.await_pos = old_await;
            }
            if old_await_ident != 0 {
                self.await_ident_pos = old_await_ident;
            }
            let arguments = expr_list.into_iter().flatten().collect();
            let loc = self.finish_loc(start, start_loc);
            Ok((Expr::Call { callee: Box::new(base), arguments, optional, loc }, true))
        } else if self.ty == TT::BackQuote {
            if optional || optional_chained {
                return Err(
                    self.raise(self.start, "Optional chaining cannot appear in the tag of tagged template expressions")
                );
            }
            let quasi = self.parse_template(true)?;
            let loc = self.finish_loc(start, start_loc);
            Ok((Expr::TaggedTemplate { tag: Box::new(base), quasi, loc }, true))
        } else {
            Ok((base, false))
        }
    }

    /// Parses an atomic expression.
    pub(super) fn parse_expr_atom(
        &mut self,
        refd: Option<&mut DestructuringErrors>,
        for_init: InFor,
        for_new: bool,
    ) -> R<Expr> {
        // A slash in expression position means the tokenizer read a division where a regexp starts.
        if self.ty == TT::Slash {
            self.read_regexp()?;
        }
        let can_be_arrow = self.potential_arrow_at == self.start as i64;
        let (start, start_loc) = (self.start, self.start_loc);
        match self.ty {
            TT::Super => {
                if !self.allow_super() {
                    return Err(self.raise(self.start, "'super' keyword outside a method"));
                }
                self.next(false)?;
                if self.ty == TT::ParenL && !self.allow_direct_super() {
                    return Err(self.raise(start, "super() call outside constructor of a subclass"));
                }
                if self.ty != TT::Dot && self.ty != TT::BracketL && self.ty != TT::ParenL {
                    return Err(self.unexpected());
                }
                Ok(Expr::Super(self.finish_loc(start, start_loc)))
            }
            TT::This => {
                self.next(false)?;
                Ok(Expr::This(self.finish_loc(start, start_loc)))
            }
            TT::Name => {
                let contains_esc = self.contains_esc;
                let id = self.parse_ident(false)?;
                if !contains_esc && &*id.name == "async" && !self.can_insert_semicolon() && self.eat(TT::Function)? {
                    self.override_context(Ctx::FExpr);
                    let f = self.parse_function(start, start_loc, 0, true, for_init)?;
                    return Ok(Expr::Function(f));
                }
                if can_be_arrow && !self.can_insert_semicolon() {
                    if self.eat(TT::Arrow)? {
                        return self.parse_arrow_expression(
                            start,
                            start_loc,
                            vec![ArrowParam::Expr(Expr::Ident(id))],
                            false,
                            for_init,
                        );
                    }
                    if &*id.name == "async"
                        && self.ty == TT::Name
                        && !contains_esc
                        && (!self.potential_arrow_in_for_await || self.word_value() != Some("of") || self.contains_esc)
                    {
                        let id = self.parse_ident(false)?;
                        if self.can_insert_semicolon() || !self.eat(TT::Arrow)? {
                            return Err(self.unexpected());
                        }
                        return self.parse_arrow_expression(
                            start,
                            start_loc,
                            vec![ArrowParam::Expr(Expr::Ident(id))],
                            true,
                            for_init,
                        );
                    }
                }
                Ok(Expr::Ident(id))
            }
            TT::Regexp | TT::Num | TT::String => Ok(Expr::Lit(self.parse_literal()?)),
            TT::Null | TT::True | TT::False => {
                let value = match self.ty {
                    TT::Null => LitValue::Null,
                    ty => LitValue::Bool(ty == TT::True),
                };
                self.next(false)?;
                Ok(Expr::Lit(Lit { value, loc: self.finish_loc(start, start_loc) }))
            }
            TT::ParenL => {
                let expr = self.parse_paren_and_distinguish_expression(can_be_arrow, for_init)?;
                if let Some(r) = refd {
                    if r.parenthesized_assign < 0 && !is_simple_assign_target(&expr) {
                        r.parenthesized_assign = start as i64;
                    }
                    if r.parenthesized_bind < 0 {
                        r.parenthesized_bind = start as i64;
                    }
                }
                Ok(expr)
            }
            TT::BracketL => {
                self.next(false)?;
                let elements = self.parse_expr_list(TT::BracketR, true, true, refd)?;
                Ok(Expr::Array { elements, loc: self.finish_loc(start, start_loc) })
            }
            TT::BraceL => {
                self.override_context(Ctx::BExpr);
                self.parse_obj_expr(refd)
            }
            TT::Function => {
                self.next(false)?;
                Ok(Expr::Function(self.parse_function(start, start_loc, 0, false, InFor::No)?))
            }
            TT::Class => {
                self.parse_class(false)?;
                Ok(Expr::Unsupported { kind: "ClassExpression", loc: self.finish_loc(start, start_loc) })
            }
            TT::New => self.parse_new(),
            TT::BackQuote => Ok(Expr::Template(self.parse_template(false)?)),
            TT::Import => self.parse_expr_import(for_new),
            _ => Err(self.unexpected()),
        }
    }

    fn parse_expr_import(&mut self, for_new: bool) -> R<Expr> {
        let (start, start_loc) = (self.start, self.start_loc);
        // `import` is consumed like an identifier, so escapes are checked here.
        if self.contains_esc {
            return Err(self.raise(self.start, "Escape sequence in keyword import"));
        }
        self.next(false)?;
        if self.ty == TT::ParenL && !for_new {
            // Dynamic import.
            self.next(false)?;
            self.parse_maybe_assign(InFor::No, None)?;
            if !self.eat(TT::ParenR)? {
                self.expect(TT::Comma)?;
                if !self.after_trailing_comma(TT::ParenR, false)? {
                    self.parse_maybe_assign(InFor::No, None)?;
                    if !self.eat(TT::ParenR)? {
                        self.expect(TT::Comma)?;
                        if !self.after_trailing_comma(TT::ParenR, false)? {
                            return Err(self.unexpected());
                        }
                    }
                }
            }
            Ok(Expr::Unsupported { kind: "ImportExpression", loc: self.finish_loc(start, start_loc) })
        } else if self.ty == TT::Dot {
            self.next(false)?;
            let contains_esc = self.contains_esc;
            let property = self.parse_ident(true)?;
            if &*property.name != "meta" {
                return Err(
                    self.raise(property.loc.start as usize, "The only valid meta property for import is 'import.meta'")
                );
            }
            if contains_esc {
                return Err(self.raise(start, "'import.meta' must not contain escaped characters"));
            }
            Err(self.raise(start, "Cannot use 'import.meta' outside a module"))
        } else {
            Err(self.unexpected())
        }
    }

    fn parse_literal(&mut self) -> R<Lit> {
        let value = match &self.value {
            TokVal::Num(n) => LitValue::Number(*n),
            TokVal::BigInt(b) => LitValue::BigInt(b.clone()),
            TokVal::Str(s) => LitValue::String(s.clone()),
            TokVal::Regex { pattern, flags } => LitValue::Regex { pattern: pattern.clone(), flags: flags.clone() },
            _ => LitValue::Null,
        };
        let (start, start_loc) = (self.start, self.start_loc);
        self.next(false)?;
        Ok(Lit { value, loc: self.finish_loc(start, start_loc) })
    }

    pub(super) fn parse_paren_expression(&mut self) -> R<Expr> {
        self.expect(TT::ParenL)?;
        let val = self.parse_expression(InFor::No, None)?;
        self.expect(TT::ParenR)?;
        Ok(val)
    }

    fn parse_paren_and_distinguish_expression(&mut self, can_be_arrow: bool, for_init: InFor) -> R<Expr> {
        let (start, start_loc) = (self.start, self.start_loc);
        self.next(false)?;
        let (inner_start, inner_start_loc) = (self.start, self.start_loc);
        let mut expr_list: Vec<ArrowParam> = Vec::new();
        let mut first = true;
        let mut last_is_comma = false;
        let mut refd = DestructuringErrors::new();
        let (old_yield, old_await) = (self.yield_pos, self.await_pos);
        let mut spread_start = None;
        self.yield_pos = 0;
        self.await_pos = 0;
        // awaitIdentPos is deliberately not saved, so awaits nested in parameters are still checked.
        while self.ty != TT::ParenR {
            if first {
                first = false;
            } else {
                self.expect(TT::Comma)?;
            }
            if self.after_trailing_comma(TT::ParenR, true)? {
                last_is_comma = true;
                break;
            } else if self.ty == TT::Ellipsis {
                spread_start = Some(self.start);
                expr_list.push(ArrowParam::Rest(self.parse_rest_binding()?));
                if self.ty == TT::Comma {
                    return Err(self.raise(self.start, "Comma is not permitted after the rest element"));
                }
                break;
            } else {
                expr_list.push(ArrowParam::Expr(self.parse_maybe_assign(InFor::No, Some(&mut refd))?));
            }
        }
        let inner_end = self.last_tok_end;
        self.expect(TT::ParenR)?;

        if can_be_arrow && !self.can_insert_semicolon() && self.eat(TT::Arrow)? {
            self.check_pattern_errors(Some(&refd), false)?;
            self.check_yield_await_in_default_params()?;
            self.yield_pos = old_yield;
            self.await_pos = old_await;
            return self.parse_arrow_expression(start, start_loc, expr_list, false, for_init);
        }

        if expr_list.is_empty() || last_is_comma {
            return Err(self.unexpected_at(self.last_tok_start));
        }
        if let Some(spread_start) = spread_start {
            return Err(self.unexpected_at(spread_start));
        }
        self.check_expression_errors(Some(&refd))?;
        if old_yield != 0 {
            self.yield_pos = old_yield;
        }
        if old_await != 0 {
            self.await_pos = old_await;
        }

        let mut exprs: Vec<Expr> = expr_list
            .into_iter()
            .map(|item| match item {
                ArrowParam::Expr(e) => e,
                // A rest element always failed above.
                ArrowParam::Spread { argument, .. } => argument,
                ArrowParam::Rest(_) => unreachable!("rest elements are rejected before"),
            })
            .collect();
        if exprs.len() > 1 {
            Ok(Expr::Sequence { expressions: exprs, loc: self.loc(inner_start, inner_start_loc, inner_end) })
        } else {
            Ok(exprs.pop().expect("one expression"))
        }
    }

    fn parse_new(&mut self) -> R<Expr> {
        if self.contains_esc {
            return Err(self.raise(self.start, "Escape sequence in keyword new"));
        }
        let (start, start_loc) = (self.start, self.start_loc);
        self.next(false)?;
        if self.ty == TT::Dot {
            self.next(false)?;
            let contains_esc = self.contains_esc;
            let property = self.parse_ident(true)?;
            if &*property.name != "target" {
                return Err(
                    self.raise(property.loc.start as usize, "The only valid meta property for new is 'new.target'")
                );
            }
            if contains_esc {
                return Err(self.raise(start, "'new.target' must not contain escaped characters"));
            }
            if !self.allow_new_dot_target() {
                return Err(self.raise(start, "'new.target' can only be used in functions and class static block"));
            }
            return Ok(Expr::Unsupported { kind: "MetaProperty", loc: self.finish_loc(start, start_loc) });
        }
        let (inner_start, inner_start_loc) = (self.start, self.start_loc);
        let atom = self.parse_expr_atom(None, InFor::No, true)?;
        let callee = self.parse_subscripts(atom, inner_start, inner_start_loc, true, InFor::No)?;
        let arguments = if self.eat(TT::ParenL)? {
            self.parse_expr_list(TT::ParenR, true, false, None)?.into_iter().flatten().collect()
        } else {
            Vec::new()
        };
        Ok(Expr::New { callee: Box::new(callee), arguments, loc: self.finish_loc(start, start_loc) })
    }

    fn parse_template_element(&mut self, is_tagged: bool) -> R<TemplateElement> {
        let (start, start_loc) = (self.start, self.start_loc);
        let (cooked, raw_src): (Option<Rc<[u16]>>, Rc<[u16]>) = match &self.value {
            TokVal::InvalidTemplate(raw) if self.ty == TT::InvalidTemplate => {
                if !is_tagged {
                    return Err(self.raise(self.start, "Bad escape sequence in untagged template literal"));
                }
                (None, raw.clone())
            }
            value => {
                let cooked = match value {
                    TokVal::Template(c) => c.clone(),
                    _ => Rc::from(Vec::new()),
                };
                (Some(cooked), Rc::from(&self.input[self.start..self.end]))
            }
        };
        // raw.replace(/\r\n?/g, "\n")
        let mut raw = Vec::with_capacity(raw_src.len());
        let mut i = 0;
        while i < raw_src.len() {
            if raw_src[i] == 13 {
                raw.push(10);
                if raw_src.get(i + 1) == Some(&10) {
                    i += 1;
                }
            } else {
                raw.push(raw_src[i]);
            }
            i += 1;
        }
        self.next(false)?;
        let tail = self.ty == TT::BackQuote;
        Ok(TemplateElement { cooked, raw: Rc::from(raw), tail, loc: self.finish_loc(start, start_loc) })
    }

    fn parse_template(&mut self, is_tagged: bool) -> R<Template> {
        let (start, start_loc) = (self.start, self.start_loc);
        self.next(false)?;
        let mut expressions = Vec::new();
        let mut cur = self.parse_template_element(is_tagged)?;
        let mut tail = cur.tail;
        let mut quasis = vec![cur];
        while !tail {
            if self.ty == TT::Eof {
                return Err(self.raise(self.pos, "Unterminated template literal"));
            }
            self.expect(TT::DollarBraceL)?;
            expressions.push(self.parse_expression(InFor::No, None)?);
            self.expect(TT::BraceR)?;
            cur = self.parse_template_element(is_tagged)?;
            tail = cur.tail;
            quasis.push(cur);
        }
        self.next(false)?;
        Ok(Template { quasis, expressions, loc: self.finish_loc(start, start_loc) })
    }

    fn is_async_prop(&self, key: &PropKey) -> bool {
        matches!(key, PropKey::Ident(id) if &*id.name == "async")
            && (matches!(self.ty, TT::Name | TT::Num | TT::String | TT::BracketL | TT::Star) || self.ty.is_keyword())
            && !has_line_break(&self.input, self.last_tok_end, self.start)
    }

    /// Parses an object literal.
    fn parse_obj_expr(&mut self, mut refd: Option<&mut DestructuringErrors>) -> R<Expr> {
        self.enter_depth()?;
        let (start, start_loc) = (self.start, self.start_loc);
        let mut first = true;
        let mut prop_hash = PropHash::default();
        let mut properties = Vec::new();
        self.next(false)?;
        while !self.eat(TT::BraceR)? {
            if !first {
                self.expect(TT::Comma)?;
                if self.after_trailing_comma(TT::BraceR, false)? {
                    break;
                }
            } else {
                first = false;
            }
            let prop = self.parse_property_expr(refd.as_deref_mut())?;
            self.check_prop_clash(&prop, &mut prop_hash, refd.as_deref_mut())?;
            properties.push(prop);
        }
        self.exit_depth();
        Ok(Expr::Object { properties, loc: self.finish_loc(start, start_loc) })
    }

    /// Parses an object binding pattern.
    pub(super) fn parse_obj_pattern(&mut self) -> R<Pat> {
        let (start, start_loc) = (self.start, self.start_loc);
        let mut first = true;
        let mut properties = Vec::new();
        self.next(false)?;
        while !self.eat(TT::BraceR)? {
            if !first {
                self.expect(TT::Comma)?;
                if self.after_trailing_comma(TT::BraceR, false)? {
                    break;
                }
            } else {
                first = false;
            }
            properties.push(self.parse_property_pattern()?);
        }
        Ok(Pat::Object { properties, loc: self.finish_loc(start, start_loc) })
    }

    fn parse_property_pattern(&mut self) -> R<crate::ast::ObjectPatMember> {
        use crate::ast::{ObjectPatMember, ObjectPatProp};
        let (start, start_loc) = (self.start, self.start_loc);
        if self.eat(TT::Ellipsis)? {
            let argument = self.parse_ident(false)?;
            if self.ty == TT::Comma {
                return Err(self.raise(self.start, "Comma is not permitted after the rest element"));
            }
            return Ok(ObjectPatMember::Rest {
                argument: Box::new(Pat::Ident(argument)),
                loc: self.finish_loc(start, start_loc),
            });
        }
        let key = self.parse_property_name()?;
        let (value, shorthand) = if self.eat(TT::Colon)? {
            let (s, sl) = (self.start, self.start_loc);
            (self.parse_maybe_default(s, sl, None)?, false)
        } else if self.ty == TT::ParenL {
            return Err(self.unexpected());
        } else if let PropKey::Ident(id) = &key {
            self.check_unreserved(id)?;
            if &*id.name == "await" && self.await_ident_pos == 0 {
                self.await_ident_pos = start;
            }
            (self.parse_maybe_default(start, start_loc, Some(Pat::Ident(id.clone())))?, true)
        } else {
            return Err(self.unexpected());
        };
        Ok(ObjectPatMember::Prop(ObjectPatProp { key, value, shorthand, loc: self.finish_loc(start, start_loc) }))
    }

    fn parse_property_expr(&mut self, refd: Option<&mut DestructuringErrors>) -> R<ObjectMember> {
        let (start, start_loc) = (self.start, self.start_loc);
        if self.eat(TT::Ellipsis)? {
            let mut refd = refd;
            let argument = self.parse_maybe_assign(InFor::No, refd.as_deref_mut())?;
            if self.ty == TT::Comma
                && let Some(r) = refd
                && r.trailing_comma < 0
            {
                r.trailing_comma = self.start as i64;
            }
            return Ok(ObjectMember::Spread { argument, loc: self.finish_loc(start, start_loc) });
        }
        // acorn only records the start when it may build an AssignmentPattern; otherwise it stays `undefined`
        // (which behaves like 0 where it leaks into `awaitIdentPos`).
        let shorthand_start = if refd.is_some() { start } else { 0 };
        let mut is_generator = self.eat(TT::Star)?;
        let contains_esc = self.contains_esc;
        let mut key = self.parse_property_name()?;
        let is_async = if !contains_esc && !is_generator && self.is_async_prop(&key) {
            is_generator = self.eat(TT::Star)?;
            key = self.parse_property_name()?;
            true
        } else {
            false
        };

        if (is_generator || is_async) && self.ty == TT::Colon {
            return Err(self.unexpected());
        }
        let mut method = false;
        let mut shorthand = false;
        let mut kind = PropKind::Init;
        let value;
        if self.eat(TT::Colon)? {
            value = self.parse_maybe_assign(InFor::No, refd)?;
        } else if self.ty == TT::ParenL {
            method = true;
            value = Expr::Function(self.parse_method(is_generator, is_async, false)?);
        } else if let Some(accessor) = match &key {
            PropKey::Ident(id)
                if !contains_esc
                    && (&*id.name == "get" || &*id.name == "set")
                    && !matches!(self.ty, TT::Comma | TT::BraceR | TT::Eq) =>
            {
                Some(if &*id.name == "get" { PropKind::Get } else { PropKind::Set })
            }
            _ => None,
        } {
            if is_generator || is_async {
                return Err(self.unexpected());
            }
            key = self.parse_property_name()?;
            let f = self.parse_method(false, false, false)?;
            let param_count = if accessor == PropKind::Get { 0 } else { 1 };
            if f.params.len() != param_count {
                let msg = if accessor == PropKind::Get {
                    "getter should have no params"
                } else {
                    "setter should have exactly one param"
                };
                return Err(self.raise(f.loc.start as usize, msg));
            } else if accessor == PropKind::Set
                && let Pat::Rest { loc, .. } = &f.params[0]
            {
                return Err(self.raise(loc.start as usize, "Setter cannot use rest params"));
            }
            kind = accessor;
            value = Expr::Function(f);
        } else if let PropKey::Ident(id) = &key {
            if is_generator || is_async {
                return Err(self.unexpected());
            }
            let id = id.clone();
            self.check_unreserved(&id)?;
            if &*id.name == "await" && self.await_ident_pos == 0 {
                self.await_ident_pos = shorthand_start;
            }
            match refd {
                Some(r) if self.ty == TT::Eq => {
                    if r.shorthand_assign < 0 {
                        r.shorthand_assign = self.start as i64;
                    }
                    self.next(false)?;
                    let right = self.parse_maybe_assign(InFor::No, None)?;
                    // Stands for acorn's AssignmentPattern; always rejected unless the object becomes a pattern.
                    value = Expr::Assign {
                        op: AssignOp::Assign,
                        left: Box::new(Pat::Ident(id)),
                        right: Box::new(right),
                        loc: self.finish_loc(start, start_loc),
                    };
                }
                _ => value = Expr::Ident(id),
            }
            shorthand = true;
        } else {
            return Err(self.unexpected());
        }
        Ok(ObjectMember::Prop(Property { key, value, kind, method, shorthand, loc: self.finish_loc(start, start_loc) }))
    }

    /// acorn's `checkPropClash` (only `__proto__` duplicates matter since ES2015).
    fn check_prop_clash(
        &mut self,
        prop: &ObjectMember,
        prop_hash: &mut PropHash,
        refd: Option<&mut DestructuringErrors>,
    ) -> R<()> {
        let ObjectMember::Prop(prop) = prop else { return Ok(()) };
        if prop.method || prop.shorthand {
            return Ok(());
        }
        let is_proto = match &prop.key {
            PropKey::Ident(id) => &*id.name == "__proto__",
            PropKey::Lit(Lit { value: LitValue::String(s), .. }) => utf16_eq(s, "__proto__"),
            _ => false,
        };
        if is_proto && prop.kind == PropKind::Init {
            if prop_hash.proto {
                let key_start = prop.key.loc().start as usize;
                match refd {
                    Some(r) => {
                        if r.double_proto < 0 {
                            r.double_proto = key_start as i64;
                        }
                    }
                    None => return Err(self.raise(key_start, "Redefinition of __proto__ property")),
                }
            }
            prop_hash.proto = true;
        }
        Ok(())
    }

    /// Parses a property name: an identifier (keywords allowed), a string or number literal, or `[expr]`.
    pub(super) fn parse_property_name(&mut self) -> R<PropKey> {
        if self.eat(TT::BracketL)? {
            let key = self.parse_maybe_assign(InFor::No, None)?;
            self.expect(TT::BracketR)?;
            return Ok(PropKey::Computed(Box::new(key)));
        }
        if self.ty == TT::Num || self.ty == TT::String {
            return Ok(PropKey::Lit(self.parse_literal()?));
        }
        Ok(PropKey::Ident(self.parse_ident(true)?))
    }

    /// Parses an object or class method (from its parameter list).
    pub(super) fn parse_method(
        &mut self,
        is_generator: bool,
        is_async: bool,
        allow_direct_super: bool,
    ) -> R<Rc<Function>> {
        let (start, start_loc) = (self.start, self.start_loc);
        let (old_yield, old_await, old_await_ident) = (self.yield_pos, self.await_pos, self.await_ident_pos);
        self.yield_pos = 0;
        self.await_pos = 0;
        self.await_ident_pos = 0;
        let flags = function_flags(is_async, is_generator)
            | SCOPE_SUPER
            | if allow_direct_super { SCOPE_DIRECT_SUPER } else { 0 };
        self.enter_scope(flags);
        self.expect(TT::ParenL)?;
        let params = self.parse_params()?;
        self.check_yield_await_in_default_params()?;
        let body = self.parse_function_body(&params, None, start, false, true, InFor::No)?;
        self.yield_pos = old_yield;
        self.await_pos = old_await;
        self.await_ident_pos = old_await_ident;
        Ok(Rc::new(Function {
            kind: FunctionKind::Expression,
            id: None,
            params,
            body,
            is_async,
            is_generator,
            loc: self.finish_loc(start, start_loc),
        }))
    }

    /// Parses a function declaration or expression (after the `function` keyword).
    pub(super) fn parse_function(
        &mut self,
        start: usize,
        start_loc: Pos,
        statement: u8,
        is_async: bool,
        for_init: InFor,
    ) -> R<Rc<Function>> {
        if self.ty == TT::Star && statement & FUNC_HANGING_STATEMENT != 0 {
            return Err(self.unexpected());
        }
        let generator = self.eat(TT::Star)?;
        let mut id = None;
        if statement & FUNC_STATEMENT != 0 {
            let ident = self.parse_ident(false)?;
            if statement & FUNC_HANGING_STATEMENT == 0 {
                // Sloppy-mode plain function declarations follow Annex B (BIND_FUNCTION).
                let binding = if self.strict || generator || is_async {
                    if self.treat_functions_as_var() { BIND_VAR } else { super::state::BIND_LEXICAL }
                } else {
                    super::state::BIND_FUNCTION
                };
                self.check_lval_ident(&ident, binding, None)?;
            }
            id = Some(ident);
        }
        let (old_yield, old_await, old_await_ident) = (self.yield_pos, self.await_pos, self.await_ident_pos);
        self.yield_pos = 0;
        self.await_pos = 0;
        self.await_ident_pos = 0;
        self.enter_scope(function_flags(is_async, generator));
        if statement & FUNC_STATEMENT == 0 {
            id = if self.ty == TT::Name { Some(self.parse_ident(false)?) } else { None };
        }
        self.expect(TT::ParenL)?;
        let params = self.parse_params()?;
        self.check_yield_await_in_default_params()?;
        let body = self.parse_function_body(&params, id.as_ref(), start, false, false, for_init)?;
        self.yield_pos = old_yield;
        self.await_pos = old_await;
        self.await_ident_pos = old_await_ident;
        let kind = if statement & FUNC_STATEMENT != 0 { FunctionKind::Declaration } else { FunctionKind::Expression };
        Ok(Rc::new(Function {
            kind,
            id,
            params,
            body,
            is_async,
            is_generator: generator,
            loc: self.finish_loc(start, start_loc),
        }))
    }

    /// Parses an arrow function's body for the given (not yet converted) parameters.
    pub(super) fn parse_arrow_expression(
        &mut self,
        start: usize,
        start_loc: Pos,
        params: Vec<ArrowParam>,
        is_async: bool,
        for_init: InFor,
    ) -> R<Expr> {
        let (old_yield, old_await, old_await_ident) = (self.yield_pos, self.await_pos, self.await_ident_pos);
        self.enter_scope(function_flags(is_async, false) | SCOPE_ARROW);
        self.yield_pos = 0;
        self.await_pos = 0;
        self.await_ident_pos = 0;
        let params = self.assignable_params(params)?;
        let body = self.parse_function_body(&params, None, start, true, false, for_init)?;
        self.yield_pos = old_yield;
        self.await_pos = old_await;
        self.await_ident_pos = old_await_ident;
        Ok(Expr::Function(Rc::new(Function {
            kind: FunctionKind::Arrow,
            id: None,
            params,
            body,
            is_async,
            is_generator: false,
            loc: self.finish_loc(start, start_loc),
        })))
    }

    /// Parses a function body and checks the parameters; exits the function scope.
    fn parse_function_body(
        &mut self,
        params: &[Pat],
        id: Option<&Ident>,
        fn_start: usize,
        is_arrow_function: bool,
        is_method: bool,
        for_init: InFor,
    ) -> R<FunctionBody> {
        let is_expression = is_arrow_function && self.ty != TT::BraceL;
        let old_strict = self.strict;
        let mut use_strict = false;

        let body = if is_expression {
            let expr = self.parse_maybe_assign(for_init, None)?;
            self.check_params(params, false)?;
            FunctionBody::Expr(Box::new(expr))
        } else {
            let non_simple = !is_simple_param_list(params);
            if !old_strict || non_simple {
                use_strict = self.strict_directive(self.end);
                if use_strict && non_simple {
                    return Err(self
                        .raise(fn_start, "Illegal 'use strict' directive in function with non-simple parameter list"));
                }
            }
            let old_labels = std::mem::take(&mut self.labels);
            if use_strict {
                self.strict = true;
            }
            let allow_duplicates =
                !old_strict && !use_strict && !is_arrow_function && !is_method && is_simple_param_list(params);
            self.check_params(params, allow_duplicates)?;
            if self.strict
                && let Some(id) = id
            {
                self.check_lval_ident(id, BIND_OUTSIDE, None)?;
            }
            let mut block: Block = self.parse_block(false, use_strict && !old_strict)?;
            self.adapt_directive_prologue(&mut block.body);
            self.labels = old_labels;
            FunctionBody::Block(block)
        };
        self.exit_scope();
        Ok(body)
    }

    fn check_params(&mut self, params: &[Pat], allow_duplicates: bool) -> R<()> {
        let mut name_hash: HashSet<Rc<str>> = HashSet::new();
        for param in params {
            let clashes = if allow_duplicates { None } else { Some(&mut name_hash) };
            self.check_lval_inner_pattern(param, BIND_VAR, clashes)?;
        }
        Ok(())
    }

    /// Parses a comma-separated expression list up to `close` (which it consumes).
    fn parse_expr_list(
        &mut self,
        close: TT,
        allow_trailing_comma: bool,
        allow_empty: bool,
        mut refd: Option<&mut DestructuringErrors>,
    ) -> R<Vec<Option<ExprOrSpread>>> {
        let mut elts = Vec::new();
        let mut first = true;
        while !self.eat(close)? {
            if !first {
                self.expect(TT::Comma)?;
                if allow_trailing_comma && self.after_trailing_comma(close, false)? {
                    break;
                }
            } else {
                first = false;
            }
            let elt = if allow_empty && self.ty == TT::Comma {
                None
            } else if self.ty == TT::Ellipsis {
                let (start, start_loc) = (self.start, self.start_loc);
                self.next(false)?;
                let argument = self.parse_maybe_assign(InFor::No, refd.as_deref_mut())?;
                let spread = ExprOrSpread::Spread { argument, loc: self.finish_loc(start, start_loc) };
                if let Some(r) = refd.as_deref_mut()
                    && self.ty == TT::Comma
                    && r.trailing_comma < 0
                {
                    r.trailing_comma = self.start as i64;
                }
                Some(spread)
            } else {
                Some(ExprOrSpread::Expr(self.parse_maybe_assign(InFor::No, refd.as_deref_mut())?))
            };
            elts.push(elt);
        }
        Ok(elts)
    }

    /// acorn's `checkUnreserved`.
    pub(super) fn check_unreserved(&self, id: &Ident) -> R<()> {
        let start = id.loc.start as usize;
        let name: &str = &id.name;
        if self.in_generator() && name == "yield" {
            return Err(self.raise(start, "Cannot use 'yield' as identifier inside a generator"));
        }
        if self.in_async() && name == "await" {
            return Err(self.raise(start, "Cannot use 'await' as identifier inside an async function"));
        }
        if self.current_this_scope().flags & SCOPE_VAR == 0 && name == "arguments" {
            return Err(self.raise(start, "Cannot use 'arguments' in class field initializer"));
        }
        if self.in_class_static_block() && (name == "arguments" || name == "await") {
            return Err(self.raise(start, &format!("Cannot use {name} in class static initialization block")));
        }
        if TT::from_keyword(name).is_some() {
            return Err(self.raise(start, &format!("Unexpected keyword '{name}'")));
        }
        let reserved = if self.strict {
            matches!(
                name,
                "enum"
                    | "implements"
                    | "interface"
                    | "let"
                    | "package"
                    | "private"
                    | "protected"
                    | "public"
                    | "static"
                    | "yield"
            )
        } else {
            name == "enum"
        };
        if reserved {
            if !self.in_async() && name == "await" {
                return Err(self.raise(start, "Cannot use keyword 'await' outside an async function"));
            }
            return Err(self.raise(start, &format!("The keyword '{name}' is reserved")));
        }
        Ok(())
    }

    /// Parses an identifier; with `liberal`, keywords are accepted as names (property names).
    pub(super) fn parse_ident(&mut self, liberal: bool) -> R<Ident> {
        let (start, start_loc) = (self.start, self.start_loc);
        let name = self.parse_ident_node()?;
        self.next(liberal)?;
        let id = Ident { name, loc: self.finish_loc(start, start_loc) };
        if !liberal {
            self.check_unreserved(&id)?;
            if &*id.name == "await" && self.await_ident_pos == 0 {
                self.await_ident_pos = start;
            }
        }
        Ok(id)
    }

    fn parse_ident_node(&mut self) -> R<Rc<str>> {
        if self.ty == TT::Name {
            return Ok(self.word_rc());
        }
        let Some(keyword) = self.ty.keyword() else { return Err(self.unexpected()) };
        // `class` and `function` pushed a context that is never popped when the keyword is used as a name
        // (unless the previous token was a dot, whose context handling ignored the keyword).
        if (keyword == "class" || keyword == "function")
            && (self.last_tok_end != self.last_tok_start + 1 || self.input[self.last_tok_start] != b'.' as u16)
        {
            self.context.pop();
        }
        self.ty = TT::Name;
        Ok(Rc::from(keyword))
    }

    pub(super) fn parse_private_ident(&mut self) -> R<Ident> {
        let (start, start_loc) = (self.start, self.start_loc);
        if self.ty != TT::PrivateId {
            return Err(self.unexpected());
        }
        let name = self.word_rc();
        self.next(false)?;
        let id = Ident { name, loc: self.finish_loc(start, start_loc) };
        match self.private_name_stack.last_mut() {
            None => {
                return Err(
                    self.raise(start, &format!("Private field '#{}' must be declared in an enclosing class", id.name))
                );
            }
            Some(scope) => scope.used.push((id.name.clone(), start)),
        }
        Ok(id)
    }

    fn parse_yield(&mut self, for_init: InFor) -> R<Expr> {
        if self.yield_pos == 0 {
            self.yield_pos = self.start;
        }
        let (start, start_loc) = (self.start, self.start_loc);
        self.next(false)?;
        let (delegate, argument) =
            if self.ty == TT::Semi || self.can_insert_semicolon() || (self.ty != TT::Star && !self.ty.starts_expr()) {
                (false, None)
            } else {
                let delegate = self.eat(TT::Star)?;
                (delegate, Some(Box::new(self.parse_maybe_assign(for_init, None)?)))
            };
        Ok(Expr::Yield { argument, delegate, loc: self.finish_loc(start, start_loc) })
    }

    fn parse_await(&mut self, for_init: InFor) -> R<Expr> {
        if self.await_pos == 0 {
            self.await_pos = self.start;
        }
        let (start, start_loc) = (self.start, self.start_loc);
        self.next(false)?;
        let argument = self.parse_maybe_unary(None, true, false, for_init)?;
        Ok(Expr::Await { argument: Box::new(argument), loc: self.finish_loc(start, start_loc) })
    }

    /// Sets the `directive` of the leading string-literal expression statements.
    pub(super) fn adapt_directive_prologue(&self, statements: &mut [Stmt]) {
        for stmt in statements {
            let Stmt::Expr { expr: Expr::Lit(Lit { value: LitValue::String(_), loc: lit_loc }), directive, loc } = stmt
            else {
                break;
            };
            let first = self.input[loc.start as usize];
            // Parenthesized strings are not directives.
            if first != b'"' as u16 && first != b'\'' as u16 {
                break;
            }
            let raw = &self.input[lit_loc.start as usize + 1..lit_loc.end as usize - 1];
            *directive = Some(Rc::from(String::from_utf16_lossy(raw)));
        }
    }
}

fn is_simple_param_list(params: &[Pat]) -> bool {
    params.iter().all(|p| matches!(p, Pat::Ident(_)))
}

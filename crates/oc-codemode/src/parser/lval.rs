//! Patterns and lvalue checks (acorn's `lval.js`).

use super::node::ArrowParam;
use super::state::{BIND_LEXICAL, BIND_NONE, BIND_OUTSIDE, DestructuringErrors, Parser, R};
use super::tokentype::TT;
use crate::ast::{Expr, ExprOrSpread, Ident, ObjectMember, ObjectPatMember, ObjectPatProp, Pat, PropKind};
use std::collections::HashSet;
use std::rc::Rc;

/// acorn's `reservedWordsStrictBind` (for `ecmaVersion >= 6`, script).
fn is_reserved_strict_bind(name: &str) -> bool {
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
            | "eval"
            | "arguments"
    )
}

pub(super) type Clashes<'a> = Option<&'a mut HashSet<Rc<str>>>;

impl Parser {
    /// acorn's `toAssignable(node, isBinding, refDestructuringErrors)` on an expression.
    pub(super) fn make_assignable(
        &mut self,
        expr: Expr,
        is_binding: bool,
        refd: Option<&DestructuringErrors>,
    ) -> R<Pat> {
        match expr {
            Expr::Ident(id) => {
                if self.in_async() && &*id.name == "await" {
                    return Err(
                        self.raise(id.loc.start as usize, "Cannot use 'await' as identifier inside an async function")
                    );
                }
                Ok(Pat::Ident(id))
            }
            Expr::Object { properties, loc } => {
                if refd.is_some() {
                    self.check_pattern_errors(refd, true)?;
                }
                let mut out = Vec::with_capacity(properties.len());
                for member in properties {
                    match member {
                        ObjectMember::Prop(prop) => {
                            if prop.kind != PropKind::Init {
                                return Err(self.raise(
                                    prop.key.loc().start as usize,
                                    "Object pattern can't contain getter or setter",
                                ));
                            }
                            let value = match prop.value {
                                // A shorthand property with a default (`{a = 1}`) already is an AssignmentPattern.
                                Expr::Assign { left, right, loc, .. } if prop.shorthand => {
                                    Pat::Assign { left, right, loc }
                                }
                                value => self.make_assignable(value, is_binding, None)?,
                            };
                            out.push(ObjectPatMember::Prop(ObjectPatProp {
                                key: prop.key,
                                value,
                                shorthand: prop.shorthand,
                                loc: prop.loc,
                            }));
                        }
                        ObjectMember::Spread { argument, loc } => {
                            let argument = self.spread_to_rest_argument(argument, is_binding)?;
                            if matches!(argument, Pat::Array { .. } | Pat::Object { .. }) {
                                return Err(self.unexpected_at(argument.loc().start as usize));
                            }
                            out.push(ObjectPatMember::Rest { argument: Box::new(argument), loc });
                        }
                    }
                }
                Ok(Pat::Object { properties: out, loc })
            }
            Expr::Array { elements, loc } => {
                if refd.is_some() {
                    self.check_pattern_errors(refd, true)?;
                }
                let mut out = Vec::with_capacity(elements.len());
                for elt in elements {
                    out.push(match elt {
                        None => None,
                        Some(ExprOrSpread::Expr(e)) => Some(self.make_assignable(e, is_binding, None)?),
                        Some(ExprOrSpread::Spread { argument, loc }) => {
                            let argument = self.spread_to_rest_argument(argument, is_binding)?;
                            Some(Pat::Rest { argument: Box::new(argument), loc })
                        }
                    });
                }
                Ok(Pat::Array { elements: out, loc })
            }
            Expr::Assign { op, left, right, loc } => {
                if op != crate::ast::AssignOp::Assign {
                    return Err(self.raise(
                        left.loc().end as usize,
                        "Only '=' operator can be used for specifying default value.",
                    ));
                }
                self.recheck_assignable(&left, is_binding)?;
                Ok(Pat::Assign { left, right, loc })
            }
            Expr::Chain { loc, .. } => {
                Err(self.raise(loc.start as usize, "Optional chaining cannot appear in left-hand side"))
            }
            Expr::Member(m) if !is_binding => Ok(Pat::Member(m)),
            other => Err(self.raise(other.loc().start as usize, "Assigning to rvalue")),
        }
    }

    /// `SpreadElement` → `RestElement`: converts the argument and rejects defaults.
    fn spread_to_rest_argument(&mut self, argument: Expr, is_binding: bool) -> R<Pat> {
        let argument = self.make_assignable(argument, is_binding, None)?;
        if let Pat::Assign { loc, .. } = &argument {
            return Err(self.raise(loc.start as usize, "Rest elements cannot have a default value"));
        }
        Ok(argument)
    }

    /// `toAssignable` applied to a node that already is a pattern (the left side of an assignment that becomes a
    /// default value).
    fn recheck_assignable(&self, pat: &Pat, is_binding: bool) -> R<()> {
        match pat {
            Pat::Ident(id) => {
                if self.in_async() && &*id.name == "await" {
                    return Err(
                        self.raise(id.loc.start as usize, "Cannot use 'await' as identifier inside an async function")
                    );
                }
                Ok(())
            }
            Pat::Member(m) if is_binding => Err(self.raise(m.loc.start as usize, "Assigning to rvalue")),
            _ => Ok(()),
        }
    }

    /// acorn's `toAssignableList(params, true)` for arrow function parameters.
    pub(super) fn assignable_params(&mut self, params: Vec<ArrowParam>) -> R<Vec<Pat>> {
        let mut out = Vec::with_capacity(params.len());
        for param in params {
            out.push(match param {
                ArrowParam::Expr(e) => self.make_assignable(e, true, None)?,
                ArrowParam::Spread { argument, loc } => {
                    let argument = self.spread_to_rest_argument(argument, true)?;
                    Pat::Rest { argument: Box::new(argument), loc }
                }
                ArrowParam::Rest(p) => p,
            });
        }
        Ok(out)
    }

    /// Parses a rest binding: `...pattern`.
    pub(super) fn parse_rest_binding(&mut self) -> R<Pat> {
        let (start, start_loc) = (self.start, self.start_loc);
        self.next(false)?;
        let argument = self.parse_binding_atom()?;
        Ok(Pat::Rest { argument: Box::new(argument), loc: self.finish_loc(start, start_loc) })
    }

    /// Parses a binding identifier or destructuring pattern.
    pub(super) fn parse_binding_atom(&mut self) -> R<Pat> {
        match self.ty {
            TT::BracketL => {
                self.enter_depth()?;
                let (start, start_loc) = (self.start, self.start_loc);
                self.next(false)?;
                let elements = self.parse_binding_list(TT::BracketR, true, true)?;
                self.exit_depth();
                Ok(Pat::Array { elements, loc: self.finish_loc(start, start_loc) })
            }
            TT::BraceL => {
                self.enter_depth()?;
                let pat = self.parse_obj_pattern()?;
                self.exit_depth();
                Ok(pat)
            }
            _ => Ok(Pat::Ident(self.parse_ident(false)?)),
        }
    }

    pub(super) fn parse_binding_list(
        &mut self,
        close: TT,
        allow_empty: bool,
        allow_trailing_comma: bool,
    ) -> R<Vec<Option<Pat>>> {
        let mut elts = Vec::new();
        let mut first = true;
        while !self.eat(close)? {
            if first {
                first = false;
            } else {
                self.expect(TT::Comma)?;
            }
            if allow_empty && self.ty == TT::Comma {
                elts.push(None);
            } else if allow_trailing_comma && self.after_trailing_comma(close, false)? {
                break;
            } else if self.ty == TT::Ellipsis {
                let rest = self.parse_rest_binding()?;
                elts.push(Some(rest));
                if self.ty == TT::Comma {
                    return Err(self.raise(self.start, "Comma is not permitted after the rest element"));
                }
                self.expect(close)?;
                break;
            } else {
                let (start, start_loc) = (self.start, self.start_loc);
                elts.push(Some(self.parse_maybe_default(start, start_loc, None)?));
            }
        }
        Ok(elts)
    }

    /// Parses function parameters (after the opening parenthesis).
    pub(super) fn parse_params(&mut self) -> R<Vec<Pat>> {
        Ok(self.parse_binding_list(TT::ParenR, false, true)?.into_iter().flatten().collect())
    }

    /// Parses an assignment pattern around `left` (or a freshly parsed binding atom) if `=` follows.
    pub(super) fn parse_maybe_default(
        &mut self,
        start: usize,
        start_loc: super::state::Pos,
        left: Option<Pat>,
    ) -> R<Pat> {
        let left = match left {
            Some(l) => l,
            None => self.parse_binding_atom()?,
        };
        if !self.eat(TT::Eq)? {
            return Ok(left);
        }
        let right = self.parse_maybe_assign(super::node::InFor::No, None)?;
        Ok(Pat::Assign { left: Box::new(left), right: Box::new(right), loc: self.finish_loc(start, start_loc) })
    }

    /// acorn's `checkLValSimple` on an identifier.
    pub(super) fn check_lval_ident(&mut self, id: &Ident, binding_type: u8, clashes: Clashes<'_>) -> R<()> {
        let is_bind = binding_type != BIND_NONE;
        let start = id.loc.start as usize;
        if self.strict && is_reserved_strict_bind(&id.name) {
            let prefix = if is_bind { "Binding " } else { "Assigning to " };
            return Err(self.raise(start, &format!("{prefix}{} in strict mode", id.name)));
        }
        if is_bind {
            if binding_type == BIND_LEXICAL && &*id.name == "let" {
                return Err(self.raise(start, "let is disallowed as a lexically bound name"));
            }
            if let Some(clashes) = clashes {
                if clashes.contains(&id.name) {
                    return Err(self.raise(start, "Argument name clash"));
                }
                clashes.insert(id.name.clone());
            }
            if binding_type != BIND_OUTSIDE {
                self.declare_name(&id.name, binding_type, start)?;
            }
        }
        Ok(())
    }

    /// acorn's `checkLValSimple` on a pattern.
    pub(super) fn check_lval_simple(&mut self, pat: &Pat, binding_type: u8, clashes: Clashes<'_>) -> R<()> {
        let is_bind = binding_type != BIND_NONE;
        match pat {
            Pat::Ident(id) => self.check_lval_ident(id, binding_type, clashes),
            Pat::Member(m) => {
                if is_bind {
                    return Err(self.raise(m.loc.start as usize, "Binding member expression"));
                }
                Ok(())
            }
            other => {
                let msg = if is_bind { "Binding rvalue" } else { "Assigning to rvalue" };
                Err(self.raise(other.loc().start as usize, msg))
            }
        }
    }

    /// acorn's `checkLValSimple(expr)` (no binding) on an expression: the target of `++`/`--` or a compound
    /// assignment.
    pub(super) fn check_lval_simple_expr(&mut self, expr: &Expr) -> R<()> {
        match expr {
            Expr::Ident(id) => self.check_lval_ident(id, BIND_NONE, None),
            Expr::Chain { loc, .. } => {
                Err(self.raise(loc.start as usize, "Optional chaining cannot appear in left-hand side"))
            }
            Expr::Member(_) => Ok(()),
            other => Err(self.raise(other.loc().start as usize, "Assigning to rvalue")),
        }
    }

    /// acorn's `checkLValPattern`.
    pub(super) fn check_lval_pattern(&mut self, pat: &Pat, binding_type: u8, mut clashes: Clashes<'_>) -> R<()> {
        match pat {
            Pat::Object { properties, .. } => {
                for prop in properties {
                    match prop {
                        ObjectPatMember::Prop(p) => {
                            self.check_lval_inner_pattern(&p.value, binding_type, clashes.as_deref_mut())?
                        }
                        ObjectPatMember::Rest { argument, .. } => {
                            self.check_lval_pattern(argument, binding_type, clashes.as_deref_mut())?
                        }
                    }
                }
                Ok(())
            }
            Pat::Array { elements, .. } => {
                for elem in elements.iter().flatten() {
                    self.check_lval_inner_pattern(elem, binding_type, clashes.as_deref_mut())?;
                }
                Ok(())
            }
            other => self.check_lval_simple(other, binding_type, clashes),
        }
    }

    /// acorn's `checkLValInnerPattern`.
    pub(super) fn check_lval_inner_pattern(&mut self, pat: &Pat, binding_type: u8, clashes: Clashes<'_>) -> R<()> {
        match pat {
            Pat::Assign { left, .. } => self.check_lval_pattern(left, binding_type, clashes),
            Pat::Rest { argument, .. } => self.check_lval_pattern(argument, binding_type, clashes),
            other => self.check_lval_pattern(other, binding_type, clashes),
        }
    }
}

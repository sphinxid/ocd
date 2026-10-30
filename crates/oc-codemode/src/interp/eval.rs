//! The AST evaluator. Port of `interpreter/interpreter.ts`.

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::rc::Rc;

use futures::future::LocalBoxFuture;

use super::coerce::{coerce_to_number, coerce_to_string, to_primitive, Hint};
use super::errors::*;
use super::function::{make_function, Closure, FuncImpl, NResult};
use super::iteration::{self, builtin_cursor};
use super::limits::{check_string_length, MAX_CALL_DEPTH};
use super::generators::{suspend_body, GeneratorHandle, ReqKind};
use super::references::reject_circular_insertion;
use super::{new_env, Binding, Env, Rt, Site};
use crate::ast::*;
use crate::object::{
    self, define, get_with_receiver, has, has_prototype, record, remove, set, Obj, ObjKind, FROZEN, HIDDEN,
};
use crate::value::{JsStr, PropKey, Sym, ToolRef, Value};

/// A statement's completion.
#[derive(Clone)]
pub enum Completion {
    Normal,
    Return(Value),
    Break(Option<Rc<str>>),
    Continue(Option<Rc<str>>),
}

type Labels = Option<Rc<Vec<Rc<str>>>>;

fn has_label(labels: &Labels, label: &Rc<str>) -> bool {
    labels.as_ref().map(|l| l.iter().any(|x| x == label)).unwrap_or(false)
}

/// What a loop does with its body's result: exit with a completion, or None to keep iterating.
fn loop_exit(result: Completion, labels: &Labels) -> Option<Completion> {
    match result {
        Completion::Return(_) => Some(result),
        Completion::Break(Some(ref l)) if !has_label(labels, l) => Some(result),
        Completion::Break(_) => Some(Completion::Normal),
        Completion::Continue(Some(ref l)) if !has_label(labels, l) => Some(result),
        _ => None,
    }
}

/// A property key as evaluated: numbers stay numbers so tool paths can reject them.
#[derive(Clone)]
enum EvKey {
    Str(JsStr),
    Num(f64),
    Sym(Sym),
}

impl EvKey {
    fn prop(&self) -> PropKey {
        match self {
            EvKey::Str(s) => PropKey::Str(s.clone()),
            EvKey::Num(n) => PropKey::Str(JsStr::from(crate::number::to_string(*n))),
            EvKey::Sym(s) => PropKey::Sym(*s),
        }
    }
}

/// ToPropertyKey on a primitive (or an opaque value, which keeps its built-in string form).
fn primitive_key(value: &Value) -> EvKey {
    match value {
        Value::String(s) => EvKey::Str(s.clone()),
        Value::Number(n) => EvKey::Num(*n),
        Value::Symbol(s) => EvKey::Sym(*s),
        other => EvKey::Str(coerce_to_string(other)),
    }
}

#[derive(Clone, Copy)]
enum BindMode {
    Declare { mutable: bool, initialize: bool },
    Assign,
}

/// A resolved member: the object to read through and the receiver inherited accessors see.
enum MemberRef {
    Prop { target: Obj, key: PropKey, receiver: Value },
    Tool(ToolRef),
    Value(Value),
}

/// Native engines name the callee (`search(...).catch is not a function`), including call chains.
fn callee_description(node: Option<&Expr>) -> Option<String> {
    match node? {
        Expr::Ident(i) => Some(i.name.to_string()),
        Expr::Call { callee, .. } => callee_description(Some(callee)).map(|t| format!("{t}(...)")),
        Expr::Member(m) => {
            let key = match &m.property {
                MemberProp::Ident(i) => i.name.to_string(),
                MemberProp::Computed(e) => match &**e {
                    Expr::Lit(Lit { value: LitValue::String(s), .. }) => String::from_utf16_lossy(s),
                    _ => return None,
                },
                MemberProp::Private(_) => return None,
            };
            let object = callee_description(Some(&m.object))?;
            Some(format!("{object}.{key}"))
        }
        _ => None,
    }
}

fn literal(node: &Lit) -> JsResult<Value> {
    Ok(match &node.value {
        LitValue::Null => Value::Null,
        LitValue::Bool(b) => Value::Bool(*b),
        LitValue::Number(n) => Value::Number(*n),
        LitValue::String(s) => Value::String(JsStr::from_rc(s.clone())),
        LitValue::BigInt(_) => return Err(type_error_at("BigInt literals are not supported.", node.loc)),
        LitValue::Regex { .. } => return Err(unsupported_syntax("RegExpLiteral", node.loc)),
    })
}

fn literal_key(node: &Lit) -> JsResult<EvKey> {
    Ok(primitive_key(&literal(node)?))
}

fn instanceof_value(lhs: &Value, rhs: &Value, node: Loc) -> JsResult<bool> {
    let Value::Object(r) = rhs else {
        return Err(type_error_at("The right-hand side of 'instanceof' is not callable.", node));
    };
    if !r.is_callable() {
        return Err(type_error_at("The right-hand side of 'instanceof' is not callable.", node));
    }
    let prototype = object::get(r, &PropKey::str("prototype")).map_err(|e| locate(e, Some(node)))?;
    let Value::Object(p) = prototype else {
        return Err(type_error_at("The right-hand side of 'instanceof' has no 'prototype' object.", node));
    };
    Ok(has_prototype(lhs, &p))
}

pub fn collect_pattern_names(pattern: &Pat, out: &mut Vec<Rc<str>>) {
    match pattern {
        Pat::Ident(i) => out.push(i.name.clone()),
        Pat::Assign { left, .. } => collect_pattern_names(left, out),
        Pat::Rest { argument, .. } => collect_pattern_names(argument, out),
        Pat::Array { elements, .. } => {
            for e in elements.iter().flatten() {
                collect_pattern_names(e, out);
            }
        }
        Pat::Object { properties, .. } => {
            for p in properties {
                match p {
                    ObjectPatMember::Rest { argument, .. } => collect_pattern_names(argument, out),
                    ObjectPatMember::Prop(p) => collect_pattern_names(&p.value, out),
                }
            }
        }
        Pat::Member(_) => {}
    }
}

fn pattern_names(pattern: &Pat) -> Vec<Rc<str>> {
    let mut out = Vec::new();
    collect_pattern_names(pattern, &mut out);
    out
}

// `var` names declared anywhere in a function body except inside nested functions.
fn collect_var_names(node: &Stmt, out: &mut Vec<Rc<str>>) {
    match node {
        Stmt::VarDecl(d) => {
            if d.kind == VarKind::Var {
                for decl in &d.declarations {
                    collect_pattern_names(&decl.id, out);
                }
            }
        }
        Stmt::Block(b) => b.body.iter().for_each(|s| collect_var_names(s, out)),
        Stmt::If { consequent, alternate, .. } => {
            collect_var_names(consequent, out);
            if let Some(a) = alternate {
                collect_var_names(a, out);
            }
        }
        Stmt::For { init, body, .. } => {
            if let Some(ForInit::Var(d)) = init
                && d.kind == VarKind::Var
            {
                for decl in &d.declarations {
                    collect_pattern_names(&decl.id, out);
                }
            }
            collect_var_names(body, out);
        }
        Stmt::ForIn { left, body, .. } | Stmt::ForOf { left, body, .. } => {
            if let ForHead::Var(d) = left
                && d.kind == VarKind::Var
            {
                for decl in &d.declarations {
                    collect_pattern_names(&decl.id, out);
                }
            }
            collect_var_names(body, out);
        }
        Stmt::While { body, .. } | Stmt::DoWhile { body, .. } | Stmt::Labeled { body, .. } => {
            collect_var_names(body, out)
        }
        Stmt::Switch { cases, .. } => {
            for c in cases {
                c.consequent.iter().for_each(|s| collect_var_names(s, out));
            }
        }
        Stmt::Try { block, handler, finalizer, .. } => {
            block.body.iter().for_each(|s| collect_var_names(s, out));
            if let Some(h) = handler {
                h.body.body.iter().for_each(|s| collect_var_names(s, out));
            }
            if let Some(f) = finalizer {
                f.body.iter().for_each(|s| collect_var_names(s, out));
            }
        }
        _ => {}
    }
}

// Whether a function body (or a parameter default) reads `arguments`, looking through arrows but not nested
// functions, which own theirs.
fn uses_arguments(f: &Function) -> bool {
    fn expr(e: &Expr) -> bool {
        match e {
            Expr::Ident(i) => &*i.name == "arguments",
            Expr::Lit(_) | Expr::This(_) | Expr::Super(_) | Expr::PrivateName(_) | Expr::Unsupported { .. } => false,
            Expr::Array { elements, .. } => elements.iter().flatten().any(eos),
            Expr::Object { properties, .. } => properties.iter().any(|p| match p {
                ObjectMember::Prop(p) => key(&p.key) || expr(&p.value),
                ObjectMember::Spread { argument, .. } => expr(argument),
            }),
            Expr::Function(f) => f.kind == FunctionKind::Arrow && func(f),
            Expr::Template(t) => t.expressions.iter().any(expr),
            Expr::TaggedTemplate { tag, quasi, .. } => expr(tag) || quasi.expressions.iter().any(expr),
            Expr::Member(m) => member(m),
            Expr::New { callee, arguments, .. } => expr(callee) || arguments.iter().any(eos),
            Expr::Call { callee, arguments, .. } => expr(callee) || arguments.iter().any(eos),
            Expr::Update { argument, .. } | Expr::Await { argument, .. } | Expr::Unary { argument, .. } => {
                expr(argument)
            }
            Expr::Binary { left, right, .. } | Expr::Logical { left, right, .. } => expr(left) || expr(right),
            Expr::Conditional { test, consequent, alternate, .. } => {
                expr(test) || expr(consequent) || expr(alternate)
            }
            Expr::Yield { argument, .. } => argument.as_ref().map(|a| expr(a)).unwrap_or(false),
            Expr::Assign { left, right, .. } => pat(left) || expr(right),
            Expr::Sequence { expressions, .. } => expressions.iter().any(expr),
            Expr::Chain { expression, .. } => expr(expression),
        }
    }
    fn member(m: &Member) -> bool {
        expr(&m.object)
            || match &m.property {
                MemberProp::Ident(i) => &*i.name == "arguments",
                MemberProp::Private(_) => false,
                MemberProp::Computed(e) => expr(e),
            }
    }
    fn key(k: &PropKey2) -> bool {
        match k {
            crate::ast::PropKey::Ident(i) => &*i.name == "arguments",
            crate::ast::PropKey::Computed(e) => expr(e),
            _ => false,
        }
    }
    type PropKey2 = crate::ast::PropKey;
    fn eos(e: &ExprOrSpread) -> bool {
        match e {
            ExprOrSpread::Expr(e) => expr(e),
            ExprOrSpread::Spread { argument, .. } => expr(argument),
        }
    }
    fn pat(p: &Pat) -> bool {
        match p {
            Pat::Ident(i) => &*i.name == "arguments",
            Pat::Member(m) => member(m),
            Pat::Object { properties, .. } => properties.iter().any(|p| match p {
                ObjectPatMember::Prop(p) => key(&p.key) || pat(&p.value),
                ObjectPatMember::Rest { argument, .. } => pat(argument),
            }),
            Pat::Array { elements, .. } => elements.iter().flatten().any(pat),
            Pat::Rest { argument, .. } => pat(argument),
            Pat::Assign { left, right, .. } => pat(left) || expr(right),
        }
    }
    fn stmt(s: &Stmt) -> bool {
        match s {
            Stmt::Expr { expr: e, .. } => expr(e),
            Stmt::Block(b) => b.body.iter().any(stmt),
            Stmt::Empty(_) | Stmt::Break { .. } | Stmt::Continue { .. } | Stmt::Unsupported { .. } => false,
            Stmt::Return { argument, .. } => argument.as_ref().map(expr).unwrap_or(false),
            Stmt::Labeled { body, .. } => stmt(body),
            Stmt::If { test, consequent, alternate, .. } => {
                expr(test) || stmt(consequent) || alternate.as_ref().map(|a| stmt(a)).unwrap_or(false)
            }
            Stmt::Switch { discriminant, cases, .. } => {
                expr(discriminant)
                    || cases.iter().any(|c| c.test.as_ref().map(expr).unwrap_or(false) || c.consequent.iter().any(stmt))
            }
            Stmt::Throw { argument, .. } => expr(argument),
            Stmt::Try { block, handler, finalizer, .. } => {
                block.body.iter().any(stmt)
                    || handler
                        .as_ref()
                        .map(|h| h.param.as_ref().map(pat).unwrap_or(false) || h.body.body.iter().any(stmt))
                        .unwrap_or(false)
                    || finalizer.as_ref().map(|f| f.body.iter().any(stmt)).unwrap_or(false)
            }
            Stmt::While { test, body, .. } | Stmt::DoWhile { test, body, .. } => expr(test) || stmt(body),
            Stmt::For { init, test, update, body, .. } => {
                init.as_ref()
                    .map(|i| match i {
                        ForInit::Var(d) => decl(d),
                        ForInit::Expr(e) => expr(e),
                    })
                    .unwrap_or(false)
                    || test.as_ref().map(expr).unwrap_or(false)
                    || update.as_ref().map(expr).unwrap_or(false)
                    || stmt(body)
            }
            Stmt::ForIn { left, right, body, .. } | Stmt::ForOf { left, right, body, .. } => {
                (match left {
                    ForHead::Var(d) => decl(d),
                    ForHead::Pat(p) => pat(p),
                }) || expr(right)
                    || stmt(body)
            }
            Stmt::FunctionDecl(_) => false,
            Stmt::VarDecl(d) => decl(d),
        }
    }
    fn decl(d: &VarDecl) -> bool {
        d.declarations.iter().any(|x| pat(&x.id) || x.init.as_ref().map(expr).unwrap_or(false))
    }
    fn func(f: &Function) -> bool {
        f.params.iter().any(pat)
            || match &f.body {
                FunctionBody::Block(b) => b.body.iter().any(stmt),
                FunctionBody::Expr(e) => expr(e),
            }
    }
    func(f)
}

/// One activation: the top-level program or a single function call.
pub struct Frame {
    pub rt: Rt,
    /// Nested call depth; resets when an await resumes, since the continuation runs from the job queue.
    pub depth: Cell<usize>,
    /// The call site this frame runs under (Effect's `CallSite` context).
    pub ambient: Site,
    /// The generator whose body this frame runs, where `yield` suspends.
    pub generator: RefCell<Option<Rc<GeneratorHandle>>>,
    pub gen_async: Cell<bool>,
}

/// The single dispatch for calls made by built-ins (upstream `ctx.call`): a root frame at depth 0.
pub async fn call_value(rt: &Rt, callable: &Value, this: Value, args: Vec<Value>, site: Site) -> JsResult<Value> {
    let frame = Frame::new(rt.clone(), 0, site);
    frame.call(callable.clone(), this, args, None, None).await
}

/// Await: suspends the innermost async body until `value` settles; a stall while it waits is reported at `node`.
pub async fn await_value(rt: &Rt, value: Value, node: Option<Loc>) -> JsResult<Value> {
    super::runner::await_value(rt, value, node).await
}

/// A native promise, or an object with a callable `then`.
fn is_thenable(value: &Value) -> bool {
    match value {
        Value::Object(o) => o.is_promise() || object::get(o, &PropKey::str("then")).map(|t| t.js_typeof() == "function").unwrap_or(false),
        _ => false,
    }
}

impl Frame {
    pub fn new(rt: Rt, depth: usize, ambient: Site) -> Frame {
        Frame { rt, depth: Cell::new(depth), ambient, generator: RefCell::new(None), gen_async: Cell::new(false) }
    }

    fn site(&self, node: Option<Loc>) -> Site {
        Site { node, depth: self.depth.get() }
    }

    // ---- scopes ----

    fn reserve(&self, env: &Env, name: &Rc<str>, mutable: bool, node: Loc) -> JsResult<()> {
        if env.has_own(name) {
            return Err(type_error_at(format!("Identifier '{name}' has already been declared."), node));
        }
        env.set_binding(name.clone(), Binding { mutable, value: Value::Undefined, initialized: false });
        Ok(())
    }

    fn initialize(&self, env: &Env, name: &Rc<str>, value: Value, node: Loc) -> JsResult<()> {
        let mut vars = env.vars.borrow_mut();
        match vars.get_mut(name) {
            Some(b) if !b.initialized => {
                b.value = value;
                b.initialized = true;
                Ok(())
            }
            _ => Err(type_error_at(format!("Identifier '{name}' has not been reserved for initialization."), node)),
        }
    }

    fn declare(&self, env: &Env, name: &Rc<str>, value: Value, mutable: bool, node: Loc) -> JsResult<()> {
        if env.has_own(name) {
            return Err(type_error_at(format!("Identifier '{name}' has already been declared."), node));
        }
        env.set_binding(name.clone(), Binding { mutable, value, initialized: true });
        Ok(())
    }

    fn lookup(&self, env: &Env, name: &str, node: Loc) -> JsResult<Value> {
        let Some(scope) = env.lookup(name) else {
            return Err(reference_error_at(format!("Unknown identifier '{name}'."), node));
        };
        let vars = scope.vars.borrow();
        let b = vars.get(name).unwrap();
        if !b.initialized {
            return Err(reference_error_at(format!("Cannot access '{name}' before initialization."), node));
        }
        Ok(b.value.clone())
    }

    fn assign_var(&self, env: &Env, name: &str, value: Value, node: Loc) -> JsResult<Value> {
        let Some(scope) = env.lookup(name) else {
            return Err(reference_error_at(format!("Unknown identifier '{name}'."), node));
        };
        let mut vars = scope.vars.borrow_mut();
        let b = vars.get_mut(name).unwrap();
        if !b.initialized {
            return Err(reference_error_at(format!("Cannot access '{name}' before initialization."), node));
        }
        if !b.mutable {
            return Err(type_error_at(format!("Cannot assign to constant '{name}'."), node));
        }
        b.value = value.clone();
        Ok(value)
    }

    fn hoist_functions(&self, env: &Env, statements: &[Stmt]) {
        for s in statements {
            if let Stmt::FunctionDecl(f) = s {
                let name = f.id.as_ref().map(|i| i.name.clone()).unwrap_or_else(|| Rc::from(""));
                let value = self.create_function(env, f, None);
                env.set_binding(name, Binding { mutable: true, value, initialized: true });
            }
        }
    }

    fn hoist_vars(&self, env: &Env, statements: &[Stmt], parameters: Option<&Env>) {
        let key = statements.as_ptr() as usize;
        let cached = self.rt.var_names.borrow().get(&key).cloned();
        let names = match cached {
            Some(n) => n,
            None => {
                let mut out = Vec::new();
                for s in statements {
                    collect_var_names(s, &mut out);
                }
                let names = Rc::new(out);
                self.rt.var_names.borrow_mut().insert(key, names.clone());
                names
            }
        };
        for name in names.iter() {
            if env.has_own(name) {
                continue;
            }
            let value = parameters
                .and_then(|p| p.vars.borrow().get(name).map(|b| b.value.clone()))
                .unwrap_or(Value::Undefined);
            env.set_binding(name.clone(), Binding { mutable: true, value, initialized: true });
        }
    }

    fn predeclare_lexical(&self, env: &Env, statements: &[Stmt]) -> JsResult<()> {
        for s in statements {
            let Stmt::VarDecl(d) = s else { continue };
            if d.kind == VarKind::Var {
                continue;
            }
            for decl in &d.declarations {
                for name in pattern_names(&decl.id) {
                    self.reserve(env, &name, d.kind != VarKind::Const, decl.loc)?;
                }
            }
        }
        Ok(())
    }

    fn predeclare_pattern(&self, env: &Env, pattern: &Pat, mutable: bool, node: Loc) -> JsResult<()> {
        for name in pattern_names(pattern) {
            self.reserve(env, &name, mutable, node)?;
        }
        Ok(())
    }

    // ---- program ----

    pub async fn run(&self, global: &Env, program: &Program) -> JsResult<Value> {
        // Keep top-level declarations separate so they can shadow builtins.
        let env = new_env(Some(global));
        self.predeclare_lexical(&env, &program.body)?;
        self.hoist_functions(&env, &program.body);
        self.hoist_vars(&env, &program.body, None);
        let mut value = Value::Undefined;
        let count = program.body.len();
        for (index, statement) in program.body.iter().enumerate() {
            if index == count - 1
                && let Stmt::Expr { expr, .. } = statement
            {
                value = self.eval(&env, expr).await?;
                break;
            }
            match self.exec(&env, statement).await? {
                Completion::Return(v) => {
                    value = v;
                    break;
                }
                Completion::Break(_) => {
                    return Err(type_error_at("Unexpected 'break' outside of a loop.", statement.loc()));
                }
                Completion::Continue(_) => {
                    return Err(type_error_at("Unexpected 'continue' outside of a loop.", statement.loc()));
                }
                Completion::Normal => {}
            }
        }
        // The implicit async body adopts returned promises before copy-out.
        if is_thenable(&value) { self.await_value(value, None).await } else { Ok(value) }
    }

    // ---- awaiting ----

    /// The continuation runs from the job queue, so the stack below it is gone when it resumes.
    pub async fn await_value(&self, value: Value, node: Option<Loc>) -> JsResult<Value> {
        let result = await_value(&self.rt, value, node).await;
        self.depth.set(0);
        result
    }

    // ---- statements ----

    pub fn exec<'a>(&'a self, env: &'a Env, node: &'a Stmt) -> LocalBoxFuture<'a, JsResult<Completion>> {
        // Each arm is boxed on its own so a statement only allocates the state its kind needs.
        if let Err(e) = self.rt.interrupt.check() {
            return Box::pin(std::future::ready(Err(e)));
        }
        let ready = |r: JsResult<Completion>| -> LocalBoxFuture<'a, JsResult<Completion>> { Box::pin(std::future::ready(r)) };
        match node {
            Stmt::Expr { expr, .. } => Box::pin(async move {
                self.eval(env, expr).await?;
                Ok(Completion::Normal)
            }),
            Stmt::VarDecl(d) => Box::pin(async move {
                self.var_decl(env, d).await?;
                Ok(Completion::Normal)
            }),
            Stmt::Return { argument, loc } => match argument {
                None => ready(Ok(Completion::Return(Value::Undefined))),
                Some(a) => Box::pin(async move {
                    let value = self.eval(env, a).await?;
                    // An async generator awaits what it returns, as it awaits what it yields.
                    if self.gen_async.get() && self.generator.borrow().is_some() {
                        return Ok(Completion::Return(self.await_value(value, Some(*loc)).await?));
                    }
                    Ok(Completion::Return(value))
                }),
            },
            Stmt::Block(b) => Box::pin(self.block(env, b)),
            Stmt::If { test, consequent, alternate, .. } => Box::pin(async move {
                if self.eval(env, test).await?.truthy() {
                    self.exec(env, consequent).await
                } else if let Some(a) = alternate {
                    self.exec(env, a).await
                } else {
                    Ok(Completion::Normal)
                }
            }),
            Stmt::Switch { discriminant, cases, .. } => Box::pin(self.switch(env, discriminant, cases)),
            Stmt::Labeled { .. } => Box::pin(self.labeled(env, node)),
            Stmt::While { .. } | Stmt::DoWhile { .. } | Stmt::For { .. } | Stmt::ForOf { .. } | Stmt::ForIn { .. } => {
                self.looping(env, node, None)
            }
            Stmt::Break { label, .. } => ready(Ok(Completion::Break(label.as_ref().map(|l| l.name.clone())))),
            Stmt::Continue { label, .. } => ready(Ok(Completion::Continue(label.as_ref().map(|l| l.name.clone())))),
            Stmt::Throw { argument, .. } => Box::pin(async move {
                let value = self.eval(env, argument).await?;
                Err(Thrown::Value(value))
            }),
            Stmt::Try { block, handler, finalizer, .. } => Box::pin(self.try_stmt(env, block, handler, finalizer)),
            Stmt::Empty(_) | Stmt::FunctionDecl(_) => ready(Ok(Completion::Normal)),
            Stmt::Unsupported { kind, loc } => ready(Err(unsupported_syntax(kind, *loc))),
        }
    }

    async fn block(&self, env: &Env, node: &Block) -> JsResult<Completion> {
        let env = new_env(Some(env));
        self.predeclare_lexical(&env, &node.body)?;
        self.hoist_functions(&env, &node.body);
        for s in &node.body {
            let result = self.exec(&env, s).await?;
            if !matches!(result, Completion::Normal) {
                return Ok(result);
            }
        }
        Ok(Completion::Normal)
    }

    async fn switch(&self, env: &Env, discriminant: &Expr, cases: &[SwitchCase]) -> JsResult<Completion> {
        let value = self.eval(env, discriminant).await?;
        let env = new_env(Some(env));
        let statements: Vec<&Stmt> = cases.iter().flat_map(|c| c.consequent.iter()).collect();
        // Predeclare/hoist over the flattened case bodies.
        for s in &statements {
            self.predeclare_lexical(&env, std::slice::from_ref(*s))?;
        }
        for s in &statements {
            self.hoist_functions(&env, std::slice::from_ref(*s));
        }
        let mut default_index = None;
        let mut selected = None;
        for (index, case) in cases.iter().enumerate() {
            let Some(test) = &case.test else {
                default_index = Some(index);
                continue;
            };
            if self.eval(&env, test).await?.strict_equals(&value) {
                selected = Some(index);
                break;
            }
        }
        let Some(start) = selected.or(default_index) else { return Ok(Completion::Normal) };
        for case in &cases[start..] {
            for s in &case.consequent {
                match self.exec(&env, s).await? {
                    Completion::Break(None) => return Ok(Completion::Normal),
                    Completion::Normal => {}
                    other => return Ok(other),
                }
            }
        }
        Ok(Completion::Normal)
    }

    async fn labeled(&self, env: &Env, node: &Stmt) -> JsResult<Completion> {
        let mut labels: Vec<Rc<str>> = Vec::new();
        let mut body = node;
        while let Stmt::Labeled { label, body: inner, .. } = body {
            labels.push(label.name.clone());
            body = inner;
        }
        let labels = Some(Rc::new(labels));
        let result = match body {
            Stmt::While { .. } | Stmt::DoWhile { .. } | Stmt::For { .. } | Stmt::ForOf { .. } | Stmt::ForIn { .. } => {
                self.looping(env, body, labels.clone()).await?
            }
            other => self.exec(env, other).await?,
        };
        Ok(match result {
            Completion::Break(Some(ref l)) if has_label(&labels, l) => Completion::Normal,
            other => other,
        })
    }

    fn looping<'a>(&'a self, env: &'a Env, node: &'a Stmt, labels: Labels) -> LocalBoxFuture<'a, JsResult<Completion>> {
        match node {
            Stmt::While { test, body, .. } => Box::pin(async move {
                while self.eval(env, test).await?.truthy() {
                    self.rt.interrupt.check()?;
                    if let Some(exit) = loop_exit(self.exec(env, body).await?, &labels) {
                        return Ok(exit);
                    }
                }
                Ok(Completion::Normal)
            }),
            Stmt::DoWhile { body, test, .. } => Box::pin(async move {
                loop {
                    self.rt.interrupt.check()?;
                    if let Some(exit) = loop_exit(self.exec(env, body).await?, &labels) {
                        return Ok(exit);
                    }
                    if !self.eval(env, test).await?.truthy() {
                        break;
                    }
                }
                Ok(Completion::Normal)
            }),
            Stmt::For { init, test, update, body, .. } => Box::pin(self.for_stmt(env, init, test, update, body, labels)),
            Stmt::ForOf { left, right, body, is_await, loc } => {
                Box::pin(self.for_of(env, left, right, body, *is_await, *loc, labels))
            }
            Stmt::ForIn { left, right, body, loc } => Box::pin(self.for_in(env, left, right, body, *loc, labels)),
            _ => unreachable!(),
        }
    }

    async fn for_stmt(
        &self,
        env: &Env,
        init: &Option<ForInit>,
        test: &Option<Expr>,
        update: &Option<Expr>,
        body: &Stmt,
        labels: Labels,
    ) -> JsResult<Completion> {
        let parent = env;
        let mut env = new_env(Some(parent));
        let lexical = matches!(init, Some(ForInit::Var(d)) if d.kind != VarKind::Var);
        if let Some(ForInit::Var(d)) = init
            && lexical
        {
            for decl in &d.declarations {
                for name in pattern_names(&decl.id) {
                    self.reserve(&env, &name, d.kind != VarKind::Const, decl.loc)?;
                }
            }
        }
        match init {
            Some(ForInit::Var(d)) => self.var_decl(&env, d).await?,
            Some(ForInit::Expr(e)) => {
                self.eval(&env, e).await?;
            }
            None => {}
        }
        let per_iteration: Vec<Rc<str>> = if lexical { env.vars.borrow().keys().cloned().collect() } else { Vec::new() };
        let next_iteration = |current: &Env| -> Env {
            if per_iteration.is_empty() {
                return current.clone();
            }
            let fresh = new_env(Some(parent));
            {
                let vars = current.vars.borrow();
                let mut out = fresh.vars.borrow_mut();
                for name in &per_iteration {
                    if let Some(b) = vars.get(name) {
                        out.insert(name.clone(), b.clone());
                    }
                }
            }
            fresh
        };
        env = next_iteration(&env);
        loop {
            self.rt.interrupt.check()?;
            if let Some(t) = test
                && !self.eval(&env, t).await?.truthy()
            {
                break;
            }
            if let Some(exit) = loop_exit(self.exec(&env, body).await?, &labels) {
                return Ok(exit);
            }
            env = next_iteration(&env);
            if let Some(u) = update {
                self.eval(&env, u).await?;
            }
        }
        Ok(Completion::Normal)
    }

    fn loop_declaration<'a>(
        &self,
        left: &'a ForHead,
        statement: &str,
    ) -> JsResult<Option<(&'a Pat, bool, bool, Loc)>> {
        let ForHead::Var(d) = left else { return Ok(None) };
        if d.declarations.len() != 1 {
            return Err(type_error_at(format!("{statement} supports one declared binding."), d.loc));
        }
        Ok(Some((&d.declarations[0].id, d.kind != VarKind::Const, d.kind != VarKind::Var, d.loc)))
    }

    #[allow(clippy::too_many_arguments)]
    async fn for_of(
        &self,
        env: &Env,
        left: &ForHead,
        right: &Expr,
        body: &Stmt,
        awaiting: bool,
        node: Loc,
        labels: Labels,
    ) -> JsResult<Completion> {
        let declared = self.loop_declaration(left, "for...of")?;
        let outer = if declared.map(|d| d.2).unwrap_or(false) { new_env(Some(env)) } else { env.clone() };
        if let Some((pattern, mutable, true, loc)) = declared {
            self.predeclare_pattern(&outer, pattern, mutable, loc)?;
        }
        let right_value = self.eval(&outer, right).await?;
        let builtin = builtin_cursor(&self.rt, &right_value);
        let custom = if builtin.is_none() {
            iteration::custom_iterator(&self.rt, &right_value, Some(node), awaiting, self.site(Some(node))).await?
        } else {
            None
        };
        if custom.is_none() && builtin.is_none() {
            return Err(invalid_data_at(
                format!(
                    "{} requires an iterable value, received {}.",
                    if awaiting { "for await...of" } else { "for...of" },
                    super::describe_value(&right_value)
                ),
                node,
            ));
        }
        // `for await` over a synchronous iterator drives it through CreateAsyncFromSyncIterator, as JS does.
        let iterator = if awaiting && !custom.as_ref().is_some_and(|c| c.asynchronous) {
            Some(iteration::async_from_sync(&self.rt, builtin.clone(), custom.clone(), Some(node)))
        } else {
            custom
        };
        let cursor = if iterator.is_none() { builtin } else { None };
        let assignment = match left {
            ForHead::Pat(p) => {
                if matches!(p, Pat::Rest { .. } | Pat::Assign { .. }) {
                    return Err(type_error_at("Unsupported for...of binding.", p.loc()));
                }
                Some(p)
            }
            ForHead::Var(_) => None,
        };
        let left_loc = match left {
            ForHead::Var(d) => d.loc,
            ForHead::Pat(p) => p.loc(),
        };
        let close = || async {
            match (&iterator, &cursor) {
                (Some(it), _) => iteration::close_iterator(&self.rt, it, Some(node), self.site(Some(node))).await,
                (None, Some(c)) => c.close().await,
                _ => Ok(()),
            }
        };
        loop {
            let step = match (&iterator, &cursor) {
                (Some(it), _) => iteration::next_iterator_result(&self.rt, it, Some(node), self.site(Some(node))).await?,
                (None, Some(c)) => c.next().await?,
                _ => unreachable!(),
            };
            if step.done {
                return Ok(Completion::Normal);
            }
            self.rt.interrupt.check()?;
            let result: JsResult<Completion> = async {
                let body_env;
                if let Some((pattern, mutable, lexical, _)) = declared {
                    if lexical {
                        body_env = new_env(Some(&outer));
                        self.predeclare_pattern(&body_env, pattern, mutable, left_loc)?;
                        self.declare_pattern(&body_env, pattern, step.value.clone(), mutable, left_loc, true).await?;
                    } else {
                        body_env = outer.clone();
                        self.assign_pattern(&body_env, pattern, step.value.clone(), left_loc).await?;
                    }
                } else {
                    body_env = outer.clone();
                    if let Some(p) = assignment {
                        self.assign_pattern(&body_env, p, step.value.clone(), left_loc).await?;
                    }
                }
                self.exec(&body_env, body).await
            }
            .await;
            match result {
                Err(Thrown::Interrupt) => return Err(Thrown::Interrupt),
                Err(e) => {
                    let closed = close().await;
                    if let (Err(ce), Thrown::GeneratorReturn(_)) = (&closed, &e) {
                        return Err(ce.clone());
                    }
                    return Err(e);
                }
                Ok(c) => {
                    if let Some(exit) = loop_exit(c, &labels) {
                        close().await?;
                        return Ok(exit);
                    }
                }
            }
        }
    }

    // for...in over null/undefined iterates nothing, like JS. After the own keys come each prototype's enumerable
    // string keys, skipping any key a nearer level has, even as a non-enumerable property.
    fn enumerable_keys(&self, value: &Value, node: Loc) -> JsResult<Vec<JsStr>> {
        match value {
            Value::Tool(t) => Ok(self.rt.tools.keys(t.path())?.into_iter().map(JsStr::from).collect()),
            Value::Undefined | Value::Null => Ok(Vec::new()),
            _ => {
                let source = crate::stdlib::object::enumerable_source(&self.rt, "for...in", value, Some(node))?;
                let mut chain = vec![source.clone()];
                // A string's characters stand in an array, but its prototype is String.prototype.
                let mut proto = self.rt.builtins.primitive_prototype(value).cloned().or_else(|| source.proto());
                while let Some(p) = proto {
                    proto = p.proto();
                    chain.push(p);
                }
                let mut out = Vec::new();
                for (depth, target) in chain.iter().enumerate() {
                    for key in object::keys(target) {
                        let prop = PropKey::Str(key.clone());
                        if !chain[..depth].iter().any(|level| object::has_own(level, &prop)) {
                            out.push(key);
                        }
                    }
                }
                Ok(out)
            }
        }
    }

    async fn for_in(
        &self,
        env: &Env,
        left: &ForHead,
        right: &Expr,
        body: &Stmt,
        _node: Loc,
        labels: Labels,
    ) -> JsResult<Completion> {
        let declared = self.loop_declaration(left, "for...in")?;
        let outer = if declared.map(|d| d.2).unwrap_or(false) { new_env(Some(env)) } else { env.clone() };
        if let Some((pattern, mutable, true, loc)) = declared {
            self.predeclare_pattern(&outer, pattern, mutable, loc)?;
        }
        let right_value = self.eval(&outer, right).await?;
        let keys = self.enumerable_keys(&right_value, right.loc())?;
        let assignment = match left {
            ForHead::Pat(p) => {
                if matches!(p, Pat::Rest { .. } | Pat::Assign { .. }) {
                    return Err(type_error_at("Unsupported for...in binding.", p.loc()));
                }
                Some(p)
            }
            ForHead::Var(_) => None,
        };
        let left_loc = match left {
            ForHead::Var(d) => d.loc,
            ForHead::Pat(p) => p.loc(),
        };
        for key in keys {
            self.rt.interrupt.check()?;
            let value = Value::String(key);
            let body_env;
            if let Some((pattern, mutable, lexical, _)) = declared {
                if lexical {
                    body_env = new_env(Some(&outer));
                    self.predeclare_pattern(&body_env, pattern, mutable, left_loc)?;
                    self.declare_pattern(&body_env, pattern, value, mutable, left_loc, true).await?;
                } else {
                    body_env = outer.clone();
                    self.assign_pattern(&body_env, pattern, value, left_loc).await?;
                }
            } else {
                body_env = outer.clone();
                if let Some(p) = assignment {
                    self.assign_pattern(&body_env, p, value, left_loc).await?;
                }
            }
            let result = self.exec(&body_env, body).await?;
            if let Some(exit) = loop_exit(result, &labels) {
                return Ok(exit);
            }
        }
        Ok(Completion::Normal)
    }

    async fn try_stmt(
        &self,
        env: &Env,
        block: &Block,
        handler: &Option<CatchClause>,
        finalizer: &Option<Block>,
    ) -> JsResult<Completion> {
        let attempted = match self.block(env, block).await {
            Ok(c) => Ok(c),
            Err(e @ (Thrown::Interrupt | Thrown::GeneratorReturn(_))) => Err(e),
            Err(e) => match handler {
                None => Err(e),
                Some(h) => {
                    let caught = materialize(&self.rt, &e);
                    let catch_env = new_env(Some(env));
                    let result: JsResult<Completion> = async {
                        if let Some(p) = &h.param {
                            self.declare_pattern(&catch_env, p, caught, true, h.loc, false).await?;
                        }
                        self.block(&catch_env, &h.body).await
                    }
                    .await;
                    result
                }
            },
        };
        let Some(f) = finalizer else { return attempted };
        let abrupt = |c: &Completion| !matches!(c, Completion::Normal);
        match attempted {
            Err(Thrown::Interrupt) => Err(Thrown::Interrupt),
            Err(e) => {
                let fin = self.block(env, f).await?;
                if abrupt(&fin) { Ok(fin) } else { Err(e) }
            }
            Ok(result) => {
                let fin = self.block(env, f).await?;
                if abrupt(&fin) { Ok(fin) } else { Ok(result) }
            }
        }
    }

    async fn var_decl(&self, env: &Env, node: &VarDecl) -> JsResult<()> {
        for decl in &node.declarations {
            let id = &decl.id;
            if node.kind == VarKind::Var {
                if let Some(init) = &decl.init {
                    let value = match id {
                        Pat::Ident(i) => self.eval_named(env, init, &i.name).await?,
                        _ => self.eval(env, init).await?,
                    };
                    self.assign_pattern(env, id, value, decl.loc).await?;
                }
                continue;
            }
            let value = match &decl.init {
                Some(init) => match id {
                    Pat::Ident(i) => self.eval_named(env, init, &i.name).await?,
                    _ => self.eval(env, init).await?,
                },
                None => Value::Undefined,
            };
            self.declare_pattern(env, id, value, node.kind != VarKind::Const, decl.loc, true).await?;
        }
        Ok(())
    }

    // ---- patterns ----

    fn declare_pattern<'a>(
        &'a self,
        env: &'a Env,
        pattern: &'a Pat,
        value: Value,
        mutable: bool,
        node: Loc,
        initialize: bool,
    ) -> LocalBoxFuture<'a, JsResult<()>> {
        Box::pin(async move {
            match pattern {
                Pat::Ident(i) => {
                    if initialize {
                        self.initialize(env, &i.name, value, node)
                    } else {
                        self.declare(env, &i.name, value, mutable, node)
                    }
                }
                Pat::Assign { left, right, .. } => {
                    let resolved =
                        if value.is_undefined() { self.eval_default(env, left, right).await? } else { value };
                    self.declare_pattern(env, left, resolved, mutable, node, initialize).await
                }
                Pat::Object { properties, loc } => {
                    if value.is_nullish() {
                        return Err(type_error_at(
                            format!("Cannot destructure {} as it is {}.", super::describe_value(&value), coerce_to_string(&value)),
                            *loc,
                        ));
                    }
                    let mut consumed: Vec<PropKey> = Vec::new();
                    for property in properties {
                        match property {
                            ObjectPatMember::Rest { argument, loc: ploc } => {
                                let rest = Obj::ordinary(&self.rt.builtins.object);
                                let source = crate::stdlib::object::enumerable_source(
                                    &self.rt,
                                    "Object destructuring",
                                    &value,
                                    Some(*loc),
                                )?;
                                object::assign(&rest, &source, Some(&consumed))?;
                                self.declare_pattern(env, argument, Value::Object(rest), mutable, *ploc, initialize)
                                    .await?;
                            }
                            ObjectPatMember::Prop(p) => {
                                let key = self.destructuring_key(env, &p.key).await?;
                                consumed.push(key.clone());
                                let item = self.destructured_property(&value, &key, p.loc)?;
                                self.declare_pattern(env, &p.value, item, mutable, p.loc, initialize).await?;
                            }
                        }
                    }
                    Ok(())
                }
                Pat::Array { .. } => {
                    self.destructure_array(env, pattern, value, BindMode::Declare { mutable, initialize }).await
                }
                other => Err(type_error_at(format!("Unsupported binding pattern '{}'.", other.type_name()), other.loc())),
            }
        })
    }

    fn assign_pattern<'a>(
        &'a self,
        env: &'a Env,
        pattern: &'a Pat,
        value: Value,
        node: Loc,
    ) -> LocalBoxFuture<'a, JsResult<()>> {
        Box::pin(async move {
            match pattern {
                Pat::Ident(i) => {
                    self.assign_var(env, &i.name, value, i.loc)?;
                    Ok(())
                }
                Pat::Member(m) => {
                    self.modify_member(env, m, move |_| Box::pin(async move { Ok((true, value.clone(), value)) }))
                        .await?;
                    Ok(())
                }
                Pat::Assign { left, right, .. } => {
                    let resolved =
                        if value.is_undefined() { self.eval_default(env, left, right).await? } else { value };
                    self.assign_pattern(env, left, resolved, node).await
                }
                Pat::Object { properties, loc } => {
                    if value.is_nullish() {
                        return Err(type_error_at(
                            format!("Cannot destructure {} as it is {}.", super::describe_value(&value), coerce_to_string(&value)),
                            *loc,
                        ));
                    }
                    let mut consumed: Vec<PropKey> = Vec::new();
                    for property in properties {
                        match property {
                            ObjectPatMember::Rest { argument, loc: ploc } => {
                                let rest = Obj::ordinary(&self.rt.builtins.object);
                                let source = crate::stdlib::object::enumerable_source(
                                    &self.rt,
                                    "Object destructuring",
                                    &value,
                                    Some(*loc),
                                )?;
                                object::assign(&rest, &source, Some(&consumed))?;
                                self.assign_pattern(env, argument, Value::Object(rest), *ploc).await?;
                            }
                            ObjectPatMember::Prop(p) => {
                                let key = self.destructuring_key(env, &p.key).await?;
                                consumed.push(key.clone());
                                let item = self.destructured_property(&value, &key, p.loc)?;
                                self.assign_pattern(env, &p.value, item, p.loc).await?;
                            }
                        }
                    }
                    Ok(())
                }
                Pat::Array { .. } => self.destructure_array(env, pattern, value, BindMode::Assign).await,
                Pat::Rest { .. } => Err(type_error_at(
                    format!("Unsupported assignment pattern '{}'.", pattern.type_name()),
                    node,
                )),
            }
        })
    }

    async fn eval_default(&self, env: &Env, left: &Pat, right: &Expr) -> JsResult<Value> {
        match left {
            Pat::Ident(i) => self.eval_named(env, right, &i.name).await,
            _ => self.eval(env, right).await,
        }
    }

    fn consume<'a>(&'a self, env: &'a Env, mode: BindMode, target: &'a Pat, item: Value, ctx: Loc) -> LocalBoxFuture<'a, JsResult<()>> {
        match mode {
            BindMode::Declare { mutable, initialize } => self.declare_pattern(env, target, item, mutable, ctx, initialize),
            BindMode::Assign => self.assign_pattern(env, target, item, ctx),
        }
    }

    async fn destructure_array(&self, env: &Env, pattern: &Pat, value: Value, mode: BindMode) -> JsResult<()> {
        let Pat::Array { elements, loc } = pattern else { unreachable!() };
        let cursor = iteration::iterate(&self.rt, &value, Some(*loc), self.site(Some(*loc))).await?;
        let Some(cursor) = cursor else {
            return Err(type_error_at("Array destructuring requires a supported iterable value.", *loc));
        };
        let mut done = false;
        for element in elements {
            if done {
                let Some(element) = element else { continue };
                match element {
                    Pat::Rest { argument, loc: rloc } => {
                        let empty = Value::Object(Obj::array(&self.rt.builtins.array, Vec::new()));
                        self.consume(env, mode, argument, empty, *rloc).await?;
                        return Ok(());
                    }
                    other => {
                        self.consume(env, mode, other, Value::Undefined, other.loc()).await?;
                        continue;
                    }
                }
            }
            let step = cursor.next().await?;
            done = step.done;
            let Some(element) = element else { continue };
            if let Pat::Rest { argument, loc: rloc } = element {
                let mut rest = Vec::new();
                if !step.done {
                    rest.push(step.value);
                }
                while !done {
                    let next = cursor.next().await?;
                    done = next.done;
                    if !done {
                        rest.push(next.value);
                    }
                }
                self.consume(env, mode, argument, Value::Object(Obj::array(&self.rt.builtins.array, rest)), *rloc).await?;
                return Ok(());
            }
            let item = if step.done { Value::Undefined } else { step.value };
            let result = self.consume(env, mode, element, item, *loc).await;
            if step.done {
                result?;
            } else {
                iteration::preserve_consumer_error(&cursor, result).await?;
            }
        }
        if !done {
            cursor.close().await?;
        }
        Ok(())
    }

    async fn destructuring_key(&self, env: &Env, key: &crate::ast::PropKey) -> JsResult<PropKey> {
        match key {
            crate::ast::PropKey::Computed(e) => {
                let v = self.eval(env, e).await?;
                Ok(self.to_property_key(v, e.loc()).await?.prop())
            }
            crate::ast::PropKey::Ident(i) => Ok(PropKey::Str(JsStr::from(&*i.name))),
            crate::ast::PropKey::Lit(l) => Ok(PropKey::Str(match &l.value {
                LitValue::String(s) => JsStr::from_rc(s.clone()),
                LitValue::Number(n) => JsStr::from(crate::number::to_string(*n)),
                LitValue::BigInt(b) => JsStr::from(&**b),
                LitValue::Bool(b) => JsStr::from(if *b { "true" } else { "false" }),
                LitValue::Null => JsStr::from("null"),
                LitValue::Regex { pattern, flags } => JsStr::from(format!("/{pattern}/{flags}")),
            })),
            crate::ast::PropKey::Private(i) => Err(unsupported_syntax("PrivateIdentifier", i.loc)),
        }
    }

    // One destructured property, read the way a member expression would read it.
    fn destructured_property(&self, source: &Value, key: &PropKey, node: Loc) -> JsResult<Value> {
        let ekey = match key {
            PropKey::Str(s) => EvKey::Str(s.clone()),
            PropKey::Sym(s) => EvKey::Sym(*s),
        };
        match self.resolve_property(source, &ekey, node, node)? {
            MemberRef::Tool(t) => Ok(Value::Tool(t)),
            MemberRef::Value(v) => Ok(v),
            MemberRef::Prop { target, key, receiver } => self.read_property(&target, &key, node, &receiver),
        }
    }

    // ---- functions ----

    fn create_function(&self, env: &Env, node: &Rc<Function>, name: Option<&str>) -> Value {
        let b = &self.rt.builtins;
        let name = match name {
            Some(n) => n.to_string(),
            None => match node.kind {
                FunctionKind::Arrow => String::new(),
                _ => node.id.as_ref().map(|i| i.name.to_string()).unwrap_or_default(),
            },
        };
        let length = node
            .params
            .iter()
            .position(|p| matches!(p, Pat::Assign { .. } | Pat::Rest { .. }))
            .unwrap_or(node.params.len());
        let closure = Rc::new(Closure { func: node.clone(), env: RefCell::new(Some(env.clone())) });
        let f = make_function(&b.function, &name, length, FuncImpl::Closure(closure.clone()));
        // Each generator function gets its own prototype, so `g() instanceof g` holds as in JS.
        if node.is_generator {
            let proto = Obj::ordinary(if node.is_async { &b.async_generator } else { &b.generator });
            define(&f, "prototype", Value::Object(proto), HIDDEN);
        }
        // The body of a named function expression sees its own name, read-only.
        if node.kind == FunctionKind::Expression
            && let Some(id) = &node.id
        {
            let own = new_env(Some(env));
            own.set_binding(id.name.clone(), Binding { mutable: false, value: Value::Object(f.clone()), initialized: true });
            *closure.env.borrow_mut() = Some(own);
        }
        Value::Object(f)
    }

    // NamedEvaluation: an anonymous function definition takes the name of what it is assigned to.
    async fn eval_named(&self, env: &Env, node: &Expr, name: &str) -> JsResult<Value> {
        if let Expr::Function(f) = node
            && (f.kind == FunctionKind::Arrow || f.id.is_none())
        {
            return Ok(self.create_function(env, f, Some(name)));
        }
        self.eval(env, node).await
    }

    // ---- calls ----

    /// The single dispatch for every invocation: call expressions and callbacks share it.
    pub fn call<'a>(
        &'a self,
        callable: Value,
        this: Value,
        args: Vec<Value>,
        node: Option<Loc>,
        callee: Option<&'a Expr>,
    ) -> LocalBoxFuture<'a, JsResult<Value>> {
        Box::pin(async move {
            match &callable {
                Value::Tool(t) => {
                    if t.path().is_empty() {
                        return Err(located(type_error("The tools root is not callable."), callee.map(|c| c.loc()).or(node)));
                    }
                    let promise = self.create_tool_call_promise(t.path().to_vec(), args, node)?;
                    return Ok(Value::Object(promise));
                }
                Value::Object(o) if o.is_callable() => {
                    if let Some(c) = o.closure() {
                        let c = c.clone();
                        return self.invoke_function(o.clone(), c, this, args, node).await;
                    }
                    if let Some(n) = o.native_fn() {
                        // A loop made only of built-in calls has no program call or loop back-edge to count.
                        self.rt.interrupt.check()?;
                        let n = n.clone();
                        let site = self.site(node);
                        let result = (n.call)(&self.rt, site, this, args).resolve().await;
                        return result.map_err(|e| locate(e, node));
                    }
                }
                _ => {}
            }
            let name = callee_description(callee).unwrap_or_else(|| "The called value".into());
            Err(located(type_error(format!("{name} is not a function.")), callee.map(|c| c.loc()).or(node)))
        })
    }

    // The call returns its promise at once. Arguments cross on the caller's stack, since `toJSON` is program code, and a
    // promise argument is awaited first; the host call starts once they have crossed, so admission and hooks happen at
    // the call site, and runs concurrently with the program.
    fn create_tool_call_promise(&self, path: Vec<Rc<str>>, args: Vec<Value>, node: Option<Loc>) -> JsResult<Obj> {
        let rt = self.rt.clone();
        let site = self.site(node);
        let made = self.rt.runner.resolving(&self.rt.runner.new_promise(&self.rt)?, None);
        let promise = made.promise.clone();
        let task = async move {
            let mut json = Vec::with_capacity(args.len());
            for arg in args {
                match crate::data::to_boundary(&rt, arg, site).await {
                    Ok(j) => json.push(j),
                    Err(error) => return made.reject(&rt, error),
                }
            }
            let path: Vec<String> = path.iter().map(|s| s.to_string()).collect();
            crate::tool_runtime::start_tool(&rt, crate::tool_runtime::ToolTarget::Path(path), json, made, node);
        };
        self.rt.runner.spawn_async(Box::pin(task), &promise);
        Ok(promise)
    }

    async fn call_arguments(&self, env: &Env, nodes: &[ExprOrSpread]) -> JsResult<Vec<Value>> {
        let mut args = Vec::with_capacity(nodes.len());
        for n in nodes {
            match n {
                ExprOrSpread::Spread { argument, loc } => {
                    let spread = self.eval(env, argument).await?;
                    let cursor = iteration::iterate(&self.rt, &spread, Some(*loc), self.site(Some(*loc))).await?;
                    let Some(cursor) = cursor else {
                        return Err(type_error_at("Spread arguments require a synchronous iterable.", *loc));
                    };
                    loop {
                        let step = cursor.next().await?;
                        if step.done {
                            break;
                        }
                        args.push(step.value);
                    }
                }
                ExprOrSpread::Expr(e) => args.push(self.eval(env, e).await?),
            }
        }
        Ok(args)
    }

    // A callback invoked by a built-in runs below the call that invoked the built-in, so the deeper counts.
    async fn invoke_function(
        &self,
        fn_obj: Obj,
        closure: Rc<Closure>,
        this: Value,
        args: Vec<Value>,
        node: Option<Loc>,
    ) -> JsResult<Value> {
        self.rt.interrupt.check()?;
        let depth = self.depth.get().max(self.ambient.depth) + 1;
        if depth > MAX_CALL_DEPTH {
            // Async functions included: since jobs run to completion (2.1), they no longer overflow the host stack
            // before reaching the limit.
            return Err(located(range_error("Maximum call stack size exceeded"), node));
        }
        let func = closure.func.clone();
        let captured = closure.env.borrow().clone().unwrap_or_else(|| new_env(None));
        let invocation = Rc::new(Frame::new(self.rt.clone(), depth, self.ambient));
        let param_env = new_env(Some(&captured));
        let arrow = func.kind == FunctionKind::Arrow;
        if !arrow {
            param_env.set_binding(Rc::from("this"), Binding { mutable: false, value: this, initialized: true });
        }
        if !arrow && self.uses_arguments(&func) {
            let arguments = Obj::new(Some(self.rt.builtins.object.clone()), ObjKind::Arguments);
            for (i, a) in args.iter().enumerate() {
                define(&arguments, i.to_string().as_str(), a.clone(), object::DATA);
            }
            define(&arguments, "length", Value::Number(args.len() as f64), HIDDEN);
            param_env
                .set_binding(Rc::from("arguments"), Binding { mutable: true, value: Value::Object(arguments), initialized: true });
        }
        // Seed all parameters first so defaults cannot fall through to same-named outer bindings.
        for p in &func.params {
            for name in pattern_names(p) {
                param_env.set_binding(name, Binding { mutable: true, value: Value::Undefined, initialized: false });
            }
        }
        // Bind parameters (inside the promise for async functions, at the call otherwise).
        let bind = {
            let (invocation, param_env, func, rt) = (invocation.clone(), param_env.clone(), func.clone(), self.rt.clone());
            async move {
                let simple: Vec<Option<Rc<str>>> =
                    func.params.iter().map(|p| if let Pat::Ident(i) = p { Some(i.name.clone()) } else { None }).collect();
                for (index, p) in func.params.iter().enumerate() {
                    if let Pat::Rest { argument, loc } = p {
                        let rest = Obj::array(&rt.builtins.array, args.get(index..).map(|s| s.to_vec()).unwrap_or_default());
                        invocation.declare_pattern(&param_env, argument, Value::Object(rest), true, *loc, true).await?;
                        break;
                    }
                    // A sloppy simple parameter list may repeat a name; the last occurrence wins, as in JS.
                    if let Pat::Ident(i) = p {
                        let last = simple.iter().rposition(|n| n.as_ref().map(|n| n == &i.name).unwrap_or(false));
                        if last != Some(index) {
                            continue;
                        }
                    }
                    let value = args.get(index).cloned().unwrap_or(Value::Undefined);
                    invocation.declare_pattern(&param_env, p, value, true, p.loc(), true).await?;
                }
                Ok::<(), Thrown>(())
            }
        };
        let body_frame = invocation.clone();
        let body_env = param_env.clone();
        let body_func = func.clone();
        let body = async move {
            match &body_func.body {
                FunctionBody::Block(b) => {
                    let var_env = new_env(Some(&body_env));
                    body_frame.hoist_vars(&var_env, &b.body, Some(&body_env));
                    match body_frame.block(&var_env, b).await? {
                        Completion::Return(v) => Ok(v),
                        _ => Ok(Value::Undefined),
                    }
                }
                FunctionBody::Expr(e) => body_frame.eval(&body_env, e).await,
            }
        };
        // Generators bind parameters at the call and defer only the body to the first `next()`.
        if func.is_generator {
            bind.await?;
            return Ok(self.create_generator(invocation, Box::pin(body), &fn_obj, func.is_async));
        }
        if !func.is_async {
            bind.await?;
            return body.await;
        }
        // The body runs now, nested in the caller's job, until it first suspends; its completion settles the promise.
        let made = super::promises::capability(&self.rt, None)?;
        let promise = made.promise.clone();
        let rt = self.rt.clone();
        let task = async move {
            let completion = async move {
                bind.await?;
                body.await
            }
            .await;
            made.settle_with(&rt, completion);
        };
        self.rt.runner.spawn_async(Box::pin(task), &promise);
        Ok(Value::Object(promise))
    }

    fn uses_arguments(&self, func: &Rc<Function>) -> bool {
        let key = Rc::as_ptr(func) as usize;
        if let Some(v) = self.rt.arguments_use.borrow().get(&key) {
            return *v;
        }
        let found = uses_arguments(func);
        self.rt.arguments_use.borrow_mut().insert(key, found);
        found
    }

    // ---- generators ----

    fn create_generator(
        &self,
        invocation: Rc<Frame>,
        run: LocalBoxFuture<'static, JsResult<Value>>,
        fn_obj: &Obj,
        asynchronous: bool,
    ) -> Value {
        // The body a generator object resumes: a `return()` request ends it with the returned value.
        let body: LocalBoxFuture<'static, JsResult<Value>> = Box::pin(async move {
            match run.await {
                Err(Thrown::GeneratorReturn(v)) => Ok(v),
                other => other,
            }
        });
        let handle = Rc::new(GeneratorHandle::new(asynchronous, body, invocation.clone()));
        *invocation.generator.borrow_mut() = Some(handle.clone());
        invocation.gen_async.set(asynchronous);
        let proto = match object::get(fn_obj, &PropKey::str("prototype")) {
            Ok(Value::Object(p)) => p,
            _ if asynchronous => self.rt.builtins.async_generator.clone(),
            _ => self.rt.builtins.generator.clone(),
        };
        Value::Object(Obj::new(Some(proto), ObjKind::Generator(handle)))
    }

    // Yield `value` and continue with the request that resumes the body: `next` supplies the value of the `yield`,
    // `throw` throws there, and `return` unwinds through `finally` blocks (an async generator awaits the value first).
    async fn suspend_generator(&self, value: Value) -> JsResult<Value> {
        let generator = self.generator.borrow().clone();
        let Some(generator) = generator else { return Err(type_error("yield is only valid inside a generator.")) };
        let request = suspend_body(&self.rt, &generator, value).await;
        match request.kind {
            ReqKind::Next => Ok(request.value),
            ReqKind::Throw => Err(Thrown::Value(request.value)),
            ReqKind::Return => {
                if self.gen_async.get() {
                    Err(Thrown::GeneratorReturn(self.await_value(request.value, None).await?))
                } else {
                    Err(Thrown::GeneratorReturn(request.value))
                }
            }
        }
    }

    async fn yield_expr(&self, env: &Env, argument: &Option<Box<Expr>>, delegate: bool, node: Loc) -> JsResult<Value> {
        if self.generator.borrow().is_none() {
            return Err(type_error_at("yield is only valid inside a generator.", node));
        }
        let value = match argument {
            Some(a) => self.eval(env, a).await?,
            None => Value::Undefined,
        };
        if delegate {
            return self.delegate_yield(value, node).await;
        }
        let yielded = if self.gen_async.get() { self.await_value(value, Some(node)).await? } else { value };
        self.suspend_generator(yielded).await
    }

    async fn delegate_yield(&self, value: Value, node: Loc) -> JsResult<Value> {
        let asynchronous = self.gen_async.get();
        let builtin = builtin_cursor(&self.rt, &value);
        if let Some(cursor) = &builtin
            && !asynchronous
        {
            loop {
                let step = cursor.next().await?;
                if step.done {
                    return Ok(Value::Undefined);
                }
                match self.suspend_generator(step.value).await {
                    Ok(_) => continue,
                    Err(e @ Thrown::GeneratorReturn(_)) => {
                        cursor.close().await?;
                        return Err(e);
                    }
                    Err(Thrown::Value(_)) => {
                        cursor.close().await?;
                        return Err(type_error_at("The delegated iterator does not provide a throw() method.", node));
                    }
                    Err(e) => return Err(e),
                }
            }
        }
        let site = self.site(Some(node));
        let custom = if builtin.is_none() {
            iteration::custom_iterator(&self.rt, &value, Some(node), asynchronous, site).await?
        } else {
            None
        };
        if builtin.is_none() && custom.is_none() {
            return Err(type_error_at("yield* requires a compatible iterable value.", node));
        }
        // An async generator delegates to a synchronous iterator through CreateAsyncFromSyncIterator, as JS does.
        let iterator = if asynchronous && !custom.as_ref().is_some_and(|c| c.asynchronous) {
            iteration::async_from_sync(&self.rt, builtin, custom, Some(node))
        } else {
            custom.unwrap()
        };
        let mut kind = ReqKind::Next;
        let mut input = Value::Undefined;
        loop {
            let method = match kind {
                ReqKind::Next => iterator.next.clone(),
                ReqKind::Return => object::get(&iterator.iterator, &PropKey::str("return"))?,
                ReqKind::Throw => object::get(&iterator.iterator, &PropKey::str("throw"))?,
            };
            if method.is_nullish() {
                if kind == ReqKind::Return {
                    let input = if asynchronous { self.await_value(input, Some(node)).await? } else { input };
                    return Err(Thrown::GeneratorReturn(input));
                }
                iteration::close_iterator(&self.rt, &iterator, Some(node), site).await?;
                return Err(type_error_at("The delegated iterator does not provide a throw() method.", node));
            }
            let label = match kind {
                ReqKind::Next => "next",
                ReqKind::Return => "return",
                ReqKind::Throw => "throw",
            };
            let method = iteration::require_iterator_method(method, &format!("Iterator {label}"), Some(node))?;
            let called = self
                .call(method, Value::Object(iterator.iterator.clone()), vec![input.clone()], Some(node), None)
                .await?;
            let called = if iterator.asynchronous { self.await_value(called, Some(node)).await? } else { called };
            let result = iteration::require_iterator_object(called, &format!("Iterator {label}() result"), Some(node))?;
            let done = object::get(&result, &PropKey::str("done"))?.truthy();
            let result_value = object::get(&result, &PropKey::str("value"))?;
            if done {
                if kind == ReqKind::Return {
                    return Err(Thrown::GeneratorReturn(result_value));
                }
                return Ok(result_value);
            }
            // Delegated values are yielded as they are; an async generator's inner promise already settled them.
            match self.suspend_generator(result_value).await {
                Ok(v) => {
                    kind = ReqKind::Next;
                    input = v;
                }
                Err(Thrown::GeneratorReturn(v)) => {
                    kind = ReqKind::Return;
                    input = v;
                }
                Err(Thrown::Value(v)) => {
                    kind = ReqKind::Throw;
                    input = v;
                }
                Err(e) => return Err(e),
            }
        }
    }

    // ---- expressions ----

    pub fn eval<'a>(&'a self, env: &'a Env, node: &'a Expr) -> LocalBoxFuture<'a, JsResult<Value>> {
        // Cheap nodes skip allocation; every other kind boxes only its own state.
        let ready = |r: JsResult<Value>| -> LocalBoxFuture<'a, JsResult<Value>> { Box::pin(std::future::ready(r)) };
        match node {
            Expr::Lit(l) => match &l.value {
                LitValue::Regex { pattern, flags } => {
                    ready(crate::stdlib::regexp::construct_regexp_literal(&self.rt, pattern, flags).map(Value::Object))
                }
                _ => ready(literal(l)),
            },
            Expr::Ident(i) => ready(self.lookup(env, &i.name, i.loc)),
            Expr::This(loc) => ready(self.lookup(env, "this", *loc)),
            Expr::Binary { op, left, right, loc } => Box::pin(self.binary(env, *op, left, right, *loc)),
            Expr::Logical { op, left, right, .. } => Box::pin(async move {
                let l = self.eval(env, left).await?;
                let take_left = match op {
                    LogicalOp::And => !l.truthy(),
                    LogicalOp::Or => l.truthy(),
                    LogicalOp::Nullish => !l.is_nullish(),
                };
                if take_left { Ok(l) } else { self.eval(env, right).await }
            }),
            Expr::Unary { op, argument, loc } => Box::pin(self.unary(env, *op, argument, *loc)),
            Expr::Assign { op, left, right, loc } => Box::pin(self.assign(env, *op, left, right, *loc)),
            Expr::Sequence { expressions, .. } => Box::pin(async move {
                let mut result = Value::Undefined;
                for e in expressions {
                    result = self.eval(env, e).await?;
                }
                Ok(result)
            }),
            Expr::Call { callee, arguments, optional, loc } => {
                Box::pin(self.call_expr(env, callee, arguments, *optional, *loc))
            }
            Expr::Function(f) => ready(Ok(self.create_function(env, f, None))),
            Expr::Member(m) => Box::pin(self.read_member(env, m)),
            Expr::Chain { expression, .. } => Box::pin(async move {
                match self.eval(env, expression).await {
                    Err(Thrown::ShortCircuit) => Ok(Value::Undefined),
                    other => other,
                }
            }),
            Expr::Object { properties, .. } => Box::pin(self.object_expr(env, properties)),
            Expr::Array { elements, .. } => Box::pin(self.array_expr(env, elements)),
            Expr::Template(t) => Box::pin(self.template(env, t)),
            Expr::TaggedTemplate { tag, quasi, loc } => Box::pin(self.tagged_template(env, tag, quasi, *loc)),
            Expr::Conditional { test, consequent, alternate, .. } => Box::pin(async move {
                if self.eval(env, test).await?.truthy() {
                    self.eval(env, consequent).await
                } else {
                    self.eval(env, alternate).await
                }
            }),
            Expr::Update { op, prefix, argument, loc } => Box::pin(self.update(env, *op, *prefix, argument, *loc)),
            // Await always suspends, including for plain values.
            Expr::Await { argument, loc } => Box::pin(async move {
                let value = self.eval(env, argument).await?;
                self.await_value(value, Some(*loc)).await
            }),
            Expr::Yield { argument, delegate, loc } => Box::pin(self.yield_expr(env, argument, *delegate, *loc)),
            Expr::New { callee, arguments, loc } => Box::pin(self.new_expr(env, callee, arguments, *loc)),
            Expr::Super(loc) => ready(Err(unsupported_syntax("Super", *loc))),
            Expr::PrivateName(i) => ready(Err(unsupported_syntax("PrivateIdentifier", i.loc))),
            Expr::Unsupported { kind, loc } => ready(Err(unsupported_syntax(kind, *loc))),
        }
    }

    async fn new_expr(&self, env: &Env, callee: &Expr, arguments: &[ExprOrSpread], loc: Loc) -> JsResult<Value> {
        let value = self.eval(env, callee).await?;
        let construct = match &value {
            Value::Object(o) => o.native_fn().and_then(|n| n.construct.clone()),
            _ => None,
        };
        let Some(construct) = construct else {
            let name = callee_description(Some(callee)).unwrap_or_else(|| "The called value".into());
            let message = match &value {
                Value::Object(o) if o.closure().is_some() => format!(
                    "{name} cannot be constructed: user-defined constructors and classes are not supported. Call it as a function that returns a plain object instead."
                ),
                Value::Object(o) if o.native_fn().is_some() => {
                    format!("new {name}(...) is not supported; call {name}(...) without new instead.")
                }
                _ => format!("{name} is not a constructor."),
            };
            return Err(type_error_at(message, loc));
        };
        let args = self.call_arguments(env, arguments).await?;
        let Value::Object(target) = value else { unreachable!() };
        let site = self.site(Some(loc));
        construct(&self.rt, site, args, target).resolve().await.map_err(|e| locate(e, Some(loc)))
    }

    async fn binary(&self, env: &Env, op: BinaryOp, left: &Expr, right: &Expr, node: Loc) -> JsResult<Value> {
        if let Expr::PrivateName(i) = left {
            return Err(unsupported_syntax("PrivateIdentifier", i.loc));
        }
        let lhs = self.eval(env, left).await?;
        let rhs = self.eval(env, right).await?;
        if op == BinaryOp::InstanceOf {
            return instanceof_value(&lhs, &rhs, node).map(Value::Bool);
        }
        if matches!(lhs, Value::Object(_)) || matches!(rhs, Value::Object(_)) {
            return Box::pin(self.apply_operator(op, lhs, rhs, node)).await;
        }
        apply_binary_operator(op, &lhs, &rhs, node)
    }

    async fn to_primitive_at(&self, value: &Value, hint: Hint, node: Loc) -> JsResult<Value> {
        to_primitive(&self.rt, value, hint, self.site(Some(node))).await.map_err(|e| locate(e, Some(node)))
    }

    // Arithmetic, relational, and bitwise operators convert both operands first, left then right.
    async fn apply_operator(&self, op: BinaryOp, lhs: Value, rhs: Value, node: Loc) -> JsResult<Value> {
        let l_obj = matches!(lhs, Value::Object(_));
        let r_obj = matches!(rhs, Value::Object(_));
        if !(l_obj || r_obj) {
            return apply_binary_operator(op, &lhs, &rhs, node);
        }
        let equality = matches!(op, BinaryOp::Eq | BinaryOp::NotEq);
        if op == BinaryOp::In
            && let (Value::Object(l), Value::Object(r)) = (&lhs, &rhs)
            && !l.is_opaque()
        {
            let key = self.to_property_key(lhs.clone(), node).await?;
            return Ok(Value::Bool(has(r, &key.prop())));
        }
        let other = if l_obj { &rhs } else { &lhs };
        let other_primitive = !matches!(other, Value::Undefined | Value::Null | Value::Object(_) | Value::Tool(_));
        let converts = is_primitive_operator(op) || (equality && other_primitive);
        if !converts {
            return apply_binary_operator(op, &lhs, &rhs, node);
        }
        let hint = if op == BinaryOp::Add || equality { Hint::Default } else { Hint::Number };
        let l = self.to_primitive_at(&lhs, hint, node).await?;
        let r = self.to_primitive_at(&rhs, hint, node).await?;
        apply_binary_operator(op, &l, &r, node)
    }

    async fn unary(&self, env: &Env, op: UnaryOp, argument: &Expr, node: Loc) -> JsResult<Value> {
        if op == UnaryOp::Delete {
            return self.delete(env, argument).await;
        }
        // Undeclared names short-circuit, but declared TDZ bindings must still throw.
        if op == UnaryOp::Typeof
            && let Expr::Ident(i) = argument
            && env.lookup(&i.name).is_none()
        {
            return Ok(Value::str("undefined"));
        }
        let value = self.eval(env, argument).await?;
        match op {
            UnaryOp::Typeof => return Ok(Value::str(value.js_typeof())),
            UnaryOp::Not => return Ok(Value::Bool(!value.truthy())),
            UnaryOp::Void => return Ok(Value::Undefined),
            _ => {}
        }
        let operand = self.to_primitive_at(&value, Hint::Number, node).await?;
        if super::references::is_opaque(&operand) {
            return Err(invalid_data_at("Unary operators require data values.", node));
        }
        let n = coerce_to_number(&operand);
        Ok(Value::Number(match op {
            UnaryOp::Plus => n,
            UnaryOp::Minus => -n,
            UnaryOp::BitNot => !crate::number::to_int32(n) as f64,
            _ => unreachable!(),
        }))
    }

    async fn assign(&self, env: &Env, op: AssignOp, left: &Pat, right: &Expr, node: Loc) -> JsResult<Value> {
        if let AssignOp::Logical(lop) = op {
            return self.logical_assign(env, lop, left, right).await;
        }
        if op == AssignOp::Assign && matches!(left, Pat::Object { .. } | Pat::Array { .. }) {
            let value = self.eval(env, right).await?;
            self.assign_pattern(env, left, value.clone(), node).await?;
            return Ok(value);
        }
        match left {
            Pat::Ident(i) => {
                if let AssignOp::Compound(bop) = op {
                    let current = self.lookup(env, &i.name, i.loc)?;
                    let r = self.eval(env, right).await?;
                    let next = if matches!(current, Value::Object(_)) || matches!(r, Value::Object(_)) {
                        Box::pin(self.apply_operator(bop, current, r, node)).await?
                    } else {
                        apply_binary_operator(bop, &current, &r, node)?
                    };
                    return self.assign_var(env, &i.name, next, i.loc);
                }
                let value = self.eval_named(env, right, &i.name).await?;
                self.assign_var(env, &i.name, value, i.loc)
            }
            Pat::Member(m) => {
                self.modify_member(env, m, move |current| {
                    Box::pin(async move {
                        let r = self.eval(env, right).await?;
                        match op {
                            AssignOp::Compound(bop) => {
                                let next = self.apply_operator(bop, current, r, node).await?;
                                Ok((true, next.clone(), next))
                            }
                            _ => Ok((true, r.clone(), r)),
                        }
                    })
                })
                .await
            }
            other => Err(type_error_at("Assignment target must be an Identifier or MemberExpression.", other.loc())),
        }
    }

    async fn logical_assign(&self, env: &Env, op: LogicalOp, left: &Pat, right: &Expr) -> JsResult<Value> {
        let should = move |current: &Value| match op {
            LogicalOp::Nullish => current.is_nullish(),
            LogicalOp::Or => !current.truthy(),
            LogicalOp::And => current.truthy(),
        };
        match left {
            Pat::Ident(i) => {
                let current = self.lookup(env, &i.name, i.loc)?;
                if !should(&current) {
                    return Ok(current);
                }
                let value = self.eval_named(env, right, &i.name).await?;
                self.assign_var(env, &i.name, value, i.loc)
            }
            Pat::Member(m) => {
                self.modify_member(env, m, move |current| {
                    Box::pin(async move {
                        if should(&current) {
                            let r = self.eval(env, right).await?;
                            Ok((true, r.clone(), r))
                        } else {
                            Ok((false, current.clone(), current))
                        }
                    })
                })
                .await
            }
            other => Err(type_error_at("Assignment target must be an Identifier or MemberExpression.", other.loc())),
        }
    }

    async fn update(&self, env: &Env, op: UpdateOp, prefix: bool, argument: &Expr, _node: Loc) -> JsResult<Value> {
        let increment = if op == UpdateOp::Increment { 1.0 } else { -1.0 };
        let symbol = if op == UpdateOp::Increment { "++" } else { "--" };
        let operand = move |current: &Value, loc: Loc| -> JsResult<f64> {
            if super::references::is_opaque(current) {
                return Err(invalid_data_at(format!("'{symbol}' requires a data value."), loc));
            }
            Ok(coerce_to_number(current))
        };
        match argument {
            Expr::Ident(i) => {
                let current = self.lookup(env, &i.name, i.loc)?;
                let current = if matches!(current, Value::Object(_)) {
                    self.to_primitive_at(&current, Hint::Number, i.loc).await?
                } else {
                    current
                };
                let before = operand(&current, i.loc)?;
                let next = before + increment;
                self.assign_var(env, &i.name, Value::Number(next), i.loc)?;
                Ok(Value::Number(if prefix { next } else { before }))
            }
            Expr::Member(m) => {
                let loc = m.loc;
                self.modify_member(env, m, move |current| {
                    Box::pin(async move {
                        let primitive = self.to_primitive_at(&current, Hint::Number, loc).await?;
                        let value = operand(&primitive, loc)?;
                        let next = value + increment;
                        Ok((true, Value::Number(next), Value::Number(if prefix { next } else { value })))
                    })
                })
                .await
            }
            other => Err(type_error_at("Update target must be an Identifier or MemberExpression.", other.loc())),
        }
    }

    // EvaluateCall: a member callee supplies its base object as `this`; anything else calls with undefined.
    async fn call_expr(
        &self,
        env: &Env,
        callee: &Expr,
        arguments: &[ExprOrSpread],
        optional: bool,
        node: Loc,
    ) -> JsResult<Value> {
        if let Expr::Super(loc) = callee {
            return Err(unsupported_syntax("Super", *loc));
        }
        let (callable, this) = match callee {
            Expr::Member(m) => self.read_method(env, m).await?,
            other => (self.eval(env, other).await?, Value::Undefined),
        };
        if callable.is_nullish() && optional {
            return Err(Thrown::ShortCircuit);
        }
        let args = self.call_arguments(env, arguments).await?;
        self.call(callable, this, args, Some(node), Some(callee)).await
    }

    async fn read_method(&self, env: &Env, node: &Member) -> JsResult<(Value, Value)> {
        Ok(match self.member_reference(env, node).await? {
            MemberRef::Tool(t) => (Value::Tool(t), Value::Undefined),
            MemberRef::Value(v) => (v, Value::Undefined),
            MemberRef::Prop { target, key, receiver } => {
                (self.read_reference(&target, &key, node, &receiver)?, receiver)
            }
        })
    }

    async fn object_expr(&self, env: &Env, properties: &[ObjectMember]) -> JsResult<Value> {
        let object = Obj::ordinary(&self.rt.builtins.object);
        for property in properties {
            match property {
                ObjectMember::Spread { argument, loc } => {
                    let spread = self.eval(env, argument).await?;
                    if spread.is_nullish() {
                        continue;
                    }
                    let source =
                        crate::stdlib::object::enumerable_source(&self.rt, "Object spread", &spread, Some(*loc))?;
                    object::assign(&object, &source, None)?;
                }
                ObjectMember::Prop(p) => {
                    if p.kind != PropKind::Init {
                        return Err(type_error_at("Only init object properties are supported.", p.loc));
                    }
                    let key = match &p.key {
                        crate::ast::PropKey::Computed(e) => {
                            let v = self.eval(env, e).await?;
                            self.to_property_key(v, e.loc()).await?
                        }
                        crate::ast::PropKey::Ident(i) => EvKey::Str(JsStr::from(&*i.name)),
                        crate::ast::PropKey::Lit(l) => literal_key(l)?,
                        crate::ast::PropKey::Private(i) => {
                            return Err(type_error_at("Unsupported object property key shape.", i.loc));
                        }
                    };
                    let name = match &key {
                        EvKey::Sym(Sym::Iterator) => "[Symbol.iterator]".to_string(),
                        EvKey::Sym(Sym::AsyncIterator) => "[Symbol.asyncIterator]".to_string(),
                        other => other.prop().display(),
                    };
                    let value = self.eval_named(env, &p.value, &name).await?;
                    define(&object, key.prop(), value, object::DATA);
                }
            }
        }
        Ok(Value::Object(object))
    }

    async fn array_expr(&self, env: &Env, elements: &[Option<ExprOrSpread>]) -> JsResult<Value> {
        let mut values: Vec<Option<Value>> = Vec::with_capacity(elements.len());
        for e in elements {
            match e {
                None => values.push(None),
                Some(ExprOrSpread::Spread { argument, loc }) => {
                    let spread = self.eval(env, argument).await?;
                    let cursor = iteration::iterate(&self.rt, &spread, Some(*loc), self.site(Some(*loc))).await?;
                    let Some(cursor) = cursor else {
                        return Err(type_error_at("Array spread requires a synchronous iterable.", *loc));
                    };
                    loop {
                        let step = cursor.next().await?;
                        if step.done {
                            break;
                        }
                        values.push(Some(step.value));
                    }
                }
                Some(ExprOrSpread::Expr(e)) => values.push(Some(self.eval(env, e).await?)),
            }
        }
        Ok(Value::Object(Obj::array_holes(&self.rt.builtins.array, values)))
    }

    async fn template(&self, env: &Env, t: &Template) -> JsResult<Value> {
        let mut output: Vec<u16> = Vec::new();
        for (index, quasi) in t.quasis.iter().enumerate() {
            let Some(cooked) = &quasi.cooked else {
                return Err(type_error_at("Invalid template literal quasi.", quasi.loc));
            };
            output.extend_from_slice(cooked);
            check_string_length(output.len())?;
            if let Some(e) = t.expressions.get(index) {
                let raw = self.eval(env, e).await?;
                let p = self.to_primitive_at(&raw, Hint::String, e.loc()).await?;
                output.extend_from_slice(coerce_to_string(&p).units());
                check_string_length(output.len())?;
            }
        }
        Ok(Value::String(JsStr::from_units(output)))
    }

    async fn tagged_template(&self, env: &Env, tag: &Expr, quasi: &Template, node: Loc) -> JsResult<Value> {
        let (callable, this) = match tag {
            Expr::Member(m) => self.read_method(env, m).await?,
            other => (self.eval(env, other).await?, Value::Undefined),
        };
        let key = quasi as *const Template as usize;
        let existing = self.rt.templates.borrow().get(&key).cloned();
        let strings = match existing {
            Some(s) => s,
            None => {
                let array = &self.rt.builtins.array;
                let cooked: Vec<Value> = quasi
                    .quasis
                    .iter()
                    .map(|q| q.cooked.as_ref().map(|c| Value::String(JsStr::from_rc(c.clone()))).unwrap_or_default())
                    .collect();
                let strings = Obj::array(array, cooked);
                let raw: Vec<Value> = quasi.quasis.iter().map(|q| Value::String(JsStr::from_rc(q.raw.clone()))).collect();
                define(&strings, "raw", Value::Object(Obj::array(array, raw)), FROZEN);
                self.rt.templates.borrow_mut().insert(key, strings.clone());
                strings
            }
        };
        let mut args = vec![Value::Object(strings)];
        for e in &quasi.expressions {
            args.push(self.eval(env, e).await?);
        }
        self.call(callable, this, args, Some(node), Some(tag)).await
    }

    async fn member_reference(&self, env: &Env, node: &Member) -> JsResult<MemberRef> {
        if let Expr::Super(loc) = &*node.object {
            return Err(unsupported_syntax("Super", *loc));
        }
        if let MemberProp::Private(i) = &node.property {
            return Err(unsupported_syntax("PrivateIdentifier", i.loc));
        }
        let object_value = self.eval(env, &node.object).await?;
        if object_value.is_nullish() && node.optional {
            return Err(Thrown::ShortCircuit);
        }
        let (key_value, property_loc) = match &node.property {
            MemberProp::Ident(i) => (Value::String(JsStr::from(&*i.name)), i.loc),
            MemberProp::Computed(e) => (self.eval(env, e).await?, e.loc()),
            MemberProp::Private(_) => unreachable!(),
        };
        // GetValue applies ToObject to the base before ToPropertyKey.
        if object_value.is_nullish() {
            return Err(type_error_at(
                format!(
                    "Cannot read properties of {} (reading '{}').",
                    coerce_to_string(&object_value),
                    coerce_to_string(&key_value)
                ),
                node.object.loc(),
            ));
        }
        let key = self.to_property_key(key_value, property_loc).await?;
        self.resolve_property(&object_value, &key, node.object.loc(), property_loc)
    }

    fn resolve_property(&self, object_value: &Value, key: &EvKey, object_node: Loc, property_node: Loc) -> JsResult<MemberRef> {
        match object_value {
            Value::Tool(t) => {
                let EvKey::Str(s) = key else {
                    return Err(type_error_at("Tool paths must use string property names.", property_node));
                };
                let mut path = t.path().to_vec();
                path.push(Rc::from(s.to_rust().as_str()));
                Ok(MemberRef::Tool(ToolRef::new(path)))
            }
            Value::Object(o) => Ok(MemberRef::Prop { target: o.clone(), key: key.prop(), receiver: object_value.clone() }),
            Value::Undefined | Value::Null => Err(type_error_at(
                format!("Cannot read properties of {} (reading '{}').", coerce_to_string(object_value), key.prop().display()),
                object_node,
            )),
            _ => {
                // Strings own length and indexes; every other primitive reads through its wrapper prototype.
                if let Value::String(s) = object_value {
                    let pk = key.prop();
                    if let PropKey::Str(k) = &pk {
                        if k.eq_str("length") {
                            return Ok(MemberRef::Value(Value::Number(s.len() as f64)));
                        }
                        if let Some(i) = object::parse_array_index(k) {
                            return Ok(MemberRef::Value(if i < s.len() {
                                Value::String(s.slice(i, i + 1))
                            } else {
                                Value::Undefined
                            }));
                        }
                    }
                }
                match self.rt.builtins.primitive_prototype(object_value) {
                    Some(proto) => {
                        Ok(MemberRef::Prop { target: proto.clone(), key: key.prop(), receiver: object_value.clone() })
                    }
                    None => Err(type_error_at("Cannot access a property on a non-object value.", object_node)),
                }
            }
        }
    }

    fn read_reference(&self, target: &Obj, key: &PropKey, node: &Member, receiver: &Value) -> JsResult<Value> {
        // Reject unknown promise properties so a missing await cannot hide.
        if target.is_promise() && !has(target, key) {
            return Err(invalid_data_at(
                "This value is an un-awaited Promise; await it first - e.g. `const result = await tools.ns.tool(...)`.",
                node.object.loc(),
            ));
        }
        self.read_property(target, key, node.loc, receiver)
    }

    fn read_property(&self, target: &Obj, key: &PropKey, node: Loc, receiver: &Value) -> JsResult<Value> {
        get_with_receiver(target, key, receiver).map_err(|e| locate(e, Some(node)))
    }

    async fn read_member(&self, env: &Env, node: &Member) -> JsResult<Value> {
        match self.member_reference(env, node).await? {
            MemberRef::Tool(t) => Ok(Value::Tool(t)),
            MemberRef::Value(v) => Ok(v),
            MemberRef::Prop { target, key, receiver } => self.read_reference(&target, &key, node, &receiver),
        }
    }

    async fn delete(&self, env: &Env, argument: &Expr) -> JsResult<Value> {
        let target = match argument {
            Expr::Chain { expression, .. } => &**expression,
            other => other,
        };
        if let Expr::Ident(_) = target {
            return Err(type_error_at("Only data fields may be deleted.", argument.loc()));
        }
        let Expr::Member(m) = target else {
            self.eval(env, target).await?;
            return Ok(Value::Bool(true));
        };
        let reference = match self.member_reference(env, m).await {
            Err(Thrown::ShortCircuit) => return Ok(Value::Bool(true)),
            other => other?,
        };
        match reference {
            MemberRef::Prop { target: t, key, receiver } if matches!(&receiver, Value::Object(r) if Obj::ptr_eq(r, &t)) => {
                if remove(&t, &key) {
                    Ok(Value::Bool(true))
                } else {
                    Err(type_error_at(format!("Cannot delete property '{}'.", key.display()), m.loc))
                }
            }
            _ => Err(invalid_data_at("Only data fields may be deleted.", m.loc)),
        }
    }

    // Resolve side-effecting object and key expressions exactly once.
    async fn modify_member<'a>(
        &'a self,
        env: &'a Env,
        node: &'a Member,
        compute: impl FnOnce(Value) -> LocalBoxFuture<'a, JsResult<(bool, Value, Value)>>,
    ) -> JsResult<Value> {
        let reference = match self.member_reference(env, node).await {
            Err(Thrown::ShortCircuit) => return Err(type_error_at("Only data fields may be assigned.", node.loc)),
            other => other?,
        };
        let MemberRef::Prop { target, key, receiver } = reference else {
            return Err(type_error_at("Only data fields may be assigned.", node.loc));
        };
        if !matches!(&receiver, Value::Object(r) if Obj::ptr_eq(r, &target)) {
            return Err(type_error_at(
                format!(
                    "Cannot create property '{}' on {} '{}'.",
                    key.display(),
                    receiver.js_typeof(),
                    coerce_to_string(&receiver)
                ),
                node.loc,
            ));
        }
        let current = self.read_reference(&target, &key, node, &receiver)?;
        let (write, next, result) = compute(current).await?;
        if write {
            self.assign_to_reference(&target, &key, next, node.loc)?;
        }
        Ok(result)
    }

    fn assign_to_reference(&self, target: &Obj, key: &PropKey, next: Value, node: Loc) -> JsResult<()> {
        let label = if target.is_array() { "Array assignment result" } else { "Object assignment result" };
        let written = reject_circular_insertion(target, &next, label)
            .and_then(|_| set(target, key, next))
            .map_err(|e| locate(e, Some(node)))?;
        if written {
            return Ok(());
        }
        Err(type_error_at(format!("Cannot assign to read only property '{}'.", key.display()), node))
    }

    // ToPropertyKey: a data object converts through its own `toString`/`valueOf` first.
    async fn to_property_key(&self, value: Value, node: Loc) -> JsResult<EvKey> {
        if !matches!(value, Value::Object(_)) {
            return Ok(primitive_key(&value));
        }
        let p = self.to_primitive_at(&value, Hint::String, node).await?;
        Ok(primitive_key(&p))
    }
}

fn located(e: Thrown, node: Option<Loc>) -> Thrown {
    locate(e, node)
}

fn is_primitive_operator(op: BinaryOp) -> bool {
    use BinaryOp::*;
    matches!(op, Add | Sub | Mul | Div | Mod | Exp | Lt | LtEq | Gt | GtEq | BitAnd | BitOr | BitXor | Shl | Shr | UShr)
}

/// IsLooselyEqual on values (objects by identity, nullish never coerces the other side).
fn loosely_equal(lhs: &Value, rhs: &Value, node: Loc) -> JsResult<bool> {
    let l_obj = matches!(lhs, Value::Object(_) | Value::Tool(_));
    let r_obj = matches!(rhs, Value::Object(_) | Value::Tool(_));
    if l_obj == r_obj {
        return Ok(if l_obj { lhs.strict_equals(rhs) } else { primitive_loose_equal(lhs, rhs) });
    }
    let primitive = if l_obj { rhs } else { lhs };
    if primitive.is_nullish() {
        return Ok(false);
    }
    Err(invalid_data_at("Binary operators require data values.", node))
}

fn primitive_loose_equal(a: &Value, b: &Value) -> bool {
    match (a, b) {
        (Value::Undefined | Value::Null, Value::Undefined | Value::Null) => true,
        (Value::Undefined | Value::Null, _) | (_, Value::Undefined | Value::Null) => false,
        (Value::Number(x), Value::String(_)) => *x == coerce_to_number(b),
        (Value::String(_), Value::Number(y)) => coerce_to_number(a) == *y,
        (Value::Bool(_), _) => primitive_loose_equal(&Value::Number(coerce_to_number(a)), b),
        (_, Value::Bool(_)) => primitive_loose_equal(a, &Value::Number(coerce_to_number(b))),
        _ => a.strict_equals(b),
    }
}

/// Binary operators on primitives (or opaque values, which only the identity operators accept).
pub fn apply_binary_operator(op: BinaryOp, lhs: &Value, rhs: &Value, node: Loc) -> JsResult<Value> {
    use BinaryOp::*;
    match op {
        StrictEq => return Ok(Value::Bool(lhs.strict_equals(rhs))),
        StrictNotEq => return Ok(Value::Bool(!lhs.strict_equals(rhs))),
        Eq => return loosely_equal(lhs, rhs, node).map(Value::Bool),
        NotEq => return loosely_equal(lhs, rhs, node).map(|b| Value::Bool(!b)),
        _ => {}
    }
    if op == In
        && let Value::Object(r) = rhs
        && !super::references::is_opaque(lhs)
    {
        return Ok(Value::Bool(has(r, &primitive_key(lhs).prop())));
    }
    if super::references::is_opaque(lhs) || super::references::is_opaque(rhs) {
        return Err(invalid_data_at("Binary operators require data values.", node));
    }
    let num = |v: &Value| coerce_to_number(v);
    Ok(match op {
        Add => {
            if matches!(lhs, Value::String(_)) || matches!(rhs, Value::String(_)) {
                let l = coerce_to_string(lhs);
                let r = coerce_to_string(rhs);
                check_string_length(l.len() + r.len())?;
                Value::String(l.concat(&r))
            } else {
                Value::Number(num(lhs) + num(rhs))
            }
        }
        Sub => Value::Number(num(lhs) - num(rhs)),
        Mul => Value::Number(num(lhs) * num(rhs)),
        Div => Value::Number(num(lhs) / num(rhs)),
        Mod => Value::Number(js_mod(num(lhs), num(rhs))),
        Exp => Value::Number(js_pow(num(lhs), num(rhs))),
        Lt | LtEq | Gt | GtEq => Value::Bool(compare(op, lhs, rhs)),
        BitAnd => Value::Number((crate::number::to_int32(num(lhs)) & crate::number::to_int32(num(rhs))) as f64),
        BitOr => Value::Number((crate::number::to_int32(num(lhs)) | crate::number::to_int32(num(rhs))) as f64),
        BitXor => Value::Number((crate::number::to_int32(num(lhs)) ^ crate::number::to_int32(num(rhs))) as f64),
        Shl => Value::Number(
            crate::number::to_int32(num(lhs)).wrapping_shl(crate::number::to_uint32(num(rhs)) & 31) as f64,
        ),
        Shr => Value::Number((crate::number::to_int32(num(lhs)) >> (crate::number::to_uint32(num(rhs)) & 31)) as f64),
        UShr => {
            Value::Number((crate::number::to_uint32(num(lhs)) >> (crate::number::to_uint32(num(rhs)) & 31)) as f64)
        }
        In => return Err(type_error_at("The 'in' operator requires a data object on the right-hand side.", node)),
        _ => return Err(type_error_at(format!("Unsupported binary operator '{}'.", op.as_str()), node)),
    })
}

fn compare(op: BinaryOp, lhs: &Value, rhs: &Value) -> bool {
    if let (Value::String(a), Value::String(b)) = (lhs, rhs) {
        return match op {
            BinaryOp::Lt => a < b,
            BinaryOp::LtEq => a <= b,
            BinaryOp::Gt => a > b,
            _ => a >= b,
        };
    }
    let (a, b) = (coerce_to_number(lhs), coerce_to_number(rhs));
    match op {
        BinaryOp::Lt => a < b,
        BinaryOp::LtEq => a <= b,
        BinaryOp::Gt => a > b,
        _ => a >= b,
    }
}

pub fn js_mod(a: f64, b: f64) -> f64 {
    if b.is_infinite() && a.is_finite() {
        return a;
    }
    let r = a % b;
    if r == 0.0 && a.is_sign_negative() { -0.0 } else { r }
}

pub fn js_pow(a: f64, b: f64) -> f64 {
    crate::stdlib::math::pow(a, b)
}

/// The program value a handler receives for a failure; one failure always yields the same value.
pub fn materialize(rt: &Rt, thrown: &Thrown) -> Value {
    match thrown {
        Thrown::Value(v) => v.clone(),
        Thrown::Pending(p) => {
            if let Some(v) = p.value.borrow().clone() {
                return Value::Object(v);
            }
            let value = super::globals::create_error_value(rt.builtins.error(p.error_type), Some(&p.message));
            if let ObjKind::Error(host) = value.kind() {
                *host.borrow_mut() = Some(p.clone());
            }
            *p.value.borrow_mut() = Some(value.clone());
            Value::Object(value)
        }
        other => {
            let message = super::globals::normalize_error(other).message;
            Value::Object(super::globals::create_error_value(rt.builtins.error(ErrorType::Error), Some(&message)))
        }
    }
}

/// `record` helper re-exported for natives.
pub fn plain_record(rt: &Rt, fields: Vec<(&str, Value)>) -> Value {
    Value::Object(record(&rt.builtins.object, fields))
}

/// Upstream `NResult` adapter: run a native in a fresh frame.
pub fn native_future(f: impl std::future::Future<Output = JsResult<Value>> + 'static) -> NResult {
    NResult::future(f)
}

#[allow(dead_code)]
fn _assert_hashmap(_: HashMap<u8, u8>) {}

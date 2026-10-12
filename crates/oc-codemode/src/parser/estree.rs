//! ESTree JSON dump of the AST, used to compare the parser against acorn (see `tests/parser_acorn.rs` and
//! `tests/fixtures/parser/gen.js`, which applies the same normalization to acorn's output).
//!
//! Every node is `{type, start, end, loc: {line, column}, ...fields}` with acorn's field names. Differences from raw
//! acorn output, mirrored by the generator: `raw` is omitted; numeric literal values are `{"f64": "<hex bits>"}`;
//! BigInt literals carry only `bigint` and regex literals only `regex`; strings holding a lone surrogate are arrays of
//! UTF-16 code units; node kinds outside the interpreter's subset carry only type and location.

use crate::ast::*;
use serde_json::{Map, Value, json};

/// Converts a program to normalized ESTree JSON.
#[doc(hidden)]
pub fn to_estree(program: &Program) -> Value {
    let mut n = node("Program", program.loc);
    n.insert("body".into(), stmts(&program.body));
    n.insert("sourceType".into(), json!("script"));
    Value::Object(n)
}

fn node(ty: &str, loc: Loc) -> Map<String, Value> {
    let mut m = Map::new();
    m.insert("type".into(), json!(ty));
    m.insert("start".into(), json!(loc.start));
    m.insert("end".into(), json!(loc.end));
    m.insert("loc".into(), json!({ "line": loc.line, "column": loc.column }));
    m
}

fn utf16(s: &[u16]) -> Value {
    match String::from_utf16(s) {
        Ok(s) => Value::String(s),
        Err(_) => Value::Array(s.iter().map(|&c| json!(c)).collect()),
    }
}

fn opt<T>(v: &Option<T>, f: impl Fn(&T) -> Value) -> Value {
    v.as_ref().map(f).unwrap_or(Value::Null)
}

fn stmts(list: &[Stmt]) -> Value {
    Value::Array(list.iter().map(stmt).collect())
}

fn ident(i: &Ident) -> Value {
    let mut n = node("Identifier", i.loc);
    n.insert("name".into(), json!(&*i.name));
    Value::Object(n)
}

fn private(i: &Ident) -> Value {
    let mut n = node("PrivateIdentifier", i.loc);
    n.insert("name".into(), json!(&*i.name));
    Value::Object(n)
}

fn block(b: &Block) -> Value {
    let mut n = node("BlockStatement", b.loc);
    n.insert("body".into(), stmts(&b.body));
    Value::Object(n)
}

fn stmt(s: &Stmt) -> Value {
    let n = match s {
        Stmt::Expr { expr: e, directive, loc } => {
            let mut n = node("ExpressionStatement", *loc);
            n.insert("expression".into(), expr(e));
            if let Some(d) = directive {
                n.insert("directive".into(), json!(&**d));
            }
            n
        }
        Stmt::Block(b) => return block(b),
        Stmt::Empty(loc) => node("EmptyStatement", *loc),
        Stmt::Return { argument, loc } => {
            let mut n = node("ReturnStatement", *loc);
            n.insert("argument".into(), opt(argument, expr));
            n
        }
        Stmt::Labeled { label, body, loc } => {
            let mut n = node("LabeledStatement", *loc);
            n.insert("body".into(), stmt(body));
            n.insert("label".into(), ident(label));
            n
        }
        Stmt::Break { label, loc } => {
            let mut n = node("BreakStatement", *loc);
            n.insert("label".into(), opt(label, ident));
            n
        }
        Stmt::Continue { label, loc } => {
            let mut n = node("ContinueStatement", *loc);
            n.insert("label".into(), opt(label, ident));
            n
        }
        Stmt::If { test, consequent, alternate, loc } => {
            let mut n = node("IfStatement", *loc);
            n.insert("test".into(), expr(test));
            n.insert("consequent".into(), stmt(consequent));
            n.insert("alternate".into(), opt(alternate, |s| stmt(s)));
            n
        }
        Stmt::Switch { discriminant, cases, loc } => {
            let mut n = node("SwitchStatement", *loc);
            n.insert("discriminant".into(), expr(discriminant));
            n.insert(
                "cases".into(),
                Value::Array(
                    cases
                        .iter()
                        .map(|c| {
                            let mut n = node("SwitchCase", c.loc);
                            n.insert("consequent".into(), stmts(&c.consequent));
                            n.insert("test".into(), opt(&c.test, expr));
                            Value::Object(n)
                        })
                        .collect(),
                ),
            );
            n
        }
        Stmt::Throw { argument, loc } => {
            let mut n = node("ThrowStatement", *loc);
            n.insert("argument".into(), expr(argument));
            n
        }
        Stmt::Try { block: b, handler, finalizer, loc } => {
            let mut n = node("TryStatement", *loc);
            n.insert("block".into(), block(b));
            n.insert(
                "handler".into(),
                opt(handler, |h| {
                    let mut n = node("CatchClause", h.loc);
                    n.insert("param".into(), opt(&h.param, pat));
                    n.insert("body".into(), block(&h.body));
                    Value::Object(n)
                }),
            );
            n.insert("finalizer".into(), opt(finalizer, block));
            n
        }
        Stmt::While { test, body, loc } => {
            let mut n = node("WhileStatement", *loc);
            n.insert("test".into(), expr(test));
            n.insert("body".into(), stmt(body));
            n
        }
        Stmt::DoWhile { body, test, loc } => {
            let mut n = node("DoWhileStatement", *loc);
            n.insert("body".into(), stmt(body));
            n.insert("test".into(), expr(test));
            n
        }
        Stmt::For { init, test, update, body, loc } => {
            let mut n = node("ForStatement", *loc);
            n.insert(
                "init".into(),
                opt(init, |i| match i {
                    ForInit::Var(v) => var_decl(v),
                    ForInit::Expr(e) => expr(e),
                }),
            );
            n.insert("test".into(), opt(test, expr));
            n.insert("update".into(), opt(update, expr));
            n.insert("body".into(), stmt(body));
            n
        }
        Stmt::ForIn { left, right, body, loc } => {
            let mut n = node("ForInStatement", *loc);
            n.insert("left".into(), for_head(left));
            n.insert("right".into(), expr(right));
            n.insert("body".into(), stmt(body));
            n
        }
        Stmt::ForOf { left, right, body, is_await, loc } => {
            let mut n = node("ForOfStatement", *loc);
            n.insert("await".into(), json!(is_await));
            n.insert("left".into(), for_head(left));
            n.insert("right".into(), expr(right));
            n.insert("body".into(), stmt(body));
            n
        }
        Stmt::FunctionDecl(f) => return function(f),
        Stmt::VarDecl(v) => return var_decl(v),
        Stmt::Unsupported { kind, loc } => node(kind, *loc),
    };
    Value::Object(n)
}

fn for_head(h: &ForHead) -> Value {
    match h {
        ForHead::Var(v) => var_decl(v),
        ForHead::Pat(p) => pat(p),
    }
}

fn var_decl(v: &VarDecl) -> Value {
    let mut n = node("VariableDeclaration", v.loc);
    n.insert(
        "declarations".into(),
        Value::Array(
            v.declarations
                .iter()
                .map(|d| {
                    let mut n = node("VariableDeclarator", d.loc);
                    n.insert("id".into(), pat(&d.id));
                    n.insert("init".into(), opt(&d.init, expr));
                    Value::Object(n)
                })
                .collect(),
        ),
    );
    n.insert("kind".into(), json!(v.kind.as_str()));
    Value::Object(n)
}

fn function(f: &Function) -> Value {
    let ty = match f.kind {
        FunctionKind::Declaration => "FunctionDeclaration",
        FunctionKind::Expression => "FunctionExpression",
        FunctionKind::Arrow => "ArrowFunctionExpression",
    };
    let mut n = node(ty, f.loc);
    n.insert("id".into(), opt(&f.id, ident));
    n.insert("expression".into(), json!(matches!(f.body, FunctionBody::Expr(_))));
    n.insert("generator".into(), json!(f.is_generator));
    n.insert("async".into(), json!(f.is_async));
    n.insert("params".into(), Value::Array(f.params.iter().map(pat).collect()));
    n.insert(
        "body".into(),
        match &f.body {
            FunctionBody::Block(b) => block(b),
            FunctionBody::Expr(e) => expr(e),
        },
    );
    Value::Object(n)
}

fn lit(l: &Lit) -> Value {
    let mut n = node("Literal", l.loc);
    match &l.value {
        LitValue::Null => {
            n.insert("value".into(), Value::Null);
        }
        LitValue::Bool(b) => {
            n.insert("value".into(), json!(b));
        }
        LitValue::Number(v) => {
            n.insert("value".into(), json!({ "f64": format!("{:016x}", v.to_bits()) }));
        }
        LitValue::String(s) => {
            n.insert("value".into(), utf16(s));
        }
        LitValue::Regex { pattern, flags } => {
            n.insert("regex".into(), json!({ "pattern": &**pattern, "flags": &**flags }));
        }
        LitValue::BigInt(b) => {
            n.insert("bigint".into(), json!(&**b));
        }
    }
    Value::Object(n)
}

fn prop_key(k: &PropKey) -> (bool, Value) {
    match k {
        PropKey::Ident(i) => (false, ident(i)),
        PropKey::Lit(l) => (false, lit(l)),
        PropKey::Computed(e) => (true, expr(e)),
        PropKey::Private(i) => (false, private(i)),
    }
}

fn template(t: &Template) -> Value {
    let mut n = node("TemplateLiteral", t.loc);
    n.insert("expressions".into(), Value::Array(t.expressions.iter().map(expr).collect()));
    n.insert(
        "quasis".into(),
        Value::Array(
            t.quasis
                .iter()
                .map(|q| {
                    let mut n = node("TemplateElement", q.loc);
                    let mut v = Map::new();
                    v.insert("raw".into(), utf16(&q.raw));
                    v.insert("cooked".into(), opt(&q.cooked, |c| utf16(c)));
                    n.insert("value".into(), Value::Object(v));
                    n.insert("tail".into(), json!(q.tail));
                    Value::Object(n)
                })
                .collect(),
        ),
    );
    Value::Object(n)
}

fn spread(argument: Value, loc: Loc) -> Value {
    let mut n = node("SpreadElement", loc);
    n.insert("argument".into(), argument);
    Value::Object(n)
}

fn args(list: &[ExprOrSpread]) -> Value {
    Value::Array(list.iter().map(expr_or_spread).collect())
}

fn expr_or_spread(e: &ExprOrSpread) -> Value {
    match e {
        ExprOrSpread::Expr(e) => expr(e),
        ExprOrSpread::Spread { argument, loc } => spread(expr(argument), *loc),
    }
}

fn member(m: &Member) -> Value {
    let mut n = node("MemberExpression", m.loc);
    n.insert("object".into(), expr(&m.object));
    let (computed, property) = match &m.property {
        MemberProp::Ident(i) => (false, ident(i)),
        MemberProp::Private(i) => (false, private(i)),
        MemberProp::Computed(e) => (true, expr(e)),
    };
    n.insert("property".into(), property);
    n.insert("computed".into(), json!(computed));
    n.insert("optional".into(), json!(m.optional));
    Value::Object(n)
}

fn unary_op(op: UnaryOp) -> &'static str {
    match op {
        UnaryOp::Minus => "-",
        UnaryOp::Plus => "+",
        UnaryOp::Not => "!",
        UnaryOp::BitNot => "~",
        UnaryOp::Typeof => "typeof",
        UnaryOp::Void => "void",
        UnaryOp::Delete => "delete",
    }
}

fn logical_op(op: LogicalOp) -> &'static str {
    match op {
        LogicalOp::And => "&&",
        LogicalOp::Or => "||",
        LogicalOp::Nullish => "??",
    }
}

fn assign_op(op: AssignOp) -> String {
    match op {
        AssignOp::Assign => "=".into(),
        AssignOp::Compound(b) => format!("{}=", b.as_str()),
        AssignOp::Logical(l) => format!("{}=", logical_op(l)),
    }
}

fn expr(e: &Expr) -> Value {
    let n = match e {
        Expr::Ident(i) => return ident(i),
        Expr::Lit(l) => return lit(l),
        Expr::This(loc) => node("ThisExpression", *loc),
        Expr::Array { elements, loc } => {
            let mut n = node("ArrayExpression", *loc);
            n.insert("elements".into(), Value::Array(elements.iter().map(|e| opt(e, expr_or_spread)).collect()));
            n
        }
        Expr::Object { properties, loc } => {
            let mut n = node("ObjectExpression", *loc);
            n.insert(
                "properties".into(),
                Value::Array(
                    properties
                        .iter()
                        .map(|p| match p {
                            ObjectMember::Prop(p) => {
                                let mut n = node("Property", p.loc);
                                let (computed, key) = prop_key(&p.key);
                                n.insert("method".into(), json!(p.method));
                                n.insert("shorthand".into(), json!(p.shorthand));
                                n.insert("computed".into(), json!(computed));
                                n.insert("key".into(), key);
                                n.insert("value".into(), expr(&p.value));
                                n.insert(
                                    "kind".into(),
                                    json!(match p.kind {
                                        PropKind::Init => "init",
                                        PropKind::Get => "get",
                                        PropKind::Set => "set",
                                    }),
                                );
                                Value::Object(n)
                            }
                            ObjectMember::Spread { argument, loc } => spread(expr(argument), *loc),
                        })
                        .collect(),
                ),
            );
            n
        }
        Expr::Function(f) => return function(f),
        Expr::Template(t) => return template(t),
        Expr::TaggedTemplate { tag, quasi, loc } => {
            let mut n = node("TaggedTemplateExpression", *loc);
            n.insert("tag".into(), expr(tag));
            n.insert("quasi".into(), template(quasi));
            n
        }
        Expr::Member(m) => return member(m),
        Expr::Super(loc) => node("Super", *loc),
        Expr::New { callee, arguments, loc } => {
            let mut n = node("NewExpression", *loc);
            n.insert("callee".into(), expr(callee));
            n.insert("arguments".into(), args(arguments));
            n
        }
        Expr::Call { callee, arguments, optional, loc } => {
            let mut n = node("CallExpression", *loc);
            n.insert("callee".into(), expr(callee));
            n.insert("arguments".into(), args(arguments));
            n.insert("optional".into(), json!(optional));
            n
        }
        Expr::Update { op, prefix, argument, loc } => {
            let mut n = node("UpdateExpression", *loc);
            n.insert(
                "operator".into(),
                json!(match op {
                    UpdateOp::Increment => "++",
                    UpdateOp::Decrement => "--",
                }),
            );
            n.insert("prefix".into(), json!(prefix));
            n.insert("argument".into(), expr(argument));
            n
        }
        Expr::Await { argument, loc } => {
            let mut n = node("AwaitExpression", *loc);
            n.insert("argument".into(), expr(argument));
            n
        }
        Expr::Unary { op, argument, loc } => {
            let mut n = node("UnaryExpression", *loc);
            n.insert("operator".into(), json!(unary_op(*op)));
            n.insert("prefix".into(), json!(true));
            n.insert("argument".into(), expr(argument));
            n
        }
        Expr::Binary { op, left, right, loc } => {
            let mut n = node("BinaryExpression", *loc);
            n.insert("left".into(), expr(left));
            n.insert("operator".into(), json!(op.as_str()));
            n.insert("right".into(), expr(right));
            n
        }
        Expr::Logical { op, left, right, loc } => {
            let mut n = node("LogicalExpression", *loc);
            n.insert("left".into(), expr(left));
            n.insert("operator".into(), json!(logical_op(*op)));
            n.insert("right".into(), expr(right));
            n
        }
        Expr::Conditional { test, consequent, alternate, loc } => {
            let mut n = node("ConditionalExpression", *loc);
            n.insert("test".into(), expr(test));
            n.insert("consequent".into(), expr(consequent));
            n.insert("alternate".into(), expr(alternate));
            n
        }
        Expr::Yield { argument, delegate, loc } => {
            let mut n = node("YieldExpression", *loc);
            n.insert("delegate".into(), json!(delegate));
            n.insert("argument".into(), opt(argument, |a| expr(a)));
            n
        }
        Expr::Assign { op, left, right, loc } => {
            let mut n = node("AssignmentExpression", *loc);
            n.insert("operator".into(), json!(assign_op(*op)));
            n.insert("left".into(), pat(left));
            n.insert("right".into(), expr(right));
            n
        }
        Expr::Sequence { expressions, loc } => {
            let mut n = node("SequenceExpression", *loc);
            n.insert("expressions".into(), Value::Array(expressions.iter().map(expr).collect()));
            n
        }
        Expr::Chain { expression, loc } => {
            let mut n = node("ChainExpression", *loc);
            n.insert("expression".into(), expr(expression));
            n
        }
        Expr::PrivateName(i) => return private(i),
        Expr::Unsupported { kind, loc } => node(kind, *loc),
    };
    Value::Object(n)
}

fn pat(p: &Pat) -> Value {
    let n = match p {
        Pat::Ident(i) => return ident(i),
        Pat::Member(m) => return member(m),
        Pat::Object { properties, loc } => {
            let mut n = node("ObjectPattern", *loc);
            n.insert(
                "properties".into(),
                Value::Array(
                    properties
                        .iter()
                        .map(|p| match p {
                            ObjectPatMember::Prop(p) => {
                                let mut n = node("Property", p.loc);
                                let (computed, key) = prop_key(&p.key);
                                n.insert("method".into(), json!(false));
                                n.insert("shorthand".into(), json!(p.shorthand));
                                n.insert("computed".into(), json!(computed));
                                n.insert("key".into(), key);
                                n.insert("value".into(), pat(&p.value));
                                n.insert("kind".into(), json!("init"));
                                Value::Object(n)
                            }
                            ObjectPatMember::Rest { argument, loc } => rest(argument, *loc),
                        })
                        .collect(),
                ),
            );
            n
        }
        Pat::Array { elements, loc } => {
            let mut n = node("ArrayPattern", *loc);
            n.insert("elements".into(), Value::Array(elements.iter().map(|e| opt(e, pat)).collect()));
            n
        }
        Pat::Rest { argument, loc } => return rest(argument, *loc),
        Pat::Assign { left, right, loc } => {
            let mut n = node("AssignmentPattern", *loc);
            n.insert("left".into(), pat(left));
            n.insert("right".into(), expr(right));
            n
        }
    };
    Value::Object(n)
}

fn rest(argument: &Pat, loc: Loc) -> Value {
    let mut n = node("RestElement", loc);
    n.insert("argument".into(), pat(argument));
    Value::Object(n)
}

//! ESTree-shaped AST produced by the acorn port in [`crate::parser`].
//!
//! The shapes mirror what acorn 8.15 (`ecmaVersion: "latest"`, `sourceType: "script"`) produces. Node kinds the
//! interpreter never evaluates (classes, `import()`, `new.target`, ...) are kept as [`Expr::Unsupported`] /
//! [`Stmt::Unsupported`] carrying only their ESTree type name and location, because the interpreter reports them
//! lazily, when reached, as `Syntax '<Type>' is not supported`.

use std::rc::Rc;

/// Source location of a node's start (and its UTF-16 offsets).
///
/// `line` is 1-based and `column` is 0-based, both counted in UTF-16 code units exactly as acorn does
/// (`\r\n`, `\r`, `\n`, ` `, ` ` are line breaks).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Loc {
    pub line: u32,
    pub column: u32,
    /// UTF-16 offset of the node start.
    pub start: u32,
    /// UTF-16 offset just past the node end.
    pub end: u32,
}

/// A UTF-16 string as JavaScript sees it (may hold lone surrogates, e.g. from `"\uD800"`).
pub type Utf16 = Rc<[u16]>;

#[derive(Debug)]
pub struct Program {
    pub body: Vec<Stmt>,
    pub loc: Loc,
}

#[derive(Debug, Clone)]
pub struct Ident {
    pub name: Rc<str>,
    pub loc: Loc,
}

#[derive(Debug)]
pub enum Stmt {
    /// `ExpressionStatement`; `directive` is set for directive-prologue string statements (acorn's `directive`).
    Expr { expr: Expr, directive: Option<Rc<str>>, loc: Loc },
    Block(Block),
    Empty(Loc),
    Return { argument: Option<Expr>, loc: Loc },
    Labeled { label: Ident, body: Box<Stmt>, loc: Loc },
    Break { label: Option<Ident>, loc: Loc },
    Continue { label: Option<Ident>, loc: Loc },
    If { test: Expr, consequent: Box<Stmt>, alternate: Option<Box<Stmt>>, loc: Loc },
    Switch { discriminant: Expr, cases: Vec<SwitchCase>, loc: Loc },
    Throw { argument: Expr, loc: Loc },
    Try { block: Block, handler: Option<CatchClause>, finalizer: Option<Block>, loc: Loc },
    While { test: Expr, body: Box<Stmt>, loc: Loc },
    DoWhile { body: Box<Stmt>, test: Expr, loc: Loc },
    For { init: Option<ForInit>, test: Option<Expr>, update: Option<Expr>, body: Box<Stmt>, loc: Loc },
    ForIn { left: ForHead, right: Expr, body: Box<Stmt>, loc: Loc },
    ForOf { left: ForHead, right: Expr, body: Box<Stmt>, is_await: bool, loc: Loc },
    FunctionDecl(Rc<Function>),
    VarDecl(VarDecl),
    /// Any statement outside the interpreter's subset: `ClassDeclaration`, `DebuggerStatement`, `WithStatement`.
    Unsupported { kind: &'static str, loc: Loc },
}

#[derive(Debug)]
pub struct Block {
    pub body: Vec<Stmt>,
    pub loc: Loc,
}

#[derive(Debug)]
pub struct SwitchCase {
    /// `None` for `default:`.
    pub test: Option<Expr>,
    pub consequent: Vec<Stmt>,
    pub loc: Loc,
}

#[derive(Debug)]
pub struct CatchClause {
    pub param: Option<Pat>,
    pub body: Block,
    pub loc: Loc,
}

#[derive(Debug)]
pub enum ForInit {
    Var(VarDecl),
    Expr(Expr),
}

#[derive(Debug)]
pub enum ForHead {
    Var(VarDecl),
    Pat(Pat),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VarKind {
    Var,
    Let,
    Const,
    Using,
    AwaitUsing,
}

impl VarKind {
    pub fn as_str(self) -> &'static str {
        match self {
            VarKind::Var => "var",
            VarKind::Let => "let",
            VarKind::Const => "const",
            VarKind::Using => "using",
            VarKind::AwaitUsing => "await using",
        }
    }
}

#[derive(Debug)]
pub struct VarDecl {
    pub kind: VarKind,
    pub declarations: Vec<VarDeclarator>,
    pub loc: Loc,
}

#[derive(Debug)]
pub struct VarDeclarator {
    pub id: Pat,
    pub init: Option<Expr>,
    pub loc: Loc,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FunctionKind {
    Declaration,
    Expression,
    Arrow,
}

#[derive(Debug)]
pub struct Function {
    pub kind: FunctionKind,
    pub id: Option<Ident>,
    pub params: Vec<Pat>,
    pub body: FunctionBody,
    pub is_async: bool,
    pub is_generator: bool,
    pub loc: Loc,
}

#[derive(Debug)]
pub enum FunctionBody {
    Block(Block),
    /// Concise arrow body (`expression: true`).
    Expr(Box<Expr>),
}

#[derive(Debug, Clone, PartialEq)]
pub enum LitValue {
    Null,
    Bool(bool),
    Number(f64),
    String(Utf16),
    /// `/pattern/flags`; acorn already validated the pattern.
    Regex { pattern: Rc<str>, flags: Rc<str> },
    /// Digits as acorn's `bigint` property (decimal string without the `n`).
    BigInt(Rc<str>),
}

#[derive(Debug)]
pub struct Lit {
    pub value: LitValue,
    pub loc: Loc,
}

#[derive(Debug)]
pub struct TemplateElement {
    /// `None` when the escape sequence is invalid (only possible in tagged templates).
    pub cooked: Option<Utf16>,
    pub raw: Utf16,
    pub tail: bool,
    pub loc: Loc,
}

#[derive(Debug)]
pub struct Template {
    pub quasis: Vec<TemplateElement>,
    pub expressions: Vec<Expr>,
    pub loc: Loc,
}

#[derive(Debug)]
pub enum ExprOrSpread {
    Expr(Expr),
    Spread { argument: Expr, loc: Loc },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PropKind {
    Init,
    Get,
    Set,
}

#[derive(Debug)]
pub enum PropKey {
    Ident(Ident),
    Lit(Lit),
    /// `[expr]` (`computed: true`).
    Computed(Box<Expr>),
    /// `#name` (only reachable in class bodies, kept for completeness).
    Private(Ident),
}

impl PropKey {
    pub fn loc(&self) -> Loc {
        match self {
            PropKey::Ident(i) | PropKey::Private(i) => i.loc,
            PropKey::Lit(l) => l.loc,
            PropKey::Computed(e) => e.loc(),
        }
    }
}

#[derive(Debug)]
pub struct Property {
    pub key: PropKey,
    pub value: Expr,
    pub kind: PropKind,
    pub method: bool,
    pub shorthand: bool,
    pub loc: Loc,
}

#[derive(Debug)]
pub enum ObjectMember {
    Prop(Property),
    Spread { argument: Expr, loc: Loc },
}

#[derive(Debug)]
pub enum MemberProp {
    /// `a.b`
    Ident(Ident),
    /// `a.#b`
    Private(Ident),
    /// `a[expr]`
    Computed(Box<Expr>),
}

#[derive(Debug)]
pub struct Member {
    pub object: Box<Expr>,
    pub property: MemberProp,
    pub optional: bool,
    pub loc: Loc,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UnaryOp {
    Minus,
    Plus,
    Not,
    BitNot,
    Typeof,
    Void,
    Delete,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BinaryOp {
    Eq,
    NotEq,
    StrictEq,
    StrictNotEq,
    Lt,
    LtEq,
    Gt,
    GtEq,
    Shl,
    Shr,
    UShr,
    Add,
    Sub,
    Mul,
    Div,
    Mod,
    Exp,
    BitOr,
    BitXor,
    BitAnd,
    In,
    InstanceOf,
}

impl BinaryOp {
    pub fn as_str(self) -> &'static str {
        use BinaryOp::*;
        match self {
            Eq => "==",
            NotEq => "!=",
            StrictEq => "===",
            StrictNotEq => "!==",
            Lt => "<",
            LtEq => "<=",
            Gt => ">",
            GtEq => ">=",
            Shl => "<<",
            Shr => ">>",
            UShr => ">>>",
            Add => "+",
            Sub => "-",
            Mul => "*",
            Div => "/",
            Mod => "%",
            Exp => "**",
            BitOr => "|",
            BitXor => "^",
            BitAnd => "&",
            In => "in",
            InstanceOf => "instanceof",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LogicalOp {
    And,
    Or,
    Nullish,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AssignOp {
    Assign,
    /// Compound arithmetic/bitwise assignment (`+=` ... `>>>=`), holding the binary operator it applies.
    Compound(BinaryOp),
    /// `&&=`, `||=`, `??=`
    Logical(LogicalOp),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UpdateOp {
    Increment,
    Decrement,
}

#[derive(Debug)]
pub enum Expr {
    Ident(Ident),
    Lit(Lit),
    This(Loc),
    Array { elements: Vec<Option<ExprOrSpread>>, loc: Loc },
    Object { properties: Vec<ObjectMember>, loc: Loc },
    /// `FunctionExpression` or `ArrowFunctionExpression` (see [`Function::kind`]).
    Function(Rc<Function>),
    Template(Template),
    TaggedTemplate { tag: Box<Expr>, quasi: Template, loc: Loc },
    Member(Member),
    /// `super` used as a callee or member object.
    Super(Loc),
    New { callee: Box<Expr>, arguments: Vec<ExprOrSpread>, loc: Loc },
    Call { callee: Box<Expr>, arguments: Vec<ExprOrSpread>, optional: bool, loc: Loc },
    Update { op: UpdateOp, prefix: bool, argument: Box<Expr>, loc: Loc },
    Await { argument: Box<Expr>, loc: Loc },
    Unary { op: UnaryOp, argument: Box<Expr>, loc: Loc },
    Binary { op: BinaryOp, left: Box<Expr>, right: Box<Expr>, loc: Loc },
    Logical { op: LogicalOp, left: Box<Expr>, right: Box<Expr>, loc: Loc },
    Conditional { test: Box<Expr>, consequent: Box<Expr>, alternate: Box<Expr>, loc: Loc },
    Yield { argument: Option<Box<Expr>>, delegate: bool, loc: Loc },
    Assign { op: AssignOp, left: Box<Pat>, right: Box<Expr>, loc: Loc },
    Sequence { expressions: Vec<Expr>, loc: Loc },
    Chain { expression: Box<Expr>, loc: Loc },
    /// `#x` as the left operand of `#x in obj`.
    PrivateName(Ident),
    /// Expression node types outside the subset: `ClassExpression`, `ImportExpression`, `MetaProperty`.
    Unsupported { kind: &'static str, loc: Loc },
}

impl Expr {
    pub fn loc(&self) -> Loc {
        match self {
            Expr::Ident(i) | Expr::PrivateName(i) => i.loc,
            Expr::Lit(l) => l.loc,
            Expr::This(loc) | Expr::Super(loc) => *loc,
            Expr::Function(f) => f.loc,
            Expr::Template(t) => t.loc,
            Expr::Member(m) => m.loc,
            Expr::Array { loc, .. }
            | Expr::Object { loc, .. }
            | Expr::TaggedTemplate { loc, .. }
            | Expr::New { loc, .. }
            | Expr::Call { loc, .. }
            | Expr::Update { loc, .. }
            | Expr::Await { loc, .. }
            | Expr::Unary { loc, .. }
            | Expr::Binary { loc, .. }
            | Expr::Logical { loc, .. }
            | Expr::Conditional { loc, .. }
            | Expr::Yield { loc, .. }
            | Expr::Assign { loc, .. }
            | Expr::Sequence { loc, .. }
            | Expr::Chain { loc, .. }
            | Expr::Unsupported { loc, .. } => *loc,
        }
    }

    /// The ESTree `type` of this node.
    pub fn type_name(&self) -> &'static str {
        match self {
            Expr::Ident(_) => "Identifier",
            Expr::Lit(_) => "Literal",
            Expr::This(_) => "ThisExpression",
            Expr::Array { .. } => "ArrayExpression",
            Expr::Object { .. } => "ObjectExpression",
            Expr::Function(f) => match f.kind {
                FunctionKind::Arrow => "ArrowFunctionExpression",
                FunctionKind::Declaration => "FunctionDeclaration",
                FunctionKind::Expression => "FunctionExpression",
            },
            Expr::Template(_) => "TemplateLiteral",
            Expr::TaggedTemplate { .. } => "TaggedTemplateExpression",
            Expr::Member(_) => "MemberExpression",
            Expr::Super(_) => "Super",
            Expr::New { .. } => "NewExpression",
            Expr::Call { .. } => "CallExpression",
            Expr::Update { .. } => "UpdateExpression",
            Expr::Await { .. } => "AwaitExpression",
            Expr::Unary { .. } => "UnaryExpression",
            Expr::Binary { .. } => "BinaryExpression",
            Expr::Logical { .. } => "LogicalExpression",
            Expr::Conditional { .. } => "ConditionalExpression",
            Expr::Yield { .. } => "YieldExpression",
            Expr::Assign { .. } => "AssignmentExpression",
            Expr::Sequence { .. } => "SequenceExpression",
            Expr::Chain { .. } => "ChainExpression",
            Expr::PrivateName(_) => "PrivateIdentifier",
            Expr::Unsupported { kind, .. } => kind,
        }
    }
}

#[derive(Debug)]
pub struct ObjectPatProp {
    pub key: PropKey,
    pub value: Pat,
    pub shorthand: bool,
    pub loc: Loc,
}

#[derive(Debug)]
pub enum ObjectPatMember {
    Prop(ObjectPatProp),
    Rest { argument: Box<Pat>, loc: Loc },
}

/// Binding and assignment targets (ESTree `Pattern`, plus `MemberExpression` in assignment position).
#[derive(Debug)]
pub enum Pat {
    Ident(Ident),
    Member(Member),
    Object { properties: Vec<ObjectPatMember>, loc: Loc },
    Array { elements: Vec<Option<Pat>>, loc: Loc },
    Rest { argument: Box<Pat>, loc: Loc },
    Assign { left: Box<Pat>, right: Box<Expr>, loc: Loc },
}

impl Pat {
    pub fn loc(&self) -> Loc {
        match self {
            Pat::Ident(i) => i.loc,
            Pat::Member(m) => m.loc,
            Pat::Object { loc, .. } | Pat::Array { loc, .. } | Pat::Rest { loc, .. } | Pat::Assign { loc, .. } => *loc,
        }
    }

    pub fn type_name(&self) -> &'static str {
        match self {
            Pat::Ident(_) => "Identifier",
            Pat::Member(_) => "MemberExpression",
            Pat::Object { .. } => "ObjectPattern",
            Pat::Array { .. } => "ArrayPattern",
            Pat::Rest { .. } => "RestElement",
            Pat::Assign { .. } => "AssignmentPattern",
        }
    }
}

impl Stmt {
    pub fn loc(&self) -> Loc {
        match self {
            Stmt::Block(b) => b.loc,
            Stmt::Empty(loc) => *loc,
            Stmt::FunctionDecl(f) => f.loc,
            Stmt::VarDecl(v) => v.loc,
            Stmt::Expr { loc, .. }
            | Stmt::Return { loc, .. }
            | Stmt::Labeled { loc, .. }
            | Stmt::Break { loc, .. }
            | Stmt::Continue { loc, .. }
            | Stmt::If { loc, .. }
            | Stmt::Switch { loc, .. }
            | Stmt::Throw { loc, .. }
            | Stmt::Try { loc, .. }
            | Stmt::While { loc, .. }
            | Stmt::DoWhile { loc, .. }
            | Stmt::For { loc, .. }
            | Stmt::ForIn { loc, .. }
            | Stmt::ForOf { loc, .. }
            | Stmt::Unsupported { loc, .. } => *loc,
        }
    }
}

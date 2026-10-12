//! Token types and tokenizer contexts (acorn's `tokentype.js` and `tokencontext.js`).

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum TT {
    Num,
    Regexp,
    String,
    Name,
    PrivateId,
    Eof,
    BracketL,
    BracketR,
    BraceL,
    BraceR,
    ParenL,
    ParenR,
    Comma,
    Semi,
    Colon,
    Dot,
    Question,
    QuestionDot,
    Arrow,
    Template,
    InvalidTemplate,
    Ellipsis,
    BackQuote,
    DollarBraceL,
    Eq,
    Assign,
    IncDec,
    Prefix,
    LogicalOr,
    LogicalAnd,
    BitwiseOr,
    BitwiseXor,
    BitwiseAnd,
    Equality,
    Relational,
    BitShift,
    PlusMin,
    Modulo,
    Star,
    Slash,
    Starstar,
    Coalesce,
    // Keywords.
    Break,
    Case,
    Catch,
    Continue,
    Debugger,
    Default,
    Do,
    Else,
    Finally,
    For,
    Function,
    If,
    Return,
    Switch,
    Throw,
    Try,
    Var,
    Const,
    While,
    With,
    New,
    This,
    Super,
    Class,
    Extends,
    Export,
    Import,
    Null,
    True,
    False,
    In,
    Instanceof,
    Typeof,
    Void,
    Delete,
}

impl TT {
    pub(super) fn keyword(self) -> Option<&'static str> {
        use TT::*;
        Some(match self {
            Break => "break",
            Case => "case",
            Catch => "catch",
            Continue => "continue",
            Debugger => "debugger",
            Default => "default",
            Do => "do",
            Else => "else",
            Finally => "finally",
            For => "for",
            Function => "function",
            If => "if",
            Return => "return",
            Switch => "switch",
            Throw => "throw",
            Try => "try",
            Var => "var",
            Const => "const",
            While => "while",
            With => "with",
            New => "new",
            This => "this",
            Super => "super",
            Class => "class",
            Extends => "extends",
            Export => "export",
            Import => "import",
            Null => "null",
            True => "true",
            False => "false",
            In => "in",
            Instanceof => "instanceof",
            Typeof => "typeof",
            Void => "void",
            Delete => "delete",
            _ => return None,
        })
    }

    pub(super) fn is_keyword(self) -> bool {
        self.keyword().is_some()
    }

    /// Maps a word to its keyword token type (acorn's `keywords` regexp for `ecmaVersion >= 6`).
    pub(super) fn from_keyword(word: &str) -> Option<TT> {
        use TT::*;
        Some(match word {
            "break" => Break,
            "case" => Case,
            "catch" => Catch,
            "continue" => Continue,
            "debugger" => Debugger,
            "default" => Default,
            "do" => Do,
            "else" => Else,
            "finally" => Finally,
            "for" => For,
            "function" => Function,
            "if" => If,
            "return" => Return,
            "switch" => Switch,
            "throw" => Throw,
            "try" => Try,
            "var" => Var,
            "const" => Const,
            "while" => While,
            "with" => With,
            "new" => New,
            "this" => This,
            "super" => Super,
            "class" => Class,
            "extends" => Extends,
            "export" => Export,
            "import" => Import,
            "null" => Null,
            "true" => True,
            "false" => False,
            "in" => In,
            "instanceof" => Instanceof,
            "typeof" => Typeof,
            "void" => Void,
            "delete" => Delete,
            _ => return None,
        })
    }

    pub(super) fn before_expr(self) -> bool {
        use TT::*;
        matches!(
            self,
            BracketL
                | BraceL
                | ParenL
                | Comma
                | Semi
                | Colon
                | Question
                | Arrow
                | Ellipsis
                | DollarBraceL
                | Eq
                | Assign
                | Prefix
                | LogicalOr
                | LogicalAnd
                | BitwiseOr
                | BitwiseXor
                | BitwiseAnd
                | Equality
                | Relational
                | BitShift
                | PlusMin
                | Modulo
                | Star
                | Slash
                | Starstar
                | Coalesce
                | Case
                | Default
                | Do
                | Else
                | Return
                | Throw
                | Extends
                | New
                | In
                | Instanceof
                | Typeof
                | Void
                | Delete
        )
    }

    pub(super) fn starts_expr(self) -> bool {
        use TT::*;
        matches!(
            self,
            Num | Regexp
                | String
                | Name
                | PrivateId
                | BracketL
                | BraceL
                | ParenL
                | BackQuote
                | DollarBraceL
                | IncDec
                | Prefix
                | PlusMin
                | Function
                | New
                | This
                | Super
                | Class
                | Import
                | Null
                | True
                | False
                | Typeof
                | Void
                | Delete
        )
    }

    pub(super) fn is_loop(self) -> bool {
        matches!(self, TT::Do | TT::For | TT::While)
    }

    pub(super) fn is_assign(self) -> bool {
        matches!(self, TT::Eq | TT::Assign)
    }

    pub(super) fn prefix(self) -> bool {
        use TT::*;
        matches!(self, IncDec | Prefix | PlusMin | Typeof | Void | Delete)
    }

    pub(super) fn postfix(self) -> bool {
        self == TT::IncDec
    }

    pub(super) fn binop(self) -> Option<u8> {
        use TT::*;
        Some(match self {
            LogicalOr => 1,
            LogicalAnd => 2,
            BitwiseOr => 3,
            BitwiseXor => 4,
            BitwiseAnd => 5,
            Equality => 6,
            Relational => 7,
            BitShift => 8,
            PlusMin => 9,
            Modulo => 10,
            Star => 10,
            Slash => 10,
            Coalesce => 1,
            In => 7,
            Instanceof => 7,
            _ => return None,
        })
    }
}

/// acorn's token contexts (`types` in `tokencontext.js`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Ctx {
    BStat,
    BExpr,
    BTmpl,
    PStat,
    PExpr,
    QTmpl,
    FStat,
    FExpr,
    FExprGen,
    FGen,
}

impl Ctx {
    pub(super) fn is_expr(self) -> bool {
        matches!(self, Ctx::BExpr | Ctx::PExpr | Ctx::QTmpl | Ctx::FExpr | Ctx::FExprGen)
    }

    pub(super) fn preserve_space(self) -> bool {
        self == Ctx::QTmpl
    }

    pub(super) fn is_function(self) -> bool {
        matches!(self, Ctx::FStat | Ctx::FExpr | Ctx::FExprGen | Ctx::FGen)
    }

    pub(super) fn generator(self) -> bool {
        matches!(self, Ctx::FExprGen | Ctx::FGen)
    }
}

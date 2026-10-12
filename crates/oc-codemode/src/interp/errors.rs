//! Failures inside the interpreter and their normalization into diagnostics. Port of `interpreter/errors.ts` and
//! the failure half of `interpreter/model.ts`.

use std::cell::RefCell;
use std::rc::Rc;

use serde::{Deserialize, Serialize};

use crate::ast::Loc;
use crate::object::Obj;
use crate::value::Value;

/// Stable categories produced by program, schema, tool, limit, and truncation diagnostics.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum DiagnosticKind {
    ParseError,
    UnsupportedSyntax,
    UnknownTool,
    InvalidToolInput,
    InvalidToolOutput,
    InvalidDataValue,
    ToolCallLimitExceeded,
    TimeoutExceeded,
    ToolFailure,
    ExecutionFailure,
    Truncated,
}

impl DiagnosticKind {
    pub fn as_str(self) -> &'static str {
        match self {
            DiagnosticKind::ParseError => "ParseError",
            DiagnosticKind::UnsupportedSyntax => "UnsupportedSyntax",
            DiagnosticKind::UnknownTool => "UnknownTool",
            DiagnosticKind::InvalidToolInput => "InvalidToolInput",
            DiagnosticKind::InvalidToolOutput => "InvalidToolOutput",
            DiagnosticKind::InvalidDataValue => "InvalidDataValue",
            DiagnosticKind::ToolCallLimitExceeded => "ToolCallLimitExceeded",
            DiagnosticKind::TimeoutExceeded => "TimeoutExceeded",
            DiagnosticKind::ToolFailure => "ToolFailure",
            DiagnosticKind::ExecutionFailure => "ExecutionFailure",
            DiagnosticKind::Truncated => "Truncated",
        }
    }
}

/// 1-based line and column in the submitted source.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Location {
    pub line: u32,
    pub column: u32,
}

/// A normalized program diagnostic safe to return across an agent tool boundary.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Diagnostic {
    pub kind: DiagnosticKind,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub location: Option<Location>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub suggestions: Option<Vec<String>>,
}

impl Diagnostic {
    pub fn new(kind: DiagnosticKind, message: impl Into<String>) -> Diagnostic {
        Diagnostic { kind, message: message.into(), location: None, suggestions: None }
    }
}

/// The JS error classes a program can see.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ErrorType {
    Error,
    TypeError,
    RangeError,
    SyntaxError,
    ReferenceError,
    EvalError,
    URIError,
    AggregateError,
}

pub const ERROR_TYPES: [ErrorType; 8] = [
    ErrorType::Error,
    ErrorType::TypeError,
    ErrorType::RangeError,
    ErrorType::SyntaxError,
    ErrorType::ReferenceError,
    ErrorType::EvalError,
    ErrorType::URIError,
    ErrorType::AggregateError,
];

impl ErrorType {
    pub fn name(self) -> &'static str {
        match self {
            ErrorType::Error => "Error",
            ErrorType::TypeError => "TypeError",
            ErrorType::RangeError => "RangeError",
            ErrorType::SyntaxError => "SyntaxError",
            ErrorType::ReferenceError => "ReferenceError",
            ErrorType::EvalError => "EvalError",
            ErrorType::URIError => "URIError",
            ErrorType::AggregateError => "AggregateError",
        }
    }
    pub fn from_name(name: &str) -> Option<ErrorType> {
        ERROR_TYPES.iter().copied().find(|t| t.name() == name)
    }
}

/// A failure raised by the interpreter or a built-in. It becomes one program Error object the first time a handler
/// observes it, so every observer of the same failure sees the same value.
pub struct PendingThrow {
    pub error_type: ErrorType,
    pub message: String,
    pub node: RefCell<Option<Loc>>,
    pub kind: DiagnosticKind,
    pub suggestions: Option<Vec<String>>,
    pub value: RefCell<Option<Obj>>,
    /// Never given a source location (a host failure upstream reports without one).
    pub unlocated: bool,
}

impl std::fmt::Debug for PendingThrow {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.error_type.name(), self.message)
    }
}

/// A tool-runtime failure with its own diagnostic kind (`UnknownTool`, `InvalidToolInput`, ...).
#[derive(Debug, Clone)]
pub struct ToolRuntimeError {
    pub kind: DiagnosticKind,
    pub message: String,
    pub suggestions: Vec<String>,
}

/// Everything that can unwind through evaluation.
#[derive(Clone, Debug)]
pub enum Thrown {
    /// `throw value` (and rejections with program values).
    Value(Value),
    /// An interpreter or built-in failure.
    Pending(Rc<PendingThrow>),
    /// A generator's `return()` unwinding its body.
    GeneratorReturn(Value),
    ToolRuntime(Rc<ToolRuntimeError>),
    /// A tool refused or failed (`ToolError`).
    ToolFailure(Rc<str>),
    /// Timeout or cancellation: bypasses `catch` and `finally`.
    Interrupt,
    /// Internal: an optional chain short-circuited (caught at the `ChainExpression`).
    ShortCircuit,
}

pub type JsResult<T> = Result<T, Thrown>;

fn pending(error_type: ErrorType, message: String, node: Option<Loc>, kind: DiagnosticKind) -> Thrown {
    Thrown::Pending(Rc::new(PendingThrow {
        error_type,
        message,
        node: RefCell::new(node),
        kind,
        suggestions: None,
        value: RefCell::new(None),
        unlocated: false,
    }))
}

pub fn type_error(message: impl Into<String>) -> Thrown {
    pending(ErrorType::TypeError, message.into(), None, DiagnosticKind::ExecutionFailure)
}
pub fn type_error_at(message: impl Into<String>, node: Loc) -> Thrown {
    pending(ErrorType::TypeError, message.into(), Some(node), DiagnosticKind::ExecutionFailure)
}
pub fn invalid_data(message: impl Into<String>) -> Thrown {
    pending(ErrorType::TypeError, message.into(), None, DiagnosticKind::InvalidDataValue)
}
pub fn invalid_data_at(message: impl Into<String>, node: Loc) -> Thrown {
    pending(ErrorType::TypeError, message.into(), Some(node), DiagnosticKind::InvalidDataValue)
}
pub fn range_error(message: impl Into<String>) -> Thrown {
    pending(ErrorType::RangeError, message.into(), None, DiagnosticKind::ExecutionFailure)
}
pub fn range_error_at(message: impl Into<String>, node: Loc) -> Thrown {
    pending(ErrorType::RangeError, message.into(), Some(node), DiagnosticKind::ExecutionFailure)
}
pub fn reference_error_at(message: impl Into<String>, node: Loc) -> Thrown {
    pending(ErrorType::ReferenceError, message.into(), Some(node), DiagnosticKind::ExecutionFailure)
}
pub fn syntax_error(message: impl Into<String>) -> Thrown {
    pending(ErrorType::SyntaxError, message.into(), None, DiagnosticKind::ExecutionFailure)
}
pub fn uri_error(message: impl Into<String>) -> Thrown {
    pending(ErrorType::URIError, message.into(), None, DiagnosticKind::ExecutionFailure)
}
/// A host-engine error escaping a built-in (`(1).toFixed(200)`), raised as the same-named program error.
pub fn host_error(error_type: ErrorType, message: impl Into<String>) -> Thrown {
    pending(error_type, message.into(), None, DiagnosticKind::ExecutionFailure)
}

/// Orient the agent rather than enumerate JavaScript; interpreter-support.md is the full matrix.
pub const SUPPORTED_SYNTAX_MESSAGE: &str = "This is a restricted JavaScript-like language. Supported: plain and async functions, data literals, destructuring, standard control flow, await and Promise, and built-ins such as Array, Object, Math, JSON, Date, RegExp, Map, Set, and URL. Unsupported: classes, getters/setters, BigInt, and custom Symbols. Use plain functions and data objects instead.";

pub fn unsupported_syntax(kind: &str, node: Loc) -> Thrown {
    Thrown::Pending(Rc::new(PendingThrow {
        error_type: ErrorType::SyntaxError,
        message: format!("Syntax '{kind}' is not supported. {SUPPORTED_SYNTAX_MESSAGE}"),
        node: RefCell::new(Some(node)),
        kind: DiagnosticKind::UnsupportedSyntax,
        suggestions: Some(vec![SUPPORTED_SYNTAX_MESSAGE.to_string()]),
        value: RefCell::new(None),
        unlocated: false,
    }))
}

pub fn tool_runtime_error(kind: DiagnosticKind, message: impl Into<String>, suggestions: Vec<String>) -> Thrown {
    Thrown::ToolRuntime(Rc::new(ToolRuntimeError { kind, message: message.into(), suggestions }))
}

/// Acorn lines are 1-based and its columns are 0-based. Diagnostics use 1-based columns.
pub fn source_location(node: Loc) -> Location {
    Location { line: node.line, column: node.column + 1 }
}

pub fn format_location(node: Option<Loc>) -> String {
    match node {
        None => String::new(),
        Some(node) => {
            let l = source_location(node);
            format!(" (line {}, col {})", l.line, l.column)
        }
    }
}

/// Gives a failure the source location of the expression that raised it, keeping the first one attached.
pub fn locate(error: Thrown, node: Option<Loc>) -> Thrown {
    if let Thrown::Pending(p) = &error
        && p.node.borrow().is_none()
        && !p.unlocated
        && let Some(node) = node
    {
        *p.node.borrow_mut() = Some(node);
    }
    error
}

/// A failure that never receives a source location.
pub fn unlocated_error(error_type: ErrorType, message: impl Into<String>) -> Thrown {
    Thrown::Pending(Rc::new(PendingThrow {
        error_type,
        message: message.into(),
        node: RefCell::new(None),
        kind: DiagnosticKind::ExecutionFailure,
        suggestions: None,
        value: RefCell::new(None),
        unlocated: true,
    }))
}

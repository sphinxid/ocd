//! Public API: build a runtime over tools and run programs. Port of `codemode.ts` and `interpreter/execute.ts`.

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value as Json};

use crate::data::{json_number, to_boundary};
use crate::host::{Extension, Host};
use crate::interp::errors::{Diagnostic, DiagnosticKind, Thrown};
use crate::interp::eval::Frame;
use crate::interp::exec::{executor, reset_executor};
use crate::interp::globals::{globals, normalize_error, GLOBAL_NAMES};
use crate::interp::{new_env, set_rt, Binding, Builtins, Interp, Interrupt, Site};
use crate::tool_runtime::{Prepared, ToolDescription, ToolEntry, ToolRuntime};

/// Resource budgets enforced during each execution. No defaults: absent means unlimited.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct ExecutionLimits {
    /// Wall-clock milliseconds before interruption (safe integer >= 1).
    pub timeout_ms: Option<f64>,
    /// Maximum number of admitted tool calls (safe integer >= 0).
    pub max_tool_calls: Option<f64>,
    /// Maximum UTF-8 bytes retained from the result and logs; warnings have a separate equal budget.
    pub max_output_bytes: Option<f64>,
}

#[derive(Clone, Copy, Debug, Default)]
struct ResolvedLimits {
    timeout_ms: Option<u64>,
    max_tool_calls: Option<usize>,
    max_output_bytes: Option<usize>,
}

/// Errors constructing a runtime (upstream throws `RangeError`/`TypeError` from `CodeMode.make`).
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum CodeModeError {
    #[error("{0}")]
    Range(String),
    #[error("{0}")]
    Type(String),
}

fn validate(name: &str, value: Option<f64>, minimum: f64) -> Result<Option<f64>, CodeModeError> {
    if let Some(v) = value
        && (!(v.is_finite() && v.fract() == 0.0 && v.abs() <= 9007199254740991.0) || v < minimum)
    {
        return Err(CodeModeError::Range(format!("{name} must be a safe integer greater than or equal to {minimum}.")));
    }
    Ok(value)
}

/// A tool call admitted during an execution.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolCall {
    pub name: String,
}

/// The structured success or diagnostic returned by an execution. Program failures are data.
#[derive(Clone, Debug, PartialEq)]
pub enum ExecResult {
    Success {
        value: Json,
        warnings: Option<Vec<Diagnostic>>,
        logs: Option<Vec<String>>,
        truncated: bool,
        tool_calls: Vec<ToolCall>,
    },
    Failure {
        error: Diagnostic,
        logs: Option<Vec<String>>,
        truncated: bool,
        tool_calls: Vec<ToolCall>,
    },
}

impl ExecResult {
    pub fn ok(&self) -> bool {
        matches!(self, ExecResult::Success { .. })
    }
    pub fn logs(&self) -> Option<&Vec<String>> {
        match self {
            ExecResult::Success { logs, .. } | ExecResult::Failure { logs, .. } => logs.as_ref(),
        }
    }
    pub fn tool_calls(&self) -> &[ToolCall] {
        match self {
            ExecResult::Success { tool_calls, .. } | ExecResult::Failure { tool_calls, .. } => tool_calls,
        }
    }
    /// The upstream `CodeMode.Result` JSON shape, key order included.
    pub fn to_json(&self) -> Json {
        let mut map = Map::new();
        let calls = Json::Array(self.tool_calls().iter().map(|c| serde_json::json!({ "name": c.name })).collect());
        match self {
            ExecResult::Success { value, warnings, logs, truncated, .. } => {
                map.insert("ok".into(), Json::Bool(true));
                map.insert("value".into(), value.clone());
                if let Some(w) = warnings {
                    map.insert("warnings".into(), serde_json::to_value(w).unwrap());
                }
                if let Some(l) = logs {
                    map.insert("logs".into(), serde_json::to_value(l).unwrap());
                }
                if *truncated {
                    map.insert("truncated".into(), Json::Bool(true));
                }
            }
            ExecResult::Failure { error, logs, truncated, .. } => {
                map.insert("ok".into(), Json::Bool(false));
                map.insert("error".into(), serde_json::to_value(error).unwrap());
                if let Some(l) = logs {
                    map.insert("logs".into(), serde_json::to_value(l).unwrap());
                }
                if *truncated {
                    map.insert("truncated".into(), Json::Bool(true));
                }
            }
        }
        map.insert("toolCalls".into(), calls);
        Json::Object(map)
    }
}

/// Extra program globals built inside the interpreter thread (test harnesses only).
#[doc(hidden)]
pub type GlobalsFactory = Arc<dyn Fn(&crate::interp::Rt) -> Vec<(String, crate::value::Value)> + Send + Sync>;

/// A reusable confined runtime over explicit tools.
#[derive(Clone)]
pub struct CodeMode {
    prepared: Arc<Prepared>,
    host: Arc<dyn Host>,
    extensions: Vec<Arc<dyn Extension>>,
    limits: ResolvedLimits,
    globals: Option<GlobalsFactory>,
}

/// Interpreter threads get a large stack: evaluation recursion is deep (call depth limit 10 000).
const STACK_SIZE: usize = 1 << 30;

impl CodeMode {
    /// Creates a runtime (upstream `CodeMode.make`).
    pub fn new(
        tools: Vec<ToolEntry>,
        host: Arc<dyn Host>,
        extensions: Vec<Arc<dyn Extension>>,
        limits: ExecutionLimits,
    ) -> Result<CodeMode, CodeModeError> {
        let prepared = Prepared::new(tools).map_err(CodeModeError::Type)?;
        let resolved = ResolvedLimits {
            timeout_ms: validate("timeoutMs", limits.timeout_ms, 1.0)?.map(|v| v as u64),
            max_tool_calls: validate("maxToolCalls", limits.max_tool_calls, 0.0)?.map(|v| v as usize),
            max_output_bytes: validate("maxOutputBytes", limits.max_output_bytes, 0.0)?.map(|v| v as usize),
        };
        let mut bound: Vec<String> = GLOBAL_NAMES.iter().map(|s| s.to_string()).collect();
        for extension in &extensions {
            for (name, _) in extension.globals() {
                if bound.contains(&name) {
                    return Err(CodeModeError::Type(format!(
                        "Extension \"{}\" global \"{name}\" is already defined.",
                        extension.name()
                    )));
                }
                bound.push(name);
            }
        }
        Ok(CodeMode { prepared: Arc::new(prepared), host, extensions, limits: resolved, globals: None })
    }

    /// Adds host-defined program globals (like upstream `executeProgram`'s `globals`); used by test harnesses.
    #[doc(hidden)]
    pub fn with_program_globals(mut self, factory: GlobalsFactory) -> CodeMode {
        self.globals = Some(factory);
        self
    }

    /// Structured tool descriptions (path, description, generated signature), sorted by path.
    pub fn catalog(&self) -> Vec<ToolDescription> {
        self.prepared.catalog()
    }

    /// Runs one program. The future is `Send`; the interpreter runs on a dedicated thread and host futures are
    /// polled there inside the caller's tokio runtime context. Dropping the future interrupts the execution.
    pub async fn execute(&self, code: &str) -> ExecResult {
        if code.trim_matches(|c: char| (c as u32) < 0x10000 && crate::number::is_js_whitespace(c as u16)).is_empty() {
            return ExecResult::Failure {
                error: Diagnostic::new(DiagnosticKind::ParseError, "Code cannot be empty."),
                logs: None,
                truncated: false,
                tool_calls: Vec::new(),
            };
        }
        let cancelled = Arc::new(AtomicBool::new(false));
        let guard = CancelOnDrop(cancelled.clone());
        let (tx, rx) = tokio::sync::oneshot::channel();
        let this = self.clone();
        let code = code.to_string();
        let handle = tokio::runtime::Handle::try_current().ok();
        let spawned = std::thread::Builder::new().name("codemode".into()).stack_size(STACK_SIZE).spawn(move || {
            let _enter = handle.as_ref().map(|h| h.enter());
            let result = this.run_on_thread(&code, cancelled);
            let _ = tx.send(result);
        });
        if spawned.is_err() {
            return ExecResult::Failure {
                error: Diagnostic::new(DiagnosticKind::ExecutionFailure, "Unable to start the interpreter thread."),
                logs: None,
                truncated: false,
                tool_calls: Vec::new(),
            };
        }
        let result = rx.await.unwrap_or_else(|_| ExecResult::Failure {
            error: Diagnostic::new(DiagnosticKind::ExecutionFailure, "The interpreter thread stopped unexpectedly."),
            logs: None,
            truncated: false,
            tool_calls: Vec::new(),
        });
        drop(guard);
        result
    }

    /// Runs a program synchronously on the current thread (blocking). Prefer [`CodeMode::execute`].
    pub fn execute_blocking(&self, code: &str) -> ExecResult {
        let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().expect("tokio runtime");
        rt.block_on(self.execute(code))
    }

    fn run_on_thread(&self, code: &str, cancelled: Arc<AtomicBool>) -> ExecResult {
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| self.run_program(code, cancelled)));
        let result = result.unwrap_or_else(|_| ExecResult::Failure {
            error: Diagnostic::new(DiagnosticKind::ExecutionFailure, "The interpreter failed unexpectedly."),
            logs: None,
            truncated: false,
            tool_calls: Vec::new(),
        });
        // Break reference cycles so every object of this run is released.
        reset_executor();
        set_rt(None);
        crate::interp::teardown_envs();
        crate::object::teardown_registry();
        match self.limits.max_output_bytes {
            Some(max) => bound_output(result, max),
            None => result,
        }
    }

    fn run_program(&self, code: &str, cancelled: Arc<AtomicBool>) -> ExecResult {
        let deadline = self.limits.timeout_ms.map(|ms| Instant::now() + Duration::from_millis(ms));
        let rt = Rc::new(Interp {
            builtins: Builtins::new(),
            pending: Default::default(),
            tools: ToolRuntime::new(self.prepared.clone(), self.host.clone(), self.limits.max_tool_calls),
            logs: RefCell::new(Vec::new()),
            templates: RefCell::new(HashMap::new()),
            global: RefCell::new(None),
            interrupt: Interrupt { deadline, cancelled: cancelled.clone(), counter: Cell::new(0), fired: Cell::new(false) },
            extensions: self.extensions.clone(),
            var_names: RefCell::new(HashMap::new()),
            arguments_use: RefCell::new(HashMap::new()),
        });
        set_rt(Some(rt.clone()));
        let logs = |rt: &Rc<Interp>| {
            let l = rt.logs.borrow();
            if l.is_empty() { None } else { Some(l.clone()) }
        };
        let calls = |rt: &Rc<Interp>| rt.tools.calls.borrow().iter().map(|n| ToolCall { name: n.clone() }).collect::<Vec<_>>();

        let program = match crate::parser::parse(code) {
            Ok(p) => Rc::new(p),
            Err(e) => {
                return ExecResult::Failure {
                    error: Diagnostic::new(DiagnosticKind::ParseError, e.message),
                    logs: None,
                    truncated: false,
                    tool_calls: Vec::new(),
                };
            }
        };
        // Program code has no receiver: top-level `this` is undefined, as in a module.
        let global = new_env(None);
        global.set_binding(Rc::from("this"), Binding { mutable: false, value: crate::value::Value::Undefined, initialized: true });
        for (name, value) in globals(&rt) {
            global.set_binding(Rc::from(name), Binding { mutable: false, value, initialized: true });
        }
        for (name, value) in crate::interp::extensions::extension_globals(&rt) {
            global.set_binding(Rc::from(name.as_str()), Binding { mutable: false, value, initialized: true });
        }
        if let Some(factory) = &self.globals {
            for (name, value) in factory(&rt) {
                global.set_binding(Rc::from(name.as_str()), Binding { mutable: false, value, initialized: true });
            }
        }
        *rt.global.borrow_mut() = Some(global.clone());

        let rt2 = rt.clone();
        let returned: Rc<RefCell<Option<Json>>> = Rc::new(RefCell::new(None));
        let slot = returned.clone();
        let root = executor().spawn(async move {
            let frame = Frame::new(rt2.clone(), 0, Site::default());
            let value = frame.run(&global, &program).await?;
            let json = to_boundary(&rt2, value, Site::default()).await?;
            *slot.borrow_mut() = Some(json.unwrap_or(Json::Null));
            Ok(crate::value::Value::Undefined)
        });
        let exec = executor();
        let finished = exec.run_until(&|| root.is_done(), deadline, &|| cancelled.load(Ordering::Relaxed));
        let timeout_failure = |rt: &Rc<Interp>| ExecResult::Failure {
            error: Diagnostic::new(
                DiagnosticKind::TimeoutExceeded,
                format!("Execution timed out after {}ms.", self.limits.timeout_ms.unwrap_or(0)),
            ),
            logs: logs(rt),
            truncated: false,
            tool_calls: calls(rt),
        };
        if !finished {
            rt.pending.interrupt();
            return timeout_failure(&rt);
        }
        match root.exit().unwrap() {
            Ok(_) => {
                let value = returned.borrow_mut().take().unwrap_or(Json::Null);
                let warnings = rt.pending.interrupt();
                ExecResult::Success {
                    value: normalize_numbers(value),
                    warnings: if warnings.is_empty() { None } else { Some(warnings) },
                    logs: logs(&rt),
                    truncated: false,
                    tool_calls: calls(&rt),
                }
            }
            Err(Thrown::Interrupt) => {
                rt.pending.interrupt();
                if cancelled.load(Ordering::Relaxed) || self.limits.timeout_ms.is_some() {
                    return timeout_failure(&rt);
                }
                timeout_failure(&rt)
            }
            Err(e) => {
                let error = normalize_error(&e);
                rt.pending.interrupt();
                ExecResult::Failure { error, logs: logs(&rt), truncated: false, tool_calls: calls(&rt) }
            }
        }
    }
}

fn normalize_numbers(value: Json) -> Json {
    match value {
        Json::Number(n) => json_number(n.as_f64().unwrap_or(0.0)),
        Json::Array(a) => Json::Array(a.into_iter().map(normalize_numbers).collect()),
        Json::Object(m) => Json::Object(m.into_iter().map(|(k, v)| (k, normalize_numbers(v))).collect()),
        other => other,
    }
}

struct CancelOnDrop(Arc<AtomicBool>);

impl Drop for CancelOnDrop {
    fn drop(&mut self) {
        self.0.store(true, Ordering::Relaxed);
    }
}

fn utf8_truncate(value: &str, max_bytes: usize) -> &str {
    if value.len() <= max_bytes {
        return value;
    }
    let mut end = max_bytes;
    while end > 0 && !value.is_char_boundary(end) {
        end -= 1;
    }
    &value[..end]
}

// Warnings have a separate budget so result data cannot starve diagnostics.
fn bound_output(result: ExecResult, max: usize) -> ExecResult {
    let mut truncated = false;
    let mut value = Json::Null;
    let mut value_bytes = 0;
    let (warnings, logs) = match &result {
        ExecResult::Success { value: v, warnings, logs, .. } => {
            let serialized = crate::data::stringify(v, "");
            let bytes = serialized.len();
            if bytes > max {
                truncated = true;
                value = Json::String(format!(
                    "{} [result truncated: {bytes} bytes exceeds the {max}-byte output limit; return a smaller value]",
                    utf8_truncate(&serialized, max)
                ));
                value_bytes = max;
            } else {
                value = v.clone();
                value_bytes = bytes;
            }
            (warnings.clone().unwrap_or_default(), logs.clone().unwrap_or_default())
        }
        ExecResult::Failure { logs, .. } => (Vec::new(), logs.clone().unwrap_or_default()),
    };
    let mut kept_warnings = Vec::new();
    let mut warning_bytes = 0;
    for w in &warnings {
        let bytes = serde_json::to_string(w).unwrap().len() + 1;
        if warning_bytes + bytes > max {
            break;
        }
        warning_bytes += bytes;
        kept_warnings.push(w.clone());
    }
    if kept_warnings.len() < warnings.len() {
        truncated = true;
        kept_warnings.push(Diagnostic::new(
            DiagnosticKind::Truncated,
            format!("{} additional warnings omitted by the output limit.", warnings.len() - kept_warnings.len()),
        ));
    }
    let mut kept = Vec::new();
    let budget = max.saturating_sub(value_bytes);
    let mut log_bytes = 0;
    for line in &logs {
        let bytes = line.len() + 1;
        if log_bytes + bytes > budget {
            break;
        }
        log_bytes += bytes;
        kept.push(line.clone());
    }
    if kept.len() < logs.len() {
        truncated = true;
        kept.push(format!("[logs truncated: showing {} of {} lines]", kept.len(), logs.len()));
    }
    if !truncated {
        return result;
    }
    let logs_part = if kept.is_empty() { None } else { Some(kept) };
    match result {
        ExecResult::Success { tool_calls, .. } => ExecResult::Success {
            value,
            warnings: if kept_warnings.is_empty() { None } else { Some(kept_warnings) },
            logs: logs_part,
            truncated: true,
            tool_calls,
        },
        ExecResult::Failure { error, tool_calls, .. } => {
            ExecResult::Failure { error, logs: logs_part, truncated: true, tool_calls }
        }
    }
}

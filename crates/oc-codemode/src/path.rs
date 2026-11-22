//! The experimental `path` global (core's `codemode/path.ts`, upstream 2.3.1 #64476, on only with `full_codemode`):
//! Node's `path` module as Bun 1.4 gives it, with `path.posix` and `path.win32`, where `resolve` and `relative` start
//! from the session's directory instead of the server's working directory.
//!
//! Upstream hands the program the host's own module, so this is a port of Node's `lib/path.js` (both flavours, on
//! UTF-16 code units like the original) with Bun's argument checks and error texts, and of Bun's glob matching for
//! `matchesGlob` (see [`crate::glob`]). Only functions and separators cross: `posix` and `win32` point back at their
//! module, which a global can't hold, so each becomes a copy of its own functions. The host's flavour (POSIX, or
//! Win32 on Windows) resolves from the session's directory; the other can't read that directory as absolute
//! (`C:\proj` isn't a POSIX path), so it resolves from its own root (`/` or `\`).

use std::sync::Arc;

use crate::host::{Extension, GlobalsExtension, HostCall, HostFn, HostValue};

/// Which of Node's path implementations.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Flavour {
    Posix,
    Win32,
}

impl Flavour {
    /// The flavour Node gives as `path` on this platform: Win32 on Windows, POSIX elsewhere.
    pub fn host() -> Flavour {
        if cfg!(windows) { Flavour::Win32 } else { Flavour::Posix }
    }
    pub fn sep(self) -> &'static str {
        match self {
            Flavour::Posix => "/",
            Flavour::Win32 => "\\",
        }
    }
    pub fn delimiter(self) -> &'static str {
        match self {
            Flavour::Posix => ":",
            Flavour::Win32 => ";",
        }
    }
}

/// The `path` extension for a run in `directory` (the session's directory): the host's flavour as `path`, with
/// `path.posix` and `path.win32`.
pub fn extension(directory: impl Into<String>) -> Arc<dyn Extension> {
    extension_for(Flavour::host(), directory)
}

/// The `path` extension as it is on a host whose own flavour is `host` (tests run both on every platform).
pub fn extension_for(host: Flavour, directory: impl Into<String>) -> Arc<dyn Extension> {
    extension_in(host, directory, process_cwd())
}

/// As [`extension_for`], with `cwd` as the server's working directory, which Bun's `win32.resolve` falls back to for
/// a drive-relative path (`D:x`) or, from `win32.toNamespacedPath`, a relative one.
pub fn extension_in(host: Flavour, directory: impl Into<String>, cwd: impl Into<String>) -> Arc<dyn Extension> {
    let directory = directory.into();
    let cwd = cwd.into();
    let flavour = |module: Flavour| {
        let base = if module == host { directory.clone() } else { module.sep().to_string() };
        flavour_object(Module { flavour: module, cwd: cwd.clone() }, base)
    };
    let HostValue::Object(mut fields) = flavour(host) else { unreachable!() };
    fields.push(("posix".into(), flavour(Flavour::Posix)));
    fields.push(("win32".into(), flavour(Flavour::Win32)));
    Arc::new(GlobalsExtension { name: "path".into(), modules: Vec::new(), globals: vec![("path".into(), HostValue::Object(fields))] })
}

// The server's working directory, which Bun's `win32.resolve` falls back to.
fn process_cwd() -> String {
    std::env::current_dir().map(|d| d.to_string_lossy().into_owned()).unwrap_or_else(|_| "/".into())
}

// One flavour as the program sees it (`flavor` in core's path.ts): Node's own functions, named as Bun binds them,
// with `resolve` and `relative` starting from `base`.
fn flavour_object(module: Module, base: String) -> HostValue {
    let bound = |name: &'static str, length: usize| {
        let module = module.clone();
        HostFn::new(format!("bound {name}"), length, move |args| HostCall::Ready(module.call(name, &args))).into_value()
    };
    let resolve = {
        let (module, base) = (module.clone(), base.clone());
        HostFn::new("resolve", 0, move |args| {
            let mut all = vec![HostValue::String(base.clone())];
            all.extend(args);
            HostCall::Ready(module.call("resolve", &all))
        })
        .into_value()
    };
    let relative = {
        let module = module.clone();
        HostFn::new("relative", 2, move |args| {
            let at = |i: usize| args.get(i).cloned().unwrap_or(HostValue::Undefined);
            let from = module.call("resolve", &[HostValue::String(base.clone()), at(0)]);
            let to = from.and_then(|from| Ok((from, module.call("resolve", &[HostValue::String(base.clone()), at(1)])?)));
            HostCall::Ready(to.and_then(|(from, to)| module.call("relative", &[from, to])))
        })
        .into_value()
    };
    HostValue::Object(vec![
        ("resolve".into(), resolve),
        ("normalize".into(), bound("normalize", 1)),
        ("isAbsolute".into(), bound("isAbsolute", 1)),
        ("join".into(), bound("join", 1)),
        ("relative".into(), relative),
        ("toNamespacedPath".into(), bound("toNamespacedPath", 1)),
        ("dirname".into(), bound("dirname", 1)),
        ("basename".into(), bound("basename", 1)),
        ("extname".into(), bound("extname", 1)),
        ("format".into(), bound("format", 1)),
        ("parse".into(), bound("parse", 1)),
        ("sep".into(), HostValue::str(module.flavour.sep())),
        ("delimiter".into(), HostValue::str(module.flavour.delimiter())),
        ("_makeLong".into(), bound("toNamespacedPath", 1)),
        ("matchesGlob".into(), bound("matchesGlob", 2)),
    ])
}

/// One flavour of Node's path module, as Bun gives it; `cwd` is the working directory its `win32.resolve` falls
/// back to.
#[derive(Clone, Debug)]
pub struct Module {
    pub flavour: Flavour,
    pub cwd: String,
}

type Units = Vec<u16>;

const SLASH: u16 = b'/' as u16;
const BACKSLASH: u16 = b'\\' as u16;
const DOT: u16 = b'.' as u16;
const COLON: u16 = b':' as u16;
const QUESTION: u16 = b'?' as u16;

fn units(s: &str) -> Units {
    s.encode_utf16().collect()
}

fn text(u: &[u16]) -> String {
    String::from_utf16_lossy(u)
}

fn is_posix_sep(c: u16) -> bool {
    c == SLASH
}

fn is_sep(c: u16) -> bool {
    c == SLASH || c == BACKSLASH
}

fn is_device_root(c: u16) -> bool {
    (b'a' as u16..=b'z' as u16).contains(&c) || (b'A' as u16..=b'Z' as u16).contains(&c)
}

fn at(s: &[u16], i: usize) -> u16 {
    s.get(i).copied().unwrap_or(0)
}

fn last_index_of(s: &[u16], c: u16) -> isize {
    s.iter().rposition(|&x| x == c).map(|i| i as isize).unwrap_or(-1)
}

// Bun's `validateString` for the path functions: `The "<name>" property must be of type string, got <type>`.
fn type_error(message: String) -> HostValue {
    HostValue::error("TypeError", message)
}

fn type_name(value: &HostValue) -> &'static str {
    match value {
        HostValue::Undefined => "undefined",
        HostValue::Null => "object",
        HostValue::Bool(_) => "boolean",
        HostValue::Number(_) => "number",
        HostValue::String(_) => "string",
        HostValue::Array(_) => "array",
        HostValue::Function(_) => "function",
        HostValue::Unrepresentable(what) if what == "a symbol" => "symbol",
        HostValue::Unrepresentable(what) if what == "a bigint" => "bigint",
        _ => "object",
    }
}

fn string_arg(value: &HostValue, name: &str) -> Result<Units, HostValue> {
    match value {
        HostValue::String(s) => Ok(units(s)),
        other => Err(type_error(format!("The \"{name}\" property must be of type string, got {}", type_name(other)))),
    }
}

// Node's own `validateString` (Bun's `matchesGlob` checks its arguments in JavaScript):
// `The "<name>" argument must be of type string. Received <what>`.
fn node_string_arg(value: &HostValue, name: &str) -> Result<String, HostValue> {
    let received = match value {
        HostValue::String(s) => return Ok(s.clone()),
        HostValue::Undefined => "undefined".to_string(),
        HostValue::Null => "null".to_string(),
        HostValue::Bool(b) => format!("type boolean ({b})"),
        HostValue::Number(n) => format!("type number ({})", if *n == 0.0 { "0".into() } else { crate::number::to_string(*n) }),
        HostValue::Function(f) => format!("function {}", f.name()),
        HostValue::Unrepresentable(what) if what == "a symbol" => "type symbol (Symbol())".to_string(),
        HostValue::Unrepresentable(what) if what == "a bigint" => "type bigint".to_string(),
        other => format!("an instance of {}", class_name(other)),
    };
    Err(type_error(format!("The \"{name}\" argument must be of type string. Received {received}")))
}

// The class of an object the host received: what a program value crossed as.
fn class_name(value: &HostValue) -> String {
    match value {
        HostValue::Array(_) => "Array".into(),
        HostValue::Date(_) => "Date".into(),
        HostValue::RegExp { .. } => "RegExp".into(),
        HostValue::Url(_) => "URL".into(),
        HostValue::SearchParams(_) => "URLSearchParams".into(),
        HostValue::Headers(_) => "Headers".into(),
        HostValue::Map(_) => "Map".into(),
        HostValue::Set(_) => "Set".into(),
        HostValue::Bytes(_) => "Uint8Array".into(),
        HostValue::Buffer(_) => "Buffer".into(),
        HostValue::Error { name, .. } => match crate::interp::errors::ErrorType::from_name(name) {
            Some(_) => name.clone(),
            None => "Error".into(),
        },
        _ => "Object".into(),
    }
}

// JavaScript truthiness of a host value.
fn truthy(value: &HostValue) -> bool {
    match value {
        HostValue::Undefined | HostValue::Null => false,
        HostValue::Bool(b) => *b,
        HostValue::Number(n) => *n != 0.0 && !n.is_nan(),
        HostValue::String(s) => !s.is_empty(),
        _ => true,
    }
}

// `String(value)` of a host value, as a template literal gives it.
fn to_js_string(value: &HostValue) -> String {
    match value {
        HostValue::Undefined => "undefined".into(),
        HostValue::Null => "null".into(),
        HostValue::Bool(b) => b.to_string(),
        HostValue::Number(n) => crate::number::to_string(*n),
        HostValue::String(s) => s.clone(),
        HostValue::Array(items) => items
            .iter()
            .map(|item| match item {
                HostValue::Undefined | HostValue::Null => String::new(),
                other => to_js_string(other),
            })
            .collect::<Vec<_>>()
            .join(","),
        HostValue::Date(t) => crate::stdlib::date::to_display(*t),
        HostValue::RegExp { source, flags } => format!("/{source}/{flags}"),
        HostValue::Url(href) => href.clone(),
        HostValue::SearchParams(pairs) => url::form_urlencoded::Serializer::new(String::new()).extend_pairs(pairs).finish(),
        HostValue::Bytes(bytes) => bytes.iter().map(|b| b.to_string()).collect::<Vec<_>>().join(","),
        HostValue::Buffer(bytes) => String::from_utf8_lossy(bytes).into_owned(),
        HostValue::Error { name, message, .. } => match (name.is_empty(), message.is_empty()) {
            (_, true) => name.clone(),
            (true, false) => message.clone(),
            _ => format!("{name}: {message}"),
        },
        HostValue::Function(f) => format!("function {}() {{\n    [native code]\n}}", f.name()),
        HostValue::Map(_) => "[object Map]".into(),
        HostValue::Set(_) => "[object Set]".into(),
        HostValue::Headers(_) => "[object Headers]".into(),
        _ => "[object Object]".into(),
    }
}

impl Module {
    pub fn new(flavour: Flavour, cwd: impl Into<String>) -> Module {
        Module { flavour, cwd: cwd.into() }
    }

    fn win32(&self) -> bool {
        self.flavour == Flavour::Win32
    }

    /// Calls the module's function `name` (`resolve`, `normalize`, `isAbsolute`, `join`, `relative`,
    /// `toNamespacedPath`, `dirname`, `basename`, `extname`, `format`, `parse`, `matchesGlob`) as a program would,
    /// with Bun's checks; `Err` is the error it throws.
    pub fn call(&self, name: &str, args: &[HostValue]) -> Result<HostValue, HostValue> {
        let arg = |i: usize| args.get(i).cloned().unwrap_or(HostValue::Undefined);
        let s = |u: Units| HostValue::String(text(&u));
        Ok(match name {
            "resolve" => {
                let mut paths = Vec::with_capacity(args.len());
                // POSIX stops at the first absolute path from the end, so earlier arguments aren't checked.
                for (i, value) in args.iter().enumerate().rev() {
                    let path = string_arg(value, &format!("paths[{i}]"))?;
                    let absolute = at(&path, 0) == SLASH;
                    paths.push(path);
                    if !self.win32() && absolute {
                        break;
                    }
                }
                paths.reverse();
                s(if self.win32() { self.win32_resolve(&paths) } else { posix_resolve(&paths, &self.cwd) })
            }
            "normalize" => {
                let path = string_arg(&arg(0), "path")?;
                s(if self.win32() { win32_normalize(&path) } else { posix_normalize(&path) })
            }
            "isAbsolute" => {
                let path = string_arg(&arg(0), "path")?;
                HostValue::Bool(if self.win32() { win32_is_absolute(&path) } else { at(&path, 0) == SLASH })
            }
            "join" => {
                let mut paths = Vec::with_capacity(args.len());
                for (i, value) in args.iter().enumerate() {
                    paths.push(string_arg(value, &format!("paths[{i}]"))?);
                }
                s(if self.win32() { win32_join(&paths) } else { posix_join(&paths) })
            }
            "relative" => {
                let from = string_arg(&arg(0), "from")?;
                let to = string_arg(&arg(1), "to")?;
                s(if self.win32() { self.win32_relative(&from, &to) } else { posix_relative(&from, &to, &self.cwd) })
            }
            "toNamespacedPath" => match arg(0) {
                HostValue::String(path) if self.win32() && !path.is_empty() => s(self.win32_to_namespaced(&units(&path))),
                other => other,
            },
            "dirname" => {
                let path = string_arg(&arg(0), "path")?;
                s(if self.win32() { win32_dirname(&path) } else { posix_dirname(&path) })
            }
            "basename" => {
                let suffix = match arg(1) {
                    HostValue::Undefined => None,
                    other => Some(string_arg(&other, "ext")?),
                };
                let path = string_arg(&arg(0), "path")?;
                s(basename(&path, suffix.as_deref(), self.win32()))
            }
            "extname" => {
                let path = string_arg(&arg(0), "path")?;
                s(extname(&path, self.win32()))
            }
            "format" => self.format(&arg(0))?,
            "parse" => {
                let path = string_arg(&arg(0), "path")?;
                let parsed = if self.win32() { win32_parse(&path) } else { posix_parse(&path) };
                HostValue::Object(
                    ["root", "dir", "base", "ext", "name"]
                        .iter()
                        .zip(parsed)
                        .map(|(key, value)| (key.to_string(), s(value)))
                        .collect(),
                )
            }
            "matchesGlob" => {
                let path = node_string_arg(&arg(0), "path")?;
                let pattern = node_string_arg(&arg(1), "pattern")?;
                // Bun matches a Windows path and pattern with every backslash as `/` (so nothing is escaped).
                let (path, pattern) =
                    if self.win32() { (path.replace('\\', "/"), pattern.replace('\\', "/")) } else { (path, pattern) };
                HostValue::Bool(crate::glob::matches(&pattern, &path))
            }
            other => return Err(type_error(format!("path.{other} is not a function"))),
        })
    }

    // `_format`: `${dir}${sep}${base}`, where `dir` is `dir || root` and `base` is `base || name + ext`.
    fn format(&self, object: &HostValue) -> Result<HostValue, HostValue> {
        let fields: &[(String, HostValue)] = match object {
            HostValue::Object(fields) => fields,
            HostValue::Undefined
            | HostValue::Null
            | HostValue::Bool(_)
            | HostValue::Number(_)
            | HostValue::String(_)
            | HostValue::Array(_)
            | HostValue::Function(_) => {
                return Err(type_error(format!(
                    "The \"pathObject\" property must be of type object, got {}",
                    type_name(object)
                )));
            }
            HostValue::Unrepresentable(what) if what == "a symbol" || what == "a bigint" => {
                return Err(type_error(format!(
                    "The \"pathObject\" property must be of type object, got {}",
                    type_name(object)
                )));
            }
            // Other objects have none of the fields.
            _ => &[],
        };
        let field = |key: &str| fields.iter().rev().find(|(k, _)| k == key).map(|(_, v)| v.clone()).unwrap_or(HostValue::Undefined);
        let root = field("root");
        let dir = match field("dir") {
            dir if truthy(&dir) => dir,
            _ => root.clone(),
        };
        let base = match field("base") {
            base if truthy(&base) => to_js_string(&base),
            _ => {
                let name = field("name");
                let name = if truthy(&name) { to_js_string(&name) } else { String::new() };
                let ext = field("ext");
                let ext = if truthy(&ext) {
                    let dotted = match &ext {
                        HostValue::String(s) => s.starts_with('.'),
                        HostValue::Array(items) => matches!(items.first(), Some(HostValue::String(s)) if s == "."),
                        _ => false,
                    };
                    format!("{}{}", if dotted { "" } else { "." }, to_js_string(&ext))
                } else {
                    String::new()
                };
                format!("{name}{ext}")
            }
        };
        if !truthy(&dir) {
            return Ok(HostValue::String(base));
        }
        let same = match (&dir, &root) {
            (HostValue::String(a), HostValue::String(b)) => a == b,
            (HostValue::Number(a), HostValue::Number(b)) => a == b,
            (HostValue::Bool(a), HostValue::Bool(b)) => a == b,
            _ => false,
        };
        let dir = to_js_string(&dir);
        Ok(HostValue::String(if same { format!("{dir}{base}") } else { format!("{dir}{}{base}", self.flavour.sep()) }))
    }

    fn win32_cwd(&self) -> Units {
        units(&self.cwd)
    }

    fn win32_resolve(&self, args: &[Units]) -> Units {
        let mut resolved_device: Units = Vec::new();
        let mut resolved_tail: Units = Vec::new();
        let mut resolved_absolute = false;
        let mut i = args.len() as isize - 1;
        while i >= -1 {
            let path: Units = if i >= 0 {
                let path = args[i as usize].clone();
                if path.is_empty() {
                    i -= 1;
                    continue;
                }
                path
            } else if resolved_device.is_empty() {
                self.win32_cwd()
            } else {
                // No per-drive working directories here (`process.env["=C:"]`): the working directory, unless it
                // is on another drive.
                let cwd = self.win32_cwd();
                let lower = |u: &[u16]| text(u).to_lowercase();
                if lower(&cwd[..cwd.len().min(2)]) != lower(&resolved_device) && at(&cwd, 2) == BACKSLASH {
                    let mut device = resolved_device.clone();
                    device.push(BACKSLASH);
                    device
                } else {
                    cwd
                }
            };
            let (device, root_end, is_absolute) = win32_root(&path, false);
            if !device.is_empty() {
                if !resolved_device.is_empty() {
                    if text(&device).to_lowercase() != text(&resolved_device).to_lowercase() {
                        i -= 1;
                        continue;
                    }
                } else {
                    resolved_device = device;
                }
            }
            if resolved_absolute {
                if !resolved_device.is_empty() {
                    break;
                }
            } else {
                let mut tail = path[root_end..].to_vec();
                tail.push(BACKSLASH);
                tail.extend_from_slice(&resolved_tail);
                resolved_tail = tail;
                resolved_absolute = is_absolute;
                if is_absolute && !resolved_device.is_empty() {
                    break;
                }
            }
            i -= 1;
        }
        let tail = normalize_string(&resolved_tail, !resolved_absolute, BACKSLASH, is_sep);
        let mut out = resolved_device;
        if resolved_absolute {
            out.push(BACKSLASH);
            out.extend_from_slice(&tail);
            out
        } else {
            out.extend_from_slice(&tail);
            if out.is_empty() { vec![DOT] } else { out }
        }
    }

    fn win32_relative(&self, from: &[u16], to: &[u16]) -> Units {
        if from == to {
            return Vec::new();
        }
        let from_orig = self.win32_resolve(&[from.to_vec()]);
        let to_orig = self.win32_resolve(&[to.to_vec()]);
        if from_orig == to_orig {
            return Vec::new();
        }
        let from = units(&text(&from_orig).to_lowercase());
        let to = units(&text(&to_orig).to_lowercase());
        if from == to {
            return Vec::new();
        }
        let mut from_start = 0;
        while from_start < from.len() && from[from_start] == BACKSLASH {
            from_start += 1;
        }
        let mut from_end = from.len();
        while from_end > from_start + 1 && from[from_end - 1] == BACKSLASH {
            from_end -= 1;
        }
        let from_len = from_end - from_start;
        let mut to_start = 0;
        while to_start < to.len() && to[to_start] == BACKSLASH {
            to_start += 1;
        }
        let mut to_end = to.len();
        while to_end > to_start + 1 && to[to_end - 1] == BACKSLASH {
            to_end -= 1;
        }
        let to_len = to_end - to_start;
        let length = from_len.min(to_len);
        let mut last_common_sep: isize = -1;
        let mut i = 0;
        while i < length {
            let code = from[from_start + i];
            if code != to[to_start + i] {
                break;
            } else if code == BACKSLASH {
                last_common_sep = i as isize;
            }
            i += 1;
        }
        let slice = |s: &[u16], a: usize, b: usize| -> Units { s[a.min(s.len())..b.min(s.len()).max(a.min(s.len()))].to_vec() };
        if i != length {
            if last_common_sep == -1 {
                return to_orig;
            }
        } else {
            if to_len > length {
                if at(&to, to_start + i) == BACKSLASH {
                    return slice(&to_orig, to_start + i + 1, to_orig.len());
                }
                if i == 2 {
                    return slice(&to_orig, to_start + i, to_orig.len());
                }
            }
            if from_len > length {
                if at(&from, from_start + i) == BACKSLASH {
                    last_common_sep = i as isize;
                } else if i == 2 {
                    last_common_sep = 3;
                }
            }
            if last_common_sep == -1 {
                last_common_sep = 0;
            }
        }
        let mut out: Units = Vec::new();
        let mut j = (from_start as isize + last_common_sep + 1) as usize;
        while j <= from_end {
            if j == from_end || from[j] == BACKSLASH {
                if out.is_empty() {
                    out.extend_from_slice(&[DOT, DOT]);
                } else {
                    out.extend_from_slice(&[BACKSLASH, DOT, DOT]);
                }
            }
            j += 1;
        }
        let mut to_start = (to_start as isize + last_common_sep) as usize;
        if !out.is_empty() {
            out.extend(slice(&to_orig, to_start, to_end));
            return out;
        }
        if at(&to_orig, to_start) == BACKSLASH {
            to_start += 1;
        }
        slice(&to_orig, to_start, to_end)
    }

    fn win32_to_namespaced(&self, path: &[u16]) -> Units {
        let resolved = self.win32_resolve(&[path.to_vec()]);
        if resolved.len() <= 2 {
            return path.to_vec();
        }
        if resolved[0] == BACKSLASH {
            if resolved[1] == BACKSLASH {
                let code = resolved[2];
                if code != QUESTION && code != DOT {
                    let mut out = units("\\\\?\\UNC\\");
                    out.extend_from_slice(&resolved[2..]);
                    return out;
                }
            }
        } else if is_device_root(resolved[0]) && resolved[1] == COLON && resolved[2] == BACKSLASH {
            let mut out = units("\\\\?\\");
            out.extend_from_slice(&resolved);
            return out;
        }
        resolved
    }
}

// `normalizeString`: resolves `.` and `..` segments.
fn normalize_string(path: &[u16], allow_above_root: bool, separator: u16, is_separator: fn(u16) -> bool) -> Units {
    let mut res: Units = Vec::new();
    let mut last_segment_length: isize = 0;
    let mut last_slash: isize = -1;
    let mut dots: i32 = 0;
    let mut code: u16 = 0;
    let mut i: isize = 0;
    while i <= path.len() as isize {
        if (i as usize) < path.len() {
            code = path[i as usize];
        } else if is_separator(code) {
            break;
        } else {
            code = SLASH;
        }
        if is_separator(code) {
            if last_slash == i - 1 || dots == 1 {
                // A repeated separator or a `.` segment.
            } else if dots == 2 {
                let n = res.len();
                if n < 2 || last_segment_length != 2 || res[n - 1] != DOT || res[n - 2] != DOT {
                    if n > 2 {
                        let last_slash_index = last_index_of(&res, separator);
                        if last_slash_index == -1 {
                            res.clear();
                            last_segment_length = 0;
                        } else {
                            res.truncate(last_slash_index as usize);
                            last_segment_length = res.len() as isize - 1 - last_index_of(&res, separator);
                        }
                        last_slash = i;
                        dots = 0;
                        i += 1;
                        continue;
                    } else if n != 0 {
                        res.clear();
                        last_segment_length = 0;
                        last_slash = i;
                        dots = 0;
                        i += 1;
                        continue;
                    }
                }
                if allow_above_root {
                    if !res.is_empty() {
                        res.push(separator);
                    }
                    res.extend_from_slice(&[DOT, DOT]);
                    last_segment_length = 2;
                }
            } else {
                if !res.is_empty() {
                    res.push(separator);
                }
                res.extend_from_slice(&path[(last_slash + 1) as usize..i as usize]);
                last_segment_length = i - last_slash - 1;
            }
            last_slash = i;
            dots = 0;
        } else if code == DOT && dots != -1 {
            dots += 1;
        } else {
            dots = -1;
        }
        i += 1;
    }
    res
}

fn posix_resolve(args: &[Units], cwd: &str) -> Units {
    let mut resolved: Units = Vec::new();
    let mut absolute = false;
    let cwd = units(cwd);
    let mut i = args.len() as isize - 1;
    while i >= -1 && !absolute {
        let path: &[u16] = if i >= 0 { &args[i as usize] } else { &cwd };
        i -= 1;
        if path.is_empty() {
            continue;
        }
        let mut next = path.to_vec();
        next.push(SLASH);
        next.extend_from_slice(&resolved);
        resolved = next;
        absolute = path[0] == SLASH;
    }
    let normalized = normalize_string(&resolved, !absolute, SLASH, is_posix_sep);
    if absolute {
        let mut out = vec![SLASH];
        out.extend(normalized);
        out
    } else if normalized.is_empty() {
        vec![DOT]
    } else {
        normalized
    }
}

fn posix_normalize(path: &[u16]) -> Units {
    if path.is_empty() {
        return vec![DOT];
    }
    let absolute = path[0] == SLASH;
    let trailing = path[path.len() - 1] == SLASH;
    let mut out = normalize_string(path, !absolute, SLASH, is_posix_sep);
    if out.is_empty() {
        if absolute {
            return vec![SLASH];
        }
        return if trailing { units("./") } else { vec![DOT] };
    }
    if trailing {
        out.push(SLASH);
    }
    if absolute {
        out.insert(0, SLASH);
    }
    out
}

fn posix_join(paths: &[Units]) -> Units {
    let parts: Vec<&Units> = paths.iter().filter(|p| !p.is_empty()).collect();
    if parts.is_empty() {
        return vec![DOT];
    }
    let joined = parts.iter().enumerate().fold(Vec::new(), |mut out, (i, p)| {
        if i > 0 {
            out.push(SLASH);
        }
        out.extend_from_slice(p);
        out
    });
    posix_normalize(&joined)
}

fn posix_relative(from: &[u16], to: &[u16], cwd: &str) -> Units {
    if from == to {
        return Vec::new();
    }
    let from = posix_resolve(&[from.to_vec()], cwd);
    let to = posix_resolve(&[to.to_vec()], cwd);
    if from == to {
        return Vec::new();
    }
    let from_start = 1;
    let from_end = from.len();
    let from_len = from_end - from_start;
    let to_start = 1;
    let to_len = to.len() - to_start;
    let length = from_len.min(to_len);
    let mut last_common_sep: isize = -1;
    let mut i = 0;
    while i < length {
        let code = from[from_start + i];
        if code != to[to_start + i] {
            break;
        } else if code == SLASH {
            last_common_sep = i as isize;
        }
        i += 1;
    }
    if i == length {
        if to_len > length {
            if to[to_start + i] == SLASH {
                return to[to_start + i + 1..].to_vec();
            }
            if i == 0 {
                return to[to_start + i..].to_vec();
            }
        } else if from_len > length {
            if from[from_start + i] == SLASH {
                last_common_sep = i as isize;
            } else if i == 0 {
                last_common_sep = 0;
            }
        }
    }
    let mut out: Units = Vec::new();
    let mut j = (from_start as isize + last_common_sep + 1) as usize;
    while j <= from_end {
        if j == from_end || from[j] == SLASH {
            if out.is_empty() {
                out.extend_from_slice(&[DOT, DOT]);
            } else {
                out.extend_from_slice(&[SLASH, DOT, DOT]);
            }
        }
        j += 1;
    }
    out.extend_from_slice(&to[(to_start as isize + last_common_sep) as usize..]);
    out
}

fn posix_dirname(path: &[u16]) -> Units {
    if path.is_empty() {
        return vec![DOT];
    }
    let has_root = path[0] == SLASH;
    let mut end: isize = -1;
    let mut matched_slash = true;
    let mut i = path.len() - 1;
    while i >= 1 {
        if path[i] == SLASH {
            if !matched_slash {
                end = i as isize;
                break;
            }
        } else {
            matched_slash = false;
        }
        i -= 1;
    }
    if end == -1 {
        return if has_root { vec![SLASH] } else { vec![DOT] };
    }
    if has_root && end == 1 {
        return vec![SLASH, SLASH];
    }
    path[..end as usize].to_vec()
}

fn basename(path: &[u16], suffix: Option<&[u16]>, win32: bool) -> Units {
    let sep: fn(u16) -> bool = if win32 { is_sep } else { is_posix_sep };
    let mut start: usize = 0;
    let mut end: isize = -1;
    let mut matched_slash = true;
    if win32 && path.len() >= 2 && is_device_root(path[0]) && path[1] == COLON {
        start = 2;
    }
    if let Some(suffix) = suffix
        && !suffix.is_empty()
        && suffix.len() <= path.len()
    {
        if suffix == path {
            return Vec::new();
        }
        let mut ext_idx = suffix.len() as isize - 1;
        let mut first_non_slash_end: isize = -1;
        let mut i = path.len() as isize - 1;
        while i >= start as isize {
            let code = path[i as usize];
            if sep(code) {
                if !matched_slash {
                    start = i as usize + 1;
                    break;
                }
            } else {
                if first_non_slash_end == -1 {
                    matched_slash = false;
                    first_non_slash_end = i + 1;
                }
                if ext_idx >= 0 {
                    if code == suffix[ext_idx as usize] {
                        ext_idx -= 1;
                        if ext_idx == -1 {
                            end = i;
                        }
                    } else {
                        ext_idx = -1;
                        end = first_non_slash_end;
                    }
                }
            }
            i -= 1;
        }
        if start as isize == end {
            end = first_non_slash_end;
        } else if end == -1 {
            end = path.len() as isize;
        }
        return slice(path, start as isize, end);
    }
    let mut i = path.len() as isize - 1;
    while i >= start as isize {
        if sep(path[i as usize]) {
            if !matched_slash {
                start = i as usize + 1;
                break;
            }
        } else if end == -1 {
            matched_slash = false;
            end = i + 1;
        }
        i -= 1;
    }
    if end == -1 {
        return Vec::new();
    }
    slice(path, start as isize, end)
}

// `String.prototype.slice` with non-negative bounds.
fn slice(s: &[u16], start: isize, end: isize) -> Units {
    let len = s.len() as isize;
    let start = start.clamp(0, len) as usize;
    let end = end.clamp(0, len) as usize;
    if start >= end { Vec::new() } else { s[start..end].to_vec() }
}

fn extname(path: &[u16], win32: bool) -> Units {
    let sep: fn(u16) -> bool = if win32 { is_sep } else { is_posix_sep };
    let mut start = 0usize;
    let mut start_dot: isize = -1;
    let mut start_part: isize = 0;
    let mut end: isize = -1;
    let mut matched_slash = true;
    let mut pre_dot_state = 0;
    if win32 && path.len() >= 2 && path[1] == COLON && is_device_root(path[0]) {
        start = 2;
        start_part = 2;
    }
    let mut i = path.len() as isize - 1;
    while i >= start as isize {
        let code = path[i as usize];
        if sep(code) {
            if !matched_slash {
                start_part = i + 1;
                break;
            }
            i -= 1;
            continue;
        }
        if end == -1 {
            matched_slash = false;
            end = i + 1;
        }
        if code == DOT {
            if start_dot == -1 {
                start_dot = i;
            } else if pre_dot_state != 1 {
                pre_dot_state = 1;
            }
        } else if start_dot != -1 {
            pre_dot_state = -1;
        }
        i -= 1;
    }
    if start_dot == -1
        || end == -1
        || pre_dot_state == 0
        || (pre_dot_state == 1 && start_dot == end - 1 && start_dot == start_part + 1)
    {
        return Vec::new();
    }
    slice(path, start_dot, end)
}

// `[root, dir, base, ext, name]`.
fn posix_parse(path: &[u16]) -> [Units; 5] {
    let mut ret: [Units; 5] = Default::default();
    let [root, dir, base, ext, name] = &mut ret;
    if path.is_empty() {
        return ret;
    }
    let absolute = path[0] == SLASH;
    let start: isize = if absolute {
        *root = vec![SLASH];
        1
    } else {
        0
    };
    let mut start_dot: isize = -1;
    let mut start_part: isize = 0;
    let mut end: isize = -1;
    let mut matched_slash = true;
    let mut pre_dot_state = 0;
    let mut i = path.len() as isize - 1;
    while i >= start {
        let code = path[i as usize];
        if code == SLASH {
            if !matched_slash {
                start_part = i + 1;
                break;
            }
            i -= 1;
            continue;
        }
        if end == -1 {
            matched_slash = false;
            end = i + 1;
        }
        if code == DOT {
            if start_dot == -1 {
                start_dot = i;
            } else if pre_dot_state != 1 {
                pre_dot_state = 1;
            }
        } else if start_dot != -1 {
            pre_dot_state = -1;
        }
        i -= 1;
    }
    if end != -1 {
        let start = if start_part == 0 && absolute { 1 } else { start_part };
        if start_dot == -1 || pre_dot_state == 0 || (pre_dot_state == 1 && start_dot == end - 1 && start_dot == start_part + 1) {
            *base = slice(path, start, end);
            *name = base.clone();
        } else {
            *name = slice(path, start, start_dot);
            *base = slice(path, start, end);
            *ext = slice(path, start_dot, end);
        }
    }
    if start_part > 0 {
        *dir = slice(path, 0, start_part - 1);
    } else if absolute {
        *dir = vec![SLASH];
    }
    ret
}

// A Win32 path's root: its device (`C:` or `\\server\share`), where the root ends, and whether it is absolute
// (`win32.resolve`'s parse; `normalize` handles a bare UNC root itself).
fn win32_root(path: &[u16], _: bool) -> (Units, usize, bool) {
    let len = path.len();
    let mut root_end = 0;
    let mut device: Units = Vec::new();
    let mut absolute = false;
    let code = at(path, 0);
    if len == 1 {
        if is_sep(code) {
            root_end = 1;
            absolute = true;
        }
    } else if is_sep(code) {
        absolute = true;
        if is_sep(at(path, 1)) {
            let mut j = 2;
            let mut last = j;
            while j < len && !is_sep(path[j]) {
                j += 1;
            }
            if j < len && j != last {
                let first = path[last..j].to_vec();
                last = j;
                while j < len && is_sep(path[j]) {
                    j += 1;
                }
                if j < len && j != last {
                    last = j;
                    while j < len && !is_sep(path[j]) {
                        j += 1;
                    }
                    if j == len || j != last {
                        device = units("\\\\");
                        device.extend(first);
                        device.push(BACKSLASH);
                        device.extend_from_slice(&path[last..j]);
                        root_end = j;
                    }
                }
            }
        } else {
            root_end = 1;
        }
    } else if is_device_root(code) && at(path, 1) == COLON {
        device = path[..2].to_vec();
        root_end = 2;
        if len > 2 && is_sep(path[2]) {
            absolute = true;
            root_end = 3;
        }
    }
    (device, root_end, absolute)
}

fn win32_normalize(path: &[u16]) -> Units {
    let len = path.len();
    if len == 0 {
        return vec![DOT];
    }
    let mut root_end = 0;
    let mut device: Option<Units> = None;
    let mut absolute = false;
    let code = path[0];
    if len == 1 {
        return if is_posix_sep(code) { vec![BACKSLASH] } else { path.to_vec() };
    }
    if is_sep(code) {
        absolute = true;
        if is_sep(path[1]) {
            let mut j = 2;
            let mut last = j;
            while j < len && !is_sep(path[j]) {
                j += 1;
            }
            if j < len && j != last {
                let first = path[last..j].to_vec();
                last = j;
                while j < len && is_sep(path[j]) {
                    j += 1;
                }
                if j < len && j != last {
                    last = j;
                    while j < len && !is_sep(path[j]) {
                        j += 1;
                    }
                    if j == len {
                        let mut out = units("\\\\");
                        out.extend(first);
                        out.push(BACKSLASH);
                        out.extend_from_slice(&path[last..]);
                        out.push(BACKSLASH);
                        return out;
                    }
                    if j != last {
                        let mut d = units("\\\\");
                        d.extend(first);
                        d.push(BACKSLASH);
                        d.extend_from_slice(&path[last..j]);
                        device = Some(d);
                        root_end = j;
                    }
                }
            }
        } else {
            root_end = 1;
        }
    } else if is_device_root(code) && path[1] == COLON {
        device = Some(path[..2].to_vec());
        root_end = 2;
        if len > 2 && is_sep(path[2]) {
            absolute = true;
            root_end = 3;
        }
    }
    let mut tail = if root_end < len {
        normalize_string(&path[root_end..], !absolute, BACKSLASH, is_sep)
    } else {
        Vec::new()
    };
    if tail.is_empty() && !absolute {
        tail = vec![DOT];
    }
    if !tail.is_empty() && is_sep(path[len - 1]) {
        tail.push(BACKSLASH);
    }
    match device {
        None => {
            if absolute {
                tail.insert(0, BACKSLASH);
            }
            tail
        }
        Some(mut device) => {
            if absolute {
                device.push(BACKSLASH);
            }
            device.extend(tail);
            device
        }
    }
}

fn win32_is_absolute(path: &[u16]) -> bool {
    let len = path.len();
    if len == 0 {
        return false;
    }
    let code = path[0];
    is_sep(code) || (len > 2 && is_device_root(code) && path[1] == COLON && is_sep(path[2]))
}

fn win32_join(paths: &[Units]) -> Units {
    let mut joined: Option<Units> = None;
    let mut first_part: Units = Vec::new();
    for arg in paths {
        if arg.is_empty() {
            continue;
        }
        match &mut joined {
            None => {
                joined = Some(arg.clone());
                first_part = arg.clone();
            }
            Some(j) => {
                j.push(BACKSLASH);
                j.extend_from_slice(arg);
            }
        }
    }
    let Some(mut joined) = joined else { return vec![DOT] };
    let mut needs_replace = true;
    let mut slash_count = 0;
    if is_sep(at(&first_part, 0)) {
        slash_count += 1;
        let first_len = first_part.len();
        if first_len > 1 && is_sep(first_part[1]) {
            slash_count += 1;
            if first_len > 2 {
                if is_sep(first_part[2]) {
                    slash_count += 1;
                } else {
                    needs_replace = false;
                }
            }
        }
    }
    if needs_replace {
        while slash_count < joined.len() && is_sep(joined[slash_count]) {
            slash_count += 1;
        }
        if slash_count >= 2 {
            let mut out = vec![BACKSLASH];
            out.extend_from_slice(&joined[slash_count..]);
            joined = out;
        }
    }
    win32_normalize(&joined)
}

fn win32_dirname(path: &[u16]) -> Units {
    let len = path.len();
    if len == 0 {
        return vec![DOT];
    }
    let mut root_end: isize = -1;
    let mut offset = 0;
    let code = path[0];
    if len == 1 {
        return if is_sep(code) { path.to_vec() } else { vec![DOT] };
    }
    if is_sep(code) {
        root_end = 1;
        offset = 1;
        if is_sep(path[1]) {
            let mut j = 2;
            let mut last = j;
            while j < len && !is_sep(path[j]) {
                j += 1;
            }
            if j < len && j != last {
                last = j;
                while j < len && is_sep(path[j]) {
                    j += 1;
                }
                if j < len && j != last {
                    last = j;
                    while j < len && !is_sep(path[j]) {
                        j += 1;
                    }
                    if j == len {
                        return path.to_vec();
                    }
                    if j != last {
                        root_end = j as isize + 1;
                        offset = j + 1;
                    }
                }
            }
        }
    } else if is_device_root(code) && path[1] == COLON {
        root_end = if len > 2 && is_sep(path[2]) { 3 } else { 2 };
        offset = root_end as usize;
    }
    let mut end: isize = -1;
    let mut matched_slash = true;
    let mut i = len as isize - 1;
    while i >= offset as isize {
        if is_sep(path[i as usize]) {
            if !matched_slash {
                end = i;
                break;
            }
        } else {
            matched_slash = false;
        }
        i -= 1;
    }
    if end == -1 {
        if root_end == -1 {
            return vec![DOT];
        }
        end = root_end;
    }
    slice(path, 0, end)
}

fn win32_parse(path: &[u16]) -> [Units; 5] {
    let mut ret: [Units; 5] = Default::default();
    if path.is_empty() {
        return ret;
    }
    let len = path.len();
    let mut root_end = 0usize;
    let code = path[0];
    {
        let [root, dir, base, _, name] = &mut ret;
        if len == 1 {
            if is_sep(code) {
                *root = path.to_vec();
                *dir = path.to_vec();
            } else {
                *base = path.to_vec();
                *name = path.to_vec();
            }
            return ret;
        }
    }
    if is_sep(code) {
        root_end = 1;
        if is_sep(path[1]) {
            let mut j = 2;
            let mut last = j;
            while j < len && !is_sep(path[j]) {
                j += 1;
            }
            if j < len && j != last {
                last = j;
                while j < len && is_sep(path[j]) {
                    j += 1;
                }
                if j < len && j != last {
                    last = j;
                    while j < len && !is_sep(path[j]) {
                        j += 1;
                    }
                    if j == len {
                        root_end = j;
                    } else if j != last {
                        root_end = j + 1;
                    }
                }
            }
        }
    } else if is_device_root(code) && path[1] == COLON {
        if len <= 2 {
            ret[0] = path.to_vec();
            ret[1] = path.to_vec();
            return ret;
        }
        root_end = 2;
        if is_sep(path[2]) {
            if len == 3 {
                ret[0] = path.to_vec();
                ret[1] = path.to_vec();
                return ret;
            }
            root_end = 3;
        }
    }
    let [root, dir, base, ext, name] = &mut ret;
    if root_end > 0 {
        *root = path[..root_end].to_vec();
    }
    let mut start_dot: isize = -1;
    let mut start_part = root_end as isize;
    let mut end: isize = -1;
    let mut matched_slash = true;
    let mut pre_dot_state = 0;
    let mut i = len as isize - 1;
    while i >= root_end as isize {
        let code = path[i as usize];
        if is_sep(code) {
            if !matched_slash {
                start_part = i + 1;
                break;
            }
            i -= 1;
            continue;
        }
        if end == -1 {
            matched_slash = false;
            end = i + 1;
        }
        if code == DOT {
            if start_dot == -1 {
                start_dot = i;
            } else if pre_dot_state != 1 {
                pre_dot_state = 1;
            }
        } else if start_dot != -1 {
            pre_dot_state = -1;
        }
        i -= 1;
    }
    if end != -1 {
        if start_dot == -1 || pre_dot_state == 0 || (pre_dot_state == 1 && start_dot == end - 1 && start_dot == start_part + 1) {
            *base = slice(path, start_part, end);
            *name = base.clone();
        } else {
            *name = slice(path, start_part, start_dot);
            *base = slice(path, start_part, end);
            *ext = slice(path, start_dot, end);
        }
    }
    if start_part > 0 && start_part != root_end as isize {
        *dir = slice(path, 0, start_part - 1);
    } else {
        *dir = root.clone();
    }
    ret
}

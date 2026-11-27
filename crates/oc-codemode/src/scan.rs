//! `CodeMode.scanTools` and `CodeMode.scanProgram` (`packages/codemode/src/scan.ts`): the `tools.<path>` a program
//! calls, the globals it references and the modules it loads, found statically.
//!
//! The walk runs over the parser's ESTree JSON (the same node shapes and key order acorn produces), so children
//! are visited in acorn's `Object.entries` order like upstream. The program is parsed like `execute` parses it, so a
//! top-level export is the same `ParseError`. Classes are walked whole (`to_estree_full`, 2.4.1 runs them); the
//! other node kinds outside the interpreter's subset (`new.target`, `import.meta`) carry no children in that JSON.

use std::collections::HashSet;

use serde_json::Value;

use crate::interp::errors::{Diagnostic, DiagnosticKind, Location};

/// Interpreter threads parse on a large stack: the parser is recursive.
const STACK_SIZE: usize = 1 << 30;

/// Extracts every `tools.<path>` referenced by a Code Mode program, in first-seen order. Fails on syntax errors,
/// dynamic bracket access (`tools[name]`), aliasing/passing `tools` rather than calling `tools.<path>(...)`, or
/// loading `tools` as a module (`import`/`require`, or one whose name is computed).
pub fn scan_tools(code: &str) -> Result<Vec<String>, Diagnostic> {
    scan_program(code, &[], &[]).map(|scanned| scanned.tools)
}

/// What [`scan_program`] finds (`ProgramScan`).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ProgramScan {
    pub tools: Vec<String>,
    /// The unshadowed references to each requested global, by global, in first-seen order.
    pub globals: Vec<(String, Vec<GlobalUse>)>,
    /// The literal modules the program loads (`import`, `require`), by module name.
    pub modules: Vec<String>,
}

impl ProgramScan {
    pub fn global(&self, name: &str) -> Option<&[GlobalUse]> {
        self.globals.iter().find(|(n, _)| n == name).map(|(_, uses)| uses.as_slice())
    }

    pub fn has_module(&self, name: &str) -> bool {
        self.modules.iter().any(|m| m == name)
    }
}

/// One reference to a global (`GlobalUse`).
#[derive(Debug, Clone, PartialEq)]
pub struct GlobalUse {
    /// The member read on the global (`Bun.file` → `file`), when static.
    pub member: Option<String>,
    /// The first non-forwarding member used on a call's result, for each place it is used; `None` means the value
    /// escaped or was used dynamically.
    pub result_members: Vec<Option<String>>,
}

/// A use's result members: fixed, or those of the variable the result was bound to, which later references add to.
enum Members {
    Fixed(Vec<Option<String>>),
    Binding(usize, String),
}

/// `scanProgram(code, globals, forwardingMembers)` (2.4.1, #65236): static tool calls, references to `globals`, and
/// literal module loads. A member in `forwarding` (`slice`) passes its call's result on, so the member read after it
/// counts instead.
pub fn scan_program(code: &str, globals: &[&str], forwarding: &[&str]) -> Result<ProgramScan, Diagnostic> {
    if code.trim_matches(|c: char| (c as u32) < 0x10000 && crate::number::is_js_whitespace(c as u16)).is_empty() {
        return Err(Diagnostic::new(DiagnosticKind::ParseError, "Code cannot be empty."));
    }
    let code = code.to_string();
    let worker = std::thread::Builder::new().name("codemode-scan".into()).stack_size(STACK_SIZE).spawn(move || {
        let program = crate::codemode::parse_program(&code)?;
        Ok::<Value, Diagnostic>(crate::parser::to_estree_full(&program))
    });
    let tree = match worker.map(|w| w.join()) {
        Ok(Ok(result)) => result?,
        _ => return Err(Diagnostic::new(DiagnosticKind::ExecutionFailure, "The interpreter failed unexpectedly.")),
    };
    let mut scanner = Scanner {
        globals,
        forwarding,
        tools: Vec::new(),
        seen: HashSet::new(),
        referenced: Vec::new(),
        local_uses: std::collections::HashMap::new(),
        modules: Vec::new(),
        scopes: vec![(0, collect_scope(&tree, true))],
        next_scope: 1,
        ancestors: Vec::new(),
    };
    if let Some(diagnostic) = scanner.visit(&tree) {
        return Err(diagnostic);
    }
    let local_uses = std::mem::take(&mut scanner.local_uses);
    let globals = scanner
        .referenced
        .into_iter()
        .map(|(name, uses)| {
            let uses = uses
                .into_iter()
                .map(|(member, members)| GlobalUse {
                    member,
                    result_members: match members {
                        Members::Fixed(members) => members,
                        Members::Binding(scope, name) => local_uses.get(&(scope, name)).cloned().unwrap_or_default(),
                    },
                })
                .collect();
            (name, uses)
        })
        .collect();
    Ok(ProgramScan { tools: scanner.tools, globals, modules: scanner.modules })
}

struct Scanner<'a, 'g> {
    globals: &'g [&'g str],
    forwarding: &'g [&'g str],
    tools: Vec<String>,
    seen: HashSet<String>,
    referenced: Vec<(String, Vec<(Option<String>, Members)>)>,
    /// The result members each local binding is used with, by `(scope, name)`: a binding may be referenced in a
    /// closure before its initializer appears in the walk.
    local_uses: std::collections::HashMap<(usize, String), Vec<Option<String>>>,
    modules: Vec<String>,
    scopes: Vec<(usize, HashSet<String>)>,
    next_scope: usize,
    ancestors: Vec<&'a Value>,
}

fn kind(node: &Value) -> &str {
    node.get("type").and_then(Value::as_str).unwrap_or_default()
}

fn field<'a>(node: &'a Value, key: &str) -> Option<&'a Value> {
    node.get(key).filter(|v| !v.is_null())
}

fn same(a: Option<&Value>, b: &Value) -> bool {
    a.is_some_and(|a| std::ptr::eq(a, b))
}

fn is_node(value: &Value) -> bool {
    value.get("type").is_some_and(Value::is_string)
}

fn fail(node: &Value, message: &str) -> Diagnostic {
    let loc =
        node.get("loc").and_then(|loc| Some(Location { line: loc.get("line")?.as_u64()? as u32, column: loc.get("column")?.as_u64()? as u32 + 1 }));
    Diagnostic {
        kind: DiagnosticKind::UnsupportedSyntax,
        message: match &loc {
            Some(l) => format!("{message} (line {}, col {})", l.line, l.column),
            None => message.to_string(),
        },
        location: loc,
        suggestions: None,
    }
}

/// `memberName(node)`: a member expression's static name.
fn member_name(node: &Value) -> Option<String> {
    let computed = node.get("computed").and_then(Value::as_bool).unwrap_or(false);
    let property = field(node, "property")?;
    if !computed && kind(property) == "Identifier" {
        return property.get("name").and_then(Value::as_str).map(String::from);
    }
    if computed && kind(property) == "Literal" {
        return property.get("value").and_then(Value::as_str).map(String::from);
    }
    None
}

/// `valueUse(value, ancestors, index)`: skips wrappers that leave the wrapped value available for another access.
fn value_use<'a>(mut value: &'a Value, ancestors: &[&'a Value], mut index: isize) -> (&'a Value, Option<&'a Value>, isize) {
    while index >= 0 {
        let parent = ancestors[index as usize];
        if (kind(parent) == "AwaitExpression" && same(field(parent, "argument"), value))
            || (kind(parent) == "ChainExpression" && same(field(parent, "expression"), value))
        {
            value = parent;
            index -= 1;
            continue;
        }
        return (value, Some(parent), index);
    }
    (value, None, index)
}

/// `resultMember(value, ancestors, index, forwardingMembers)`
fn result_member<'a>(value: &'a Value, ancestors: &[&'a Value], index: isize, forwarding: &[&str]) -> Option<String> {
    let (value, parent, index) = value_use(value, ancestors, index);
    let parent = parent.filter(|p| kind(p) == "MemberExpression" && same(field(p, "object"), value))?;
    let name = member_name(parent)?;
    if !forwarding.contains(&name.as_str()) {
        return Some(name);
    }
    let call = (index >= 1).then(|| ancestors[(index - 1) as usize])?;
    if kind(call) == "CallExpression" && same(field(call, "callee"), parent) {
        return result_member(call, ancestors, index - 2, forwarding);
    }
    None
}

impl<'a> Scanner<'a, '_> {
    /// The innermost scope declaring `name` (`scopes.findLast`).
    fn scope_of(&self, name: &str) -> Option<usize> {
        self.scopes.iter().rev().find(|(_, scope)| scope.contains(name)).map(|(id, _)| *id)
    }

    fn visit(&mut self, node: &'a Value) -> Option<Diagnostic> {
        let name = (kind(node) == "Identifier").then(|| node.get("name").and_then(Value::as_str)).flatten();
        if let Some(name) = name
            && (!self.globals.is_empty() || name == "tools")
            && is_reference_position(node, self.ancestors.last().copied())
        {
            let scope = self.scope_of(name);
            if let Some(scope) = scope
                && !self.globals.is_empty()
            {
                let member = result_member(node, &self.ancestors, self.ancestors.len() as isize - 1, self.forwarding);
                self.local_uses.entry((scope, name.to_string())).or_default().push(member);
            }
            if scope.is_none() && name == "tools" {
                return match read_static_tool_call(node, &self.ancestors) {
                    Err((at, reason)) => Some(fail(at, reason)),
                    Ok(path) => {
                        if self.seen.insert(path.clone()) {
                            self.tools.push(path);
                        }
                        None
                    }
                };
            }
            if scope.is_none() && self.globals.contains(&name) {
                let use_ = self.global_use(node);
                match self.referenced.iter_mut().find(|(n, _)| n == name) {
                    Some((_, uses)) => uses.push(use_),
                    None => self.referenced.push((name.to_string(), vec![use_])),
                }
            }
        }
        // `import`/`require` of "tools" would reach tools the scan never sees.
        if let Some(loaded) = loaded_module(node) {
            let name = loaded.map(crate::interp::modules::module_name);
            match name {
                None => return Some(fail(node, LOADED)),
                Some(name) if name == "tools" => return Some(fail(node, LOADED)),
                Some(name) => {
                    if !self.modules.iter().any(|m| *m == name) {
                        self.modules.push(name.to_string());
                    }
                }
            }
        }
        let pushed = enter_node_scope(node);
        let had = pushed.is_some();
        if let Some(scope) = pushed {
            self.scopes.push((self.next_scope, scope));
            self.next_scope += 1;
        }
        self.ancestors.push(node);
        let error = self.visit_children(node);
        self.ancestors.pop();
        if had {
            self.scopes.pop();
        }
        error
    }

    /// One unshadowed reference to a requested global: its member, and what its call's result is used for.
    fn global_use(&self, node: &'a Value) -> (Option<String>, Members) {
        let len = self.ancestors.len() as isize;
        let property = self.ancestors.last().copied();
        let member = property.filter(|p| kind(p) == "MemberExpression" && same(field(p, "object"), node)).and_then(member_name);
        let call = (len >= 2).then(|| self.ancestors[(len - 2) as usize]);
        let is_call = property.is_some_and(|p| call.is_some_and(|c| kind(c) == "CallExpression" && same(field(c, "callee"), p)));
        if member.is_none() || !is_call {
            return (member, Members::Fixed(vec![None]));
        }
        let call = call.unwrap();
        let (value, parent, index) = value_use(call, &self.ancestors, len - 3);
        if parent.is_some_and(|p| kind(p) == "ExpressionStatement") {
            return (member, Members::Fixed(Vec::new()));
        }
        if let Some(parent) = parent
            && kind(parent) == "VariableDeclarator"
            && same(field(parent, "init"), value)
            && let Some(id) = field(parent, "id").filter(|id| kind(id) == "Identifier")
            && let Some(name) = id.get("name").and_then(Value::as_str)
            && let Some(binding) = self.scope_of(name)
        {
            return (member, Members::Binding(binding, name.to_string()));
        }
        (member, Members::Fixed(vec![result_member(value, &self.ancestors, index, self.forwarding)]))
    }

    fn visit_children(&mut self, node: &'a Value) -> Option<Diagnostic> {
        let Value::Object(map) = node else { return None };
        for (key, value) in map {
            if matches!(key.as_str(), "type" | "start" | "end" | "loc" | "range") {
                continue;
            }
            match value {
                Value::Array(items) => {
                    for item in items {
                        if is_node(item)
                            && let Some(error) = self.visit(item)
                        {
                            return Some(error);
                        }
                    }
                }
                other if is_node(other) => {
                    if let Some(error) = self.visit(other) {
                        return Some(error);
                    }
                }
                _ => {}
            }
        }
        None
    }
}

fn is_reference_position(node: &Value, parent: Option<&Value>) -> bool {
    let Some(parent) = parent else { return true };
    let computed = parent.get("computed").and_then(Value::as_bool).unwrap_or(false);
    match kind(parent) {
        "ImportSpecifier" | "ImportDefaultSpecifier" | "ImportNamespaceSpecifier" => return false,
        "MemberExpression" if same(field(parent, "property"), node) && !computed => return false,
        "Property" if same(field(parent, "key"), node) && !computed && !parent.get("shorthand").and_then(Value::as_bool).unwrap_or(false) => {
            return false;
        }
        "MethodDefinition" if same(field(parent, "key"), node) && !computed => return false,
        "LabeledStatement" | "BreakStatement" | "ContinueStatement" if same(field(parent, "label"), node) => return false,
        "FunctionDeclaration" | "FunctionExpression" | "ClassDeclaration" | "ClassExpression" if same(field(parent, "id"), node) => {
            return false;
        }
        "VariableDeclarator" if same(field(parent, "id"), node) => return false,
        _ => {}
    }
    true
}

const DYNAMIC: &str = "Automation code must reference tools statically (e.g. tools.slack.read_thread), not with dynamic bracket expressions.";
const ALIAS: &str = "Automation code must call tools directly as tools.<name>(...), not alias or pass the tools object.";
const LOADED: &str = "Automation code must call tools directly as tools.<name>(...), not import or require them.";

/// `loadedModule(node)`: the module an `import` or `require(...)` loads, `Some(None)` when it is computed, or `None`
/// for other nodes.
fn loaded_module(node: &Value) -> Option<Option<&str>> {
    let specifier = match kind(node) {
        "ImportDeclaration" | "ImportExpression" => field(node, "source")?,
        "CallExpression" => {
            let callee = field(node, "callee")?;
            if kind(callee) != "Identifier" || callee.get("name").and_then(Value::as_str) != Some("require") {
                return None;
            }
            node.get("arguments").and_then(Value::as_array).and_then(|a| a.first())?
        }
        _ => return None,
    };
    Some(if kind(specifier) == "Literal" { specifier.get("value").and_then(Value::as_str) } else { None })
}

fn read_static_tool_call<'a>(root: &'a Value, ancestors: &[&'a Value]) -> Result<String, (&'a Value, &'static str)> {
    let mut segments: Vec<String> = Vec::new();
    let mut current = root;
    let mut index = ancestors.len() as isize - 1;
    while index >= 0 {
        let parent = ancestors[index as usize];
        if kind(parent) == "ChainExpression" && same(field(parent, "expression"), current) {
            current = parent;
            index -= 1;
            continue;
        }
        if kind(parent) != "MemberExpression" || !same(field(parent, "object"), current) {
            break;
        }
        let Some(member) = member_name(parent) else {
            return Err((field(parent, "property").unwrap_or(parent), DYNAMIC));
        };
        segments.push(member);
        current = parent;
        index -= 1;
    }
    let call = if index >= 0 { Some(ancestors[index as usize]) } else { None };
    if segments.is_empty() || !call.is_some_and(|c| kind(c) == "CallExpression" && same(field(c, "callee"), current)) {
        return Err((current, ALIAS));
    }
    Ok(segments.join("."))
}

fn enter_node_scope(node: &Value) -> Option<HashSet<String>> {
    match kind(node) {
        "FunctionDeclaration" | "FunctionExpression" | "ArrowFunctionExpression" => {
            let mut names = HashSet::new();
            if let Some(id) = field(node, "id")
                && let Some(name) = id.get("name").and_then(Value::as_str)
            {
                names.insert(name.to_string());
            }
            for param in node.get("params").and_then(Value::as_array).into_iter().flatten() {
                collect_pattern(param, &mut names);
            }
            if let Some(body) = field(node, "body")
                && kind(body) == "BlockStatement"
            {
                names.extend(collect_scope(body, true));
            }
            Some(names)
        }
        "BlockStatement" => Some(collect_scope(node, false)),
        "CatchClause" => {
            let mut names = HashSet::new();
            if let Some(param) = field(node, "param") {
                collect_pattern(param, &mut names);
            }
            Some(names)
        }
        "ForStatement" => {
            let init = field(node, "init")?;
            if kind(init) != "VariableDeclaration" || init.get("kind").and_then(Value::as_str) == Some("var") {
                return None;
            }
            Some(declared(init))
        }
        "ForInStatement" | "ForOfStatement" => {
            let left = field(node, "left")?;
            if kind(left) != "VariableDeclaration" || left.get("kind").and_then(Value::as_str) == Some("var") {
                return None;
            }
            Some(declared(left))
        }
        _ => None,
    }
}

fn declared(declaration: &Value) -> HashSet<String> {
    let mut names = HashSet::new();
    for decl in declaration.get("declarations").and_then(Value::as_array).into_iter().flatten() {
        if let Some(id) = field(decl, "id") {
            collect_pattern(id, &mut names);
        }
    }
    names
}

fn collect_scope(container: &Value, include_var: bool) -> HashSet<String> {
    let mut names = HashSet::new();
    let body = container.get("body").and_then(Value::as_array).cloned().unwrap_or_default();
    for statement in &body {
        match kind(statement) {
            "ImportDeclaration" => {
                for specifier in statement.get("specifiers").and_then(Value::as_array).into_iter().flatten() {
                    if let Some(name) = field(specifier, "local").and_then(|l| l.get("name")).and_then(Value::as_str) {
                        names.insert(name.to_string());
                    }
                }
            }
            "FunctionDeclaration" | "ClassDeclaration" => {
                if let Some(name) = field(statement, "id").and_then(|id| id.get("name")).and_then(Value::as_str) {
                    names.insert(name.to_string());
                }
            }
            "VariableDeclaration" if include_var || statement.get("kind").and_then(Value::as_str) != Some("var") => {
                names.extend(declared(statement));
            }
            _ => {}
        }
    }
    if include_var {
        for statement in &body {
            collect_nested_vars(statement, &mut names);
        }
    }
    names
}

fn collect_nested_vars(node: &Value, names: &mut HashSet<String>) {
    if !is_node(node) {
        return;
    }
    if matches!(kind(node), "FunctionDeclaration" | "FunctionExpression" | "ArrowFunctionExpression") {
        return;
    }
    if kind(node) == "VariableDeclaration" && node.get("kind").and_then(Value::as_str) == Some("var") {
        names.extend(declared(node));
    }
    let Value::Object(map) = node else { return };
    for (key, value) in map {
        if matches!(key.as_str(), "type" | "start" | "end" | "loc" | "range") {
            continue;
        }
        match value {
            Value::Array(items) => items.iter().for_each(|item| collect_nested_vars(item, names)),
            other => collect_nested_vars(other, names),
        }
    }
}

fn collect_pattern(pattern: &Value, names: &mut HashSet<String>) {
    match kind(pattern) {
        "Identifier" => {
            if let Some(name) = pattern.get("name").and_then(Value::as_str) {
                names.insert(name.to_string());
            }
        }
        "AssignmentPattern" => {
            if let Some(left) = field(pattern, "left") {
                collect_pattern(left, names);
            }
        }
        "RestElement" => {
            if let Some(argument) = field(pattern, "argument") {
                collect_pattern(argument, names);
            }
        }
        "ArrayPattern" => {
            for element in pattern.get("elements").and_then(Value::as_array).into_iter().flatten() {
                if !element.is_null() {
                    collect_pattern(element, names);
                }
            }
        }
        "ObjectPattern" => {
            for prop in pattern.get("properties").and_then(Value::as_array).into_iter().flatten() {
                match kind(prop) {
                    "RestElement" => {
                        if let Some(argument) = field(prop, "argument") {
                            collect_pattern(argument, names);
                        }
                    }
                    "Property" => {
                        if let Some(value) = field(prop, "value") {
                            collect_pattern(value, names);
                        }
                    }
                    _ => {}
                }
            }
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_static_calls_in_first_seen_order() {
        let scanned = scan_tools(
            r#"
      const chapters = await tools["hex-meetings"].get_chapters({ meeting_id: "m1" })
      const thread = await tools.slack.read_thread({ ts: await tools.slack.latest_ts({}) })
      await tools.slack.read_thread({ ts: "again" })
      const ignored = { tools: 1 }.tools
      return { wake: true, state: chapters }
    "#,
        );
        assert_eq!(scanned.unwrap(), vec!["hex-meetings.get_chapters", "slack.read_thread", "slack.latest_ts"]);
    }

    #[test]
    fn ignores_shadowed_tools() {
        let scanned = scan_tools(
            r#"
      function helper(tools) {
        return tools[dynamicKey]()
      }
      {
        const tools = { any: () => 1 }
        tools.any()
      }
      return helper({ [dynamicKey]: () => ({ wake: false }) })
    "#,
        );
        assert_eq!(scanned.unwrap(), Vec::<String>::new());
    }

    #[test]
    fn rejects_empty_syntax_dynamic_and_alias() {
        assert_eq!(scan_tools("   ").unwrap_err().message, "Code cannot be empty.");
        assert_eq!(scan_tools("const x = {").unwrap_err().kind, DiagnosticKind::ParseError);
        assert_eq!(scan_tools("return await tools.slack[name]({})").unwrap_err().message, format!("{DYNAMIC} (line 1, col 26)"));
        assert_eq!(scan_tools("const s = tools.slack; return await s.read_thread({})").unwrap_err().message, format!("{ALIAS} (line 1, col 11)"));
        assert_eq!(scan_tools("return Object.keys(tools)").unwrap_err().message, format!("{ALIAS} (line 1, col 20)"));
        assert_eq!(scan_tools("return tools.a?.b()").unwrap(), vec!["a.b"]);
    }
}

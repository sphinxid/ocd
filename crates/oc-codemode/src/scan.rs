//! `CodeMode.scanTools` (`packages/codemode/src/scan.ts`): the `tools.<path>` a program calls, found statically.
//!
//! The walk runs over the parser's ESTree JSON (the same node shapes and key order acorn produces), so children
//! are visited in acorn's `Object.entries` order like upstream. The program is parsed like `execute` parses it, so a
//! top-level export is the same `ParseError`. Node kinds outside the interpreter's subset (classes, `new.target`,
//! `import.meta`) carry no children in that JSON, so a `tools` reference inside a class body is not seen;
//! such a program fails when it runs anyway (`Syntax 'ClassDeclaration' is not supported`).

use std::collections::HashSet;

use serde_json::Value;

use crate::interp::errors::{Diagnostic, DiagnosticKind, Location};

/// Interpreter threads parse on a large stack: the parser is recursive.
const STACK_SIZE: usize = 1 << 30;

/// Extracts every `tools.<path>` referenced by a Code Mode program, in first-seen order. Fails on syntax errors,
/// dynamic bracket access (`tools[name]`), or aliasing/passing `tools` rather than calling `tools.<path>(...)`.
pub fn scan_tools(code: &str) -> Result<Vec<String>, Diagnostic> {
    if code.trim_matches(|c: char| (c as u32) < 0x10000 && crate::number::is_js_whitespace(c as u16)).is_empty() {
        return Err(Diagnostic::new(DiagnosticKind::ParseError, "Code cannot be empty."));
    }
    let code = code.to_string();
    let worker = std::thread::Builder::new().name("codemode-scan".into()).stack_size(STACK_SIZE).spawn(move || {
        let program = crate::codemode::parse_program(&code)?;
        Ok::<Value, Diagnostic>(crate::parser::to_estree(&program))
    });
    let tree = match worker.map(|w| w.join()) {
        Ok(Ok(result)) => result?,
        _ => return Err(Diagnostic::new(DiagnosticKind::ExecutionFailure, "The interpreter failed unexpectedly.")),
    };
    let mut scanner = Scanner { found: Vec::new(), seen: HashSet::new(), scopes: vec![collect_scope(&tree, true)], ancestors: Vec::new() };
    match scanner.visit(&tree) {
        Some(diagnostic) => Err(diagnostic),
        None => Ok(scanner.found),
    }
}

struct Scanner<'a> {
    found: Vec<String>,
    seen: HashSet<String>,
    scopes: Vec<HashSet<String>>,
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

impl<'a> Scanner<'a> {
    fn shadowed(&self) -> bool {
        self.scopes.iter().any(|scope| scope.contains("tools"))
    }

    fn visit(&mut self, node: &'a Value) -> Option<Diagnostic> {
        if kind(node) == "Identifier" && node.get("name").and_then(Value::as_str) == Some("tools") && !self.shadowed() {
            if !is_reference_position(node, self.ancestors.last().copied()) {
                return None;
            }
            return match read_static_tool_call(node, &self.ancestors) {
                Err((at, reason)) => Some(fail(at, reason)),
                Ok(path) => {
                    if self.seen.insert(path.clone()) {
                        self.found.push(path);
                    }
                    None
                }
            };
        }
        let pushed = enter_node_scope(node);
        let had = pushed.is_some();
        if let Some(scope) = pushed {
            self.scopes.push(scope);
        }
        self.ancestors.push(node);
        let error = self.visit_children(node);
        self.ancestors.pop();
        if had {
            self.scopes.pop();
        }
        error
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

const DYNAMIC: &str = "Schedule checks must reference tools statically (e.g. tools.slack.read_thread), not with dynamic bracket expressions.";
const ALIAS: &str = "Schedule checks must call tools directly as tools.<name>(...), not alias or pass the tools object.";

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
        let computed = parent.get("computed").and_then(Value::as_bool).unwrap_or(false);
        let property = field(parent, "property").unwrap_or(&Value::Null);
        if !computed && kind(property) == "Identifier" {
            segments.push(property.get("name").and_then(Value::as_str).unwrap_or_default().to_string());
            current = parent;
            index -= 1;
            continue;
        }
        if computed
            && kind(property) == "Literal"
            && let Some(s) = property.get("value").and_then(Value::as_str)
        {
            segments.push(s.to_string());
            current = parent;
            index -= 1;
            continue;
        }
        return Err((property, DYNAMIC));
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

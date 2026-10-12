//! Effect CLI (`effect/cli` 4.0.1) compatible lexer and parser: `internal/lexer.ts`,
//! `internal/parser.ts`, `internal/command.ts` (`makeParser`), `Primitive.ts`, `CliError.ts`.
//! The observable behaviour (which tokens bind where, error texts and their order) matches the
//! TypeScript CLI; verified against golden captures of the real framework (`tests/golden`).

use indexmap::IndexMap;
use serde_json::{Map, Value};

use super::spec::{ArgDef, ArgMode, Arity, Cmd, FlagDef, IntCheck, Param, Prim};

// ---------------------------------------------------------------------------------------------
// lexer

#[derive(Debug, Clone, PartialEq)]
pub enum Tok {
    Long { name: String, raw: String, value: Option<String> },
    Short { flag: String, raw: String, value: Option<String> },
    Value(String),
}

impl Tok {
    fn is_flag(&self) -> bool {
        !matches!(self, Tok::Value(_))
    }
    fn flag_name(&self) -> &str {
        match self {
            Tok::Long { name, .. } => name,
            Tok::Short { flag, .. } => flag,
            Tok::Value(v) => v,
        }
    }
    fn raw(&self) -> &str {
        match self {
            Tok::Long { raw, .. } | Tok::Short { raw, .. } => raw,
            Tok::Value(v) => v,
        }
    }
    fn inline(&self) -> Option<&String> {
        match self {
            Tok::Long { value, .. } | Tok::Short { value, .. } => value.as_ref(),
            Tok::Value(_) => None,
        }
    }
}

fn negative_number(arg: &str) -> bool {
    static RE: std::sync::LazyLock<regex::Regex> =
        std::sync::LazyLock::new(|| regex::Regex::new(r"^-(?:\d+\.?\d*|\.\d+)(?:[eE][+-]?\d+)?$").unwrap());
    RE.is_match(arg)
}

/// `lex(argv)` → tokens before the first `--` and the trailing operands after it.
pub fn lex(argv: &[String]) -> (Vec<Tok>, Vec<String>) {
    let (head, trailing) = match argv.iter().position(|a| a == "--") {
        Some(i) => (&argv[..i], argv[i + 1..].to_vec()),
        None => (argv, vec![]),
    };
    let mut tokens = vec![];
    for arg in head {
        if !arg.starts_with('-') || negative_number(arg) {
            tokens.push(Tok::Value(arg.clone()));
        } else if let Some(rest) = arg.strip_prefix("--") {
            match rest.find('=') {
                Some(eq) => tokens.push(Tok::Long { name: rest[..eq].into(), raw: arg.clone(), value: Some(rest[eq + 1..].into()) }),
                None => tokens.push(Tok::Long { name: rest.into(), raw: arg.clone(), value: None }),
            }
        } else if arg.chars().count() > 1 {
            let flags = &arg[1..];
            match flags.find('=') {
                Some(eq) => {
                    let flag = &flags[..eq];
                    tokens.push(Tok::Short { flag: flag.into(), raw: format!("-{flag}"), value: Some(flags[eq + 1..].into()) })
                }
                None => {
                    for ch in flags.chars() {
                        tokens.push(Tok::Short { flag: ch.to_string(), raw: format!("-{ch}"), value: None })
                    }
                }
            }
        } else {
            tokens.push(Tok::Value(arg.clone()));
        }
    }
    (tokens, trailing)
}

// ---------------------------------------------------------------------------------------------
// errors (`CliError.ts`)

#[derive(Debug, Clone, PartialEq)]
pub enum CliError {
    UnrecognizedOption { option: String, command: Vec<String>, suggestions: Vec<String> },
    MissingArgument { argument: String },
    UnexpectedArgument { arguments: Vec<String> },
    InvalidValue { option: String, value: String, expected: String, argument: bool },
    UnknownSubcommand { subcommand: String, parent: Vec<String>, suggestions: Vec<String> },
}

impl CliError {
    pub fn tag(&self) -> &'static str {
        match self {
            CliError::UnrecognizedOption { .. } => "UnrecognizedOption",
            CliError::MissingArgument { .. } => "MissingArgument",
            CliError::UnexpectedArgument { .. } => "UnexpectedArgument",
            CliError::InvalidValue { .. } => "InvalidValue",
            CliError::UnknownSubcommand { .. } => "UnknownSubcommand",
        }
    }

    pub fn message(&self) -> String {
        fn suggestion_text(s: &[String]) -> String {
            if s.is_empty() { String::new() } else { format!("\n\n  Did you mean this?\n    {}", s.join("\n    ")) }
        }
        match self {
            CliError::UnrecognizedOption { option, command, suggestions } => {
                let base = if command.is_empty() {
                    format!("Unrecognized flag: {option}")
                } else {
                    format!("Unrecognized flag: {option} in command {}", command.join(" "))
                };
                base + &suggestion_text(suggestions)
            }
            CliError::MissingArgument { argument } => format!("Missing required argument: {argument}"),
            CliError::UnexpectedArgument { arguments } => {
                let label = if arguments.len() == 1 { "argument" } else { "arguments" };
                let list: Vec<String> = arguments.iter().map(|a| serde_json::to_string(a).unwrap()).collect();
                format!("Unexpected positional {label}: {}", list.join(", "))
            }
            CliError::InvalidValue { option, value, expected, argument } => {
                let expectation = if expected.starts_with("Expected ") || expected.starts_with("Expected:") {
                    expected.clone()
                } else {
                    format!("Expected: {expected}")
                };
                if *argument {
                    format!("Invalid value for argument <{option}>: \"{value}\". {expectation}")
                } else if value.is_empty() {
                    format!("Missing value for flag --{option}. {expectation}")
                } else {
                    format!("Invalid value for flag --{option}: \"{value}\". {expectation}")
                }
            }
            CliError::UnknownSubcommand { subcommand, parent, suggestions } => {
                if parent.is_empty() {
                    format!("Unknown subcommand \"{subcommand}\"{}", suggestion_text(suggestions))
                } else {
                    format!("Unknown subcommand \"{subcommand}\" for \"{}\"{}", parent.join(" "), suggestion_text(suggestions))
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------------------------
// primitives (`Primitive.ts`)

pub const TRUE_LITERALS: &[&str] = &["true", "yes", "on", "1", "y"];
pub const FALSE_LITERALS: &[&str] = &["false", "no", "off", "0", "n"];

fn is_bool_literal(v: &str) -> bool {
    TRUE_LITERALS.contains(&v) || FALSE_LITERALS.contains(&v)
}

pub fn type_name(prim: Prim) -> &'static str {
    match prim {
        Prim::Bool => "boolean",
        Prim::Str | Prim::NonEmpty | Prim::PairUrl => "string",
        Prim::Int(_) => "integer",
        Prim::Choice(_) => "choice",
        Prim::KeyValue => "key=value",
    }
}

pub fn choice_keys(prim: Prim) -> Option<&'static [&'static str]> {
    match prim {
        Prim::Choice(c) => Some(c),
        _ => None,
    }
}

/// Parsed value of one primitive.
#[derive(Debug, Clone, PartialEq)]
pub enum PVal {
    Bool(bool),
    Str(String),
    Int(i64),
    Map(IndexMap<String, String>),
}

fn finite_number(v: &str) -> bool {
    static RE: std::sync::LazyLock<regex::Regex> =
        std::sync::LazyLock::new(|| regex::Regex::new(r"^[+-]?\d*\.?\d+(?:[Ee][+-]?\d+)?$").unwrap());
    RE.is_match(v)
}

/// Parse one raw value; `Err(expected)` carries the primitive's failure message.
pub fn parse_prim(prim: Prim, value: &str) -> Result<PVal, String> {
    match prim {
        Prim::Bool => {
            if TRUE_LITERALS.contains(&value) {
                Ok(PVal::Bool(true))
            } else if FALSE_LITERALS.contains(&value) {
                Ok(PVal::Bool(false))
            } else {
                Err(r#"Expected "true" | "yes" | "on" | "1" | "y" | "false" | "no" | "off" | "0" | "n""#.into())
            }
        }
        Prim::Str => Ok(PVal::Str(value.into())),
        Prim::NonEmpty => {
            if value.is_empty() {
                Err("Schema validation failed: Expected a value with a length of at least 1".into())
            } else {
                Ok(PVal::Str(value.into()))
            }
        }
        Prim::Int(check) => {
            if !finite_number(value) {
                return Err("Expected a string representing a finite number".into());
            }
            let n: f64 = value.parse().map_err(|_| "Expected a string representing a finite number".to_string())?;
            if !n.is_finite() || n.fract() != 0.0 || n.abs() > 9_007_199_254_740_991.0 {
                return Err("Expected an integer".into());
            }
            let n = n as i64;
            match check {
                IntCheck::None => {}
                IntCheck::Min(min) if n < min => {
                    return Err(format!("Schema validation failed: Expected a value greater than or equal to {min}"));
                }
                IntCheck::Between(a, b) if n < a || n > b => {
                    return Err(format!("Schema validation failed: Expected a value between {a} and {b}"));
                }
                _ => {}
            }
            Ok(PVal::Int(n))
        }
        Prim::Choice(choices) => {
            if choices.contains(&value) {
                Ok(PVal::Str(value.into()))
            } else {
                Err(choices.iter().map(|c| format!("\"{c}\"")).collect::<Vec<_>>().join(" | "))
            }
        }
        Prim::KeyValue => {
            let Some(sep) = value.find('=') else {
                return Err(format!("Invalid key=value format. Expected format: key=value, got: {value}"));
            };
            let (k, v) = (&value[..sep], &value[sep + 1..]);
            if k.is_empty() || v.is_empty() {
                return Err(format!("Invalid key=value format. Both key and value must be non-empty. Got: {value}"));
            }
            let mut m = IndexMap::new();
            m.insert(k.to_string(), v.to_string());
            Ok(PVal::Map(m))
        }
        Prim::PairUrl => {
            const MSG: &str = "Expected an HTTP(S) server URL without credentials, query parameters, or a fragment";
            let url = url::Url::parse(value).map_err(|_| MSG.to_string())?;
            let bad = !matches!(url.scheme(), "http" | "https")
                || !url.username().is_empty()
                || url.password().is_some()
                || url.query().is_some_and(|q| !q.is_empty())
                || url.fragment().is_some_and(|f| !f.is_empty());
            if bad {
                return Err(MSG.into());
            }
            Ok(PVal::Str(url.as_str().trim_end_matches('/').to_string()))
        }
    }
}

// ---------------------------------------------------------------------------------------------
// registries

struct Registry<'a> {
    params: Vec<&'a FlagDef>,
}

struct Resolved<'a> {
    param: &'a FlagDef,
    negated: bool,
}

impl<'a> Registry<'a> {
    fn new(params: Vec<&'a FlagDef>) -> Self {
        Registry { params }
    }
    fn index(&self, key: &str) -> Option<&'a FlagDef> {
        self.params.iter().copied().find(|p| p.name == key || p.alias == Some(key))
    }
    fn resolve(&self, token: &Tok) -> Option<Resolved<'a>> {
        let name = token.flag_name();
        let direct = self.index(name);
        if let Some(d) = direct
            && d.name == name
        {
            return Some(Resolved { param: d, negated: false });
        }
        if let Tok::Long { name, .. } = token
            && let Some(canonical) = name.strip_prefix("no-")
            && let Some(p) = self.index(canonical)
            && p.name == canonical
            && p.prim == Prim::Bool
        {
            return Some(Resolved { param: p, negated: true });
        }
        direct.map(|param| Resolved { param, negated: false })
    }
}

struct Cursor<'t> {
    tokens: &'t [Tok],
    i: usize,
}

impl<'t> Cursor<'t> {
    fn new(tokens: &'t [Tok]) -> Self {
        Cursor { tokens, i: 0 }
    }
    fn peek(&self) -> Option<&'t Tok> {
        self.tokens.get(self.i)
    }
    fn take(&mut self) -> Option<&'t Tok> {
        let t = self.tokens.get(self.i);
        if t.is_some() {
            self.i += 1;
        }
        t
    }
    fn rest(&self) -> &'t [Tok] {
        &self.tokens[self.i.min(self.tokens.len())..]
    }
}

fn missing_value(spec: &FlagDef) -> CliError {
    let expected = match choice_keys(spec.prim) {
        Some(c) => c.join(" | "),
        None => spec.metavar.unwrap_or(type_name(spec.prim)).to_string(),
    };
    CliError::InvalidValue { option: spec.name.into(), value: String::new(), expected, argument: false }
}

/// `consumeFlagValueWithTokens`: the value (or error) plus the tokens it consumed.
fn consume_value(cursor: &mut Cursor, token: &Tok, spec: &FlagDef, negated: bool) -> (Result<Option<String>, CliError>, Vec<Tok>) {
    if negated {
        let invalid = |v: &str| CliError::InvalidValue {
            option: spec.name.into(),
            value: v.into(),
            expected: format!("omit the value and use {} by itself to set --{} to false", token.raw(), spec.name),
            argument: false,
        };
        if let Some(v) = token.inline() {
            return (Err(invalid(v)), vec![]);
        }
        if let Some(Tok::Value(v)) = cursor.peek()
            && is_bool_literal(v)
        {
            let t = cursor.take().unwrap().clone();
            return (Err(invalid(v)), vec![t]);
        }
        return (Ok(Some("false".into())), vec![]);
    }
    if let Some(v) = token.inline() {
        return (Ok(Some(v.clone())), vec![]);
    }
    if spec.prim == Prim::Bool {
        if let Some(Tok::Value(v)) = cursor.peek()
            && is_bool_literal(v)
        {
            let t = cursor.take().unwrap().clone();
            return (Ok(Some(v.clone())), vec![t]);
        }
        return (Ok(Some("true".into())), vec![]);
    }
    if let Some(Tok::Value(v)) = cursor.peek() {
        let t = cursor.take().unwrap().clone();
        return (Ok(Some(v.clone())), vec![t]);
    }
    (Err(missing_value(spec)), vec![])
}

pub type FlagMap = IndexMap<String, Vec<String>>;

fn empty_map(params: &[&FlagDef]) -> FlagMap {
    params.iter().map(|p| (p.name.to_string(), vec![])).collect()
}

// ---------------------------------------------------------------------------------------------
// global flags (`consumeGlobalFlags`)

pub struct GlobalScan {
    pub flags: FlagMap,
    pub remainder: Vec<Tok>,
    pub errors: Vec<CliError>,
}

pub fn consume_global_flags(tokens: &[Tok], command: &Cmd, globals: &[FlagDef]) -> GlobalScan {
    let registry = Registry::new(globals.iter().collect());
    let mut out = GlobalScan { flags: empty_map(&registry.params), remainder: vec![], errors: vec![] };
    fn level(tokens: &[Tok], command: &Cmd, registry: &Registry, out: &mut GlobalScan) -> Vec<Tok> {
        let local = Registry::new(command.flags().collect());
        let mut cursor = Cursor::new(tokens);
        let mut remainder = vec![];
        let mut awaiting_first = true;
        while let Some(token) = cursor.take() {
            if token.is_flag() {
                let l = local.resolve(token);
                let g = registry.resolve(token);
                if let Some(l) = &l
                    && (g.is_none()
                        || !awaiting_first
                        || !local_precedes_subcommand(token, cursor.rest(), l, command, &local, registry))
                {
                    remainder.push(token.clone());
                    let (_, consumed) = consume_value(&mut cursor, token, l.param, l.negated);
                    remainder.extend(consumed);
                    continue;
                }
                if let Some(g) = g {
                    let (value, _) = consume_value(&mut cursor, token, g.param, g.negated);
                    match value {
                        Err(e) => out.errors.push(e),
                        Ok(Some(v)) => out.flags.get_mut(g.param.name).unwrap().push(v),
                        Ok(None) => {}
                    }
                    continue;
                }
                remainder.push(token.clone());
                continue;
            }
            if let Tok::Value(v) = token
                && awaiting_first
            {
                if let Some(sub) = command.find(v) {
                    remainder.push(token.clone());
                    remainder.extend(level(cursor.rest(), sub, registry, out));
                    return remainder;
                }
                awaiting_first = false;
            }
            remainder.push(token.clone());
        }
        remainder
    }
    let rem = level(tokens, command, &registry, &mut out);
    out.remainder = rem;
    out
}

fn local_precedes_subcommand(token: &Tok, rest: &[Tok], resolved: &Resolved, command: &Cmd, local: &Registry, globals: &Registry) -> bool {
    let mut cursor = Cursor::new(rest);
    let _ = consume_value(&mut cursor, token, resolved.param, resolved.negated);
    while let Some(t) = cursor.take() {
        if t.is_flag() {
            if let Some(k) = local.resolve(t).or_else(|| globals.resolve(t)) {
                let _ = consume_value(&mut cursor, t, k.param, k.negated);
            }
            continue;
        }
        if let Tok::Value(v) = t {
            return command.find(v).is_some();
        }
    }
    false
}

// ---------------------------------------------------------------------------------------------
// command levels (`parseArgs`)

#[derive(Debug, Clone, Default)]
pub struct ParsedTokens {
    pub flags: FlagMap,
    pub arguments: Vec<String>,
    pub subcommand: Option<(String, Box<ParsedTokens>)>,
    pub errors: Vec<CliError>,
}

impl ParsedTokens {
    pub fn command_path(&self) -> Vec<String> {
        match &self.subcommand {
            None => vec![],
            Some((name, sub)) => {
                let mut p = vec![name.clone()];
                p.extend(sub.command_path());
                p
            }
        }
    }
    pub fn all_errors(&self) -> Vec<CliError> {
        self.errors.clone()
    }
    /// The leaf level's parsed tokens.
    pub fn leaf(&self) -> &ParsedTokens {
        match &self.subcommand {
            Some((_, sub)) => sub.leaf(),
            None => self,
        }
    }
}

fn suggest(input: &str, candidates: &[String]) -> Vec<String> {
    fn lev(a: &[u16], b: &[u16]) -> usize {
        let (m, n) = (a.len(), b.len());
        let mut dp = vec![vec![0usize; n + 1]; m + 1];
        for (i, row) in dp.iter_mut().enumerate() {
            row[0] = i;
        }
        for j in 0..=n {
            dp[0][j] = j;
        }
        for i in 1..=m {
            for j in 1..=n {
                let cost = usize::from(a[i - 1] != b[j - 1]);
                dp[i][j] = (dp[i - 1][j] + 1).min(dp[i][j - 1] + 1).min(dp[i - 1][j - 1] + cost);
            }
        }
        dp[m][n]
    }
    let a: Vec<u16> = input.encode_utf16().collect();
    let mut d: Vec<(usize, &String)> = candidates
        .iter()
        .map(|c| (lev(&a, &c.encode_utf16().collect::<Vec<_>>()), c))
        .filter(|(d, _)| *d <= 2)
        .collect();
    d.sort_by_key(|(d, _)| *d);
    let Some(min) = d.first().map(|x| x.0) else { return vec![] };
    d.into_iter().filter(|(x, _)| *x == min).map(|(_, c)| c.clone()).collect()
}

fn unrecognized(token: &Tok, params: &[&FlagDef], path: &[String]) -> CliError {
    let printable = match token {
        Tok::Long { name, .. } => format!("--{name}"),
        Tok::Short { flag, .. } => format!("-{flag}"),
        Tok::Value(v) => v.clone(),
    };
    let mut valid = vec![];
    for p in params {
        if p.hidden {
            continue;
        }
        valid.push(p.name.to_string());
        if p.prim == Prim::Bool {
            valid.push(format!("no-{}", p.name));
        }
        if let Some(a) = p.alias {
            valid.push(a.to_string());
        }
    }
    let suggestions =
        suggest(token.flag_name(), &valid).into_iter().map(|n| if n.chars().count() == 1 { format!("-{n}") } else { format!("--{n}") }).collect();
    CliError::UnrecognizedOption { option: printable, command: path.to_vec(), suggestions }
}

pub fn parse_args(tokens: &[Tok], trailing: &[String], command: &Cmd, parent_path: &[String]) -> ParsedTokens {
    let mut path = parent_path.to_vec();
    path.push(command.name.to_string());
    let registry = Registry::new(command.flags().collect());
    let mut flags = empty_map(&registry.params);
    let mut arguments: Vec<String> = vec![];
    let mut errors: Vec<CliError> = vec![];
    let mut awaiting_first = true;
    let mut cursor = Cursor::new(tokens);
    while let Some(token) = cursor.take() {
        if token.is_flag() {
            match registry.resolve(token) {
                None => errors.push(unrecognized(token, &registry.params, &path)),
                Some(r) => match consume_value(&mut cursor, token, r.param, r.negated).0 {
                    Err(e) => errors.push(e),
                    Ok(Some(v)) => flags.get_mut(r.param.name).unwrap().push(v),
                    Ok(None) => {}
                },
            }
            continue;
        }
        let Tok::Value(value) = token else { continue };
        if awaiting_first {
            if let Some(sub) = command.find(value) {
                let mut selected = path.clone();
                selected.push(sub.name.to_string());
                // Local flags are not inherited by subcommands.
                for p in &registry.params {
                    if flags.get(p.name).is_some_and(|v| !v.is_empty()) {
                        errors.push(CliError::UnrecognizedOption {
                            option: format!("--{}", p.name),
                            command: selected.clone(),
                            suggestions: vec![],
                        });
                    }
                }
                let child = parse_args(cursor.rest(), trailing, sub, &path);
                let mut all = errors;
                all.extend(child.errors.iter().cloned());
                return ParsedTokens { flags, arguments: vec![], subcommand: Some((sub.name.to_string(), Box::new(child))), errors: all };
            }
            let expects_args = command.args().next().is_some();
            if !expects_args && !command.subcommands.is_empty() {
                let mut keys = vec![];
                for s in &command.subcommands {
                    keys.push(s.name.to_string());
                    if let Some(a) = s.alias {
                        keys.push(a.to_string());
                    }
                }
                errors.push(CliError::UnknownSubcommand {
                    subcommand: value.clone(),
                    parent: path.clone(),
                    suggestions: suggest(value, &keys),
                });
            }
            awaiting_first = false;
        }
        arguments.push(value.clone());
    }
    arguments.extend(trailing.iter().cloned());
    ParsedTokens { flags, arguments, subcommand: None, errors }
}

// ---------------------------------------------------------------------------------------------
// typed input (`makeParser`)

#[derive(Debug, Clone, PartialEq)]
pub enum V {
    Bool(bool),
    Str(String),
    Int(i64),
    None,
    Some(Box<V>),
    List(Vec<V>),
    Map(IndexMap<String, String>),
}

/// The decoded input of one command, keyed by TS property name in declaration order.
#[derive(Debug, Clone, Default)]
pub struct Input {
    pub values: IndexMap<&'static str, V>,
}

impl Input {
    fn get(&self, key: &str) -> &V {
        self.values.get(key).unwrap_or(&V::None)
    }
    pub fn flag(&self, key: &str) -> bool {
        matches!(self.get(key), V::Bool(true))
    }
    pub fn opt_bool(&self, key: &str) -> Option<bool> {
        match self.get(key) {
            V::Some(v) => match **v {
                V::Bool(b) => Some(b),
                _ => None,
            },
            V::Bool(b) => Some(*b),
            _ => None,
        }
    }
    pub fn str(&self, key: &str) -> Option<String> {
        match self.get(key) {
            V::Str(s) => Some(s.clone()),
            V::Some(v) => match &**v {
                V::Str(s) => Some(s.clone()),
                _ => None,
            },
            _ => None,
        }
    }
    pub fn int(&self, key: &str) -> Option<i64> {
        match self.get(key) {
            V::Int(n) => Some(*n),
            V::Some(v) => match **v {
                V::Int(n) => Some(n),
                _ => None,
            },
            _ => None,
        }
    }
    pub fn list(&self, key: &str) -> Vec<String> {
        match self.get(key) {
            V::List(items) => items
                .iter()
                .filter_map(|v| match v {
                    V::Str(s) => Some(s.clone()),
                    _ => None,
                })
                .collect(),
            _ => vec![],
        }
    }
    pub fn map(&self, key: &str) -> Option<IndexMap<String, String>> {
        match self.get(key) {
            V::Some(v) => match &**v {
                V::Map(m) => Some(m.clone()),
                _ => None,
            },
            _ => None,
        }
    }

    /// `JSON.stringify(input)` as the TS handler would see it (Options encoded by Effect).
    pub fn dump(&self) -> String {
        fn conv(v: &V) -> Value {
            match v {
                V::Bool(b) => Value::Bool(*b),
                V::Str(s) => Value::String(s.clone()),
                V::Int(n) => Value::from(*n),
                V::None => serde_json::json!({"_id": "Option", "_tag": "None"}),
                V::Some(v) => serde_json::json!({"_id": "Option", "_tag": "Some", "value": conv(v)}),
                V::List(items) => Value::Array(items.iter().map(conv).collect()),
                V::Map(m) => Value::Object(m.iter().map(|(k, v)| (k.clone(), Value::String(v.clone()))).collect::<Map<_, _>>()),
            }
        }
        let obj: Map<String, Value> = self.values.iter().map(|(k, v)| (k.to_string(), conv(v))).collect();
        serde_json::to_string(&Value::Object(obj)).unwrap()
    }
}

fn pval(p: PVal) -> V {
    match p {
        PVal::Bool(b) => V::Bool(b),
        PVal::Str(s) => V::Str(s),
        PVal::Int(n) => V::Int(n),
        PVal::Map(m) => V::Map(m),
    }
}

fn parse_flag(f: &FlagDef, values: &[String]) -> Result<V, CliError> {
    let one = |raw: &str| {
        parse_prim(f.prim, raw).map(pval).map_err(|expected| CliError::InvalidValue {
            option: f.name.into(),
            value: raw.into(),
            expected,
            argument: false,
        })
    };
    if f.prim == Prim::KeyValue {
        if values.is_empty() {
            return Ok(V::None);
        }
        let mut merged = IndexMap::new();
        for raw in values {
            if let V::Map(m) = one(raw)? {
                merged.extend(m);
            }
        }
        return Ok(V::Some(Box::new(V::Map(merged))));
    }
    match f.arity {
        Arity::Default(default) => match values.first() {
            Some(raw) => one(raw),
            None => Ok(match f.prim {
                Prim::Bool => V::Bool(default == "true"),
                Prim::Int(_) => V::Int(default.parse().unwrap_or(0)),
                _ => V::Str(default.into()),
            }),
        },
        Arity::Optional => match values.first() {
            Some(raw) => Ok(V::Some(Box::new(one(raw)?))),
            None => Ok(V::None),
        },
        Arity::Many => Ok(V::List(values.iter().map(|r| one(r)).collect::<Result<Vec<_>, _>>()?)),
    }
}

fn parse_arg(a: &ArgDef, args: &mut Vec<String>) -> Result<V, CliError> {
    let check = |raw: String| -> Result<V, CliError> {
        parse_prim(a.prim, &raw).map(pval).map_err(|expected| CliError::InvalidValue {
            option: a.name.into(),
            value: raw.clone(),
            expected,
            argument: true,
        })
    };
    match a.mode {
        ArgMode::Required => {
            if args.is_empty() {
                return Err(CliError::MissingArgument { argument: a.name.into() });
            }
            check(args.remove(0))
        }
        ArgMode::Optional => {
            if args.is_empty() {
                return Ok(V::None);
            }
            Ok(V::Some(Box::new(check(args.remove(0))?)))
        }
        ArgMode::Variadic { min, max } => {
            let take = max.map_or(args.len(), |m| m.min(args.len()));
            if take < min {
                return Err(CliError::MissingArgument { argument: a.name.into() });
            }
            let taken: Vec<String> = args.drain(..take).collect();
            Ok(V::List(taken.into_iter().map(check).collect::<Result<Vec<_>, _>>()?))
        }
    }
}

/// `commandImpl.parse` for the selected leaf command.
pub fn decode(command: &Cmd, parsed: &ParsedTokens) -> Result<Input, CliError> {
    let mut args = parsed.arguments.clone();
    let mut input = Input::default();
    for p in &command.params {
        let v = match p {
            Param::Flag(f) => {
                let empty = vec![];
                parse_flag(f, parsed.flags.get(f.name).unwrap_or(&empty))?
            }
            Param::Arg(a) => parse_arg(a, &mut args)?,
        };
        input.values.insert(
            match p {
                Param::Flag(f) => f.key,
                Param::Arg(a) => a.key,
            },
            v,
        );
    }
    if !args.is_empty() {
        return Err(CliError::UnexpectedArgument { arguments: args });
    }
    Ok(input)
}

/// Parse a global flag's collected values (`flag.flag.parse(emptyArgs)`).
pub fn decode_global(f: &FlagDef, values: &[String]) -> Result<V, CliError> {
    parse_flag(f, values)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(v: &[&str]) -> Vec<String> {
        v.iter().map(|x| x.to_string()).collect()
    }

    #[test]
    fn lexes_like_effect() {
        let (t, rest) = lex(&s(&["run", "-abc", "--x=1", "-5", "-c=false", "-", "--", "--y"]));
        assert_eq!(rest, s(&["--y"]));
        assert_eq!(t.len(), 8);
        assert_eq!(t[4], Tok::Long { name: "x".into(), raw: "--x=1".into(), value: Some("1".into()) });
        assert_eq!(t[5], Tok::Value("-5".into()));
        assert_eq!(t[6], Tok::Short { flag: "c".into(), raw: "-c".into(), value: Some("false".into()) });
        assert_eq!(t[7], Tok::Value("-".into()));
    }

    #[test]
    fn ints() {
        for (v, ok) in [("1e3", true), ("+5", true), ("5.", false), (".5", false), (" 5", false), ("9007199254740992", false)] {
            assert_eq!(parse_prim(Prim::Int(IntCheck::None), v).is_ok(), ok, "{v}");
        }
        assert_eq!(parse_prim(Prim::Int(IntCheck::None), ".5").unwrap_err(), "Expected an integer");
        assert_eq!(parse_prim(Prim::Int(IntCheck::None), "1e3").unwrap(), PVal::Int(1000));
    }

    #[test]
    fn suggestions() {
        let c = s(&["format", "no-format", "c"]);
        assert_eq!(suggest("formt", &c), s(&["format"]));
        assert!(suggest("zzzzzz", &c).is_empty());
    }
}

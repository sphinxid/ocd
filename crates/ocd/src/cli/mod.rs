//! The CLI framework layer: an `effect/cli`-compatible parser, help renderer and the
//! `Command.runWith` control flow (global action flags, parse errors, settings).

pub mod help;
pub mod parse;
pub mod spec;

use parse::{CliError, V};
use spec::{Cmd, FlagDef};

/// What the command line resolved to.
#[derive(Debug)]
pub enum Outcome {
    /// Print help for `path` (stdout), then the errors (stderr). Exit 1 when errors exist.
    Help { path: Vec<String>, errors: Vec<CliError> },
    Version,
    Completions(&'static str),
    Wizard,
    /// A failure outside the ShowHelp channel (`Cause.pretty` on stderr, exit 1).
    Fatal(String),
    Run(Invocation),
}

#[derive(Debug, Clone)]
pub struct Invocation {
    /// `["ocd", "auth", "list"]`
    pub path: Vec<String>,
    /// Handler key (`"run"`, `"auth list"`, `""` for the root).
    pub name: String,
    pub input: parse::Input,
    pub log_level: Option<String>,
    pub print_logs: bool,
    /// The server settings in scope for this command (`Standalone`, `Server`).
    pub settings: Settings,
}

/// `GlobalFlag.Setting` values of the server flags (2.2), as `ServerConnection.resolve` reads them:
/// `server` when given; `standalone` (default false) only in the commands that declare it.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Settings {
    pub server: Option<String>,
    pub standalone: Option<bool>,
}

static SETTINGS: std::sync::OnceLock<Settings> = std::sync::OnceLock::new();

/// Record the running command's server settings (once, before its handler runs).
pub fn set_settings(s: Settings) {
    let _ = SETTINGS.set(s);
}

/// The running command's server settings.
pub fn settings() -> Settings {
    SETTINGS.get().cloned().unwrap_or_default()
}

impl Settings {
    /// `JSON.stringify({ server, standalone })` as the parse dump prints it.
    pub fn dump(&self) -> String {
        serde_json::json!({ "server": self.server, "standalone": self.standalone }).to_string()
    }
}

impl Invocation {
    /// The canonical leaf name (`HANDLER <name>` in the parse dump).
    pub fn leaf(&self) -> &str {
        self.path.last().map(String::as_str).unwrap_or(spec::CLI_NAME)
    }
}

const COMPLETIONS_BASH: &str = include_str!("../../assets/completions/bash");
const COMPLETIONS_ZSH: &str = include_str!("../../assets/completions/zsh");
const COMPLETIONS_FISH: &str = include_str!("../../assets/completions/fish");
const COMPLETIONS_POWERSHELL: &str = include_str!("../../assets/completions/powershell");

/// Push `flags` not yet present by name (`dedupeGlobalFlags`: the declarations are shared objects upstream).
fn dedupe_into(out: &mut Vec<FlagDef>, flags: &[FlagDef]) {
    for f in flags {
        if !out.iter().any(|o| o.name == f.name) {
            out.push(f.clone());
        }
    }
}

/// `getGlobalFlagsForCommandTree`: built-ins, then every declaration in the tree (depth first).
pub fn tree_globals(root: &Cmd) -> Vec<FlagDef> {
    fn visit(c: &Cmd, out: &mut Vec<FlagDef>) {
        dedupe_into(out, &c.globals);
        for s in &c.subcommands {
            visit(s, out);
        }
    }
    let mut out = spec::global_flags();
    visit(root, &mut out);
    out
}

/// `getGlobalFlagsForCommandPath`: built-ins, then the declarations from the root to the leaf of `path`.
pub fn path_globals(root: &Cmd, path: &[String]) -> Vec<FlagDef> {
    let mut out = spec::global_flags();
    let mut cmd = root;
    dedupe_into(&mut out, &cmd.globals);
    for name in path.iter().skip(1) {
        match cmd.subcommands.iter().find(|s| s.name == name) {
            Some(s) => {
                cmd = s;
                dedupe_into(&mut out, &cmd.globals);
            }
            None => break,
        }
    }
    out
}

/// `Command.runWith(...)(args)`, up to (not including) running the handler.
pub fn resolve(root: &Cmd, args: &[String]) -> Outcome {
    let globals = tree_globals(root);
    let (tokens, trailing) = parse::lex(args);
    let scan = parse::consume_global_flags(&tokens, root, &globals);
    let parsed = parse::parse_args(&scan.remainder, &trailing, root, &[]);
    let mut path = vec![root.name.to_string()];
    path.extend(parsed.command_path());
    let active = path_globals(root, &path);

    // Globals passed outside the active command scope (`getOutOfScopeGlobalFlagErrors`).
    let out_of_scope: Vec<CliError> = globals
        .iter()
        .filter(|f| !active.iter().any(|a| a.name == f.name))
        .filter(|f| scan.flags.get(f.name).is_some_and(|v| !v.is_empty()))
        .map(|f| CliError::UnrecognizedOption { option: format!("--{}", f.name), command: path.clone(), suggestions: vec![] })
        .collect();
    if !scan.errors.is_empty() || !out_of_scope.is_empty() {
        let mut errors = scan.errors.clone();
        errors.extend(out_of_scope);
        errors.extend(parsed.all_errors());
        return Outcome::Help { path, errors };
    }

    let present = |name: &str| scan.flags.get(name).is_some_and(|v| !v.is_empty());
    // action flags, first present wins: help, version, wizard, completions
    if present("help") {
        return Outcome::Help { path, errors: vec![] };
    }
    if present("version") {
        return Outcome::Version;
    }
    if present("wizard") {
        return Outcome::Wizard;
    }
    if present("completions") {
        let def = globals.iter().find(|f| f.name == "completions").unwrap();
        return match parse::decode_global(def, &scan.flags["completions"]) {
            Err(e) => Outcome::Fatal(pretty_cli(&e)),
            Ok(V::Some(v)) => match *v {
                V::Str(s) => Outcome::Completions(match s.as_str() {
                    "zsh" => COMPLETIONS_ZSH,
                    "fish" => COMPLETIONS_FISH,
                    "powershell" | "pwsh" => COMPLETIONS_POWERSHELL,
                    _ => COMPLETIONS_BASH,
                }),
                _ => Outcome::Fatal("Error: invalid completions value".into()),
            },
            Ok(_) => Outcome::Fatal("Error: invalid completions value".into()),
        };
    }

    if !parsed.errors.is_empty() {
        return Outcome::Help { path, errors: parsed.errors.clone() };
    }
    let mut command = root;
    for name in path.iter().skip(1) {
        command = command.subcommands.iter().find(|c| &c.name == name).expect("parsed subcommand exists");
    }
    let leaf = parsed.leaf();
    let input = match parse::decode(command, leaf) {
        Ok(i) => i,
        Err(e) => return Outcome::Help { path, errors: vec![e] },
    };
    // A group without a `$` handler and no subcommand: ShowHelp without errors.
    if !command.handler {
        return Outcome::Help { path, errors: vec![] };
    }

    let setting = |name: &str| -> Result<V, String> {
        let def: &FlagDef = globals.iter().find(|f| f.name == name).unwrap();
        parse::decode_global(def, scan.flags.get(name).map(Vec::as_slice).unwrap_or(&[])).map_err(|e| pretty_cli(&e))
    };
    let log_level = match setting("log-level") {
        Ok(V::Some(v)) => match *v {
            V::Str(s) => Some(s),
            _ => None,
        },
        Ok(_) => None,
        Err(e) => return Outcome::Fatal(e),
    };
    let print_logs = match setting("print-logs") {
        Ok(V::Bool(b)) => b,
        Ok(_) => false,
        Err(e) => return Outcome::Fatal(e),
    };
    // The server settings the command declares (`Effect.serviceOption(Server|Standalone)`).
    let mut settings = Settings::default();
    if active.iter().any(|f| f.name == "server") {
        match setting("server") {
            Ok(V::Some(v)) => {
                if let V::Str(s) = *v {
                    settings.server = Some(s);
                }
            }
            Ok(_) => {}
            Err(e) => return Outcome::Fatal(e),
        }
    }
    if active.iter().any(|f| f.name == "standalone") {
        match setting("standalone") {
            Ok(V::Bool(b)) => settings.standalone = Some(b),
            Ok(_) => {}
            Err(e) => return Outcome::Fatal(e),
        }
    }
    let name = path[1..].join(" ");
    Outcome::Run(Invocation { path, name, input, log_level, print_logs, settings })
}

/// `Cause.pretty` first line for a CLI error that escaped the ShowHelp channel.
fn pretty_cli(e: &CliError) -> String {
    format!("~effect/cli/CliError/{}: {}", e.tag(), e.message())
}

/// Help colors (`TerminalCapabilities.canColor`, effect 4.0.6): never with `TERM=dumb` or a non-empty
/// `NO_COLOR`; otherwise when `FORCE_COLOR` is set to anything but `0`/`false`, or stdout is a TTY.
pub fn colors() -> help::Colors {
    let var = |k: &str| std::env::var(k).ok();
    help::Colors { on: can_color(var("TERM").as_deref(), var("NO_COLOR").as_deref(), var("FORCE_COLOR").as_deref(), crate::term::stdout_is_tty()) }
}

fn can_color(term: Option<&str>, no_color: Option<&str>, force_color: Option<&str>, tty: bool) -> bool {
    if term == Some("dumb") {
        return false;
    }
    let present = |v: Option<&str>| v.is_some_and(|v| !v.is_empty());
    let forcing = present(force_color) && force_color != Some("0") && !force_color.is_some_and(|v| v.eq_ignore_ascii_case("false"));
    !present(no_color) && (forcing || tty)
}

/// Print an outcome that does not run a handler; returns the exit code.
pub fn emit(root: &Cmd, outcome: &Outcome) -> i32 {
    use std::io::Write;
    let c = colors();
    match outcome {
        Outcome::Help { path, errors } => {
            let text = help::render(root, path, &path_globals(root, path), c);
            let mut out = std::io::stdout().lock();
            let _ = writeln!(out, "{text}");
            let _ = out.flush();
            if errors.is_empty() {
                return 0;
            }
            let list: Vec<(&str, String)> = errors.iter().map(|e| (e.tag(), e.message())).collect();
            eprintln!("{}", help::errors(&list, c));
            1
        }
        Outcome::Version => {
            match crate::BUILD {
                Some(build) => println!("{} (opencode {})", help::version(root.name, build, c), oc_util::version::VERSION),
                None => println!("{}", help::version(root.name, oc_util::version::VERSION, c)),
            }
            0
        }
        Outcome::Completions(script) => {
            print!("{script}");
            0
        }
        Outcome::Wizard => {
            eprintln!("Error: Wizard mode requires an interactive prompt and is not supported by this build");
            1
        }
        Outcome::Fatal(msg) => {
            eprintln!("{msg}");
            1
        }
        Outcome::Run(_) => 0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn colors_follow_terminal_capabilities() {
        assert!(can_color(None, None, None, true));
        assert!(!can_color(None, None, None, false));
        assert!(!can_color(Some("dumb"), None, Some("1"), true));
        assert!(can_color(Some("xterm"), None, Some("1"), false));
        assert!(can_color(None, Some(""), Some("true"), false));
        assert!(!can_color(None, None, Some("0"), false));
        assert!(!can_color(None, None, Some("FALSE"), false));
        assert!(!can_color(None, Some("1"), Some("1"), true));
    }

    fn run(args: &[&str]) -> Outcome {
        resolve(&spec::root(), &args.iter().map(|s| s.to_string()).collect::<Vec<_>>())
    }

    #[test]
    fn server_flags_are_accepted_anywhere_in_their_commands() {
        let url = "http://127.0.0.1:1";
        for args in [
            vec!["--server", url, "session", "list"],
            vec!["session", "--server", url, "list"],
            vec!["session", "list", "--server", url],
            vec!["models", "--server", url],
            vec!["run", "hello", "--server", url],
            vec!["debug", "--server", url, "agents"],
            vec!["mcp", "--server", url, "auth", "linear"],
        ] {
            match run(&args) {
                Outcome::Run(inv) => assert_eq!(inv.settings, Settings { server: Some(url.into()), standalone: Some(false) }, "{args:?}"),
                other => panic!("{args:?}: {other:?}"),
            }
        }
        // The root keeps them as its own flags.
        match run(&["--server", url]) {
            Outcome::Run(inv) => {
                assert_eq!(inv.input.str("server").as_deref(), Some(url));
                assert_eq!(inv.settings, Settings::default());
            }
            other => panic!("{other:?}"),
        }
        for args in [
            vec!["plugin", "add", "x", "--server", url],
            vec!["mcp", "add", "x", "--url", "http://y", "--server", url],
            vec!["serve", "--server", url],
            vec!["--server", url, "serve"],
            vec!["service", "status", "--server", url],
        ] {
            match run(&args) {
                Outcome::Help { errors, .. } => {
                    assert!(matches!(&errors[0], CliError::UnrecognizedOption { option, .. } if option == "--server"), "{args:?}: {errors:?}")
                }
                other => panic!("{args:?}: {other:?}"),
            }
        }
    }
}

//! The CLI framework layer: an `effect/cli`-compatible parser, help renderer and the
//! `Command.runWith` control flow (global action flags, parse errors, settings).

pub mod help;
pub mod parse;
pub mod prompt;
pub mod spec;
pub mod wizard;

use parse::{CliError, V};
use spec::{Cmd, FlagDef};

/// What the command line resolved to.
#[derive(Debug)]
pub enum Outcome {
    /// Print help for `path` (stdout), then the errors (stderr). Exit 1 when errors exist. `requested`: the
    /// `--help` action, which succeeds; any other help (`ShowHelp`) fails the process, as the log records.
    Help { path: Vec<String>, errors: Vec<CliError>, requested: bool },
    Version,
    Completions(&'static str),
    /// `--wizard` from the command at `path` (run by [`run_wizard`]).
    Wizard { path: Vec<String> },
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

impl Outcome {
    /// Whether the process fails after this outcome is emitted (`CliError.ShowHelp` or another error), which
    /// the log records; `--help`, `--version` and completions succeed.
    pub fn fails(&self) -> bool {
        match self {
            Outcome::Help { requested, errors, .. } => !*requested || !errors.is_empty(),
            Outcome::Fatal(_) => true,
            _ => false,
        }
    }
}

/// How the log records a `CliError.ShowHelp` failure (`Cause.pretty` of the failed effect).
pub const SHOW_HELP_CAUSE: &str = "Cause([Fail(~effect/cli/CliError/ShowHelp: Help requested)])";

/// A handler answered with help (`new CliError.ShowHelp(...)`): the help and its errors are printed, and the
/// process exits 1.
#[derive(Debug)]
pub struct ShowHelp;

impl std::fmt::Display for ShowHelp {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Help requested")
    }
}

impl std::error::Error for ShowHelp {}

/// Print the help for `path` with `errors` from inside a handler, then fail with [`ShowHelp`].
pub fn show_help(path: &[&str], errors: Vec<CliError>) -> anyhow::Error {
    let root = spec::root();
    emit(&root, &Outcome::Help { path: path.iter().map(|s| s.to_string()).collect(), errors, requested: false });
    ShowHelp.into()
}

/// The arguments as the log records them (`loggedArgs`, `index.ts`): `service set <key> <value>` and
/// `service set env <name> <value>` carry secrets such as the password or a provider key, so everything after the
/// key (or the variable name) is `<redacted>`. Flags and their values may come first, so the pair is found anywhere.
pub fn logged_args(args: &[String]) -> Vec<String> {
    let set = (0..args.len()).find(|&i| args[i] == "service" && args.get(i + 1).is_some_and(|a| a == "set"));
    let kept = match set {
        None => args.len(),
        Some(i) => i + if args.get(i + 2).is_some_and(|a| a == "env") { 4 } else { 3 },
    };
    args.iter().enumerate().map(|(i, a)| if i < kept { a.clone() } else { "<redacted>".to_string() }).collect()
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
        return Outcome::Help { path, errors, requested: false };
    }

    let present = |name: &str| scan.flags.get(name).is_some_and(|v| !v.is_empty());
    // effect 4.1.0: a global flag's own parse error (a bad value, a repeated flag) is shown with the help
    // (`toShowHelp`), like any other parse error.
    let decode = |name: &str| -> Result<V, CliError> {
        let def: &FlagDef = globals.iter().find(|f| f.name == name).unwrap();
        parse::decode_global(def, scan.flags.get(name).map(Vec::as_slice).unwrap_or(&[]))
    };
    // action flags, first present wins: help, version, wizard, completions
    for action in ["help", "version", "wizard", "completions"] {
        if !present(action) {
            continue;
        }
        let value = match decode(action) {
            Ok(v) => v,
            Err(e) => return Outcome::Help { path, errors: vec![e], requested: false },
        };
        return match action {
            "help" => Outcome::Help { path, errors: vec![], requested: true },
            "version" => Outcome::Version,
            "wizard" => Outcome::Wizard { path },
            _ => match value {
                V::Some(v) => match *v {
                    V::Str(s) => Outcome::Completions(match s.as_str() {
                        "zsh" => COMPLETIONS_ZSH,
                        "fish" => COMPLETIONS_FISH,
                        "powershell" | "pwsh" => COMPLETIONS_POWERSHELL,
                        _ => COMPLETIONS_BASH,
                    }),
                    _ => Outcome::Fatal("Error: invalid completions value".into()),
                },
                _ => Outcome::Fatal("Error: invalid completions value".into()),
            },
        };
    }

    if !parsed.errors.is_empty() {
        return Outcome::Help { path, errors: parsed.errors.clone(), requested: false };
    }
    let mut command = root;
    for name in path.iter().skip(1) {
        command = command.subcommands.iter().find(|c| &c.name == name).expect("parsed subcommand exists");
    }
    let leaf = parsed.leaf();
    let input = match parse::decode(command, leaf) {
        Ok(i) => i,
        Err(e) => return Outcome::Help { path, errors: vec![e], requested: false },
    };
    // A group without a `$` handler and no subcommand: ShowHelp without errors.
    if !command.handler {
        return Outcome::Help { path, errors: vec![], requested: false };
    }

    let setting = decode;
    let log_level = match setting("log-level") {
        Ok(V::Some(v)) => match *v {
            V::Str(s) => Some(s),
            _ => None,
        },
        Ok(_) => None,
        Err(e) => return Outcome::Help { path, errors: vec![e], requested: false },
    };
    let print_logs = match setting("print-logs") {
        Ok(V::Bool(b)) => b,
        Ok(_) => false,
        Err(e) => return Outcome::Help { path, errors: vec![e], requested: false },
    };
    // The server settings the command declares (`Effect.serviceOption(Server|Standalone)`).
    let mut settings = Settings::default();
    // In declaration order (`withGlobalFlags([Standalone, Server])`): the first failing one is reported.
    if active.iter().any(|f| f.name == "standalone") {
        match setting("standalone") {
            Ok(V::Bool(b)) => settings.standalone = Some(b),
            Ok(_) => {}
            Err(e) => return Outcome::Help { path, errors: vec![e], requested: false },
        }
    }
    if active.iter().any(|f| f.name == "server") {
        match setting("server") {
            Ok(V::Some(v)) => {
                if let V::Str(s) = *v {
                    settings.server = Some(s);
                }
            }
            Ok(_) => {}
            Err(e) => return Outcome::Help { path, errors: vec![e], requested: false },
        }
    }
    let name = path[1..].join(" ");
    Outcome::Run(Invocation { path, name, input, log_level, print_logs, settings })
}

/// The `--wizard` action (`Command.runWith`): the wizard on the terminal, then the built command line
/// resolved like a fresh `runWith(command, { renderErrors: false })`, whose help (a parse error or a group)
/// is printed once before the caller emits it again with its errors. `Err(code)`: nothing to run.
pub fn run_wizard(root: &Cmd, argv: &[String], path: &[String]) -> Result<Outcome, i32> {
    let version = crate::BUILD.unwrap_or(oc_util::version::VERSION);
    let mut term = prompt::TtyTerminal::new();
    let end = wizard::run(root, argv, path, version, prompt::ansi::detect_colors(), &mut term);
    term.release();
    match end {
        wizard::End::Run(args) => {
            let outcome = resolve(root, &args);
            if let Outcome::Help { path, .. } = &outcome {
                crate::term::write_stdout(&format!("{}\n", help::render(root, path, &path_globals(root, path), colors())));
            }
            Ok(outcome)
        }
        wizard::End::Declined | wizard::End::Cancelled => Err(0),
        wizard::End::Failed(e) => {
            crate::term::write_stderr(&format!("{}\n", pretty_cli(&e)));
            Err(1)
        }
    }
}

/// `Cause.pretty` first line for a CLI error that escaped the ShowHelp channel.
fn pretty_cli(e: &CliError) -> String {
    format!("~effect/cli/CliError/{}: {}", e.tag(), e.message())
}

/// Help colors (`TerminalCapabilities.canColor`, effect 4.1.0): never with `TERM=dumb` or a non-empty
/// `NO_COLOR`; otherwise when `FORCE_COLOR` is set to anything but `0`/`false`, or stdout is a TTY.
pub fn colors() -> help::Colors {
    let var = |k: &str| std::env::var(k).ok();
    help::Colors { on: can_color(var("TERM").as_deref(), var("NO_COLOR").as_deref(), var("FORCE_COLOR").as_deref(), crate::term::stdout_is_tty()) }
}

/// Whether Bun colors `console.error` (as the 2.2.2 binary does): a `FORCE_COLOR` other than `0` forces it, even with
/// `NO_COLOR` or `TERM=dumb`; otherwise a non-empty `NO_COLOR` turns it off, else it follows stderr being a TTY.
fn bun_stderr_colors() -> bool {
    use std::io::IsTerminal;
    match std::env::var("FORCE_COLOR").ok().filter(|v| !v.is_empty()) {
        Some(v) => v != "0",
        None => std::env::var("NO_COLOR").map(|v| v.is_empty()).unwrap_or(true) && std::io::stderr().is_terminal(),
    }
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
        Outcome::Help { path, errors, .. } => {
            let text = help::render(root, path, &path_globals(root, path), c);
            let mut out = std::io::stdout().lock();
            let _ = writeln!(out, "{text}");
            let _ = out.flush();
            if errors.is_empty() {
                return 0;
            }
            let list: Vec<(&str, String)> = errors.iter().map(|e| (e.tag(), e.message())).collect();
            let text = help::errors(&list, c);
            // `Console.error` under Bun prints in red when its stderr colors are on.
            if bun_stderr_colors() {
                eprintln!("\x1b[0m\x1b[31m{text}\x1b[0m");
            } else {
                eprintln!("{text}");
            }
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
        // `main` runs the wizard (`run_wizard`) and emits what it resolves to.
        Outcome::Wizard { .. } => 0,
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

    // `service.test.ts`: "service set keeps the value out of the CLI log"
    #[test]
    fn service_set_values_stay_out_of_the_log() {
        let strings = |a: &[&str]| a.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        for (args, logged) in [
            (vec!["service", "set", "password", "not-in-log"], vec!["service", "set", "password", "<redacted>"]),
            (vec!["service", "set", "env", "PROVIDER_KEY", "not-in-log"], vec!["service", "set", "env", "PROVIDER_KEY", "<redacted>"]),
            (
                vec!["--server", "http://127.0.0.1:9", "service", "set", "password", "not-in-log"],
                vec!["--server", "http://127.0.0.1:9", "service", "set", "password", "<redacted>"],
            ),
            (vec!["service", "set", "password", "a", "b"], vec!["service", "set", "password", "<redacted>", "<redacted>"]),
            (vec!["service", "get", "password"], vec!["service", "get", "password"]),
            (vec!["run", "service", "set"], vec!["run", "service", "set"]),
        ] {
            assert_eq!(logged_args(&strings(&args)), strings(&logged), "{args:?}");
        }
    }

    #[test]
    fn help_outcomes_fail_unless_requested() {
        assert!(!run(&["--help"]).fails());
        assert!(!run(&["--version"]).fails());
        assert!(run(&["auth"]).fails());
        assert!(run(&["--nope"]).fails());
        assert!(!run(&["models"]).fails());
    }

    // `login-alias.test.ts`
    #[test]
    fn login_is_hidden_from_root_help_but_parses_like_auth_login() {
        let root = spec::root();
        let plain = help::Colors { on: false };
        let render = |path: &[&str]| {
            let path: Vec<String> = path.iter().map(|s| s.to_string()).collect();
            help::render(&root, &path, &path_globals(&root, &path), plain)
        };
        let top = render(&["ocd"]);
        assert!(top.lines().any(|l| l.trim_start().starts_with("auth ")), "{top}");
        assert!(!top.lines().any(|l| l.trim_start().starts_with("login ")), "{top}");
        let (login, auth) = (render(&["ocd", "login"]), render(&["ocd", "auth", "login"]));
        assert!(login.contains("ocd login [flags] [<target>]"), "{login}");
        let sections = |h: &str| h[h.find("ARGUMENTS").unwrap()..].to_string();
        assert_eq!(sections(&login), sections(&auth));

        let url = "http://127.0.0.1:4096";
        let args = ["openai", "--method", "api", "--answer", "key=value", "--server", url];
        let parse = |prefix: &[&str]| match run(&[prefix, &args[..]].concat()) {
            Outcome::Run(inv) => inv,
            other => panic!("{other:?}"),
        };
        let (login, auth) = (parse(&["login"]), parse(&["auth", "login"]));
        assert_eq!((login.name.as_str(), auth.name.as_str()), ("login", "auth login"));
        assert_eq!(login.input.dump(), auth.input.dump());
        assert_eq!(login.settings, Settings { server: Some(url.into()), standalone: Some(false) });
        assert_eq!(login.settings, auth.settings);
    }

    #[test]
    fn hidden_commands_stay_out_of_typo_suggestions() {
        let mut root = spec::root();
        // The root takes a directory argument, so a typo there is a directory: check under a group.
        let auth = root.subcommands.iter_mut().find(|c| c.name == "auth").unwrap();
        auth.subcommands.iter_mut().find(|c| c.name == "logout").unwrap().hidden = true;
        let args: Vec<String> = ["auth", "logot"].iter().map(|s| s.to_string()).collect();
        match resolve(&root, &args) {
            Outcome::Help { errors, .. } => match &errors[0] {
                CliError::UnknownSubcommand { suggestions, .. } => {
                    assert!(!suggestions.iter().any(|s| s == "logout"), "{suggestions:?}")
                }
                other => panic!("{other:?}"),
            },
            other => panic!("{other:?}"),
        }
        let auth = root.subcommands.iter().find(|c| c.name == "auth").unwrap();
        let path = vec!["ocd".to_string(), "auth".to_string()];
        let text = help::render(&root, &path, &path_globals(&root, &path), help::Colors { on: false });
        assert!(!text.contains("logout"), "{text}");
        assert!(auth.find("logout").is_some());
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
            // machine.test.ts "accept the global --server flag in any position"
            vec!["machine", "list", "--server", url],
            vec!["machine", "--server", url, "add", "gpu"],
            vec!["--server", url, "machine", "remove", "gpu"],
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

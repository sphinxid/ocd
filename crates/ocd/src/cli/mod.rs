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

/// `Command.runWith(...)(args)`, up to (not including) running the handler.
pub fn resolve(root: &Cmd, args: &[String]) -> Outcome {
    let globals = spec::global_flags();
    let (tokens, trailing) = parse::lex(args);
    let scan = parse::consume_global_flags(&tokens, root, &globals);
    let parsed = parse::parse_args(&scan.remainder, &trailing, root, &[]);
    let mut path = vec![root.name.to_string()];
    path.extend(parsed.command_path());

    if !scan.errors.is_empty() {
        let mut errors = scan.errors.clone();
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
    let name = path[1..].join(" ");
    Outcome::Run(Invocation { path, name, input, log_level, print_logs })
}

/// `Cause.pretty` first line for a CLI error that escaped the ShowHelp channel.
fn pretty_cli(e: &CliError) -> String {
    format!("~effect/cli/CliError/{}: {}", e.tag(), e.message())
}

/// Help colors follow stdout (`stdout.isTTY && !NO_COLOR`).
pub fn colors() -> help::Colors {
    let no_color = std::env::var("NO_COLOR").map(|v| !v.is_empty()).unwrap_or(false);
    help::Colors { on: crate::term::stdout_is_tty() && !no_color }
}

/// Print an outcome that does not run a handler; returns the exit code.
pub fn emit(root: &Cmd, outcome: &Outcome) -> i32 {
    use std::io::Write;
    let c = colors();
    match outcome {
        Outcome::Help { path, errors } => {
            let text = help::render(root, path, &spec::global_flags(), c);
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

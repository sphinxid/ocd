//! `--wizard`: effect 4.0.6 `cli/internal/wizard.ts` and the wizard branch of `Command.runWith`. The
//! wizard walks the command tree from the command the line names: a `Command` select for each level
//! with subcommands, then the leaf's flags and arguments (`Set <label>?` for optional ones, a count for
//! repeated ones, then one prompt per value by type: confirm for booleans, select for choices, an
//! integer prompt, text otherwise), each validated like the parser would. It echoes the built command
//! line, asks `Run this command?` and hands the line back to be run.

use super::parse::{self, CliError};
use super::prompt::{self, Choice, Confirm, Number, Quit, Select, Terminal, Text, TextKind, Theme, Toggle, ansi, js_len};
use super::spec::{ArgDef, ArgMode, Arity, Cmd, FlagDef, Param, Prim};

/// How the wizard ended.
#[derive(Debug, PartialEq)]
pub enum End {
    /// Run the built command line (without the program name).
    Run(Vec<String>),
    /// `Run this command?` answered no.
    Declined,
    /// A prompt was quit (escape, ctrl+c, ctrl+d, end of input): `Wizard cancelled.` was printed.
    Cancelled,
    /// A value failed the parameter's own parsing (`param.parse`).
    Failed(CliError),
}

enum Stop {
    Quit,
    Failed(CliError),
}

impl From<Quit> for Stop {
    fn from(_: Quit) -> Stop {
        Stop::Quit
    }
}

/// One command-line word and how the wizard shows it (`<redacted>` for secrets).
#[derive(Clone)]
struct Word {
    value: String,
    display: String,
}

fn word(value: &str) -> Word {
    Word { value: value.into(), display: value.into() }
}

struct Wizard<'t> {
    term: &'t mut dyn Terminal,
    theme: Theme,
    line: Vec<Word>,
}

/// The `--wizard` action: the introduction, the wizard from the command at `path`, the completed command
/// line and the run confirmation. `argv` is the original command line; without its `--wizard` words it
/// starts the built line.
pub fn run(root: &Cmd, argv: &[String], path: &[String], version: &str, colors: bool, term: &mut dyn Terminal) -> End {
    term.log(&render_introduction(root.name, version, Some(root.description), colors));
    let mut line = vec![word(root.name)];
    line.extend(argv.iter().filter(|a| *a != "--wizard" && !a.starts_with("--wizard=")).map(|a| word(a)));
    let mut w = Wizard { term, theme: Theme::new(colors), line };
    match w.steps(root, path) {
        Ok(true) => End::Run(w.line[1..].iter().map(|a| a.value.clone()).collect()),
        Ok(false) => End::Declined,
        Err(Stop::Failed(e)) => End::Failed(e),
        Err(Stop::Quit) => {
            w.term.log(&render_quit(colors));
            End::Cancelled
        }
    }
}

/// `getCommandAtPath`.
fn command_at_path<'a>(root: &'a Cmd, path: &[String]) -> &'a Cmd {
    let mut current = root;
    for name in path.iter().skip(1) {
        match current.subcommands.iter().find(|c| c.name == name || c.alias == Some(name.as_str())) {
            Some(child) => current = child,
            None => break,
        }
    }
    current
}

/// `hasWizardSteps`.
fn has_wizard_steps(cmd: &Cmd) -> bool {
    !cmd.subcommands.is_empty() || !cmd.params.is_empty()
}

impl Wizard<'_> {
    fn steps(&mut self, root: &Cmd, path: &[String]) -> Result<bool, Stop> {
        let selected = command_at_path(root, path);
        self.log_current();
        let section = if std::ptr::eq(selected, root) { "ROOT" } else { selected.name };
        self.command(selected, section)?;
        let display: Vec<String> = self.line.iter().map(|a| a.display.clone()).collect();
        self.term.log(&render_completion(&display, self.theme.colors));
        let toggle = Toggle {
            message: "Run this command?".into(),
            initial: true,
            active: "yes".into(),
            inactive: "no".into(),
            theme: self.theme.clone(),
        };
        let run = prompt::run(&toggle, self.term)?;
        if run {
            self.term.log("");
        }
        Ok(run)
    }

    fn log_current(&mut self) {
        let display: Vec<String> = self.line.iter().map(|a| a.display.clone()).collect();
        self.term.log(&render_command_block("Current command", &display, self.theme.colors, ansi::MAGENTA, ansi::WHITE));
    }

    /// `promptCommand`: a command with subcommands has no parameters of its own here (its `contextConfig`
    /// is empty), so it only asks for the subcommand.
    fn command(&mut self, cmd: &Cmd, section: &str) -> Result<(), Stop> {
        let leaf = cmd.subcommands.is_empty();
        let params: &[Param] = if leaf { &cmd.params } else { &[] };
        let flags: Vec<&FlagDef> = params.iter().filter_map(|p| if let Param::Flag(f) = p { Some(f) } else { None }).collect();
        let args: Vec<&ArgDef> = params.iter().filter_map(|p| if let Param::Arg(a) = p { Some(a) } else { None }).collect();
        if !flags.is_empty() {
            self.term.log(&render_section(section, "FLAGS", self.theme.colors));
            for f in &flags {
                let words = self.param(Single::Flag(f))?;
                self.line.extend(words);
            }
            if !args.is_empty() || !leaf {
                self.log_current();
            }
        }
        if !args.is_empty() {
            self.term.log(&render_section(section, "ARGUMENTS", self.theme.colors));
            for a in &args {
                let words = self.param(Single::Arg(a))?;
                self.line.extend(words);
            }
            if !leaf {
                self.log_current();
            }
        }
        if leaf {
            return Ok(());
        }
        let choices = cmd
            .subcommands
            .iter()
            .map(|c| Choice { title: c.name.into(), description: Some(c.description.into()), disabled: false })
            .collect();
        let index = prompt::run(&Select::new(Some("Command"), choices, &self.theme), self.term)?;
        let child = &cmd.subcommands[index];
        self.term.log("");
        self.line.push(word(child.name));
        if has_wizard_steps(child) {
            self.log_current();
        }
        self.command(child, child.name)
    }

    /// `promptParam`.
    fn param(&mut self, single: Single) -> Result<Vec<Word>, Stop> {
        let meta = single.metadata();
        let label = single.label();
        if meta.optional {
            let include = prompt::run(&Confirm::new(&format!("Set {label}?"), false, &self.theme), self.term)?;
            if !include {
                self.term.log("");
                return Ok(vec![]);
            }
        }
        let count = if !meta.variadic {
            1
        } else {
            let mut count = Number::int(&format!("{label} count"), &self.theme);
            let min = meta.min.unwrap_or(0) as f64;
            count.default = Some(min);
            count.min = min;
            if let Some(max) = meta.max {
                count.max = max as f64;
            }
            prompt::run(&count, self.term)? as usize
        };
        let mut values = vec![];
        for _ in 0..count {
            values.push(self.single(single.prim(), &label)?);
        }
        let raw: Vec<String> = values.iter().map(|v| v.value.clone()).collect();
        let parsed = match single {
            Single::Flag(f) => parse::parse_flag(f, &raw),
            Single::Arg(a) => parse::parse_arg(a, &mut raw.clone()),
        };
        parsed.map_err(Stop::Failed)?;
        self.term.log("");
        let Single::Flag(f) = single else { return Ok(values) };
        // Option-looking values are inlined so the lexer cannot take them for flags or `--`.
        Ok(values
            .into_iter()
            .flat_map(|v| {
                if v.value.starts_with('-') && js_len(&v.value) > 1 {
                    vec![Word { value: format!("--{}={}", f.name, v.value), display: format!("--{}={}", f.name, v.display) }]
                } else {
                    vec![word(&format!("--{}", f.name)), v]
                }
            })
            .collect())
    }

    /// `promptSingle`: by primitive type.
    fn single(&mut self, prim: Prim, message: &str) -> Result<Word, Stop> {
        Ok(match prim {
            Prim::Bool => {
                let mut confirm = Confirm::new(message, false, &self.theme);
                confirm.label_confirm = "true".into();
                confirm.label_deny = "false".into();
                confirm.placeholder_confirm = "(T/f)".into();
                confirm.placeholder_deny = "(t/F)".into();
                word(if prompt::run(&confirm, self.term)? { "true" } else { "false" })
            }
            Prim::Choice(choices) => {
                let options = choices.iter().map(|c| Choice { title: (*c).into(), description: None, disabled: false }).collect();
                word(choices[prompt::run(&Select::new(Some(message), options, &self.theme), self.term)?])
            }
            Prim::Int(_) => word(&prompt::js_number(prompt::run(&Number::int(message, &self.theme), self.term)?)),
            Prim::Str | Prim::NonEmpty | Prim::KeyValue | Prim::PairUrl => {
                word(&prompt::run(&Text::new(message, TextKind::Text, &self.theme), self.term)?)
            }
        })
    }
}

/// The parameter a wizard step asks for.
#[derive(Clone, Copy)]
enum Single<'a> {
    Flag(&'a FlagDef),
    Arg(&'a ArgDef),
}

/// `Param.getParamMetadata`.
struct Metadata {
    optional: bool,
    variadic: bool,
    min: Option<usize>,
    max: Option<usize>,
}

impl Single<'_> {
    fn prim(&self) -> Prim {
        match self {
            Single::Flag(f) => f.prim,
            Single::Arg(a) => a.prim,
        }
    }

    fn metadata(&self) -> Metadata {
        let m = |optional, variadic, min, max| Metadata { optional, variadic, min, max };
        match self {
            // `Flag.KeyValuePair` is a variadic (at least one) flag, optional here.
            Single::Flag(f) if f.prim == Prim::KeyValue => m(true, true, Some(1), None),
            // `withDefault` is an optional flag with a fallback.
            Single::Flag(f) => match f.arity {
                Arity::Default(_) | Arity::Optional => m(true, false, None, None),
                // `atMost(n)` / `atLeast(0)`
                Arity::Many { max: Some(max) } => m(false, true, None, Some(max)),
                Arity::Many { max: None } => m(false, true, Some(0), None),
            },
            Single::Arg(a) => match a.mode {
                ArgMode::Required => m(false, false, None, None),
                ArgMode::Optional => m(true, false, None, None),
                ArgMode::Variadic { min, max } => m(false, true, Some(min), max),
            },
        }
    }

    /// `renderParamLabel`: a flag's description when it is short (at most 32 characters), else the
    /// humanized name; flags add `(--name)`.
    fn label(&self) -> String {
        match self {
            Single::Flag(f) => {
                let description = f.description.map(str::trim).filter(|d| js_len(d) <= 32);
                format!("{} (--{})", description.map(String::from).unwrap_or_else(|| humanize(f.name)), f.name)
            }
            Single::Arg(a) => humanize(a.name),
        }
    }
}

/// `humanize`: words split on `-`/`_`, the first capitalized.
fn humanize(name: &str) -> String {
    let words: Vec<&str> = name.split(['-', '_']).filter(|w| !w.is_empty()).collect();
    let Some(first) = words.first() else { return name.to_string() };
    let mut chars = first.chars();
    let head = chars.next().map(|c| c.to_uppercase().collect::<String>()).unwrap_or_default();
    let mut out = vec![format!("{head}{}", chars.as_str())];
    out.extend(words[1..].iter().map(|w| w.to_string()));
    out.join(" ")
}

fn render_section(command: &str, section: &str, colors: bool) -> String {
    format!(
        "{} {} {}",
        ansi::annotate(colors, &command.to_uppercase(), &[ansi::BOLD, ansi::CYAN_BRIGHT]),
        ansi::annotate(colors, "·", &[ansi::BLACK_BRIGHT]),
        ansi::annotate(colors, section, &[ansi::BOLD, ansi::WHITE])
    )
}

/// `renderIntroduction`.
pub fn render_introduction(name: &str, version: &str, summary: Option<&str>, colors: bool) -> String {
    let title = format!(
        "{} {} {}",
        ansi::annotate(colors, name, &[ansi::BOLD, ansi::CYAN_BRIGHT]),
        ansi::annotate(colors, &format!("v{version}"), &[ansi::WHITE]),
        ansi::annotate(colors, "· Command wizard", &[ansi::BOLD, ansi::WHITE])
    );
    let mut lines = vec![title];
    if let Some(summary) = summary.filter(|s| !s.is_empty()) {
        lines.push(summary.to_string());
    }
    lines.push(ansi::annotate(colors, "Build a command interactively. Press Ctrl+C to cancel.", &[ansi::BLACK_BRIGHT]));
    lines.push(String::new());
    lines.join("\n")
}

/// `renderCompletion`.
pub fn render_completion(line: &[String], colors: bool) -> String {
    render_command_block("Command ready", line, colors, ansi::CYAN_BRIGHT, ansi::GREEN)
}

/// `renderQuit`.
pub fn render_quit(colors: bool) -> String {
    format!("\n{}", ansi::annotate(colors, "Wizard cancelled.", &[ansi::RED]))
}

fn render_command_block(label: &str, line: &[String], colors: bool, command_color: &str, label_color: &str) -> String {
    let mut out = vec![ansi::annotate(colors, label, &[ansi::BOLD, label_color])];
    out.extend(wrap_command(line).iter().map(|l| ansi::annotate(colors, l, &[command_color])));
    out.push(String::new());
    out.join("\n")
}

/// `wrapCommand`: `  $ ` and shell-quoted words, wrapped with ` \` before 88 columns.
fn wrap_command(line: &[String]) -> Vec<String> {
    const WIDTH: usize = 88;
    const FIRST: &str = "  $ ";
    const CONTINUATION: &str = "    ";
    let mut lines = vec![];
    let mut current = FIRST.to_string();
    for arg in line.iter().map(|a| format_shell_arg(a)) {
        let separator = if current == FIRST || current == CONTINUATION { "" } else { " " };
        if js_len(&current) + separator.len() + js_len(&arg) > WIDTH && current != FIRST {
            lines.push(format!("{current} \\"));
            current = format!("{CONTINUATION}{arg}");
        } else {
            current.push_str(separator);
            current.push_str(&arg);
        }
    }
    if current != FIRST {
        lines.push(current);
    }
    lines
}

/// `formatShellArg`: plain words as they are, anything else single-quoted.
fn format_shell_arg(arg: &str) -> String {
    let plain = !arg.is_empty() && arg.chars().all(|c| c.is_ascii_alphanumeric() || "_./:@%+=,-".contains(c));
    if plain { arg.to_string() } else { format!("'{}'", arg.replace('\'', "'\"'\"'")) }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::prompt::ScriptTerminal;
    use crate::cli::spec;

    fn s(v: &[&str]) -> Vec<String> {
        v.iter().map(|x| x.to_string()).collect()
    }

    #[test]
    fn labels() {
        assert_eq!(humanize("max-count"), "Max count");
        assert_eq!(humanize("operation | method path"), "Operation | method path");
        assert_eq!(humanize("--"), "--");
        let root = spec::root();
        let run = root.find("run").unwrap();
        let labels: Vec<String> = run
            .params
            .iter()
            .map(|p| match p {
                Param::Flag(f) => Single::Flag(f).label(),
                Param::Arg(a) => Single::Arg(a).label(),
            })
            .collect();
        assert_eq!(
            labels,
            [
                "Message",
                "Continue the last session (--continue)",
                "Session (--session)",
                "Fork (--fork)",
                "Model (--model)",
                "Agent to use (--agent)",
                "Output format (--format)",
                "File to attach to the message (--file)",
                "Session title (--title)",
                "Show thinking blocks (--thinking)",
                "Auto (--auto)",
                "Yolo (--yolo)",
                "Dangerously skip permissions (--dangerously-skip-permissions)",
            ]
        );
    }

    #[test]
    fn command_lines_quote_and_wrap() {
        assert_eq!(format_shell_arg("a/b:c@d%e+f=g,h-i_j.k"), "a/b:c@d%e+f=g,h-i_j.k");
        assert_eq!(format_shell_arg(""), "''");
        assert_eq!(format_shell_arg("it's here"), "'it'\"'\"'s here'");
        let line: Vec<String> = std::iter::once("ocd".to_string()).chain((0..12).map(|i| format!("word{i:02}-xxxxxx"))).collect();
        let wrapped = wrap_command(&line);
        assert_eq!(wrapped.len(), 3);
        assert!(wrapped[0].starts_with("  $ ocd word00-xxxxxx") && wrapped[0].ends_with(" \\"));
        assert!(js_len(&wrapped[0]) <= 90);
        assert!(wrapped[1].starts_with("    word"));
    }

    #[test]
    fn introduction_completion_quit_without_colors() {
        assert_eq!(
            render_introduction("ocd", "2.2.2", Some("ocd command line interface"), false),
            "ocd v2.2.2 · Command wizard\nocd command line interface\nBuild a command interactively. Press Ctrl+C to cancel.\n"
        );
        assert_eq!(render_completion(&s(&["ocd", "models"]), false), "Command ready\n  $ ocd models\n");
        assert_eq!(render_quit(false), "\nWizard cancelled.");
        assert_eq!(render_quit(true), "\n\x1b[31mWizard cancelled.\x1b[0m");
    }

    #[test]
    fn values_failing_the_parameter_parse_end_the_wizard() {
        let root = spec::root();
        let mut term = ScriptTerminal::new(&["y", "-", "1", "\r"], 100);
        let end = run(&root, &s(&["stats", "--wizard"]), &s(&["ocd", "stats"]), "2.2.2", false, &mut term);
        match end {
            End::Failed(e) => assert_eq!(
                e.message(),
                "Invalid value for flag --days: \"-1\". Expected: Schema validation failed: Expected a value greater than or equal to 0"
            ),
            other => panic!("{other:?}"),
        }
    }

    /// Transcripts of upstream 2.2.2's `--wizard` on a terminal (or with piped stdin and stdout), captured with
    /// the same key chunks: colors, cursor and erase sequences, validation messages, cancellation, keys
    /// dropped after a submission, narrow widths and wide characters. The command's own output after
    /// `Run this command?` and Bun's stack trace after an error's first line are cut.
    #[test]
    fn upstream_transcripts() {
        #[derive(serde::Deserialize)]
        struct Case {
            name: String,
            args: Vec<String>,
            keys: Vec<String>,
            columns: usize,
            colors: bool,
            end: serde_json::Value,
            expected: String,
        }
        let cases: Vec<Case> = serde_json::from_str(include_str!("../../tests/golden/wizard.json")).unwrap();
        assert!(cases.len() >= 20);
        let root = spec::root();
        for case in cases {
            let crate::cli::Outcome::Wizard { path } = crate::cli::resolve(&root, &case.args) else {
                panic!("{}: not a wizard", case.name)
            };
            let mut term = ScriptTerminal::new(&case.keys, case.columns);
            let end = run(&root, &case.args, &path, "VERSION", case.colors, &mut term);
            let mut actual = term.out.clone();
            let kind = match &end {
                End::Run(_) => "run",
                End::Declined => "declined",
                End::Cancelled => "cancelled",
                End::Failed(e) => {
                    actual.push_str(&crate::cli::pretty_cli(e));
                    "failed"
                }
            };
            let mut expected = case.expected.clone();
            if let Some(error) = case.end.get("error").and_then(|e| e.as_str()) {
                expected.push_str(error);
            }
            assert_eq!(kind, case.end["kind"], "{}", case.name);
            assert_eq!(actual, expected, "{}", case.name);
        }
    }

    #[test]
    fn the_built_line_runs_as_typed() {
        let root = spec::root();
        let d = "\x1b[B";
        let keys = [d, d, d, d, "\r", d, d, d, "\r", "y", d, "\r", "\r"];
        let mut term = ScriptTerminal::new(&keys, 100);
        assert_eq!(run(&root, &s(&["--wizard"]), &s(&["ocd"]), "2.2.2", false, &mut term), End::Run(s(&["debug", "paths", "home"])));
        let keys = ["y", "-x", "\r", "\r", "\r"];
        let mut term = ScriptTerminal::new(&keys, 100);
        let args = s(&["session", "import", "--wizard=true"]);
        let end = run(&root, &args, &s(&["ocd", "session", "import"]), "2.2.2", false, &mut term);
        assert_eq!(end, End::Run(s(&["session", "import", "--directory=-x", ""])), "{}", term.out);
    }
}

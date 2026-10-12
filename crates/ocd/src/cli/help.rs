//! Help / version / error rendering (`effect/cli` `CliOutput.defaultFormatter`, `internal/help.ts`).

use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

use super::parse::{choice_keys, type_name};
use super::spec::{ArgMode, Cmd, FlagDef};

#[derive(Clone, Copy)]
pub struct Colors {
    pub on: bool,
}

impl Colors {
    fn wrap(&self, code: &str, text: &str) -> String {
        if self.on { format!("\x1b[{code}m{text}\x1b[0m") } else { text.to_string() }
    }
    pub fn bold(&self, t: &str) -> String {
        self.wrap("1", t)
    }
    pub fn dim(&self, t: &str) -> String {
        self.wrap("2", t)
    }
    pub fn cyan(&self, t: &str) -> String {
        self.wrap("36", t)
    }
    pub fn green(&self, t: &str) -> String {
        self.wrap("32", t)
    }
}

pub(crate) fn strip_ansi(s: &str) -> String {
    static RE: std::sync::LazyLock<regex::Regex> = std::sync::LazyLock::new(|| regex::Regex::new("\x1b\\[[0-9;]*m").unwrap());
    RE.replace_all(s, "").into_owned()
}

fn visual_len(s: &str) -> usize {
    strip_ansi(s).graphemes(true).map(|g| UnicodeWidthStr::width(g)).sum()
}

fn pad(s: &str, width: usize) -> String {
    let len = visual_len(s);
    format!("{s}{}", " ".repeat(width.saturating_sub(len)))
}

fn table(rows: &[(String, String)], cap: Option<usize>) -> String {
    let max = rows.iter().map(|(l, _)| visual_len(l)).max().unwrap_or(0) + 4;
    let col = cap.map_or(max, |c| max.min(c));
    rows.iter()
        .map(|(l, r)| format!("  {}{r}", pad(l, col.max(visual_len(l) + 1))))
        .collect::<Vec<_>>()
        .join("\n")
}

fn flag_row(f: &FlagDef, c: Colors) -> (String, String) {
    let mut names = vec![c.green(&format!("--{}", f.name))];
    if let Some(a) = f.alias {
        names.push(c.green(&if a.chars().count() == 1 { format!("-{a}") } else { format!("--{a}") }));
    }
    let ty = f.metavar.unwrap_or(type_name(f.prim));
    let type_part = if ty != "boolean" { format!(" {}", c.dim(ty)) } else { String::new() };
    let description = match (f.description, choice_keys(f.prim)) {
        (d, Some(keys)) if !keys.is_empty() => {
            let suffix = format!("(choices: {})", keys.join(", "));
            match d {
                Some(d) => format!("{d} {suffix}"),
                None => suffix,
            }
        }
        (d, _) => d.unwrap_or("").to_string(),
    };
    (names.join(", ") + &type_part, description)
}

/// `formatHelpDoc(getHelpForCommandPath(...))` for the command at `path` (root name first).
pub fn render(root: &Cmd, path: &[String], globals: &[FlagDef], c: Colors) -> String {
    let mut cmd = root;
    for name in path.iter().skip(1) {
        match cmd.subcommands.iter().find(|s| s.name == name) {
            Some(s) => cmd = s,
            None => break,
        }
    }
    let mut out: Vec<String> = vec![];
    if !cmd.description.is_empty() {
        out.push(c.bold("DESCRIPTION"));
        out.push(format!("  {}", cmd.description));
        out.push(String::new());
    }
    let mut usage = if path.is_empty() { cmd.name.to_string() } else { path.join(" ") };
    if !cmd.subcommands.is_empty() {
        usage.push_str(" <subcommand>");
    }
    usage.push_str(" [flags]");
    let args: Vec<_> = cmd.args().collect();
    for a in &args {
        let variadic = matches!(a.mode, ArgMode::Variadic { .. });
        let name = if variadic { format!("<{}...>", a.name) } else { format!("<{}>", a.name) };
        if required(a.mode) {
            usage.push_str(&format!(" {name}"));
        } else {
            usage.push_str(&format!(" [{name}]"));
        }
    }
    out.push(c.bold("USAGE"));
    out.push(format!("  {}", c.cyan(&usage)));
    out.push(String::new());
    if !args.is_empty() {
        out.push(c.bold("ARGUMENTS"));
        let rows: Vec<(String, String)> = args
            .iter()
            .map(|a| {
                let mut name = a.name.to_string();
                if matches!(a.mode, ArgMode::Variadic { .. }) {
                    name.push_str("...");
                }
                let left = format!("{} {}", c.green(&name), c.dim(type_name(a.prim)));
                let suffix = if required(a.mode) { String::new() } else { c.dim(" (optional)") };
                (left, format!("{}{suffix}", a.description))
            })
            .collect();
        out.push(table(&rows, Some(25)));
        out.push(String::new());
    }
    let flags: Vec<_> = cmd.flags().filter(|f| !f.hidden).collect();
    if !flags.is_empty() {
        out.push(c.bold("FLAGS"));
        out.push(table(&flags.iter().map(|f| flag_row(f, c)).collect::<Vec<_>>(), None));
        out.push(String::new());
    }
    let globals: Vec<_> = globals.iter().filter(|f| !f.hidden).collect();
    if !globals.is_empty() {
        out.push(c.bold("GLOBAL FLAGS"));
        out.push(table(&globals.iter().map(|f| flag_row(f, c)).collect::<Vec<_>>(), None));
        out.push(String::new());
    }
    if !cmd.subcommands.is_empty() {
        out.push(c.bold("SUBCOMMANDS"));
        let rows: Vec<(String, String)> = cmd
            .subcommands
            .iter()
            .map(|s| {
                let name = match s.alias {
                    Some(a) => format!("{}, {a}", s.name),
                    None => s.name.to_string(),
                };
                (c.cyan(&name), s.description.to_string())
            })
            .collect();
        out.push(table(&rows, Some(20)));
    }
    if out.last().is_some_and(|l| l.is_empty()) {
        out.pop();
    }
    out.join("\n")
}

fn required(mode: ArgMode) -> bool {
    match mode {
        ArgMode::Required => true,
        ArgMode::Optional => false,
        ArgMode::Variadic { min, .. } => min > 0,
    }
}

fn escape_control(text: &str) -> String {
    text.chars()
        .map(|ch| {
            let n = ch as u32;
            if (n <= 0x08) || n == 0x0b || n == 0x0c || (0x0e..=0x1f).contains(&n) || n == 0x7f {
                format!("\\x{n:02x}")
            } else {
                ch.to_string()
            }
        })
        .collect()
}

/// `formatErrors(errors)` (written to stderr with a trailing newline by `Console.error`).
pub fn errors(errors: &[(&str, String)], c: Colors) -> String {
    let (reset, red, bold) = if c.on { ("\x1b[0m", "\x1b[31m", "\x1b[1m") } else { ("", "", "") };
    match errors.len() {
        0 => String::new(),
        1 => format!("\n{bold}{red}ERROR{reset}\n  {}{reset}", escape_control(&errors[0].1)),
        _ => {
            // grouped by `_tag`, groups in first-seen order
            let mut groups: Vec<(&str, Vec<&String>)> = vec![];
            for (tag, m) in errors {
                match groups.iter_mut().find(|(t, _)| t == tag) {
                    Some((_, g)) => g.push(m),
                    None => groups.push((tag, vec![m])),
                }
            }
            let mut s = vec![format!("\n{bold}{red}ERRORS{reset}")];
            for (_, g) in groups {
                for m in g {
                    s.push(format!("  {}{reset}", escape_control(m)));
                }
            }
            s.join("\n")
        }
    }
}

/// `formatVersion(name, version)`.
pub fn version(name: &str, version: &str, c: Colors) -> String {
    format!("{} {}{}", c.bold(name), c.dim("v"), c.bold(version))
}

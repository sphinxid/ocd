//! Formatters (`core/src/formatter.ts`, `core/src/formatter/builtins.ts`,
//! `core/src/config/plugin/formatter.ts`).

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use parking_lot::Mutex;

use crate::host::ToolHost;
use crate::util;

/// One configured formatter override (`formatter.<name>`).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FormatterEntry {
    pub disabled: bool,
    pub command: Option<Vec<String>>,
    pub environment: Vec<(String, String)>,
    pub extensions: Option<Vec<String>>,
}

/// Resolved `formatter` config (documents already merged by core: a boolean resets,
/// objects merge per name).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum FormatterConfig {
    /// `undefined` / `false`: formatting is off.
    #[default]
    Disabled,
    /// `true` (empty overrides) or an object of overrides in key order.
    Enabled(Vec<(String, FormatterEntry)>),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Detect {
    /// `which(name)` → `[bin, ...args]`.
    Executable,
    Prettier,
    Oxfmt,
    Biome,
    Clang,
    Ruff,
    Air,
    Uv,
    Ocamlformat,
    Pint,
    /// Configured command.
    Fixed,
}

#[derive(Debug, Clone)]
struct Info {
    name: String,
    environment: Vec<(String, String)>,
    extensions: Vec<String>,
    detect: Detect,
    args: Vec<String>,
    command: Option<Vec<String>>,
}

const JS_EXTS: &[&str] = &[".js", ".jsx", ".mjs", ".cjs", ".ts", ".tsx", ".mts", ".cts"];
const WEB_EXTS: &[&str] = &[
    ".js", ".jsx", ".mjs", ".cjs", ".ts", ".tsx", ".mts", ".cts", ".html", ".htm", ".css", ".scss", ".sass", ".less", ".vue",
    ".svelte", ".json", ".jsonc", ".yaml", ".yml", ".toml", ".xml", ".md", ".mdx", ".graphql", ".gql",
];

fn strings(items: &[&str]) -> Vec<String> {
    items.iter().map(|s| s.to_string()).collect()
}

fn exe(name: &str, extensions: &[&str], args: &[&str]) -> Info {
    Info {
        name: name.into(),
        environment: Vec::new(),
        extensions: strings(extensions),
        detect: Detect::Executable,
        args: strings(args),
        command: None,
    }
}

fn special(name: &str, extensions: &[&str], detect: Detect, bun: bool) -> Info {
    Info {
        name: name.into(),
        environment: if bun { vec![("BUN_BE_BUN".into(), "1".into())] } else { Vec::new() },
        extensions: strings(extensions),
        detect,
        args: Vec::new(),
        command: None,
    }
}

/// The 26 built-ins in upstream order.
fn builtins() -> Vec<Info> {
    vec![
        exe("gofmt", &[".go"], &["-w", "$FILE"]),
        exe("mix", &[".ex", ".exs", ".eex", ".heex", ".leex", ".neex", ".sface"], &["format", "$FILE"]),
        special("oxfmt", JS_EXTS, Detect::Oxfmt, true),
        special("prettier", WEB_EXTS, Detect::Prettier, true),
        special("biome", WEB_EXTS, Detect::Biome, true),
        exe("zig", &[".zig", ".zon"], &["fmt", "$FILE"]),
        special(
            "clang-format",
            &[".c", ".cc", ".cpp", ".cxx", ".c++", ".h", ".hh", ".hpp", ".hxx", ".h++", ".ino", ".C", ".H"],
            Detect::Clang,
            false,
        ),
        exe("ktlint", &[".kt", ".kts"], &["-F", "$FILE"]),
        special("ruff", &[".py", ".pyi"], Detect::Ruff, false),
        special("air", &[".R"], Detect::Air, false),
        special("uv", &[".py", ".pyi"], Detect::Uv, false),
        exe("rubocop", &[".rb", ".rake", ".gemspec", ".ru"], &["--autocorrect", "$FILE"]),
        exe("standardrb", &[".rb", ".rake", ".gemspec", ".ru"], &["--fix", "$FILE"]),
        exe("htmlbeautifier", &[".erb", ".html.erb"], &["$FILE"]),
        exe("dart", &[".dart"], &["format", "$FILE"]),
        special("ocamlformat", &[".ml", ".mli"], Detect::Ocamlformat, false),
        exe("terraform", &[".tf", ".tfvars"], &["fmt", "$FILE"]),
        exe("latexindent", &[".tex"], &["-w", "-s", "$FILE"]),
        exe("gleam", &[".gleam"], &["format", "$FILE"]),
        exe("shfmt", &[".sh", ".bash"], &["-w", "$FILE"]),
        exe("nixfmt", &[".nix"], &["$FILE"]),
        exe("rustfmt", &[".rs"], &["$FILE"]),
        special("pint", &[".php"], Detect::Pint, false),
        exe("ormolu", &[".hs"], &["-i", "$FILE"]),
        exe("cljfmt", &[".clj", ".cljs", ".cljc", ".edn"], &["fix", "--quiet", "$FILE"]),
        exe("dfmt", &[".d"], &["-i", "$FILE"]),
    ]
}

/// Names of the built-in formatters in order.
pub fn builtin_names() -> Vec<String> {
    builtins().into_iter().map(|i| i.name).collect()
}

fn configured(config: &FormatterConfig) -> Vec<Info> {
    let FormatterConfig::Enabled(overrides) = config else { return Vec::new() };
    let builtins = builtins();
    let mut list = builtins.clone();
    let set = |list: &mut Vec<Info>, info: Info| match list.iter().position(|i| i.name == info.name) {
        Some(index) => list[index] = info,
        None => list.push(info),
    };
    for (name, entry) in overrides {
        if entry.disabled {
            list.retain(|i| &i.name != name);
            continue;
        }
        let builtin = builtins.iter().find(|i| &i.name == name);
        let mut environment = builtin.map(|b| b.environment.clone()).unwrap_or_default();
        for (k, v) in &entry.environment {
            match environment.iter_mut().find(|(key, _)| key == k) {
                Some(slot) => slot.1 = v.clone(),
                None => environment.push((k.clone(), v.clone())),
            }
        }
        let info = match (builtin, &entry.command) {
            (Some(b), None) => Info {
                name: name.clone(),
                environment,
                extensions: entry.extensions.clone().unwrap_or_else(|| b.extensions.clone()),
                ..b.clone()
            },
            (_, command) => Info {
                name: name.clone(),
                environment,
                extensions: entry.extensions.clone().or_else(|| builtin.map(|b| b.extensions.clone())).unwrap_or_default(),
                detect: Detect::Fixed,
                args: Vec::new(),
                command: command.clone(),
            },
        };
        set(&mut list, info);
    }
    list
}

/// `path.extname`.
pub fn extname(path: &Path) -> String {
    let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    match name.rfind('.') {
        Some(0) | None => String::new(),
        Some(index) => name[index..].to_string(),
    }
}

/// Location-scoped formatter runner with the enabled-command cache.
pub struct Formatter {
    cache: Mutex<HashMap<(PathBuf, String), Vec<String>>>,
}

impl Default for Formatter {
    fn default() -> Self {
        Self::new()
    }
}

/// Where formatters run.
pub struct FormatEnv<'a> {
    pub directory: &'a Path,
    pub worktree: &'a Path,
    pub bin: &'a Path,
    pub config: &'a FormatterConfig,
    pub host: &'a Arc<dyn ToolHost>,
}

fn find_up(target: &str, start: &Path, stop: &Path) -> Vec<PathBuf> {
    let mut result = Vec::new();
    let mut current = start.to_path_buf();
    loop {
        let search = current.join(target);
        if search.exists() {
            result.push(search);
        }
        if current == stop {
            break;
        }
        match current.parent() {
            Some(parent) if parent != current => current = parent.to_path_buf(),
            _ => break,
        }
    }
    result
}

fn has_record_key(json: &serde_json::Value, field: &str, key: &str) -> bool {
    json.get(field).and_then(|v| v.as_object()).is_some_and(|o| o.contains_key(key))
}

fn has_dependency(json: &serde_json::Value, dependency: &str) -> bool {
    has_record_key(json, "dependencies", dependency) || has_record_key(json, "devDependencies", dependency)
}

fn read_json(path: &Path) -> Option<serde_json::Value> {
    serde_json::from_str(&std::fs::read_to_string(path).ok()?).ok()
}

async fn command_output(command: &[String], directory: &Path) -> Option<(i32, String)> {
    let output = tokio::process::Command::new(&command[0])
        .args(&command[1..])
        .current_dir(directory)
        .stdin(std::process::Stdio::null())
        .output()
        .await
        .ok()?;
    Some((output.status.code().unwrap_or(-1), String::from_utf8_lossy(&output.stdout).into_owned()))
}

impl Formatter {
    pub fn new() -> Self {
        Self { cache: Mutex::new(HashMap::new()) }
    }

    /// Drop cached commands (call when the config revision changes).
    pub fn reset(&self) {
        self.cache.lock().clear();
    }

    async fn enabled(&self, info: &Info, env: &FormatEnv<'_>) -> Option<Vec<String>> {
        let key = (env.directory.to_path_buf(), info.name.clone());
        if let Some(cached) = self.cache.lock().get(&key) {
            return Some(cached.clone());
        }
        let result = self.detect(info, env).await;
        if let Some(command) = &result {
            self.cache.lock().insert(key, command.clone());
        }
        result
    }

    async fn detect(&self, info: &Info, env: &FormatEnv<'_>) -> Option<Vec<String>> {
        let which = |name: &str| util::which(name, Some(env.bin)).map(|p| p.to_string_lossy().into_owned());
        let up = |target: &str| find_up(target, env.directory, env.worktree);
        let with = |bin: String, args: &[&str]| {
            let mut v = vec![bin];
            v.extend(args.iter().map(|s| s.to_string()));
            v
        };
        let read_text = |p: &Path| std::fs::read_to_string(p).unwrap_or_default();
        match info.detect {
            Detect::Fixed => info.command.clone(),
            Detect::Executable => which(&info.name).map(|bin| {
                let mut v = vec![bin];
                v.extend(info.args.iter().cloned());
                v
            }),
            Detect::Prettier | Detect::Oxfmt => {
                let package = if info.detect == Detect::Prettier { "prettier" } else { "oxfmt" };
                for file in up("package.json") {
                    let Some(json) = read_json(&file) else { return None };
                    if !has_dependency(&json, package) {
                        continue;
                    }
                    if let Some(bin) = env.host.npm_which(package).await {
                        return Some(if info.detect == Detect::Prettier { with(bin, &["--write", "$FILE"]) } else { with(bin, &["$FILE"]) });
                    }
                }
                None
            }
            Detect::Biome => {
                if up("biome.json").is_empty() && up("biome.jsonc").is_empty() {
                    return None;
                }
                env.host.npm_which("@biomejs/biome").await.map(|bin| with(bin, &["format", "--write", "$FILE"]))
            }
            Detect::Clang => {
                if up(".clang-format").is_empty() {
                    return None;
                }
                which("clang-format").map(|bin| with(bin, &["-i", "$FILE"]))
            }
            Detect::Ocamlformat => {
                if up(".ocamlformat").is_empty() {
                    return None;
                }
                which("ocamlformat").map(|bin| with(bin, &["-i", "$FILE"]))
            }
            Detect::Ruff => {
                let bin = which("ruff")?;
                for config in ["pyproject.toml", "ruff.toml", ".ruff.toml"] {
                    let found = up(config);
                    if found.is_empty() {
                        continue;
                    }
                    if config != "pyproject.toml" || read_text(&found[0]).contains("[tool.ruff]") {
                        return Some(with(bin, &["format", "$FILE"]));
                    }
                }
                for dependency in ["requirements.txt", "pyproject.toml", "Pipfile"] {
                    let found = up(dependency);
                    if !found.is_empty() && read_text(&found[0]).contains("ruff") {
                        return Some(with(bin, &["format", "$FILE"]));
                    }
                }
                None
            }
            Detect::Air => {
                let bin = which("air")?;
                let (code, stdout) = command_output(&[bin.clone(), "--help".into()], env.directory).await?;
                if code != 0 {
                    return None;
                }
                let first = stdout.split('\n').next().unwrap_or("");
                (first.contains("R language") && first.contains("formatter")).then(|| with(bin, &["format", "$FILE"]))
            }
            Detect::Uv => {
                let bin = which("uv")?;
                let (code, _) = command_output(&[bin.clone(), "format".into(), "--help".into()], env.directory).await?;
                (code == 0).then(|| with(bin, &["format", "--", "$FILE"]))
            }
            Detect::Pint => {
                for file in up("composer.json") {
                    let json = read_json(&file)?;
                    if has_record_key(&json, "require", "laravel/pint") || has_record_key(&json, "require-dev", "laravel/pint") {
                        return Some(vec!["./vendor/bin/pint".into(), "$FILE".into()]);
                    }
                }
                None
            }
        }
    }

    /// `Formatter.file(path)`: true only if a formatter exited 0.
    pub async fn file(&self, path: &Path, env: &FormatEnv<'_>) -> bool {
        if matches!(env.config, FormatterConfig::Disabled) {
            return false;
        }
        let extension = extname(path);
        let filepath = path.to_string_lossy().into_owned();
        for info in configured(env.config).iter().filter(|i| i.extensions.iter().any(|e| e == &extension)) {
            let Some(enabled) = self.enabled(info, env).await else { continue };
            if enabled.is_empty() {
                continue;
            }
            let command: Vec<String> = enabled.iter().map(|arg| arg.replacen("$FILE", &filepath, 1)).collect();
            tracing::info!(file = %filepath, command = ?command, "formatting file");
            let mut process = tokio::process::Command::new(&command[0]);
            process
                .args(&command[1..])
                .current_dir(env.directory)
                .stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null());
            for (k, v) in &info.environment {
                process.env(k, v);
            }
            match process.status().await {
                Ok(status) if status.success() => return true,
                Ok(status) => {
                    tracing::error!(file = %filepath, command = ?command, exit_code = ?status.code(), "formatter exited unsuccessfully")
                }
                Err(error) => tracing::error!(file = %filepath, command = ?command, error = %error, "failed to format file"),
            }
        }
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builtin_order_and_count() {
        let names = builtin_names();
        assert_eq!(names.len(), 26);
        assert_eq!(names[0], "gofmt");
        assert_eq!(names[3], "prettier");
        assert_eq!(names[25], "dfmt");
    }

    #[test]
    fn extname_semantics() {
        assert_eq!(extname(Path::new("/a/b.html.erb")), ".erb");
        assert_eq!(extname(Path::new("/a/.bashrc")), "");
        assert_eq!(extname(Path::new("/a/Makefile")), "");
        assert_eq!(extname(Path::new("x.R")), ".R");
    }

    #[test]
    fn overrides() {
        let config = FormatterConfig::Enabled(vec![
            ("gofmt".into(), FormatterEntry { disabled: true, ..Default::default() }),
            (
                "custom".into(),
                FormatterEntry { command: Some(vec!["fmt".into(), "$FILE".into()]), extensions: Some(vec![".foo".into()]), ..Default::default() },
            ),
            ("prettier".into(), FormatterEntry { extensions: Some(vec![".x".into()]), ..Default::default() }),
        ]);
        let list = configured(&config);
        assert!(!list.iter().any(|i| i.name == "gofmt"));
        assert_eq!(list.last().unwrap().name, "custom");
        let prettier = list.iter().find(|i| i.name == "prettier").unwrap();
        assert_eq!(prettier.extensions, vec![".x".to_string()]);
        assert_eq!(prettier.detect, Detect::Prettier);
        assert!(configured(&FormatterConfig::Disabled).is_empty());
    }
}

//! Code syntax highlighting via syntect, mapped onto the theme's tree-sitter style scopes
//! (`theme/src/tui/syntax.ts`).

use super::text::Seg;
use crate::theme::{SyntaxStyle, TokenStyle};
use once_cell::sync::Lazy;
use ratatui::style::{Modifier, Style};
use syntect::parsing::{ParseState, ScopeStack, SyntaxReference, SyntaxSet};

static SYNTAXES: Lazy<SyntaxSet> = Lazy::new(SyntaxSet::load_defaults_newlines);

/// `src/util/filetype.ts`: extension → filetype name.
pub fn filetype(path: Option<&str>) -> String {
    let Some(path) = path else { return "none".into() };
    let name = path.rsplit('/').next().unwrap_or(path);
    let ext = match name.rsplit_once('.') {
        Some((_, e)) => e.to_lowercase(),
        None => return name.to_lowercase(),
    };
    match ext.as_str() {
        "ts" | "tsx" | "mts" | "cts" | "js" | "jsx" | "mjs" | "cjs" => "typescript".into(),
        "py" | "pyi" => "python".into(),
        "rs" => "rust".into(),
        "go" => "go".into(),
        "c" | "h" => "c".into(),
        "cc" | "cpp" | "cxx" | "hpp" | "hh" | "hxx" => "cpp".into(),
        "cs" => "csharp".into(),
        "sh" | "bash" | "zsh" => "bash".into(),
        "java" => "java".into(),
        "kt" | "kts" => "kotlin".into(),
        "rb" => "ruby".into(),
        "php" => "php".into(),
        "scala" => "scala".into(),
        "html" | "htm" => "html".into(),
        "vue" => "vue".into(),
        "tf" | "hcl" => "hcl".into(),
        "json" | "jsonc" => "json".into(),
        "yaml" | "yml" => "yaml".into(),
        "hs" => "haskell".into(),
        "css" | "scss" => "css".into(),
        "jl" => "julia".into(),
        "lua" => "lua".into(),
        "ml" | "mli" => "ocaml".into(),
        "clj" | "cljs" => "clojure".into(),
        "swift" => "swift".into(),
        "toml" => "toml".into(),
        "nix" => "nix".into(),
        "diff" | "patch" => "diff".into(),
        "ex" | "exs" => "elixir".into(),
        "fs" | "fsx" => "fsharp".into(),
        "r" => "r".into(),
        "mk" => "make".into(),
        "vim" => "vim".into(),
        "xml" | "svg" => "xml".into(),
        "md" | "markdown" => "markdown".into(),
        other => other.to_string(),
    }
}

fn find_syntax(lang: &str) -> Option<&'static SyntaxReference> {
    let ss = &*SYNTAXES;
    let token = match lang {
        "typescript" | "tsx" | "ts" => "js",
        "javascript" | "jsx" => "js",
        "bash" | "shell" | "sh" | "zsh" => "sh",
        "python" => "py",
        "rust" => "rs",
        "csharp" => "cs",
        "cpp" | "c++" => "cpp",
        "ruby" => "rb",
        "kotlin" => "java",
        "haskell" => "hs",
        "markdown" => "md",
        "yaml" | "yml" => "yaml",
        "make" | "makefile" => "Makefile",
        "ocaml" => "ml",
        "clojure" => "clj",
        "elixir" | "fsharp" | "julia" | "swift" | "nix" | "vue" | "hcl" | "toml" | "vim" => return None,
        "none" | "" | "text" => return None,
        other => other,
    };
    ss.find_syntax_by_token(token).or_else(|| ss.find_syntax_by_extension(token))
}

/// Map a TextMate scope atom string onto a tree-sitter style scope.
fn map_scope(scope: &str) -> Option<&'static str> {
    let s = scope;
    let starts = |p: &str| s == p || s.starts_with(&format!("{p}."));
    Some(if starts("comment") {
        "comment"
    } else if starts("punctuation.definition.comment") {
        "comment"
    } else if starts("string.regexp") {
        "string.regexp"
    } else if starts("constant.character.escape") {
        "string.escape"
    } else if starts("punctuation.definition.string") || starts("string") {
        "string"
    } else if starts("constant.numeric") {
        "number"
    } else if starts("constant.language") {
        "boolean"
    } else if starts("constant") {
        "constant"
    } else if starts("keyword.operator") {
        "operator"
    } else if starts("keyword.control.import") || starts("keyword.control.export") {
        "keyword.import"
    } else if starts("keyword.control.flow.return") || s.contains("return") && s.starts_with("keyword") {
        "keyword.return"
    } else if starts("keyword.control.conditional") || starts("keyword.control.loop") {
        "keyword.conditional"
    } else if starts("keyword") {
        "keyword"
    } else if starts("storage.type.function") {
        "keyword.function"
    } else if starts("storage.type") {
        "keyword"
    } else if starts("storage.modifier") {
        "keyword.modifier"
    } else if starts("storage") {
        "keyword"
    } else if starts("entity.name.function") || starts("support.function") || starts("meta.function-call") {
        "function"
    } else if starts("entity.name.tag") {
        "tag"
    } else if starts("entity.other.attribute-name") {
        "tag.attribute"
    } else if starts("entity.name.type") || starts("entity.name.class") || starts("support.type") || starts("support.class") || starts("entity.other.inherited-class") {
        "type"
    } else if starts("entity.name.namespace") || starts("entity.name.module") {
        "module"
    } else if starts("variable.parameter") {
        "variable.parameter"
    } else if starts("variable.language") {
        "variable.builtin"
    } else if starts("variable.other.member") || starts("variable.other.property") || starts("meta.property") {
        "property"
    } else if starts("variable") {
        "variable"
    } else if starts("markup.heading") {
        "markup.heading"
    } else if starts("markup.bold") {
        "markup.bold"
    } else if starts("markup.italic") {
        "markup.italic"
    } else if starts("markup.raw") {
        "markup.raw"
    } else if starts("markup.quote") {
        "markup.quote"
    } else if starts("markup.list") {
        "markup.list"
    } else if starts("markup.underline.link") || starts("markup.link") {
        "markup.link"
    } else if starts("markup.inserted") {
        "diff.plus"
    } else if starts("markup.deleted") {
        "diff.minus"
    } else if starts("meta.diff.range") || starts("meta.diff.header") {
        "diff.delta"
    } else if starts("punctuation.section") || starts("punctuation.bracket") || s.contains("brace") || s.contains("bracket") {
        "punctuation.bracket"
    } else if starts("punctuation.separator") || starts("punctuation.terminator") || starts("punctuation.accessor") {
        "punctuation.delimiter"
    } else if starts("punctuation") {
        "punctuation"
    } else {
        return None;
    })
}

pub fn token_style(t: TokenStyle) -> Style {
    let mut s = Style::default();
    if let Some(f) = t.fg {
        s = s.fg(f.tui());
    }
    if let Some(b) = t.bg {
        s = s.bg(b.tui());
    }
    if t.bold {
        s = s.add_modifier(Modifier::BOLD);
    }
    if t.italic {
        s = s.add_modifier(Modifier::ITALIC);
    }
    if t.underline {
        s = s.add_modifier(Modifier::UNDERLINED);
    }
    s
}

fn stack_style(stack: &ScopeStack, styles: &SyntaxStyle) -> Style {
    for scope in stack.as_slice().iter().rev() {
        let name = scope.build_string();
        if let Some(mapped) = map_scope(&name) {
            return token_style(styles.get(mapped));
        }
    }
    token_style(styles.default)
}

/// Highlight `code` as `lang`; one segment list per source line.
pub fn highlight(code: &str, lang: &str, styles: &SyntaxStyle) -> Vec<Vec<Seg>> {
    let default = token_style(styles.default);
    let Some(syntax) = find_syntax(lang) else {
        return code.split('\n').map(|l| vec![Seg::new(l.to_string(), default)]).collect();
    };
    let ss = &*SYNTAXES;
    let mut state = ParseState::new(syntax);
    let mut stack = ScopeStack::new();
    let mut out = Vec::new();
    for line in code.split('\n') {
        let with_nl = format!("{line}\n");
        let ops = match state.parse_line(&with_nl, ss) {
            Ok(o) => o,
            Err(_) => {
                out.push(vec![Seg::new(line.to_string(), default)]);
                continue;
            }
        };
        let mut segs: Vec<Seg> = Vec::new();
        let mut last = 0usize;
        let push = |segs: &mut Vec<Seg>, text: &str, style: Style| {
            if text.is_empty() {
                return;
            }
            match segs.last_mut() {
                Some(s) if s.style == style => s.text.push_str(text),
                _ => segs.push(Seg::new(text.to_string(), style)),
            }
        };
        for (pos, op) in ops {
            let pos = pos.min(line.len());
            if pos > last {
                let style = stack_style(&stack, styles);
                push(&mut segs, &line[last..pos], style);
                last = pos;
            }
            let _ = stack.apply(&op);
        }
        if last < line.len() {
            let style = stack_style(&stack, styles);
            push(&mut segs, &line[last..], style);
        }
        out.push(segs);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::theme::{Mode, OPENCODE_V2, resolve};

    #[test]
    fn rust_keywords_colored() {
        let doc: serde_json::Value = serde_json::from_str(OPENCODE_V2).unwrap();
        let t = resolve::resolve_document(&doc, Mode::Dark).unwrap();
        let st = SyntaxStyle::from_theme(&t);
        let lines = highlight("fn main() { let x = \"s\"; }", "rust", &st);
        let kw = lines[0].iter().find(|s| s.text.contains("let")).unwrap();
        assert_eq!(kw.style.fg, Some(t.syntax.keyword.tui()));
        let s = lines[0].iter().find(|s| s.text.contains("\"s")).unwrap();
        assert_eq!(s.style.fg, Some(t.syntax.string.tui()));
        assert_eq!(filetype(Some("src/a.tsx")), "typescript");
        assert_eq!(filetype(None), "none");
    }
}

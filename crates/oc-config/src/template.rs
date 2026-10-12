//! Command template evaluation (`config/plugin/command.ts` `evaluateTemplate`).

use std::future::Future;
use std::sync::LazyLock;

use futures::StreamExt;
use regex::Regex;

static ARGS: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#"(?i)(?:\[Image\s+\d+\]|"[^"]*"|'[^']*'|[^\s"']+)"#).unwrap());
static PLACEHOLDER: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\$(\d+)").unwrap());
static SHELL: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"!`([^`]+)`").unwrap());

/// Shell interpolation concurrency.
pub const SHELL_CONCURRENCY: usize = 2;

/// `parseArguments`: split on the args regex and trim one leading/trailing quote.
pub fn parse_arguments(input: &str) -> Vec<String> {
    ARGS.find_iter(input)
        .map(|m| {
            let s = m.as_str();
            let s = s.strip_prefix(['"', '\'']).unwrap_or(s);
            let s = s.strip_suffix(['"', '\'']).unwrap_or(s);
            s.to_string()
        })
        .collect()
}

fn js_number(digits: &str) -> f64 {
    digits.parse::<f64>().unwrap_or(f64::INFINITY)
}

/// Expand `$1..$N`, `$ARGUMENTS` and the append-if-absent rule (no shell interpolation).
pub fn expand(template: &str, input: &str) -> String {
    let args = parse_arguments(input);
    let placeholders: Vec<f64> = PLACEHOLDER.captures_iter(template).map(|c| js_number(&c[1])).collect();
    let last = placeholders.iter().copied().fold(0.0_f64, f64::max);
    let expanded = PLACEHOLDER.replace_all(template, |c: &regex::Captures| {
        let position = js_number(&c[1]);
        let arg_index = position - 1.0;
        if arg_index >= args.len() as f64 {
            return String::new();
        }
        if position == last {
            // `args.slice(argIndex)` (a negative index counts from the end).
            let start = if arg_index < 0.0 { args.len().saturating_sub(1) } else { arg_index as usize };
            return args[start..].join(" ");
        }
        if arg_index < 0.0 {
            return "undefined".into();
        }
        args[arg_index as usize].clone()
    });
    let with_arguments = expanded.replace("$ARGUMENTS", input);
    let trim = crate::schema::js_trim;
    if placeholders.is_empty() && !template.contains("$ARGUMENTS") && !trim(input).is_empty() {
        trim(&format!("{with_arguments}\n\n{input}")).to_string()
    } else {
        trim(&with_arguments).to_string()
    }
}

/// `` !`cmd` `` sources in the expanded text.
pub fn shell_commands(text: &str) -> Vec<String> {
    SHELL.captures_iter(text).map(|c| c[1].to_string()).collect()
}

/// Verbatim failure message for a failed shell interpolation.
pub fn shell_failure(source: &str, error: &str) -> String {
    format!("Shell interpolation failed for {}: {}", serde_json::to_string(source).unwrap_or_default(), error)
}

/// Evaluate a command template. `run` executes one shell source in the location directory
/// (stdin ignored, combined stdout+stderr) and returns its output or an error message; at most
/// two run concurrently and outputs are spliced in order. On failure returns the verbatim
/// `Shell interpolation failed for "<source>": <message>` error.
pub async fn evaluate<F, Fut>(template: &str, input: &str, run: F) -> Result<String, String>
where
    F: Fn(String) -> Fut,
    Fut: Future<Output = Result<String, String>>,
{
    let text = expand(template, input);
    let sources = shell_commands(&text);
    if sources.is_empty() {
        return Ok(text);
    }
    let outputs: Vec<Result<String, String>> = futures::stream::iter(sources.into_iter().map(|source| {
        let fut = run(source.clone());
        async move { fut.await.map_err(|e| shell_failure(&source, &e)) }
    }))
    .buffered(SHELL_CONCURRENCY)
    .collect()
    .await;
    let outputs = outputs.into_iter().collect::<Result<Vec<_>, _>>()?;
    let mut iter = outputs.into_iter();
    Ok(SHELL.replace_all(&text, |_: &regex::Captures| iter.next().unwrap_or_default()).into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn positional_last_swallows_rest() {
        assert_eq!(expand("Run $1 then $2", "a b c d"), "Run a then b c d");
        assert_eq!(expand("$1-$1", "x y"), "x y-x y");
        assert_eq!(expand("only $3", "a"), "only");
    }

    #[test]
    fn arguments_and_append() {
        assert_eq!(expand("Review $ARGUMENTS now", "the diff"), "Review the diff now");
        assert_eq!(expand("Do it", "extra words"), "Do it\n\nextra words");
        assert_eq!(expand("Do it", "   "), "Do it");
    }

    #[test]
    fn quoted_and_image_args() {
        assert_eq!(parse_arguments(r#"one "two words" 'three' [Image 1] [image 22]"#), vec![
            "one",
            "two words",
            "three",
            "[Image 1]",
            "[image 22]"
        ]);
    }

    #[test]
    fn zero_placeholder_quirks() {
        assert_eq!(expand("$0", "a b"), "b");
        assert_eq!(expand("$0 $1", "a b"), "undefined a b");
    }

    #[tokio::test]
    async fn shell_interpolation_in_order() {
        let out = evaluate("A !`echo 1` B !`echo 2`", "", |s| async move { Ok(format!("<{s}>")) }).await.unwrap();
        assert_eq!(out, "A <echo 1> B <echo 2>");
        let err = evaluate("!`boom`", "", |_| async { Err::<String, _>("exit 1".to_string()) }).await.unwrap_err();
        assert_eq!(err, r#"Shell interpolation failed for "boom": exit 1"#);
    }
}

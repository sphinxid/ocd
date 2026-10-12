//! JSONC parsing equivalent to `jsonc-parser` `parse(text, errors, { allowTrailingComma: true })`
//! where any error rejects the whole document.

use serde_json::Value;

#[derive(Debug, thiserror::Error)]
#[error("invalid JSONC: {0}")]
pub struct JsoncError(pub String);

/// Strip `//` and `/* */` comments (outside strings) and trailing commas, then parse strictly.
pub fn parse(text: &str) -> Result<Value, JsoncError> {
    let text = text.strip_prefix('\u{feff}').unwrap_or(text);
    let stripped = strip(text)?;
    if stripped.trim().is_empty() {
        return Err(JsoncError("empty document".into()));
    }
    serde_json::from_str(&stripped).map_err(|e| JsoncError(e.to_string()))
}

fn strip(text: &str) -> Result<String, JsoncError> {
    let bytes: Vec<char> = text.chars().collect();
    let mut out = String::with_capacity(text.len());
    let mut i = 0;
    while i < bytes.len() {
        let c = bytes[i];
        match c {
            '"' => {
                out.push(c);
                i += 1;
                while i < bytes.len() {
                    let d = bytes[i];
                    out.push(d);
                    i += 1;
                    if d == '\\' {
                        if i < bytes.len() {
                            out.push(bytes[i]);
                            i += 1;
                        }
                    } else if d == '"' {
                        break;
                    }
                }
            }
            '/' if bytes.get(i + 1) == Some(&'/') => {
                while i < bytes.len() && bytes[i] != '\n' {
                    i += 1;
                }
            }
            '/' if bytes.get(i + 1) == Some(&'*') => {
                i += 2;
                loop {
                    if i + 1 >= bytes.len() {
                        return Err(JsoncError("unterminated block comment".into()));
                    }
                    if bytes[i] == '*' && bytes[i + 1] == '/' {
                        i += 2;
                        break;
                    }
                    // keep newlines so serde_json error positions stay meaningful
                    if bytes[i] == '\n' {
                        out.push('\n');
                    }
                    i += 1;
                }
                out.push(' ');
            }
            ',' => {
                // trailing comma: next significant char is } or ]
                let mut j = i + 1;
                loop {
                    while j < bytes.len() && bytes[j].is_whitespace() {
                        j += 1;
                    }
                    if j + 1 < bytes.len() && bytes[j] == '/' && bytes[j + 1] == '/' {
                        while j < bytes.len() && bytes[j] != '\n' {
                            j += 1;
                        }
                        continue;
                    }
                    if j + 1 < bytes.len() && bytes[j] == '/' && bytes[j + 1] == '*' {
                        j += 2;
                        while j + 1 < bytes.len() && !(bytes[j] == '*' && bytes[j + 1] == '/') {
                            j += 1;
                        }
                        j += 2;
                        continue;
                    }
                    break;
                }
                if !matches!(bytes.get(j), Some('}') | Some(']')) {
                    out.push(',');
                }
                i += 1;
            }
            _ => {
                out.push(c);
                i += 1;
            }
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn comments_and_trailing_commas() {
        let v = parse("{\n // c\n \"a\": \"//x\", /* b */ \"b\": [1,2,],\n}").unwrap();
        assert_eq!(v, serde_json::json!({"a": "//x", "b": [1, 2]}));
    }
    #[test]
    fn rejects_garbage() {
        assert!(parse("{a:1}").is_err());
    }
}

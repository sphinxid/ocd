//! The `Intl` the host gives `localeCompare` and `Date.prototype.toLocale*` (2.4.1, #64935, #64938): locales and
//! options read as upstream reads them, validated with Bun 1.4.4's messages, and handed to ocd's collator
//! ([`crate::stdlib::string::collate`]) and date formatter. There is still no global `Intl`.

use crate::interp::coerce::{coerce_to_integer, coerce_to_string, to_primitive, Hint};
use crate::interp::errors::{range_error, type_error, JsResult};
use crate::interp::limits::check_array_length;
use crate::interp::{Rt, Site};
use crate::object::{get, has};
use crate::value::{PropKey, Value};

/// A BCP 47 language tag as the host canonicalizes it, or `None` when it is not one.
pub fn canonical_tag(tag: &str) -> Option<String> {
    if tag.is_empty() || !tag.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-') {
        return None;
    }
    icu_locale_core::Locale::try_from_str(tag).ok().map(|l| l.to_string())
}

/// The locale list of `locales`: `undefined`, a string, or an array-like of strings (length and indexes read through
/// getters), each through the program's `toString` as upstream converts them. `null` is the caller's to refuse.
pub async fn read_locales(rt: &Rt, locales: &Value, site: Site, label: &str) -> JsResult<Option<Vec<Value>>> {
    let list = match locales {
        Value::Undefined => return Ok(None),
        Value::Object(o) => {
            let length = coerce_to_integer(&get(o, &PropKey::str("length"))?).max(0.0);
            check_array_length(length)?;
            let mut out = Vec::new();
            for index in 0..length as usize {
                let key = PropKey::str(&index.to_string());
                // Holes are skipped, as `Array.prototype.filter` and HasProperty skip them.
                if has(o, &key) {
                    out.push(get(o, &key)?);
                }
            }
            out
        }
        // A number or boolean converts to an object without a length: no locales.
        Value::String(_) => vec![locales.clone()],
        _ => Vec::new(),
    };
    let mut values = Vec::with_capacity(list.len());
    for item in list {
        values.push(match &item {
            Value::String(_) => item,
            Value::Object(_) => to_primitive(rt, &item, Hint::String, site).await?,
            _ => {
                return Err(type_error(prefixed(label, "locale value must be a string or object")));
            }
        });
    }
    Ok(Some(values))
}

/// The host's error text, after `label: ` when there is one.
pub fn prefixed(label: &str, message: &str) -> String {
    if label.is_empty() { message.to_string() } else { format!("{label}: {message}") }
}

/// Canonicalized tags, or the RangeError for the first invalid one.
pub fn canonical_tags(values: &[Value], label: &str) -> JsResult<Vec<String>> {
    values
        .iter()
        .map(|v| {
            let text = coerce_to_string(v).to_rust();
            canonical_tag(&text).ok_or_else(|| range_error(prefixed(label, &format!("invalid language tag: {text}"))))
        })
        .collect()
}

/// A string option: one of `allowed`, or the host's RangeError (`x must be either "a", "b", or "c"`).
pub fn string_option(value: &Value, name: &str, allowed: &[&str], label: &str) -> JsResult<Option<String>> {
    if value.is_undefined() {
        return Ok(None);
    }
    let text = coerce_to_string(value).to_rust();
    if allowed.contains(&text.as_str()) {
        return Ok(Some(text));
    }
    let quoted: Vec<String> = allowed.iter().map(|a| format!("\"{a}\"")).collect();
    let list = match quoted.len() {
        2 => format!("either {} or {}", quoted[0], quoted[1]),
        _ => format!("either {}, or {}", quoted[..quoted.len() - 1].join(", "), quoted[quoted.len() - 1]),
    };
    Err(range_error(prefixed(label, &format!("{name} must be {list}"))))
}

/// A boolean option (`numeric`, `ignorePunctuation`, `hour12`).
pub fn bool_option(value: &Value) -> Option<bool> {
    if value.is_undefined() { None } else { Some(value.truthy()) }
}

/// The collator options in upstream's read order, each read once through getters; an object for a flag reads as
/// `true`, other objects convert through the program's `toString`.
pub const COLLATOR_OPTIONS: [&str; 7] =
    ["usage", "localeMatcher", "collation", "numeric", "caseFirst", "sensitivity", "ignorePunctuation"];

pub async fn read_options(rt: &Rt, options: &Value, keys: &[&str], flags: &[&str], site: Site) -> JsResult<Vec<Value>> {
    let Value::Object(o) = options else { return Ok(vec![Value::Undefined; keys.len()]) };
    let mut raw = Vec::with_capacity(keys.len());
    for key in keys {
        raw.push(get(o, &PropKey::str(key))?);
    }
    let mut out = Vec::with_capacity(raw.len());
    for (key, value) in keys.iter().zip(raw) {
        out.push(match value {
            Value::Object(_) if flags.contains(key) => Value::Bool(true),
            Value::Object(_) => to_primitive(rt, &value, Hint::String, site).await?,
            other => other,
        });
    }
    Ok(out)
}

/// A locale's Unicode extension keyword (`-u-kn-true` → `kn` = `true`).
pub fn extension(tag: &str, key: &str) -> Option<String> {
    let lower = tag.to_ascii_lowercase();
    let parts: Vec<&str> = lower.split('-').collect();
    let at = parts.iter().position(|p| *p == "u")?;
    let mut i = at + 1;
    while i < parts.len() && parts[i].len() != 1 {
        if parts[i] == key {
            let value = parts.get(i + 1).filter(|v| v.len() > 2).copied().unwrap_or("true");
            return Some(value.to_string());
        }
        i += 1;
    }
    None
}

/// What `localeCompare` asks the collator for.
#[derive(Clone, Debug, Default)]
pub struct Collation {
    /// The first requested locale's language, which picks the tailoring.
    pub language: Option<String>,
    pub numeric: bool,
    /// `Some(true)`: uppercase first; `Some(false)`: lowercase first (the default order); `None`: off.
    pub upper_first: bool,
    pub sensitivity: Sensitivity,
    pub ignore_punctuation: bool,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Sensitivity {
    Base,
    Accent,
    Case,
    #[default]
    Variant,
}

/// `Intl.Collator(locales, options)`'s resolution, with Bun's validation order and messages.
pub fn collation(tags: &[String], options: &[Value], label: &str) -> JsResult<Collation> {
    let [usage, matcher, _collation, numeric, case_first, sensitivity, ignore] = options else {
        unreachable!("seven collator options")
    };
    string_option(usage, "usage", &["sort", "search"], label)?;
    string_option(matcher, "localeMatcher", &["lookup", "best fit"], label)?;
    let numeric = bool_option(numeric);
    let case_first = string_option(case_first, "caseFirst", &["upper", "lower", "false"], label)?;
    let sensitivity = string_option(sensitivity, "sensitivity", &["base", "accent", "case", "variant"], label)?;
    let ignore = bool_option(ignore);
    let tag = tags.first();
    let numeric = numeric.unwrap_or_else(|| tag.and_then(|t| extension(t, "kn")).is_some_and(|v| v == "true"));
    let case_first = case_first.or_else(|| tag.and_then(|t| extension(t, "kf")));
    Ok(Collation {
        language: tag.map(|t| t.split('-').next().unwrap_or("").to_ascii_lowercase()),
        numeric,
        upper_first: case_first.as_deref() == Some("upper"),
        sensitivity: match sensitivity.as_deref() {
            Some("base") => Sensitivity::Base,
            Some("accent") => Sensitivity::Accent,
            Some("case") => Sensitivity::Case,
            _ => Sensitivity::Variant,
        },
        // Thai is the one locale whose default ignores punctuation.
        ignore_punctuation: ignore.unwrap_or(false),
    })
}

/// `localeCompare(that, locales, options)`.
pub async fn locale_compare(rt: &Rt, value: &[u16], that: &[u16], locales: &Value, options: &Value, site: Site) -> JsResult<Value> {
    const LABEL: &str = "String.localeCompare";
    if matches!(locales, Value::Null) || matches!(options, Value::Null) {
        return Err(type_error(
            "String.localeCompare cannot take null locales or options; pass undefined for the defaults.",
        ));
    }
    let list = read_locales(rt, locales, site, LABEL).await?;
    let read = read_options(rt, options, &COLLATOR_OPTIONS, &["numeric", "ignorePunctuation"], site).await?;
    let tags = match &list {
        Some(values) => canonical_tags(values, LABEL)?,
        None => Vec::new(),
    };
    let collation = collation(&tags, &read, LABEL)?;
    Ok(Value::Number(crate::stdlib::string::collate(value, that, &collation)))
}

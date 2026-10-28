//! Shared config and credentials files as `@smithy/core/config` reads them (`parseIni`,
//! `getConfigData`, `loadSharedConfigFiles`, `parseKnownFiles`, `loadSsoSessionData`,
//! `fromSharedConfigFiles`/`loadConfig`).

use crate::protocols::cloud_env::Env;
use indexmap::IndexMap;
use once_cell::sync::Lazy;
use regex::Regex;
use std::path::PathBuf;

/// Keys of one section, in file order.
pub type Section = IndexMap<String, String>;
/// Sections by name (`default`, `x` for `[profile x]`, `sso-session.y`, `services.z`).
pub type Sections = IndexMap<String, Section>;

const SECTION_TYPES: [&str; 3] = ["profile", "sso-session", "services"];

static PREFIX_KEY: Lazy<Regex> = Lazy::new(|| Regex::new(r#"^([A-Za-z0-9_-]+)\s(["']?)([A-Za-z0-9_\-@+.%:/]+)(["']?)$"#).unwrap());
static COMMENT: Lazy<Regex> = Lazy::new(|| Regex::new(r"(^|\s)[;#]").unwrap());

/// `parseIni`. `Err` for a section named `__proto__` (the whole file is then ignored).
pub fn parse_ini(text: &str) -> Result<Sections, String> {
    let mut map = Sections::new();
    let mut current: Option<String> = None;
    let mut sub: Option<String> = None;
    for raw in text.split('\n') {
        let raw = raw.strip_suffix('\r').unwrap_or(raw);
        let uncommented = match COMMENT.find(raw) {
            Some(m) => &raw[..m.start()],
            None => raw,
        };
        let line = uncommented.trim_matches(is_js_space);
        if line.starts_with('[') && line.ends_with(']') && !line.is_empty() {
            current = None;
            sub = None;
            let name = if line.len() >= 2 { &line[1..line.len() - 1] } else { "" };
            match PREFIX_KEY.captures(name) {
                Some(caps) if caps[2] == caps[4] => {
                    let prefix = &caps[1];
                    if SECTION_TYPES.contains(&prefix) {
                        current = Some(format!("{prefix}.{}", &caps[3]));
                    }
                }
                _ => current = Some(name.to_string()),
            }
            if name == "__proto__" || name == "profile __proto__" {
                return Err(format!("Found invalid profile name \"{name}\""));
            }
        } else if let Some(section) = &current {
            let Some(eq) = line.find('=') else { continue };
            if eq == 0 {
                continue;
            }
            let name = line[..eq].trim_matches(is_js_space).to_string();
            let value = line[eq + 1..].trim_matches(is_js_space).to_string();
            if value.is_empty() {
                sub = Some(name);
            } else {
                if sub.is_some() && raw.trim_start_matches(is_js_space) == raw {
                    sub = None;
                }
                let key = match &sub {
                    Some(s) => format!("{s}.{name}"),
                    None => name,
                };
                map.entry(section.clone()).or_default().insert(key, value);
            }
        }
    }
    Ok(map)
}

fn is_js_space(c: char) -> bool {
    c.is_whitespace() || c == '\u{feff}'
}

/// `getConfigData`: `[default]`, `[profile x]` → `x`, `[sso-session y]`, `[services z]`.
fn config_data(data: Sections) -> Sections {
    let mut out = Sections::new();
    if let Some(default) = data.get("default") {
        out.insert("default".into(), default.clone());
    }
    for (key, value) in data {
        let Some(dot) = key.find('.') else { continue };
        let prefix = &key[..dot];
        if !SECTION_TYPES.contains(&prefix) {
            continue;
        }
        let name = if prefix == "profile" {
            key[dot + 1..].to_string()
        } else {
            key.clone()
        };
        out.insert(name, value);
    }
    out
}

fn expand(env: &Env, path: String) -> PathBuf {
    match path.strip_prefix("~/") {
        Some(rest) => env.aws_home().join(rest),
        None => PathBuf::from(path),
    }
}

/// `AWS_CONFIG_FILE` or `~/.aws/config`.
pub fn config_path(env: &Env) -> PathBuf {
    match env.non_empty("AWS_CONFIG_FILE") {
        Some(path) => expand(env, path),
        None => env.aws_home().join(".aws").join("config"),
    }
}

/// `AWS_SHARED_CREDENTIALS_FILE` or `~/.aws/credentials`.
pub fn credentials_path(env: &Env) -> PathBuf {
    match env.non_empty("AWS_SHARED_CREDENTIALS_FILE") {
        Some(path) => expand(env, path),
        None => env.aws_home().join(".aws").join("credentials"),
    }
}

fn read(path: &PathBuf) -> Option<Sections> {
    let text = std::fs::read_to_string(path).ok()?;
    parse_ini(&text).ok()
}

/// `loadSharedConfigFiles`: unreadable or invalid files are empty.
pub struct SharedFiles {
    pub config: Sections,
    pub credentials: Sections,
}

pub fn load_shared(env: &Env) -> SharedFiles {
    SharedFiles { config: read(&config_path(env)).map(config_data).unwrap_or_default(), credentials: read(&credentials_path(env)).unwrap_or_default() }
}

/// `parseKnownFiles`: config sections, then the credentials file merged over them.
pub fn known_profiles(env: &Env) -> Sections {
    let SharedFiles { mut config, credentials } = load_shared(env);
    for (name, values) in credentials {
        let entry = config.entry(name).or_default();
        for (k, v) in values {
            entry.insert(k, v);
        }
    }
    config
}

/// `loadSsoSessionData`: `[sso-session x]` sections of the config file by name.
pub fn sso_sessions(env: &Env) -> Sections {
    let Some(parsed) = read(&config_path(env)) else { return Sections::new() };
    parsed.into_iter().filter_map(|(k, v)| k.strip_prefix("sso-session.").map(|name| (name.to_string(), v))).collect()
}

/// `getProfileName`: the explicit profile, `AWS_PROFILE`, then `default` (empty values skipped).
pub fn profile_name(env: &Env, profile: Option<&str>) -> String {
    profile.filter(|p| !p.is_empty()).map(String::from).or_else(|| env.non_empty("AWS_PROFILE")).unwrap_or_else(|| "default".into())
}

/// Which file wins when a key is in both (`fromSharedConfigFiles` `preferredFile`).
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Preferred {
    Config,
    Credentials,
}

/// The merged profile `fromSharedConfigFiles` hands its selector, plus the preferred file.
pub fn profile_values(env: &Env, profile: Option<&str>, preferred: Preferred) -> (Section, Sections) {
    let name = profile_name(env, profile);
    let SharedFiles { config, credentials } = load_shared(env);
    let from_credentials = credentials.get(&name).cloned().unwrap_or_default();
    let from_config = config.get(&name).cloned().unwrap_or_default();
    let (mut merged, over) = match preferred {
        Preferred::Config => (from_credentials, from_config),
        Preferred::Credentials => (from_config, from_credentials),
    };
    for (k, v) in over {
        merged.insert(k, v);
    }
    let file = match preferred {
        Preferred::Config => config,
        Preferred::Credentials => credentials,
    };
    (merged, file)
}

/// `loadConfig({environmentVariableSelector, configFileSelector, default})` without the default:
/// the environment selector, then the shared files; `None` lets the caller apply its default.
/// Selectors return `None` both for "not set" and for values they reject (a thrown selector error
/// moves the chain on).
pub fn load_config<T>(
    env: &Env,
    profile: Option<&str>,
    preferred: Preferred,
    from_env: impl Fn(&Env) -> Option<T>,
    from_file: impl Fn(&Section, &Sections) -> Option<T>,
) -> Option<T> {
    if let Some(value) = from_env(env) {
        return Some(value);
    }
    let (merged, file) = profile_values(env, profile, preferred);
    from_file(&merged, &file)
}

/// `booleanSelector`: `"true"` / `"false"`, anything else is rejected.
pub fn boolean(value: Option<&String>) -> Option<bool> {
    match value.map(String::as_str) {
        Some("true") => Some(true),
        Some("false") => Some(false),
        _ => None,
    }
}

/// `parseInt(value, 10)` (leading whitespace, sign, digits); `None` for `NaN`.
pub fn parse_int(value: &str) -> Option<i64> {
    let s = value.trim_start_matches(is_js_space);
    let (sign, digits) = match s.as_bytes().first() {
        Some(b'-') => (-1, &s[1..]),
        Some(b'+') => (1, &s[1..]),
        _ => (1, s),
    };
    let end = digits.find(|c: char| !c.is_ascii_digit()).unwrap_or(digits.len());
    if end == 0 {
        return None;
    }
    digits[..end].parse::<i64>().ok().map(|n| n * sign)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_like_smithy() {
        let text = "[default]\naws_access_key_id = A # c\nkey=v#x\n; comment\n[profile dev]\nregion=us-west-2\ns3 =\n  max = 5\nafter=1\n[sso-session corp]\nsso_region=eu-west-1\n[bad name]\nx=1\n[odd section here]\ny=2\n[profile 'q']\nk=v\n";
        let ini = parse_ini(text).unwrap();
        assert_eq!(ini["default"]["aws_access_key_id"], "A");
        assert_eq!(ini["default"]["key"], "v#x");
        assert_eq!(ini["profile.dev"]["region"], "us-west-2");
        assert_eq!(ini["profile.dev"]["s3.max"], "5");
        assert_eq!(ini["profile.dev"]["after"], "1");
        assert_eq!(ini["sso-session.corp"]["sso_region"], "eu-west-1");
        assert_eq!(ini["profile.q"]["k"], "v");
        assert!(!ini.keys().any(|k| k.contains("bad")));
        assert_eq!(ini["odd section here"]["y"], "2");
        assert!(parse_ini("[__proto__]\na=b").is_err());
        let config = config_data(ini);
        assert_eq!(config.keys().collect::<Vec<_>>(), ["default", "dev", "sso-session.corp", "q"]);
    }

    #[test]
    fn credentials_override_config() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("config"), "[profile p]\nregion=us-west-2\naws_access_key_id=C\n[p]\nignored=1\n").unwrap();
        std::fs::write(dir.path().join("credentials"), "[p]\naws_access_key_id=K\n[profile x]\na=b\n").unwrap();
        let env = Env::from_map([
            ("AWS_CONFIG_FILE", dir.path().join("config").to_string_lossy().to_string()),
            ("AWS_SHARED_CREDENTIALS_FILE", dir.path().join("credentials").to_string_lossy().to_string()),
        ]);
        let profiles = known_profiles(&env);
        assert_eq!(profiles["p"]["aws_access_key_id"], "K");
        assert_eq!(profiles["p"]["region"], "us-west-2");
        assert!(!profiles["p"].contains_key("ignored"));
        assert!(profiles.contains_key("profile.x"));
        let (merged, _) = profile_values(&env, Some("p"), Preferred::Config);
        assert_eq!(merged["aws_access_key_id"], "C");
        assert_eq!(parse_int(" 12ab"), Some(12));
        assert_eq!(parse_int("x"), None);
    }
}

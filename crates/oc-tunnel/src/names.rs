//! Route names and targets (`@opentunnel/protocol/names`).

/// The route name for the tunnel hostname itself.
pub const ROOT_ROUTE: &str = "@";

/// `LABEL_PATTERN`, `^[a-z0-9](?:[a-z0-9-]{0,61}[a-z0-9])?$`: a route name other than `@`, and a profile name.
pub fn is_label(value: &str) -> bool {
    let bytes = value.as_bytes();
    let ok = |c: u8| c.is_ascii_lowercase() || c.is_ascii_digit();
    match bytes.len() {
        0 => false,
        1 => ok(bytes[0]),
        n if n <= 63 => ok(bytes[0]) && ok(bytes[n - 1]) && bytes[1..n - 1].iter().all(|&c| ok(c) || c == b'-'),
        _ => false,
    }
}

/// Whether `value` is a route name: `@` or one lowercase DNS label.
pub fn is_valid_route(value: &str) -> bool {
    value == ROOT_ROUTE || is_label(value)
}

/// `publicHostname`: `<route>.<hostname>`, or `<hostname>` for `@`.
pub fn public_hostname(route: &str, hostname: &str) -> String {
    if route == ROOT_ROUTE { hostname.to_string() } else { format!("{route}.{hostname}") }
}

pub fn is_valid_profile(value: &str) -> bool {
    is_label(value)
}

/// Resolves the route name for a TLS SNI on the given tunnel hostname.
pub fn route_for_sni(sni: &str, hostname: &str) -> Option<String> {
    let name = js_lower(sni);
    let host = js_lower(hostname);
    if name == host {
        return Some(ROOT_ROUTE.to_string());
    }
    let suffix = format!(".{host}");
    let route = name.strip_suffix(&suffix)?;
    is_label(route).then(|| route.to_string())
}

/// `String.prototype.toLowerCase` for the ASCII names a relay forwards (other characters fail the label test anyway).
fn js_lower(value: &str) -> String {
    value.to_lowercase()
}

/// A parsed `host:port` route target.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Target {
    pub host: String,
    pub port: u16,
}

/// Validates a `host:port` route target and returns its parts.
pub fn parse_target(value: &str) -> Option<Target> {
    let separator = value.rfind(':')?;
    let port_text = &value[separator + 1..];
    if port_text.is_empty() || port_text.len() > 5 || !port_text.bytes().all(|c| c.is_ascii_digit()) {
        return None;
    }
    let port: u32 = port_text.parse().ok()?;
    if !(1..=65535).contains(&port) {
        return None;
    }
    let mut host = &value[..separator];
    if host.starts_with('[') {
        if !host.ends_with(']') || host.len() < 2 {
            return None;
        }
        host = &host[1..host.len() - 1];
    }
    if host.is_empty() || !host.bytes().all(|c| c.is_ascii_alphanumeric() || matches!(c, b'.' | b'-' | b':' | b'_')) {
        return None;
    }
    Some(Target { host: host.to_string(), port: port as u16 })
}

/// `validateRoutes` for `host:port` targets: the error is the message the SDK throws. Routes with options are
/// checked by [`crate::routes::resolve_route`].
pub fn validate_routes<'a>(routes: impl IntoIterator<Item = (&'a String, &'a String)>) -> Result<(), String> {
    for (name, target) in routes {
        if !is_valid_route(name) {
            return Err(format!("Invalid route name '{name}'"));
        }
        if parse_target(target).is_none() {
            return Err(format!("Invalid target '{target}' for route '{name}': use host:port"));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn labels() {
        assert!(is_valid_route("@"));
        assert!(is_valid_route("0123456789abcdef"));
        assert!(is_valid_route("a"));
        assert!(is_valid_route("a-b"));
        assert!(!is_valid_route("-a"));
        assert!(!is_valid_route("a-"));
        assert!(!is_valid_route("A"));
        assert!(!is_valid_route(""));
        assert!(is_valid_route(&"a".repeat(63)));
        assert!(!is_valid_route(&"a".repeat(64)));
        assert!(!is_valid_profile("@"));
    }

    #[test]
    fn sni_routes() {
        assert_eq!(route_for_sni("Host.Example", "host.example").as_deref(), Some("@"));
        assert_eq!(route_for_sni("api.host.example", "HOST.example").as_deref(), Some("api"));
        assert_eq!(route_for_sni("a.b.host.example", "host.example"), None);
        assert_eq!(route_for_sni("other.example", "host.example"), None);
        assert_eq!(route_for_sni("xhost.example", "host.example"), None);
    }

    #[test]
    fn targets() {
        assert_eq!(parse_target("127.0.0.1:4096"), Some(Target { host: "127.0.0.1".into(), port: 4096 }));
        assert_eq!(parse_target("[::1]:80"), Some(Target { host: "::1".into(), port: 80 }));
        assert_eq!(parse_target("::1:80"), Some(Target { host: "::1".into(), port: 80 }));
        assert_eq!(parse_target("localhost:0"), None);
        assert_eq!(parse_target("localhost:65536"), None);
        assert_eq!(parse_target("localhost:123456"), None);
        assert_eq!(parse_target("localhost"), None);
        assert_eq!(parse_target(":80"), None);
        assert_eq!(parse_target("[::1:80"), None);
        assert_eq!(parse_target("a b:80"), None);
        let routes = [("x".to_string(), "h:1".to_string())];
        assert!(validate_routes(routes.iter().map(|(a, b)| (a, b))).is_ok());
        let bad = [("X".to_string(), "h:1".to_string())];
        assert_eq!(validate_routes(bad.iter().map(|(a, b)| (a, b))).unwrap_err(), "Invalid route name 'X'");
        let bad = [("x".to_string(), "h".to_string())];
        assert_eq!(
            validate_routes(bad.iter().map(|(a, b)| (a, b))).unwrap_err(),
            "Invalid target 'h' for route 'x': use host:port"
        );
    }
}

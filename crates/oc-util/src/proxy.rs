//! Proxy rules of upstream's runtime (Bun 1.4.4's environment loader, `src/dotenv/env_loader.rs` and
//! `src/dotenv/no_proxy.rs`), shared by `fetch` and `WebSocket`.
//!
//! - `fetch` (`get_http_proxy_for`): `http_proxy` / `HTTP_PROXY` for `http:` URLs, `https_proxy` / `HTTPS_PROXY`
//!   for `https:` ones (lower case first; an empty lower-case value falls through to the upper-case one; empty-ish
//!   values (empty, `""`, `''`) mean none), else `all_proxy` / `ALL_PROXY` when it has no scheme or an `http:` /
//!   `https:` one (a SOCKS `ALL_PROXY` is ignored); then no proxy when [`is_no_proxy`] exempts the host and port.
//!   A proxy without a scheme is an HTTP proxy.
//! - `NO_PROXY` ([`no_proxy_list`]): `no_proxy`, else `NO_PROXY`; matching in [`no_proxy_matches`].
//!
//! [`crate::http::client_builder`] applies the `fetch` rules to every outgoing HTTP client, and the Responses
//! WebSocket (oc-ai) applies [`is_no_proxy`] on top of upstream's own selection.

use std::net::IpAddr;

/// An environment lookup (`process.env[name]`): `None` when the variable is not set.
pub type Env<'a> = &'a (dyn Fn(&str) -> Option<String> + Sync);

/// The process environment (non-UTF-8 values read lossily).
pub fn process_env(name: &str) -> Option<String> {
    std::env::var_os(name).map(|v| v.to_string_lossy().into_owned())
}

/// Bun's `emptyish`: empty, `""` or `''`.
fn emptyish(value: &str) -> bool {
    value.is_empty() || value == "\"\"" || value == "''"
}

/// `no_proxy_list`: `no_proxy`, else `NO_PROXY` (each skipped when empty-ish).
pub fn no_proxy_list(env: Env<'_>) -> String {
    let read = |name: &str| env(name).filter(|v| !emptyish(v));
    read("no_proxy").or_else(|| read("NO_PROXY")).unwrap_or_default()
}

/// `is_no_proxy(host, port)` (`Bun__isNoProxy`).
pub fn is_no_proxy(env: Env<'_>, hostname: &str, port: u16) -> bool {
    no_proxy_matches(&no_proxy_list(env), hostname, port)
}

fn strip_brackets(host: &str) -> &str {
    host.strip_prefix('[').and_then(|h| h.strip_suffix(']')).unwrap_or(host)
}

fn strip_trailing_dot(host: &str) -> &str {
    host.strip_suffix('.').unwrap_or(host)
}

/// A dotted quad or IPv6 address, `::ffff:a.b.c.d` folded into `a.b.c.d`.
fn parse_ip(text: &str) -> Option<IpAddr> {
    text.parse::<IpAddr>().ok().map(|ip| ip.to_canonical())
}

/// `host[:port]`, where `host` may be a bracketed or bare IPv6 literal.
fn split_port(entry: &str) -> (&str, Option<&str>) {
    if entry.starts_with('[') {
        if let Some(close) = entry.find(']') {
            return (&entry[1..close], entry[close + 1..].strip_prefix(':'));
        }
        return (entry, None);
    }
    if entry.matches(':').count() == 1 {
        let (host, port) = entry.split_once(':').unwrap_or((entry, ""));
        return (host, Some(port));
    }
    (entry, None)
}

/// ASCII digits only.
fn parse_digits<T: std::str::FromStr>(text: &str) -> Option<T> {
    if text.is_empty() || !text.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    text.parse().ok()
}

fn cidr_contains(entry: &str, slash: usize, host: IpAddr) -> bool {
    let Ok(written) = strip_brackets(&entry[..slash]).parse::<IpAddr>() else { return false };
    let Some(mut bits) = parse_digits::<u8>(&entry[slash + 1..]) else { return false };
    let network = written.to_canonical();
    if written.is_ipv6() && network.is_ipv4() {
        let Some(v4_bits) = bits.checked_sub(96) else { return false };
        bits = v4_bits;
    }
    fn prefix_eq(a: &[u8], b: &[u8], bits: u8) -> bool {
        let bits = bits as usize;
        if bits > a.len() * 8 {
            return false;
        }
        let (whole, rem) = (bits / 8, bits % 8);
        if a[..whole] != b[..whole] {
            return false;
        }
        rem == 0 || (a[whole] ^ b[whole]) >> (8 - rem) == 0
    }
    match (network, host) {
        (IpAddr::V4(n), IpAddr::V4(h)) => prefix_eq(&n.octets(), &h.octets(), bits),
        (IpAddr::V6(n), IpAddr::V6(h)) => prefix_eq(&n.octets(), &h.octets(), bits),
        _ => false,
    }
}

/// Bun's `no_proxy::matches`: whether `hostname` (an IPv6 literal may be bracketed) on `port` is exempted by
/// `list`. Entries are separated by commas or whitespace: `*`; a domain (`example.com`, `.example.com`,
/// `*.example.com`: the name and every subdomain); an IP address; a CIDR block. A domain or address may carry
/// `:port`, which must then equal `port`. An IP-literal host only matches an address or CIDR entry.
pub fn no_proxy_matches(list: &str, hostname: &str, port: u16) -> bool {
    let hostname = strip_trailing_dot(strip_brackets(hostname));
    if hostname.is_empty() {
        return false;
    }
    let host_ip = parse_ip(hostname);
    for entry in list.split([',', ' ', '\t', '\r', '\n']).filter(|e| !e.is_empty()) {
        if entry == "*" {
            return true;
        }
        if let Some(slash) = entry.find('/') {
            if host_ip.is_some_and(|ip| cidr_contains(entry, slash, ip)) {
                return true;
            }
            continue;
        }
        let (entry_host, entry_port) = split_port(entry);
        if let Some(entry_port) = entry_port {
            match parse_digits::<u16>(entry_port) {
                Some(p) if p == port => {}
                _ => continue,
            }
        }
        let entry_host = entry_host.strip_prefix("*.").or_else(|| entry_host.strip_prefix('.')).unwrap_or(entry_host);
        let entry_host = strip_trailing_dot(entry_host);
        if entry_host.is_empty() {
            continue;
        }
        if let Some(ip) = host_ip {
            if parse_ip(entry_host) == Some(ip) {
                return true;
            }
            continue;
        }
        let (h, e) = (hostname.as_bytes(), entry_host.as_bytes());
        if h.len() == e.len() {
            if h.eq_ignore_ascii_case(e) {
                return true;
            }
        } else if h.len() > e.len() && h[h.len() - e.len() - 1] == b'.' && h[h.len() - e.len()..].eq_ignore_ascii_case(e) {
            return true;
        }
    }
    false
}

/// `get_lower_then_upper`: `lower` unless it is unset or empty, else `upper`.
fn lower_then_upper(env: Env<'_>, lower: &str, upper: &str) -> Option<String> {
    match env(lower) {
        Some(value) if !value.is_empty() => Some(value),
        _ => env(upper),
    }
}

/// `proxy_env_for_scheme`: the proxy variable for an `http:` (`is_http`) or `https:` target.
pub fn proxy_env_for_scheme(env: Env<'_>, is_http: bool) -> Option<String> {
    let (lower, upper) = if is_http { ("http_proxy", "HTTP_PROXY") } else { ("https_proxy", "HTTPS_PROXY") };
    lower_then_upper(env, lower, upper).filter(|p| !emptyish(p)).or_else(|| all_proxy(env))
}

/// `all_proxy`: only a value without a scheme or with an `http:` / `https:` one.
fn all_proxy(env: Env<'_>) -> Option<String> {
    let value = lower_then_upper(env, "all_proxy", "ALL_PROXY").filter(|p| !emptyish(p))?;
    match value.split_once("://") {
        None => Some(value),
        Some((scheme, _)) if scheme.eq_ignore_ascii_case("http") || scheme.eq_ignore_ascii_case("https") => Some(value),
        Some(_) => None,
    }
}

/// `get_http_proxy_for(url)`: the proxy `fetch` uses for `url`, `None` for a direct request.
pub fn fetch_proxy(env: Env<'_>, url: &reqwest::Url) -> Option<reqwest::Url> {
    let value = proxy_env_for_scheme(env, url.scheme() == "http")?;
    if is_no_proxy(env, url.host_str().unwrap_or_default(), url.port_or_known_default().unwrap_or(0)) {
        return None;
    }
    let text = if value.contains("://") { value } else { format!("http://{value}") };
    reqwest::Url::parse(&text).ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn env(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> + Sync + use<> {
        let map: HashMap<String, String> = pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect();
        move |name: &str| map.get(name).cloned()
    }

    #[test]
    fn bun_no_proxy_rules() {
        assert!(no_proxy_matches("example.com", "api.example.com", 443));
        assert!(no_proxy_matches("example.com", "example.com.", 443));
        assert!(no_proxy_matches(".example.com", "example.com", 443));
        assert!(no_proxy_matches("*.example.com", "a.b.example.com", 443));
        assert!(!no_proxy_matches("ample.com", "example.com", 443));
        assert!(no_proxy_matches("example.com:443", "example.com", 443));
        assert!(!no_proxy_matches("example.com:80", "example.com", 443));
        assert!(no_proxy_matches("10.0.0.0/8", "10.1.2.3", 443));
        assert!(!no_proxy_matches("10.0.0.0/8", "11.1.2.3", 443));
        assert!(no_proxy_matches("::1", "[::1]", 443));
        assert!(no_proxy_matches("[::1]:443", "[::1]", 443));
        assert!(no_proxy_matches("127.0.0.2", "127.0.0.2", 80));
        // An IP host never matches a domain suffix.
        assert!(!no_proxy_matches("0.2", "127.0.0.2", 80));
        assert!(no_proxy_matches("a.test *", "x.test", 80));
        // `no_proxy` first, empty-ish values skipped.
        let e = env(&[("no_proxy", "''"), ("NO_PROXY", "x.test")]);
        assert!(is_no_proxy(&e, "x.test", 80));
        let e = env(&[("no_proxy", "y.test"), ("NO_PROXY", "x.test")]);
        assert!(!is_no_proxy(&e, "x.test", 80));
    }

    #[test]
    fn fetch_rules() {
        let url = |u: &str| reqwest::Url::parse(u).unwrap();
        let proxy = |e: &[(&str, &str)], u: &str| fetch_proxy(&env(e), &url(u)).map(|p| p.to_string());
        let e = [("http_proxy", "http://lower:1"), ("HTTP_PROXY", "http://upper:1"), ("HTTPS_PROXY", "http://secure:1")];
        assert_eq!(proxy(&e, "http://api.test/").as_deref(), Some("http://lower:1/"));
        assert_eq!(proxy(&e, "https://api.test/").as_deref(), Some("http://secure:1/"));
        // An empty lower-case value falls through; empty-ish values mean none.
        assert_eq!(proxy(&[("http_proxy", ""), ("HTTP_PROXY", "http://upper:1")], "http://api.test/").as_deref(), Some("http://upper:1/"));
        assert_eq!(proxy(&[("HTTP_PROXY", "''")], "http://api.test/"), None);
        // `ALL_PROXY`: HTTP proxies (also without a scheme) only.
        assert_eq!(proxy(&[("ALL_PROXY", "proxy.test:3128")], "https://api.test/").as_deref(), Some("http://proxy.test:3128/"));
        assert_eq!(proxy(&[("ALL_PROXY", "socks5://proxy.test:1080")], "https://api.test/"), None);
        // `no_proxy` is read before `NO_PROXY`.
        let e = [("HTTP_PROXY", "http://p:1"), ("NO_PROXY", "other.test"), ("no_proxy", "api.test")];
        assert_eq!(proxy(&e, "http://api.test/"), None);
        let e = [("HTTP_PROXY", "http://p:1"), ("NO_PROXY", "api.test:8080")];
        assert_eq!(proxy(&e, "http://api.test:8080/"), None);
        assert_eq!(proxy(&e, "http://api.test/").as_deref(), Some("http://p:1/"));
    }
}

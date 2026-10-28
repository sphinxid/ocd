//! Proxy selection for the Responses WebSocket.
//!
//! Upstream picks the proxy in two layers, and both apply:
//!
//! 1. `core/src/effect/websocket-constructor.ts` (`proxy`): no proxy for the hosts `127.0.0.1` and `localhost`
//!    (`::1` is listed too, but a URL's `hostname` is `[::1]`, so it never matches), none when `NO_PROXY` (else
//!    `no_proxy`) lists the host ([`bypasses_proxy`]); otherwise `WSS_PROXY` / `WS_PROXY` (by the socket's scheme),
//!    then `HTTPS_PROXY` / `HTTP_PROXY`, then `ALL_PROXY`. Each name is read upper case first, then lower case,
//!    and a variable that is set but empty counts as set (it yields "no proxy" instead of falling through).
//! 2. Bun 1.4.4's `WebSocket` with that `proxy` option (`WebSocket.cpp` `setupProxy` / `connect`): the URL must be
//!    `http:` or `https:` (anything else makes the constructor throw, and upstream falls back to HTTP);
//!    credentials in it become `Proxy-Authorization: Basic ...`; and Bun checks its own `NO_PROXY` matcher again
//!    ([`bun_no_proxy`]: `no_proxy`, else `NO_PROXY`, with suffix, IP, CIDR and port rules), which can still send
//!    the socket direct.
//!
//! Without a selected proxy the socket connects directly: Bun's `WebSocket` reads no proxy variable itself.

use base64::Engine;

pub use oc_util::proxy::{Env, is_no_proxy as bun_no_proxy, no_proxy_list as bun_no_proxy_list, no_proxy_matches, process_env};

/// `environmentValue`: `NAME`, else `name`.
fn env_value(env: Env<'_>, name: &str) -> Option<String> {
    env(name).or_else(|| env(&name.to_ascii_lowercase()))
}

/// JavaScript's `\s` (WhiteSpace and LineTerminator).
fn js_space(c: char) -> bool {
    matches!(
        c,
        '\t' | '\n' | '\u{b}' | '\u{c}' | '\r' | ' ' | '\u{a0}' | '\u{1680}' | '\u{2000}'
            ..='\u{200a}' | '\u{2028}' | '\u{2029}' | '\u{202f}' | '\u{205f}' | '\u{3000}' | '\u{feff}'
    )
}

/// `entry.match(/^(.+?):(\d+)$/)`: the host and port of `host:port` (the port ASCII digits after the last colon,
/// the host at least one character).
fn split_entry_port(entry: &str) -> Option<(&str, &str)> {
    let colon = entry.rfind(':')?;
    let (host, port) = (&entry[..colon], &entry[colon + 1..]);
    (!host.is_empty() && !port.is_empty() && port.bytes().all(|b| b.is_ascii_digit())).then_some((host, port))
}

/// `bypassesProxy(url, value)` of websocket-constructor.ts: whether the `NO_PROXY` value lists the socket's host.
/// Entries are split on commas and whitespace; `*` matches everything; `host:port` only matches that port (the
/// URL's port, or 443 / 80 by scheme); a leading `*` is dropped, and an entry starting with `.` matches host names
/// ending with it, any other entry only the exact host name (case-insensitive).
pub fn bypasses_proxy(url: &url::Url, value: Option<&str>) -> bool {
    let Some(value) = value.filter(|v| !v.is_empty()) else { return false };
    let port = url.port().map(|p| p.to_string()).unwrap_or_else(|| if url.scheme() == "wss" { "443" } else { "80" }.to_string());
    let hostname = url.host_str().unwrap_or_default().to_lowercase();
    value.split(|c: char| c == ',' || js_space(c)).any(|entry| {
        if entry.is_empty() {
            return false;
        }
        if entry == "*" {
            return true;
        }
        let matched = split_entry_port(entry);
        if let Some((_, entry_port)) = matched
            && entry_port != port
        {
            return false;
        }
        let host = matched.map_or(entry, |(host, _)| host).to_lowercase();
        let host = host.strip_prefix('*').unwrap_or(&host);
        if host.starts_with('.') { hostname.ends_with(host) } else { hostname == host }
    })
}

/// `WebSocketConstructor.proxy(url)`: the proxy URL upstream hands to Bun's `WebSocket` for `url` (a `ws:` /
/// `wss:` URL), or `None` (also for an empty variable).
pub fn select(url: &str, env: Env<'_>) -> Option<String> {
    let url = url::Url::parse(url).ok()?;
    if ["127.0.0.1", "localhost", "::1"].contains(&url.host_str().unwrap_or_default()) {
        return None;
    }
    if bypasses_proxy(&url, env_value(env, "NO_PROXY").as_deref()) {
        return None;
    }
    let (protocol, standard) = if url.scheme() == "wss" { ("WSS_PROXY", "HTTPS_PROXY") } else { ("WS_PROXY", "HTTP_PROXY") };
    env_value(env, protocol).or_else(|| env_value(env, standard)).or_else(|| env_value(env, "ALL_PROXY")).filter(|v| !v.is_empty())
}

/// A parsed `proxy` option (Bun's `ProxyConfig`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProxyConfig {
    /// The proxy's host (an IPv6 address in brackets).
    pub host: String,
    pub port: u16,
    /// `Basic <base64(user:password)>` from the URL's credentials (percent-decoded).
    pub authorization: Option<String>,
    /// `https:` proxy: TLS to the proxy, then `CONNECT` inside it.
    pub https: bool,
}

/// Bun's `setupProxy`: the error is the `SyntaxError` the constructor throws.
pub fn setup(proxy_url: &str) -> Result<Option<ProxyConfig>, String> {
    if proxy_url.is_empty() {
        return Ok(None);
    }
    let url = url::Url::parse(proxy_url).map_err(|_| format!("Invalid proxy URL: {proxy_url}"))?;
    if url.scheme() != "http" && url.scheme() != "https" {
        return Err(format!("Unsupported proxy protocol \"{}\" (expected \"http\" or \"https\")", url.scheme()));
    }
    let https = url.scheme() == "https";
    Ok(Some(ProxyConfig {
        host: url.host_str().unwrap_or_default().to_string(),
        port: url.port().unwrap_or(if https { 443 } else { 80 }),
        authorization: basic_authorization(&url),
        https,
    }))
}

/// `basicAuthorization(url)`: the percent-decoded user and password, base64-encoded.
pub fn basic_authorization(url: &url::Url) -> Option<String> {
    if url.username().is_empty() && url.password().is_none() {
        return None;
    }
    let mut bytes: Vec<u8> = percent_encoding::percent_decode_str(url.username()).collect();
    bytes.push(b':');
    bytes.extend(percent_encoding::percent_decode_str(url.password().unwrap_or_default()));
    Some(format!("Basic {}", base64::engine::general_purpose::STANDARD.encode(bytes)))
}

/// The proxy a socket to `url` goes through (both layers), or `None` for a direct connection. The error is a
/// rejected proxy URL (the constructor throws).
pub fn resolve(url: &url::Url, env: Env<'_>) -> Result<Option<ProxyConfig>, String> {
    let Some(selected) = select(url.as_str(), env) else { return Ok(None) };
    let Some(config) = setup(&selected)? else { return Ok(None) };
    let port = url.port_or_known_default().unwrap_or(if url.scheme() == "wss" { 443 } else { 80 });
    if bun_no_proxy(env, url.host_str().unwrap_or_default(), port) {
        return Ok(None);
    }
    Ok(Some(config))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn env(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> {
        let map: HashMap<String, String> = pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect();
        move |name: &str| map.get(name).cloned()
    }

    #[test]
    fn selection_order_and_case() {
        let e = env(&[("HTTPS_PROXY", "http://h:1"), ("WSS_PROXY", "http://w:1"), ("ALL_PROXY", "http://a:1"), ("WS_PROXY", "http://p:1")]);
        assert_eq!(select("wss://api.example.com/v1", &e).as_deref(), Some("http://w:1"));
        assert_eq!(select("ws://api.example.com/v1", &e).as_deref(), Some("http://p:1"));
        let e = env(&[("https_proxy", "http://lower:1"), ("ALL_PROXY", "http://a:1")]);
        assert_eq!(select("wss://api.example.com/v1", &e).as_deref(), Some("http://lower:1"));
        assert_eq!(select("ws://api.example.com/v1", &e).as_deref(), Some("http://a:1"));
        // Upper case wins over lower case; a set but empty variable stops the search.
        let e = env(&[("HTTPS_PROXY", "http://upper:1"), ("https_proxy", "http://lower:1")]);
        assert_eq!(select("wss://x.test/", &e).as_deref(), Some("http://upper:1"));
        let e = env(&[("WSS_PROXY", ""), ("HTTPS_PROXY", "http://h:1")]);
        assert_eq!(select("wss://x.test/", &e), None);
        // Loopback names are never proxied, but `[::1]` is (its hostname keeps the brackets).
        let e = env(&[("HTTPS_PROXY", "http://h:1"), ("HTTP_PROXY", "http://h:1")]);
        assert_eq!(select("wss://localhost:8443/", &e), None);
        assert_eq!(select("ws://127.0.0.1:80/", &e), None);
        assert_eq!(select("ws://127.0.0.2:80/", &e).as_deref(), Some("http://h:1"));
        assert_eq!(select("ws://[::1]:80/", &e).as_deref(), Some("http://h:1"));
    }

    #[test]
    fn upstream_no_proxy_rules() {
        let url = |u: &str| url::Url::parse(u).unwrap();
        let u = url("wss://api.example.com/v1/responses");
        assert!(bypasses_proxy(&u, Some("*")));
        assert!(bypasses_proxy(&u, Some("foo, api.example.com")));
        assert!(bypasses_proxy(&u, Some("API.EXAMPLE.COM")));
        assert!(bypasses_proxy(&u, Some(".example.com")));
        assert!(bypasses_proxy(&u, Some("*.example.com")));
        assert!(bypasses_proxy(&u, Some("api.example.com:443")));
        assert!(!bypasses_proxy(&u, Some("api.example.com:80")));
        // Only `.`-prefixed entries match subdomains here.
        assert!(!bypasses_proxy(&u, Some("example.com")));
        assert!(!bypasses_proxy(&u, Some("")));
        assert!(!bypasses_proxy(&u, None));
        let u = url("ws://127.0.0.2:8080/x");
        assert!(bypasses_proxy(&u, Some("127.0.0.2:8080")));
        assert!(!bypasses_proxy(&u, Some("127.0.0.0/8")));
        let e = env(&[("NO_PROXY", ".example.com"), ("HTTPS_PROXY", "http://h:1")]);
        assert_eq!(select("wss://api.example.com/", &e), None);
        // `NO_PROXY` is read before `no_proxy`, and an empty one hides the lower-case one.
        let e = env(&[("NO_PROXY", ""), ("no_proxy", "*"), ("HTTPS_PROXY", "http://h:1")]);
        assert_eq!(select("wss://api.example.com/", &e).as_deref(), Some("http://h:1"));
    }

    #[test]
    fn proxy_urls() {
        assert_eq!(
            setup("http://us%40er:p%3Ass@proxy.test:3128").unwrap(),
            Some(ProxyConfig {
                host: "proxy.test".into(),
                port: 3128,
                authorization: Some(format!("Basic {}", base64::engine::general_purpose::STANDARD.encode("us@er:p:ss"))),
                https: false,
            })
        );
        assert_eq!(setup("https://proxy.test").unwrap().map(|c| (c.port, c.https)), Some((443, true)));
        assert_eq!(setup("http://proxy.test").unwrap().map(|c| (c.port, c.authorization)), Some((80, None)));
        assert_eq!(
            setup("http://user@proxy.test").unwrap().and_then(|c| c.authorization),
            Some(format!("Basic {}", base64::engine::general_purpose::STANDARD.encode("user:")))
        );
        assert_eq!(setup("socks5://proxy.test:1080").unwrap_err(), "Unsupported proxy protocol \"socks5\" (expected \"http\" or \"https\")");
        assert_eq!(setup("proxy.test:3128").unwrap_err(), "Unsupported proxy protocol \"proxy.test\" (expected \"http\" or \"https\")");
        assert_eq!(setup("127.0.0.1:3128").unwrap_err(), "Invalid proxy URL: 127.0.0.1:3128");
        assert_eq!(setup("").unwrap(), None);
    }

    #[test]
    fn both_layers() {
        let url = url::Url::parse("wss://api.example.com/v1/responses").unwrap();
        let e = env(&[("HTTPS_PROXY", "http://proxy.test:3128")]);
        assert_eq!(resolve(&url, &e).unwrap().map(|c| c.host), Some("proxy.test".into()));
        // Bun's matcher bypasses where upstream's does not (a bare domain matches subdomains).
        let e = env(&[("HTTPS_PROXY", "http://proxy.test:3128"), ("NO_PROXY", "example.com")]);
        assert_eq!(resolve(&url, &e).unwrap(), None);
        // ... but the proxy URL is checked first.
        let e = env(&[("HTTPS_PROXY", "socks5://proxy.test:1080"), ("NO_PROXY", "example.com")]);
        assert!(resolve(&url, &e).is_err());
        // Bun reads `no_proxy` first: upstream's layer misses the lower-case list when `NO_PROXY` is set too.
        let e = env(&[("HTTPS_PROXY", "http://proxy.test:3128"), ("NO_PROXY", "other.test"), ("no_proxy", "api.example.com")]);
        assert_eq!(resolve(&url, &e).unwrap(), None);
    }
}

//! HTTP proxies (`client/src/effect/proxy.ts`), as the SDK uses them in upstream's runtime (Bun 1.4.4).
//!
//! - **API calls and the certificate check** go through the runtime's `fetch`, which selects a proxy itself with
//!   Bun's rules ([`oc_util::proxy::fetch_proxy`]: `http_proxy`/`HTTP_PROXY` or `https_proxy`/`HTTPS_PROXY`, an HTTP
//!   `ALL_PROXY`, and Bun's `NO_PROXY` matching). A proxy `fetch` cannot use (such as `socks5://`) or a value that
//!   is not a URL fails the request as a network error.
//! - **The bridge WebSocket**: Bun's WebSocket ignores the environment, so the SDK selects the proxy with its own
//!   rules ([`proxy_from_env`]: `HTTPS_PROXY`, `https_proxy`, `ALL_PROXY`, `all_proxy` for `wss:`, the `HTTP_`
//!   ones for `ws:`, upper case first, and [`bypasses_proxy`] for `NO_PROXY`), and fails when the selected proxy is
//!   not an `http://` or `https://` URL. The WebSocket then skips even that proxy when Bun's own `NO_PROXY` rules
//!   exempt the host.
//! - **The `proxy` option** replaces the environment's proxy for all three; Bun's `NO_PROXY` still applies.

use std::net::IpAddr;
use std::sync::Arc;

use url::Url;

/// An environment lookup (`process.env[name]`), `None` when the variable is not set.
pub type Env = Arc<dyn Fn(&str) -> Option<String> + Send + Sync>;

/// The process environment, read when a request is made.
pub fn process_env() -> Env {
    Arc::new(oc_util::proxy::process_env)
}

/// An environment of the given variables only (for tests and probes).
pub fn env_of(pairs: &[(&str, &str)]) -> Env {
    let map: std::collections::HashMap<String, String> =
        pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect();
    Arc::new(move |name: &str| map.get(name).cloned())
}

/// `^[a-z][a-z\d+.-]*:\/\/` (case-insensitive): the value names its scheme.
fn has_scheme(value: &str) -> bool {
    let Some((scheme, _)) = value.split_once("://") else { return false };
    let mut bytes = scheme.bytes();
    bytes.next().is_some_and(|b| b.is_ascii_alphabetic())
        && bytes.all(|b| b.is_ascii_alphanumeric() || matches!(b, b'+' | b'.' | b'-'))
}

/// `parseProxy`: a proxy URL, taking a value without a scheme as `http://` like curl. `source` names where the value
/// came from; errors never include the URL, which may carry credentials.
pub fn parse_proxy(value: &str, source: &str) -> Result<Url, String> {
    let trimmed = value.trim();
    let text = if has_scheme(trimmed) { trimmed.to_string() } else { format!("http://{trimmed}") };
    let url = Url::parse(&text).map_err(|_| format!("{source} is not a valid proxy URL"))?;
    if url.scheme() != "http" && url.scheme() != "https" {
        return Err(format!(
            "{source} uses an unsupported proxy scheme '{}': OpenTunnel connects through http:// and https:// proxies",
            url.scheme()
        ));
    }
    Ok(url)
}

fn secure(url: &Url) -> bool {
    url.scheme() == "https" || url.scheme() == "wss"
}

/// `Net.isIP`: an IPv4 address in strict dotted-decimal form, or an IPv6 address (a `%zone` suffix allowed).
fn parse_ip(value: &str) -> Option<IpAddr> {
    if let Ok(v4) = value.parse::<std::net::Ipv4Addr>() {
        return Some(IpAddr::V4(v4));
    }
    let (address, zone) = match value.split_once('%') {
        Some((address, zone)) => (address, Some(zone)),
        None => (value, None),
    };
    if zone.is_some_and(|z| {
        z.is_empty() || !z.bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b':'))
    }) {
        return None;
    }
    address.parse::<std::net::Ipv6Addr>().ok().map(IpAddr::V6)
}

/// Node's `ERR_OUT_OF_RANGE` text for a subnet prefix (large integers get `_` separators).
fn prefix_out_of_range(max: u32, received: &str) -> String {
    let digits = received.trim_start_matches('0');
    let digits = if digits.is_empty() { "0" } else { digits };
    let shown = if digits.len() > 10 || digits.parse::<u64>().is_ok_and(|n| n > 1u64 << 32) {
        let mut out = String::new();
        let mut rest = digits;
        while rest.len() > 3 {
            let (head, tail) = rest.split_at(rest.len() - 3);
            out = format!("_{tail}{out}");
            rest = head;
        }
        format!("{rest}{out}")
    } else {
        digits.to_string()
    };
    format!("The value of \"prefix\" is out of range. It must be >= 0 && <= {max}. Received {shown}")
}

/// `sameAddress`: whether `entry` (an address or CIDR range) covers `host`, of the same family. A prefix out of
/// range throws, as Node's `BlockList.addSubnet` does.
fn same_address(entry: &str, host: &str) -> Result<bool, String> {
    let mut parts = entry.split('/');
    let address = parts.next().unwrap_or("");
    let bits = parts.next();
    let Some(network) = parse_ip(address) else { return Ok(false) };
    let Some(host) = parse_ip(host) else { return Ok(false) };
    if network.is_ipv4() != host.is_ipv4() {
        return Ok(false);
    }
    let Some(bits) = bits else { return Ok(network == host) };
    if bits.is_empty() || !bits.bytes().all(|b| b.is_ascii_digit()) {
        return Ok(false);
    }
    let max = if network.is_ipv4() { 32 } else { 128 };
    let prefix = match bits.parse::<u32>() {
        Ok(prefix) if prefix <= max => prefix as usize,
        _ => return Err(prefix_out_of_range(max, bits)),
    };
    let (a, b) = match (network, host) {
        (IpAddr::V4(n), IpAddr::V4(h)) => (n.octets().to_vec(), h.octets().to_vec()),
        (IpAddr::V6(n), IpAddr::V6(h)) => (n.octets().to_vec(), h.octets().to_vec()),
        _ => return Ok(false),
    };
    let (whole, rem) = (prefix / 8, prefix % 8);
    Ok(a[..whole] == b[..whole] && (rem == 0 || (a[whole] ^ b[whole]) >> (8 - rem) == 0))
}

/// `bypassesProxy`: whether `NO_PROXY` exempts `url`: comma or space separated entries, each `*`, a domain that
/// also covers its subdomains (a leading `.` or `*.` is optional), an IP address or CIDR range, optionally with
/// `:port`. Fails like the SDK on a CIDR prefix out of range.
pub fn bypasses_proxy(url: &Url, no_proxy: &str) -> Result<bool, String> {
    let host = url.host_str().unwrap_or("").to_lowercase();
    let host = host.strip_prefix('[').and_then(|h| h.strip_suffix(']')).unwrap_or(&host).to_string();
    let port = url.port().unwrap_or(if secure(url) { 443 } else { 80 });
    for raw in no_proxy.to_lowercase().split(|c: char| c == ',' || c.is_whitespace()) {
        if raw.is_empty() {
            continue;
        }
        if raw == "*" {
            return Ok(true);
        }
        let mut entry = raw;
        let mut entry_port: Option<Option<u16>> = None;
        // `^\[([^\]]+)\](?::(\d+))?$`
        let bracketed = raw.strip_prefix('[').and_then(|rest| {
            let close = rest.find(']')?;
            let inner = &rest[..close];
            let after = &rest[close + 1..];
            if inner.is_empty() {
                return None;
            }
            match after.strip_prefix(':') {
                None if after.is_empty() => Some((inner, None)),
                Some(digits) if !digits.is_empty() && digits.bytes().all(|b| b.is_ascii_digit()) => {
                    Some((inner, Some(digits)))
                }
                _ => None,
            }
        });
        if let Some((inner, digits)) = bracketed {
            entry = inner;
            entry_port = digits.map(|d| d.parse::<u16>().ok());
        } else {
            let parts: Vec<&str> = raw.split(':').collect();
            if parts.len() == 2 && !parts[1].is_empty() && parts[1].bytes().all(|b| b.is_ascii_digit()) {
                entry = parts[0];
                entry_port = Some(parts[1].parse::<u16>().ok());
            }
        }
        // A port that does not fit cannot equal the URL's.
        if let Some(entry_port) = entry_port
            && entry_port != Some(port)
        {
            continue;
        }
        if same_address(entry, &host)? {
            return Ok(true);
        }
        let domain = entry.strip_prefix("*.").or_else(|| entry.strip_prefix('.')).unwrap_or(entry);
        if !domain.is_empty() && (host == domain || host.ends_with(&format!(".{domain}"))) {
            return Ok(true);
        }
    }
    Ok(false)
}

/// `proxyFromEnv`: the proxy the environment selects for `url`: `HTTPS_PROXY` for `https:` and `wss:`, `HTTP_PROXY`
/// for `http:` and `ws:`, then `ALL_PROXY` (each upper case first), unless `NO_PROXY` exempts the host. Fails when
/// the selected proxy is not an `http://` or `https://` URL.
pub fn proxy_from_env(url: &Url, env: &Env) -> Result<Option<Url>, String> {
    let names: [&str; 4] = if secure(url) {
        ["HTTPS_PROXY", "https_proxy", "ALL_PROXY", "all_proxy"]
    } else {
        ["HTTP_PROXY", "http_proxy", "ALL_PROXY", "all_proxy"]
    };
    let Some((name, value)) =
        names.iter().find_map(|name| env(name).filter(|v| !v.trim().is_empty()).map(|v| (*name, v)))
    else {
        return Ok(None);
    };
    let no_proxy = env("NO_PROXY").filter(|v| !v.is_empty()).or_else(|| env("no_proxy")).unwrap_or_default();
    if bypasses_proxy(url, &no_proxy)? {
        return Ok(None);
    }
    parse_proxy(&value, name).map(Some)
}

/// Bun's `NO_PROXY` check, which its `fetch` and WebSocket apply to every proxy, even one given explicitly.
fn bun_exempts(env: &Env, url: &Url) -> bool {
    let env_ref: oc_util::proxy::Env<'_> = &**env;
    oc_util::proxy::is_no_proxy(env_ref, url.host_str().unwrap_or_default(), url.port_or_known_default().unwrap_or(0))
}

/// Why `fetch` could not pick a proxy: the request fails without being sent.
#[derive(Debug, Clone, PartialEq)]
pub struct FetchProxyError;

/// The proxies the client connects through (`makeTransport`).
#[derive(Clone)]
pub struct Transport {
    env: Env,
    explicit: Option<Url>,
}

impl Transport {
    /// Through the proxies `env` selects.
    pub fn from_env(env: Env) -> Transport {
        Transport { env, explicit: None }
    }

    /// Through `proxy` (the `proxy` option), when given. Fails when it is not usable, with the SDK's message.
    pub fn new(proxy: Option<&str>, env: Env) -> Result<Transport, String> {
        let explicit = proxy.map(|p| parse_proxy(p, "The proxy option")).transpose()?;
        Ok(Transport { env, explicit })
    }

    pub fn env(&self) -> &Env {
        &self.env
    }

    /// The proxy `fetch` uses for an API call or the certificate check.
    pub fn fetch_proxy(&self, url: &Url) -> Result<Option<Url>, FetchProxyError> {
        if let Some(explicit) = &self.explicit {
            return Ok((!bun_exempts(&self.env, url)).then(|| explicit.clone()));
        }
        let env_ref: oc_util::proxy::Env<'_> = &*self.env;
        let Some(value) = oc_util::proxy::proxy_env_for_scheme(env_ref, url.scheme() == "http") else {
            return Ok(None);
        };
        if bun_exempts(&self.env, url) {
            return Ok(None);
        }
        let text = if value.contains("://") { value } else { format!("http://{value}") };
        let proxy = Url::parse(&text).map_err(|_| FetchProxyError)?;
        if (proxy.scheme() != "http" && proxy.scheme() != "https") || proxy.host_str().is_none_or(str::is_empty) {
            return Err(FetchProxyError);
        }
        Ok(Some(proxy))
    }

    /// The proxy the bridge WebSocket connects through; the error stops the tunnel.
    pub fn bridge_proxy(&self, url: &Url) -> Result<Option<Url>, String> {
        let selected = match &self.explicit {
            Some(explicit) => Some(explicit.clone()),
            None => proxy_from_env(url, &self.env)?,
        };
        Ok(selected.filter(|_| !bun_exempts(&self.env, url)))
    }
}

#[cfg(test)]
mod tests;

#[cfg(test)]
mod transport_tests {
    use super::*;

    fn url(u: &str) -> Url {
        Url::parse(u).unwrap()
    }

    #[test]
    fn fetch_selects_proxies_with_buns_rules() {
        let fetch = |pairs: &[(&str, &str)], u: &str| {
            Transport::from_env(env_of(pairs)).fetch_proxy(&url(u)).map(|p| p.map(|p| p.to_string()))
        };
        assert_eq!(fetch(&[("https_proxy", "proxy:1")], "https://api.test/"), Ok(Some("http://proxy:1/".into())));
        assert_eq!(fetch(&[("HTTP_PROXY", "http://p:1")], "https://api.test/"), Ok(None));
        // `fetch` cannot use a SOCKS proxy or a value that is not a URL: the request fails.
        assert_eq!(fetch(&[("HTTPS_PROXY", "socks5://p:1")], "https://api.test/"), Err(FetchProxyError));
        assert_eq!(fetch(&[("HTTPS_PROXY", "http://[bad")], "https://api.test/"), Err(FetchProxyError));
        // A SOCKS `ALL_PROXY` is ignored.
        assert_eq!(fetch(&[("ALL_PROXY", "socks5://p:1")], "https://api.test/"), Ok(None));
        assert_eq!(fetch(&[("ALL_PROXY", "p:1")], "https://api.test/"), Ok(Some("http://p:1/".into())));
        // Bun's NO_PROXY: `no_proxy` first, and a CIDR entry takes no port.
        let e = [("HTTPS_PROXY", "http://p:1"), ("NO_PROXY", "other"), ("no_proxy", "api.test")];
        assert_eq!(fetch(&e, "https://api.test/"), Ok(None));
        let e = [("HTTP_PROXY", "http://p:1"), ("NO_PROXY", "127.0.0.0/8:80")];
        assert_eq!(fetch(&e, "http://127.0.0.1/"), Ok(Some("http://p:1/".into())));
    }

    #[test]
    fn the_bridge_selects_proxies_with_the_sdks_rules_then_buns_no_proxy() {
        let bridge = |pairs: &[(&str, &str)], u: &str| {
            Transport::from_env(env_of(pairs)).bridge_proxy(&url(u)).map(|p| p.map(|p| p.to_string()))
        };
        assert_eq!(bridge(&[("HTTPS_PROXY", "p:1")], "wss://api.test/x"), Ok(Some("http://p:1/".into())));
        assert_eq!(
            bridge(&[("ALL_PROXY", "socks5://p:1")], "wss://api.test/x"),
            Err("ALL_PROXY uses an unsupported proxy scheme 'socks5': OpenTunnel connects through http:// and https:// proxies".into())
        );
        // The SDK reads `NO_PROXY` first, Bun's WebSocket `no_proxy`: either exempts the host.
        let e = [("HTTPS_PROXY", "http://p:1"), ("NO_PROXY", "other"), ("no_proxy", "api.test")];
        assert_eq!(bridge(&e, "wss://api.test/x"), Ok(None));
        let e = [("HTTP_PROXY", "http://p:1"), ("NO_PROXY", "127.0.0.0/8:80")];
        assert_eq!(bridge(&e, "ws://127.0.0.1/x"), Ok(None));
        // An explicit proxy is used for every host Bun's NO_PROXY does not exempt.
        let explicit = Transport::new(Some("proxy.test:3128"), env_of(&[("NO_PROXY", "api.test")])).unwrap();
        assert_eq!(explicit.bridge_proxy(&url("wss://api.test/")), Ok(None));
        assert_eq!(
            explicit.bridge_proxy(&url("wss://other.test/")).unwrap().unwrap().as_str(),
            "http://proxy.test:3128/"
        );
        assert_eq!(explicit.fetch_proxy(&url("https://api.test/")), Ok(None));
        assert!(Transport::new(Some("socks5://p:1"), env_of(&[])).is_err());
    }
}

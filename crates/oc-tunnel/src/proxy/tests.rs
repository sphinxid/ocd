//! `proxy.js` vectors: each expected value is what `@opentunnel/client` 0.6.1's `bypassesProxy`, `parseProxy`
//! (as `.href`) and `proxyFromEnv` returned or threw under Bun 1.4.4.

use super::*;

#[test]
fn no_proxy_entries_match_like_the_sdk() {
    let cases: Vec<(&str, &str, Result<bool, &str>)> = vec![
        ("https://api.example.com/", "example.com", Ok(true)),
        ("https://example.com/", ".example.com", Ok(true)),
        ("https://a.b.example.com/", "*.example.com", Ok(true)),
        ("https://notexample.com/", "example.com", Ok(false)),
        ("https://example.com./", "example.com", Ok(false)),
        ("https://example.com/", "example.com.", Ok(false)),
        ("https://EXAMPLE.com/", "Example.COM", Ok(true)),
        ("https://example.com/", "*", Ok(true)),
        ("https://example.com/", "foo, *", Ok(true)),
        ("https://example.com/", "foo bar\texample.com", Ok(true)),
        ("https://example.com/", "foo,,example.com", Ok(true)),
        ("https://example.com/", "example.com:443", Ok(true)),
        ("https://example.com/", "example.com:80", Ok(false)),
        ("http://example.com/", "example.com:80", Ok(true)),
        ("wss://example.com/", "example.com:443", Ok(true)),
        ("ws://example.com:8080/", "example.com:8080", Ok(true)),
        ("https://example.com/", "example.com:abc", Ok(false)),
        ("https://example.com/", "*example.com", Ok(false)),
        ("https://example.com/", "*.*.example.com", Ok(false)),
        ("http://127.0.0.1:5000/", "127.0.0.1", Ok(true)),
        ("http://127.0.0.1:5000/", "127.0.0.1:5000", Ok(true)),
        ("http://127.0.0.1:5000/", "127.0.0.1:5001", Ok(false)),
        ("http://127.0.0.1/", "127.0.0.0/8", Ok(true)),
        ("http://127.0.0.1/", "127.0.0.0/8:80", Ok(true)),
        ("http://127.0.0.1/", "10.0.0.0/8", Ok(false)),
        (
            "http://127.0.0.1/",
            "127.0.0.0/33",
            Err("The value of \"prefix\" is out of range. It must be >= 0 && <= 32. Received 33"),
        ),
        ("http://127.0.0.1/", "127.0.0.0/x", Ok(false)),
        ("http://127.0.0.1/", "127.0.0.0/8/9", Ok(true)),
        ("http://127.0.0.1/", "127.1", Ok(false)),
        ("http://127.1/", "127.0.0.1", Ok(true)),
        ("http://127.0.0.1/", "0127.0.0.1", Ok(false)),
        ("http://127.0.0.1/", "::ffff:127.0.0.1", Ok(false)),
        ("http://[::ffff:127.0.0.1]/", "127.0.0.1", Ok(false)),
        ("http://[::1]/", "::1", Ok(true)),
        ("http://[::1]/", "[::1]", Ok(true)),
        ("http://[::1]:8080/", "[::1]:8080", Ok(true)),
        ("http://[::1]:8080/", "[::1]:8081", Ok(false)),
        ("http://[::1]/", "::/0", Ok(true)),
        ("http://[::1]/", "0::1", Ok(true)),
        ("http://[::1]/", "::1/128", Ok(true)),
        ("http://[2001:db8::5]/", "2001:db8::/32", Ok(true)),
        ("http://[2001:db8::5]/", "[2001:db8::/32]", Ok(true)),
        ("http://[::1]/", "::1:80", Ok(false)),
        ("http://localhost/", "LOCALHOST", Ok(true)),
        ("http://localhost/", "localhost:80", Ok(true)),
        ("http://localhost/", "", Ok(false)),
        ("http://localhost/", " , ", Ok(false)),
        ("http://localhost/", ".", Ok(false)),
        ("http://localhost/", "*.", Ok(false)),
        ("http://a.localhost/", ".localhost", Ok(true)),
        ("http://xn--bcher-kva.example/", "bücher.example", Ok(false)),
        ("http://bücher.example/", "xn--bcher-kva.example", Ok(true)),
        ("http://127.0.0.1/", "127.0.0.1/", Ok(false)),
        ("http://127.0.0.1/", "127.0.0.1/32", Ok(true)),
        ("http://127.0.0.1/", "127.0.0.1/032", Ok(true)),
        ("http://127.0.0.1/", "127.0.0.1/-1", Ok(false)),
        ("http://127.0.0.1/", "[127.0.0.1]", Ok(true)),
        ("http://127.0.0.1/", "[127.0.0.1]:80", Ok(true)),
        (
            "http://[::1]/",
            "::1/129",
            Err("The value of \"prefix\" is out of range. It must be >= 0 && <= 128. Received 129"),
        ),
        ("http://[fe80::1]/", "fe80::1%eth0", Ok(true)),
        ("http://1.2.3.4/", "1.2.3.4.example", Ok(false)),
    ];
    for (url, list, expected) in cases {
        let got = bypasses_proxy(&Url::parse(url).unwrap(), list);
        assert_eq!(got, expected.map_err(String::from), "{url} / {list:?}");
    }
}

#[test]
fn proxy_urls_parse_like_the_sdk() {
    let cases: Vec<(&str, Result<&str, &str>)> = vec![
        ("http://proxy:8080", Ok("http://proxy:8080/")),
        ("proxy:8080", Ok("http://proxy:8080/")),
        (" proxy:8080 ", Ok("http://proxy:8080/")),
        ("HTTPS://Proxy:443/", Ok("https://proxy/")),
        (
            "socks5://proxy:1080",
            Err(
                "HTTPS_PROXY uses an unsupported proxy scheme 'socks5': OpenTunnel connects through http:// and https:// proxies",
            ),
        ),
        (
            "socks5h://proxy",
            Err(
                "HTTPS_PROXY uses an unsupported proxy scheme 'socks5h': OpenTunnel connects through http:// and https:// proxies",
            ),
        ),
        (
            "SOCKS://proxy",
            Err(
                "HTTPS_PROXY uses an unsupported proxy scheme 'socks': OpenTunnel connects through http:// and https:// proxies",
            ),
        ),
        (
            "ftp://proxy",
            Err(
                "HTTPS_PROXY uses an unsupported proxy scheme 'ftp': OpenTunnel connects through http:// and https:// proxies",
            ),
        ),
        ("http://[bad", Err("HTTPS_PROXY is not a valid proxy URL")),
        ("http://", Err("HTTPS_PROXY is not a valid proxy URL")),
        ("::bad", Err("HTTPS_PROXY is not a valid proxy URL")),
        ("1.2.3.4", Ok("http://1.2.3.4/")),
        ("http://u:p%40x@proxy:1", Ok("http://u:p%40x@proxy:1/")),
        ("user:pass@proxy:3128", Ok("http://user:pass@proxy:3128/")),
        ("proxy", Ok("http://proxy/")),
        ("http://proxy:99999", Err("HTTPS_PROXY is not a valid proxy URL")),
        ("a b", Err("HTTPS_PROXY is not a valid proxy URL")),
        ("http://pro xy", Err("HTTPS_PROXY is not a valid proxy URL")),
        (
            "file:///x",
            Err(
                "HTTPS_PROXY uses an unsupported proxy scheme 'file': OpenTunnel connects through http:// and https:// proxies",
            ),
        ),
        (
            "x://y",
            Err(
                "HTTPS_PROXY uses an unsupported proxy scheme 'x': OpenTunnel connects through http:// and https:// proxies",
            ),
        ),
        ("localhost:3128/path?q", Ok("http://localhost:3128/path?q")),
        ("javascript:alert(1)", Err("HTTPS_PROXY is not a valid proxy URL")),
        ("mailto:x@y", Ok("http://mailto:x@y/")),
        ("http:proxy", Err("HTTPS_PROXY is not a valid proxy URL")),
        ("http:/proxy", Ok("http://http/proxy")),
        ("[::1]:3128", Ok("http://[::1]:3128/")),
        ("http://[::1]:3128", Ok("http://[::1]:3128/")),
        ("", Err("HTTPS_PROXY is not a valid proxy URL")),
    ];
    for (value, expected) in cases {
        let got = parse_proxy(value, "HTTPS_PROXY").map(|u| u.to_string());
        assert_eq!(got, expected.map(String::from).map_err(String::from), "{value:?}");
    }
}

#[test]
fn the_environment_selects_the_bridge_proxy_like_the_sdk() {
    type Case = (&'static str, Vec<(&'static str, &'static str)>, Result<Option<&'static str>, &'static str>);
    let cases: Vec<Case> = vec![
        ("https://api.test/", vec![("HTTPS_PROXY", "http://p:1")], Ok(Some("http://p:1/"))),
        ("https://api.test/", vec![("https_proxy", "http://p:1")], Ok(Some("http://p:1/"))),
        (
            "https://api.test/",
            vec![("HTTPS_PROXY", "http://a:1"), ("https_proxy", "http://b:1")],
            Ok(Some("http://a:1/")),
        ),
        ("https://api.test/", vec![("HTTPS_PROXY", "  "), ("https_proxy", "http://b:1")], Ok(Some("http://b:1/"))),
        ("https://api.test/", vec![("HTTP_PROXY", "http://p:1")], Ok(None)),
        ("http://api.test/", vec![("HTTP_PROXY", "http://p:1")], Ok(Some("http://p:1/"))),
        ("ws://api.test/", vec![("http_proxy", "http://p:1")], Ok(Some("http://p:1/"))),
        ("wss://api.test/", vec![("ALL_PROXY", "http://p:1")], Ok(Some("http://p:1/"))),
        (
            "wss://api.test/",
            vec![("all_proxy", "socks5://p:1")],
            Err(
                "all_proxy uses an unsupported proxy scheme 'socks5': OpenTunnel connects through http:// and https:// proxies",
            ),
        ),
        ("wss://api.test/", vec![("HTTPS_PROXY", "socks5://p:1"), ("NO_PROXY", "api.test")], Ok(None)),
        (
            "wss://api.test/",
            vec![("HTTPS_PROXY", "socks5://p:1"), ("NO_PROXY", ""), ("no_proxy", "api.test")],
            Ok(None),
        ),
        (
            "wss://api.test/",
            vec![("HTTPS_PROXY", "http://p:1"), ("NO_PROXY", "other"), ("no_proxy", "api.test")],
            Ok(Some("http://p:1/")),
        ),
        (
            "wss://api.test/",
            vec![("HTTPS_PROXY", "socks5://p:1")],
            Err(
                "HTTPS_PROXY uses an unsupported proxy scheme 'socks5': OpenTunnel connects through http:// and https:// proxies",
            ),
        ),
        ("wss://api.test/", vec![("HTTPS_PROXY", "http://[bad")], Err("HTTPS_PROXY is not a valid proxy URL")),
        ("wss://api.test/", vec![("ALL_PROXY", "::bad")], Err("ALL_PROXY is not a valid proxy URL")),
        ("wss://api.test/", vec![("HTTPS_PROXY", "\"\"")], Ok(Some("http://\"\"/"))),
        ("wss://api.test/", vec![], Ok(None)),
    ];
    for (url, pairs, expected) in cases {
        let got = proxy_from_env(&Url::parse(url).unwrap(), &env_of(&pairs)).map(|p| p.map(|u| u.to_string()));
        assert_eq!(got, expected.map(|p| p.map(String::from)).map_err(String::from), "{url} {pairs:?}");
    }
}

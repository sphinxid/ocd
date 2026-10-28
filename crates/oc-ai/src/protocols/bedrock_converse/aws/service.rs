//! What the SDK's nested clients (`@aws-sdk/nested-clients` sts / sso / sso-oidc / signin) do around
//! each call: region → endpoint (the endpoint rule sets and the partitions table), configured
//! endpoint overrides (`AWS_ENDPOINT_URL_<SERVICE>`, `AWS_ENDPOINT_URL`, `endpoint_url` and
//! `[services]` in the shared config, `AWS_IGNORE_CONFIGURED_ENDPOINT_URLS`), FIPS / dual-stack
//! switches, and the standard retry strategy (`AWS_MAX_ATTEMPTS` / `max_attempts`, default 3).

use super::ini::{self, Preferred};
use crate::protocols::cloud_env::Env;
use rand::Rng;
use std::time::Duration;

/// The partition a region belongs to (`partitionsInfo` of `@aws-sdk/core/client`).
pub struct Partition {
    pub name: &'static str,
    pub dns_suffix: &'static str,
    pub dual_stack_dns_suffix: &'static str,
}

const PARTITIONS: &[(&str, &str, &str, &str, &[&str])] = &[
    (
        "aws",
        "amazonaws.com",
        "api.aws",
        r"^(us|eu|ap|sa|ca|me|af|il|mx)\-\w+\-\d+$",
        &[
            "af-south-1",
            "ap-east-1",
            "ap-east-2",
            "ap-northeast-1",
            "ap-northeast-2",
            "ap-northeast-3",
            "ap-south-1",
            "ap-south-2",
            "ap-southeast-1",
            "ap-southeast-2",
            "ap-southeast-3",
            "ap-southeast-4",
            "ap-southeast-5",
            "ap-southeast-6",
            "ap-southeast-7",
            "aws-global",
            "ca-central-1",
            "ca-west-1",
            "eu-central-1",
            "eu-central-2",
            "eu-north-1",
            "eu-south-1",
            "eu-south-2",
            "eu-west-1",
            "eu-west-2",
            "eu-west-3",
            "il-central-1",
            "me-central-1",
            "me-south-1",
            "mx-central-1",
            "sa-east-1",
            "us-east-1",
            "us-east-2",
            "us-west-1",
            "us-west-2",
        ],
    ),
    ("aws-cn", "amazonaws.com.cn", "api.amazonwebservices.com.cn", r"^cn\-\w+\-\d+$", &["aws-cn-global", "cn-north-1", "cn-northwest-1"]),
    ("aws-eusc", "amazonaws.eu", "api.amazonwebservices.eu", r"^eusc\-(de)\-\w+\-\d+$", &["eusc-de-east-1"]),
    ("aws-iso", "c2s.ic.gov", "api.aws.ic.gov", r"^us\-iso\-\w+\-\d+$", &["aws-iso-global", "us-iso-east-1", "us-iso-west-1"]),
    ("aws-iso-b", "sc2s.sgov.gov", "api.aws.scloud", r"^us\-isob\-\w+\-\d+$", &["aws-iso-b-global", "us-isob-east-1", "us-isob-west-1"]),
    ("aws-iso-e", "cloud.adc-e.uk", "api.cloud-aws.adc-e.uk", r"^eu\-isoe\-\w+\-\d+$", &["aws-iso-e-global", "eu-isoe-west-1"]),
    ("aws-iso-f", "csp.hci.ic.gov", "api.aws.hci.ic.gov", r"^us\-isof\-\w+\-\d+$", &["aws-iso-f-global", "us-isof-east-1", "us-isof-south-1"]),
    ("aws-us-gov", "amazonaws.com", "api.aws", r"^us\-gov\-\w+\-\d+$", &["aws-us-gov-global", "us-gov-east-1", "us-gov-west-1"]),
];

/// `partition(region)`: an explicit region, then the region patterns, then `aws`.
pub fn partition(region: &str) -> Partition {
    let make = |p: &(&'static str, &'static str, &'static str, &'static str, &'static [&'static str])| Partition {
        name: p.0,
        dns_suffix: p.1,
        dual_stack_dns_suffix: p.2,
    };
    if let Some(p) = PARTITIONS.iter().find(|p| p.4.contains(&region)) {
        return make(p);
    }
    for p in PARTITIONS {
        // The patterns use JS `\w` (ASCII).
        let pattern = p.3.replace(r"\w", "[A-Za-z0-9_]");
        if regex::Regex::new(&pattern).map(|re| re.is_match(region)).unwrap_or(false) {
            return make(p);
        }
    }
    make(&PARTITIONS[0])
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Service {
    Sts,
    Sso,
    SsoOidc,
    Signin,
}

impl Service {
    /// The client's `serviceId`.
    pub fn id(self) -> &'static str {
        match self {
            Service::Sts => "STS",
            Service::Sso => "SSO",
            Service::SsoOidc => "SSO OIDC",
            Service::Signin => "Signin",
        }
    }
}

/// A resolved endpoint: base URL (no trailing slash) and SigV4 signing region.
#[derive(Debug, Clone)]
pub struct Endpoint {
    pub url: String,
    pub signing_region: String,
}

/// `getEndpointFromConfig(serviceId)`.
fn configured_endpoint(env: &Env, service: Service) -> Option<String> {
    let ignore = ini::load_config(
        env,
        None,
        Preferred::Config,
        |env| ini::boolean(env.var("AWS_IGNORE_CONFIGURED_ENDPOINT_URLS").as_ref()),
        |profile, _| ini::boolean(profile.get("ignore_configured_endpoint_urls")),
    )
    .unwrap_or(false);
    if ignore {
        return None;
    }
    let id = service.id();
    ini::load_config(
        env,
        None,
        Preferred::Config,
        |env| {
            let suffix = id.split(' ').map(|w| w.to_uppercase()).collect::<Vec<_>>().join("_");
            env.non_empty(&format!("AWS_ENDPOINT_URL_{suffix}")).or_else(|| env.non_empty("AWS_ENDPOINT_URL"))
        },
        |profile, config| {
            if let Some(services) = profile.get("services") {
                // A missing section is an error the loader treats as "not found".
                let section = config.get(&format!("services.{services}"))?;
                let key = format!("{}.endpoint_url", id.split(' ').map(|w| w.to_lowercase()).collect::<Vec<_>>().join("_"));
                if let Some(url) = section.get(&key).filter(|v| !v.is_empty()) {
                    return Some(url.clone());
                }
            }
            profile.get("endpoint_url").filter(|v| !v.is_empty()).cloned()
        },
    )
}

fn flag(env: &Env, var: &str, key: &str) -> bool {
    ini::load_config(env, None, Preferred::Config, |env| ini::boolean(env.var(var).as_ref()), |profile, _| ini::boolean(profile.get(key))).unwrap_or(false)
}

/// The endpoint rule set of each nested client for the default parameters.
pub fn endpoint(env: &Env, service: Service, region: &str) -> Result<Endpoint, String> {
    if let Some(url) = configured_endpoint(env, service) {
        return Ok(Endpoint { url: url.trim_end_matches('/').to_string(), signing_region: region.to_string() });
    }
    let fips = flag(env, "AWS_USE_FIPS_ENDPOINT", "use_fips_endpoint");
    let dual = flag(env, "AWS_USE_DUALSTACK_ENDPOINT", "use_dualstack_endpoint");
    let p = partition(region);
    let at = |template: &str| template.replace("{Region}", region).replace("{dns}", p.dns_suffix).replace("{dual}", p.dual_stack_dns_suffix);
    let url = match service {
        Service::Sts => {
            if !fips && !dual && region == "aws-global" {
                return Ok(Endpoint { url: "https://sts.amazonaws.com".into(), signing_region: "us-east-1".into() });
            }
            match (fips, dual) {
                (true, true) => at("https://sts-fips.{Region}.{dual}"),
                (true, false) if p.name == "aws-us-gov" => at("https://sts.{Region}.amazonaws.com"),
                (true, false) => at("https://sts-fips.{Region}.{dns}"),
                (false, true) => at("https://sts.{Region}.{dual}"),
                (false, false) => at("https://sts.{Region}.{dns}"),
            }
        }
        Service::Sso | Service::SsoOidc => {
            let host = if service == Service::Sso { "portal.sso" } else { "oidc" };
            let fips_host = if service == Service::Sso {
                "portal.sso-fips"
            } else {
                "oidc-fips"
            };
            match (fips, dual) {
                (true, true) => at(&format!("https://{fips_host}.{{Region}}.{{dual}}")),
                (true, false) if p.name == "aws-us-gov" => at(&format!("https://{host}.{{Region}}.amazonaws.com")),
                (true, false) => at(&format!("https://{fips_host}.{{Region}}.{{dns}}")),
                (false, true) => at(&format!("https://{host}.{{Region}}.{{dual}}")),
                (false, false) => at(&format!("https://{host}.{{Region}}.{{dns}}")),
            }
        }
        Service::Signin => {
            if fips {
                return Err("FIPS endpoints are not supported for OAuth operations. Disable FIPS or use a non-OAuth operation.".into());
            }
            if dual {
                at("https://signin.{Region}.{dual}")
            } else {
                at(match p.name {
                    "aws" => "https://{Region}.signin.aws.amazon.com",
                    "aws-cn" => "https://{Region}.signin.amazonaws.cn",
                    "aws-us-gov" => "https://{Region}.signin.amazonaws-us-gov.com",
                    "aws-iso" => "https://{Region}.signin.c2shome.ic.gov",
                    "aws-iso-b" => "https://{Region}.signin.sc2shome.sgov.gov",
                    "aws-iso-f" => "https://{Region}.signin.csphome.hci.ic.gov",
                    "aws-iso-e" => "https://{Region}.signin.csphome.adc-e.uk",
                    "aws-eusc" => "https://{Region}.signin.amazonaws-eusc.eu",
                    _ => "https://{Region}.signin.{dns}",
                })
            }
        }
    };
    Ok(Endpoint { url, signing_region: region.to_string() })
}

/// A client for the SDK's own requests: Node's `http`/`https` modules, which ignore proxy variables.
pub fn http_client() -> reqwest::Client {
    oc_util::http::client_builder().no_proxy().build().expect("reqwest client")
}

/// A response read in full.
pub struct Response {
    pub status: u16,
    pub headers: reqwest::header::HeaderMap,
    pub body: String,
}

/// `maxAttempts`: `AWS_MAX_ATTEMPTS`, `max_attempts`, then 3.
fn max_attempts(env: &Env) -> u32 {
    let parse = |v: Option<&String>| v.and_then(|s| ini::parse_int(s)).map(|n| n.max(1) as u32);
    ini::load_config(env, None, Preferred::Config, |env| parse(env.var("AWS_MAX_ATTEMPTS").as_ref()), |profile, _| parse(profile.get("max_attempts")))
        .unwrap_or(3)
}

const THROTTLING: &[&str] = &[
    "BandwidthLimitExceeded",
    "EC2ThrottledException",
    "LimitExceededException",
    "PriorRequestNotComplete",
    "ProvisionedThroughputExceededException",
    "RequestLimitExceeded",
    "RequestThrottled",
    "RequestThrottledException",
    "SlowDown",
    "ThrottledException",
    "Throttling",
    "ThrottlingException",
    "TooManyRequestsException",
    "TransactionInProgressException",
];
const TRANSIENT: &[&str] = &["TimeoutError", "RequestTimeout", "RequestTimeoutException"];

/// Sends with the standard retry strategy: transient network errors, 500/502/503/504, 429 and
/// throttling codes are retried with `random × 2^attempt × base` backoff (100 ms, 500 ms when
/// throttled, at most 20 s). `Err` carries the network error text of the last attempt.
pub async fn send(env: &Env, build: impl Fn() -> reqwest::RequestBuilder, error_code: impl Fn(&Response) -> Option<String>) -> Result<Response, String> {
    let attempts = max_attempts(env);
    let mut attempt = 0;
    loop {
        attempt += 1;
        let (retry, throttled, result) = match build().send().await {
            Ok(response) => {
                let status = response.status().as_u16();
                let headers = response.headers().clone();
                let body = response.text().await.unwrap_or_default();
                let response = Response { status, headers, body };
                let code = if status >= 300 { error_code(&response) } else { None };
                let throttled = status == 429 || code.as_deref().is_some_and(|c| THROTTLING.contains(&c));
                let transient = matches!(status, 500 | 502 | 503 | 504) || code.as_deref().is_some_and(|c| TRANSIENT.contains(&c));
                (throttled || transient, throttled, Ok(response))
            }
            Err(error) => (true, false, Err(network_message(&error))),
        };
        if !retry || attempt >= attempts {
            return result;
        }
        let base = if throttled { 500.0 } else { 100.0 };
        let delay = (rand::rng().random::<f64>() * 2f64.powi(attempt as i32) * base).min(20_000.0);
        tokio::time::sleep(Duration::from_millis(delay as u64)).await;
    }
}

/// A network error as Node's `http` reports it (`connect ECONNREFUSED 127.0.0.1:1`, ...).
pub fn network_message(error: &reqwest::Error) -> String {
    let mut source: Option<&dyn std::error::Error> = Some(error);
    let mut text = error.to_string();
    while let Some(e) = source {
        if let Some(io) = e.downcast_ref::<std::io::Error>() {
            let code = match io.kind() {
                std::io::ErrorKind::ConnectionRefused => Some("ECONNREFUSED"),
                std::io::ErrorKind::ConnectionReset => Some("ECONNRESET"),
                std::io::ErrorKind::TimedOut => Some("ETIMEDOUT"),
                _ => None,
            };
            if let (Some(code), Some(url)) = (code, error.url()) {
                let host = url.host_str().unwrap_or_default();
                let port = url.port_or_known_default().unwrap_or_default();
                return format!("connect {code} {host}:{port}");
            }
            text = io.to_string();
        }
        source = e.source();
    }
    text
}

/// `extendedEncodeURIComponent` (form values of the query protocol).
pub fn encode_component(value: &str) -> String {
    super::super::sigv4::encode_uri_component(value).replace('!', "%21").replace('\'', "%27").replace('(', "%28").replace(')', "%29").replace('*', "%2A")
}

/// The text of the first `<tag>` element (XML entities decoded).
pub fn xml_text(body: &str, tag: &str) -> Option<String> {
    let open = format!("<{tag}>");
    let close = format!("</{tag}>");
    let start = body.find(&open)? + open.len();
    let end = body[start..].find(&close)? + start;
    Some(xml_unescape(&body[start..end]))
}

fn xml_unescape(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(amp) = rest.find('&') {
        out.push_str(&rest[..amp]);
        let tail = &rest[amp..];
        let Some(semi) = tail.find(';') else {
            out.push_str(tail);
            return out;
        };
        let entity = &tail[1..semi];
        let decoded = match entity {
            "amp" => Some('&'),
            "lt" => Some('<'),
            "gt" => Some('>'),
            "quot" => Some('"'),
            "apos" => Some('\''),
            _ if entity.starts_with("#x") => u32::from_str_radix(&entity[2..], 16).ok().and_then(char::from_u32),
            _ if entity.starts_with('#') => entity[1..].parse().ok().and_then(char::from_u32),
            _ => None,
        };
        match decoded {
            Some(c) => out.push(c),
            None => out.push_str(&tail[..=semi]),
        }
        rest = &tail[semi + 1..];
    }
    out.push_str(rest);
    out
}

/// The REST-JSON error code: `x-amzn-errortype`, then `code`, then `__type` (sanitized).
pub fn json_error_code(response: &Response) -> Option<String> {
    let sanitize = |raw: &str| {
        let mut v = raw.to_string();
        if let Some(i) = v.find(',') {
            v.truncate(i);
        }
        if let Some(i) = v.find(':') {
            v.truncate(i);
        }
        if let Some(i) = v.find('#') {
            v = v[i + 1..].to_string();
        }
        v
    };
    if let Some(value) = response.headers.get("x-amzn-errortype").and_then(|v| v.to_str().ok()) {
        return Some(sanitize(value));
    }
    let body: serde_json::Value = serde_json::from_str(&response.body).ok()?;
    let object = body.as_object()?;
    if let Some((_, code)) = object.iter().find(|(k, _)| k.eq_ignore_ascii_case("code")) {
        if let Some(s) = code.as_str() {
            return Some(sanitize(s));
        }
        if let Some(n) = code.as_f64() {
            return Some(sanitize(&n.to_string()));
        }
    }
    object.get("__type").and_then(|v| v.as_str()).map(sanitize)
}

/// A REST-JSON service exception as `String(error)` prints it (`Name: message`).
pub fn json_exception(response: &Response) -> (String, String) {
    let name = json_error_code(response).unwrap_or_else(|| "Unknown".into());
    let body: serde_json::Value = serde_json::from_str(&response.body).unwrap_or_default();
    let message = body.get("message").or_else(|| body.get("Message")).and_then(|v| v.as_str()).unwrap_or("UnknownError").to_string();
    (name, message)
}

/// The query-protocol (STS) error code.
pub fn query_error_code(response: &Response) -> Option<String> {
    xml_text(&response.body, "Code").or_else(|| (response.status == 404).then(|| "NotFound".to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn partitions_and_endpoints() {
        assert_eq!(partition("eu-west-1").name, "aws");
        assert_eq!(partition("cn-north-1").dns_suffix, "amazonaws.com.cn");
        assert_eq!(partition("us-gov-west-1").name, "aws-us-gov");
        assert_eq!(partition("us-isob-east-1").name, "aws-iso-b");
        assert_eq!(partition("nowhere").name, "aws");
        let env = Env::from_map([("HOME", "/nonexistent-ocd-test")]);
        assert_eq!(endpoint(&env, Service::Sts, "eu-west-1").unwrap().url, "https://sts.eu-west-1.amazonaws.com");
        let global = endpoint(&env, Service::Sts, "aws-global").unwrap();
        assert_eq!((global.url.as_str(), global.signing_region.as_str()), ("https://sts.amazonaws.com", "us-east-1"));
        assert_eq!(endpoint(&env, Service::Sso, "cn-north-1").unwrap().url, "https://portal.sso.cn-north-1.amazonaws.com.cn");
        assert_eq!(endpoint(&env, Service::SsoOidc, "us-east-2").unwrap().url, "https://oidc.us-east-2.amazonaws.com");
        assert_eq!(endpoint(&env, Service::Signin, "us-east-1").unwrap().url, "https://us-east-1.signin.aws.amazon.com");
        let fips = Env::from_map([("HOME", "/nonexistent-ocd-test"), ("AWS_USE_FIPS_ENDPOINT", "true")]);
        assert_eq!(endpoint(&fips, Service::Sts, "us-west-2").unwrap().url, "https://sts-fips.us-west-2.amazonaws.com");
        let custom = Env::from_map([("HOME", "/nonexistent-ocd-test"), ("AWS_ENDPOINT_URL", "http://a/"), ("AWS_ENDPOINT_URL_SSO_OIDC", "http://b")]);
        assert_eq!(endpoint(&custom, Service::SsoOidc, "us-east-1").unwrap().url, "http://b");
        assert_eq!(endpoint(&custom, Service::Sts, "us-east-1").unwrap().url, "http://a");
        let ignored = Env::from_map([("HOME", "/x"), ("AWS_ENDPOINT_URL", "http://a"), ("AWS_IGNORE_CONFIGURED_ENDPOINT_URLS", "true")]);
        assert_eq!(endpoint(&ignored, Service::Sts, "us-east-1").unwrap().url, "https://sts.us-east-1.amazonaws.com");
    }

    #[test]
    fn services_section() {
        let dir = tempfile::tempdir().unwrap();
        let config = dir.path().join("config");
        std::fs::write(&config, "[default]\nservices = local\n[services local]\nsts =\n  endpoint_url = http://127.0.0.1:9\n").unwrap();
        let env = Env::from_map([("AWS_CONFIG_FILE", config.to_string_lossy().to_string()), ("HOME", "/x".into())]);
        assert_eq!(endpoint(&env, Service::Sts, "us-east-1").unwrap().url, "http://127.0.0.1:9");
        assert_eq!(endpoint(&env, Service::Sso, "us-east-1").unwrap().url, "https://portal.sso.us-east-1.amazonaws.com");
    }

    #[test]
    fn xml_helpers() {
        assert_eq!(xml_text("<a><Code>X&amp;Y</Code></a>", "Code").as_deref(), Some("X&Y"));
        assert_eq!(encode_component("a b!*"), "a%20b%21%2A");
    }
}

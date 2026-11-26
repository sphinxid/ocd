//! `machine add|list|remove` (2.4.1 `cli/src/commands/handlers/machine/*.ts`): pairs remote machines with the
//! server through its `experimental.machine.*` routes.
//!
//! ocd's server has no machines (upstream ships only the home side, and no worker to link with), so it answers these
//! routes as a release build with the `machines` flag off: every command ends with the "disabled on this server"
//! message. Against an upstream server with the flag on they work as upstream's do.

use oc_util::Global;
use serde_json::{Value, json};

use crate::cli::parse::Input;
use crate::client::{Client, ClientError, Req};
use crate::connection::{self, Args};
use crate::service::ServiceConfig;
use crate::term::{EOL, write_stderr, write_stdout};

/// `MACHINE_LINK_PATH`
pub const MACHINE_LINK_PATH: &str = "/api/experimental/machine/link";

/// What a failing handler step gives `reportMachineError`: a client error keeps the server's decoded body.
#[derive(Debug)]
pub enum Failure {
    Client(ClientError),
    Other(String),
}

impl From<ClientError> for Failure {
    fn from(e: ClientError) -> Self {
        Failure::Client(e)
    }
}

impl From<anyhow::Error> for Failure {
    fn from(e: anyhow::Error) -> Self {
        match e.downcast::<ClientError>() {
            Ok(c) => Failure::Client(c),
            Err(e) => Failure::Other(e.to_string()),
        }
    }
}

/// `ensureMachineClient`: a machine needs a server that keeps running, so a private standalone server is refused,
/// and so is a disabled background service unless `--server` names another server.
async fn ensure_client(global: &Global) -> Result<(connection::Resolved, Client), Failure> {
    let settings = crate::cli::settings();
    if settings.standalone.unwrap_or(false) {
        return Err(Failure::Other(
            "--standalone starts a server for this command only; a machine needs one that keeps running.".into(),
        ));
    }
    if settings.server.is_none() && ServiceConfig::new(global).read().disabled == Some(true) {
        return Err(Failure::Other(
            "Managing machines requires the background service; run `ocd service unset disabled` first.".into(),
        ));
    }
    let server = connection::resolve(global, Args::default()).await?;
    let client = server.client();
    Ok((server, client))
}

/// `serverURLs(client, url)`: the `--url` given, else the server's own URLs.
async fn server_urls(client: &Client, url: Option<String>) -> Result<Vec<String>, Failure> {
    if let Some(url) = url {
        return Ok(vec![url]);
    }
    // `Effect.tryPromise` without a `catch`: the failure is an `UnknownError` with Effect's own message.
    let info = client
        .call("server.info", Req::new())
        .await
        .map_err(|_| Failure::Other("An error occurred in Effect.tryPromise".into()))?;
    Ok(info
        .get("urls")
        .and_then(Value::as_array)
        .map(|a| a.iter().filter_map(|u| u.as_str().map(String::from)).collect())
        .unwrap_or_default())
}

pub async fn add(global: &Global, input: &Input) -> anyhow::Result<i32> {
    let name = input.str("name").unwrap_or_default();
    let run = async {
        let (_server, client) = ensure_client(global).await?;
        let urls = server_urls(&client, input.str("url")).await?;
        let reach = machine_reach(&urls);
        let mut body = json!({ "name": name, "reach": reach });
        if input.flag("replace") {
            body["replace"] = Value::Bool(true);
        }
        let invited = client.call("experimental.machine.add", Req::new().body(body)).await?;
        let data = invited.get("data").cloned().unwrap_or(Value::Null);
        // A server from before this field existed returns none; show what was sent.
        let shown = data.get("reach").and_then(Value::as_array).cloned().unwrap_or(reach);
        write_stdout(&format_machine_add(&AddOutput {
            name: data.get("name").and_then(Value::as_str).unwrap_or_default(),
            join: data.get("join").and_then(Value::as_str).unwrap_or_default(),
            expires: data.get("expires").and_then(Value::as_f64).unwrap_or(0.0) as i64,
            reach: &shown,
            tty: crate::term::stdout_is_tty(),
            now: None,
        }));
        Ok::<(), Failure>(())
    };
    Ok(run.await.map_or_else(|e| report_machine_error(&e, Some(&name)), |()| 0))
}

pub async fn list(global: &Global, _input: &Input) -> anyhow::Result<i32> {
    let run = async {
        let (_server, client) = ensure_client(global).await?;
        let machines = client.call("experimental.machine.list", Req::new()).await?;
        let machines = machines.get("data").and_then(Value::as_array).cloned().unwrap_or_default();
        write_stdout(&format!("{}{EOL}", format_machine_list(&machines)));
        Ok::<(), Failure>(())
    };
    Ok(run.await.map_or_else(|e| report_machine_error(&e, None), |()| 0))
}

pub async fn remove(global: &Global, input: &Input) -> anyhow::Result<i32> {
    let name = input.str("name").unwrap_or_default();
    let run = async {
        let (_server, client) = ensure_client(global).await?;
        client.call("experimental.machine.remove", Req::new().path("name", name.clone())).await?;
        write_stdout(&format!("Removed machine {name}.{EOL}"));
        Ok::<(), Failure>(())
    };
    Ok(run.await.map_or_else(|e| report_machine_error(&e, Some(&name)), |()| 0))
}

/// `isLoopback(hostname)` (`services/service-urls.ts`), on a WHATWG hostname (IPv6 in brackets).
pub fn is_loopback(hostname: &str) -> bool {
    hostname == "localhost" || hostname.ends_with(".localhost") || hostname.starts_with("127.") || hostname == "[::1]"
}

/// `new URL(raw).hostname`
fn hostname(url: &url::Url) -> Option<String> {
    let host = url.host_str()?;
    Some(match url.host() {
        Some(url::Host::Ipv6(_)) => format!("[{}]", host.trim_start_matches('[').trim_end_matches(']')),
        _ => host.to_string(),
    })
}

/// `machineReach(urls)`: each server URL as the link's WebSocket address (`https` → `wss`, anything else → `ws`),
/// addresses other computers can reach first and loopback ones last, without duplicates.
pub fn machine_reach(urls: &[String]) -> Vec<Value> {
    let parsed: Vec<(String, bool)> = urls
        .iter()
        .filter_map(|raw| {
            let mut url = url::Url::parse(raw).ok()?.join(MACHINE_LINK_PATH).ok()?;
            let scheme = if url.scheme() == "https" { "wss" } else { "ws" };
            url.set_scheme(scheme).ok()?;
            let loopback = hostname(&url).is_some_and(|h| is_loopback(&h));
            Some((url.to_string(), loopback))
        })
        .collect();
    let mut seen = std::collections::HashSet::new();
    parsed
        .iter()
        .filter(|(_, loopback)| !loopback)
        .chain(parsed.iter().filter(|(_, loopback)| *loopback))
        .filter(|(href, _)| seen.insert(href.clone()))
        .map(|(href, _)| json!({ "type": "url", "url": href }))
        .collect()
}

/// The input of `formatMachineAdd`.
pub struct AddOutput<'a> {
    pub name: &'a str,
    pub join: &'a str,
    /// Epoch milliseconds.
    pub expires: i64,
    pub reach: &'a [Value],
    pub tty: bool,
    pub now: Option<i64>,
}

fn is_iroh(entry: &Value) -> bool {
    entry.get("type").and_then(Value::as_str) == Some("iroh")
}

/// `formatMachineAdd`: only the join string when stdout is not a terminal (so it can be piped), else the details.
pub fn format_machine_add(input: &AddOutput) -> String {
    if !input.tty {
        return format!("{}{EOL}", input.join);
    }
    let url_of = |entry: &Value| entry.get("url").and_then(Value::as_str).unwrap_or_default().to_string();
    let addresses: Vec<String> = if input.reach.is_empty() {
        vec!["  (none)".into()]
    } else {
        input.reach.iter().map(|entry| format!("  {}", if is_iroh(entry) { "iroh".into() } else { url_of(entry) })).collect()
    };
    let reachable = |entry: &Value| {
        entry.get("type").and_then(Value::as_str) == Some("url")
            && url::Url::parse(&url_of(entry)).ok().and_then(|u| hostname(&u)).is_some_and(|h| !is_loopback(&h))
    };
    let access = if input.reach.iter().any(is_iroh) {
        "Works from any network."
    } else if input.reach.iter().any(reachable) {
        "Works from computers that can reach one of these addresses."
    } else {
        "Only works on this computer. Pass --url with an address other computers can reach."
    };
    let mut lines = vec![
        format!("Machine: {}", input.name),
        format_expiry(input.expires, input.now.unwrap_or_else(|| chrono::Utc::now().timestamp_millis())),
        String::new(),
        input.join.to_string(),
        String::new(),
        access.to_string(),
        "Addresses:".to_string(),
    ];
    lines.extend(addresses);
    format!("{}{EOL}", lines.join(EOL))
}

/// `formatExpiry`: minutes left, rounded, and the local time (`toLocaleTimeString([], {hour: "2-digit",
/// minute: "2-digit"})` in Bun's en-US default locale: `01:05 PM`).
fn format_expiry(expires: i64, now: i64) -> String {
    use chrono::{Local, TimeZone, Timelike};
    let remaining = (expires - now).max(0);
    let minutes = js_round(remaining as f64 / 60_000.0) as i64;
    let at = match Local.timestamp_millis_opt(expires).single() {
        Some(t) => {
            let (pm, h12) = t.hour12();
            format!("{h12:02}:{:02} {}", t.minute(), if pm { "PM" } else { "AM" })
        }
        None => "Invalid Date".into(),
    };
    if minutes == 0 {
        return format!("Expires in less than a minute (at {at}).");
    }
    format!("Expires in {minutes} {} (at {at}).", if minutes == 1 { "minute" } else { "minutes" })
}

/// `Math.round`
fn js_round(x: f64) -> f64 {
    (x + 0.5).floor()
}

/// `String(n)` for the numbers `formatMachineList` prints.
fn js_number(n: f64) -> String {
    if n.is_nan() {
        "NaN".into()
    } else if n.is_infinite() {
        if n > 0.0 { "Infinity".into() } else { "-Infinity".into() }
    } else if n == n.trunc() && n.abs() < 1e21 {
        format!("{}", n as i64)
    } else {
        format!("{n}")
    }
}

/// `String.prototype.padEnd` (UTF-16 length).
fn pad_end(s: &str, width: usize) -> String {
    let len = s.encode_utf16().count();
    format!("{s}{}", " ".repeat(width.saturating_sub(len)))
}

fn str_field<'a>(v: &'a Value, key: &str) -> Option<&'a str> {
    v.get(key).and_then(Value::as_str)
}

fn truthy(v: &Value, key: &str) -> bool {
    match v.get(key) {
        Some(Value::Bool(b)) => *b,
        Some(Value::String(s)) => !s.is_empty(),
        Some(Value::Number(n)) => n.as_f64().is_some_and(|f| f != 0.0),
        Some(Value::Null) | None => false,
        Some(_) => true,
    }
}

/// `formatMachineList`: one aligned row per machine — name, paired or waiting, connected or offline, then the platform
/// and, while connected, how (`iroh direct 82ms`).
pub fn format_machine_list(machines: &[Value]) -> String {
    if machines.is_empty() {
        return "No machines added.".into();
    }
    let id = |m: &Value| str_field(m, "id").unwrap_or_default().to_string();
    let width = |f: &dyn Fn(&Value) -> String| machines.iter().map(|m| f(m).encode_utf16().count()).max().unwrap_or(0);
    let paired = |m: &Value| if truthy(m, "paired") { "paired" } else { "waiting" }.to_string();
    let online = |m: &Value| if truthy(m, "online") { "connected" } else { "offline" }.to_string();
    let name_width = width(&id);
    let paired_width = width(&paired);
    let online_width = width(&online);
    machines
        .iter()
        .map(|machine| {
            let paired = pad_end(&paired(machine), paired_width);
            let online = online(machine);
            let (os, arch) = (str_field(machine, "os"), str_field(machine, "arch"));
            let platform = match (os, arch) {
                (Some(os), Some(arch)) if !os.is_empty() && !arch.is_empty() => Some(format!("{os}/{arch}")),
                _ => os.or(arch).map(String::from),
            };
            let connection = truthy(machine, "online").then(|| machine.get("connection")).flatten().filter(|c| c.is_object());
            let carrier = connection.map(|c| {
                let rtt = match c.get("rtt") {
                    Some(Value::Number(n)) => Some(format!("{}ms", js_number(js_round(n.as_f64().unwrap_or(f64::NAN))))),
                    Some(Value::String(s)) => Some(format!("{s}ms")),
                    // `Math.round(null)`
                    Some(Value::Null) => Some("0ms".into()),
                    _ => None,
                };
                [str_field(c, "kind").map(String::from), str_field(c, "path").map(String::from), rtt]
                    .into_iter()
                    .flatten()
                    .collect::<Vec<_>>()
                    .join(" ")
            });
            let details = [platform, carrier].into_iter().flatten().collect::<Vec<_>>().join("  ");
            let id = pad_end(&id(machine), name_width);
            if details.is_empty() {
                return format!("{id}  {paired}  {online}");
            }
            format!("{id}  {paired}  {}  {details}", pad_end(&online, online_width))
        })
        .collect::<Vec<_>>()
        .join(EOL)
}

/// `formatMachineError` on a server error body (`_tag`, `message`, ...); `message` is `errorMessage(error)`.
pub fn format_error_body(body: Option<&Value>, message: &str, name: Option<&str>) -> String {
    let tag = body.and_then(|b| str_field(b, "_tag"));
    let quoted = |s: &str| serde_json::to_string(s).unwrap_or_default();
    match tag {
        Some("ForbiddenError") => {
            "Machines are experimental and disabled on this server. Turn them on with OPENCODE_FLAGS=machines or experimental.machines: true in your config.".into()
        }
        Some("InvalidRequestError") if body.and_then(|b| str_field(b, "kind")) == Some("invalid_name") => {
            let label = name.map(quoted).unwrap_or_else(|| "That name".into());
            format!(
                "{label} is not a valid machine name. Use lowercase letters, digits, and hyphens, starting with a letter, 2 to 63 characters, and not \"local\"."
            )
        }
        Some("MachineNotFoundError") => {
            let machine = name.or_else(|| body.and_then(|b| str_field(b, "machine"))).unwrap_or_default();
            format!("No machine named {}.", quoted(machine))
        }
        Some("ConflictError") => {
            let label = match name.or_else(|| body.and_then(|b| str_field(b, "resource"))) {
                Some(target) => format!("Machine {}", quoted(target)),
                None => "This machine".into(),
            };
            format!(
                "{label} is already paired. Pass --replace to issue a new join string; once the new computer joins, the computer currently paired stops being able to connect."
            )
        }
        _ => message.to_string(),
    }
}

/// `formatMachineError(error, name)`
pub fn format_machine_error(error: &Failure, name: Option<&str>) -> String {
    match error {
        Failure::Client(e) => format_error_body(e.body(), &e.to_string(), name),
        Failure::Other(message) => format_error_body(None, message, name),
    }
}

/// `reportMachineError`: the message on stderr, exit code 1.
fn report_machine_error(error: &Failure, name: Option<&str>) -> i32 {
    write_stderr(&format!("{}{EOL}", format_machine_error(error, name)));
    1
}

#[cfg(test)]
mod tests {
    use super::*;

    fn urls(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| s.to_string()).collect()
    }

    fn reach_urls(reach: &[Value]) -> Vec<String> {
        reach.iter().map(|r| r["url"].as_str().unwrap().to_string()).collect()
    }

    #[test]
    fn reach_puts_reachable_addresses_first_and_loopback_last_converting_to_websockets() {
        let reach = machine_reach(&urls(&[
            "http://127.0.0.1:4096",
            "http://localhost:4096",
            "http://10.0.0.8:4096",
            "https://example.com",
            "http://[::1]:4096",
            "https://example.com/",
            "https://gpu.example.com:8443",
        ]));
        assert!(reach.iter().all(|r| r["type"] == "url"));
        assert_eq!(
            reach_urls(&reach),
            [
                "ws://10.0.0.8:4096/api/experimental/machine/link",
                "wss://example.com/api/experimental/machine/link",
                "wss://gpu.example.com:8443/api/experimental/machine/link",
                "ws://127.0.0.1:4096/api/experimental/machine/link",
                "ws://localhost:4096/api/experimental/machine/link",
                "ws://[::1]:4096/api/experimental/machine/link",
            ]
        );
    }

    #[test]
    fn url_replaces_the_server_urls_with_that_single_validated_address() {
        let parse = |v: &str| crate::cli::parse::parse_server_url(v);
        let override_url = parse("https://override.example.com:8443/").unwrap();
        assert_eq!(override_url, "https://override.example.com:8443");
        assert_eq!(reach_urls(&machine_reach(&[override_url])), ["wss://override.example.com:8443/api/experimental/machine/link"]);
        assert_eq!(
            reach_urls(&machine_reach(&urls(&["http://127.0.0.1:4096", "https://example.com"]))),
            ["wss://example.com/api/experimental/machine/link", "ws://127.0.0.1:4096/api/experimental/machine/link"]
        );
        assert!(parse("ftp://example.com").is_err());
        assert!(parse("https://user:pass@example.com").is_err());
        assert!(parse("https://example.com?a=1").is_err());
    }

    fn add(reach: &[Value], tty: bool, expires: i64, now: i64) -> String {
        format_machine_add(&AddOutput { name: "gpu", join: "ocjoin_example", expires, reach, tty, now: Some(now) })
    }

    #[test]
    fn add_prints_only_the_join_string_off_a_terminal_and_details_on_one() {
        let reach = [json!({ "type": "url", "url": "wss://example.com/api/experimental/machine/link" })];
        let now = chrono::TimeZone::with_ymd_and_hms(&chrono::Utc, 2026, 1, 1, 12, 0, 0).unwrap().timestamp_millis();
        let expires = now + 5 * 60_000;
        assert_eq!(add(&reach, false, expires, now), format!("ocjoin_example{EOL}"));
        let tty = add(&reach, true, expires, now);
        let lines: Vec<&str> = tty.trim_end().split(EOL).collect();
        assert_eq!(lines[0], "Machine: gpu");
        assert!(lines[1].contains("Expires in 5 minutes"), "{}", lines[1]);
        assert!(lines.contains(&"ocjoin_example"));
        assert!(lines.contains(&"  wss://example.com/api/experimental/machine/link"));
        assert!(lines.contains(&"Works from computers that can reach one of these addresses."));
    }

    #[test]
    fn add_explains_the_invite_reach() {
        let cases = [
            (vec![json!({ "type": "url", "url": "ws://localhost:4096" }), json!({ "type": "iroh" })], "Works from any network."),
            (vec![json!({ "type": "url", "url": "wss://example.com" })], "Works from computers that can reach one of these addresses."),
            (
                ["localhost", "127.0.0.1", "[::1]"].iter().map(|h| json!({ "type": "url", "url": format!("ws://{h}:4096") })).collect(),
                "Only works on this computer. Pass --url with an address other computers can reach.",
            ),
        ];
        for (reach, sentence) in cases {
            let output = add(&reach, true, 300_000, 0);
            assert!(output.contains(&format!("{sentence}{EOL}Addresses:{EOL}")), "{output}");
            for entry in &reach {
                let shown = if is_iroh(entry) { "iroh".to_string() } else { entry["url"].as_str().unwrap().to_string() };
                assert!(output.contains(&format!("  {shown}{EOL}")), "{output}");
            }
            assert_eq!(add(&reach, false, 300_000, 0), format!("ocjoin_example{EOL}"));
        }
        assert!(add(&[], true, 300_000, 0).ends_with(&format!("Addresses:{EOL}  (none){EOL}")));
    }

    #[test]
    fn expiry_rounds_minutes_and_shows_the_local_time() {
        use chrono::TimeZone;
        let expires = chrono::Local.with_ymd_and_hms(2026, 1, 1, 13, 5, 0).unwrap().timestamp_millis();
        assert_eq!(format_expiry(expires, expires - 5 * 60_000), "Expires in 5 minutes (at 01:05 PM).");
        assert_eq!(format_expiry(expires, expires - 90_000), "Expires in 2 minutes (at 01:05 PM).");
        assert_eq!(format_expiry(expires, expires - 60_000), "Expires in 1 minute (at 01:05 PM).");
        assert_eq!(format_expiry(expires, expires - 29_000), "Expires in less than a minute (at 01:05 PM).");
        assert_eq!(format_expiry(expires, expires + 1), "Expires in less than a minute (at 01:05 PM).");
        let midnight = chrono::Local.with_ymd_and_hms(2026, 1, 1, 0, 7, 0).unwrap().timestamp_millis();
        assert_eq!(format_expiry(midnight, midnight), "Expires in less than a minute (at 12:07 AM).");
    }

    #[test]
    fn list_prints_the_connection() {
        let cases = [
            (json!({ "kind": "websocket" }), "websocket"),
            (json!({ "kind": "iroh" }), "iroh"),
            (json!({ "kind": "iroh", "path": "direct", "rtt": 81.6 }), "iroh direct 82ms"),
            (json!({ "kind": "iroh", "path": "relayed", "rtt": 123 }), "iroh relayed 123ms"),
            (json!({ "kind": "iroh", "path": "direct", "rtt": 0 }), "iroh direct 0ms"),
        ];
        for (connection, label) in cases {
            let online = json!({ "id": "gpu", "paired": true, "online": true, "os": "linux", "arch": "x64", "connection": connection });
            assert_eq!(format_machine_list(&[online]), format!("gpu  paired  connected  linux/x64  {label}"));
            let offline = json!({ "id": "gpu", "paired": true, "online": false, "connection": connection });
            assert_eq!(format_machine_list(&[offline]), "gpu  paired  offline");
        }
    }

    #[test]
    fn list_formats_empty_and_populated_lists() {
        assert_eq!(format_machine_list(&[]), "No machines added.");
        assert_eq!(
            format_machine_list(&[
                json!({ "id": "gpu", "paired": true, "online": true, "os": "linux", "arch": "x64" }),
                json!({ "id": "laptop", "paired": true, "online": false }),
                json!({ "id": "worker", "paired": false, "online": false }),
            ]),
            ["gpu     paired   connected  linux/x64", "laptop  paired   offline", "worker  waiting  offline"].join(EOL)
        );
        // Only one of the platform's parts, and a connection without a platform.
        assert_eq!(
            format_machine_list(&[json!({ "id": "a", "paired": true, "online": true, "os": "darwin" })]),
            "a  paired  connected  darwin"
        );
        assert_eq!(
            format_machine_list(&[json!({ "id": "a", "paired": true, "online": true, "connection": { "kind": "websocket" } })]),
            "a  paired  connected  websocket"
        );
    }

    fn body_error(body: Value, name: Option<&str>) -> String {
        let message = body["message"].as_str().unwrap_or_default().to_string();
        format_error_body(Some(&body), &message, name)
    }

    #[test]
    fn errors_are_actionable() {
        let forbidden = body_error(json!({ "_tag": "ForbiddenError", "message": "disabled" }), None);
        assert!(forbidden.contains("OPENCODE_FLAGS=machines"));
        assert!(forbidden.contains("experimental.machines: true"));
        assert_eq!(
            forbidden,
            "Machines are experimental and disabled on this server. Turn them on with OPENCODE_FLAGS=machines or experimental.machines: true in your config."
        );
        let invalid = body_error(json!({ "_tag": "InvalidRequestError", "kind": "invalid_name", "message": "bad" }), Some("Bad"));
        assert_eq!(
            invalid,
            "\"Bad\" is not a valid machine name. Use lowercase letters, digits, and hyphens, starting with a letter, 2 to 63 characters, and not \"local\"."
        );
        assert!(body_error(json!({ "_tag": "InvalidRequestError", "kind": "invalid_name", "message": "bad" }), None).starts_with("That name is not"));
        // Any other invalid request is the server's message.
        assert_eq!(body_error(json!({ "_tag": "InvalidRequestError", "kind": "Payload", "message": "Missing key" }), Some("x")), "Missing key");
        let conflict = body_error(json!({ "_tag": "ConflictError", "resource": "gpu", "message": "paired" }), Some("gpu"));
        assert!(conflict.contains("--replace"));
        assert!(conflict.contains("stops being able to connect"));
        assert!(conflict.starts_with("Machine \"gpu\" is already paired."));
        assert!(body_error(json!({ "_tag": "ConflictError", "message": "paired" }), None).starts_with("This machine is already paired."));
        assert_eq!(body_error(json!({ "_tag": "MachineNotFoundError", "machine": "x", "message": "missing" }), Some("x")), "No machine named \"x\".");
        assert_eq!(body_error(json!({ "_tag": "MachineNotFoundError", "machine": "y", "message": "missing" }), None), "No machine named \"y\".");
        assert_eq!(format_machine_error(&Failure::Other("Could not reach server".into()), Some("x")), "Could not reach server");
    }
}

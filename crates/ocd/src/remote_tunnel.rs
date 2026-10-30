//! Remote access through OpenTunnel (`services/remote-tunnel.ts`), on the native client in `oc-tunnel`.
//!
//! OpenTunnel keeps one tunnel per device in its default profile, shared by the opentunnel CLI and every app using
//! the SDK; each claims its own routes. The service claims a subdomain (its route) rather than the tunnel hostname,
//! which the CLI may route. Tunnel hostnames appear in public certificate logs but the certificate covers subdomains
//! with a wildcard, so a random route keeps the service's address unguessable.

use std::time::Duration;

use tokio::sync::watch;

use crate::term::{EOL, write_stderr};

/// Test-only: the OpenTunnel API to use instead of `https://opentunnel.xyz` (a local fake service). Upstream has no
/// such setting.
pub const API_OVERRIDE: &str = "OCD_TEST_OPENTUNNEL_API";

const PROFILE: &str = "default";

fn client() -> oc_tunnel::Client {
    let api = std::env::var(API_OVERRIDE)
        .ok()
        .and_then(|v| url::Url::parse(&v).ok())
        .unwrap_or_else(|| url::Url::parse(oc_tunnel::DEFAULT_API).expect("api url"));
    oc_tunnel::Client::new(api, oc_tunnel::Storage::xdg())
}

/// An error's cause as `<name>: <message>`: the API error's `_tag`, or `Error` for the attach error (a plain `Error`
/// subclass) and other failures.
fn cause_text(error: &oc_tunnel::Error) -> Option<String> {
    match error {
        oc_tunnel::Error::Client { cause: Some(cause), .. } => Some(match cause {
            oc_tunnel::Cause::Api(api) => match &api.tag {
                Some(tag) => format!("{tag}: {}", api.message),
                None => format!("Error: {}", api.message),
            },
            other => format!("Error: {other}"),
        }),
        oc_tunnel::Error::Storage { cause: Some(cause), .. } => Some(format!("Error: {cause}")),
        _ => None,
    }
}

/// The error as upstream's logger formats it (`Formatter.format`): `<tag>: <message> (cause: <cause>)`.
pub fn describe(error: &oc_tunnel::Error) -> String {
    let head = format!("{}: {}", error.tag(), error.message());
    match cause_text(error) {
        Some(cause) => format!("{head} (cause: {cause})"),
        None => head,
    }
}

/// The error a command fails with, printed as Bun inspects it without the stack frames:
/// `<tag>: <message> {\n  [cause]: <cause>\n}`. `main` prints it as is, without the `Error: ` prefix.
#[derive(Debug)]
pub struct Inspected(pub String);

impl std::fmt::Display for Inspected {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for Inspected {}

pub fn inspected(error: oc_tunnel::Error) -> anyhow::Error {
    if let oc_tunnel::Error::Defect(message) = &error {
        return anyhow::anyhow!("{message}");
    }
    let head = format!("{}: {}", error.tag(), error.message());
    anyhow::Error::new(Inspected(match cause_text(&error) {
        Some(cause) => format!("{head} {{{EOL}  [cause]: {cause}{EOL}}}"),
        None => head,
    }))
}

/// Retry delays for setup failures: 1 s doubling, at most 30 s
/// (`Schedule.min([Schedule.exponential("1 second"), Schedule.spaced("30 seconds")])`).
pub fn retry_delay(attempt: u32) -> Duration {
    Duration::from_secs(2u64.saturating_pow(attempt).min(30))
}

async fn stopped(stop: &mut watch::Receiver<bool>) {
    let _ = stop.wait_for(|v| *v).await;
}

/// Holds the route for the life of the service. The client reconnects through network failures itself, so only
/// setup failures reach the retry here; a rejected token, failed certificate or unusable proxy (such as a socks5://
/// `HTTPS_PROXY`, which the bridge cannot connect through) stops it for good (`oc_tunnel::Error::is_fatal`, which
/// also matches `/unsupported proxy scheme|not a valid proxy URL/` in the message). `on_url` gets the
/// public URL while the route is attached, and `None` when it is not. Returns when `stop` turns true (closing the
/// tunnel) or after a fatal error.
pub async fn run(
    route: String,
    target: String,
    on_url: impl Fn(Option<String>) + Send,
    mut stop: watch::Receiver<bool>,
) {
    let client = client();
    let mut attempt = 0u32;
    loop {
        let routes = oc_tunnel::routes::routes([(route.clone(), target.clone())]);
        let connected = tokio::select! {
            connected = client.connect(PROFILE, routes, None) => connected,
            _ = stopped(&mut stop) => {
                on_url(None);
                return;
            }
        };
        let result = match connected {
            Ok(connection) => {
                on_url(Some(format!("https://{route}.{}", connection.tunnel.hostname)));
                tokio::select! {
                    closed = connection.closed() => closed,
                    _ = stopped(&mut stop) => {
                        connection.close().await;
                        on_url(None);
                        return;
                    }
                }
            }
            Err(e) => Err(e),
        };
        on_url(None);
        match result {
            Ok(()) => return,
            Err(error) if error.is_fatal() => {
                // Logged with the whole cause, as `Cause([Fail(...)])` (or `Die` for a defect).
                let kind = if matches!(error, oc_tunnel::Error::Defect(_)) { "Die" } else { "Fail" };
                tracing::error!(
                    cause = %format!("Cause([{kind}({})])", describe(&error)),
                    "remote access tunnel stopped; run `ocd service set remote true` to retry"
                );
                return;
            }
            Err(error) => {
                tracing::warn!(cause = %describe(&error), "remote access tunnel unavailable; retrying");
                let delay = retry_delay(attempt);
                attempt = attempt.saturating_add(1);
                tokio::select! {
                    _ = tokio::time::sleep(delay) => {}
                    _ = stopped(&mut stop) => return,
                }
            }
        }
    }
}

/// A device without a tunnel creates its shared one here, in the foreground, because issuing the certificate takes a
/// while; the service then only attaches its route on start. An interrupted issuance resumes next time.
pub async fn ensure() -> anyhow::Result<String> {
    let client = client();
    let work = async {
        if client.get(PROFILE)?.is_none() {
            write_stderr(&format!("Setting up remote access; this can take a minute...{EOL}"));
        }
        Ok::<_, oc_tunnel::Error>(client.ensure(PROFILE).await?.hostname)
    };
    match tokio::time::timeout(Duration::from_secs(5 * 60), work).await {
        Ok(result) => result.map_err(inspected),
        Err(_) => anyhow::bail!("Timed out creating the remote access tunnel; run the command again to resume"),
    }
}

/// The tunnel hostname is persisted once the certificate is ready, so this is `None` until then.
pub fn hostname(route: &str) -> anyhow::Result<Option<String>> {
    Ok(oc_tunnel::Storage::xdg()
        .load(PROFILE)
        .map_err(inspected)?
        .map(|identity| format!("{route}.{}", identity.hostname)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn retries_back_off_to_thirty_seconds() {
        let delays: Vec<u64> = (0..8).map(|a| retry_delay(a).as_secs()).collect();
        assert_eq!(delays, vec![1, 2, 4, 8, 16, 30, 30, 30]);
        assert_eq!(retry_delay(200).as_secs(), 30);
    }

    #[test]
    fn describes_client_errors() {
        let error = oc_tunnel::Error::Client {
            message: "Failed to connect tunnel: Bridge attach failed: bad_token".into(),
            cause: Some(oc_tunnel::Cause::Attach("bad_token".into())),
        };
        assert_eq!(
            describe(&error),
            "OpenTunnelClientError: Failed to connect tunnel: Bridge attach failed: bad_token (cause: Error: Bridge attach failed: bad_token)"
        );
        assert!(error.is_fatal());
        let error = oc_tunnel::Error::Client {
            message: "Failed to read certificate: ServiceUnavailableError: Down".into(),
            cause: Some(oc_tunnel::Cause::Api(oc_tunnel::ApiError {
                status: Some(503),
                tag: Some("ServiceUnavailableError".into()),
                message: "Down".into(),
            })),
        };
        assert_eq!(
            describe(&error),
            "OpenTunnelClientError: Failed to read certificate: ServiceUnavailableError: Down (cause: ServiceUnavailableError: Down)"
        );
        assert_eq!(
            inspected(error).to_string(),
            format!(
                "OpenTunnelClientError: Failed to read certificate: ServiceUnavailableError: Down {{{EOL}  [cause]: ServiceUnavailableError: Down{EOL}}}"
            )
        );
        let error = oc_tunnel::Error::Client { message: "Certificate issuance failed: no".into(), cause: None };
        assert_eq!(inspected(error).to_string(), "OpenTunnelClientError: Certificate issuance failed: no");
    }

    #[test]
    fn unusable_proxies_are_fatal_like_upstream() {
        // `/unsupported proxy scheme|not a valid proxy URL/` in the message stops the retry.
        for message in [
            "HTTPS_PROXY uses an unsupported proxy scheme 'socks5': OpenTunnel connects through http:// and https:// proxies",
            "HTTPS_PROXY is not a valid proxy URL",
        ] {
            let error = oc_tunnel::Error::Client {
                message: format!("Failed to connect tunnel: {message}"),
                cause: Some(oc_tunnel::Cause::Message(message.into())),
            };
            assert!(error.is_fatal(), "{message}");
        }
        // A network error creating the tunnel is retried.
        let error = oc_tunnel::Error::Client {
            message: "Failed to create tunnel: Transport error (POST https://opentunnel.xyz/api/tunnel)".into(),
            cause: None,
        };
        assert!(!error.is_fatal());
    }
}

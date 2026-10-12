//! Provider OAuth flows (spec §6.4). Each flow implements [`super::OAuthMethod`] so the generic attempt
//! lifecycle ([`super::Integrations::connect_oauth`]) can drive it; `authorize` can also be called directly.

use super::callback::Response;
use super::integration::{FlowError, FlowResult};
use tokio::sync::oneshot;

pub mod chatgpt;
pub mod codex;
pub mod console;
pub mod copilot;
pub mod digitalocean;
pub mod gitlab;
pub mod poe;
pub mod snowflake;
pub mod xai;

pub use chatgpt::ChatGptTokenSharing;
pub use codex::{CodexBrowser, CodexDevice};
pub use console::ConsoleDevice;
pub use copilot::CopilotDevice;
pub use digitalocean::DigitalOceanImplicit;
pub use gitlab::GitLabPkce;
pub use poe::PoeBrowser;
pub use snowflake::SnowflakeBrowser;
pub use xai::XaiDevice;

/// Device-flow polling timing. `second_ms` scales server-provided intervals (seconds); `margin_ms` is the
/// safety margin added to every wait (3 s upstream). Tests shrink both.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PollTiming {
    pub second_ms: u64,
    pub margin_ms: u64,
}

impl Default for PollTiming {
    fn default() -> Self {
        Self { second_ms: 1000, margin_ms: 3000 }
    }
}

impl PollTiming {
    pub fn fast() -> Self {
        Self { second_ms: 1, margin_ms: 0 }
    }
}

pub(crate) async fn sleep_ms(ms: u64) {
    tokio::time::sleep(std::time::Duration::from_millis(ms)).await;
}

/// A redirect received by a loopback server whose HTTP response is sent after the token exchange.
pub(crate) struct Received<T> {
    pub value: T,
    pub responder: oneshot::Sender<Response>,
}

/// Awaits a deferred loopback result; a closed server (cancelled scope) fails the attempt.
pub(crate) async fn await_deferred<T>(rx: oneshot::Receiver<FlowResult<T>>) -> FlowResult<T> {
    rx.await.unwrap_or_else(|_| Err(FlowError::new("OAuth callback server closed")))
}

/// Binds a loopback listener, mapping `EADDRINUSE` to `in_use` when given.
pub(crate) async fn bind(host: &str, port: u16, in_use: Option<String>) -> FlowResult<super::callback::Bound> {
    super::callback::bind(host, port).await.map_err(|error| match (error.kind(), in_use) {
        (std::io::ErrorKind::AddrInUse, Some(message)) => FlowError::new(message),
        _ => FlowError::new(error.to_string()),
    })
}

/// `Number.parseInt(value, 10)`: leading optional sign + digits, else `None` (NaN).
pub(crate) fn parse_int(value: &str) -> Option<i64> {
    let trimmed = value.trim_start();
    let (sign, rest) = match trimmed.strip_prefix('-') {
        Some(rest) => (-1, rest),
        None => (1, trimmed.strip_prefix('+').unwrap_or(trimmed)),
    };
    let digits: String = rest.chars().take_while(|c| c.is_ascii_digit()).collect();
    if digits.is_empty() {
        return None;
    }
    digits.parse::<i64>().ok().map(|n| n * sign)
}

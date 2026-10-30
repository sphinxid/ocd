//! Retries of a failed API request (`@opentunnel/protocol/api-retry`): capped exponential backoff with jitter.

/// First retry delay, doubling per attempt.
pub const RETRY_BASE_MS: f64 = 250.0;
/// Longest retry delay, also the cap on a `Retry-After`.
pub const RETRY_MAX_MS: f64 = 8000.0;
/// Retries after the first attempt.
pub const RETRIES: u32 = 6;

/// How often a request may be sent: reads and binding the same CSR (which answers with the current certificate)
/// may be repeated freely; anything else only when it cannot have reached the server or the server says so.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Repeat {
    Idempotent,
    Once,
}

/// `retryableStatus`: whether a response with `status` is a transient failure worth retrying.
pub fn retryable_status(status: u16, repeat: Repeat) -> bool {
    match repeat {
        Repeat::Idempotent => status == 429 || (500..=599).contains(&status),
        // Rate limited, the API's own "unavailable", or Cloudflare could not reach the server at all.
        Repeat::Once => [429, 503, 521, 522, 523, 525].contains(&status),
    }
}

/// `retryDelayMs`: the delay range in milliseconds before retry `attempt` (from 0): `RETRY_BASE_MS` doubling up to
/// `RETRY_MAX_MS`, between half of that and all of it. A `Retry-After` in seconds is followed instead, up to
/// `RETRY_MAX_MS`.
pub fn retry_delay_ms(attempt: u32, retry_after: Option<&str>) -> (f64, f64) {
    if let Some(seconds) = retry_after.map(str::trim)
        && !seconds.is_empty()
        && seconds.bytes().all(|b| b.is_ascii_digit())
    {
        // `Number(seconds) * 1000`: digits beyond f64's precision still give a large number, capped below.
        let value: f64 = seconds.parse().unwrap_or(f64::INFINITY);
        let delay = (value * 1000.0).min(RETRY_MAX_MS);
        return (delay, delay);
    }
    let delay = (RETRY_BASE_MS * 2f64.powi(attempt.min(16) as i32)).min(RETRY_MAX_MS);
    (delay / 2.0, delay)
}

/// `cloudflareReason`: what Cloudflare, which proxies the hosted API, means by its own 52x statuses.
pub fn cloudflare_reason(status: u16) -> Option<&'static str> {
    Some(match status {
        520 => "the server returned an unknown error",
        521 => "the server refused the connection",
        522 => "the connection to the server timed out",
        523 => "the server is unreachable",
        524 => "the server did not answer in time",
        525 => "the SSL handshake with the server failed",
        526 => "the server's certificate is invalid",
        527 => "the connection to the server failed",
        _ => return None,
    })
}

/// `statusMessage`: the message for an error response without an API error body.
pub fn status_message(status: u16, body: &str) -> String {
    if let Some(reason) = cloudflare_reason(status) {
        return format!("Cloudflare {status}: {reason}");
    }
    let trimmed = body.trim();
    if trimmed.is_empty() { format!("HTTP {status}") } else { trimmed.chars().take(200).collect() }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn statuses() {
        assert!(retryable_status(500, Repeat::Idempotent));
        assert!(retryable_status(599, Repeat::Idempotent));
        assert!(retryable_status(429, Repeat::Once));
        assert!(!retryable_status(500, Repeat::Once));
        assert!(retryable_status(503, Repeat::Once));
        assert!(retryable_status(525, Repeat::Once));
        assert!(!retryable_status(524, Repeat::Once));
        assert!(!retryable_status(409, Repeat::Idempotent));
    }

    #[test]
    fn delays() {
        assert_eq!(retry_delay_ms(0, None), (125.0, 250.0));
        assert_eq!(retry_delay_ms(3, None), (1000.0, 2000.0));
        assert_eq!(retry_delay_ms(5, None), (4000.0, 8000.0));
        assert_eq!(retry_delay_ms(40, None), (4000.0, 8000.0));
        assert_eq!(retry_delay_ms(0, Some(" 2 ")), (2000.0, 2000.0));
        assert_eq!(retry_delay_ms(0, Some("60")), (8000.0, 8000.0));
        assert_eq!(retry_delay_ms(1, Some("soon")), (250.0, 500.0));
        assert_eq!(retry_delay_ms(1, Some("")), (250.0, 500.0));
    }

    #[test]
    fn messages() {
        assert_eq!(status_message(522, "<html>"), "Cloudflare 522: the connection to the server timed out");
        assert_eq!(status_message(500, "  "), "HTTP 500");
        assert_eq!(status_message(500, " oops "), "oops");
        assert_eq!(status_message(500, &"é".repeat(300)).chars().count(), 200);
    }
}

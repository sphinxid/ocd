//! `GET /api/fs/read/*` response logic (spec 06 §8.5): ETag / Last-Modified / single Range.

use std::sync::LazyLock;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use axum::body::Body;
use axum::http::{HeaderMap, HeaderValue, Method, StatusCode, header};
use axum::response::{IntoResponse, Response};
use regex::Regex;
use tokio::io::{AsyncReadExt, AsyncSeekExt};

use crate::backend::{FsFile, FsSource};

/// Weak ETag `W/"<size hex>-<mtime ms hex | 0>"`.
pub fn etag(size: u64, mtime_ms: Option<i64>) -> String {
    let m = match mtime_ms {
        Some(ms) => format!("{ms:x}"),
        None => "0".into(),
    };
    format!("W/\"{size:x}-{m}\"")
}

/// `Date.toUTCString()` of an epoch-ms timestamp.
pub fn http_date(ms: i64) -> String {
    let t = if ms >= 0 { UNIX_EPOCH + Duration::from_millis(ms as u64) } else { UNIX_EPOCH };
    httpdate::fmt_http_date(t)
}

fn parse_date(s: &str) -> Option<SystemTime> {
    httpdate::parse_http_date(s.trim()).ok()
}

fn strip_weak(tag: &str) -> &str {
    let t = tag.trim();
    if t.len() >= 2 && t[..2].eq_ignore_ascii_case("w/") { &t[2..] } else { t }
}

fn header_str<'a>(h: &'a HeaderMap, name: &str) -> Option<&'a str> {
    h.get(name).and_then(|v| v.to_str().ok())
}

pub(crate) fn is_not_modified(headers: &HeaderMap, etag: &str, last_modified: Option<&str>) -> bool {
    if let Some(inm) = header_str(headers, "if-none-match") {
        return inm.split(',').any(|t| t.trim() == "*" || strip_weak(t) == strip_weak(etag));
    }
    match (header_str(headers, "if-modified-since"), last_modified) {
        (Some(ims), Some(lm)) => match (parse_date(lm), parse_date(ims)) {
            (Some(a), Some(b)) => a <= b,
            _ => false,
        },
        _ => false,
    }
}

fn matches_if_range(if_range: Option<&str>, etag: &str, last_modified: Option<&str>) -> bool {
    static QUOTED: LazyLock<Regex> = LazyLock::new(|| Regex::new(r#"(?i)^\s*(w/)?""#).expect("regex"));
    let Some(ir) = if_range else { return true };
    if QUOTED.is_match(ir) {
        return strip_weak(ir) == strip_weak(etag);
    }
    match (last_modified.and_then(parse_date), parse_date(ir)) {
        (Some(a), Some(b)) => a == b,
        _ => false,
    }
}

/// Result of parsing a `Range` header.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Range {
    /// Serve the whole file (no / malformed / multi range).
    Full,
    Partial { start: u64, end: u64 },
    Unsatisfiable,
}

/// `^bytes=\s*(\d*)\s*-\s*(\d*)\s*$` (case-insensitive) on the trimmed header.
pub fn parse_range(header: &str, size: u64) -> Range {
    static RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?i)^bytes=\s*(\d*)\s*-\s*(\d*)\s*$").expect("regex"));
    let Some(c) = RE.captures(header.trim()) else { return Range::Full };
    let (a, b) = (&c[1], &c[2]);
    if a.is_empty() && b.is_empty() {
        return Range::Full;
    }
    let num = |s: &str| s.parse::<f64>().unwrap_or(f64::INFINITY);
    if a.is_empty() {
        let suffix = num(b);
        if suffix == 0.0 || size == 0 {
            return Range::Unsatisfiable;
        }
        let start = (size as f64 - suffix).max(0.0) as u64;
        return Range::Partial { start, end: size - 1 };
    }
    let start = num(a);
    let end = if b.is_empty() { (size as f64) - 1.0 } else { num(b) };
    if start > end || start >= size as f64 {
        return Range::Unsatisfiable;
    }
    Range::Partial { start: start as u64, end: end.min(size as f64 - 1.0) as u64 }
}

/// Serve a file with validators and range support.
pub async fn serve(method: &Method, headers: &HeaderMap, file: FsFile) -> Response {
    let etag = etag(file.size, file.mtime_ms);
    let last_modified = file.mtime_ms.map(http_date);
    let validators = |h: &mut HeaderMap| {
        h.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-cache, no-transform"));
        if let Ok(v) = HeaderValue::from_str(&etag) {
            h.insert(header::ETAG, v);
        }
        if let Some(lm) = &last_modified
            && let Ok(v) = HeaderValue::from_str(lm)
        {
            h.insert(header::LAST_MODIFIED, v);
        }
    };
    if is_not_modified(headers, &etag, last_modified.as_deref()) {
        let mut res = StatusCode::NOT_MODIFIED.into_response();
        validators(res.headers_mut());
        return res;
    }
    let range_header = if method == Method::GET { header_str(headers, "range") } else { None };
    let range = match range_header {
        Some(r) if matches_if_range(header_str(headers, "if-range"), &etag, last_modified.as_deref()) => parse_range(r, file.size),
        _ => Range::Full,
    };
    if range == Range::Unsatisfiable {
        let mut res = StatusCode::RANGE_NOT_SATISFIABLE.into_response();
        let h = res.headers_mut();
        h.insert(header::ACCEPT_RANGES, HeaderValue::from_static("bytes"));
        if let Ok(v) = HeaderValue::from_str(&format!("bytes */{}", file.size)) {
            h.insert(header::CONTENT_RANGE, v);
        }
        return res;
    }
    let (status, offset, length) = match range {
        Range::Partial { start, end } => (StatusCode::PARTIAL_CONTENT, start, end - start + 1),
        _ => (StatusCode::OK, 0, file.size),
    };
    let body = match body(&file.source, offset, length).await {
        Ok(b) => b,
        Err(e) => {
            tracing::error!(error = %e, "fs.read failed to open file");
            return StatusCode::INTERNAL_SERVER_ERROR.into_response();
        }
    };
    let mut res = (status, body).into_response();
    let h = res.headers_mut();
    validators(h);
    h.insert(header::CONTENT_LENGTH, HeaderValue::from(length));
    h.insert(header::ACCEPT_RANGES, HeaderValue::from_static("bytes"));
    if let Ok(v) = HeaderValue::from_str(&file.mime) {
        h.insert(header::CONTENT_TYPE, v);
    }
    if let Range::Partial { start, end } = range
        && let Ok(v) = HeaderValue::from_str(&format!("bytes {start}-{end}/{}", file.size))
    {
        h.insert(header::CONTENT_RANGE, v);
    }
    res
}

async fn body(source: &FsSource, offset: u64, length: u64) -> std::io::Result<Body> {
    if length == 0 {
        return Ok(Body::empty());
    }
    match source {
        FsSource::Bytes(b) => {
            let start = (offset as usize).min(b.len());
            let end = (start + length as usize).min(b.len());
            Ok(Body::from(b.slice(start..end)))
        }
        FsSource::Path(p) => {
            let mut f = tokio::fs::File::open(p).await?;
            f.seek(std::io::SeekFrom::Start(offset)).await?;
            let reader = f.take(length);
            Ok(Body::from_stream(tokio_util_stream(reader)))
        }
    }
}

fn tokio_util_stream<R: tokio::io::AsyncRead + Send + Unpin + 'static>(
    reader: R,
) -> impl futures::Stream<Item = std::io::Result<bytes::Bytes>> + Send {
    futures::stream::unfold(reader, |mut r| async move {
        let mut buf = vec![0u8; 64 * 1024];
        match r.read(&mut buf).await {
            Ok(0) => None,
            Ok(n) => {
                buf.truncate(n);
                Some((Ok(bytes::Bytes::from(buf)), r))
            }
            Err(e) => Some((Err(e), r)),
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ranges() {
        assert_eq!(parse_range("bytes=0-4", 10), Range::Partial { start: 0, end: 4 });
        assert_eq!(parse_range("bytes=5-", 10), Range::Partial { start: 5, end: 9 });
        assert_eq!(parse_range("bytes=-3", 10), Range::Partial { start: 7, end: 9 });
        assert_eq!(parse_range("bytes=-30", 10), Range::Partial { start: 0, end: 9 });
        assert_eq!(parse_range("bytes=2-100", 10), Range::Partial { start: 2, end: 9 });
        assert_eq!(parse_range("bytes=-0", 10), Range::Unsatisfiable);
        assert_eq!(parse_range("bytes=10-", 10), Range::Unsatisfiable);
        assert_eq!(parse_range("bytes=5-2", 10), Range::Unsatisfiable);
        assert_eq!(parse_range("bytes=-1", 0), Range::Unsatisfiable);
        assert_eq!(parse_range("bytes=0-1,3-4", 10), Range::Full);
        assert_eq!(parse_range("bytes=-", 10), Range::Full);
        assert_eq!(parse_range("items=0-1", 10), Range::Full);
        assert_eq!(parse_range("  BYTES= 1 - 2 ", 10), Range::Partial { start: 1, end: 2 });
    }

    #[test]
    fn etags_and_conditionals() {
        assert_eq!(etag(255, Some(4096)), "W/\"ff-1000\"");
        assert_eq!(etag(0, None), "W/\"0-0\"");
        let e = etag(10, Some(1000));
        let lm = http_date(1_760_000_000_000);
        assert_eq!(lm, "Thu, 09 Oct 2025 08:53:20 GMT");
        let mut h = HeaderMap::new();
        h.insert("if-none-match", HeaderValue::from_str(&format!("\"x\", {}", &e[2..])).unwrap());
        assert!(is_not_modified(&h, &e, Some(&lm)));
        let mut h = HeaderMap::new();
        h.insert("if-modified-since", HeaderValue::from_str(&lm).unwrap());
        assert!(is_not_modified(&h, &e, Some(&lm)));
        assert!(matches_if_range(Some(&e), &e, Some(&lm)));
        assert!(matches_if_range(Some(&lm), &e, Some(&lm)));
        assert!(!matches_if_range(Some("\"other\""), &e, Some(&lm)));
    }
}

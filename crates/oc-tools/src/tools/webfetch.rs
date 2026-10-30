//! `webfetch` (`core/src/tool/plugin/webfetch.ts`, `core/src/tool/http-body.ts`).

use std::time::Duration;

use async_trait::async_trait;
use futures::StreamExt;
use serde::Deserialize;
use serde_json::{Value, json};

use futures::future::BoxFuture;

use crate::host::PermissionError;
use crate::html;
use crate::tool::{DefinitionContext, ErrorCause, Tool, ToolContext, ToolDefinition, ToolError, ToolOptions, ToolResult, decode, meta};

pub const MAX_RESPONSE_BYTES: usize = 5 * 1024 * 1024;
pub const DEFAULT_TIMEOUT_SECONDS: f64 = 30.0;
pub const DESCRIPTION: &str = "Fetch content from an HTTP or HTTPS URL and return it as text, markdown, or HTML. Markdown is the default.\n\nUse a more targeted tool when one is available. This tool is read-only. Large text results may be replaced with a preview while the complete output is retained in managed storage.";
const USER_AGENT: &str = "Mozilla/5.0 AppleWebKit/537.36 (KHTML, like Gecko); compatible; OpenCode-User/1.0; +https://opencode.ai";

pub fn accept_header(format: &str) -> &'static str {
    match format {
        "text" => "text/plain;q=1.0, text/markdown;q=0.9, text/html;q=0.8, */*;q=0.1",
        "html" => "text/html;q=1.0, application/xhtml+xml;q=0.9, text/plain;q=0.8, text/markdown;q=0.7, */*;q=0.1",
        _ => "text/markdown;q=1.0, text/x-markdown;q=0.9, text/plain;q=0.8, text/html;q=0.7, */*;q=0.1",
    }
}

pub fn mime_from(content_type: &str) -> String {
    content_type.split(';').next().unwrap_or("").trim().to_lowercase()
}

pub fn is_image_attachment(mime: &str) -> bool {
    mime.starts_with("image/") && mime != "image/svg+xml" && mime != "image/vnd.fastbidsheet"
}

pub fn is_textual_mime(mime: &str) -> bool {
    crate::mime::is_textual(mime)
}

/// Convert by format when the content type is HTML.
pub fn convert(content: &str, content_type: &str, format: &str) -> String {
    if !content_type.contains("text/html") {
        return content.to_string();
    }
    match format {
        "markdown" => html::convert_html_to_markdown(content),
        "text" => html::extract_text_from_html(content),
        _ => content.to_string(),
    }
}

#[derive(Deserialize)]
struct Input {
    url: String,
    format: Option<String>,
    timeout: Option<f64>,
}

pub struct WebFetchTool {
    http: reqwest::Client,
}

/// `b3`/`traceparent` of a new client span (upstream fetches through Effect's traced HTTP client).
fn trace_headers() -> reqwest::header::HeaderMap {
    let mut map = reqwest::header::HeaderMap::new();
    for (k, v) in oc_util::trace::headers() {
        if let Ok(v) = reqwest::header::HeaderValue::from_str(&v) {
            map.insert(k, v);
        }
    }
    map
}

/// Redirects followed before a fetch fails (`MAX_REDIRECTS`).
pub const MAX_REDIRECTS: usize = 10;
const REDIRECT_STATUSES: [u16; 5] = [301, 302, 303, 307, 308];

/// A fetch failure: a message, or a redirect target the permission rules deny.
#[derive(Debug)]
enum FetchError {
    Message(String),
    Permission(PermissionError),
}

impl From<String> for FetchError {
    fn from(message: String) -> Self {
        FetchError::Message(message)
    }
}

/// Checks a redirect target against the permission rules (`assertNotDenied`).
type RedirectCheck<'a> = &'a (dyn Fn(String) -> BoxFuture<'static, Result<(), PermissionError>> + Send + Sync);

impl WebFetchTool {
    /// Redirects are followed by the tool, never by the client, so each new URL is checked
    /// against the permission rules first.
    pub fn new() -> Self {
        let http = oc_util::http::client_builder()
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .unwrap_or_default();
        Self { http }
    }

    async fn request(&self, url: &str, format: &str, agent: &str) -> Result<reqwest::Response, String> {
        let response = self
            .http
            .get(url)
            .header("User-Agent", agent)
            .header("Accept", accept_header(format))
            .header("Accept-Language", "en-US,en;q=0.9")
            .headers(trace_headers())
            .send()
            .await
            .map_err(|_| format!("Transport error (GET {url})"))?;
        Ok(response)
    }

    /// `execute`: GET `url`, following up to [`MAX_REDIRECTS`] redirects whose targets the
    /// permission rules don't deny; a non-2xx final response is a status error.
    async fn execute(
        &self,
        check: RedirectCheck<'_>,
        url: &str,
        format: &str,
        agent: &str,
    ) -> Result<reqwest::Response, (FetchError, bool)> {
        let mut current = url.to_string();
        let mut redirects = 0;
        loop {
            let response = self.request(&current, format, agent).await.map_err(|m| (m.into(), false))?;
            let status = response.status().as_u16();
            let location = response.headers().get("location").and_then(|v| v.to_str().ok()).map(str::to_string);
            let Some(location) = location.filter(|_| REDIRECT_STATUSES.contains(&status)) else {
                if !response.status().is_success() {
                    let challenge = status == 403
                        && response.headers().get("cf-mitigated").and_then(|v| v.to_str().ok()) == Some("challenge");
                    let message = format!("StatusCode: non 2xx status code ({status} GET {current})");
                    return Err((message.into(), challenge));
                }
                return Ok(response);
            };
            if redirects >= MAX_REDIRECTS {
                return Err((format!("Too many redirects (more than {MAX_REDIRECTS})").into(), false));
            }
            let next = match url::Url::parse(&current).and_then(|base| base.join(&location)) {
                Ok(next) if next.scheme() == "http" || next.scheme() == "https" => next.to_string(),
                Ok(_) => return Err(("URL must use http:// or https://".to_string().into(), false)),
                Err(_) => {
                    let message = format!("{} cannot be parsed as a URL.", crate::util::js::quote(&location));
                    return Err((message.into(), false));
                }
            };
            check(next.clone()).await.map_err(|error| (FetchError::Permission(error), false))?;
            current = next;
            redirects += 1;
        }
    }

    async fn fetch(&self, check: RedirectCheck<'_>, url: &str, format: &str) -> Result<(Vec<u8>, String), FetchError> {
        let response = match self.execute(check, url, format, USER_AGENT).await {
            Ok(response) => response,
            Err((_, true)) => self.execute(check, url, format, "opencode").await.map_err(|(error, _)| error)?,
            Err((error, false)) => return Err(error),
        };
        let content_type = response.headers().get("content-type").and_then(|v| v.to_str().ok()).unwrap_or("").to_string();
        let mime = mime_from(&content_type);
        if is_image_attachment(&mime) {
            return Err(format!("Unsupported fetched image content type: {mime}").into());
        }
        if !is_textual_mime(&mime) {
            return Err(format!("Unsupported fetched file content type: {mime}").into());
        }
        let too_large = || format!("Response too large (exceeds {MAX_RESPONSE_BYTES} byte limit)");
        if let Some(length) = response.headers().get("content-length").and_then(|v| v.to_str().ok()).and_then(|v| v.trim().parse::<u64>().ok()) {
            if length > MAX_RESPONSE_BYTES as u64 {
                return Err(too_large().into());
            }
        }
        let mut body = Vec::new();
        let mut stream = response.bytes_stream();
        while let Some(chunk) = stream.next().await {
            let chunk = chunk.map_err(|_| format!("Transport error (GET {url})"))?;
            if body.len() + chunk.len() > MAX_RESPONSE_BYTES {
                return Err(too_large().into());
            }
            body.extend_from_slice(&chunk);
        }
        Ok((body, content_type))
    }
}

#[async_trait]
impl Tool for WebFetchTool {
    fn name(&self) -> &str {
        "webfetch"
    }

    fn options(&self) -> ToolOptions {
        ToolOptions::direct()
    }

    fn definition(&self, _ctx: &DefinitionContext) -> ToolDefinition {
        super::definition("webfetch", DESCRIPTION, include_str!("../../schemas/webfetch.json"))
    }

    async fn execute(&self, raw: Value, ctx: ToolContext) -> Result<ToolResult, ToolError> {
        let input: Input = decode(raw.clone())?;
        let format = input.format.clone().unwrap_or_else(|| "markdown".into());
        let leaf = format!("Unable to fetch {}", input.url);
        let fail = |m: String| ToolError::with_cause(leaf.clone(), ErrorCause::Error(m));
        match url::Url::parse(&input.url) {
            Ok(u) if u.scheme() == "http" || u.scheme() == "https" => {}
            Ok(_) => return Err(fail("URL must use http:// or https://".into())),
            Err(_) => return Err(fail(format!("{} cannot be parsed as a URL.", crate::util::js::quote(&input.url)))),
        }
        let mut metadata = meta(vec![("url", json!(input.url)), ("format", json!(format))]);
        if let Some(t) = input.timeout {
            metadata.insert("timeout".into(), json!(t));
        }
        super::assert(&ctx, "webfetch", vec![input.url.clone()], Some(vec!["*".into()]), Some(metadata.clone()))
            .await
            .map_err(super::perm(leaf.clone()))?;
        // Redirect targets only fail on a deny; they never ask.
        let check = |url: String| -> BoxFuture<'static, Result<(), PermissionError>> {
            let mut metadata = metadata.clone();
            metadata.insert("url".into(), json!(url));
            let request = ctx.permission("webfetch", vec![url], None, Some(metadata));
            let host = ctx.host.clone();
            Box::pin(async move { host.check_permission(request).await })
        };
        let seconds = input.timeout.unwrap_or(DEFAULT_TIMEOUT_SECONDS);
        let fetched = tokio::time::timeout(Duration::from_secs_f64(seconds), self.fetch(&check, &input.url, &format)).await;
        let (body, content_type) = match fetched {
            Err(_) => return Err(fail("Request timed out".into())),
            Ok(Err(FetchError::Message(message))) => return Err(fail(message)),
            Ok(Err(FetchError::Permission(error))) => return Err(super::perm(leaf.clone())(error)),
            Ok(Ok(result)) => result,
        };
        let content = String::from_utf8_lossy(&body);
        let content = content.strip_prefix('\u{FEFF}').unwrap_or(&content);
        let output = convert(content, &content_type, &format);
        Ok(ToolResult::text(output.clone())
            .with_output(json!({"url": input.url, "contentType": content_type, "format": format, "output": output}))
            .with_metadata(meta(vec![("contentType", json!(content_type))])))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mime_rules() {
        assert_eq!(mime_from("Text/HTML; charset=utf-8"), "text/html");
        assert!(is_image_attachment("image/png"));
        assert!(!is_image_attachment("image/svg+xml"));
        assert!(is_textual_mime(""));
        assert!(is_textual_mime("application/ld+json"));
        assert!(!is_textual_mime("application/pdf"));
        assert_eq!(convert("<p>hi</p>", "text/plain", "markdown"), "<p>hi</p>");
        assert_eq!(convert("<p>hi <b>x</b></p>", "text/html", "markdown"), "hi **x**");
        assert_eq!(convert("<p>hi<script>x</script></p>", "text/html", "text"), "hi");
    }

    #[tokio::test]
    async fn fetches_from_local_server() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            for _ in 0..3 {
                let (mut socket, _) = listener.accept().await.unwrap();
                let mut buf = vec![0u8; 4096];
                let n = socket.read(&mut buf).await.unwrap();
                let request = String::from_utf8_lossy(&buf[..n]).to_string();
                let response = if request.starts_with("GET /html") {
                    "HTTP/1.1 200 OK\r\ncontent-type: text/html\r\ncontent-length: 17\r\nconnection: close\r\n\r\n<h1>Title</h1>   "
                } else if request.starts_with("GET /img") {
                    "HTTP/1.1 200 OK\r\ncontent-type: image/png\r\ncontent-length: 0\r\nconnection: close\r\n\r\n"
                } else {
                    "HTTP/1.1 404 Not Found\r\ncontent-length: 0\r\nconnection: close\r\n\r\n"
                };
                socket.write_all(response.as_bytes()).await.unwrap();
            }
        });
        let tool = WebFetchTool::new();
        let allow = |_: String| -> BoxFuture<'static, Result<(), PermissionError>> { Box::pin(async { Ok(()) }) };
        let (body, ct) = tool.fetch(&allow, &format!("http://{addr}/html"), "markdown").await.unwrap();
        assert_eq!(convert(&String::from_utf8_lossy(&body), &ct, "markdown"), "# Title");
        let image = tool.fetch(&allow, &format!("http://{addr}/img"), "markdown").await.unwrap_err();
        assert_eq!(message(image), "Unsupported fetched image content type: image/png");
        let url = format!("http://{addr}/missing");
        let missing = tool.fetch(&allow, &url, "markdown").await.unwrap_err();
        assert_eq!(message(missing), format!("StatusCode: non 2xx status code (404 GET {url})"));
    }

    fn message(error: FetchError) -> String {
        match error {
            FetchError::Message(message) => message,
            FetchError::Permission(error) => format!("{error:?}"),
        }
    }

    /// A local server answering each request with `respond(path)`; returns its address and the
    /// paths it was asked for.
    async fn serve(
        respond: impl Fn(&str) -> String + Send + Sync + 'static,
    ) -> (std::net::SocketAddr, std::sync::Arc<std::sync::Mutex<Vec<String>>>) {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let seen = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let paths = seen.clone();
        tokio::spawn(async move {
            loop {
                let Ok((mut socket, _)) = listener.accept().await else { return };
                let mut buf = vec![0u8; 4096];
                let n = socket.read(&mut buf).await.unwrap_or(0);
                let request = String::from_utf8_lossy(&buf[..n]).to_string();
                let path = request.split(' ').nth(1).unwrap_or("").to_string();
                paths.lock().unwrap().push(path.clone());
                let _ = socket.write_all(respond(&path).as_bytes()).await;
            }
        });
        (addr, seen)
    }

    fn redirect(status: u16, location: &str) -> String {
        format!("HTTP/1.1 {status} Moved\r\nlocation: {location}\r\ncontent-length: 0\r\nconnection: close\r\n\r\n")
    }

    // tool-webfetch.test.ts "follows redirects, checking each target against the rules without asking"
    #[tokio::test]
    async fn follows_redirects_checking_each_target() {
        let (addr, seen) = serve(|path| {
            if path == "/start" {
                redirect(302, "/target")
            } else {
                "HTTP/1.1 200 OK\r\ncontent-type: text/plain\r\ncontent-length: 2\r\nconnection: close\r\n\r\nok".into()
            }
        })
        .await;
        let checks = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let recorded = checks.clone();
        let check = move |url: String| -> BoxFuture<'static, Result<(), PermissionError>> {
            recorded.lock().unwrap().push(url);
            Box::pin(async { Ok(()) })
        };
        let tool = WebFetchTool::new();
        let (body, _) = tool.fetch(&check, &format!("http://{addr}/start"), "text").await.unwrap();
        assert_eq!(body, b"ok");
        assert_eq!(*checks.lock().unwrap(), vec![format!("http://{addr}/target")]);
        assert_eq!(*seen.lock().unwrap(), vec!["/start", "/target"]);
    }

    // tool-webfetch.test.ts "stops at a redirect target the rules deny"
    #[tokio::test]
    async fn stops_at_a_redirect_target_the_rules_deny() {
        let target = "http://169.254.169.254/latest/meta-data";
        let (addr, seen) = serve(move |_| redirect(302, target)).await;
        let deny = |_: String| -> BoxFuture<'static, Result<(), PermissionError>> {
            Box::pin(async { Err(PermissionError::Blocked { message: "Permission denied: webfetch".into() }) })
        };
        let error = WebFetchTool::new().fetch(&deny, &format!("http://{addr}/start"), "text").await.unwrap_err();
        assert!(matches!(error, FetchError::Permission(PermissionError::Blocked { ref message }) if message == "Permission denied: webfetch"));
        assert_eq!(*seen.lock().unwrap(), vec!["/start"]);
    }

    // tool-webfetch.test.ts "rejects redirects to non-HTTP schemes and endless redirect chains"
    #[tokio::test]
    async fn rejects_non_http_redirects_and_endless_chains() {
        let checks = std::sync::Arc::new(std::sync::Mutex::new(0));
        let counted = checks.clone();
        let check = move |_: String| -> BoxFuture<'static, Result<(), PermissionError>> {
            *counted.lock().unwrap() += 1;
            Box::pin(async { Ok(()) })
        };
        let (addr, _) = serve(|_| redirect(301, "file:///etc/passwd")).await;
        let error = WebFetchTool::new().fetch(&check, &format!("http://{addr}/a"), "text").await.unwrap_err();
        assert_eq!(message(error), "URL must use http:// or https://");
        assert_eq!(*checks.lock().unwrap(), 0);

        let (addr, seen) = serve(|path| redirect(307, &format!("{path}x"))).await;
        let error = WebFetchTool::new().fetch(&check, &format!("http://{addr}/a"), "text").await.unwrap_err();
        assert_eq!(message(error), format!("Too many redirects (more than {MAX_REDIRECTS})"));
        assert_eq!(seen.lock().unwrap().len(), MAX_REDIRECTS + 1);
    }
}

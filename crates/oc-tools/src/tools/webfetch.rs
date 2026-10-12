//! `webfetch` (`core/src/tool/plugin/webfetch.ts`, `core/src/tool/http-body.ts`).

use std::time::Duration;

use async_trait::async_trait;
use futures::StreamExt;
use serde::Deserialize;
use serde_json::{Value, json};

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
    mime.is_empty()
        || mime.starts_with("text/")
        || mime == "application/json"
        || mime.ends_with("+json")
        || mime == "application/xml"
        || mime.ends_with("+xml")
        || mime == "application/javascript"
        || mime == "application/x-javascript"
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

impl WebFetchTool {
    pub fn new(http: reqwest::Client) -> Self {
        Self { http }
    }

    async fn request(&self, url: &str, format: &str, agent: &str) -> Result<reqwest::Response, String> {
        let response = self
            .http
            .get(url)
            .header("User-Agent", agent)
            .header("Accept", accept_header(format))
            .header("Accept-Language", "en-US,en;q=0.9")
            .send()
            .await
            .map_err(|_| format!("Transport error (GET {url})"))?;
        Ok(response)
    }

    async fn fetch(&self, url: &str, format: &str) -> Result<(Vec<u8>, String), String> {
        let mut response = self.request(url, format, USER_AGENT).await?;
        if response.status().as_u16() == 403
            && response.headers().get("cf-mitigated").and_then(|v| v.to_str().ok()) == Some("challenge")
        {
            response = self.request(url, format, "opencode").await?;
        }
        let status = response.status();
        if !status.is_success() {
            return Err(format!("StatusCode: non 2xx status code ({} GET {url})", status.as_u16()));
        }
        let content_type = response.headers().get("content-type").and_then(|v| v.to_str().ok()).unwrap_or("").to_string();
        let mime = mime_from(&content_type);
        if is_image_attachment(&mime) {
            return Err(format!("Unsupported fetched image content type: {mime}"));
        }
        if !is_textual_mime(&mime) {
            return Err(format!("Unsupported fetched file content type: {mime}"));
        }
        let too_large = || format!("Response too large (exceeds {MAX_RESPONSE_BYTES} byte limit)");
        if let Some(length) = response.headers().get("content-length").and_then(|v| v.to_str().ok()).and_then(|v| v.trim().parse::<u64>().ok()) {
            if length > MAX_RESPONSE_BYTES as u64 {
                return Err(too_large());
            }
        }
        let mut body = Vec::new();
        let mut stream = response.bytes_stream();
        while let Some(chunk) = stream.next().await {
            let chunk = chunk.map_err(|_| format!("Transport error (GET {url})"))?;
            if body.len() + chunk.len() > MAX_RESPONSE_BYTES {
                return Err(too_large());
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
        super::assert(&ctx, "webfetch", vec![input.url.clone()], Some(vec!["*".into()]), Some(metadata))
            .await
            .map_err(super::perm(leaf.clone()))?;
        let seconds = input.timeout.unwrap_or(DEFAULT_TIMEOUT_SECONDS);
        let (body, content_type) = match tokio::time::timeout(Duration::from_secs_f64(seconds), self.fetch(&input.url, &format)).await {
            Err(_) => return Err(fail("Request timed out".into())),
            Ok(Err(message)) => return Err(fail(message)),
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
        let tool = WebFetchTool::new(reqwest::Client::new());
        let (body, ct) = tool.fetch(&format!("http://{addr}/html"), "markdown").await.unwrap();
        assert_eq!(convert(&String::from_utf8_lossy(&body), &ct, "markdown"), "# Title");
        assert_eq!(tool.fetch(&format!("http://{addr}/img"), "markdown").await.unwrap_err(), "Unsupported fetched image content type: image/png");
        let url = format!("http://{addr}/missing");
        assert_eq!(tool.fetch(&url, "markdown").await.unwrap_err(), format!("StatusCode: non 2xx status code (404 GET {url})"));
    }
}

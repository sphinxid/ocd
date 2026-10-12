//! End-to-end tests against an axum-based fake Streamable HTTP MCP server that also acts as the
//! OAuth protected resource and authorization server.

use std::sync::Arc;
use std::time::Duration;

use axum::Router;
use axum::body::Body;
use axum::extract::{Form, State};
use axum::http::{HeaderMap, Method, Response, StatusCode, Uri};
use axum::routing::{get, post};
use indexmap::IndexMap;
use oc_mcp::{
    CallToolContent, ClientInfo, ManagerOptions, McpCredentialStore, McpManager, MemoryCredentialStore, OAuthSetting,
    RemoteConfig, ServerConfig, Status, TimeoutConfig,
};
use parking_lot::Mutex;
use serde_json::{Value, json};

#[derive(Default)]
struct Fake {
    port: u16,
    sessions: Vec<String>,
    next_session: u32,
    requests: Vec<(String, HeaderMap, Value)>,
    deleted: Vec<String>,
    reject_codemode_param: bool,
    require_token: Option<String>,
    registrations: Vec<Value>,
    token_requests: Vec<Vec<(String, String)>>,
    issue_token: String,
}

type Shared = Arc<Mutex<Fake>>;

fn json_response(status: u16, body: Value, extra: &[(&str, String)]) -> Response<Body> {
    let mut builder = Response::builder().status(status).header("content-type", "application/json");
    for (k, v) in extra {
        builder = builder.header(*k, v);
    }
    builder.body(Body::from(body.to_string())).unwrap()
}

async fn mcp_post(State(fake): State<Shared>, uri: Uri, headers: HeaderMap, body: String) -> Response<Body> {
    let message: Value = serde_json::from_str(&body).unwrap();
    let method = message.get("method").and_then(Value::as_str).unwrap_or("").to_string();
    let mut fake = fake.lock();
    fake.requests.push((uri.to_string(), headers.clone(), message.clone()));
    if let Some(token) = fake.require_token.clone() {
        let auth = headers.get("authorization").and_then(|v| v.to_str().ok()).unwrap_or("");
        if auth != format!("Bearer {token}") {
            let port = fake.port;
            return Response::builder()
                .status(401)
                .header(
                    "www-authenticate",
                    format!(r#"Bearer resource_metadata="http://127.0.0.1:{port}/.well-known/oauth-protected-resource/mcp", scope="mcp.read""#),
                )
                .body(Body::from("unauthorized"))
                .unwrap();
        }
    }
    if fake.reject_codemode_param && uri.query().is_some_and(|q| q.contains("codemode")) {
        return Response::builder().status(404).body(Body::from("unknown param")).unwrap();
    }
    let id = message.get("id").cloned();
    if method == "initialize" {
        fake.next_session += 1;
        let session = format!("sess-{}", fake.next_session);
        fake.sessions.push(session.clone());
        return json_response(
            200,
            json!({"jsonrpc": "2.0", "id": id, "result": {
                "protocolVersion": "2025-11-25",
                "capabilities": {"tools": {}},
                "serverInfo": {"name": "fake-http", "version": "1"}
            }}),
            &[("mcp-session-id", session)],
        );
    }
    let session = headers.get("mcp-session-id").and_then(|v| v.to_str().ok()).unwrap_or("").to_string();
    if !fake.sessions.contains(&session) {
        return Response::builder().status(404).body(Body::from("Session not found")).unwrap();
    }
    if id.is_none() {
        return Response::builder().status(202).body(Body::empty()).unwrap();
    }
    match method.as_str() {
        "tools/list" => json_response(
            200,
            json!({"jsonrpc": "2.0", "id": id, "result": {"tools": [{"name": "hello", "inputSchema": {"type": "object"}}]}}),
            &[],
        ),
        "tools/call" => {
            let result = json!({"jsonrpc": "2.0", "id": id, "result": {"content": [{"type": "text", "text": format!("hi {}", message["params"]["arguments"]["name"].as_str().unwrap_or(""))}]}});
            let progress = json!({"jsonrpc": "2.0", "method": "notifications/message", "params": {"level": "info", "data": "x"}});
            Response::builder()
                .status(200)
                .header("content-type", "text/event-stream")
                .body(Body::from(format!("event: message\ndata: {progress}\n\nid: e1\nevent: message\ndata: {result}\n\n")))
                .unwrap()
        }
        _ => json_response(200, json!({"jsonrpc": "2.0", "id": id, "error": {"code": -32601, "message": "Method not found"}}), &[]),
    }
}

async fn mcp_get() -> Response<Body> {
    Response::builder().status(405).body(Body::empty()).unwrap()
}

async fn mcp_delete(State(fake): State<Shared>, headers: HeaderMap) -> Response<Body> {
    let session = headers.get("mcp-session-id").and_then(|v| v.to_str().ok()).unwrap_or("").to_string();
    let mut fake = fake.lock();
    fake.sessions.retain(|s| s != &session);
    fake.deleted.push(session);
    Response::builder().status(200).body(Body::empty()).unwrap()
}

async fn prm(State(fake): State<Shared>) -> Response<Body> {
    let port = fake.lock().port;
    json_response(
        200,
        json!({"resource": format!("http://127.0.0.1:{port}/mcp"), "authorization_servers": [format!("http://127.0.0.1:{port}")], "scopes_supported": ["mcp.read"]}),
        &[],
    )
}

async fn as_metadata(State(fake): State<Shared>) -> Response<Body> {
    let port = fake.lock().port;
    let base = format!("http://127.0.0.1:{port}");
    json_response(
        200,
        json!({
            "issuer": base,
            "authorization_endpoint": format!("{base}/authorize"),
            "token_endpoint": format!("{base}/token"),
            "registration_endpoint": format!("{base}/register"),
            "response_types_supported": ["code"],
            "code_challenge_methods_supported": ["S256"],
            "token_endpoint_auth_methods_supported": ["none"]
        }),
        &[],
    )
}

async fn register(State(fake): State<Shared>, body: String) -> Response<Body> {
    let body: Value = serde_json::from_str(&body).unwrap();
    fake.lock().registrations.push(body.clone());
    let mut out = body.as_object().cloned().unwrap();
    out.insert("client_id".into(), json!("dyn-client"));
    json_response(201, Value::Object(out), &[])
}

async fn token(State(fake): State<Shared>, Form(form): Form<Vec<(String, String)>>) -> Response<Body> {
    let mut fake = fake.lock();
    fake.token_requests.push(form.clone());
    let grant = form.iter().find(|(k, _)| k == "grant_type").map(|(_, v)| v.clone()).unwrap_or_default();
    let ok = match grant.as_str() {
        "authorization_code" => form.iter().any(|(k, v)| k == "code" && v == "abc"),
        "refresh_token" => form.iter().any(|(k, v)| k == "refresh_token" && v == "ref-1"),
        _ => false,
    };
    if !ok {
        return json_response(400, json!({"error": "invalid_grant", "error_description": "bad grant"}), &[]);
    }
    json_response(
        200,
        json!({"access_token": fake.issue_token, "token_type": "Bearer", "expires_in": 3600, "refresh_token": "ref-2", "scope": "mcp.read"}),
        &[],
    )
}

async fn start_fake(configure: impl FnOnce(&mut Fake)) -> (Shared, u16) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let mut fake = Fake { port, issue_token: "tok-1".into(), ..Default::default() };
    configure(&mut fake);
    let shared: Shared = Arc::new(Mutex::new(fake));
    let app = Router::new()
        .route("/mcp", post(mcp_post).get(mcp_get).delete(mcp_delete))
        .route("/.well-known/oauth-protected-resource/mcp", get(prm))
        .route("/.well-known/oauth-authorization-server", get(as_metadata))
        .route("/register", post(register))
        .route("/token", post(token))
        .with_state(shared.clone());
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (shared, port)
}

fn remote(port: u16, oauth: Option<OAuthSetting>) -> ServerConfig {
    let mut headers = IndexMap::new();
    headers.insert("X-Custom".to_string(), "yes".to_string());
    ServerConfig::Remote(RemoteConfig {
        url: format!("http://127.0.0.1:{port}/mcp"),
        headers: Some(headers),
        oauth,
        disabled: None,
        codemode: None,
        timeout: Some(TimeoutConfig { startup: Some(10_000), catalog: None, execution: None }),
        protocol: None,
    })
}

fn manager(credentials: Option<Arc<dyn McpCredentialStore>>) -> McpManager {
    McpManager::new(ManagerOptions {
        directory: std::env::temp_dir(),
        client_info: ClientInfo { name: "opencode".into(), version: "test".into() },
        credentials,
        elicitation: None,
    })
}

async fn load(manager: &McpManager, name: &str, config: ServerConfig) {
    let mut servers = IndexMap::new();
    servers.insert(name.to_string(), config);
    manager.set_servers(servers).await;
    manager.wait_ready(name).await.unwrap();
}

fn header(headers: &HeaderMap, name: &str) -> Option<String> {
    headers.get(name).and_then(|v| v.to_str().ok()).map(String::from)
}

#[tokio::test]
async fn http_handshake_sse_session_and_expiry() {
    let (fake, port) = start_fake(|_| {}).await;
    let manager = manager(None);
    load(&manager, "web", remote(port, None)).await;
    assert_eq!(manager.status("web"), Some(Status::Connected), "{:?}", manager.servers());
    let info = &manager.servers()[0];
    assert!(info.integration_id.as_deref().unwrap().starts_with("mcp_"));
    assert_eq!(manager.tools()[0].prefixed_name(), "web_hello");

    {
        let fake = fake.lock();
        let (uri, headers, body) = &fake.requests[0];
        assert_eq!(uri, "/mcp?codemode=false");
        assert_eq!(body["method"], "initialize");
        assert_eq!(body["params"]["clientInfo"], json!({"name": "opencode", "version": "test"}));
        assert_eq!(body["params"]["protocolVersion"], "2025-11-25");
        assert_eq!(body["params"]["capabilities"], json!({"roots": {}}));
        assert!(header(headers, "mcp-session-id").is_none());
        assert_eq!(header(headers, "accept").as_deref(), Some("application/json, text/event-stream"));
        assert_eq!(header(headers, "x-custom").as_deref(), Some("yes"));
        let (_, headers, body) = &fake.requests[1];
        assert_eq!(body["method"], "notifications/initialized");
        assert_eq!(header(headers, "mcp-session-id").as_deref(), Some("sess-1"));
        assert_eq!(header(headers, "mcp-protocol-version").as_deref(), Some("2025-11-25"));
    }

    // SSE response (notification before the response is delivered and ignored).
    let result = manager.call_tool("web", "hello", Some(json!({"name": "bob"})), None).await.unwrap();
    assert_eq!(result.content, vec![CallToolContent::Text { text: "hi bob".into() }]);

    // Session expiry: the server forgets the session → 404 → reconnect and retry once.
    fake.lock().sessions.clear();
    let result = manager.call_tool("web", "hello", Some(json!({"name": "eve"})), None).await.unwrap();
    assert_eq!(result.content, vec![CallToolContent::Text { text: "hi eve".into() }]);
    assert_eq!(fake.lock().next_session, 2);
    assert_eq!(manager.status("web"), Some(Status::Connected));

    // Shutdown terminates the session with DELETE.
    manager.shutdown().await;
    assert_eq!(fake.lock().deleted, vec!["sess-2".to_string()]);
}

#[tokio::test]
async fn http_codemode_param_fallback() {
    let (fake, port) = start_fake(|f| f.reject_codemode_param = true).await;
    let manager = manager(None);
    load(&manager, "web", remote(port, Some(OAuthSetting::Disabled))).await;
    assert_eq!(manager.status("web"), Some(Status::Connected), "{:?}", manager.servers());
    assert!(manager.servers()[0].integration_id.is_none());
    let uris: Vec<String> = fake.lock().requests.iter().map(|r| r.0.clone()).collect();
    assert_eq!(uris[0], "/mcp?codemode=false");
    assert_eq!(uris[1], "/mcp");
    manager.shutdown().await;
}

#[tokio::test]
async fn http_unreachable_and_invalid_url() {
    let manager = manager(None);
    let mut servers = IndexMap::new();
    servers.insert(
        "bad".to_string(),
        ServerConfig::Remote(RemoteConfig {
            url: "not a url".into(),
            headers: None,
            oauth: Some(OAuthSetting::Disabled),
            disabled: None,
            codemode: None,
            timeout: None,
            protocol: None,
        }),
    );
    manager.set_servers(servers).await;
    manager.wait_ready("bad").await.unwrap();
    assert_eq!(manager.status("bad"), Some(Status::failed("Invalid MCP URL for \"bad\"")));

    // 401 without OAuth integration → failed with the HTTP status appended.
    let (_fake, port) = start_fake(|f| f.require_token = Some("x".into())).await;
    load(&manager, "noauth", remote(port, Some(OAuthSetting::Disabled))).await;
    assert_eq!(manager.status("noauth"), Some(Status::failed("Error POSTing to endpoint: unauthorized (HTTP 401)")));
}

#[tokio::test]
async fn http_oauth_flow() {
    let (fake, port) = start_fake(|f| f.require_token = Some("tok-1".into())).await;
    let store = Arc::new(MemoryCredentialStore::default());
    let manager = manager(Some(store.clone()));
    load(&manager, "secure", remote(port, None)).await;
    let url = format!("http://127.0.0.1:{port}/mcp");
    assert_eq!(
        manager.status("secure"),
        Some(Status::needs_auth(format!("MCP server \"{url}\" requires a login before it can register a client")))
    );
    let error = manager.call_tool("secure", "hello", None, None).await.unwrap_err();
    assert!(error.to_string().starts_with("MCP server \"secure\" needs authentication: "));

    // Start the interactive flow.
    let handle = manager.auth_start("secure").await.unwrap();
    assert_eq!(handle.instructions, "Authorize secure in your browser. This window will close automatically.");
    assert_eq!(handle.mode, "auto");
    let auth_url = url::Url::parse(&handle.url).unwrap();
    assert_eq!(auth_url.path(), "/authorize");
    let param = |k: &str| auth_url.query_pairs().find(|(key, _)| key == k).map(|(_, v)| v.into_owned());
    assert_eq!(param("response_type").as_deref(), Some("code"));
    assert_eq!(param("client_id").as_deref(), Some("dyn-client"));
    assert_eq!(param("code_challenge_method").as_deref(), Some("S256"));
    assert_eq!(param("scope").as_deref(), Some("mcp.read"));
    assert_eq!(param("resource").as_deref(), Some(url.as_str()));
    let redirect = param("redirect_uri").unwrap();
    assert!(redirect.starts_with("http://127.0.0.1:") && redirect.ends_with("/callback"), "{redirect}");
    let state = param("state").unwrap();

    // DCR body.
    let registration = fake.lock().registrations[0].clone();
    assert_eq!(registration["redirect_uris"], json!([redirect]));
    assert_eq!(registration["client_name"], "opencode");
    assert_eq!(registration["client_uri"], "https://opencode.ai");
    assert_eq!(registration["grant_types"], json!(["authorization_code", "refresh_token"]));
    assert_eq!(registration["token_endpoint_auth_method"], "none");
    assert_eq!(registration["application_type"], "native");
    assert_eq!(registration["scope"], "mcp.read");

    let http = reqwest::Client::new();
    let callback = url::Url::parse(&redirect).unwrap();
    // Wrong path.
    let response = http.get(callback.join("/elsewhere").unwrap()).send().await.unwrap();
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    assert_eq!(response.text().await.unwrap(), "Not found");
    // Correct callback: held until the token exchange finishes.
    let response = http.get(format!("{redirect}?code=abc&state={state}")).send().await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response.headers()["content-type"], "text/html");
    let html = response.text().await.unwrap();
    assert!(html.contains("OpenCode is now connected to secure."));
    // Duplicate callback.
    let response = http.get(format!("{redirect}?code=abc&state={state}")).send().await;
    if let Ok(response) = response {
        assert_eq!(response.status(), StatusCode::CONFLICT);
        assert_eq!(response.text().await.unwrap(), "OAuth callback already received");
    }

    let credential = handle.wait().await.unwrap();
    let integration = manager.integration_id("secure").unwrap();
    assert_eq!(credential["type"], "oauth");
    assert_eq!(credential["methodID"], integration);
    assert_eq!(credential["access"], "tok-1");
    assert_eq!(credential["refresh"], "ref-2");
    assert!(credential["expires"].as_u64().unwrap() > 0);
    assert_eq!(credential["metadata"]["serverUrl"], url);
    assert_eq!(credential["metadata"]["tokenType"], "Bearer");
    assert_eq!(credential["metadata"]["scope"], "mcp.read");
    assert_eq!(credential["metadata"]["issuer"], format!("http://127.0.0.1:{port}"));
    assert_eq!(credential["metadata"]["client"]["client_id"], "dyn-client");

    // Token request carried PKCE, the redirect and the resource.
    let form = fake.lock().token_requests[0].clone();
    let get = |k: &str| form.iter().find(|(key, _)| key == k).map(|(_, v)| v.clone());
    assert_eq!(get("grant_type").as_deref(), Some("authorization_code"));
    assert_eq!(get("redirect_uri").as_deref(), Some(redirect.as_str()));
    assert_eq!(get("resource").as_deref(), Some(url.as_str()));
    assert_eq!(get("client_id").as_deref(), Some("dyn-client"));
    assert_eq!(get("code_verifier").map(|v| v.len()), Some(43));

    // Storing the credential reconnects the server.
    manager.auth_finish("secure", credential).await.unwrap();
    assert_eq!(manager.status("secure"), Some(Status::Connected), "{:?}", manager.servers());
    assert_eq!(store.list(&integration).await.unwrap().len(), 1);
    assert_eq!(store.list(&integration).await.unwrap()[0].label, "secure");

    // Logout removes credentials and drops back to needs_auth.
    assert_eq!(manager.logout("secure").await.unwrap(), 1);
    assert!(matches!(manager.status("secure"), Some(Status::NeedsAuth { .. })));
    manager.shutdown().await;
}

#[tokio::test]
async fn http_oauth_refresh_on_401() {
    let (fake, port) = start_fake(|f| {
        f.require_token = Some("tok-2".into());
        f.issue_token = "tok-2".into();
    })
    .await;
    let store: Arc<MemoryCredentialStore> = Arc::new(MemoryCredentialStore::default());
    let url = format!("http://127.0.0.1:{port}/mcp");
    let integration = oc_mcp::oauth::integration_id("secure", &url);
    store
        .create(
            &integration,
            "secure",
            json!({"type": "oauth", "methodID": integration, "access": "old", "refresh": "ref-1", "expires": 1,
                   "metadata": {"serverUrl": url, "tokenType": "Bearer", "client": {"client_id": "dyn-client"}}}),
        )
        .await
        .unwrap();
    let manager = manager(Some(store.clone()));
    load(&manager, "secure", remote(port, None)).await;
    assert_eq!(manager.status("secure"), Some(Status::Connected), "{:?}", manager.servers());
    let rows = store.list(&integration).await.unwrap();
    assert_eq!(rows[0].value["access"], "tok-2");
    assert_eq!(rows[0].value["refresh"], "ref-2");
    assert_eq!(rows[0].value["metadata"]["client"]["client_id"], "dyn-client");
    let form = fake.lock().token_requests[0].clone();
    assert!(form.contains(&("grant_type".to_string(), "refresh_token".to_string())));
    // The refresh used the configured headers too (createFetchWithInit).
    manager.shutdown().await;
}

#[tokio::test]
async fn oauth_callback_state_mismatch() {
    let (_fake, port) = start_fake(|f| f.require_token = Some("tok-1".into())).await;
    let manager = manager(Some(Arc::new(MemoryCredentialStore::default())));
    load(&manager, "secure", remote(port, None)).await;
    let handle = manager.auth_start("secure").await.unwrap();
    let auth_url = url::Url::parse(&handle.url).unwrap();
    let redirect = auth_url.query_pairs().find(|(k, _)| k == "redirect_uri").unwrap().1.into_owned();
    let response = reqwest::get(format!("{redirect}?code=abc&state=wrong")).await.unwrap();
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let html = response.text().await.unwrap();
    assert!(html.contains("<title>Authorization failed · OpenCode</title>"));
    assert!(html.contains(">OAuth state mismatch</pre>"));
    assert_eq!(handle.wait().await.unwrap_err(), "OAuth state mismatch");

    let handle = manager.auth_start("secure").await.unwrap();
    let auth_url = url::Url::parse(&handle.url).unwrap();
    let redirect = auth_url.query_pairs().find(|(k, _)| k == "redirect_uri").unwrap().1.into_owned();
    let response = reqwest::get(format!("{redirect}?error=access_denied&error_description=User%20said%20no")).await.unwrap();
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    assert!(response.text().await.unwrap().contains("User said no"));
    assert_eq!(handle.wait().await.unwrap_err(), "User said no");
    let _ = Method::GET;
    tokio::time::sleep(Duration::from_millis(10)).await;
    manager.shutdown().await;
}

#[tokio::test]
async fn http_protocol_negotiation_falls_back_or_fails() {
    let (fake, port) = start_fake(|_| {}).await;
    let manager = manager(None);
    let mut auto = remote(port, Some(OAuthSetting::Disabled));
    if let ServerConfig::Remote(r) = &mut auto {
        r.protocol = Some(oc_mcp::Protocol::Auto);
    }
    load(&manager, "auto", auto).await;
    assert_eq!(manager.status("auto"), Some(Status::Connected), "{:?}", manager.servers());
    {
        let fake = fake.lock();
        let probe = &fake.requests[0].2;
        assert_eq!(probe["method"], "server/discover");
        assert_eq!(probe["id"], "server-discover-probe-1");
        assert_eq!(probe["params"]["_meta"]["io.modelcontextprotocol/protocolVersion"], "2026-07-28");
        assert_eq!(fake.requests[1].2["method"], "initialize");
    }
    let mut pinned = remote(port, Some(OAuthSetting::Disabled));
    if let ServerConfig::Remote(r) = &mut pinned {
        r.protocol = Some(oc_mcp::Protocol::V2026_07_28);
    }
    load(&manager, "pinned", pinned).await;
    assert_eq!(
        manager.status("pinned"),
        Some(Status::failed(
            "Version negotiation failed: the server did not offer pinned protocol version 2026-07-28 via server/discover (no fallback in pin mode)"
        ))
    );
    manager.shutdown().await;
}

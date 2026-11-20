//! MCP OAuth connections and logins against local fake servers (`packages/core/test/mcp-oauth.test.ts`, 2.3.0):
//! a connection's provider refreshes 30 s before expiry with the discovery its login saved, refreshes a refused
//! token once, refuses to send a credential to another issuer, signs out only rejected credentials, and needs a
//! login for a 403 `insufficient_scope`; a login registers both loopback redirects and saves its discovery.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use axum::Router;
use axum::body::Body;
use axum::http::{HeaderMap, Method, Response, Uri};
use oc_mcp::oauth::{ConnectAuth, connect_provider, fetch::Fetcher};
use oc_mcp::{
    AuthorizeHandle, ClientInfo, ConnectError, Connection, McpCredentialStore, MemoryCredentialStore, OAuthConfig,
    OAuthSetting, RemoteConfig, ServerConfig, StoredCredential, TimeoutConfig,
};
use parking_lot::Mutex;
use serde_json::{Value, json};

// ---- a scriptable local server ---------------------------------------------------------------------------------

#[derive(Debug, Clone)]
struct Req {
    method: Method,
    path: String,
    query: String,
    headers: HeaderMap,
    body: String,
}

impl Req {
    fn form(&self) -> HashMap<String, String> {
        url::form_urlencoded::parse(self.body.as_bytes()).into_owned().collect()
    }
}

/// `None` leaves the request unanswered.
type Handler = Arc<dyn Fn(&Req, &str) -> Option<Response<Body>> + Send + Sync>;

struct Server {
    origin: String,
    requests: Arc<Mutex<Vec<Req>>>,
}

impl Server {
    fn url(&self, path: &str) -> String {
        format!("{}{path}", self.origin)
    }
    fn paths(&self) -> Vec<String> {
        self.requests.lock().iter().map(|r| r.path.clone()).collect()
    }
}

async fn serve(handler: Handler) -> Server {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let origin = format!("http://127.0.0.1:{}", listener.local_addr().unwrap().port());
    let requests: Arc<Mutex<Vec<Req>>> = Arc::default();
    let (seen, base) = (requests.clone(), origin.clone());
    let app = Router::new().fallback(move |method: Method, uri: Uri, headers: HeaderMap, body: String| {
        let (handler, seen, base) = (handler.clone(), seen.clone(), base.clone());
        async move {
            let req = Req {
                method,
                path: uri.path().to_string(),
                query: uri.query().unwrap_or("").to_string(),
                headers,
                body,
            };
            seen.lock().push(req.clone());
            match handler(&req, &base) {
                Some(response) => response,
                None => std::future::pending().await,
            }
        }
    });
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    Server { origin, requests }
}

fn json(status: u16, body: Value) -> Option<Response<Body>> {
    Some(Response::builder().status(status).header("content-type", "application/json").body(Body::from(body.to_string())).unwrap())
}

fn status(status: u16) -> Option<Response<Body>> {
    Some(Response::builder().status(status).body(Body::empty()).unwrap())
}

fn as_metadata(issuer: &str) -> Value {
    json!({"issuer": issuer, "authorization_endpoint": format!("{issuer}/authorize"), "token_endpoint": format!("{issuer}/token"), "response_types_supported": ["code"]})
}

/// Answers an MCP `initialize` (and accepts notifications).
fn mcp(req: &Req) -> Option<Response<Body>> {
    if req.method == Method::GET {
        return status(405);
    }
    let body: Value = serde_json::from_str(&req.body).unwrap_or_default();
    if body["method"] != "initialize" {
        return status(202);
    }
    json(
        200,
        json!({"jsonrpc": "2.0", "id": body["id"], "result": {"protocolVersion": body["params"]["protocolVersion"], "capabilities": {}, "serverInfo": {"name": "test", "version": "1"}}}),
    )
}

// ---- credentials -----------------------------------------------------------------------------------------------

/// A credential store whose refreshes also take the cross-process `credential-refresh:<id>` file lock.
struct LockedStore {
    rows: MemoryCredentialStore,
    root: std::path::PathBuf,
}

#[async_trait::async_trait]
impl McpCredentialStore for LockedStore {
    async fn list(&self, integration_id: &str) -> anyhow::Result<Vec<StoredCredential>> {
        self.rows.list(integration_id).await
    }
    async fn get(&self, id: &str) -> anyhow::Result<Option<StoredCredential>> {
        self.rows.get(id).await
    }
    async fn create(&self, integration_id: &str, label: &str, value: Value) -> anyhow::Result<String> {
        self.rows.create(integration_id, label, value).await
    }
    async fn update(&self, id: &str, value: Value) -> anyhow::Result<()> {
        self.rows.update(id, value).await
    }
    async fn remove(&self, id: &str) -> anyhow::Result<()> {
        self.rows.remove(id).await
    }
    fn lock_root(&self) -> Option<std::path::PathBuf> {
        Some(self.root.clone())
    }
}

fn now() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_millis() as u64
}

fn oauth(config: OAuthConfig) -> Option<OAuthSetting> {
    Some(OAuthSetting::Config(config))
}

fn remote_with(url: &str, oauth: Option<OAuthSetting>) -> RemoteConfig {
    RemoteConfig {
        url: url.to_string(),
        headers: None,
        oauth,
        disabled: None,
        codemode: None,
        timeout: Some(TimeoutConfig { startup: Some(10_000), catalog: None, execution: None }),
        protocol: None,
        pool: None,
    }
}

/// `remote(url)`: a static client.
fn remote(url: &str) -> RemoteConfig {
    remote_with(url, oauth(OAuthConfig { client_id: Some("client".into()), ..Default::default() }))
}

struct Setup {
    store: Arc<dyn McpCredentialStore>,
    id: String,
    config: RemoteConfig,
    _dir: tempfile::TempDir,
}

impl Setup {
    /// A login (`a1`/`r1`, valid for an hour unless `expires` says otherwise) with `metadata` merged in.
    async fn new(url: &str, expires: Option<u64>, metadata: Value, config: Option<RemoteConfig>) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let store: Arc<dyn McpCredentialStore> =
            Arc::new(LockedStore { rows: MemoryCredentialStore::default(), root: dir.path().join("locks") });
        let mut meta = json!({"serverUrl": url, "tokenType": "Bearer"});
        for (k, v) in metadata.as_object().cloned().unwrap_or_default() {
            meta[k] = v;
        }
        let value = json!({"type": "oauth", "methodID": "oauth", "access": "a1", "refresh": "r1",
                           "expires": expires.unwrap_or(now() + 3_600_000), "metadata": meta});
        let id = store.create("mcp_test", "test", value).await.unwrap();
        Self { store, id, config: config.unwrap_or_else(|| remote(url)), _dir: dir }
    }

    /// `McpOAuth.connectProvider` for the credential this test made (an "instance" of its own).
    async fn provider(&self) -> ConnectAuth {
        let credential = self.store.get(&self.id).await.unwrap();
        connect_provider(&self.config, credential, Some(&self.store), Fetcher::new("test"))
    }

    async fn stored(&self) -> Option<(String, String)> {
        let row = self.store.get(&self.id).await.unwrap()?.value;
        Some((row["access"].as_str()?.to_string(), row["refresh"].as_str()?.to_string()))
    }

    async fn value(&self) -> Value {
        self.store.get(&self.id).await.unwrap().map(|r| r.value).unwrap_or(Value::Null)
    }
}

fn pair(a: &str, b: &str) -> Option<(String, String)> {
    Some((a.to_string(), b.to_string()))
}

fn challenge(value: &str) -> HeaderMap {
    let mut headers = HeaderMap::new();
    headers.insert("www-authenticate", value.parse().unwrap());
    headers
}

async fn connect(provider: ConnectAuth, config: &RemoteConfig) -> Result<Connection, ConnectError> {
    Connection::connect(
        "test",
        &ServerConfig::Remote(config.clone()),
        &std::env::temp_dir(),
        Some(Arc::new(provider)),
        None,
        &ClientInfo { name: "opencode".into(), version: "test".into() },
    )
    .await
}

fn token_requests(server: &Server) -> Vec<HashMap<String, String>> {
    server.requests.lock().iter().filter(|r| r.method == Method::POST && r.path.ends_with("/token")).map(Req::form).collect()
}

// ---- a server whose MCP endpoint accepts only the refreshed token ---------------------------------------------

#[derive(Default)]
struct Guard {
    token: &'static str,
    token_endpoint: Option<String>,
    refused: usize,
}

async fn guarded() -> (Server, Arc<Mutex<Guard>>) {
    let state = Arc::new(Mutex::new(Guard { token: "ok", ..Default::default() }));
    let shared = state.clone();
    let server = serve(Arc::new(move |req, origin| {
        let mut state = shared.lock();
        match (req.method.as_str(), req.path.as_str()) {
            (_, "/.well-known/oauth-authorization-server") => {
                let mut metadata = as_metadata(origin);
                metadata["token_endpoint"] = json!(state.token_endpoint.clone().unwrap_or(format!("{origin}/token")));
                json(200, metadata)
            }
            ("POST", "/token") => match state.token {
                "unavailable" => Some(Response::builder().status(503).body(Body::from("upstream unavailable")).unwrap()),
                "invalid_grant" => json(400, json!({"error": "invalid_grant"})),
                _ => json(200, json!({"access_token": "a2", "token_type": "Bearer", "refresh_token": "r2", "expires_in": 3600})),
            },
            (_, "/mcp") => {
                if req.headers.get("authorization").and_then(|v| v.to_str().ok()) != Some("Bearer a2") {
                    state.refused += 1;
                    return status(401);
                }
                mcp(req)
            }
            _ => status(404),
        }
    }))
    .await;
    (server, state)
}

fn refresh_tokens(server: &Server) -> Vec<String> {
    token_requests(server).iter().map(|f| f.get("refresh_token").cloned().unwrap_or_default()).collect()
}

#[tokio::test]
async fn refreshes_a_refused_token_and_retries_with_the_new_one() {
    let (server, _) = guarded().await;
    let url = server.url("/mcp");
    let setup = Setup::new(&url, None, json!({}), None).await;
    let connection = connect(setup.provider().await, &setup.config).await.unwrap();
    connection.close().await;
    assert_eq!(refresh_tokens(&server), ["r1"]);
    let value = setup.value().await;
    assert_eq!((value["access"].as_str(), value["refresh"].as_str()), (Some("a2"), Some("r2")));
    assert_eq!(value["metadata"]["issuer"], server.origin);
    // The discovery this refresh found is saved for the next one.
    assert_eq!(value["metadata"]["discovery"], json!({"authorizationServer": format!("{}/", server.origin), "resource": url}));
}

// ---- ahead of expiry -------------------------------------------------------------------------------------------

/// An authorization server under `/as` whose token endpoint the test switches, and the discovery a login saves.
async fn issuer_server(unavailable: bool) -> (Server, Value) {
    let server = serve(Arc::new(move |req, origin| match (req.method.as_str(), req.path.as_str()) {
        (_, "/.well-known/oauth-authorization-server/as") => json(200, as_metadata(&format!("{origin}/as"))),
        ("POST", "/as/token") if unavailable => status(503),
        ("POST", "/as/token") => {
            json(200, json!({"access_token": "a2", "token_type": "Bearer", "refresh_token": "r2", "expires_in": 3600}))
        }
        _ => status(404),
    }))
    .await;
    let discovery = json!({"authorizationServer": server.url("/as"), "resource": server.url("/mcp")});
    (server, discovery)
}

#[tokio::test]
async fn refreshes_a_token_that_expires_within_30_seconds_with_the_saved_discovery_before_sending_it() {
    let (server, discovery) = issuer_server(false).await;
    let url = server.url("/mcp");
    let setup = Setup::new(&url, Some(now() + 10_000), json!({"discovery": discovery}), None).await;
    assert_eq!(setup.provider().await.token().await.unwrap().as_deref(), Some("a2"));
    // The saved discovery names the authorization server and resource; the MCP server is never probed.
    let resources: Vec<String> = token_requests(&server).iter().map(|f| f["resource"].clone()).collect();
    assert_eq!(resources, [url.clone()]);
    assert!(server.paths().iter().all(|p| p.starts_with("/as") || p.starts_with("/.well-known/oauth-authorization-server/as")));
    assert_eq!(setup.stored().await, pair("a2", "r2"));
}

#[tokio::test]
async fn sends_the_current_token_when_the_refresh_ahead_fails() {
    let (server, discovery) = issuer_server(true).await;
    let setup = Setup::new(&server.url("/mcp"), Some(now() + 10_000), json!({"discovery": discovery}), None).await;
    assert_eq!(setup.provider().await.token().await.unwrap().as_deref(), Some("a1"));
    assert_eq!(setup.stored().await, pair("a1", "r1"));
}

#[tokio::test]
async fn leaves_tokens_with_time_left_without_a_lifetime_or_saved_without_discovery_to_the_401() {
    let (server, discovery) = issuer_server(false).await;
    let url = server.url("/mcp");
    let cases = [
        (now() + 60_000, json!({"discovery": discovery})),
        (0, json!({"discovery": discovery})),
        (now() + 10_000, json!({})),
    ];
    for (expires, metadata) in cases {
        let setup = Setup::new(&url, Some(expires), metadata, None).await;
        assert_eq!(setup.provider().await.token().await.unwrap().as_deref(), Some("a1"));
    }
    assert!(token_requests(&server).is_empty());
}

#[tokio::test]
async fn refreshes_the_credential_it_was_made_with_not_the_one_signed_in_since() {
    let (server, _) = guarded().await;
    let url = server.url("/mcp");
    let setup = Setup::new(&url, None, json!({}), None).await;
    let provider = setup.provider().await;
    let newer = setup
        .store
        .create("mcp_test", "test", json!({"type": "oauth", "methodID": "oauth", "access": "other", "refresh": "other-refresh", "expires": 0}))
        .await
        .unwrap();
    provider.token().await.unwrap();
    provider.on_unauthorized(&HeaderMap::new()).await.unwrap();
    assert_eq!(refresh_tokens(&server), ["r1"]);
    assert_eq!(setup.stored().await, pair("a2", "r2"));
    assert_eq!(setup.store.get(&newer).await.unwrap().unwrap().value["access"], "other");
}

#[tokio::test]
async fn reports_needs_auth_for_an_unauthorized_server_without_a_credential_or_any_oauth_request() {
    let server = serve(Arc::new(|req, _| if req.path == "/mcp" { status(401) } else { status(404) })).await;
    let config = remote(&server.url("/mcp"));
    let provider = connect_provider(&config, None, None, Fetcher::new("test"));
    match connect(provider, &config).await {
        Err(ConnectError::NeedsAuth { message, .. }) => assert_eq!(message, "Unauthorized"),
        other => panic!("{:?}", other.err()),
    }
    assert!(server.paths().iter().all(|p| p == "/mcp"));
    // The anonymous provider sends no token.
    assert_eq!(ConnectAuth::anonymous().token().await.unwrap(), None);
}

// ---- resource --------------------------------------------------------------------------------------------------

/// An authorization server at the origin that records token requests.
async fn authorization_server(extra: Value) -> Server {
    serve(Arc::new(move |req, origin| match (req.method.as_str(), req.path.as_str()) {
        (_, "/.well-known/oauth-authorization-server") => {
            let mut metadata = as_metadata(origin);
            metadata["registration_endpoint"] = json!(format!("{origin}/register"));
            for (k, v) in extra.as_object().cloned().unwrap_or_default() {
                metadata[k] = v;
            }
            json(200, metadata)
        }
        ("POST", "/register") => json(200, json!({"client_id": "registered", "redirect_uris": []})),
        ("POST", "/token") => json(200, json!({"access_token": "access", "token_type": "Bearer"})),
        _ => status(404),
    }))
    .await
}

#[tokio::test]
async fn sends_the_configured_url_when_the_server_publishes_no_metadata() {
    let server = authorization_server(json!({})).await;
    let url = server.url("/mcp");
    let setup = Setup::new(&url, None, json!({}), None).await;
    let provider = setup.provider().await;
    provider.token().await.unwrap();
    provider.on_unauthorized(&HeaderMap::new()).await.unwrap();
    let forms = token_requests(&server);
    assert_eq!(forms[0]["grant_type"], "refresh_token");
    assert_eq!(forms[0]["resource"], url);
}

#[tokio::test]
async fn keeps_the_configured_url_when_metadata_echoes_the_dialed_query() {
    let server = serve(Arc::new(|req, origin| match (req.method.as_str(), req.path.as_str()) {
        (_, "/.well-known/oauth-authorization-server") => json(200, as_metadata(origin)),
        (_, "/.well-known/oauth-protected-resource/mcp") => json(
            200,
            json!({"resource": format!("{origin}/mcp?{}", req.query), "authorization_servers": [origin]}),
        ),
        ("POST", "/token") => json(200, json!({"access_token": "next", "token_type": "Bearer"})),
        _ => status(404),
    }))
    .await;
    let url = server.url("/mcp");
    let setup = Setup::new(&url, None, json!({}), None).await;
    let provider = setup.provider().await;
    // The transport dials with ?codemode=false and the 401 points at metadata that echoes it.
    provider.token().await.unwrap();
    let rm = format!(r#"Bearer resource_metadata="{}/.well-known/oauth-protected-resource/mcp?codemode=false""#, server.origin);
    provider.on_unauthorized(&challenge(&rm)).await.unwrap();
    assert_eq!(token_requests(&server)[0]["resource"], url);
}

#[tokio::test]
async fn discovers_the_resource_for_a_login_saved_without_one() {
    let server = serve(Arc::new(|req, origin| match (req.method.as_str(), req.path.as_str()) {
        (_, "/.well-known/oauth-authorization-server") => json(200, as_metadata(origin)),
        (_, "/.well-known/oauth-protected-resource/mcp") => {
            json(200, json!({"resource": format!("{origin}/"), "authorization_servers": [origin]}))
        }
        ("POST", "/token") => json(200, json!({"access_token": "a2", "token_type": "Bearer"})),
        _ => status(404),
    }))
    .await;
    let setup = Setup::new(&server.url("/mcp"), None, json!({}), None).await;
    let provider = setup.provider().await;
    provider.token().await.unwrap();
    provider.on_unauthorized(&HeaderMap::new()).await.unwrap();
    assert_eq!(token_requests(&server)[0]["resource"], server.url("/"));
}

#[tokio::test]
async fn follows_the_401_to_the_resource_for_a_login_saved_without_discovery_and_saves_it() {
    let server = serve(Arc::new(|req, origin| match (req.method.as_str(), req.path.as_str()) {
        (_, "/custom/metadata") => {
            json(200, json!({"resource": format!("{origin}/"), "authorization_servers": [format!("{origin}/as")]}))
        }
        (_, "/.well-known/oauth-authorization-server/as") => json(200, as_metadata(&format!("{origin}/as"))),
        ("POST", "/as/token") => {
            json(200, json!({"access_token": "a2", "token_type": "Bearer", "refresh_token": "r2", "expires_in": 3600}))
        }
        _ => status(404),
    }))
    .await;
    let setup = Setup::new(&server.url("/mcp"), Some(now() - 1_000), json!({}), None).await;
    let provider = setup.provider().await;
    provider.token().await.unwrap();
    let rm = format!(r#"Bearer resource_metadata="{}/custom/metadata""#, server.origin);
    provider.on_unauthorized(&challenge(&rm)).await.unwrap();
    let resources: Vec<String> = token_requests(&server).iter().map(|f| f["resource"].clone()).collect();
    assert_eq!(resources, [server.url("/")]);
    assert_eq!(
        setup.value().await["metadata"]["discovery"],
        json!({"authorizationServer": server.url("/as"), "resource": server.url("/")})
    );
}

#[tokio::test]
async fn refreshes_through_configured_authorization_server_metadata() {
    let server = authorization_server(json!({})).await;
    let resource = serve(Arc::new(|_, _| status(404))).await;
    let url = resource.url("/mcp");
    let config = remote_with(
        &url,
        oauth(OAuthConfig {
            client_id: Some("client".into()),
            auth_server_metadata_url: Some(server.url("/.well-known/oauth-authorization-server")),
            ..Default::default()
        }),
    );
    let setup = Setup::new(&url, None, json!({}), Some(config)).await;
    let provider = setup.provider().await;
    provider.token().await.unwrap();
    provider.on_unauthorized(&HeaderMap::new()).await.unwrap();
    let forms = token_requests(&server);
    assert_eq!(forms[0]["grant_type"], "refresh_token");
    assert_eq!(forms[0]["resource"], url);
    // A configured metadata URL is read on every refresh, so no discovery is saved.
    assert!(setup.value().await["metadata"].get("discovery").is_none());
    assert!(resource.paths().is_empty());
}

// ---- across instances ------------------------------------------------------------------------------------------

/// A token endpoint that rotates refresh tokens and rejects reuse; it holds its answers until released.
async fn rotating() -> (Server, Arc<tokio::sync::Notify>, Arc<Mutex<bool>>) {
    let state = Arc::new(Mutex::new(("r1".to_string(), 1)));
    let released = Arc::new(Mutex::new(false));
    let notify = Arc::new(tokio::sync::Notify::new());
    let (gate, wake) = (released.clone(), notify.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let origin = format!("http://127.0.0.1:{}", listener.local_addr().unwrap().port());
    let requests: Arc<Mutex<Vec<Req>>> = Arc::default();
    let seen = requests.clone();
    let app = Router::new().fallback(move |method: Method, uri: Uri, headers: HeaderMap, body: String| {
        let (state, gate, wake, seen) = (state.clone(), gate.clone(), wake.clone(), seen.clone());
        async move {
            let req = Req { method, path: uri.path().to_string(), query: String::new(), headers, body };
            if req.method != Method::POST || req.path != "/token" {
                return Response::builder().status(404).body(Body::empty()).unwrap();
            }
            seen.lock().push(req.clone());
            wake.notify_waiters();
            while !*gate.lock() {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
            let token = req.form().get("refresh_token").cloned().unwrap_or_default();
            let mut state = state.lock();
            if token != state.0 {
                return json(400, json!({"error": "invalid_grant"})).unwrap();
            }
            state.1 += 1;
            state.0 = format!("r{}", state.1);
            json(200, json!({"access_token": format!("a{}", state.1), "token_type": "Bearer", "refresh_token": state.0, "expires_in": 3600}))
                .unwrap()
        }
    });
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (Server { origin, requests }, notify, released)
}

#[tokio::test]
async fn sends_one_refresh_when_two_instances_refresh_the_same_credential_at_once() {
    let (server, received, released) = rotating().await;
    let url = format!("{}/", server.origin);
    let setup = Setup::new(&url, None, json!({}), None).await;
    let (first, second) = (setup.provider().await, setup.provider().await);
    first.token().await.unwrap();
    second.token().await.unwrap();
    let waiter = received.notified();
    let none = HeaderMap::new();
    let pending = first.on_unauthorized(&none);
    let waiting = async {
        // The first instance holds the credential lock until its rotated tokens are saved.
        waiter.await;
        let result = second.on_unauthorized(&none);
        *released.lock() = true;
        result.await
    };
    let (a, b) = tokio::join!(pending, waiting);
    a.unwrap();
    b.unwrap();
    assert_eq!(refresh_tokens(&server), ["r1"]);
    assert_eq!(first.token().await.unwrap().as_deref(), Some("a2"));
    assert_eq!(second.token().await.unwrap().as_deref(), Some("a2"));
    assert_eq!(setup.stored().await, pair("a2", "r2"));
}

#[tokio::test]
async fn uses_the_stored_tokens_instead_of_spending_a_refresh_token_another_instance_already_spent() {
    let (server, _, released) = rotating().await;
    *released.lock() = true;
    let setup = Setup::new(&format!("{}/", server.origin), None, json!({}), None).await;
    let (stale, other) = (setup.provider().await, setup.provider().await);
    stale.token().await.unwrap();
    other.token().await.unwrap();
    other.on_unauthorized(&HeaderMap::new()).await.unwrap();
    stale.on_unauthorized(&HeaderMap::new()).await.unwrap();
    assert_eq!(refresh_tokens(&server), ["r1"]);
    assert_eq!(stale.token().await.unwrap().as_deref(), Some("a2"));
}

#[tokio::test]
async fn needs_auth_without_a_request_when_the_credential_was_signed_out() {
    let (server, _, released) = rotating().await;
    *released.lock() = true;
    let setup = Setup::new(&format!("{}/", server.origin), None, json!({}), None).await;
    let provider = setup.provider().await;
    provider.token().await.unwrap();
    setup.store.remove(&setup.id).await.unwrap();
    let error = provider.on_unauthorized(&HeaderMap::new()).await.unwrap_err();
    assert!(matches!(error, oc_mcp::McpError::Unauthorized(_)), "{error:?}");
    assert!(server.requests.lock().is_empty());
}

// ---- when the refresh fails ------------------------------------------------------------------------------------

#[tokio::test]
async fn fails_without_signing_out_when_the_token_endpoint_answers_503() {
    let (server, state) = guarded().await;
    let url = server.url("/mcp");
    let setup = Setup::new(&url, None, json!({}), None).await;
    state.lock().token = "unavailable";
    match connect(setup.provider().await, &setup.config).await {
        Err(ConnectError::Connect { message, .. }) => {
            assert_eq!(message, "Could not refresh the OAuth token: HTTP 503 (temporarily_unavailable)")
        }
        other => panic!("{:?}", other.err()),
    }
    assert_eq!(setup.stored().await, pair("a1", "r1"));
    state.lock().token = "ok";
    connect(setup.provider().await, &setup.config).await.unwrap().close().await;
    assert_eq!(refresh_tokens(&server), ["r1", "r1"]);
    assert_eq!(setup.stored().await, pair("a2", "r2"));
}

#[tokio::test]
async fn fails_without_signing_out_when_the_token_endpoint_is_unreachable() {
    let (server, state) = guarded().await;
    let closed = {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        listener.local_addr().unwrap().port()
    };
    state.lock().token_endpoint = Some(format!("http://127.0.0.1:{closed}/token"));
    let setup = Setup::new(&server.url("/mcp"), None, json!({}), None).await;
    match connect(setup.provider().await, &setup.config).await {
        Err(ConnectError::Connect { message, .. }) => {
            assert!(message.starts_with("Could not refresh the OAuth token: "), "{message}")
        }
        other => panic!("{:?}", other.err()),
    }
    assert_eq!(setup.stored().await, pair("a1", "r1"));
    state.lock().token_endpoint = None;
    connect(setup.provider().await, &setup.config).await.unwrap().close().await;
}

#[tokio::test]
async fn signs_out_when_the_token_endpoint_answers_invalid_grant() {
    let (server, state) = guarded().await;
    state.lock().token = "invalid_grant";
    let setup = Setup::new(&server.url("/mcp"), None, json!({}), None).await;
    match connect(setup.provider().await, &setup.config).await {
        Err(ConnectError::NeedsAuth { message, .. }) => {
            assert_eq!(message, format!("MCP server \"{}\" requires user authorization", server.url("/mcp")))
        }
        other => panic!("{:?}", other.err()),
    }
    assert_eq!(setup.stored().await, None);
}

#[tokio::test(start_paused = true)]
async fn keeps_the_credential_when_the_refresh_times_out() {
    // Discovery answers; the token endpoint never does.
    let server = serve(Arc::new(|req, origin| match req.path.as_str() {
        "/.well-known/oauth-authorization-server" => json(200, as_metadata(origin)),
        "/token" => None,
        _ => status(404),
    }))
    .await;
    let setup = Setup::new(&format!("{}/", server.origin), None, json!({}), None).await;
    let provider = setup.provider().await;
    provider.token().await.unwrap();
    let error = provider.on_unauthorized(&HeaderMap::new()).await.unwrap_err();
    match error {
        oc_mcp::McpError::OAuth { code, message } => {
            assert_eq!(code, "temporarily_unavailable");
            assert_eq!(message, "Could not refresh the OAuth token: Operation timed out after '30s'");
        }
        other => panic!("{other:?}"),
    }
    assert_eq!(setup.stored().await, pair("a1", "r1"));
}

#[tokio::test]
async fn sends_nothing_to_an_authorization_server_other_than_the_one_that_issued_the_credential() {
    let attacker = serve(Arc::new(|req, origin| match (req.method.as_str(), req.path.as_str()) {
        (_, "/resource") => json(200, json!({"resource": format!("{origin}/"), "authorization_servers": [origin]})),
        (_, "/.well-known/oauth-authorization-server") => json(200, as_metadata(origin)),
        _ => json(200, json!({"access_token": "stolen", "token_type": "Bearer"})),
    }))
    .await;
    // Saved before discovery was stored: only the issuer the login found is known.
    let setup =
        Setup::new(&attacker.url("/mcp"), None, json!({"issuer": "https://auth.legit.example"}), None).await;
    let provider = setup.provider().await;
    provider.token().await.unwrap();
    let rm = format!(r#"Bearer resource_metadata="{}/resource""#, attacker.origin);
    let error = provider.on_unauthorized(&challenge(&rm)).await.unwrap_err();
    assert!(matches!(error, oc_mcp::McpError::Unauthorized(_)), "{error:?}");
    assert!(attacker.requests.lock().iter().all(|r| r.method != Method::POST));
    let value = setup.value().await;
    assert_eq!(
        (value["access"].as_str(), value["refresh"].as_str(), value["metadata"]["issuer"].as_str()),
        (Some("a1"), Some("r1"), Some("https://auth.legit.example"))
    );
}

#[tokio::test]
async fn names_only_the_error_code_of_a_token_endpoint_that_answers_without_an_oauth_error() {
    let (server, state) = guarded().await;
    let echo = serve(Arc::new(|_, _| {
        Some(Response::builder().status(400).body(Body::from("<html>secret echo of the request</html>")).unwrap())
    }))
    .await;
    state.lock().token_endpoint = Some(echo.url("/token"));
    let setup = Setup::new(&server.url("/mcp"), None, json!({}), None).await;
    let provider = setup.provider().await;
    provider.token().await.unwrap();
    match provider.on_unauthorized(&HeaderMap::new()).await.unwrap_err() {
        oc_mcp::McpError::OAuth { message, .. } => {
            assert_eq!(message, "Could not refresh the OAuth token: server_error")
        }
        other => panic!("{other:?}"),
    }
    assert_eq!(setup.stored().await, pair("a1", "r1"));
}

#[tokio::test]
async fn signs_out_a_credential_without_client_information() {
    let server = authorization_server(json!({})).await;
    let url = server.url("/mcp");
    let setup = Setup::new(&url, None, json!({}), Some(remote_with(&url, None))).await;
    let provider = setup.provider().await;
    provider.token().await.unwrap();
    let error = provider.on_unauthorized(&HeaderMap::new()).await.unwrap_err();
    assert!(matches!(error, oc_mcp::McpError::Unauthorized(_)), "{error:?}");
    assert!(token_requests(&server).is_empty());
    assert_eq!(setup.stored().await, None);
}

#[tokio::test]
async fn tries_the_next_metadata_url_when_one_answers_502() {
    let server = serve(Arc::new(|req, origin| match (req.method.as_str(), req.path.as_str()) {
        (_, "/.well-known/oauth-authorization-server") => status(502),
        (_, "/.well-known/openid-configuration") => json(
            200,
            json!({"issuer": origin, "authorization_endpoint": format!("{origin}/authorize"), "token_endpoint": format!("{origin}/token"),
                   "response_types_supported": ["code"], "subject_types_supported": ["public"],
                   "id_token_signing_alg_values_supported": ["RS256"], "jwks_uri": format!("{origin}/jwks")}),
        ),
        ("POST", "/token") => json(200, json!({"access_token": "a2", "token_type": "Bearer", "refresh_token": "r2"})),
        _ => status(404),
    }))
    .await;
    let setup = Setup::new(&server.url("/mcp"), None, json!({}), None).await;
    let provider = setup.provider().await;
    provider.token().await.unwrap();
    provider.on_unauthorized(&HeaderMap::new()).await.unwrap();
    assert_eq!(token_requests(&server).len(), 1);
    assert_eq!(setup.stored().await, pair("a2", "r2"));
}

#[tokio::test]
async fn refuses_a_saved_discovery_that_names_another_issuer() {
    // The saved discovery's authorization server now answers as another issuer than the one that issued the login.
    let (server, discovery) = issuer_server(false).await;
    let setup = Setup::new(
        &server.url("/mcp"),
        Some(now() + 10_000),
        json!({"discovery": discovery, "issuer": "https://auth.legit.example"}),
        None,
    )
    .await;
    let provider = setup.provider().await;
    // Refreshing ahead fails, so the current token goes out; the 401 then needs a login and keeps the credential.
    assert_eq!(provider.token().await.unwrap().as_deref(), Some("a1"));
    let error = provider.on_unauthorized(&HeaderMap::new()).await.unwrap_err();
    assert!(matches!(error, oc_mcp::McpError::Unauthorized(_)), "{error:?}");
    assert!(token_requests(&server).is_empty());
    assert_eq!(setup.stored().await, pair("a1", "r1"));
}

#[tokio::test]
async fn reports_needs_auth_when_the_server_asks_for_more_scope() {
    let server = serve(Arc::new(|_, _| {
        Some(
            Response::builder()
                .status(403)
                .header("www-authenticate", r#"Bearer error="insufficient_scope", scope="admin""#)
                .body(Body::empty())
                .unwrap(),
        )
    }))
    .await;
    let url = server.url("/mcp");
    let setup = Setup::new(&url, None, json!({}), None).await;
    match connect(setup.provider().await, &setup.config).await {
        Err(ConnectError::NeedsAuth { message, .. }) => assert_eq!(message, r#"Insufficient scope: required "admin""#),
        other => panic!("{:?}", other.err()),
    }
    // No step-up: nothing but the MCP endpoint was asked.
    assert!(server.paths().iter().all(|p| p == "/mcp"));
}

// ---- logins ----------------------------------------------------------------------------------------------------

async fn start(server: &Server, oauth_config: Option<OAuthConfig>) -> (AuthorizeHandle, url::Url) {
    let handle = oc_mcp::oauth::authorize(oc_mcp::oauth::AuthorizeOptions {
        name: "test".into(),
        config: remote_with(&server.url("/"), oauth_config.map(OAuthSetting::Config)),
        integration_id: "mcp_test".into(),
        method_id: "oauth".into(),
        credentials: None,
    })
    .await
    .unwrap();
    let url = url::Url::parse(&handle.url).unwrap();
    (handle, url)
}

fn param(url: &url::Url, key: &str) -> String {
    url.query_pairs().find(|(k, _)| k == key).map(|(_, v)| v.into_owned()).unwrap_or_default()
}

#[tokio::test]
async fn completes_interactive_authorization_and_saves_the_discovery_it_found() {
    let server = serve(Arc::new(|req, _| match (req.method.as_str(), req.path.as_str()) {
        ("POST", "/token") => {
            json(200, json!({"access_token": "access", "token_type": "Bearer", "refresh_token": "refresh", "expires_in": 3600}))
        }
        _ => status(404),
    }))
    .await;
    let (handle, url) = start(&server, Some(OAuthConfig { client_id: Some("client".into()), ..Default::default() })).await;
    let redirect = format!("{}?code=accepted&state={}", param(&url, "redirect_uri"), param(&url, "state"));
    assert_eq!(reqwest::get(&redirect).await.unwrap().status(), 200);
    let credential = handle.wait().await.unwrap();
    assert_eq!((credential["access"].as_str(), credential["refresh"].as_str()), (Some("access"), Some("refresh")));
    let forms = token_requests(&server);
    assert_eq!(forms.len(), 1);
    assert_eq!(forms[0]["grant_type"], "authorization_code");
    assert_eq!(forms[0]["resource"], server.url("/"));
    assert_eq!(
        credential["metadata"]["discovery"],
        json!({"authorizationServer": server.url("/"), "resource": server.url("/")})
    );
}

#[tokio::test]
async fn redeems_the_code_with_the_localhost_redirect_a_server_sent_the_browser_back_to() {
    let server = authorization_server(json!({})).await;
    let (handle, url) = start(&server, None).await;
    let redirect = url::Url::parse(&param(&url, "redirect_uri")).unwrap();
    let port = redirect.port().unwrap();
    assert_eq!(redirect.host_str(), Some("127.0.0.1"));
    // Clerk's consent page rewrites 127.0.0.1 to localhost; the browser reaches the same listener.
    let response = reqwest::Client::new()
        .get(format!("{redirect}?code=accepted&state={}", param(&url, "state")))
        .header("host", format!("localhost:{port}"))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    handle.wait().await.unwrap();
    let registration: Value = serde_json::from_str(
        &server.requests.lock().iter().find(|r| r.path == "/register").map(|r| r.body.clone()).unwrap(),
    )
    .unwrap();
    assert_eq!(
        registration["redirect_uris"],
        json!([format!("http://127.0.0.1:{port}/callback"), format!("http://localhost:{port}/callback")])
    );
    assert_eq!(token_requests(&server)[0]["redirect_uri"], format!("http://localhost:{port}/callback"));
}

#[tokio::test]
async fn registers_only_a_configured_redirect_uri() {
    let server = authorization_server(json!({})).await;
    let fixed = {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        listener.local_addr().unwrap().port()
    };
    let redirect = format!("http://127.0.0.1:{fixed}/fixed-callback");
    let (_handle, url) =
        start(&server, Some(OAuthConfig { redirect_uri: Some(redirect.clone()), ..Default::default() })).await;
    assert_eq!(param(&url, "redirect_uri"), redirect);
    let registration: Value = serde_json::from_str(
        &server.requests.lock().iter().find(|r| r.path == "/register").map(|r| r.body.clone()).unwrap(),
    )
    .unwrap();
    assert_eq!(registration["redirect_uris"], json!([redirect]));
}

#[tokio::test]
async fn saves_no_discovery_when_the_authorization_server_metadata_url_is_configured() {
    let issuer = authorization_server(json!({})).await;
    let resource = serve(Arc::new(|_, _| status(404))).await;
    let handle = oc_mcp::oauth::authorize(oc_mcp::oauth::AuthorizeOptions {
        name: "test".into(),
        config: remote_with(
            &resource.url("/mcp"),
            oauth(OAuthConfig {
                client_id: Some("client".into()),
                auth_server_metadata_url: Some(issuer.url("/.well-known/oauth-authorization-server")),
                ..Default::default()
            }),
        ),
        integration_id: "mcp_test".into(),
        method_id: "oauth".into(),
        credentials: None,
    })
    .await
    .unwrap();
    let url = url::Url::parse(&handle.url).unwrap();
    assert_eq!(url.origin().ascii_serialization(), issuer.origin);
    assert_eq!(param(&url, "resource"), resource.url("/mcp"));
    let redirect = format!("{}?code=accepted&state={}", param(&url, "redirect_uri"), param(&url, "state"));
    reqwest::get(&redirect).await.unwrap();
    let credential = handle.wait().await.unwrap();
    assert!(credential["metadata"].get("discovery").is_none(), "{credential}");
}

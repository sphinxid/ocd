use base64::Engine;
use oc_ai::credentials::callback::{self, Request, Response, sync_handler};
use oc_ai::credentials::flows::{
    self, ChatGptTokenSharing, CodexBrowser, CodexDevice, ConsoleDevice, CopilotDevice, DigitalOceanImplicit,
    GitLabPkce, PoeBrowser, PollTiming, SnowflakeBrowser, XaiDevice,
};
use oc_ai::credentials::form::FormAnswer;
use oc_ai::credentials::http::FlowContext;
use oc_ai::credentials::integration::{AttemptMode, OAuthMethod};
use oc_ai::credentials::kv::{KvStore, MemoryKv};
use oc_ai::credentials::pkce;
use oc_ai::credentials::{OAuthValue, map_env, now_ms, page};
use parking_lot::Mutex;
use serde_json::{Value, json};
use std::collections::HashMap;
use std::sync::Arc;
use tokio_util::sync::{CancellationToken, DropGuard};

const UA: &str = "opencode/test/1.0.0/cli";

fn ctx() -> FlowContext {
    FlowContext::with_client(reqwest::Client::builder().no_proxy().build().unwrap(), UA)
}

fn client() -> reqwest::Client {
    reqwest::Client::builder().no_proxy().redirect(reqwest::redirect::Policy::none()).build().unwrap()
}

struct Mock {
    base: String,
    requests: Arc<Mutex<Vec<Request>>>,
    _guard: DropGuard,
}

impl Mock {
    fn requests_to(&self, path: &str) -> Vec<Request> {
        self.requests.lock().iter().filter(|r| r.path == path).cloned().collect()
    }
}

/// Hand-rolled HTTP mock: `route(request, nth call to this path)`.
async fn mock(route: impl Fn(&Request, usize) -> Response + Send + Sync + 'static) -> Mock {
    let requests = Arc::new(Mutex::new(Vec::<Request>::new()));
    let counts = Arc::new(Mutex::new(HashMap::<String, usize>::new()));
    let recorded = requests.clone();
    let handler = sync_handler(move |request| {
        let n = {
            let mut counts = counts.lock();
            let entry = counts.entry(request.path.clone()).or_insert(0);
            *entry += 1;
            *entry - 1
        };
        recorded.lock().push(request.clone());
        route(&request, n)
    });
    let bound = callback::bind("127.0.0.1", 0).await.unwrap();
    let base = format!("http://127.0.0.1:{}", bound.port);
    let scope = CancellationToken::new();
    bound.serve(handler, scope.clone());
    Mock { base, requests, _guard: scope.drop_guard() }
}

fn json_response(status: u16, value: Value) -> Response {
    Response::json(status, value.to_string())
}

fn query(url: &str) -> Vec<(String, String)> {
    url::Url::parse(url).unwrap().query_pairs().map(|(k, v)| (k.into_owned(), v.into_owned())).collect()
}

fn param(url: &str, key: &str) -> String {
    query(url).into_iter().find(|(k, _)| k == key).map(|(_, v)| v).unwrap()
}

fn keys(url: &str) -> Vec<String> {
    query(url).into_iter().map(|(k, _)| k).collect()
}

fn body_pairs(request: &Request) -> Vec<(String, String)> {
    url::form_urlencoded::parse(&request.body).map(|(k, v)| (k.into_owned(), v.into_owned())).collect()
}

fn header<'a>(request: &'a Request, name: &str) -> Option<&'a str> {
    request.headers.iter().find(|(k, _)| k == name).map(|(_, v)| v.as_str())
}

fn fake_jwt(claims: Value) -> String {
    format!("eyJhbGciOiJub25lIn0.{}.sig", pkce::base64url(claims.to_string().as_bytes()))
}

fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port()
}

// ---------------------------------------------------------------------------------------------
// PKCE / state / JWT helpers
// ---------------------------------------------------------------------------------------------

#[test]
fn pkce_and_state_formats() {
    // RFC 7636 appendix B.
    assert_eq!(pkce::challenge("dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk"), "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM");
    let charset = |c: char| c.is_ascii_alphanumeric() || "-._~".contains(c);
    let openai = pkce::openai_pkce();
    assert_eq!(openai.verifier.len(), 43);
    assert!(openai.verifier.chars().all(charset));
    assert_eq!(openai.challenge, pkce::challenge(&openai.verifier));
    assert_eq!(openai.challenge.len(), 43);
    assert_eq!(pkce::verifier_from_bytes(&[0, 65, 66, 255]), "A~A5");
    let urlsafe = |s: &str| s.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_');
    let gitlab = pkce::base64_pkce(32);
    assert_eq!(gitlab.verifier.len(), 43);
    assert!(urlsafe(&gitlab.verifier));
    assert_eq!(pkce::base64_pkce(48).verifier.len(), 64);
    let state = pkce::random_state();
    assert_eq!(state.len(), 43);
    assert!(urlsafe(&state));
    assert_ne!(state, pkce::random_state());
    assert_eq!(pkce::form_encode(&[("scope", "a b:c/d"), ("x", "~*")]), "scope=a+b%3Ac%2Fd&x=%7E*");
    assert_eq!(pkce::encode_uri_component("my role/é"), "my%20role%2F%C3%A9");
}

#[test]
fn chatgpt_account_id_claims() {
    let token = fake_jwt(json!({"chatgpt_account_id":"direct","https://api.openai.com/auth":{"chatgpt_account_id":"nested"},"organizations":[{"id":"org"}]}));
    assert_eq!(pkce::chatgpt_account_id(&token).as_deref(), Some("direct"));
    let token = fake_jwt(json!({"https://api.openai.com/auth":{"chatgpt_account_id":"nested"},"organizations":[{"id":"org"}]}));
    assert_eq!(pkce::chatgpt_account_id(&token).as_deref(), Some("nested"));
    let token = fake_jwt(json!({"organizations":[{"id":"org_1"},{"id":"org_2"}]}));
    assert_eq!(pkce::chatgpt_account_id(&token).as_deref(), Some("org_1"));
    // Shape mismatch → none.
    assert_eq!(pkce::chatgpt_account_id(&fake_jwt(json!({"chatgpt_account_id": 5}))), None);
    assert_eq!(pkce::chatgpt_account_id("not-a-jwt"), None);
    let id = fake_jwt(json!({"sub":"x"}));
    let access = fake_jwt(json!({"chatgpt_account_id":"from-access"}));
    assert_eq!(pkce::extract_account_id(Some(&id), &access).as_deref(), Some("from-access"));
}

const TEST_KEY: &str = "MIIEvAIBADANBgkqhkiG9w0BAQEFAASCBKYwggSiAgEAAoIBAQCtA6LT6IPEb+6KSirgqA2OXf1V5SX2JUrku3cUpBwLDMlboaAV4CI0THCTFJjorZvwWO/Y6Mmbg1wlbEHU4aZ/YSid/WOq4DmCZavgZFZrXaP+D1Jj/o6oAT3RaG31pCSIF3hY6lDL47grY6LaaEP0rnGTKQLmaMJhH0WLNHVPyjUhrct5MDbg56F/CRJ64jKWhfVCdZeRUFl3r9riTgYGUkEGSfXaobK1TltjTRB+ffQru3Rsmpmd6t6Rn3tkUBfPuiRhvYV+VSFDD0DeVOsRFhyiElXjwwwS75E99YTZcfPCr+ABIgxQF1Cz5hEtATG3kAuKk5ekG+dGh30v8HHpAgMBAAECggEADY604jUcqjZ5M0X3kS4qDcdotCPmMyEd+smD3c60UdtZJgCGymALXhzFT4k0KDt+djmJ5e3r6m9DN72BdRyPjvi+lY7HEhtK9mBgNQ+3w+cNFDoAI0zNwfWC8hdkUnTf1fg82n7WAPwhsOi4WLQF6zUGOlli3EpGPRUVvoBIVw2oxI3ajMAbjBLk2ygVFriC9eeLEUUKU/8XnNHnFIv3sZ2gbIrRhfrQhdU5Sj7GV3XcaA5gMM4caY/vL0hApvXJmINc1sh+yvZiK+fbiUX9AZ5AI7NHIR7uVaYUmm5YJnN+QVf55GdY3lhpENsBhT17/5CfEv5UDN9ESz1za30ifwKBgQDiiyzot6fjtzXRIcgUgfLoaAF6eOQz9kL5pEaWxoU/oPn5baxBZlKAskEzLGmKH2CLZVa/QojrSzSwU1Y5KCWgkPfNcYrW/JwWqYXDFNVFf5G1jZSgUnLGJzYlPUxwIhMqyotuIfIXyTAkIr5DVEEIcl2nedq+OLmqpSemBsl6ywKBgQDDgqhPq8pcLS/cYBks4+S2dUcxC4NNRX2BISAK7yRcVnlNfq6zPjsuSnKrK2Uz9ZMCieMv+8TnCu/E2f4wdAZY7AMAUXY5MGKfD5QJjrNNJ9/Jwj6CU+vhGNeLCHbnXSLhjZQR8XhSjsPpCH2Zvfs+09vlUXsb6H04yqmCa5krmwKBgCcnhNCIBYVvAecuXoVk3FXcu/yt7kso0fSRtv2SPC9+JBY/JVl8VOAXbvtoPql3uNj+39pYpo98RRQAo1Nno/E3fQzOBgAYUuX70Lmh3SaQ4N8vJDP9bAbhehM/n/l1ejiYHzceuNfxltd4BchfRIowMg2jGvJrnXNn7g1c+O+3AoGARGkvo9Ek2eQl3yLxLDgCC6Y15v90TlGGfOyuXaUYQAcyIt+1GZso8iWywhpJK0k+y6CcEmoyl/XHuEXfH+05W1waIL5gc9puZ+H3lvCL5O+S6vJBFTFwVC9xHIE1ufdpASudKDoqNBkojB4gdO95Pp67Tx6XyUgsqPahIbWsvwkCgYAtB+2ZmG4tSnJr2jkdMu+nUhMrAJquBcBhn+fHi1+q+9j5XvArTmFqBzBCF1gi+u+TkX8YYM/2LkEDmnpE7IMXKusOCAfp7vXyfBWO0Cvfyz0U5jRnI8Wu0j3PKtXBdTGKyC7SAoXQGuVfYNtmBItsCHtZ5DQ6BhxP5BWdgdrATg==";
const TEST_N: &str = "rQOi0-iDxG_uikoq4KgNjl39VeUl9iVK5Lt3FKQcCwzJW6GgFeAiNExwkxSY6K2b8Fjv2OjJm4NcJWxB1OGmf2Eonf1jquA5gmWr4GRWa12j_g9SY_6OqAE90Wht9aQkiBd4WOpQy-O4K2Oi2mhD9K5xkykC5mjCYR9FizR1T8o1Ia3LeTA24OehfwkSeuIyloX1QnWXkVBZd6_a4k4GBlJBBkn12qGytU5bY00Qfn30K7t0bJqZnerekZ97ZFAXz7okYb2FflUhQw9A3lTrERYcohJV48MMEu-RPfWE2XHzwq_gASIMUBdQs-YRLQExt5ALipOXpBvnRod9L_Bx6Q";

fn jwks() -> Value {
    json!({"keys":[{"kty":"RSA","kid":"k1","alg":"RS256","use":"sig","n":TEST_N,"e":"AQAB"}]})
}

fn sign(claims: Value) -> String {
    let der = base64::engine::general_purpose::STANDARD.decode(TEST_KEY).unwrap();
    let key = ring::signature::RsaKeyPair::from_pkcs8(&der).unwrap();
    let header = pkce::base64url(json!({"alg":"RS256","kid":"k1","typ":"JWT"}).to_string().as_bytes());
    let payload = pkce::base64url(claims.to_string().as_bytes());
    let message = format!("{header}.{payload}");
    let mut signature = vec![0u8; key.public().modulus_len()];
    key.sign(&ring::signature::RSA_PKCS1_SHA256, &ring::rand::SystemRandom::new(), message.as_bytes(), &mut signature)
        .unwrap();
    format!("{message}.{}", pkce::base64url(&signature))
}

#[test]
fn id_token_verification() {
    use flows::chatgpt::verify_id_token_with_jwks as verify;
    let issuer = "https://auth.openai.com";
    let claims = json!({"iss":issuer,"aud":"client_1","exp":2000,"sub":"user","nonce":"n1"});
    let token = sign(claims.clone());
    assert!(verify(&token, &jwks(), "client_1", "n1", issuer, 1000).is_ok());
    assert_eq!(verify(&token, &jwks(), "client_1", "n2", issuer, 1000).unwrap_err(), "ID token nonce does not match this sign-in attempt");
    assert_eq!(verify(&token, &jwks(), "client_2", "n1", issuer, 1000).unwrap_err(), "unexpected aud");
    assert_eq!(verify(&token, &jwks(), "client_1", "n1", issuer, 2000).unwrap_err(), "expired");
    assert_eq!(verify(&token, &jwks(), "client_1", "n1", "https://evil", 1000).unwrap_err(), "unexpected iss");
    // Tampered payload.
    let parts: Vec<&str> = token.split('.').collect();
    let forged = format!("{}.{}.{}", parts[0], pkce::base64url(json!({"iss":issuer,"aud":"client_1","exp":2000,"sub":"other","nonce":"n1"}).to_string().as_bytes()), parts[2]);
    assert_eq!(verify(&forged, &jwks(), "client_1", "n1", issuer, 1000).unwrap_err(), "signature verification failed");
    let no_sub = sign(json!({"iss":issuer,"aud":["client_1"],"exp":2000,"sub":"  ","nonce":"n1"}));
    assert_eq!(verify(&no_sub, &jwks(), "client_1", "n1", issuer, 1000).unwrap_err(), "ID token subject is missing");
}

// ---------------------------------------------------------------------------------------------
// Authorize URLs
// ---------------------------------------------------------------------------------------------

#[test]
fn exact_authorize_urls() {
    let pkce = pkce::Pkce { verifier: "v".into(), challenge: "CHAL".into() };
    assert_eq!(
        flows::chatgpt::authorize_url("https://auth.openai.com", "http://127.0.0.1:5555/auth/callback", &pkce, "ST", "NO", None, "urn:uuid:1"),
        "https://auth.openai.com/api/accounts/authorize?client_id=dynamic_agent_client&agent_name_hint=OpenCode&ext_agent_host_id=urn%3Auuid%3A1&response_type=code&redirect_uri=http%3A%2F%2F127.0.0.1%3A5555%2Fauth%2Fcallback&scope=openid+profile+email+offline_access+resource.invoke+chatgpt.tokens.use.direct&resource=https%3A%2F%2Fapi.openai.com%2Fv1&state=ST&nonce=NO&code_challenge_method=S256&code_challenge=CHAL"
    );
    assert_eq!(
        flows::chatgpt::authorize_url("https://auth.openai.com", "r", &pkce, "ST", "NO", Some("app_saved"), "h"),
        "https://auth.openai.com/api/accounts/authorize?client_id=app_saved&ext_agent_host_id=h&response_type=code&redirect_uri=r&scope=openid+profile+email+offline_access+resource.invoke+chatgpt.tokens.use.direct&resource=https%3A%2F%2Fapi.openai.com%2Fv1&state=ST&nonce=NO&code_challenge_method=S256&code_challenge=CHAL"
    );
    assert_eq!(
        flows::codex::authorize_url("https://auth.openai.com", "http://localhost:1455/auth/callback", &pkce, "ST"),
        "https://auth.openai.com/oauth/authorize?response_type=code&client_id=app_EMoamEEZ73f0CkXaXp7hrann&redirect_uri=http%3A%2F%2Flocalhost%3A1455%2Fauth%2Fcallback&scope=openid+profile+email+offline_access&code_challenge=CHAL&code_challenge_method=S256&id_token_add_organizations=true&codex_cli_simplified_flow=true&state=ST&originator=opencode"
    );
    assert_eq!(flows::snowflake::scope_for_role(""), "refresh_token");
    assert_eq!(flows::snowflake::scope_for_role("ANALYST_1"), "refresh_token session:role:ANALYST_1");
    assert_eq!(flows::snowflake::scope_for_role("my role"), "refresh_token session:role-encoded:my%20role");
    assert_eq!(flows::snowflake::normalize_account(Some(&json!(" https://Org-Acct.snowflakecomputing.com/ "))), "Org-Acct");
    assert_eq!(
        flows::snowflake::endpoint(None, "acct"),
        "https://acct.snowflakecomputing.com/api/v2/cortex/v1"
    );
    assert_eq!(flows::copilot::normalize_domain("https://company.ghe.com/"), "company.ghe.com");
    assert_eq!(flows::copilot::copilot_base_url(None), "https://api.githubcopilot.com");
    let mut metadata = serde_json::Map::new();
    metadata.insert("enterpriseUrl".into(), json!("company.ghe.com"));
    assert_eq!(flows::copilot::copilot_base_url(Some(&metadata)), "https://copilot-api.company.ghe.com");
}

#[test]
fn callback_pages() {
    let success = page::success(&page::CallbackPageOptions::provider("ChatGPT"));
    assert!(success.starts_with("<!doctype html>"));
    assert!(success.contains("<title>Authorization successful · OpenCode</title>"));
    assert!(success.contains("OpenCode is now connected to ChatGPT."));
    assert!(success.contains("setTimeout(function(){try{window.close()}catch(e){}},2500)"));
    assert!(success.contains(r#"<pre class="detail" id="oc-detail" hidden></pre>"#));
    assert!(success.contains("--oc-bg: #f8f8f8;"));
    let error = page::error("bad <code>", &page::CallbackPageOptions::default());
    assert!(error.contains("<title>Authorization failed · OpenCode</title>"));
    assert!(error.contains("OpenCode couldn&#39;t complete authorization.") || error.contains("OpenCode couldn't complete authorization."));
    assert!(error.contains(r#"<pre class="detail" id="oc-detail">bad &lt;code&gt;</pre>"#));
    let bootstrap = page::bootstrap("/auth/token", Some("DigitalOcean"));
    assert!(bootstrap.contains("var PROVIDER=\"DigitalOcean\";\nvar TOKEN_URL=new URL(\"/auth/token\",window.location.origin).href;"));
    assert!(bootstrap.contains("Completing your DigitalOcean authorization."));
}

// ---------------------------------------------------------------------------------------------
// Flows against a local mock
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn chatgpt_token_sharing_loopback_flow() {
    let id_token = Arc::new(Mutex::new(String::new()));
    let served = id_token.clone();
    let server = mock(move |request, _| match request.path.as_str() {
        "/api/accounts/oauth/token" => json_response(
            200,
            json!({
                "access_token": "at",
                "refresh_token": "rt",
                "id_token": served.lock().clone(),
                "expires_in": 100,
                "scope": "openid chatgpt.tokens.use.direct"
            }),
        ),
        "/jwks" => json_response(200, jwks()),
        _ => Response::not_found("nope"),
    })
    .await;
    let storage = Arc::new(MemoryKv::new());
    let mut flow = ChatGptTokenSharing::new(ctx(), storage.clone(), Arc::new(|| None));
    flow.issuer = server.base.clone();
    flow.jwks_url = format!("{}/jwks", server.base);
    let scope = CancellationToken::new();
    let result = flow.authorize(FormAnswer::new(), scope.clone()).await.unwrap();
    assert_eq!(result.mode(), AttemptMode::Auto);
    assert_eq!(result.instructions, "Complete authorization in your browser. This window will close automatically.");
    let url = result.url.clone();
    assert!(url.starts_with(&format!("{}/api/accounts/authorize?client_id=dynamic_agent_client&agent_name_hint=OpenCode&ext_agent_host_id=urn%3Auuid%3A", server.base)));
    assert_eq!(keys(&url), ["client_id", "agent_name_hint", "ext_agent_host_id", "response_type", "redirect_uri", "scope", "resource", "state", "nonce", "code_challenge_method", "code_challenge"]);
    let host_id = param(&url, "ext_agent_host_id");
    assert_eq!(storage.get("chatgpt-agent-host-id").await.unwrap(), Some(json!(host_id)));
    let issuer = server.base.clone();
    *id_token.lock() = sign(json!({"iss": issuer, "aud":"client_x", "exp": now_ms() / 1000 + 600, "sub":"u", "nonce": param(&url, "nonce")}));
    let redirect = param(&url, "redirect_uri");
    let state = param(&url, "state");
    let finish = tokio::spawn(result.finish(None));

    // Wrong path → 404.
    let base = redirect.trim_end_matches("/auth/callback");
    let response = client().get(format!("{base}/other")).send().await.unwrap();
    assert_eq!(response.status(), 404);
    let response = client()
        .get(format!("{redirect}?code=abc&state={state}&client_id=client_x"))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    assert!(response.text().await.unwrap().contains("OpenCode is now connected to ChatGPT."));
    let value = finish.await.unwrap().unwrap();
    assert_eq!(value.method_id, "chatgpt-token-sharing");
    assert_eq!(value.access, "at");
    assert_eq!(value.refresh, "rt");
    assert_eq!(
        serde_json::to_string(value.metadata.as_ref().unwrap()).unwrap(),
        r#"{"clientID":"client_x","scopes":["openid","chatgpt.tokens.use.direct"]}"#
    );
    let token = server.requests_to("/api/accounts/oauth/token").remove(0);
    assert_eq!(header(&token, "user-agent"), Some(UA));
    let body: Vec<String> = body_pairs(&token).into_iter().map(|(k, _)| k).collect();
    assert_eq!(body, ["grant_type", "client_id", "code", "code_verifier", "redirect_uri", "resource"]);
    // A second callback is rejected.
    let response = client().get(format!("{redirect}?code=abc&state={state}")).send().await.unwrap();
    assert_eq!(response.status(), 409);
    scope.cancel();

    // Second sign-in reuses the stored host id and the saved client id.
    let mut flow = ChatGptTokenSharing::new(ctx(), storage, Arc::new(|| Some("client_saved".into())));
    flow.issuer = server.base.clone();
    let scope = CancellationToken::new();
    let result = flow.authorize(FormAnswer::new(), scope.clone()).await.unwrap();
    assert_eq!(param(&result.url, "ext_agent_host_id"), host_id);
    assert_eq!(param(&result.url, "client_id"), "client_saved");
    assert!(!keys(&result.url).contains(&"agent_name_hint".to_string()));
    let redirect = param(&result.url, "redirect_uri");
    let finish = tokio::spawn(result.finish(None));
    let response = client().get(format!("{redirect}?code=abc&state=wrong")).send().await.unwrap();
    assert_eq!(response.status(), 400);
    assert_eq!(finish.await.unwrap().unwrap_err().message, "Invalid OAuth state");
    scope.cancel();
}

#[tokio::test]
async fn chatgpt_refresh_requires_client_id() {
    let server = mock(|_, _| json_response(200, json!({"access_token":"a2","refresh_token":"r2"}))).await;
    let url = format!("{}/token", server.base);
    let value = OAuthValue { method_id: "chatgpt-token-sharing".into(), refresh: "r".into(), access: "a".into(), expires: 0, metadata: None };
    let error = flows::chatgpt::refresh(&ctx(), &url, &value).await.unwrap_err();
    assert_eq!(error.message, "This ChatGPT connection has no registered client ID. Connect again.");
    let mut metadata = serde_json::Map::new();
    metadata.insert("clientID".into(), json!("c1"));
    metadata.insert("scopes".into(), json!(["s1"]));
    metadata.insert("models".into(), json!([]));
    let value = OAuthValue { metadata: Some(metadata), ..value };
    let next = flows::chatgpt::refresh(&ctx(), &url, &value).await.unwrap();
    assert_eq!(next.access, "a2");
    assert_eq!(serde_json::to_string(next.metadata.as_ref().unwrap()).unwrap(), r#"{"clientID":"c1","scopes":["s1"]}"#);
    let body = body_pairs(&server.requests_to("/token")[0]);
    assert_eq!(
        body,
        [("grant_type".into(), "refresh_token".into()), ("client_id".into(), "c1".into()), ("refresh_token".into(), "r".into()), ("resource".into(), "https://api.openai.com/v1".into())]
    );
    assert!(flows::chatgpt::forbids_retry(Some(r#"{"error":{"code":"subscription_sharing_invalid_user"}}"#)));
    assert!(!flows::chatgpt::forbids_retry(Some("subscription_sharing_usage_unavailable")));
}

#[tokio::test]
async fn codex_device_flow() {
    let access = fake_jwt(json!({"https://api.openai.com/auth":{"chatgpt_account_id":"acct_1"}}));
    let served = access.clone();
    let server = mock(move |request, n| match request.path.as_str() {
        "/api/accounts/deviceauth/usercode" => json_response(200, json!({"device_auth_id":"dev1","user_code":"ABCD-1234","interval":"0"})),
        "/api/accounts/deviceauth/token" if n < 2 => json_response(403, json!({"error":"pending"})),
        "/api/accounts/deviceauth/token" => json_response(200, json!({"authorization_code":"code1","code_verifier":"ver1"})),
        "/oauth/token" => json_response(200, json!({"id_token": fake_jwt(json!({})), "access_token": served.clone(), "refresh_token":"rt"})),
        _ => Response::not_found("nope"),
    })
    .await;
    let mut flow = CodexDevice::new(ctx());
    flow.issuer = server.base.clone();
    flow.timing = PollTiming::fast();
    let result = flow.authorize(FormAnswer::new(), CancellationToken::new()).await.unwrap();
    assert_eq!(result.url, format!("{}/codex/device", server.base));
    assert_eq!(result.instructions, "Enter code: ABCD-1234");
    let value = result.finish(None).await.unwrap();
    assert_eq!(value.method_id, "chatgpt-headless");
    assert_eq!(value.access, access);
    assert_eq!(value.metadata.unwrap()["accountID"], json!("acct_1"));
    assert_eq!(server.requests_to("/api/accounts/deviceauth/token").len(), 3);
    let usercode = &server.requests_to("/api/accounts/deviceauth/usercode")[0];
    assert_eq!(String::from_utf8_lossy(&usercode.body), r#"{"client_id":"app_EMoamEEZ73f0CkXaXp7hrann"}"#);
    let exchange = body_pairs(&server.requests_to("/oauth/token")[0]);
    assert_eq!(
        exchange,
        [
            ("grant_type".into(), "authorization_code".into()),
            ("code".into(), "code1".into()),
            ("redirect_uri".into(), format!("{}/deviceauth/callback", server.base)),
            ("client_id".into(), "app_EMoamEEZ73f0CkXaXp7hrann".into()),
            ("code_verifier".into(), "ver1".into()),
        ]
    );

    // Unexpected status fails.
    let server = mock(|request, _| match request.path.as_str() {
        "/api/accounts/deviceauth/usercode" => json_response(200, json!({"device_auth_id":"d","user_code":"u","interval":"1"})),
        _ => json_response(500, json!({})),
    })
    .await;
    let mut flow = CodexDevice::new(ctx());
    flow.issuer = server.base.clone();
    flow.timing = PollTiming::fast();
    let result = flow.authorize(FormAnswer::new(), CancellationToken::new()).await.unwrap();
    assert_eq!(result.finish(None).await.unwrap_err().message, "Device authorization failed: 500");
}

#[tokio::test]
async fn codex_browser_flow_and_refresh() {
    let server = mock(|request, _| match request.path.as_str() {
        "/oauth/token" => json_response(200, json!({"id_token": fake_jwt(json!({"chatgpt_account_id":"acct_9"})), "access_token":"at","refresh_token":"rt","expires_in":10})),
        _ => Response::not_found("nope"),
    })
    .await;
    let mut flow = CodexBrowser::new(ctx());
    flow.issuer = server.base.clone();
    flow.port = free_port();
    flow.fallback_port = free_port();
    let scope = CancellationToken::new();
    let result = flow.authorize(FormAnswer::new(), scope.clone()).await.unwrap();
    let redirect = param(&result.url, "redirect_uri");
    assert_eq!(redirect, format!("http://localhost:{}/auth/callback", flow.port));
    let state = param(&result.url, "state");
    let finish = tokio::spawn(result.finish(None));
    let local = redirect.replace("localhost", "127.0.0.1");
    let response = client().get(format!("{local}?code=c1&state={state}")).send().await.unwrap();
    assert_eq!(response.status(), 200);
    let value = finish.await.unwrap().unwrap();
    assert_eq!(value.method_id, "chatgpt-browser");
    assert_eq!(value.metadata.as_ref().unwrap()["accountID"], json!("acct_9"));
    scope.cancel();

    // Refresh keeps metadata when the new tokens carry no account id.
    let server = mock(|_, _| json_response(200, json!({"access_token":"a2","refresh_token":"r2"}))).await;
    let next = flows::codex::refresh(&ctx(), &server.base, "chatgpt-browser", &value).await.unwrap();
    assert_eq!(next.access, "a2");
    assert_eq!(next.metadata, value.metadata);
    assert_eq!(
        body_pairs(&server.requests_to("/oauth/token")[0]),
        [("grant_type".into(), "refresh_token".into()), ("refresh_token".into(), "rt".into()), ("client_id".into(), "app_EMoamEEZ73f0CkXaXp7hrann".into())]
    );
}

#[tokio::test]
async fn codex_browser_port_fallback() {
    // Occupy the primary port with a listener that never answers `/cancel` meaningfully.
    let blocker = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let busy = blocker.local_addr().unwrap().port();
    let mut flow = CodexBrowser::new(ctx());
    flow.port = busy;
    flow.fallback_port = free_port();
    let scope = CancellationToken::new();
    let result = flow.authorize(FormAnswer::new(), scope.clone()).await.unwrap();
    assert_eq!(param(&result.url, "redirect_uri"), format!("http://localhost:{}/auth/callback", flow.fallback_port));
    scope.cancel();
}

#[tokio::test]
async fn copilot_device_flow() {
    let server = mock(|request, n| match request.path.as_str() {
        "/login/device/code" => json_response(200, json!({"verification_uri":"https://github.com/login/device","user_code":"WXYZ","device_code":"dc","interval":0})),
        "/login/oauth/access_token" if n == 0 => json_response(200, json!({"error":"authorization_pending"})),
        "/login/oauth/access_token" if n == 1 => json_response(200, json!({"error":"slow_down","interval":0})),
        "/login/oauth/access_token" => json_response(200, json!({"access_token":"gho_1"})),
        "/copilot_internal/user" => json_response(200, json!({"chat_enabled":true,"endpoints":{"api":"https://api.business.githubcopilot.com/"}})),
        _ => Response::not_found("nope"),
    })
    .await;
    let mut flow = CopilotDevice::new(ctx());
    flow.base_override = Some(server.base.clone());
    flow.timing = PollTiming::fast();
    let mut answer = FormAnswer::new();
    answer.insert("deploymentType".into(), json!("github.com"));
    let result = flow.authorize(answer, CancellationToken::new()).await.unwrap();
    assert_eq!(result.url, "https://github.com/login/device");
    assert_eq!(result.instructions, "Enter code: WXYZ");
    let value = result.finish(None).await.unwrap();
    assert_eq!(value.access, "gho_1");
    assert_eq!(value.refresh, "gho_1");
    assert_eq!(value.expires, 0);
    assert_eq!(serde_json::to_string(&value.metadata).unwrap(), r#"{"apiEndpoint":"https://api.business.githubcopilot.com"}"#);
    let device = &server.requests_to("/login/device/code")[0];
    assert_eq!(String::from_utf8_lossy(&device.body), r#"{"client_id":"Ov23li8tweQw6odWQebz","scope":"read:user"}"#);
    assert_eq!(header(device, "accept"), Some("application/json"));
    let token = &server.requests_to("/login/oauth/access_token")[0];
    assert_eq!(
        String::from_utf8_lossy(&token.body),
        r#"{"client_id":"Ov23li8tweQw6odWQebz","device_code":"dc","grant_type":"urn:ietf:params:oauth:grant-type:device_code"}"#
    );
    let user = &server.requests_to("/copilot_internal/user")[0];
    assert_eq!(header(user, "authorization"), Some("Bearer gho_1"));
    assert_eq!(header(user, "x-github-api-version"), Some("2025-04-01"));

    // Enterprise without URL.
    let mut answer = FormAnswer::new();
    answer.insert("deploymentType".into(), json!("enterprise"));
    assert_eq!(flow.authorize(answer, CancellationToken::new()).await.unwrap_err().message, "Enterprise URL is required");

    // Entitlement denial and other errors.
    let server = mock(|request, _| match request.path.as_str() {
        "/login/device/code" => json_response(200, json!({"verification_uri":"v","user_code":"u","device_code":"d","interval":1})),
        "/login/oauth/access_token" => json_response(200, json!({"access_token":"gho_2"})),
        "/copilot_internal/user" => json_response(200, json!({"chat_enabled":false,"can_signup_for_limited":true})),
        _ => Response::not_found("nope"),
    })
    .await;
    flow.base_override = Some(server.base.clone());
    let result = flow.authorize(FormAnswer::new(), CancellationToken::new()).await.unwrap();
    assert_eq!(
        result.finish(None).await.unwrap_err().message,
        "This GitHub account is not signed up for GitHub Copilot. Sign up for Copilot Free at https://github.com/features/copilot/plans and connect again."
    );
    let server = mock(|request, _| match request.path.as_str() {
        "/login/device/code" => json_response(200, json!({"verification_uri":"v","user_code":"u","device_code":"d","interval":1})),
        _ => json_response(200, json!({"error":"access_denied"})),
    })
    .await;
    flow.base_override = Some(server.base.clone());
    let result = flow.authorize(FormAnswer::new(), CancellationToken::new()).await.unwrap();
    assert_eq!(result.finish(None).await.unwrap_err().message, "Device authorization failed: access_denied");
}

#[tokio::test]
async fn console_device_flow_and_refresh() {
    let server = mock(|request, n| match request.path.as_str() {
        "/console/auth/device/code" => json_response(200, json!({"device_code":"dc","user_code":"UC","verification_uri_complete":"device?code=UC","expires_in":600,"interval":0})),
        "/console/auth/device/token" => {
            let body: Value = serde_json::from_slice(&request.body).unwrap();
            if body["grant_type"] == json!("refresh_token") {
                if body["refresh_token"] == json!("revoked") {
                    return json_response(400, json!({"error":"invalid_grant"}));
                }
                return json_response(200, json!({"access_token":"a3","refresh_token":"r3","expires_in":60,"org_id":"o9"}));
            }
            if n == 0 {
                json_response(400, json!({"error":"authorization_pending"}))
            } else {
                json_response(200, json!({"access_token":"a1","refresh_token":"r1","expires_in":60,"org_id":null}))
            }
        }
        "/console/api/user" => json_response(200, json!({"id":"usr_1","email":"me@example.com"})),
        "/console/api/orgs" => json_response(200, json!([{"id":"o2","name":"beta"},{"id":"o1","name":"Alpha"}])),
        _ => Response::not_found("nope"),
    })
    .await;
    let mut flow = ConsoleDevice::new(ctx());
    flow.timing = PollTiming::fast();
    let mut answer = FormAnswer::new();
    answer.insert("server".into(), json!(format!("{}/console/", server.base)));
    let result = flow.authorize(answer, CancellationToken::new()).await.unwrap();
    assert_eq!(result.url, format!("{}/console/device?code=UC", server.base));
    assert_eq!(result.instructions, "Enter code: UC");
    let value = result.finish(None).await.unwrap();
    assert_eq!(value.method_id, "device");
    assert_eq!(
        serde_json::to_string(value.metadata.as_ref().unwrap()).unwrap(),
        format!(r#"{{"server":"{}/console","accountID":"usr_1","email":"me@example.com","orgID":"o1","orgName":"Alpha"}}"#, server.base)
    );
    assert_eq!(flow.label(&value).as_deref(), Some("Alpha"));
    let code = &server.requests_to("/console/auth/device/code")[0];
    assert_eq!(String::from_utf8_lossy(&code.body), r#"{"client_id":"opencode-cli","supports_org_scope":true}"#);
    assert_eq!(header(code, "user-agent"), Some(UA));

    let refreshed = flow.refresh(value.clone()).await.unwrap();
    assert_eq!(refreshed.access, "a3");
    let metadata = refreshed.metadata.unwrap();
    assert_eq!(metadata["orgID"], json!("o9"));
    assert_eq!(metadata["orgName"], json!("o9"));
    let revoked = OAuthValue { refresh: "revoked".into(), ..value };
    assert_eq!(flow.refresh(revoked).await.unwrap_err().message, "Reconnect OpenCode Console to continue.");

    let mut answer = FormAnswer::new();
    answer.insert("server".into(), json!("ftp://x"));
    assert_eq!(flow.authorize(answer, CancellationToken::new()).await.unwrap_err().message, "Invalid OpenCode server URL: expected HTTP(S)");
}

#[tokio::test]
async fn xai_device_flow() {
    let access = fake_jwt(json!({"exp": 4_000_000_000u64}));
    let served = access.clone();
    let server = mock(move |request, n| match request.path.as_str() {
        "/device/code" => json_response(200, json!({"device_code":"dc","user_code":"UC","verification_uri":"https://accounts.x.ai/device","verification_uri_complete":"https://accounts.x.ai/device?code=UC","expires_in":300,"interval":1})),
        "/token" if n == 0 => json_response(400, json!({"error":"authorization_pending"})),
        "/token" if n == 1 => json_response(400, json!({"error":"slow_down"})),
        "/token" => json_response(200, json!({"access_token": served.clone(), "refresh_token":"rt"})),
        _ => Response::not_found("nope"),
    })
    .await;
    let mut flow = XaiDevice::new(ctx());
    flow.issuer = server.base.clone();
    flow.timing = PollTiming::fast();
    let before = now_ms();
    let result = flow.authorize(FormAnswer::new(), CancellationToken::new()).await.unwrap();
    assert_eq!(result.url, "https://accounts.x.ai/device?code=UC");
    assert_eq!(result.instructions, "Open https://accounts.x.ai/device on any device and enter code: UC");
    let expires_at = result.expires_at.unwrap();
    assert!(expires_at >= before + 300_000 && expires_at <= now_ms() + 300_000);
    let value = result.finish(None).await.unwrap();
    assert_eq!(value.method_id, "device");
    assert_eq!(value.expires, 4_000_000_000_000);
    assert_eq!(
        body_pairs(&server.requests_to("/device/code")[0]),
        [("client_id".into(), "b1a00492-073a-47ea-816f-4c329264a828".into()), ("scope".into(), "openid profile email offline_access grok-cli:access api:access".into()), ("referrer".into(), "opencode".into())]
    );
    let poll = body_pairs(&server.requests_to("/token")[0]);
    assert_eq!(poll[0], ("grant_type".into(), "urn:ietf:params:oauth:grant-type:device_code".into()));

    let server = mock(|request, _| match request.path.as_str() {
        "/device/code" => json_response(200, json!({"device_code":"dc","user_code":"UC","verification_uri":"v"})),
        _ => json_response(400, json!({"error":"access_denied"})),
    })
    .await;
    flow.issuer = server.base.clone();
    let result = flow.authorize(FormAnswer::new(), CancellationToken::new()).await.unwrap();
    assert_eq!(result.expires_at, None);
    assert_eq!(result.url, "v");
    assert_eq!(result.finish(None).await.unwrap_err().message, "xAI device authorization was denied");

    let legacy = OAuthValue { method_id: "browser".into(), refresh: "r".into(), access: "a".into(), expires: 1, metadata: None }.into_value();
    assert_eq!(flows::xai::migrate_legacy(&legacy).unwrap().method_id(), Some("device"));
}

#[tokio::test]
async fn gitlab_pkce_flow() {
    let server = mock(|request, _| {
        let body = body_pairs(request);
        let code = body.iter().find(|(k, _)| k == "code").map(|(_, v)| v.clone());
        if code.as_deref() == Some("stale") {
            return json_response(400, json!({"error":"invalid_grant","error_description":"The provided authorization grant is invalid"}));
        }
        json_response(200, json!({"access_token":"glat","refresh_token":"glrt","expires_in":7200}))
    })
    .await;
    let port = free_port();
    let env = map_env([("GITLAB_OAUTH_CLIENT_ID", " custom-app ")]);
    let mut flow = GitLabPkce::new(ctx(), env);
    flow.callback_port = port;
    let mut answer = FormAnswer::new();
    answer.insert("instanceUrl".into(), json!(format!("{}/gitlab/", server.base)));
    let scope = CancellationToken::new();
    let result = flow.authorize(answer.clone(), scope.clone()).await.unwrap();
    assert!(result.url.starts_with(&format!("{}/gitlab/oauth/authorize?client_id=custom-app&redirect_uri=http%3A%2F%2F127.0.0.1%3A{port}%2Fcallback&response_type=code&state=", server.base)));
    assert_eq!(keys(&result.url), ["client_id", "redirect_uri", "response_type", "state", "scope", "code_challenge", "code_challenge_method"]);
    assert_eq!(param(&result.url, "scope"), "api");
    let state = param(&result.url, "state");
    let finish = tokio::spawn(result.finish(None));
    let response = client().post(format!("http://127.0.0.1:{port}/callback")).send().await.unwrap();
    assert_eq!(response.status(), 404);
    let response = client().get(format!("http://127.0.0.1:{port}/callback?code=good&state={state}")).send().await.unwrap();
    assert_eq!(response.status(), 200);
    assert!(response.text().await.unwrap().contains("OpenCode is now connected to GitLab."));
    let value = finish.await.unwrap().unwrap();
    assert_eq!(value.method_id, "pkce");
    assert_eq!(
        serde_json::to_string(value.metadata.as_ref().unwrap()).unwrap(),
        format!(r#"{{"instanceUrl":"{}/gitlab","clientID":"custom-app"}}"#, server.base)
    );
    assert_eq!(flow.label(&value), Some(server.base.trim_start_matches("http://").to_string()));
    let keys: Vec<String> = body_pairs(&server.requests_to("/gitlab/oauth/token")[0]).into_iter().map(|(k, _)| k).collect();
    assert_eq!(keys, ["grant_type", "code", "redirect_uri", "code_verifier", "client_id"]);
    scope.cancel();
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;

    // Rejected grant → hints.
    let scope = CancellationToken::new();
    let result = flow.authorize(answer, scope.clone()).await.unwrap();
    let state = param(&result.url, "state");
    let finish = tokio::spawn(result.finish(None));
    let response = client().get(format!("http://127.0.0.1:{port}/callback?code=stale&state={state}")).send().await.unwrap();
    assert_eq!(response.status(), 400);
    let message = finish.await.unwrap().unwrap_err().message;
    assert!(message.starts_with("The provided authorization grant is invalid\n\nLikely causes:\n- the authorization code was already used"));
    assert!(message.contains(&format!("- redirect_uri sent: http://127.0.0.1:{port}/callback — the application must register this exactly")));
    assert!(message.contains("- client_id used: custom-app... (from GITLAB_OAUTH_CLIENT_ID)"));
    assert!(message.contains("Unset GITLAB_OAUTH_CLIENT_ID to fall back to the bundled application."));
    scope.cancel();

    // The bundled application only exists on gitlab.com.
    let flow = GitLabPkce::new(ctx(), map_env::<[(&str, &str); 0], &str, &str>([]));
    let mut answer = FormAnswer::new();
    answer.insert("instanceUrl".into(), json!("https://gitlab.example.com"));
    let error = flow.authorize(answer, CancellationToken::new()).await.unwrap_err();
    assert_eq!(
        error.message,
        "The bundled GitLab OAuth application only exists on gitlab.com. To sign in to gitlab.example.com, register an OAuth application there with redirect URI http://127.0.0.1:8080/callback and the `api` scope (not marked \"Confidential\"), then set GITLAB_OAUTH_CLIENT_ID to its application ID."
    );
}

#[tokio::test]
async fn gitlab_refresh_is_single_flight() {
    let server = mock(|_, _| {
        std::thread::sleep(std::time::Duration::from_millis(50));
        json_response(200, json!({"access_token":"new","refresh_token":"rotated"}))
    })
    .await;
    let flow = Arc::new(GitLabPkce::new(ctx(), map_env::<[(&str, &str); 0], &str, &str>([])));
    let mut metadata = serde_json::Map::new();
    metadata.insert("instanceUrl".into(), json!(server.base));
    let value = OAuthValue { method_id: "pkce".into(), refresh: format!("rt-{}", now_ms()), access: "old".into(), expires: 0, metadata: Some(metadata) };
    let (a, b) = tokio::join!(flow.refresh(value.clone()), flow.refresh(value.clone()));
    assert_eq!(a.unwrap().refresh, "rotated");
    assert_eq!(b.unwrap().access, "new");
    // A caller still holding the old token within 60 s reuses the result.
    assert_eq!(flow.refresh(value).await.unwrap().access, "new");
    let requests = server.requests_to("/oauth/token");
    assert_eq!(requests.len(), 1);
    let body = body_pairs(&requests[0]);
    assert_eq!(body.last().unwrap(), &("client_id".to_string(), flows::gitlab::LEGACY_CLIENT_ID.to_string()));
}

#[tokio::test]
async fn poe_flow() {
    let server = mock(|request, _| {
        let code = body_pairs(request).into_iter().find(|(k, _)| k == "code").map(|(_, v)| v);
        match code.as_deref() {
            Some("leak") => json_response(400, json!({"error":"invalid_grant","error_description":"code leak is invalid"})),
            _ => json_response(200, json!({"api_key":"  pk-123  ","api_key_expires_in":null})),
        }
    })
    .await;
    let mut flow = PoeBrowser::new(ctx());
    flow.token_url = format!("{}/token", server.base);
    let scope = CancellationToken::new();
    let result = flow.authorize(FormAnswer::new(), scope.clone()).await.unwrap();
    assert!(result.url.starts_with("https://poe.com/oauth/authorize?response_type=code&client_id=client_728290227fc048cc9262091a1ea197ea&redirect_uri=http%3A%2F%2F127.0.0.1%3A"));
    assert_eq!(keys(&result.url), ["response_type", "client_id", "redirect_uri", "scope", "code_challenge", "code_challenge_method", "state"]);
    let redirect = param(&result.url, "redirect_uri");
    let state = param(&result.url, "state");
    let finish = tokio::spawn(result.finish(None));
    let response = client().get(format!("{redirect}?code=c&state={state}&iss=https://evil.com")).send().await.unwrap();
    assert_eq!(response.status(), 400);
    assert_eq!(finish.await.unwrap().unwrap_err().message, "Invalid OAuth issuer");
    scope.cancel();

    let scope = CancellationToken::new();
    let result = flow.authorize(FormAnswer::new(), scope.clone()).await.unwrap();
    let redirect = param(&result.url, "redirect_uri");
    let state = param(&result.url, "state");
    let finish = tokio::spawn(result.finish(None));
    let response = client().get(format!("{redirect}?code=good&state={state}&iss=https://poe.com")).send().await.unwrap();
    assert_eq!(response.status(), 200);
    let value = finish.await.unwrap().unwrap();
    assert_eq!(value.access, "pk-123");
    assert_eq!(value.refresh, "");
    assert_eq!(value.expires, flows::poe::MAX_EXPIRY);
    scope.cancel();

    let error = flows::poe::exchange_code(&ctx(), &flow.token_url, "leak", "r", "v").await.unwrap_err();
    assert_eq!(error.message, "Poe token exchange failed (400)");
    let expired = OAuthValue { expires: 1, ..value };
    assert_eq!(flow.refresh(expired).await.unwrap_err().message, "Poe API key expired. Log in with Poe again.");
}

#[tokio::test]
async fn digitalocean_implicit_flow() {
    let port = free_port();
    let flow = DigitalOceanImplicit { callback_port: port };
    let scope = CancellationToken::new();
    let result = flow.authorize(FormAnswer::new(), scope.clone()).await.unwrap();
    let state = param(&result.url, "state");
    assert_eq!(
        result.url,
        format!("https://cloud.digitalocean.com/v1/oauth/authorize?response_type=token&client_id=b1a6c5158156caac821fd1b30253ca8acb52454a48fa744420e41889cb589f82&redirect_uri=http%3A%2F%2Flocalhost%3A{port}%2Fauth%2Fcallback&scope=genai%3Aread+inference%3Aquery&state={state}")
    );
    let finish = tokio::spawn(result.finish(None));
    let page = client().get(format!("http://127.0.0.1:{port}/auth/callback")).send().await.unwrap().text().await.unwrap();
    assert!(page.contains("Finishing sign-in"));
    let response = client()
        .post(format!("http://127.0.0.1:{port}/auth/token"))
        .body(json!({"access_token":"do_tok","expires_in":"3600","state":"wrong"}).to_string())
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 400);
    assert_eq!(response.text().await.unwrap(), r#"{"error":"Invalid OAuth state"}"#);
    assert_eq!(finish.await.unwrap().unwrap_err().message, "Invalid OAuth state");
    scope.cancel();
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;

    let scope = CancellationToken::new();
    let result = flow.authorize(FormAnswer::new(), scope.clone()).await.unwrap();
    let state = param(&result.url, "state");
    let finish = tokio::spawn(result.finish(None));
    let response = client()
        .post(format!("http://127.0.0.1:{port}/auth/token"))
        .body(json!({"access_token":"do_tok","expires_in":"0","state":state}).to_string())
        .send()
        .await
        .unwrap();
    assert_eq!(response.text().await.unwrap(), r#"{"ok":true}"#);
    let value = finish.await.unwrap().unwrap();
    assert_eq!(value.access, "do_tok");
    assert_eq!(value.refresh, "");
    let thirty_days = 30 * 24 * 3600 * 1000;
    assert!(value.expires > now_ms() + thirty_days - 60_000);
    scope.cancel();
}

#[tokio::test]
async fn snowflake_flow() {
    let server = mock(|request, _| {
        let body = body_pairs(request);
        if body.iter().any(|(k, v)| k == "grant_type" && v == "refresh_token") {
            return json_response(200, json!({"access_token":"sf2"}));
        }
        json_response(200, json!({"access_token":"sf1","refresh_token":"sfr","expires_in":600}))
    })
    .await;
    let mut flow = SnowflakeBrowser::new(ctx(), map_env::<[(&str, &str); 0], &str, &str>([]));
    flow.issuer_override = Some(server.base.clone());
    let mut answer = FormAnswer::new();
    answer.insert("account".into(), json!("myorg-acct.snowflakecomputing.com"));
    answer.insert("role".into(), json!(" my role "));
    let scope = CancellationToken::new();
    let result = flow.authorize(answer, scope.clone()).await.unwrap();
    assert_eq!(result.instructions, "Complete Snowflake sign-in in your browser.");
    assert_eq!(keys(&result.url), ["client_id", "response_type", "redirect_uri", "state", "scope", "code_challenge", "code_challenge_method"]);
    assert_eq!(param(&result.url, "scope"), "refresh_token session:role-encoded:my%20role");
    assert_eq!(pkce::base64url_decode(&param(&result.url, "code_challenge")).unwrap().len(), 32);
    let redirect = param(&result.url, "redirect_uri");
    assert!(redirect.ends_with('/'));
    let state = param(&result.url, "state");
    let finish = tokio::spawn(result.finish(None));
    let response = client().get(format!("{redirect}?code=c1&state={state}")).send().await.unwrap();
    assert_eq!(response.status(), 200);
    let value = finish.await.unwrap().unwrap();
    assert_eq!(value.metadata.as_ref().unwrap()["account"], json!("myorg-acct"));
    assert_eq!(flow.label(&value).as_deref(), Some("myorg-acct"));
    let token = &server.requests_to("/oauth/token-request")[0];
    assert_eq!(header(token, "authorization"), Some("Basic TE9DQUxfQVBQTElDQVRJT046TE9DQUxfQVBQTElDQVRJT04="));
    let keys: Vec<String> = body_pairs(token).into_iter().map(|(k, _)| k).collect();
    assert_eq!(keys, ["grant_type", "code", "redirect_uri", "code_verifier", "client_id"]);
    scope.cancel();

    // Refresh keeps the old refresh token when none is returned.
    let next = flow.refresh(value.clone()).await.unwrap();
    assert_eq!(next.access, "sf2");
    assert_eq!(next.refresh, "sfr");
    // An env token short-circuits refresh.
    let mut env_flow = SnowflakeBrowser::new(ctx(), map_env([("SNOWFLAKE_CORTEX_PAT", "pat")]));
    env_flow.issuer_override = Some("http://127.0.0.1:1".into());
    assert_eq!(env_flow.refresh(value.clone()).await.unwrap(), value);
}

// ---------------------------------------------------------------------------------------------
// Azure CLI
// ---------------------------------------------------------------------------------------------

struct FakeAz {
    calls: Mutex<Vec<String>>,
    expires_on: u64,
}

#[async_trait::async_trait]
impl oc_ai::credentials::azure::AzCli for FakeAz {
    async fn run(&self, args: &[&str]) -> Result<String, String> {
        self.calls.lock().push(args.join(" "));
        Ok(json!({"accessToken": format!("tok{}", self.calls.lock().len()), "expires_on": self.expires_on}).to_string())
    }
}

#[tokio::test]
async fn azure_cli_tokens() {
    use oc_ai::credentials::azure::{AzureCliTokens, COGNITIVE_SCOPE, FOUNDRY_SCOPE, parse_token, scope_for_url};
    assert_eq!(scope_for_url("https://x.services.ai.azure.com/openai/v1/responses"), FOUNDRY_SCOPE);
    assert_eq!(scope_for_url("https://x.services.ai.azure.com/models/chat"), COGNITIVE_SCOPE);
    assert_eq!(scope_for_url("https://x.openai.azure.com/openai"), COGNITIVE_SCOPE);
    let now = Arc::new(std::sync::atomic::AtomicU64::new(1_000_000_000));
    let clock_now = now.clone();
    let cli = Arc::new(FakeAz { calls: Mutex::new(Vec::new()), expires_on: 1_000_000 + 600 });
    let tokens = AzureCliTokens::with_clock(cli.clone(), Arc::new(move || clock_now.load(std::sync::atomic::Ordering::SeqCst)));
    let first = tokens.token(COGNITIVE_SCOPE).await.unwrap();
    assert_eq!(first.access, "tok1");
    assert_eq!(first.expires, 1_000_600_000);
    assert_eq!(tokens.token(COGNITIVE_SCOPE).await.unwrap().access, "tok1");
    // Within 5 minutes of expiry → re-minted.
    now.store(1_000_600_000 - 300_000, std::sync::atomic::Ordering::SeqCst);
    assert_eq!(tokens.token(COGNITIVE_SCOPE).await.unwrap().access, "tok2");
    assert_eq!(cli.calls.lock()[0], format!("account get-access-token --scope {COGNITIVE_SCOPE} --output json"));
    assert_eq!(tokens.bearer("https://x.services.ai.azure.com/api").await.unwrap(), "Bearer tok3");
    assert_eq!(parse_token(r#"{"accessToken":"a","expiresOn":"nonsense"}"#).unwrap_err(), "Azure CLI returned an invalid token expiration");
    assert!(parse_token(r#"{"accessToken":"a","expiresOn":"2030-01-01 10:00:00.000000"}"#).is_ok());
}

use super::*;
use std::collections::HashMap;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

// ---------- fakes ----------

#[derive(Default)]
struct FakeStore {
    kv: Mutex<HashMap<String, Value>>,
    keys: Mutex<HashMap<String, String>>,
}

#[async_trait]
impl WebSearchStore for FakeStore {
    async fn kv_get(&self, key: &str) -> Option<Value> {
        self.kv.lock().get(key).cloned()
    }
    async fn kv_set(&self, key: &str, value: Value) {
        self.kv.lock().insert(key.into(), value);
    }
    async fn kv_remove(&self, key: &str) {
        self.kv.lock().remove(key);
    }
    async fn api_key(&self, integration_id: &str) -> Option<String> {
        self.keys.lock().get(integration_id).cloned()
    }
}

struct Fake {
    id: String,
    name: String,
    calls: Mutex<Vec<String>>,
    failure: Mutex<Option<ProviderError>>,
    empty: bool,
}

impl Fake {
    fn new(id: &str) -> Arc<Fake> {
        Arc::new(Fake {
            id: id.into(),
            name: id.to_uppercase(),
            calls: Mutex::new(vec![]),
            failure: Mutex::new(None),
            empty: false,
        })
    }
    fn fail(&self, error: Option<ProviderError>) {
        *self.failure.lock() = error;
    }
    fn calls(&self) -> Vec<String> {
        self.calls.lock().clone()
    }
}

#[async_trait]
impl SearchProvider for Fake {
    fn info(&self) -> ProviderInfo {
        ProviderInfo { id: self.id.clone(), name: self.name.clone() }
    }
    async fn execute(&self, query: &str) -> Result<Vec<SearchResult>, ProviderError> {
        self.calls.lock().push(query.into());
        if let Some(error) = self.failure.lock().clone() {
            return Err(error);
        }
        if self.empty {
            return Ok(vec![]);
        }
        Ok(vec![SearchResult {
            url: format!("https://{}.example.com", self.id),
            title: Some(query.into()),
            content: Some(format!("{}: {query}", self.id)),
            published: None,
        }])
    }
}

fn http_error(status: u16, retry_after: Option<&str>) -> ProviderError {
    ProviderError::status(status, "POST", "https://example.com", retry_after.map(str::to_string))
}

fn limited() -> ProviderError {
    http_error(429, None)
}

struct Harness {
    service: WebSearch,
    store: Arc<FakeStore>,
    clock: Arc<Mutex<f64>>,
}

impl Harness {
    fn new() -> Self {
        let store = Arc::new(FakeStore::default());
        let service = WebSearch::empty(store.clone());
        let clock = Arc::new(Mutex::new(0.0));
        let c = clock.clone();
        service.set_clock(move || *c.lock());
        Harness { service, store, clock }
    }
    fn register(&self, id: &str) -> Arc<Fake> {
        let fake = Fake::new(id);
        self.service.add_provider(fake.clone());
        fake
    }
    fn advance(&self, ms: f64) {
        *self.clock.lock() += ms;
    }
    async fn q(&self, query: &str) -> Result<Response, WebSearchError> {
        self.service.query(query, None, None, &|_| {}).await
    }
    async fn qs(&self, query: &str, session: &str) -> Result<Response, WebSearchError> {
        self.service.query(query, None, Some(session), &|_| {}).await
    }
    async fn id(&self, query: &str) -> String {
        self.q(query).await.unwrap().provider_id
    }
}

fn request_provider(error: &WebSearchError) -> &str {
    match error {
        WebSearchError::Request { provider_id, .. } => provider_id,
        other => panic!("expected request error, got {other:?}"),
    }
}

// ---------- service ----------

#[tokio::test]
async fn requires_a_provider_without_registrations() {
    let h = Harness::new();
    assert!(h.service.providers().await.is_empty());
    assert_eq!(h.q("unconfigured").await.unwrap_err(), WebSearchError::ProviderRequired);
}

#[tokio::test]
async fn executes_an_explicit_provider_without_changing_the_default() {
    let h = Harness::new();
    h.register("exa");
    let parallel = h.register("parallel");
    let response = h.service.query("effect", Some("parallel"), None, &|_| {}).await.unwrap();
    assert_eq!(
        response,
        Response {
            provider_id: "parallel".into(),
            results: vec![SearchResult {
                url: "https://parallel.example.com".into(),
                title: Some("effect".into()),
                content: Some("parallel: effect".into()),
                published: None,
            }],
        }
    );
    assert_eq!(h.q("default").await.unwrap_err(), WebSearchError::ProviderRequired);
    assert_eq!(parallel.calls(), vec!["effect"]);
    assert_eq!(
        h.service.query("x", Some("missing"), None, &|_| {}).await.unwrap_err(),
        WebSearchError::ProviderNotFound("missing".into())
    );
}

#[tokio::test]
async fn uses_the_configured_default() {
    let h = Harness::new();
    h.register("exa");
    h.register("parallel");
    assert_eq!(h.q("layers").await.unwrap_err(), WebSearchError::ProviderRequired);
    h.service.set_config_selection(Some(Selection::Provider("parallel".into())));
    assert_eq!(h.id("configured").await, "parallel");
    h.service.set_config_selection(Some(Selection::Provider("exa".into())));
    assert_eq!(h.service.default_provider().await.unwrap().unwrap().id, "exa");
}

#[tokio::test]
async fn persists_the_selected_provider_in_kv() {
    let h = Harness::new();
    h.register("parallel");
    h.service.select(Selection::Provider("parallel".into())).await;
    assert_eq!(h.store.kv.lock().get(PROVIDER_KEY), Some(&json_str("parallel")));
    assert_eq!(h.id("remembered").await, "parallel");
}

fn json_str(s: &str) -> Value {
    Value::String(s.into())
}

#[tokio::test]
async fn keeps_config_above_the_persisted_selection() {
    let h = Harness::new();
    h.register("exa");
    h.register("parallel");
    h.service.select(Selection::Provider("parallel".into())).await;
    h.service.set_config_selection(Some(Selection::Provider("exa".into())));
    assert_eq!(h.id("configured").await, "exa");
}

#[tokio::test]
async fn removes_invalid_persisted_selection() {
    let h = Harness::new();
    h.register("exa");
    h.store.kv.lock().insert(PROVIDER_KEY.into(), Value::Bool(true));
    assert_eq!(h.q("x").await.unwrap_err(), WebSearchError::ProviderRequired);
    assert!(h.store.kv.lock().get(PROVIDER_KEY).is_none());
    h.store.kv.lock().insert(PROVIDER_KEY.into(), Value::Bool(false));
    assert_eq!(h.q("x").await.unwrap_err(), WebSearchError::Disabled);
    assert_eq!(h.store.kv.lock().get(PROVIDER_KEY), Some(&Value::Bool(false)));
}

#[tokio::test]
async fn keeps_the_random_provider_across_queries_and_default_lookups() {
    let h = Harness::new();
    h.register("exa");
    h.register("parallel");
    h.service.set_config_selection(Some(Selection::Random));
    let first = h.id("first").await;
    assert!(["exa", "parallel"].contains(&first.as_str()));
    assert_eq!(h.service.default_provider().await.unwrap().unwrap().id, first);
    let results = futures::future::join_all((0..10).map(|_| h.q("next"))).await;
    assert!(results.into_iter().all(|r| r.unwrap().provider_id == first));
}

#[tokio::test]
async fn preserves_persisted_random_selection() {
    let h = Harness::new();
    h.register("exa");
    h.register("parallel");
    h.store.kv.lock().insert(PROVIDER_KEY.into(), json_str("random"));
    let first = h.id("legacy").await;
    assert_eq!(h.id("sticky").await, first);
    h.service.select(Selection::Random).await;
    assert_eq!(h.store.kv.lock().get(PROVIDER_KEY), Some(&json_str("random")));
    assert_eq!(h.id("canonical").await, first);
}

#[tokio::test]
async fn fails_over_on_rate_limits_and_keeps_the_replacement_after_cooldown() {
    let h = Harness::new();
    let exa = h.register("exa");
    let parallel = h.register("parallel");
    h.service.set_config_selection(Some(Selection::Random));
    let first = h.id("first").await;
    let (lim, rep) = if first == "exa" { (exa, parallel) } else { (parallel, exa) };
    lim.fail(Some(limited()));
    let progress = Mutex::new(Vec::<String>::new());
    let response = h.service.query("retry", None, None, &|p| progress.lock().push(p.id.clone())).await.unwrap();
    assert_eq!(response.provider_id, rep.id);
    assert_eq!(*progress.lock(), vec![lim.id.clone(), rep.id.clone()]);
    assert_eq!(lim.calls().last().unwrap(), "retry");
    assert_eq!(rep.calls(), vec!["retry"]);

    lim.fail(None);
    h.advance(59_000.0);
    assert_eq!(h.id("cooling").await, rep.id);
    assert_eq!(lim.calls().len(), 2);
    h.advance(1_000.0);
    assert_eq!(h.id("still sticky").await, rep.id);
    rep.fail(Some(limited()));
    assert_eq!(h.id("recovered").await, lim.id);
}

#[tokio::test]
async fn fails_promptly_when_all_providers_are_cooling_down() {
    let h = Harness::new();
    let providers = [h.register("exa"), h.register("parallel"), h.register("tavily")];
    h.service.select(Selection::Random).await;
    for p in &providers {
        p.fail(Some(limited()));
    }
    assert!(matches!(h.q("limited").await, Err(WebSearchError::Request { .. })));
    assert_eq!(providers.iter().map(|p| p.calls().len()).collect::<Vec<_>>(), vec![1, 1, 1]);
    assert!(h.service.default_provider().await.unwrap().is_some());
    assert!(matches!(h.q("still limited").await, Err(WebSearchError::Request { .. })));
    assert_eq!(providers.iter().map(|p| p.calls().len()).collect::<Vec<_>>(), vec![1, 1, 1]);
}

#[tokio::test]
async fn tries_each_provider_once_even_with_zero_cooldown() {
    let h = Harness::new();
    let providers = [h.register("exa"), h.register("parallel")];
    h.service.select(Selection::Random).await;
    for p in &providers {
        p.fail(Some(http_error(429, Some("0"))));
    }
    assert!(matches!(h.q("limited").await, Err(WebSearchError::Request { .. })));
    assert_eq!(providers.iter().map(|p| p.calls().len()).collect::<Vec<_>>(), vec![1, 1]);
}

#[tokio::test]
async fn respects_retry_after_and_recovers_after_cooldown() {
    for (header, millis) in [
        (Some("120"), 120_000.0),
        (Some("Thu, 01 Jan 1970 00:02:00 GMT"), 120_000.0),
        (None, 60_000.0),
        (Some("invalid"), 60_000.0),
        (Some(""), 60_000.0),
        (Some("-1"), 60_000.0),
    ] {
        let h = Harness::new();
        let provider = h.register("exa");
        h.service.select(Selection::Random).await;
        provider.fail(Some(http_error(429, header)));
        assert!(matches!(h.q("limited").await, Err(WebSearchError::Request { .. })), "{header:?}");
        provider.fail(None);
        h.advance(millis - 1.0);
        assert!(matches!(h.q("early").await, Err(WebSearchError::Request { .. })), "{header:?}");
        assert_eq!(provider.calls().len(), 1, "{header:?}");
        h.advance(1.0);
        assert_eq!(h.id("recovered").await, "exa", "{header:?}");
        assert_eq!(provider.calls().len(), 2);
    }
}

#[tokio::test]
async fn does_not_rotate_or_cool_down_for_other_failures() {
    let h = Harness::new();
    let exa = h.register("exa");
    let parallel = h.register("parallel");
    h.service.select(Selection::Random).await;
    let first = h.id("first").await;
    let (provider, other) = if first == "exa" { (&exa, &parallel) } else { (&parallel, &exa) };
    for cause in [http_error(401, None), http_error(500, None), ProviderError::new("timeout")] {
        provider.fail(Some(cause.clone()));
        let error = h.q("failure").await.unwrap_err();
        assert_eq!(error, WebSearchError::Request { provider_id: first.clone(), error: cause });
        assert_eq!(h.service.default_provider().await.unwrap().unwrap().id, first);
    }
    provider.fail(None);
    assert_eq!(h.id("recovered").await, first);
    assert!(other.calls().is_empty());
}

#[tokio::test]
async fn does_not_fail_over_fixed_or_explicit_providers() {
    let h = Harness::new();
    let exa = h.register("exa");
    let parallel = h.register("parallel");
    exa.fail(Some(limited()));
    h.service.select(Selection::Provider("exa".into())).await;
    assert_eq!(request_provider(&h.q("fixed").await.unwrap_err()), "exa");
    h.service.select(Selection::Random).await;
    let error = h.service.query("explicit", Some("exa"), Some("ses_a"), &|_| {}).await.unwrap_err();
    assert_eq!(request_provider(&error), "exa");
    assert_eq!(exa.calls().len(), 2);
    assert!(parallel.calls().is_empty());
}

#[tokio::test]
async fn reselects_when_the_active_provider_is_removed() {
    let h = Harness::new();
    h.register("exa");
    h.service.select(Selection::Random).await;
    assert_eq!(h.id("first").await, "exa");
    h.register("parallel");
    assert_eq!(h.id("still sticky").await, "exa");
    h.service.remove_provider("exa");
    assert_eq!(h.id("removed").await, "parallel");
}

#[tokio::test]
async fn uses_updated_registrations_for_the_sticky_provider() {
    let h = Harness::new();
    let exa = h.register("exa");
    h.service.select(Selection::Random).await;
    assert_eq!(h.q("original").await.unwrap().results.len(), 1);
    h.service.add_provider(Arc::new(Fake {
        id: "exa".into(),
        name: "Updated Exa".into(),
        calls: Mutex::new(vec![]),
        failure: Mutex::new(None),
        empty: true,
    }));
    assert_eq!(
        h.service.default_provider().await.unwrap(),
        Some(ProviderInfo { id: "exa".into(), name: "Updated Exa".into() })
    );
    assert!(h.q("updated").await.unwrap().results.is_empty());
    h.service.add_provider(exa.clone());
    assert_eq!(h.q("restored").await.unwrap().results.len(), 1);
    assert_eq!(exa.calls(), vec!["original", "restored"]);
}

#[tokio::test]
async fn keeps_independent_session_affinities() {
    let h = Harness::new();
    h.register("exa");
    h.register("parallel");
    h.register("tavily");
    h.service.select(Selection::Random).await;
    let first = futures::future::join_all((0..8).map(|_| h.qs("parallel", "ses_first"))).await;
    let second = futures::future::join_all((0..8).map(|_| h.qs("parallel", "ses_second"))).await;
    let ids = |group: Vec<Result<Response, WebSearchError>>| {
        group.into_iter().map(|r| r.unwrap().provider_id).collect::<HashSet<_>>()
    };
    let first = ids(first);
    let second = ids(second);
    assert_eq!((first.len(), second.len()), (1, 1));
    assert_eq!(&h.qs("later", "ses_first").await.unwrap().provider_id, first.iter().next().unwrap());
    assert_eq!(&h.qs("later", "ses_second").await.unwrap().provider_id, second.iter().next().unwrap());
}

#[tokio::test]
async fn does_not_overwrite_peer_or_location_affinity_when_a_session_switches() {
    let h = Harness::new();
    let exa = h.register("exa");
    h.service.select(Selection::Random).await;
    h.q("location").await.unwrap();
    h.qs("first", "ses_first").await.unwrap();
    h.qs("second", "ses_second").await.unwrap();
    h.register("parallel");
    exa.fail(Some(limited()));
    assert_eq!(h.qs("switch", "ses_first").await.unwrap().provider_id, "parallel");
    assert_eq!(h.service.default_provider().await.unwrap().unwrap().id, "exa");
    exa.fail(None);
    h.advance(60_000.0);
    assert_eq!(h.qs("peer", "ses_second").await.unwrap().provider_id, "exa");
    assert_eq!(h.id("location").await, "exa");
    assert_eq!(h.qs("sticky replacement", "ses_first").await.unwrap().provider_id, "parallel");
}

#[tokio::test]
async fn shares_cooldowns_without_sending_a_peer_back() {
    let h = Harness::new();
    let exa = h.register("exa");
    h.service.select(Selection::Random).await;
    h.qs("first", "ses_first").await.unwrap();
    h.qs("second", "ses_second").await.unwrap();
    h.register("parallel");
    exa.fail(Some(http_error(429, Some("120"))));
    h.qs("switch", "ses_first").await.unwrap();
    assert_eq!(h.qs("peer", "ses_second").await.unwrap().provider_id, "parallel");
    assert_eq!(exa.calls().len(), 3);
    exa.fail(None);
    h.advance(120_000.0);
    assert_eq!(h.qs("sticky peer", "ses_second").await.unwrap().provider_id, "parallel");
}

#[tokio::test]
async fn keeps_fixed_and_explicit_providers_pinned_with_session_context() {
    let h = Harness::new();
    let exa = h.register("exa");
    let parallel = h.register("parallel");
    exa.fail(Some(limited()));
    h.service.select(Selection::Provider("exa".into())).await;
    assert_eq!(request_provider(&h.qs("fixed", "ses_first").await.unwrap_err()), "exa");
    h.service.select(Selection::Random).await;
    let error = h.service.query("explicit", Some("exa"), Some("ses_first"), &|_| {}).await.unwrap_err();
    assert_eq!(request_provider(&error), "exa");
    assert!(parallel.calls().is_empty());
}

#[tokio::test]
async fn forgets_session_affinity() {
    let h = Harness::new();
    h.register("exa");
    h.service.select(Selection::Random).await;
    h.qs("seed", "ses_first").await.unwrap();
    h.service.forget_session("ses_first");
    h.service.remove_provider("exa");
    h.register("parallel");
    assert_eq!(h.qs("new affinity", "ses_first").await.unwrap().provider_id, "parallel");
    h.register("exa");
    assert_eq!(h.qs("still new affinity", "ses_first").await.unwrap().provider_id, "parallel");
}

#[tokio::test]
async fn fails_when_disabled_and_falls_back_when_default_missing() {
    let h = Harness::new();
    h.register("exa");
    h.service.set_config_selection(Some(Selection::Disabled));
    assert_eq!(h.q("disabled").await.unwrap_err(), WebSearchError::Disabled);
    assert_eq!(h.service.default_provider().await.unwrap_err(), WebSearchError::Disabled);
    h.service.set_config_selection(Some(Selection::Provider("missing".into())));
    assert_eq!(h.q("fallback").await.unwrap_err(), WebSearchError::ProviderRequired);
}

#[tokio::test]
async fn removes_provider_registrations_and_sorts_by_name() {
    let h = Harness::new();
    h.service.add_provider(Arc::new(Fake { name: "tinyFish".into(), ..take(Fake::new("b")) }));
    h.service.add_provider(Arc::new(Fake { name: "Exa".into(), ..take(Fake::new("a")) }));
    h.service.add_provider(Arc::new(Fake { name: "Tavily".into(), ..take(Fake::new("c")) }));
    let names: Vec<_> = h.service.providers().await.into_iter().map(|p| p.name).collect();
    assert_eq!(names, vec!["Exa", "Tavily", "tinyFish"]);
    h.service.remove_provider("a");
    assert_eq!(h.service.providers().await.len(), 2);
}

fn take(fake: Arc<Fake>) -> Fake {
    Arc::try_unwrap(fake).ok().unwrap()
}

#[test]
fn integrations_match_upstream() {
    let list = integrations();
    assert_eq!(list.iter().map(|i| i.id.as_str()).collect::<Vec<_>>(), vec!["exa", "firecrawl", "parallel", "tavily", "tinyfish"]);
    assert_eq!(list[1].name, "Firecrawl");
    assert_eq!(
        list[1].methods,
        vec![IntegrationMethod::Key, IntegrationMethod::Env { names: vec!["FIRECRAWL_API_KEY".into()] }]
    );
    assert_eq!(list[4].name, "TinyFish");
}

#[test]
fn selection_json_roundtrip() {
    assert_eq!(Selection::from_json(&json_str("random")), Some(Selection::Random));
    assert_eq!(Selection::from_json(&json_str("exa")), Some(Selection::Provider("exa".into())));
    assert_eq!(Selection::from_json(&Value::Bool(false)), Some(Selection::Disabled));
    assert_eq!(Selection::from_json(&Value::Bool(true)), None);
    assert_eq!(Selection::from_json(&Value::Null), None);
    assert_eq!(Selection::Disabled.to_json(), Value::Bool(false));
}

#[test]
fn cooldown_millis_parsing() {
    assert_eq!(cooldown_millis(Some("120"), 0.0), 120_000.0);
    assert_eq!(cooldown_millis(Some(" 1.5 "), 0.0), 1_500.0);
    assert_eq!(cooldown_millis(Some("0"), 0.0), 0.0);
    assert_eq!(cooldown_millis(Some("-1"), 0.0), 60_000.0);
    assert_eq!(cooldown_millis(Some(""), 0.0), 60_000.0);
    assert_eq!(cooldown_millis(Some("   "), 0.0), 60_000.0);
    assert_eq!(cooldown_millis(None, 0.0), 60_000.0);
    assert_eq!(cooldown_millis(Some("nope"), 0.0), 60_000.0);
    assert_eq!(cooldown_millis(Some("Thu, 01 Jan 1970 00:02:00 GMT"), 0.0), 120_000.0);
    assert_eq!(cooldown_millis(Some("Thu, 01 Jan 1970 00:02:00 GMT"), 200_000.0), 0.0);
}

#[test]
fn search_result_json_shape() {
    let result = SearchResult { url: "u".into(), title: None, content: Some("c".into()), published: Some(5) };
    assert_eq!(serde_json::to_string(&result).unwrap(), r#"{"url":"u","content":"c","time":{"published":5.0}}"#);
    let plain = SearchResult { url: "u".into(), ..Default::default() };
    assert_eq!(serde_json::to_string(&plain).unwrap(), r#"{"url":"u","time":{}}"#);
    let parsed: SearchResult = serde_json::from_str(r#"{"url":"u","title":"t","time":{"published":1000}}"#).unwrap();
    assert_eq!(parsed.published, Some(1000));
    assert!(serde_json::from_str::<SearchResult>(r#"{"url":"u"}"#).is_err());
}

// ---------- parsing ----------

#[test]
fn exa_parses_blocks() {
    let text = "Title: Effect\nURL: https://effect.website\nPublished: 2026-07-25T00:00:00.000Z\nAuthor: N/A\nHighlights:\nEffect documentation\n\n---\n\nTitle: N/A\nURL: https://b.example\nPublished: N/A\nText: body one\nbody two\n\n---\n\nTitle: no url";
    let results = exa::parse_results(text);
    assert_eq!(
        results,
        vec![
            SearchResult {
                url: "https://effect.website".into(),
                title: Some("Effect".into()),
                content: Some("Effect documentation".into()),
                published: Some(js_date_parse("2026-07-25T00:00:00.000Z").unwrap()),
            },
            SearchResult {
                url: "https://b.example".into(),
                title: None,
                content: Some("body one\nbody two".into()),
                published: None,
            },
        ]
    );
    assert_eq!(results[0].published, Some(1_784_937_600_000));
}

#[test]
fn mcp_parse_response_variants() {
    let decode = |v: &Value| Ok::<_, String>(v.clone());
    assert_eq!(parse_response(r#"{"result":{"a":1}}"#, decode).unwrap(), Some(serde_json::json!({"a":1})));
    let sse = "event: message\ndata: {\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{\"b\":2}}\n\n";
    assert_eq!(parse_response(sse, decode).unwrap(), Some(serde_json::json!({"b":2})));
    assert_eq!(parse_response("event: ping\n\n", decode).unwrap(), None);
    assert_eq!(parse_response("", decode).unwrap(), None);
    assert!(parse_response(r#"{"jsonrpc":"2.0","error":{}}"#, decode).is_err());
    assert!(parse_response("{not json", decode).is_err());
    assert!(parse_response("data: {\"error\":1}", decode).is_err());
}

#[test]
fn firecrawl_and_tinyfish_text_payloads() {
    let fc = r#"{"success":true,"data":{"web":[{"url":"https://a","title":"A","description":"D"},{"url":"https://b","title":null,"description":""}]}}"#;
    assert_eq!(
        firecrawl::parse_search_response(fc).unwrap(),
        vec![
            SearchResult { url: "https://a".into(), title: Some("A".into()), content: Some("D".into()), published: None },
            SearchResult { url: "https://b".into(), ..Default::default() },
        ]
    );
    assert!(firecrawl::parse_search_response(r#"{"data":{"web":[]}}"#).is_none());
    assert!(firecrawl::parse_search_response("plain text").is_none());
    let tf = r#"{"query":"q","results":[{"position":1,"url":"https://e","title":"E","snippet":""}]}"#;
    assert_eq!(
        tinyfish::parse_search_response(tf).unwrap(),
        vec![SearchResult { url: "https://e".into(), title: Some("E".into()), content: None, published: None }]
    );
    assert!(tinyfish::parse_search_response(r#"{"results":[{"url":"x"}]}"#).is_none());
}

#[test]
fn parallel_decode_validates_structured_content() {
    let ok = serde_json::json!({
        "content": [{"type": "text", "text": "search results"}],
        "structuredContent": {
            "search_id": "s", "session_id": "x", "warnings": null,
            "usage": [{"name": "sku", "count": 1}],
            "results": [
                {"url": "https://a", "title": "A", "publish_date": "2025-01-02", "excerpts": ["one", "two"]},
                {"url": "https://b", "publish_date": null, "excerpts": []}
            ]
        }
    });
    assert_eq!(
        parallel::decode(&ok).unwrap(),
        vec![
            SearchResult {
                url: "https://a".into(),
                title: Some("A".into()),
                content: Some("one\n\ntwo".into()),
                published: Some(1_735_776_000_000),
            },
            SearchResult { url: "https://b".into(), ..Default::default() },
        ]
    );
    let missing = serde_json::json!({"content": []});
    assert!(parallel::decode(&missing).is_err());
}

#[test]
fn tavily_decode() {
    let body = r#"{"query":"q","results":[{"url":"https://e","title":"","content":"C","score":0.9}]}"#;
    assert_eq!(
        tavily::decode(body).unwrap(),
        vec![SearchResult { url: "https://e".into(), title: Some("".into()), content: Some("C".into()), published: None }]
    );
    assert!(tavily::decode(r#"{"results":[{"url":"x"}]}"#).is_err());
}

#[test]
fn console_normalizes_server() {
    assert_eq!(normalize_server("https://opencode.ai/console/").unwrap(), "https://opencode.ai/console");
    assert_eq!(normalize_server("http://localhost:3000").unwrap(), "http://localhost:3000");
    assert_eq!(normalize_server("ftp://x").unwrap_err(), "Invalid OpenCode server URL: expected HTTP(S)");
    assert_eq!(normalize_server("nope").unwrap_err(), "Invalid OpenCode server URL: Invalid URL");
}

#[test]
fn date_parse_formats() {
    assert_eq!(js_date_parse("1970-01-01T00:00:01Z"), Some(1000));
    assert_eq!(js_date_parse("1970-01-02"), Some(86_400_000));
    assert_eq!(js_date_parse("Thu, 01 Jan 1970 00:02:00 GMT"), Some(120_000));
    assert_eq!(js_date_parse("garbage"), None);
}

// ---------- providers over HTTP ----------

#[derive(Debug, Clone)]
struct Recorded {
    path: String,
    headers: HashMap<String, String>,
    body: Value,
}

struct MockServer {
    url: String,
    requests: Arc<Mutex<Vec<Recorded>>>,
}

async fn mock(status: u16, body: String, extra_headers: &'static str) -> MockServer {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let requests = Arc::new(Mutex::new(Vec::new()));
    let recorded = requests.clone();
    tokio::spawn(async move {
        loop {
            let Ok((mut socket, _)) = listener.accept().await else { return };
            let recorded = recorded.clone();
            let body = body.clone();
            tokio::spawn(async move {
                let mut buffer = Vec::new();
                let mut chunk = [0u8; 4096];
                let (head, length) = loop {
                    let n = socket.read(&mut chunk).await.unwrap_or(0);
                    if n == 0 {
                        return;
                    }
                    buffer.extend_from_slice(&chunk[..n]);
                    if let Some(pos) = buffer.windows(4).position(|w| w == b"\r\n\r\n") {
                        let head = String::from_utf8_lossy(&buffer[..pos]).to_string();
                        let length = head
                            .lines()
                            .find_map(|l| {
                                let (k, v) = l.split_once(':')?;
                                k.eq_ignore_ascii_case("content-length").then(|| v.trim().parse::<usize>().ok())?
                            })
                            .unwrap_or(0);
                        buffer.drain(..pos + 4);
                        break (head, length);
                    }
                };
                while buffer.len() < length {
                    let n = socket.read(&mut chunk).await.unwrap_or(0);
                    if n == 0 {
                        break;
                    }
                    buffer.extend_from_slice(&chunk[..n]);
                }
                let mut lines = head.lines();
                let path = lines.next().unwrap_or("").split(' ').nth(1).unwrap_or("").to_string();
                let headers = lines
                    .filter_map(|l| l.split_once(':').map(|(k, v)| (k.trim().to_lowercase(), v.trim().to_string())))
                    .collect();
                recorded.lock().push(Recorded {
                    path,
                    headers,
                    body: serde_json::from_slice(&buffer).unwrap_or(Value::Null),
                });
                let response = format!(
                    "HTTP/1.1 {status} X\r\ncontent-length: {}\r\nconnection: close\r\n{extra_headers}\r\n{body}",
                    body.len()
                );
                let _ = socket.write_all(response.as_bytes()).await;
                let _ = socket.shutdown().await;
            });
        }
    });
    MockServer { url, requests }
}

fn ua() -> Arc<RwLock<String>> {
    Arc::new(RwLock::new("opencode/test/1.0/opencode".into()))
}

fn exa_body() -> String {
    let payload = serde_json::json!({
        "jsonrpc": "2.0", "id": 1,
        "result": {"content": [{
            "type": "text",
            "text": "Title: Effect\nURL: https://effect.website\nPublished: 2026-07-25T00:00:00.000Z\nAuthor: N/A\nHighlights:\nEffect documentation",
            "_meta": {"searchTime": 123}
        }]}
    });
    format!("event: message\ndata: {payload}\n\n")
}

#[tokio::test]
async fn exa_sends_key_as_query_parameter() {
    let server = mock(200, exa_body(), "").await;
    let store = Arc::new(FakeStore::default());
    store.keys.lock().insert("exa".into(), "exa secret".into());
    let provider = ExaProvider::new(reqwest::Client::new(), store).with_endpoint(format!("{}/mcp", server.url));
    let results = provider.execute("effect typescript").await.unwrap();
    assert_eq!(
        results,
        vec![SearchResult {
            url: "https://effect.website".into(),
            title: Some("Effect".into()),
            content: Some("Effect documentation".into()),
            published: Some(1_784_937_600_000),
        }]
    );
    let requests = server.requests.lock().clone();
    assert_eq!(requests[0].path, "/mcp?exaApiKey=exa+secret");
    assert_eq!(
        requests[0].body,
        serde_json::json!({"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"web_search_exa","arguments":{"query":"effect typescript","numResults":8}}})
    );
    assert_eq!(requests[0].headers["accept"], "application/json, text/event-stream");
}

#[tokio::test]
async fn parallel_keeps_its_credential_in_the_authorization_header() {
    let body = serde_json::json!({
        "jsonrpc": "2.0", "id": 1,
        "result": {
            "content": [{"type": "text", "text": "search results"}],
            "structuredContent": {
                "search_id": "search_1",
                "results": [{"url": "https://effect.website", "title": "Effect", "publish_date": null, "excerpts": ["Effect documentation"]}],
                "warnings": null,
                "usage": [{"name": "sku_search", "count": 1}],
                "session_id": "ses_parallel"
            }
        }
    })
    .to_string();
    let server = mock(200, body, "").await;
    let store = Arc::new(FakeStore::default());
    store.keys.lock().insert("parallel".into(), "parallel-secret".into());
    let provider = ParallelProvider::new(reqwest::Client::new(), store, ua()).with_endpoint(server.url.clone());
    let results = provider.execute("effect layers").await.unwrap();
    assert_eq!(
        results,
        vec![SearchResult {
            url: "https://effect.website".into(),
            title: Some("Effect".into()),
            content: Some("Effect documentation".into()),
            published: None,
        }]
    );
    let request = server.requests.lock()[0].clone();
    assert_eq!(request.headers["authorization"], "Bearer parallel-secret");
    assert_eq!(request.headers["user-agent"], "opencode/test/1.0/opencode");
    assert_eq!(
        request.body["params"],
        serde_json::json!({"name":"web_search","arguments":{"objective":"effect layers","search_queries":["effect layers"]}})
    );
}

#[tokio::test]
async fn tavily_keyless_and_keyed() {
    let body = r#"{"query":"effect typescript","results":[{"url":"https://effect.website","title":"Effect","content":"Effect documentation","score":0.99}]}"#;
    let server = mock(200, body.into(), "").await;
    let store = Arc::new(FakeStore::default());
    let provider = TavilyProvider::new(reqwest::Client::new(), store.clone(), ua()).with_endpoint(server.url.clone());
    let results = provider.execute("effect typescript").await.unwrap();
    assert_eq!(results[0].title.as_deref(), Some("Effect"));
    {
        let requests = server.requests.lock();
        assert_eq!(requests[0].headers["x-client-name"], "opencode2");
        assert_eq!(requests[0].headers["x-tavily-access-mode"], "keyless");
        assert!(!requests[0].headers.contains_key("authorization"));
        assert_eq!(
            requests[0].body,
            serde_json::json!({"query":"effect typescript","search_depth":"basic","chunks_per_source":3,"max_results":8})
        );
    }
    store.keys.lock().insert("tavily".into(), "tavily-secret".into());
    provider.execute("effect typescript").await.unwrap();
    let requests = server.requests.lock();
    assert_eq!(requests[1].headers["authorization"], "Bearer tavily-secret");
    assert!(!requests[1].headers.contains_key("x-tavily-access-mode"));
}

#[tokio::test]
async fn tinyfish_keyless_and_keyed() {
    let inner = serde_json::json!({"query":"effect typescript","results":[{"position":1,"site_name":"effect.website","snippet":"Effect documentation","title":"Effect","url":"https://effect.website"}],"total_results":1,"page":0}).to_string();
    let body = serde_json::json!({"jsonrpc":"2.0","id":1,"result":{"content":[{"type":"text","text":inner}]}}).to_string();
    let server = mock(200, body, "mcp-session-id: ses_tinyfish\r\n").await;
    let store = Arc::new(FakeStore::default());
    let provider = TinyFishProvider::new(reqwest::Client::new(), store.clone(), ua()).with_endpoint(server.url.clone());
    assert_eq!(
        provider.execute("effect typescript").await.unwrap(),
        vec![SearchResult {
            url: "https://effect.website".into(),
            title: Some("Effect".into()),
            content: Some("Effect documentation".into()),
            published: None,
        }]
    );
    store.keys.lock().insert("tinyfish".into(), "tinyfish-secret".into());
    provider.execute("effect typescript").await.unwrap();
    let requests = server.requests.lock();
    assert_eq!(requests[0].headers["x-tinyfish-access-mode"], "keyless");
    assert!(!requests[0].headers.contains_key("authorization"));
    assert_eq!(requests[0].body["params"]["name"], "search");
    assert!(!requests[0].headers.contains_key("mcp-session-id"));
    // Keyed searches initialize an MCP session first (86755e1ce).
    assert_eq!(requests[1].body["method"], "initialize");
    assert_eq!(requests[1].body["params"]["protocolVersion"], "2025-06-18");
    assert_eq!(requests[1].headers["x-api-key"], "tinyfish-secret");
    assert_eq!(requests[2].body, serde_json::json!({ "jsonrpc": "2.0", "method": "notifications/initialized" }));
    assert_eq!(requests[2].headers["mcp-session-id"], "ses_tinyfish");
    assert_eq!(requests[3].body["method"], "tools/call");
    assert_eq!(requests[3].headers["x-api-key"], "tinyfish-secret");
    assert_eq!(requests[3].headers["mcp-session-id"], "ses_tinyfish");
    assert!(!requests[3].headers.contains_key("x-tinyfish-access-mode"));
}

#[tokio::test]
async fn tinyfish_keyed_search_needs_a_session_id() {
    let server = mock(200, "{}".into(), "").await;
    let store = Arc::new(FakeStore::default());
    store.keys.lock().insert("tinyfish".into(), "tinyfish-secret".into());
    let provider = TinyFishProvider::new(reqwest::Client::new(), store, ua()).with_endpoint(server.url.clone());
    let error = provider.execute("effect typescript").await.unwrap_err();
    assert!(error.to_string().contains("TinyFish MCP initialize did not return an Mcp-Session-Id header"), "{error}");
    assert_eq!(server.requests.lock().len(), 1);
}

#[tokio::test]
async fn firecrawl_over_http() {
    let inner = r#"{"success":true,"data":{"web":[{"url":"https://a","title":"A","description":"D"}]}}"#;
    let body = serde_json::json!({"jsonrpc":"2.0","id":1,"result":{"content":[{"type":"text","text":inner}]}}).to_string();
    let server = mock(200, body, "").await;
    let store = Arc::new(FakeStore::default());
    store.keys.lock().insert("firecrawl".into(), "fc".into());
    let provider = FirecrawlProvider::new(reqwest::Client::new(), store, ua()).with_endpoint(server.url.clone());
    assert_eq!(provider.execute("q").await.unwrap()[0].content.as_deref(), Some("D"));
    let requests = server.requests.lock();
    assert_eq!(requests[0].headers["authorization"], "Bearer fc");
    assert_eq!(requests[0].body["params"]["arguments"], serde_json::json!({"query":"q","limit":8}));
}

#[tokio::test]
async fn every_provider_reports_rate_limits_as_http_status() {
    let server = mock(429, "Rate limited".into(), "retry-after: 7\r\n").await;
    let store: Arc<dyn WebSearchStore> = Arc::new(FakeStore::default());
    let client = reqwest::Client::new();
    let providers: Vec<Arc<dyn SearchProvider>> = vec![
        Arc::new(ExaProvider::new(client.clone(), store.clone()).with_endpoint(server.url.clone())),
        Arc::new(ParallelProvider::new(client.clone(), store.clone(), ua()).with_endpoint(server.url.clone())),
        Arc::new(FirecrawlProvider::new(client.clone(), store.clone(), ua()).with_endpoint(server.url.clone())),
        Arc::new(TavilyProvider::new(client.clone(), store.clone(), ua()).with_endpoint(server.url.clone())),
        Arc::new(TinyFishProvider::new(client.clone(), store.clone(), ua()).with_endpoint(server.url.clone())),
    ];
    for provider in providers {
        let error = provider.execute("limited").await.unwrap_err();
        assert_eq!(error.status, Some(429), "{}", provider.info().id);
        assert_eq!(error.retry_after.as_deref(), Some("7"));
        assert!(error.message.starts_with("StatusCode: non 2xx status code (429 POST "));
    }
}

#[tokio::test]
async fn mcp_body_cap() {
    let big = "x".repeat(MAX_RESPONSE_BYTES + 1);
    let server = mock(200, big, "").await;
    let provider = ExaProvider::new(reqwest::Client::new(), Arc::new(FakeStore::default())).with_endpoint(server.url.clone());
    let error = provider.execute("q").await.unwrap_err();
    assert_eq!(error.message, "web_search_exa response exceeded 262144 bytes");
    assert_eq!(error.status, None);
}

#[tokio::test]
async fn console_provider_request_and_mismatch() {
    let ok = r#"{"providerID":"exa","results":[{"url":"https://a","time":{"published":5}}]}"#;
    let server = mock(200, ok.into(), "").await;
    let server_url = format!("{}/console/", server.url);
    let credential: ConsoleCredentialFn = Arc::new(move || {
        let server_url = server_url.clone();
        Box::pin(async move {
            Ok(ConsoleCredential { token: "tok".into(), org_id: Some("org_1".into()), server: Some(server_url) })
        })
    });
    let provider = ConsoleProvider::new("exa", credential.clone());
    assert_eq!(provider.info().name, "OpenCode Web Search");
    let results = provider.execute("q").await.unwrap();
    assert_eq!(results, vec![SearchResult { url: "https://a".into(), published: Some(5), ..Default::default() }]);
    {
        let requests = server.requests.lock();
        assert_eq!(requests[0].path, "/console/api/websearch");
        assert_eq!(requests[0].headers["authorization"], "Bearer tok");
        assert_eq!(requests[0].headers["x-org-id"], "org_1");
        assert_eq!(requests[0].body, serde_json::json!({"query":"q","providerID":"exa"}));
    }
    let other = ConsoleProvider::new("parallel", credential);
    assert_eq!(
        other.execute("q").await.unwrap_err().message,
        "OpenCode web search returned provider exa instead of parallel"
    );
    let missing: ConsoleCredentialFn =
        Arc::new(|| Box::pin(async { Err("OpenCode Console is not connected".to_string()) }));
    assert_eq!(
        ConsoleProvider::new("exa", missing).execute("q").await.unwrap_err().message,
        "OpenCode Console is not connected"
    );
}

#[tokio::test]
async fn service_registers_builtins_in_plugin_order() {
    let service = WebSearch::new(Arc::new(FakeStore::default()), reqwest::Client::new());
    let ids: Vec<_> = service.data.read().providers.iter().map(|p| p.info().id).collect();
    assert_eq!(ids, vec!["exa", "firecrawl", "parallel", "tavily", "tinyfish"]);
    let names: Vec<_> = service.providers().await.into_iter().map(|p| p.name).collect();
    assert_eq!(names, vec!["Exa", "Firecrawl", "Parallel", "Tavily", "TinyFish"]);
}

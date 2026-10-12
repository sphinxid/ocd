use async_trait::async_trait;
use oc_ai::credentials::form::FormAnswer;
use oc_ai::credentials::integration::{
    AttemptMode, AttemptStatus, AuthorizeResult, CommandAttemptStatus, ConnectionInfo, ConnectionStatus, FlowError,
    FlowResult, IntegrationError, Integrations, Method, OAuthCallback, OAuthConnect, OAuthMethod, OAuthMethodInfo,
};
use oc_ai::credentials::store::{CreateCredential, CredentialStore, MemoryCredentialStore};
use oc_ai::credentials::{Clock, CredentialValue, OAuthValue, map_env};
use parking_lot::Mutex;
use serde_json::json;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use tokio::sync::oneshot;
use tokio_util::sync::CancellationToken;

fn oauth(access: &str, expires: u64) -> OAuthValue {
    OAuthValue { method_id: "m".into(), refresh: "r".into(), access: access.into(), expires, metadata: None }
}

struct Manual(Arc<AtomicU64>);

impl Manual {
    fn new(start: u64) -> (Self, Clock) {
        let time = Arc::new(AtomicU64::new(start));
        let clock_time = time.clone();
        (Self(time), Arc::new(move || clock_time.load(Ordering::SeqCst)))
    }
    fn advance(&self, ms: u64) {
        self.0.fetch_add(ms, Ordering::SeqCst);
    }
}

/// OAuth method whose auto callback waits for the test to deliver a result.
struct AutoMethod {
    pending: Mutex<Option<oneshot::Receiver<FlowResult<OAuthValue>>>>,
    scope: Mutex<Option<CancellationToken>>,
    refreshes: Arc<AtomicU64>,
    refresh_error: bool,
    expires_at: Option<u64>,
}

#[async_trait]
impl OAuthMethod for AutoMethod {
    fn integration_id(&self) -> String {
        "acme".into()
    }
    fn info(&self) -> OAuthMethodInfo {
        OAuthMethodInfo::new("m", "Acme login")
    }
    async fn authorize(&self, _answer: FormAnswer, scope: CancellationToken) -> FlowResult<AuthorizeResult> {
        *self.scope.lock() = Some(scope);
        let rx = self.pending.lock().take().expect("one authorize per test");
        let mut result = AuthorizeResult::auto("https://acme.test/authorize", "Go", async move {
            rx.await.unwrap_or_else(|_| Err(FlowError::new("dropped")))
        });
        result.expires_at = self.expires_at;
        Ok(result)
    }
    fn can_refresh(&self) -> bool {
        true
    }
    async fn refresh(&self, value: OAuthValue) -> FlowResult<OAuthValue> {
        self.refreshes.fetch_add(1, Ordering::SeqCst);
        if self.refresh_error {
            return Err(FlowError::new(""));
        }
        Ok(OAuthValue { access: "refreshed".into(), expires: value.expires + 3_600_000, ..value })
    }
    fn label(&self, value: &OAuthValue) -> Option<String> {
        (value.access == "labelled").then(|| "From method".to_string())
    }
}

struct CodeMethod;

#[async_trait]
impl OAuthMethod for CodeMethod {
    fn integration_id(&self) -> String {
        "acme".into()
    }
    fn info(&self) -> OAuthMethodInfo {
        OAuthMethodInfo::new("code", "Paste code").with_form(vec![json!({"type":"string","key":"region","required":true,"options":[{"label":"EU","value":"eu"}]})])
    }
    async fn authorize(&self, answer: FormAnswer, _scope: CancellationToken) -> FlowResult<AuthorizeResult> {
        let region = answer.get("region").and_then(|v| v.as_str()).unwrap_or("").to_string();
        Ok(AuthorizeResult {
            url: format!("https://acme.test/{region}"),
            instructions: "Paste the code".into(),
            expires_at: None,
            callback: OAuthCallback::Code(Box::new(|code: String| {
                Box::pin(async move {
                    if code == "bad" {
                        Err(FlowError::new("Token exchange failed"))
                    } else {
                        Ok(OAuthValue { method_id: "code".into(), refresh: String::new(), access: code, expires: 0, metadata: None })
                    }
                })
            })),
        })
    }
}

fn auto_method(refresh_error: bool) -> (Arc<AutoMethod>, oneshot::Sender<FlowResult<OAuthValue>>) {
    let (tx, rx) = oneshot::channel();
    (
        Arc::new(AutoMethod {
            pending: Mutex::new(Some(rx)),
            scope: Mutex::new(None),
            refreshes: Arc::new(AtomicU64::new(0)),
            refresh_error,
            expires_at: None,
        }),
        tx,
    )
}

fn service(env: &[(&str, &str)], clock: Clock) -> (Arc<MemoryCredentialStore>, Arc<Integrations>) {
    let store = MemoryCredentialStore::shared();
    let env = map_env(env.iter().map(|(k, v)| (k.to_string(), v.to_string())));
    let integrations = Integrations::with(store.clone(), env, clock);
    integrations.update("acme", |r| r.name = "Acme".into());
    (store, integrations)
}

async fn wait_status(integrations: &Integrations, attempt: &str) -> AttemptStatus {
    for _ in 0..200 {
        let status = integrations.oauth_status("acme", attempt).unwrap();
        if !matches!(status, AttemptStatus::Pending { .. }) {
            return status;
        }
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    }
    panic!("attempt never settled");
}

#[tokio::test]
async fn active_connection_prefers_stored_then_env_order() {
    let (_, clock) = Manual::new(1_000);
    let (store, integrations) = service(&[("ACME_KEY_B", "b"), ("ACME_KEY_A", "a")], clock);
    integrations.register_method("acme", Method::Env { names: vec!["ACME_KEY_A".into(), "ACME_KEY_B".into(), "ACME_KEY_C".into()] });
    let active = integrations.active("acme").await.unwrap().unwrap();
    assert_eq!(active.key(), "env:ACME_KEY_A");
    assert_eq!(integrations.resolve(&active).await.unwrap(), Some(CredentialValue::key("a")));
    let info = integrations.get("acme").await.unwrap().unwrap();
    let keys: Vec<String> = info.connections.iter().map(ConnectionInfo::key).collect();
    assert_eq!(keys, ["env:ACME_KEY_A", "env:ACME_KEY_B"]);

    let first = store.create(CreateCredential::new("acme", CredentialValue::key("k1"))).await.unwrap();
    let second = store.create(CreateCredential::new("acme", CredentialValue::key("k2"))).await.unwrap();
    let info = integrations.get("acme").await.unwrap().unwrap();
    let keys: Vec<String> = info.connections.iter().map(ConnectionInfo::key).collect();
    assert_eq!(
        keys,
        [format!("credential:{}", second.id), format!("credential:{}", first.id), "env:ACME_KEY_A".into(), "env:ACME_KEY_B".into()]
    );
    integrations.activate(&first.id).await.unwrap();
    assert_eq!(integrations.active("acme").await.unwrap().unwrap().key(), format!("credential:{}", first.id));

    // JSON shapes and key order.
    let json = serde_json::to_value(&info).unwrap();
    assert_eq!(
        serde_json::to_string(&json["connections"][0]).unwrap(),
        format!(r#"{{"type":"credential","id":"{}","label":"default","method":"key"}}"#, second.id)
    );
    assert_eq!(serde_json::to_string(&json["methods"][0]).unwrap(), r#"{"type":"env","names":["ACME_KEY_A","ACME_KEY_B","ACME_KEY_C"]}"#);
}

#[tokio::test]
async fn statuses_project_onto_connections() {
    let (_, clock) = Manual::new(0);
    let (_, integrations) = service(&[("ACME_KEY", "x")], clock);
    integrations.register_method("acme", Method::Env { names: vec!["ACME_KEY".into()] });
    let connection = ConnectionInfo::Env { name: "ACME_KEY".into(), status: None };
    let mut events = integrations.subscribe();
    integrations.set_status("acme", &connection, Some(ConnectionStatus::needs_auth("Sign in again", None)));
    assert!(events.try_recv().is_ok());
    integrations.set_status("acme", &connection, Some(ConnectionStatus::needs_auth("Sign in again", None)));
    assert!(events.try_recv().is_err());
    let info = integrations.get("acme").await.unwrap().unwrap();
    assert_eq!(
        serde_json::to_string(&info.connections[0]).unwrap(),
        r#"{"type":"env","name":"ACME_KEY","status":{"status":"needs_auth","message":"Sign in again"}}"#
    );
}

#[tokio::test]
async fn refresh_on_resolve_within_five_minutes() {
    let (clock_handle, clock) = Manual::new(1_000_000);
    let (store, integrations) = service(&[], clock);
    let (method, _tx) = auto_method(false);
    integrations.register_oauth(method.clone());
    let fresh = store
        .create(CreateCredential::new("acme", oauth("old", 1_000_000 + 5 * 60_000 + 1).into_value()))
        .await
        .unwrap();
    let connection = integrations.active("acme").await.unwrap().unwrap();
    let value = integrations.resolve(&connection).await.unwrap().unwrap();
    assert_eq!(value.as_oauth().unwrap().access, "old");
    assert_eq!(method.refreshes.load(Ordering::SeqCst), 0);
    clock_handle.advance(1);
    let value = integrations.resolve(&connection).await.unwrap().unwrap();
    assert_eq!(value.as_oauth().unwrap().access, "refreshed");
    assert_eq!(method.refreshes.load(Ordering::SeqCst), 1);
    assert_eq!(store.get(&fresh.id).await.unwrap().unwrap().value, value);
}

#[tokio::test]
async fn refresh_failure_is_authorization_error() {
    let (_, clock) = Manual::new(10_000_000);
    let (store, integrations) = service(&[], clock);
    let (method, _tx) = auto_method(true);
    integrations.register_oauth(method);
    store.create(CreateCredential::new("acme", oauth("old", 0).into_value())).await.unwrap();
    let connection = integrations.active("acme").await.unwrap().unwrap();
    let error = integrations.resolve(&connection).await.unwrap_err();
    assert_eq!(error, IntegrationError::Authorization("Authorization failed".into()));
    assert_eq!(error.tag(), "Integration.Authorization");
}

#[tokio::test]
async fn key_and_external_methods() {
    let (_, clock) = Manual::new(0);
    let (store, integrations) = service(&[], clock);
    integrations.register_method("acme", Method::Key { label: None, form: None });
    let mut answer = FormAnswer::new();
    answer.insert("x".into(), json!("y"));
    let error = integrations.connect_key("acme", "sk", Some(answer), None).await.unwrap_err();
    assert_eq!(error.to_string(), "Key method does not accept a form answer");
    integrations.connect_key("acme", "sk-1", None, None).await.unwrap();
    integrations.connect_key("acme", "sk-2", None, None).await.unwrap();
    let labels: Vec<String> = store.list("acme").await.unwrap().into_iter().map(|c| c.label).collect();
    assert_eq!(labels, ["Acme", "Acme 2"]);

    integrations.register_method(
        "acme",
        Method::External {
            id: "cli".into(),
            label: "CLI".into(),
            form: Some(vec![json!({"type":"string","key":"resourceName","required":true,"pattern":"^[a-z]+$"})]),
        },
    );
    let error = integrations.connect_external("acme", "nope", None, None).await.unwrap_err();
    assert_eq!(error.to_string(), "External method not found: nope");
    let error = integrations.connect_external("acme", "cli", None, None).await.unwrap_err();
    assert_eq!(error.to_string(), "Missing required form field: resourceName");
    let mut answer = FormAnswer::new();
    answer.insert("resourceName".into(), json!("Bad1"));
    let error = integrations.connect_external("acme", "cli", Some(answer), None).await.unwrap_err();
    assert_eq!(error.to_string(), "Form field does not match pattern: resourceName");
    let mut answer = FormAnswer::new();
    answer.insert("resourceName".into(), json!("models"));
    integrations.connect_external("acme", "cli", Some(answer), Some("Mine".into())).await.unwrap();
    let last = store.list("acme").await.unwrap().pop().unwrap();
    assert_eq!(last.label, "Mine");
    assert_eq!(serde_json::to_string(&last.value).unwrap(), r#"{"type":"external","methodID":"cli","metadata":{"resourceName":"models"}}"#);
}

#[tokio::test]
async fn auto_attempt_lifecycle_success() {
    let (_, clock) = Manual::new(5_000);
    let (store, integrations) = service(&[], clock);
    let (method, tx) = auto_method(false);
    integrations.register_oauth(method.clone());
    let attempt = integrations
        .connect_oauth(OAuthConnect { integration_id: "acme".into(), method_id: "m".into(), ..Default::default() })
        .await
        .unwrap();
    assert!(attempt.attempt_id.starts_with("con_"));
    assert_eq!(attempt.mode, AttemptMode::Auto);
    assert_eq!(attempt.time.created, 5_000);
    assert_eq!(attempt.time.expires, 5_000 + 600_000);
    let json = serde_json::to_value(&attempt).unwrap();
    assert_eq!(json["attemptID"], json!(attempt.attempt_id));
    assert_eq!(json["mode"], json!("auto"));
    assert!(matches!(integrations.oauth_status("acme", &attempt.attempt_id).unwrap(), AttemptStatus::Pending { .. }));
    // Completing an auto attempt is a defect.
    let error = integrations.complete_oauth("acme", &attempt.attempt_id, None).await.unwrap_err();
    assert!(matches!(error, IntegrationError::Defect(_)));
    // Wrong integration → not found.
    assert!(matches!(
        integrations.oauth_status("other", &attempt.attempt_id),
        Err(IntegrationError::AttemptNotFound { .. })
    ));
    tx.send(Ok(oauth("labelled", 0))).unwrap();
    let status = wait_status(&integrations, &attempt.attempt_id).await;
    assert_eq!(serde_json::to_value(&status).unwrap(), json!({"status":"complete","time":{"created":5000,"expires":605000}}));
    let saved = store.list("acme").await.unwrap();
    assert_eq!(saved.len(), 1);
    assert_eq!(saved[0].label, "From method");
    // Scope is closed once settled.
    assert!(method.scope.lock().as_ref().unwrap().is_cancelled());
}

#[tokio::test]
async fn auto_attempt_failure_and_retention() {
    let (clock_handle, clock) = Manual::new(0);
    let (store, integrations) = service(&[], clock);
    let (method, tx) = auto_method(false);
    integrations.register_oauth(method);
    let attempt = integrations
        .connect_oauth(OAuthConnect { integration_id: "acme".into(), method_id: "m".into(), ..Default::default() })
        .await
        .unwrap();
    tx.send(Err(FlowError::new("Device authorization failed: access_denied"))).unwrap();
    let status = wait_status(&integrations, &attempt.attempt_id).await;
    assert_eq!(
        status,
        AttemptStatus::Failed { message: "Device authorization failed: access_denied".into(), time: attempt.time }
    );
    assert!(store.list("acme").await.unwrap().is_empty());
    clock_handle.advance(59_999);
    integrations.scrub();
    assert!(integrations.oauth_status("acme", &attempt.attempt_id).is_ok());
    clock_handle.advance(1);
    integrations.scrub();
    assert!(matches!(
        integrations.oauth_status("acme", &attempt.attempt_id),
        Err(IntegrationError::AttemptNotFound { .. })
    ));
}

#[tokio::test]
async fn attempt_expiry_and_cancel() {
    let (clock_handle, clock) = Manual::new(0);
    let (_, integrations) = service(&[], clock);
    let (method, _tx) = auto_method(false);
    integrations.register_oauth(method.clone());
    let attempt = integrations
        .connect_oauth(OAuthConnect { integration_id: "acme".into(), method_id: "m".into(), ..Default::default() })
        .await
        .unwrap();
    clock_handle.advance(600_000);
    integrations.scrub();
    assert_eq!(integrations.oauth_status("acme", &attempt.attempt_id).unwrap(), AttemptStatus::Expired { time: attempt.time });
    assert!(method.scope.lock().as_ref().unwrap().is_cancelled());

    let (method, _tx) = auto_method(false);
    integrations.register_oauth(method.clone());
    let attempt = integrations
        .connect_oauth(OAuthConnect { integration_id: "acme".into(), method_id: "m".into(), ..Default::default() })
        .await
        .unwrap();
    integrations.cancel_oauth("acme", &attempt.attempt_id);
    assert!(method.scope.lock().as_ref().unwrap().is_cancelled());
    assert!(matches!(
        integrations.oauth_status("acme", &attempt.attempt_id),
        Err(IntegrationError::AttemptNotFound { .. })
    ));
}

#[tokio::test]
async fn provider_expiry_overrides_lifetime() {
    let (_, clock) = Manual::new(100);
    let (_, integrations) = service(&[], clock);
    let (tx, rx) = oneshot::channel::<FlowResult<OAuthValue>>();
    drop(tx);
    integrations.register_oauth(Arc::new(AutoMethod {
        pending: Mutex::new(Some(rx)),
        scope: Mutex::new(None),
        refreshes: Arc::new(AtomicU64::new(0)),
        refresh_error: false,
        expires_at: Some(42_000),
    }));
    let attempt = integrations
        .connect_oauth(OAuthConnect { integration_id: "acme".into(), method_id: "m".into(), ..Default::default() })
        .await
        .unwrap();
    assert_eq!(attempt.time.expires, 42_000);
}

#[tokio::test]
async fn code_attempt_lifecycle() {
    let (_, clock) = Manual::new(0);
    let (store, integrations) = service(&[], clock);
    integrations.register_oauth(Arc::new(CodeMethod));
    // Form validation.
    let error = integrations
        .connect_oauth(OAuthConnect { integration_id: "acme".into(), method_id: "code".into(), ..Default::default() })
        .await
        .unwrap_err();
    assert_eq!(error.to_string(), "Missing required form field: region");
    let mut answer = FormAnswer::new();
    answer.insert("region".into(), json!("us"));
    let error = integrations
        .connect_oauth(OAuthConnect {
            integration_id: "acme".into(),
            method_id: "code".into(),
            answer: Some(answer),
            label: None,
        })
        .await
        .unwrap_err();
    assert_eq!(error.to_string(), "Invalid option for form field: region");

    let mut answer = FormAnswer::new();
    answer.insert("region".into(), json!("eu"));
    let connect = OAuthConnect {
        integration_id: "acme".into(),
        method_id: "code".into(),
        answer: Some(answer),
        label: Some("Work".into()),
    };
    let attempt = integrations.connect_oauth(connect.clone()).await.unwrap();
    assert_eq!(attempt.mode, AttemptMode::Code);
    assert_eq!(attempt.url, "https://acme.test/eu");
    let error = integrations.complete_oauth("acme", &attempt.attempt_id, None).await.unwrap_err();
    assert_eq!(error, IntegrationError::CodeRequired { attempt_id: attempt.attempt_id.clone() });
    integrations.complete_oauth("acme", &attempt.attempt_id, Some("tok".into())).await.unwrap();
    assert!(matches!(integrations.oauth_status("acme", &attempt.attempt_id).unwrap(), AttemptStatus::Complete { .. }));
    // Completing a terminal attempt is a no-op.
    integrations.complete_oauth("acme", &attempt.attempt_id, Some("tok".into())).await.unwrap();
    let saved = store.list("acme").await.unwrap();
    assert_eq!(saved.len(), 1);
    assert_eq!(saved[0].label, "Work");

    let attempt = integrations.connect_oauth(connect).await.unwrap();
    let error = integrations.complete_oauth("acme", &attempt.attempt_id, Some("bad".into())).await.unwrap_err();
    assert_eq!(error, IntegrationError::Authorization("Token exchange failed".into()));
    assert_eq!(
        integrations.oauth_status("acme", &attempt.attempt_id).unwrap(),
        AttemptStatus::Failed { message: "Token exchange failed".into(), time: attempt.time }
    );
    assert!(matches!(
        integrations.complete_oauth("acme", "con_missing", Some("x".into())).await,
        Err(IntegrationError::AttemptNotFound { .. })
    ));
}

async fn wait_command(integrations: &Integrations, attempt: &str) -> CommandAttemptStatus {
    for _ in 0..400 {
        let status = integrations.command_status("acme", attempt).unwrap();
        if !matches!(status, CommandAttemptStatus::Pending { .. }) {
            return status;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    panic!("command never settled");
}

#[tokio::test]
async fn command_method_runner() {
    let (_, clock) = Manual::new(0);
    let (store, integrations) = service(&[], clock);
    let command = |id: &str, script: &str| Method::Command {
        id: id.into(),
        label: "Log in".into(),
        command: vec!["sh".into(), "-c".into(), script.into()],
    };
    integrations.register_method("acme", command("ok", "echo progress >&2; echo '  secret-token  '"));
    integrations.register_method("acme", command("fail", "exit 3"));
    integrations.register_method("acme", command("stderr", "echo 'denied by policy' >&2; exit 1"));
    integrations.register_method("acme", command("empty", "echo '   '"));

    let attempt = integrations.connect_command("acme", "ok", None).await.unwrap();
    assert!(matches!(wait_command(&integrations, &attempt.attempt_id).await, CommandAttemptStatus::Complete { .. }));
    let saved = store.list("acme").await.unwrap();
    assert_eq!(saved[0].value, CredentialValue::key("secret-token"));
    assert_eq!(saved[0].label, "Acme");

    let attempt = integrations.connect_command("acme", "fail", None).await.unwrap();
    assert_eq!(
        wait_command(&integrations, &attempt.attempt_id).await,
        CommandAttemptStatus::Failed { message: "Authentication command exited 3".into(), time: attempt.time }
    );
    let attempt = integrations.connect_command("acme", "stderr", None).await.unwrap();
    assert_eq!(
        wait_command(&integrations, &attempt.attempt_id).await,
        CommandAttemptStatus::Failed { message: "denied by policy".into(), time: attempt.time }
    );
    let attempt = integrations.connect_command("acme", "empty", None).await.unwrap();
    assert_eq!(
        wait_command(&integrations, &attempt.attempt_id).await,
        CommandAttemptStatus::Failed { message: "Authentication command returned no credential".into(), time: attempt.time }
    );
    let error = integrations.connect_command("acme", "missing", None).await.unwrap_err();
    assert_eq!(error.to_string(), "Command method not found: acme/missing");
}

#[test]
fn method_json_key_order() {
    let method = Method::Oauth { id: "device".into(), label: "Login".into(), form: None };
    assert_eq!(serde_json::to_string(&method).unwrap(), r#"{"id":"device","type":"oauth","label":"Login"}"#);
    let method = Method::Command { id: "login".into(), label: "Log in".into(), command: vec!["a".into()] };
    assert_eq!(serde_json::to_string(&method).unwrap(), r#"{"id":"login","type":"command","label":"Log in","command":["a"]}"#);
    let method = Method::Key { label: Some("API key".into()), form: None };
    assert_eq!(serde_json::to_string(&method).unwrap(), r#"{"type":"key","label":"API key"}"#);
    let parsed: Method = serde_json::from_str(r#"{"type":"env","names":["X"]}"#).unwrap();
    assert_eq!(parsed, Method::Env { names: vec!["X".into()] });
    assert_eq!(ConnectionInfo::Credential { id: "cred_1".into(), label: "l".into(), method: "key".into(), status: None }.key(), "credential:cred_1");
}

#[test]
fn form_validation_rules() {
    use oc_ai::credentials::form::{validate_answer, validate_fields};
    let fields = vec![
        json!({"type":"string","key":"deploymentType","required":true,"options":[{"label":"a","value":"github.com"},{"label":"b","value":"enterprise"}]}),
        json!({"type":"string","key":"enterpriseUrl","required":true,"when":[{"key":"deploymentType","op":"eq","value":"enterprise"}]}),
    ];
    assert_eq!(validate_fields(&fields), None);
    let mut answer = FormAnswer::new();
    answer.insert("deploymentType".into(), json!("github.com"));
    assert_eq!(validate_answer(&fields, &answer), None);
    answer.insert("enterpriseUrl".into(), json!("x"));
    assert_eq!(validate_answer(&fields, &answer).as_deref(), Some("Form field is not active: enterpriseUrl"));
    answer.insert("deploymentType".into(), json!("enterprise"));
    assert_eq!(validate_answer(&fields, &answer), None);
    answer.remove("enterpriseUrl");
    assert_eq!(validate_answer(&fields, &answer).as_deref(), Some("Missing required form field: enterpriseUrl"));
    answer.insert("nope".into(), json!(1));
    assert_eq!(validate_answer(&fields, &answer).as_deref(), Some("Unknown form field: nope"));
    let bad = vec![json!({"type":"string","key":"a","when":[{"key":"b","op":"eq","value":"x"}]})];
    assert_eq!(validate_fields(&bad).as_deref(), Some("Form field condition must reference an earlier field: a -> b"));
    assert_eq!(validate_fields(&[]).as_deref(), Some("Form must have at least one field"));
}

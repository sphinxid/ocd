#![allow(dead_code)]

use std::sync::Arc;

use axum::Router;
use axum::body::Body;
use axum::http::{HeaderMap, Request, StatusCode};
use base64::Engine;
use bytes::Bytes;
use futures::StreamExt;
use oc_server::generated::*;
use oc_server::*;
use parking_lot::Mutex;
use serde_json::{Value, json};
use tokio::sync::{broadcast, mpsc};
use tower::ServiceExt;

pub fn agent() -> Value {
    json!({"id":"build","name":"build","request":{"settings":{},"headers":{},"body":{}},"mode":"primary","hidden":false,"permissions":[]})
}

pub fn session(id: &str, updated: i64) -> Value {
    json!({
        "id": id, "projectID": "p1", "cost": 0.5,
        "tokens": {"input":1,"output":2,"reasoning":0,"cache":{"read":0,"write":0}},
        "time": {"created": 1760000000000i64, "updated": updated},
        "location": {"directory": "/tmp"}
    })
}

pub fn message(id: &str) -> Value {
    json!({"id": id, "time": {"created": 1}, "text": "hi", "type": "user"})
}

pub fn pty(id: &str) -> Value {
    json!({"id": id, "title": "t", "command": "/bin/sh", "args": [], "cwd": "/tmp", "status": "running", "pid": 1})
}

pub fn persistent_info(id: &str) -> Value {
    json!({
        "id": id, "title": "t", "command": "/bin/sh", "args": [], "cwd": "/tmp", "status": "running", "pid": 1,
        "sessionID": "ses_1", "foregroundProcess": null, "size": {"cols": 80, "rows": 24},
        "output": {"head": 0, "tail": 0}
    })
}

pub fn from<T: serde::de::DeserializeOwned>(v: Value) -> T {
    serde_json::from_value(v.clone()).unwrap_or_else(|e| panic!("fixture {v}: {e}"))
}

/// A controllable fake engine.
pub struct Mock {
    pub events: broadcast::Sender<Value>,
    /// (operation, ctx.location.directory, debug of request) of the last call.
    pub calls: Mutex<Vec<(String, String, String)>>,
    pub sessions: Mutex<Vec<Value>>,
    pub pty_input: Arc<Mutex<Vec<String>>>,
    pub pty_output: Mutex<Option<mpsc::UnboundedReceiver<String>>>,
    pub persistent_input: Arc<Mutex<Vec<(u8, u16, u16, Vec<u8>)>>>,
}

impl Mock {
    pub fn new() -> Arc<Self> {
        let (events, _) = broadcast::channel(16);
        Arc::new(Self {
            events,
            calls: Mutex::new(Vec::new()),
            sessions: Mutex::new(vec![session("ses_b", 2), session("ses_a", 1)]),
            pty_input: Arc::new(Mutex::new(Vec::new())),
            pty_output: Mutex::new(None),
            persistent_input: Arc::new(Mutex::new(Vec::new())),
        })
    }

    fn record(&self, ctx: &RequestContext, req: impl std::fmt::Debug) {
        self.calls.lock().push((ctx.operation.to_string(), ctx.location.directory.clone(), format!("{req:?}")));
    }

    pub fn last_call(&self) -> (String, String, String) {
        self.calls.lock().last().cloned().expect("a backend call")
    }
}

struct PtyCtl(Arc<Mutex<Vec<String>>>);
impl PtyControl for PtyCtl {
    fn write(&self, data: &str) {
        self.0.lock().push(data.to_string());
    }
}

struct PersistentCtl(Arc<Mutex<Vec<(u8, u16, u16, Vec<u8>)>>>);
#[async_trait::async_trait]
impl PersistentPtyControl for PersistentCtl {
    async fn input(&self, cols: u16, rows: u16, data: Bytes) {
        self.0.lock().push((1, cols, rows, data.to_vec()));
    }
    async fn control(&self, cols: u16, rows: u16) {
        self.0.lock().push((0, cols, rows, Vec::new()));
    }
}

#[async_trait::async_trait]
impl Backend for Mock {
    async fn agent_list(&self, ctx: &RequestContext, req: AgentListRequest) -> Result<Vec<AgentInfo>, ApiError> {
        self.record(ctx, &req);
        Ok(vec![from(agent())])
    }

    async fn agent_get(&self, ctx: &RequestContext, req: AgentGetRequest) -> Result<AgentInfo, ApiError> {
        self.record(ctx, &req);
        if req.agent_id == "build" { Ok(from(agent())) } else { Err(ApiError::agent_not_found(req.agent_id)) }
    }

    async fn location_get(&self, ctx: &RequestContext, req: LocationGetRequest) -> Result<LocationPublicInfo, ApiError> {
        self.record(ctx, &req);
        let d = &ctx.location.directory;
        Ok(from(json!({"directory": d, "project": {"id": "p1", "directory": d, "canonical": d}})))
    }

    async fn session_list(&self, ctx: &RequestContext, req: SessionListRequest) -> Result<Vec<SessionInfo>, ApiError> {
        self.record(ctx, &req);
        if req.query.search.as_deref() == Some("none") {
            return Ok(vec![]);
        }
        Ok(self.sessions.lock().iter().take(req.limit as usize).cloned().map(from).collect())
    }

    async fn session_get(&self, ctx: &RequestContext, req: SessionGetRequest) -> Result<SessionInfo, ApiError> {
        self.record(ctx, &req);
        self.sessions
            .lock()
            .iter()
            .find(|s| s["id"] == req.session_id.as_str())
            .cloned()
            .map(from)
            .ok_or_else(|| ApiError::session_not_found(req.session_id))
    }

    async fn session_remove(&self, ctx: &RequestContext, req: SessionRemoveRequest) -> Result<(), ApiError> {
        self.record(ctx, &req);
        Ok(())
    }

    async fn session_prompt(&self, ctx: &RequestContext, req: SessionPromptRequest) -> Result<SessionInboxUser, ApiError> {
        self.record(ctx, &req);
        if req.session_id == "ses_missing" {
            return Err(ApiError::session_not_found(req.session_id));
        }
        Ok(from(json!({
            "id": "msg_1", "sessionID": req.session_id, "time": {"created": 1},
            "type": "user", "payload": {"text": req.body.text}, "delivery": "steer"
        })))
    }

    async fn session_interrupt(&self, ctx: &RequestContext, req: SessionInterruptRequest) -> Result<SessionInterruptResponse, ApiError> {
        self.record(ctx, &req);
        Ok(SessionInterruptResponse { interrupted: req.resume.unwrap_or(false) })
    }

    async fn session_message_list(
        &self,
        ctx: &RequestContext,
        req: SessionMessageListRequest,
    ) -> Result<Vec<SessionMessageInfo>, ApiError> {
        self.record(ctx, &req);
        Ok(vec![from(message("msg_1")), from(message("msg_2"))])
    }

    async fn session_log(&self, ctx: &RequestContext, req: SessionLogRequest) -> Result<SessionLogStream, ApiError> {
        self.record(ctx, &req);
        if req.session_id == "ses_missing" {
            return Err(ApiError::session_not_found(req.session_id));
        }
        let items = vec![
            Ok(json!({"type": "session.renamed", "durable": {"aggregateID": "ses_1", "seq": 1, "version": 1}, "data": {}})),
            Ok(json!({"type": "log.synced", "aggregateID": "ses_1", "seq": 1})),
            Err(StreamFailure::die("boom")),
            Ok(json!({"never": true})),
        ];
        Ok(futures::stream::iter(items).boxed())
    }

    async fn session_form_list(&self, ctx: &RequestContext, req: SessionFormListRequest) -> Result<Vec<FormInfo>, ApiError> {
        self.record(ctx, &req);
        Ok(vec![])
    }

    async fn project_list(&self, ctx: &RequestContext, req: ProjectListRequest) -> Result<Vec<Project>, ApiError> {
        self.record(ctx, &req);
        Ok(vec![from(json!({"id":"p1","canonical":"/tmp","time":{"created":1,"updated":1,"active":1},"sandboxes":[]}))])
    }

    async fn credential_list(&self, ctx: &RequestContext, req: CredentialListRequest) -> Result<Vec<CredentialEntry>, ApiError> {
        self.record(ctx, &req);
        Ok(vec![from(json!({"id":"cred_1","integrationID":"openai","label":"x","active":true,"value":{"type":"key","key":"sk"}}))])
    }

    async fn rpc_call(&self, ctx: &RequestContext, req: RpcCallRequest) -> Result<RpcOutput, ApiError> {
        self.record(ctx, &req);
        match req.method.as_str() {
            "die" => Err(ApiError::defect("kaboom")),
            "bad" => Err(ApiError::rpc("rpc.bad_input", "bad input", Some(json!({"x": 1})))),
            "none" => Ok(RpcOutput { output: None }),
            _ => Ok(RpcOutput { output: req.body.input }),
        }
    }

    async fn event_subscribe(&self, ctx: &RequestContext, req: EventSubscribeRequest) -> Result<EventSubscription, ApiError> {
        self.record(ctx, &req);
        Ok(self.events.subscribe().into())
    }

    async fn fs_read(&self, ctx: &RequestContext, req: FsReadRequest) -> Result<FsFile, ApiError> {
        self.record(ctx, &req);
        if req.path != "hello.txt" {
            return Err(ApiError::file_not_found(req.path));
        }
        Ok(FsFile {
            size: 10,
            mtime_ms: Some(1_760_000_000_000),
            mime: "text/plain".into(),
            source: FsSource::Bytes(Bytes::from_static(b"0123456789")),
        })
    }

    async fn experimental_fs_write(
        &self,
        ctx: &RequestContext,
        req: ExperimentalFsWriteRequest,
    ) -> Result<FileSystemWrite, ApiError> {
        self.record(ctx, &req);
        Ok(from(json!({"path": format!("{}/{}", ctx.location.directory, req.path)})))
    }

    async fn vcs_init(&self, ctx: &RequestContext, req: VcsInitRequest) -> Result<(), ApiError> {
        self.record(ctx, &req);
        Ok(())
    }

    async fn pty_get(&self, ctx: &RequestContext, req: PtyGetRequest) -> Result<Pty, ApiError> {
        self.record(ctx, &req);
        if req.pty_id == "pty_1" { Ok(from(pty("pty_1"))) } else { Err(ApiError::pty_not_found(req.pty_id)) }
    }

    async fn pty_connect(&self, ctx: &RequestContext, req: PtyAttachRequest) -> Result<PtyAttachment, ApiError> {
        self.record(ctx, &req);
        let rx = self.pty_output.lock().take().ok_or_else(ApiError::pty_exited)?;
        let output = futures::stream::unfold(rx, |mut rx| async move { rx.recv().await.map(|s| (s, rx)) }).boxed();
        Ok(PtyAttachment { replay: "hello".into(), cursor: 5, output, control: Arc::new(PtyCtl(self.pty_input.clone())) })
    }

    async fn server_experimental_persistent_pty_get(
        &self,
        ctx: &RequestContext,
        req: ServerExperimentalPersistentPtyGetRequest,
    ) -> Result<PersistentPtyInfo, ApiError> {
        self.record(ctx, &req);
        if req.pty_id == "pty_p" { Ok(from(persistent_info("pty_p"))) } else { Err(ApiError::pty_not_found(req.pty_id)) }
    }

    async fn persistent_pty_connect(
        &self,
        ctx: &RequestContext,
        req: PersistentPtyAttachRequest,
    ) -> Result<PersistentPtyAttachment, ApiError> {
        self.record(ctx, &req);
        if req.pty_id != "pty_p" {
            return Err(ApiError::pty_not_found(req.pty_id));
        }
        let events = futures::stream::iter(vec![
            PersistentPtyEvent::Output(Bytes::from_static(b"live")),
            PersistentPtyEvent::Resized { cols: 100, rows: 30, generation: 2, checkpoint: Bytes::from_static(b"cp") },
            PersistentPtyEvent::Json(json!({"type": "title_changed", "title": "x"})),
        ])
        .chain(futures::stream::pending())
        .boxed();
        Ok(PersistentPtyAttachment {
            info: from(persistent_info("pty_p")),
            role: req.role,
            generation: 1,
            replay: PersistentPtyReplay {
                requested_offset: req.cursor,
                available_offset: 0,
                end_offset: 4,
                truncated: false,
                data: Bytes::from_static(b"past"),
            },
            events,
            control: Arc::new(PersistentCtl(self.persistent_input.clone())),
        })
    }
}

pub const PASSWORD: &str = "secret";

pub fn options(password: Option<&str>, cwd: &str) -> ServerOptions {
    ServerOptions {
        password: password.map(str::to_string),
        cwd: Some(cwd.to_string()),
        version: "1.2.3".into(),
        tmp_dir: "/tmp/oc".into(),
        cors: vec!["https://extra.example".into()],
        ready: true,
        ..Default::default()
    }
}

pub fn basic(password: &str) -> String {
    format!("Basic {}", base64::engine::general_purpose::STANDARD.encode(format!("opencode:{password}")))
}

pub struct Harness {
    pub router: Router,
    pub handle: RouterHandle,
    pub mock: Arc<Mock>,
    pub dir: tempfile::TempDir,
}

pub fn harness(password: Option<&str>) -> Harness {
    let dir = tempfile::tempdir().unwrap();
    let mock = Mock::new();
    let (router, handle) = oc_server::router(mock.clone(), &options(password, dir.path().to_str().unwrap())).unwrap();
    Harness { router, handle, mock, dir }
}

pub struct Res {
    pub status: StatusCode,
    pub headers: HeaderMap,
    pub body: Bytes,
}

impl Res {
    pub fn json(&self) -> Value {
        serde_json::from_slice(&self.body).unwrap_or_else(|e| panic!("json body ({e}): {:?}", String::from_utf8_lossy(&self.body)))
    }
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers.get(name).and_then(|v| v.to_str().ok())
    }
}

impl Harness {
    pub fn dir(&self) -> String {
        self.dir.path().to_str().unwrap().to_string()
    }

    pub async fn send(&self, req: Request<Body>) -> Res {
        let res = self.router.clone().oneshot(req).await.unwrap();
        let status = res.status();
        let headers = res.headers().clone();
        let body = axum::body::to_bytes(res.into_body(), usize::MAX).await.unwrap();
        Res { status, headers, body }
    }

    /// Authenticated request (Basic) with an optional JSON body.
    pub async fn req(&self, method: &str, uri: &str, body: Option<Value>) -> Res {
        let mut b = Request::builder().method(method).uri(uri).header("authorization", basic(PASSWORD));
        let body = match body {
            Some(v) => {
                b = b.header("content-type", "application/json");
                Body::from(serde_json::to_vec(&v).unwrap())
            }
            None => Body::empty(),
        };
        self.send(b.body(body).unwrap()).await
    }

    pub async fn get(&self, uri: &str) -> Res {
        self.req("GET", uri, None).await
    }
}

pub fn enc(s: &str) -> String {
    percent_encoding::utf8_percent_encode(s, percent_encoding::NON_ALPHANUMERIC).to_string()
}

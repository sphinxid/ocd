//! Transport behavior against an in-process fake daemon speaking protocol 7 over a Unix socket.

#![cfg(unix)]

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use oc_persistent_pty::frame::{FrameReader, encode, encode_payload, output_frame_payload};
use oc_persistent_pty::{AttachInput, BinaryFn, Options, PersistentPty, Role, StreamEvent};
use parking_lot::Mutex;
use serde_json::{Value, json};
use tokio::io::AsyncWriteExt;
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::Notify;

#[derive(Default)]
struct Fake {
    token: Mutex<String>,
    protocol: Mutex<u64>,
    pong_pid: Mutex<Option<u64>>,
    /// Fail the next N non-ping requests with "authentication failed" (and rotate the token).
    auth_failures: AtomicUsize,
    requests: Mutex<Vec<Value>>,
    owner_close: Notify,
}

const PID: u64 = 4242;

fn terminal(id: u64, group: &str) -> Value {
    json!({
        "id": id, "pid": 99, "title": "Terminal", "foreground_process": null, "group_id": group,
        "command": ["/bin/sh"], "cwd": "/", "cols": 80, "rows": 24,
        "lifecycle": {"status": "running"}, "output_head": 0, "output_tail": 5
    })
}

fn write_registration(directory: &Path, fake: &Fake, socket: &Path) {
    std::fs::create_dir_all(directory).unwrap();
    let registration = json!({
        "instance_id": "inst-1", "pid": PID, "protocol": *fake.protocol.lock(),
        "socket": socket.to_string_lossy(), "token": *fake.token.lock(),
    });
    std::fs::write(directory.join("service.json"), registration.to_string()).unwrap();
}

async fn start_fake(directory: PathBuf, fake: Arc<Fake>) -> PathBuf {
    let socket = std::env::temp_dir().join(format!("ocpty-fake-{}.sock", uuid::Uuid::new_v4()));
    let listener = UnixListener::bind(&socket).unwrap();
    write_registration(&directory, &fake, &socket);
    let socket_path = socket.clone();
    tokio::spawn(async move {
        loop {
            let Ok((stream, _)) = listener.accept().await else { return };
            let fake = fake.clone();
            let directory = directory.clone();
            let socket_path = socket_path.clone();
            tokio::spawn(async move { handle(stream, fake, directory, socket_path).await });
        }
    });
    socket
}

async fn send(stream: &mut UnixStream, value: Value) {
    stream.write_all(&encode(&value).unwrap()).await.unwrap();
}

async fn handle(mut stream: UnixStream, fake: Arc<Fake>, directory: PathBuf, socket: PathBuf) {
    let (read, mut write) = stream.split();
    let mut reader = FrameReader::new(read);
    let Ok(Some(frame)) = reader.next().await else { return };
    let envelope: Value = serde_json::from_slice(&frame).unwrap();
    let request = envelope["request"].clone();
    let op = request["op"].as_str().unwrap_or_default().to_string();
    if op != "ping" && fake.auth_failures.load(Ordering::SeqCst) > 0 {
        fake.auth_failures.fetch_sub(1, Ordering::SeqCst);
        *fake.token.lock() = "rotated".into();
        write_registration(&directory, &fake, &socket);
        write.write_all(&encode(&json!({"type": "error", "message": "authentication failed"})).unwrap()).await.unwrap();
        return;
    }
    if envelope["token"] != Value::String(fake.token.lock().clone()) {
        write.write_all(&encode(&json!({"type": "error", "message": "authentication failed"})).unwrap()).await.unwrap();
        return;
    }
    fake.requests.lock().push(request.clone());
    match op.as_str() {
        "ping" => {
            let pid = fake.pong_pid.lock().unwrap_or(PID);
            send(&mut stream, json!({"type": "pong", "instance_id": "inst-1", "pid": pid, "protocol": 7})).await;
        }
        "own" => {
            send(&mut stream, json!({"type": "owned"})).await;
            let (read, mut write) = stream.split();
            let mut reader = FrameReader::new(read);
            loop {
                tokio::select! {
                    _ = fake.owner_close.notified() => return,
                    frame = reader.next() => {
                        let Ok(Some(frame)) = frame else { return };
                        let envelope: Value = serde_json::from_slice(&frame).unwrap();
                        fake.requests.lock().push(envelope["request"].clone());
                        let response = json!({"type": "handoff", "ticket": "tick", "expires_at": 1_900_000_000_000u64});
                        write.write_all(&encode(&response).unwrap()).await.unwrap();
                    }
                }
            }
        }
        "list" => send(&mut stream, json!({"type": "terminals", "terminals": [terminal(1, "ses_a"), terminal(2, "ses_b")]})).await,
        "subscribe" => {
            let attached = json!({
                "type": "attached", "terminal": terminal(1, "ses_a"), "role": request["role"], "generation": 1,
                "requested_offset": request["offset"], "available_offset": 0, "end_offset": 5, "truncated": false,
                "replay_base64": "aGVsbG8="
            });
            send(&mut stream, attached).await;
            stream.write_all(&encode_payload(&output_frame_payload(5, 8, b"abc")).unwrap()).await.unwrap();
            send(&mut stream, json!({"type": "title_changed", "title": "vim"})).await;
            send(&mut stream, json!({"type": "ok"})).await; // ignored by the pump
            send(&mut stream, json!({"type": "exited", "exit_code": 0, "final_offset": 8})).await;
        }
        "terminate" | "write" | "resize" | "control" | "input" => send(&mut stream, json!({"type": "ok"})).await,
        _ => send(&mut stream, json!({"type": "error", "message": format!("unsupported {op}")})).await,
    }
}

fn no_spawn() -> BinaryFn {
    Arc::new(|| Box::pin(async { Err("spawning disabled in tests".to_string()) }))
}

async fn service(root: &Path, id: &str) -> PersistentPty {
    let mut g = oc_util::Global::resolve();
    g.state = root.join("state");
    g.bin = root.join("bin");
    PersistentPty::new(
        &g,
        Options { root: Some(root.to_path_buf()), id: Some(id.into()), binary: Some(no_spawn()), ..Default::default() },
    )
    .await
}

fn temp_root() -> PathBuf {
    let root = std::env::temp_dir().join(format!("ocpty-fake-root-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&root).unwrap();
    root
}

#[tokio::test]
async fn list_without_daemon_is_empty_and_create_reports_spawn_failure() {
    let root = temp_root();
    let pty = service(&root, "absent").await;
    assert!(pty.list(None).await.unwrap().is_empty());
    let err = pty.create("ses", Default::default()).await.unwrap_err();
    assert_eq!(err, oc_persistent_pty::Error::Unavailable("spawning disabled in tests".into()));
    // The failed spawn moved to a fresh runtime directory.
    assert_ne!(pty.daemon().id(), "absent");
}

#[tokio::test]
async fn protocol_and_registration_mismatches() {
    let root = temp_root();
    let fake = Arc::new(Fake::default());
    *fake.token.lock() = "tok".into();
    *fake.protocol.lock() = 6;
    start_fake(root.join("d1"), fake.clone()).await;
    let pty = service(&root, "d1").await;
    let err = pty.daemon().discover(None).await.unwrap_err();
    assert_eq!(err.message, "opencode-pty protocol mismatch: daemon=6, client=7");
    assert_eq!(err.pid, Some(PID));
    // Protocol errors are not "connect" errors: list surfaces them.
    assert_eq!(pty.list(None).await.unwrap_err().to_string(), "opencode-pty protocol mismatch: daemon=6, client=7");

    let fake = Arc::new(Fake::default());
    *fake.token.lock() = "tok".into();
    *fake.protocol.lock() = 7;
    *fake.pong_pid.lock() = Some(1);
    start_fake(root.join("d2"), fake).await;
    let pty = service(&root, "d2").await;
    assert_eq!(pty.daemon().discover(None).await.unwrap_err().message, "opencode-pty registration mismatch");
}

#[tokio::test]
async fn owns_lists_attaches_and_hands_off() {
    let root = temp_root();
    let fake = Arc::new(Fake::default());
    *fake.token.lock() = "tok".into();
    *fake.protocol.lock() = 7;
    start_fake(root.join("d"), fake.clone()).await;
    let pty = service(&root, "d").await;

    let all = pty.list(None).await.unwrap();
    assert_eq!(all.iter().map(|t| t.id.as_str()).collect::<Vec<_>>(), vec!["pty_persistent_1", "pty_persistent_2"]);
    assert_eq!(pty.list(Some("ses_b")).await.unwrap().len(), 1);
    assert!(pty.daemon().is_owner());
    {
        let requests = fake.requests.lock();
        assert_eq!(requests[0], json!({"op": "ping"}));
        assert_eq!(requests[1], json!({"op": "own", "instance_id": "inst-1"}));
        assert_eq!(requests[2], json!({"op": "list"}));
    }

    let mut attachment = pty
        .attach("pty_persistent_1", AttachInput { cursor: 3, attachment_id: "a1".into(), role: Role::Controller, takeover: true })
        .await
        .unwrap();
    assert_eq!(attachment.replay.data, b"hello");
    assert_eq!(attachment.replay.requested_offset, 3);
    assert_eq!(attachment.replay.end_offset, 5);
    let subscribe = fake.requests.lock().iter().find(|r| r["op"] == "subscribe").cloned().unwrap();
    assert_eq!(
        subscribe,
        json!({"op": "subscribe", "id": 1, "offset": 3, "attachment_id": "a1", "role": "controller", "takeover": true})
    );
    let mut stream = attachment.activate().unwrap();
    let mut events = Vec::new();
    while let Some(event) = stream.recv().await {
        events.push(event);
    }
    assert_eq!(
        events,
        vec![
            StreamEvent::Output { start: 5, end: 8, data: b"abc".to_vec() },
            StreamEvent::TitleChanged { title: "vim".into() },
            StreamEvent::Exited { exit_code: Some(0), final_offset: 8 },
        ]
    );
    // The observed exit triggers a background terminate.
    for _ in 0..100 {
        if fake.requests.lock().iter().any(|r| r["op"] == "terminate") {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    assert!(fake.requests.lock().contains(&json!({"op": "terminate", "id": 1})));

    // Controller attach selected the terminal; resize with attachment null.
    pty.resize("pty_persistent_2", 120, 40, None).await.unwrap();
    assert!(fake.requests.lock().contains(&json!({"op": "resize", "id": 2, "attachment_id": null, "cols": 120, "rows": 40})));

    let handoff = pty.handoff().await.unwrap().unwrap();
    assert_eq!(handoff.directory, root.join("d").to_string_lossy());
    assert_eq!((handoff.instance_id.as_str(), handoff.ticket.as_str()), ("inst-1", "tick"));
    assert_eq!(handoff.expires_at, 1_900_000_000_000);
    assert!(fake.requests.lock().contains(&json!({"op": "prepare_handoff"})));

    // Losing the owner connection drops the registration; the next request re-discovers and re-claims.
    fake.owner_close.notify_waiters();
    for _ in 0..100 {
        if !pty.daemon().is_owner() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    assert!(!pty.daemon().is_owner());
    assert_eq!(pty.list(None).await.unwrap().len(), 2);
    assert!(pty.daemon().is_owner());
    let owns = fake.requests.lock().iter().filter(|r| r["op"] == "own").count();
    assert_eq!(owns, 2);
}

#[tokio::test]
async fn authentication_failure_rediscovers_once() {
    let root = temp_root();
    let fake = Arc::new(Fake::default());
    *fake.token.lock() = "tok".into();
    *fake.protocol.lock() = 7;
    start_fake(root.join("d"), fake.clone()).await;
    let pty = service(&root, "d").await;
    assert_eq!(pty.list(None).await.unwrap().len(), 2);
    // Next list: auth fails once (token rotated on disk), retried with the new registration.
    fake.auth_failures.store(1, Ordering::SeqCst);
    assert_eq!(pty.list(None).await.unwrap().len(), 2);
    // Two consecutive failures surface the error.
    fake.auth_failures.store(2, Ordering::SeqCst);
    assert_eq!(pty.list(None).await.unwrap_err().to_string(), "authentication failed");
}

#[tokio::test]
async fn expired_or_mismatched_handoff_is_discarded() {
    let root = temp_root();
    let fake = Arc::new(Fake::default());
    *fake.token.lock() = "tok".into();
    *fake.protocol.lock() = 7;
    start_fake(root.join("old"), fake.clone()).await;
    let mut g = oc_util::Global::resolve();
    g.state = root.join("state");
    let handoff = oc_persistent_pty::Handoff {
        directory: root.join("old").to_string_lossy().into_owned(),
        instance_id: "other-instance".into(),
        ticket: "t".into(),
        expires_at: 4_000_000_000_000,
    };
    let pty = PersistentPty::new(
        &g,
        Options { root: Some(root.clone()), handoff: Some(handoff), binary: Some(no_spawn()), ..Default::default() },
    )
    .await;
    assert!(!pty.daemon().is_owner());
    assert_ne!(pty.daemon().id(), "old");
    // The mismatched daemon was asked to shut down.
    assert!(fake.requests.lock().contains(&json!({"op": "shutdown"})));
}

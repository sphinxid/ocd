//! Location sync retries and the missing-folder screen (`context/location.tsx`,
//! `routes/session/location-missing.tsx`).

use super::tests::{app, render, session};
use super::*;
use crate::keymap::Stroke;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

/// Answers `GET /api/location` with `(status, body)` in order (the last one repeats).
async fn location_server(replies: Vec<(u16, &'static str)>) -> (String, Arc<AtomicUsize>) {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let hits = Arc::new(AtomicUsize::new(0));
    let counter = hits.clone();
    tokio::spawn(async move {
        loop {
            let Ok((mut sock, _)) = listener.accept().await else { return };
            let mut buf = vec![0u8; 4096];
            let n = sock.read(&mut buf).await.unwrap_or(0);
            let req = String::from_utf8_lossy(&buf[..n]).to_string();
            assert!(req.starts_with("GET /api/location?"), "{req}");
            let i = counter.fetch_add(1, Ordering::SeqCst);
            let (status, body) = replies[i.min(replies.len() - 1)];
            let resp = format!("HTTP/1.1 {status} X\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}", body.len());
            let _ = sock.write_all(resp.as_bytes()).await;
        }
    });
    (url, hits)
}

async fn drive(app: &mut App, rx: &mut tokio::sync::mpsc::UnboundedReceiver<AppMsg>, ms: u64) {
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_millis(ms);
    while let Ok(Some(msg)) = tokio::time::timeout_at(deadline, rx.recv()).await {
        if let AppMsg::Apply(f) = msg {
            f(app);
        }
    }
}

const NOT_FOUND: &str = r#"{"_tag":"LocationNotFoundError","location":{"directory":"/work/proj"},"message":"Location not found"}"#;
const BUSY: &str = r#"{"_tag":"ServiceUnavailableError","message":"Plugins are starting"}"#;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn other_errors_are_retried_then_not_found_marks_the_folder_missing() {
    let (url, hits) = location_server(vec![(503, BUSY), (404, NOT_FOUND)]).await;
    let (tx, mut rx) = unbounded_channel();
    let mut a = app();
    a.tx = tx;
    a.client = crate::api::Client::new(&url, None, "/work/proj");
    let sid = session(&mut a);
    let started = std::time::Instant::now();
    a.sync_location_info("/work/proj".into());
    drive(&mut a, &mut rx, 2500).await;
    // One retry after 500 ms; the typed not-found error is not retried.
    assert_eq!(hits.load(Ordering::SeqCst), 2);
    assert!(started.elapsed() >= std::time::Duration::from_millis(500));
    assert_eq!(a.location.missing.as_deref(), Some("/work/proj"));
    assert_eq!(a.session_id(), Some(sid.as_str()));
    assert_eq!(a.location_missing().as_deref(), Some("/work/proj"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn three_failures_never_count_as_missing() {
    let (url, hits) = location_server(vec![(503, BUSY)]).await;
    let (tx, mut rx) = unbounded_channel();
    let mut a = app();
    a.tx = tx;
    a.client = crate::api::Client::new(&url, None, "/work/proj");
    let _sid = session(&mut a);
    a.sync_location_info("/work/proj".into());
    drive(&mut a, &mut rx, 3000).await;
    // 3 tries, 500 ms then 1 s apart.
    assert_eq!(hits.load(Ordering::SeqCst), 3);
    assert_eq!(a.location.missing, None);
}

#[test]
fn missing_folder_replaces_the_prompt() {
    let mut a = app();
    let sid = session(&mut a);
    if let Some(s) = a.store.sessions.get_mut(&sid) {
        s.project_id = "proj_1".into();
    }
    a.location.missing = Some("/work/proj".into());
    let lines = render(&mut a, 100, 30);
    let joined = lines.join("\n");
    let y = lines.iter().position(|l| l.contains("Session location unavailable")).unwrap_or_else(|| panic!("{joined}"));
    // The bar at column 2, the header and body one cell further in than the bar's padding.
    assert!(lines[y].starts_with("  ┃  △ Session location unavailable"), "{joined}");
    assert!(lines[y + 1].trim_end().ends_with('┃'));
    assert!(lines[y + 2].starts_with("  ┃  /work/proj"), "{joined}");
    assert!(lines[y + 4].starts_with("  ┃  Choose another directory to continue this session."), "{joined}");
    let band = lines.iter().find(|l| l.contains("Choose directory")).unwrap();
    assert!(band.starts_with("  ┃   Choose directory "), "{band}");
    assert!(band.trim_end().ends_with("enter confirm"), "{band}");
    assert!(!a.prompt_active());
    // Typing does nothing; Enter opens the Worktrees dialog to move the session.
    a.on_key(Stroke::key("x"));
    assert!(a.editor.is_empty());
    a.on_key(Stroke::key("return"));
    assert_eq!(a.dialogs.last().map(|d| d.purpose.as_str()), Some("worktrees"));
    assert_eq!(a.dialogs.last().map(|d| d.data["id"].clone()), Some(serde_json::json!("proj_1")));
    // Another location (a moved session) is not missing.
    a.dialogs.clear();
    a.location.missing = Some("/elsewhere".into());
    assert_eq!(a.location_missing(), None);
    assert!(a.prompt_active());
}

//! Stored media references in the transcript (#60116, `experimental.media_blob`; upstream's
//! `test/session-media-refs.test.tsx`): the transcript store reads with the `experimental-media-uri` capability, and
//! a rewind (undo, queued undo, revert) or fork reads a prompt's `media:` images back before it changes anything.

use super::tests::{app, session};
use super::*;
use serde_json::{Value, json};
use std::sync::{Arc, Mutex};

const HASH: &str = "eeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee";
const PNG: &[u8] = b"\x89PNG\r\n\x1a\nfake";

/// What the mock answers: status, content type, body.
type Reply = (u16, &'static str, Vec<u8>);
type Handler = Arc<dyn Fn(&str, &str) -> Reply + Send + Sync>;

/// Requests as `METHOD path status`, and the capability header of each.
#[derive(Clone, Default)]
struct Log(Arc<Mutex<Vec<(String, Option<String>)>>>);

impl Log {
    fn calls(&self) -> Vec<String> {
        self.0.lock().unwrap().iter().map(|(c, _)| c.clone()).collect()
    }
    fn capability(&self, prefix: &str) -> Option<Option<String>> {
        self.0.lock().unwrap().iter().find(|(c, _)| c.starts_with(prefix)).map(|(_, h)| h.clone())
    }
}

async fn serve(route: Handler, log: Log) -> String {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move {
        loop {
            let Ok((mut sock, _)) = listener.accept().await else { return };
            let (route, log) = (route.clone(), log.clone());
            tokio::spawn(async move {
                let mut buf = Vec::new();
                let mut chunk = vec![0u8; 8192];
                let head = loop {
                    let n = sock.read(&mut chunk).await.unwrap_or(0);
                    if n == 0 {
                        return;
                    }
                    buf.extend_from_slice(&chunk[..n]);
                    if let Some(end) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                        let head = String::from_utf8_lossy(&buf[..end]).to_string();
                        let len = head
                            .lines()
                            .find_map(|l| l.to_ascii_lowercase().strip_prefix("content-length:").map(|v| v.trim().parse::<usize>().unwrap_or(0)))
                            .unwrap_or(0);
                        while buf.len() < end + 4 + len {
                            let n = sock.read(&mut chunk).await.unwrap_or(0);
                            if n == 0 {
                                break;
                            }
                            buf.extend_from_slice(&chunk[..n]);
                        }
                        break head;
                    }
                };
                let line = head.lines().next().unwrap_or("").to_string();
                let mut it = line.split(' ');
                let method = it.next().unwrap_or("").to_string();
                let path = it.next().unwrap_or("").split('?').next().unwrap_or("").to_string();
                let capability = head
                    .lines()
                    .find_map(|l| l.split_once(':').filter(|(k, _)| k.eq_ignore_ascii_case("x-opencode-capabilities")).map(|(_, v)| v.trim().to_string()));
                let (status, ctype, body) = route(&method, &path);
                log.0.lock().unwrap().push((format!("{method} {path} {status}"), capability));
                let mut resp =
                    format!("HTTP/1.1 {status} X\r\ncontent-type: {ctype}\r\ncontent-length: {}\r\nconnection: close\r\n\r\n", body.len()).into_bytes();
                resp.extend_from_slice(&body);
                let _ = sock.write_all(&resp).await;
            });
        }
    });
    url
}

async fn flow(route: Handler) -> (App, tokio::sync::mpsc::UnboundedReceiver<AppMsg>, Log) {
    let log = Log::default();
    let url = serve(route, log.clone()).await;
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
    let mut a = app();
    a.tx = tx;
    a.client = crate::api::Client::new(&url, None, "/work/proj");
    (a, rx, log)
}

async fn drive_until(a: &mut App, rx: &mut tokio::sync::mpsc::UnboundedReceiver<AppMsg>, ms: u64, done: impl Fn(&App) -> bool) {
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_millis(ms);
    while !done(a) {
        match tokio::time::timeout_at(deadline, rx.recv()).await {
            Ok(Some(AppMsg::Apply(f))) => f(a),
            Ok(Some(_)) => {}
            _ => break,
        }
    }
}

/// Lets anything that should not follow (a rewind after a failed read) show up.
async fn settle(a: &mut App, rx: &mut tokio::sync::mpsc::UnboundedReceiver<AppMsg>) {
    drive_until(a, rx, 300, |_| false).await;
}

fn json_reply(status: u16, v: Value) -> Reply {
    (status, "application/json", v.to_string().into_bytes())
}

/// `media`: how `GET /api/experimental/media/<hash>` answers.
fn routes(media: fn() -> Reply) -> Handler {
    Arc::new(move |method: &str, path: &str| match (method, path) {
        ("GET", p) if p.starts_with("/api/experimental/media/") => media(),
        ("POST", p) if p.ends_with("/interrupt") => json_reply(200, json!({ "data": true })),
        ("POST", p) if p.ends_with("/revert/stage") => json_reply(200, json!({ "data": { "messageID": "msg_shot" } })),
        ("POST", p) if p.ends_with("/fork") => json_reply(200, json!({ "data": { "id": "ses_forked", "title": "Forked" } })),
        ("DELETE", _) => (204, "application/json", vec![]),
        ("GET", p) if p.ends_with("/message") => json_reply(200, json!({ "data": [], "cursor": {} })),
        _ => json_reply(200, json!({ "data": null })),
    })
}

fn image() -> Reply {
    (200, "image/png", PNG.to_vec())
}

/// What the server answers once a blob is swept.
fn gone() -> Reply {
    json_reply(404, json!({ "_tag": "FileNotFoundError", "path": format!("/api/experimental/media/{HASH}"), "message": "gone" }))
}

/// A failure where the blob may still exist.
fn unavailable() -> Reply {
    (503, "text/plain", b"unavailable".to_vec())
}

fn shot(id: &str, images: usize) -> Value {
    let file = |i: usize| {
        json!({ "uri": format!("media:{HASH}"), "mime": "image/png", "source": { "type": "inline" }, "name": "shot.png",
                "mention": { "start": 11 + i * 10, "end": 20 + i * 10, "text": format!("[Image {}]", i + 1) } })
    };
    json!({ "id": id, "type": "user", "text": "What is in [Image 1]", "time": { "created": 1 }, "files": (0..images).map(file).collect::<Vec<_>>() })
}

/// The session with `msg_shot` (a prompt with `images` media refs) as its last user message.
fn with_shot(a: &mut App, images: usize) -> String {
    let sid = session(a);
    a.store.messages.insert(sid.clone(), vec![Message::from_value(shot("msg_shot", images)).unwrap()]);
    sid
}

fn png_uri() -> String {
    use base64::Engine as _;
    format!("data:image/png;base64,{}", base64::engine::general_purpose::STANDARD.encode(PNG))
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_transcript_reads_with_media_refs_and_undo_loads_a_refs_bytes_before_rewinding() {
    let (mut a, mut rx, log) = flow(routes(image)).await;
    let sid = with_shot(&mut a, 1);
    a.fetch_messages(&sid);
    a.fetch_pending(&sid);
    drive_until(&mut a, &mut rx, 3000, |_| log.calls().len() >= 2).await;
    assert_eq!(log.capability("GET /api/session/ses_test/message "), Some(Some("experimental-media-uri".into())));
    assert_eq!(log.capability("GET /api/session/ses_test/inbox "), Some(Some("experimental-media-uri".into())));
    // Mini mode and other readers keep inline bytes.
    a.client.messages(&sid, 10, None).await.unwrap();
    assert_eq!(log.0.lock().unwrap().last().unwrap().1, None);
    log.0.lock().unwrap().clear();
    a.store.messages.insert(sid.clone(), vec![Message::from_value(shot("msg_shot", 1)).unwrap()]);

    a.run_command("session.undo", None);
    drive_until(&mut a, &mut rx, 3000, |a| a.editor.text.contains("What is in")).await;
    let calls = log.calls();
    assert_eq!(calls.first().map(String::as_str), Some(format!("GET /api/experimental/media/{HASH} 200").as_str()));
    assert_eq!(calls.last().map(String::as_str), Some("POST /api/session/ses_test/revert/stage 200"));
    assert_eq!(a.editor.info().files.iter().map(|f| f["uri"].as_str().unwrap().to_string()).collect::<Vec<_>>(), [png_uri()]);
    // A hash names its bytes: the next restore reads nothing.
    assert!(a.media.peek(&format!("media:{HASH}")).is_some());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn undo_goes_ahead_without_an_image_the_server_no_longer_has() {
    let (mut a, mut rx, log) = flow(routes(gone)).await;
    with_shot(&mut a, 1);
    a.run_command("session.undo", None);
    drive_until(&mut a, &mut rx, 3000, |a| a.editor.text.contains("What is in")).await;
    let toast = a.toast.clone().unwrap();
    assert_eq!(
        (toast.variant.as_str(), toast.message.as_str()),
        ("warning", "An image in this message is no longer stored, so it can't be restored")
    );
    assert!(a.editor.info().files.is_empty());
    assert_eq!(log.calls().last().map(String::as_str), Some("POST /api/session/ses_test/revert/stage 200"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn several_missing_images_are_counted() {
    let (mut a, mut rx, _) = flow(routes(gone)).await;
    with_shot(&mut a, 2);
    a.run_command("session.undo", None);
    drive_until(&mut a, &mut rx, 3000, |a| a.editor.text.contains("What is in")).await;
    assert_eq!(a.toast.clone().unwrap().message, "2 images in this message are no longer stored, so they can't be restored");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn undo_keeps_the_message_when_an_image_read_fails_for_another_reason() {
    let (mut a, mut rx, log) = flow(routes(unavailable)).await;
    with_shot(&mut a, 1);
    a.run_command("session.undo", None);
    drive_until(&mut a, &mut rx, 3000, |a| a.toast.is_some()).await;
    settle(&mut a, &mut rx).await;
    assert_eq!(log.calls(), [format!("GET /api/experimental/media/{HASH} 503")]);
    let toast = a.toast.clone().unwrap();
    assert_eq!((toast.variant.as_str(), toast.message.as_str(), toast.duration), ("error", "unavailable", 5000));
    assert_eq!(a.editor.text, "");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_queued_undo_reads_media_before_cancelling() {
    for (media, files) in [(image as fn() -> Reply, 1), (gone, 0)] {
        let (mut a, mut rx, log) = flow(routes(media)).await;
        let sid = session(&mut a);
        let mut payload = shot("msg_queued", 1);
        payload.as_object_mut().unwrap().retain(|k, _| matches!(k.as_str(), "text" | "files"));
        a.store.admit_local(InboxItem {
            id: "msg_queued".into(),
            session_id: sid,
            kind: "user".into(),
            payload,
            delivery: "queue".into(),
            time: MessageTime { created: 4, ..Default::default() },
        });
        a.undo_queued("msg_queued");
        drive_until(&mut a, &mut rx, 3000, |a| a.editor.text.contains("What is in")).await;
        let status = if files == 1 { 200 } else { 404 };
        assert_eq!(log.calls(), [format!("GET /api/experimental/media/{HASH} {status}"), "DELETE /api/session/ses_test/inbox/msg_queued 204".into()]);
        assert_eq!(a.editor.info().files.len(), files);
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_queued_undo_whose_read_fails_cancels_nothing() {
    let (mut a, mut rx, log) = flow(routes(unavailable)).await;
    let sid = session(&mut a);
    let mut payload = shot("msg_queued", 1);
    payload.as_object_mut().unwrap().retain(|k, _| matches!(k.as_str(), "text" | "files"));
    a.store.admit_local(InboxItem {
        id: "msg_queued".into(),
        session_id: sid,
        kind: "user".into(),
        payload,
        delivery: "queue".into(),
        time: MessageTime { created: 4, ..Default::default() },
    });
    a.undo_queued("msg_queued");
    drive_until(&mut a, &mut rx, 3000, |a| a.toast.is_some()).await;
    settle(&mut a, &mut rx).await;
    assert_eq!(log.calls(), [format!("GET /api/experimental/media/{HASH} 503")]);
    let toast = a.toast.clone().unwrap();
    assert_eq!((toast.title.as_deref(), toast.message.as_str()), (Some("Failed to undo pending prompt"), "unavailable"));
    assert!(a.pending_actions.is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn revert_reads_media_first() {
    // Gone: the revert goes ahead with a warning.
    let (mut a, mut rx, log) = flow(routes(gone)).await;
    let sid = with_shot(&mut a, 1);
    a.revert_message(&sid, "msg_shot");
    drive_until(&mut a, &mut rx, 3000, |a| a.editor.text.contains("What is in")).await;
    assert_eq!(log.calls(), [format!("GET /api/experimental/media/{HASH} 404"), "POST /api/session/ses_test/revert/stage 200".into()]);
    assert!(a.toast.clone().unwrap().message.contains("no longer stored"));
    // Unavailable: nothing is reverted.
    let (mut a, mut rx, log) = flow(routes(unavailable)).await;
    let sid = with_shot(&mut a, 1);
    a.revert_message(&sid, "msg_shot");
    drive_until(&mut a, &mut rx, 3000, |a| a.toast.is_some()).await;
    settle(&mut a, &mut rx).await;
    assert_eq!(log.calls(), [format!("GET /api/experimental/media/{HASH} 503")]);
    assert_eq!(a.editor.text, "");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn forking_from_a_prompt_whose_image_is_gone_does_not_fork() {
    let (mut a, mut rx, log) = flow(routes(gone)).await;
    let sid = with_shot(&mut a, 1);
    a.fork(&sid, Some("msg_shot".into()));
    drive_until(&mut a, &mut rx, 3000, |a| a.toast.is_some()).await;
    settle(&mut a, &mut rx).await;
    assert_eq!(log.calls(), [format!("GET /api/experimental/media/{HASH} 404")]);
    let toast = a.toast.clone().unwrap();
    assert_eq!((toast.variant.as_str(), toast.message.as_str(), toast.duration), ("error", "gone", 5000));
    assert!(a.dialogs.is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_fork_opens_with_the_prompt_read_back() {
    let (mut a, mut rx, log) = flow(routes(image)).await;
    let sid = with_shot(&mut a, 1);
    a.fork(&sid, Some("msg_shot".into()));
    drive_until(&mut a, &mut rx, 3000, |a| a.session_id() == Some("ses_forked")).await;
    assert_eq!(log.calls()[..2], [format!("GET /api/experimental/media/{HASH} 200"), "POST /api/session/ses_test/fork 200".into()]);
    assert_eq!(a.editor.text, "What is in [Image 1]");
    assert_eq!(a.editor.info().files.iter().map(|f| f["uri"].as_str().unwrap().to_string()).collect::<Vec<_>>(), [png_uri()]);
}

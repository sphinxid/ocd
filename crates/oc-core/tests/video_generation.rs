//! Durable video jobs (`core/test/video-generation.test.ts`, 2.4.1) and the video tool's background job
//! (`tool-video-generate.test.ts`), against a local xAI video mock: each submission gets the next remote ID, whose
//! status the test sets, and a finished job's video downloads from the mock.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use axum::extract::{Path, State};
use axum::routing::{get, post};
use axum::{Json, Router};
use oc_core::{app::App, generate::VideoInput, runner::Engine, session::CreateInput, video::VideoJob};
use oc_schema::types::{LocationRef, ModelRef};
use parking_lot::Mutex;
use serde_json::{Value, json};

// The start of an MP4: a `ftyp` box, so its type is read from its bytes.
const MP4: [u8; 16] = [0, 0, 0, 0x14, b'f', b't', b'y', b'p', b'm', b'p', b'4', b'2', 0, 0, 0, 0];

#[derive(Default)]
struct Provider {
    submitted: usize,
    status: HashMap<String, &'static str>,
    bodies: Vec<Value>,
}

type Shared = Arc<Mutex<Provider>>;

async fn serve(provider: Shared) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://127.0.0.1:{}", listener.local_addr().unwrap().port());
    let origin = base.clone();
    let router = Router::new()
        .route(
            "/v1/videos/generations",
            post(|State(p): State<Shared>, Json(body): Json<Value>| async move {
                let mut p = p.lock();
                p.submitted += 1;
                p.bodies.push(body);
                Json(json!({ "request_id": format!("remote_{}", p.submitted) }))
            }),
        )
        .route(
            "/v1/videos/{id}",
            get(move |State(p): State<Shared>, Path(id): Path<String>| {
                let origin = origin.clone();
                async move {
                    let status = p.lock().status.get(&id).copied().unwrap_or("pending");
                    Json(match status {
                        "done" => json!({ "status": "done", "video": { "url": format!("{origin}/files/{id}.mp4"), "duration": 2 } }),
                        other => json!({ "status": other }),
                    })
                }
            }),
        )
        .route("/files/{name}", get(|| async { MP4.to_vec() }))
        .with_state(provider);
    tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    base
}

fn global(root: &std::path::Path) -> oc_util::Global {
    let g = |p: &str| {
        let d = root.join(p);
        std::fs::create_dir_all(&d).unwrap();
        d
    };
    oc_util::Global {
        home: g("home"),
        data: g("data"),
        cache: g("cache"),
        config: g("config"),
        state: g("state"),
        tmp: g("tmp"),
        bin: g("bin"),
        log: g("log"),
        repos: g("repos"),
    }
}

struct Harness {
    app: Arc<App>,
    engine: Arc<Engine>,
    provider: Shared,
    _root: tempfile::TempDir,
    project: std::path::PathBuf,
}

async fn harness() -> Harness {
    unsafe { std::env::set_var("OPENCODE_DISABLE_MODELS_FETCH", "1") };
    let root = tempfile::tempdir().unwrap();
    let path = oc_util::fs::canonicalize(root.path()).unwrap();
    let provider: Shared = Default::default();
    let base = serve(provider.clone()).await;
    let g = global(&path);
    let config = json!({ "providers": { "test": {
        "name": "Test",
        "package": "@opencode/ai/providers/xai/videos",
        "settings": { "baseURL": format!("{base}/v1"), "apiKey": "fixture" },
        "models": { "director": { "name": "Director", "type": "video" } },
    } } });
    std::fs::write(g.config.join("opencode.json"), config.to_string()).unwrap();
    let project = path.join("project");
    std::fs::create_dir_all(&project).unwrap();
    std::fs::write(project.join("opencode.json"), config.to_string()).unwrap();
    let app = App::new(g, oc_storage::Database::memory().unwrap(), "1.2.3");
    let engine = Engine::new(app.clone(), &oc_core::services::default_services(&app));
    Harness { app, engine, provider, _root: root, project }
}

fn input(prompt: &str) -> VideoInput {
    VideoInput {
        model: ModelRef { id: "director".into(), provider_id: "test".into(), variant: None },
        prompt: prompt.into(),
        start: None,
        end: None,
        references: vec![],
        duration: None,
        aspect: None,
        resolution: None,
        sound: None,
    }
}

/// Runs check passes until the job leaves `running`.
async fn settled(videos: &Arc<oc_core::video::VideoGeneration>, id: &str) -> VideoJob {
    for _ in 0..400 {
        videos.tick().await;
        let job = videos.get(id).await.unwrap();
        if !job.is_running() {
            return job;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    panic!("{id} never settled");
}

// video-generation.test.ts: "keeps video jobs across restarts and forgets them a day after they finish".
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn keeps_video_jobs_across_restarts_and_forgets_them_a_day_after_they_finish() {
    let h = harness().await;
    let first = h.engine.videos.clone();
    let done = first.start(input("a cat"), None).await.unwrap();
    let slow = first.start(input("a snail"), None).await.unwrap();
    assert!(oc_core::video::valid_id(&done), "{done}");
    assert_eq!(h.provider.lock().bodies[0], json!({ "model": "director", "prompt": "a cat" }));
    h.provider.lock().status.insert("remote_1".into(), "done");
    let job = settled(&first, &done).await;
    let VideoJob::Completed { blobs, .. } = &job else { panic!("{job:?}") };
    assert_eq!(blobs.len(), 1);
    assert!(blobs[0].id.starts_with("blob_"));
    assert_eq!((blobs[0].mime.as_str(), blobs[0].bytes), ("video/mp4", MP4.len() as u64));
    assert_eq!(first.get(&slow).await.unwrap(), VideoJob::Running { id: slow.clone() });
    first.release();

    // A restarted server reads the same records and takes over polling.
    let second = Arc::new(oc_core::video::VideoGeneration::new(Arc::downgrade(&h.engine)));
    assert_eq!(second.get(&done).await.unwrap(), job);
    h.provider.lock().status.insert("remote_2".into(), "done");
    assert!(matches!(settled(&second, &slow).await, VideoJob::Completed { .. }));

    // A record is dropped 24 hours after its job finished.
    let key = format!("video:{done}");
    let mut record = h.app.db.kv_get(&key).unwrap().unwrap();
    record["finished"] = json!(oc_schema::id::now_ms() as i64 - 25 * 3600 * 1000);
    h.app.db.kv_set(&key, &record).unwrap();
    second.tick().await;
    assert!(h.app.db.kv_get(&key).unwrap().is_none());
    let missing = second.get(&done).await.unwrap_err();
    assert_eq!(missing.message, format!("Video not found: {done}"));
    // An unreadable record is dropped too.
    h.app.db.kv_set("video:video_0123456789abABCDEFGHIJKLMN", &json!({ "status": "nope" })).unwrap();
    second.tick().await;
    assert!(h.app.db.kv_get("video:video_0123456789abABCDEFGHIJKLMN").unwrap().is_none());
}

// video-generation.test.ts: "waits for a job to finish and cancels a running one".
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn waits_for_a_job_to_finish_and_cancels_a_running_one() {
    let h = harness().await;
    let videos = h.engine.videos.clone();
    let done = videos.start(input("a cat"), None).await.unwrap();
    let slow = videos.start(input("a snail"), None).await.unwrap();
    let waiting = {
        let videos = videos.clone();
        let id = done.clone();
        tokio::spawn(async move { videos.wait(&id).await })
    };
    h.provider.lock().status.insert("remote_1".into(), "done");
    settled(&videos, &done).await;
    let finished = tokio::time::timeout(Duration::from_secs(30), waiting).await.unwrap().unwrap().unwrap();
    assert!(matches!(finished, VideoJob::Completed { .. }), "{finished:?}");

    videos.cancel(&slow).await.unwrap();
    assert_eq!(
        videos.wait(&slow).await.unwrap(),
        VideoJob::Error { id: slow.clone(), reason: "cancelled".into(), error: "The video generation was cancelled".into() }
    );
    // A provider failure ends the job with its reason.
    let failed = videos.start(input("a dog"), None).await.unwrap();
    h.provider.lock().status.insert("remote_3".into(), "failed");
    assert_eq!(
        settled(&videos, &failed).await,
        VideoJob::Error { id: failed.clone(), reason: "failed".into(), error: "The provider reported the job as failed".into() }
    );
    assert_eq!(videos.cancel("video_0123456789abABCDEFGHIJKLMN").await.unwrap_err().message, "Video not found: video_0123456789abABCDEFGHIJKLMN");
}

// tool-video-generate.test.ts: "starts the job, saves every video it returns and lists them all in the notice" (the
// job half: the wait, the copy into the workspace and the `<background-result kind="video">` notice).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_tracked_job_saves_the_video_and_notifies_the_session() {
    let h = harness().await;
    let session = h
        .app
        .sessions
        .create(CreateInput { location: Some(LocationRef { directory: h.project.display().to_string(), workspace_id: None }), ..Default::default() })
        .await
        .unwrap();
    let id = h.engine.videos.start(input("the kite takes off"), Some(session.location.clone())).await.unwrap();
    let file = h.project.join("kite.mp4").display().to_string();
    h.engine.track_video(oc_tools::host::VideoTrack {
        session_id: session.id.clone(),
        video_id: id.clone(),
        path: file.clone(),
        title: Some("the kite takes off".into()),
        notification_id: None,
    });
    let listed = h.app.jobs.background_jobs();
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].kind(), "video");
    assert_eq!(
        listed[0].to_value(),
        json!({ "kind": "video", "id": id, "sessionID": session.id, "status": "running", "title": "the kite takes off",
                "time": { "started": listed[0].started }, "path": file })
    );
    h.provider.lock().status.insert("remote_1".into(), "done");
    settled(&h.engine.videos, &id).await;
    let info = tokio::time::timeout(Duration::from_secs(30), h.app.jobs.wait(&id)).await.unwrap().unwrap();
    assert_eq!(info.output.as_deref(), Some(format!("Video request finished. Saved video: {file}.").as_str()));
    assert_eq!(std::fs::read(&file).unwrap(), MP4);
    let expected = format!("<background-result kind=\"video\" id=\"{id}\" state=\"completed\">\nVideo request finished. Saved video: {file}.\n</background-result>");
    let mut found = None;
    for _ in 0..200 {
        found = h
            .app
            .db
            .with(|c| {
                c.query_row(
                    "SELECT payload FROM session_inbox WHERE session_id = ?1 AND payload LIKE '%background-result%'",
                    [&session.id],
                    |r| r.get::<_, String>(0),
                )
            })
            .ok();
        if found.is_some() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    let data: Value = serde_json::from_str(&found.expect("the video notice was admitted")).unwrap();
    assert_eq!(data["text"], json!(expected), "{data}");
    assert_eq!(data["metadata"], json!({ "source": "generate_video", "jobID": id, "state": "completed", "path": file }));
}

/// Synthetic notices of a session, delivered or still in its inbox.
fn notice_texts(app: &App, session: &str) -> Vec<String> {
    let mut out: Vec<String> = app
        .db
        .session_messages(session)
        .unwrap()
        .into_iter()
        .filter_map(|(_, m)| match m {
            oc_schema::Message::Synthetic { text, .. } => Some(text),
            _ => None,
        })
        .collect();
    for (_, kind, payload, ..) in app.db.session_inbox(session).unwrap() {
        if kind == "synthetic" {
            out.push(payload["text"].as_str().unwrap_or_default().to_string());
        }
    }
    out
}

// session-execution.test.ts: "waits again for a running video after a restart, then saves it and delivers the notice",
// and a video whose session's folder is gone is reported as lost.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn waits_again_for_a_running_video_after_a_restart() {
    let h = harness().await;
    h.engine.install();
    let location = |dir: &std::path::Path| Some(LocationRef { directory: dir.display().to_string(), workspace_id: None });
    let session = h.app.sessions.create(CreateInput { location: location(&h.project), ..Default::default() }).await.unwrap();
    let gone_dir = h.project.parent().unwrap().join("gone");
    std::fs::create_dir_all(&gone_dir).unwrap();
    let gone = h.app.sessions.create(CreateInput { location: location(&gone_dir), ..Default::default() }).await.unwrap();
    std::fs::remove_dir_all(&gone_dir).unwrap();
    let id = h.engine.videos.start(input("a kite"), Some(session.location.clone())).await.unwrap();
    let file = h.project.join("running.mp4").display().to_string();
    let insert = |nid: &str, id: &str, session: &str, path: &str| {
        let recovery = json!({ "kind": "video", "sessionID": session, "path": path, "videoID": id });
        h.app
            .db
            .with(|c| {
                c.execute(
                    "INSERT INTO job (notification_id, id, session_id, child_session_id, kind, title, status, output, error, recovery, time_created, time_updated) \
                     VALUES (?, ?, ?, NULL, 'video', ?, 'running', NULL, NULL, ?, 1, 1)",
                    rusqlite::params![nid, id, session, path, recovery.to_string()],
                )
            })
            .unwrap();
    };
    insert("msg_video", &id, &session.id, &file);
    insert("msg_lost", "video_0123456789abABCDEFGHIJKLMN", &gone.id, "/tmp/lost.mp4");

    h.engine.resume_suspended().await.unwrap();
    assert_eq!(
        notice_texts(&h.app, &gone.id),
        ["<background-result kind=\"video\" id=\"video_0123456789abABCDEFGHIJKLMN\" state=\"cancelled\">\nVideo request for /tmp/lost.mp4 couldn't be saved after the server restarted: its project folder no longer exists.\n</background-result>"]
    );
    // The recovered job runs again under its own notice ID.
    assert_eq!(h.app.jobs.get(&id).unwrap().notification_id.as_deref(), Some("msg_video"));
    h.provider.lock().status.insert("remote_1".into(), "done");
    settled(&h.engine.videos, &id).await;
    let info = tokio::time::timeout(Duration::from_secs(30), h.app.jobs.wait(&id)).await.unwrap().unwrap();
    assert_eq!(info.output.as_deref(), Some(format!("Video request finished. Saved video: {file}.").as_str()));
    assert_eq!(std::fs::read(&file).unwrap(), MP4);
    let expected = format!("<background-result kind=\"video\" id=\"{id}\" state=\"completed\">\nVideo request finished. Saved video: {file}.\n</background-result>");
    for _ in 0..400 {
        if notice_texts(&h.app, &session.id).contains(&expected) {
            break;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    assert!(notice_texts(&h.app, &session.id).contains(&expected), "{:?}", notice_texts(&h.app, &session.id));
    // Both notices settled their rows.
    for _ in 0..200 {
        if h.app.jobs.pending_background().is_empty() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    assert_eq!(h.app.jobs.pending_background(), Vec::<Value>::new());
}

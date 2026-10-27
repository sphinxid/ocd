//! `debug heap-dump` (port of upstream's `packages/cli/test/debug-heap-dump.test.ts`): how the command finds a
//! server, what it prints, and that it never starts or replaces one. The server side is covered by
//! `oc-backend/tests/api.rs`.

mod common;

use std::net::TcpListener;
use std::sync::{Arc, Mutex};

use axum::Router;
use axum::extract::Request;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde_json::{Value, json};

type Handler = Arc<dyn Fn(&Request) -> Response + Send + Sync>;
/// `(method, path, authorization)` of each request the fake server received.
type Requests = Arc<Mutex<Vec<(String, String, Option<String>)>>>;

/// Answers the health probe like a ready V2 server and passes every other request to `handle`.
fn fake(handle: Handler) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!(
        "http://127.0.0.1:{}/",
        listener.local_addr().unwrap().port()
    );
    listener.set_nonblocking(true).unwrap();
    let app = Router::new().fallback(move |request: Request| {
        let handle = handle.clone();
        async move {
            if request.uri().path() == "/api/info" {
                let version = oc_util::version::VERSION;
                return axum::Json(
                    json!({ "version": version, "pid": std::process::id(), "urls": [] }),
                )
                .into_response();
            }
            handle(&request)
        }
    });
    std::thread::spawn(move || {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async move {
            let l = tokio::net::TcpListener::from_std(listener).unwrap();
            axum::serve(l, app).await.unwrap();
        });
    });
    url
}

fn register(env: &common::Env, url: &str) -> (std::path::PathBuf, Value) {
    let file = env.path("state/opencode/service.json");
    let version = oc_util::version::VERSION;
    let info =
        json!({ "version": version, "url": url, "pid": std::process::id(), "password": "secret" });
    std::fs::create_dir_all(file.parent().unwrap()).unwrap();
    std::fs::write(&file, info.to_string()).unwrap();
    (file, info)
}

fn query(request: &Request, key: &str) -> Option<String> {
    url::form_urlencoded::parse(request.uri().query().unwrap_or("").as_bytes())
        .find(|(k, _)| k == key)
        .map(|(_, v)| v.into_owned())
}

#[test]
fn prints_the_snapshot_path_from_the_registered_server_without_replacing_it() {
    let env = common::Env::new();
    let requests: Requests = Arc::default();
    let log = requests.clone();
    let url = fake(Arc::new(move |request: &Request| {
        let authorization = request
            .headers()
            .get("authorization")
            .and_then(|v| v.to_str().ok())
            .map(String::from);
        log.lock().unwrap().push((
            request.method().to_string(),
            request.uri().path().to_string(),
            authorization,
        ));
        axum::Json(
            json!({ "path": "/server/log/heap-1-2.heapsnapshot", "pid": std::process::id() }),
        )
        .into_response()
    }));
    let (file, info) = register(&env, &url);

    let out = env.run(&["debug", "heap-dump"]);

    assert_eq!(
        (
            out.status.code(),
            common::text(&out.stderr),
            common::text(&out.stdout).trim().to_string()
        ),
        (
            Some(0),
            String::new(),
            "/server/log/heap-1-2.heapsnapshot".to_string()
        )
    );
    use base64::Engine;
    let basic = format!(
        "Basic {}",
        base64::engine::general_purpose::STANDARD.encode("opencode:secret")
    );
    assert_eq!(
        *requests.lock().unwrap(),
        vec![(
            "POST".to_string(),
            "/api/debug/heap-dump".to_string(),
            Some(basic)
        )]
    );
    let stored: Value = serde_json::from_str(&std::fs::read_to_string(file).unwrap()).unwrap();
    assert_eq!(stored, info);
}

#[test]
fn sends_directory_as_an_absolute_path_and_warns_only_when_the_server_ignores_it() {
    let env = common::Env::new();
    // The command runs from `work`, so a relative directory resolves there.
    let directory = env.path("work").join("snapshots").display().to_string();
    let sent: Arc<Mutex<Vec<String>>> = Arc::default();
    let log = sent.clone();
    let url = fake(Arc::new(move |request: &Request| {
        let requested = query(request, "directory").unwrap_or_default();
        let mut sent = log.lock().unwrap();
        sent.push(requested.clone());
        // Answer like a current server, which joins the directory and file name, for the first request, and like
        // one that predates `directory` for the second.
        let folder = if sent.len() == 1 {
            requested.trim_end_matches('/').to_string()
        } else {
            "/server/log".to_string()
        };
        axum::Json(
            json!({ "path": format!("{folder}/heap-1-2.heapsnapshot"), "pid": std::process::id() }),
        )
        .into_response()
    }));
    register(&env, &url);

    // A trailing separator is not a different directory.
    let with_separator = format!("{directory}/");
    let current = env.run(&["debug", "heap-dump", "--directory", &with_separator]);
    let old = env.run(&["debug", "heap-dump", "--directory", "snapshots"]);

    assert_eq!(
        *sent.lock().unwrap(),
        vec![with_separator.clone(), directory.clone()]
    );
    assert_eq!(
        [&current, &old].map(|o| (o.status.code(), common::text(&o.stderr).trim().to_string())),
        [
            (Some(0), String::new()),
            (Some(0), "The server ignored --directory. Update and restart it to choose where snapshots go.".to_string())
        ]
    );
    assert_eq!(
        common::text(&current.stdout).trim(),
        format!("{directory}/heap-1-2.heapsnapshot")
    );
}

#[test]
fn fails_without_starting_a_server_when_none_is_registered() {
    let env = common::Env::new();

    let out = env.run(&["debug", "heap-dump"]);

    assert!(
        common::text(&out.stderr).contains("No background server is running"),
        "{}",
        common::text(&out.stderr)
    );
    assert_eq!(out.status.code(), Some(1));
    assert!(!env.path("state/opencode/service.json").exists());
}

#[test]
fn explains_when_the_server_predates_heap_dumps() {
    let env = common::Env::new();
    let url = fake(Arc::new(|_: &Request| {
        StatusCode::NOT_FOUND.into_response()
    }));

    let out = env
        .command(&["debug", "heap-dump", "--server", &url])
        .env("OPENCODE_PASSWORD", "secret")
        .output()
        .unwrap();

    assert_eq!(
        common::text(&out.stderr).trim(),
        format!("The server at {url} does not support heap dumps. Update and restart it first.")
    );
    assert_eq!(out.status.code(), Some(1));
}

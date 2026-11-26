//! `ocd machine add|list|remove` (2.4.1 `cli/src/commands/handlers/machine/*.ts`, `cli/test/machine.test.ts`): against
//! a stand-in for an upstream server with the `machines` flag on, which ocd's own server never is, and against `ocd serve`,
//! which answers like a release build with the flag off. The formatting itself is covered by `commands::machine::tests`.

mod common;

use std::io::{BufRead, BufReader};
use std::net::TcpListener;
use std::process::Stdio;
use std::sync::{Arc, Mutex};

use axum::Router;
use axum::extract::Request;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde_json::{Value, json};

/// `(method, path, body)` of each machine request the stand-in received.
type Requests = Arc<Mutex<Vec<(String, String, Value)>>>;

const DISABLED: &str = "Machines are experimental and disabled on this server. Turn them on with OPENCODE_FLAGS=machines or experimental.machines: true in your config.\n";

fn error(status: StatusCode, body: Value) -> Response {
    (status, axum::Json(body)).into_response()
}

/// A server with the `machines` flag on, holding `gpu` (paired, connected over iroh) and `worker` (waiting). `forbid`
/// turns it into one with the flag off.
fn upstream(forbid: bool) -> (String, Requests) {
    let requests: Requests = Arc::default();
    let log = requests.clone();
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://127.0.0.1:{}", listener.local_addr().unwrap().port());
    listener.set_nonblocking(true).unwrap();
    let app = Router::new().fallback(move |request: Request| {
        let log = log.clone();
        async move {
            let (parts, body) = request.into_parts();
            let path = parts.uri.path().to_string();
            if path == "/api/info" {
                let urls = ["http://127.0.0.1:4096", "https://example.com"];
                return axum::Json(json!({ "version": oc_util::version::VERSION, "pid": 1, "urls": urls })).into_response();
            }
            let bytes = axum::body::to_bytes(body, usize::MAX).await.unwrap();
            let body: Value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
            log.lock().unwrap().push((parts.method.to_string(), path.clone(), body.clone()));
            if forbid {
                let message = "Machines are disabled; enable experimental.machines";
                return error(StatusCode::FORBIDDEN, json!({ "_tag": "ForbiddenError", "message": message }));
            }
            match (parts.method.as_str(), path.as_str()) {
                ("GET", "/api/experimental/machine") => axum::Json(json!({ "data": [
                    { "id": "gpu", "online": true, "paired": true, "os": "linux", "arch": "x64",
                      "connection": { "kind": "iroh", "path": "direct", "rtt": 81.6 } },
                    { "id": "worker", "online": false, "paired": false },
                ] }))
                .into_response(),
                ("POST", "/api/experimental/machine") => match body["name"].as_str().unwrap_or("") {
                    "Bad" => error(
                        StatusCode::BAD_REQUEST,
                        json!({ "_tag": "InvalidRequestError", "message": "invalid", "kind": "invalid_name", "field": "name" }),
                    ),
                    "gpu" if body.get("replace").is_none() => {
                        error(StatusCode::CONFLICT, json!({ "_tag": "ConflictError", "message": "paired", "resource": "gpu" }))
                    }
                    name => {
                        let mut reach = body["reach"].as_array().cloned().unwrap_or_default();
                        reach.push(json!({ "type": "iroh" }));
                        axum::Json(json!({ "data": { "name": name, "join": "ocjoin_example", "expires": 1, "reach": reach } }))
                            .into_response()
                    }
                },
                ("DELETE", "/api/experimental/machine/gpu") => StatusCode::NO_CONTENT.into_response(),
                ("DELETE", other) => {
                    let name = other.rsplit('/').next().unwrap_or("").to_string();
                    let message = format!("No machine named {}.", json!(name));
                    error(StatusCode::NOT_FOUND, json!({ "_tag": "MachineNotFoundError", "machine": name, "message": message }))
                }
                _ => StatusCode::NOT_FOUND.into_response(),
            }
        }
    });
    std::thread::spawn(move || {
        let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        rt.block_on(async move {
            axum::serve(tokio::net::TcpListener::from_std(listener).unwrap(), app).await.unwrap();
        });
    });
    (url, requests)
}

fn run(env: &common::Env, args: &[&str]) -> (Option<i32>, String, String) {
    let out = env.run(args);
    (out.status.code(), common::text(&out.stdout), common::text(&out.stderr))
}

fn ok(stdout: &str) -> (Option<i32>, String, String) {
    (Some(0), stdout.to_string(), String::new())
}

fn failed(stderr: &str) -> (Option<i32>, String, String) {
    (Some(1), String::new(), stderr.to_string())
}

#[test]
fn the_commands_manage_machines_on_a_server_that_has_them() {
    let env = common::Env::new();
    let (url, requests) = upstream(false);
    let link = "/api/experimental/machine/link";

    assert_eq!(
        run(&env, &["machine", "list", "--server", &url]),
        ok("gpu     paired   connected  linux/x64  iroh direct 82ms\nworker  waiting  offline\n")
    );
    // Off a terminal, `add` prints only the join string, so it can be piped.
    assert_eq!(run(&env, &["machine", "--server", &url, "add", "laptop"]), ok("ocjoin_example\n"));
    assert_eq!(
        run(&env, &["machine", "add", "gpu", "--url", "https://override.example.com:8443/", "--replace", "--server", &url]),
        ok("ocjoin_example\n")
    );
    assert_eq!(run(&env, &["--server", &url, "machine", "remove", "gpu"]), ok("Removed machine gpu.\n"));
    assert_eq!(
        *requests.lock().unwrap(),
        vec![
            ("GET".to_string(), "/api/experimental/machine".to_string(), Value::Null),
            (
                "POST".to_string(),
                "/api/experimental/machine".to_string(),
                json!({ "name": "laptop", "reach": [
                    { "type": "url", "url": format!("wss://example.com{link}") },
                    { "type": "url", "url": format!("ws://127.0.0.1:4096{link}") },
                ] })
            ),
            (
                "POST".to_string(),
                "/api/experimental/machine".to_string(),
                json!({ "name": "gpu", "reach": [{ "type": "url", "url": format!("wss://override.example.com:8443{link}") }], "replace": true })
            ),
            ("DELETE".to_string(), "/api/experimental/machine/gpu".to_string(), Value::Null),
        ]
    );

    assert_eq!(
        run(&env, &["machine", "add", "Bad", "--server", &url]),
        failed("\"Bad\" is not a valid machine name. Use lowercase letters, digits, and hyphens, starting with a letter, 2 to 63 characters, and not \"local\".\n")
    );
    assert_eq!(
        run(&env, &["machine", "add", "gpu", "--server", &url]),
        failed("Machine \"gpu\" is already paired. Pass --replace to issue a new join string; once the new computer joins, the computer currently paired stops being able to connect.\n")
    );
    assert_eq!(run(&env, &["machine", "remove", "nope", "--server", &url]), failed("No machine named \"nope\".\n"));
}

#[test]
fn the_commands_say_when_machines_are_disabled_on_the_server() {
    let env = common::Env::new();
    let (url, requests) = upstream(true);
    for args in [vec!["machine", "list"], vec!["machine", "add", "gpu"], vec!["machine", "remove", "gpu"]] {
        let args: Vec<&str> = args.into_iter().chain(["--server", url.as_str()]).collect();
        assert_eq!(run(&env, &args), failed(DISABLED), "{args:?}");
    }
    assert_eq!(requests.lock().unwrap().len(), 3);
}

#[test]
fn the_commands_refuse_a_server_that_does_not_keep_running() {
    let env = common::Env::new();
    let standalone = "--standalone starts a server for this command only; a machine needs one that keeps running.\n";
    assert_eq!(run(&env, &["machine", "list", "--standalone"]), failed(standalone));
    assert_eq!(run(&env, &["machine", "--standalone", "add", "gpu"]), failed(standalone));
    // Before `--server` is looked at.
    assert_eq!(run(&env, &["machine", "remove", "gpu", "--standalone", "--server", "http://127.0.0.1:9"]), failed(standalone));

    std::fs::create_dir_all(env.path("config/opencode")).unwrap();
    std::fs::write(env.path("config/opencode/service.json"), r#"{"disabled": true}"#).unwrap();
    let disabled = "Managing machines requires the background service; run `ocd service unset disabled` first.\n";
    for args in [vec!["machine", "list"], vec!["machine", "add", "gpu"], vec!["machine", "remove", "gpu"]] {
        assert_eq!(run(&env, &args), failed(disabled), "{args:?}");
    }
    // `--server` names a server that keeps running, so the disabled service does not matter.
    let (url, _) = upstream(true);
    assert_eq!(run(&env, &["machine", "list", "--server", &url]), failed(DISABLED));
    // Failing to reach it is reported like any server error, without the `Error:` prefix.
    let (code, stdout, stderr) = run(&env, &["machine", "list", "--server", "http://127.0.0.1:9"]);
    assert_eq!((code, stdout.as_str()), (Some(1), ""));
    assert!(stderr.starts_with("Could not reach server at http://127.0.0.1:9: "), "{stderr}");
}

/// `ocd serve` answers the machine routes as a 2.4.1 release build with the `machines` flag off, and so does it when
/// the flag is on (ocd has no machines), so `ocd machine` always says they are disabled there.
#[test]
fn ocd_servers_have_machines_disabled_even_with_the_flag_on() {
    for flags in [None, Some("machines"), Some("all")] {
        let env = common::Env::new();
        let mut serve = env.command(&["serve", "--port", "0"]);
        serve.env("OPENCODE_PASSWORD", "pw").stdout(Stdio::piped()).stderr(Stdio::null());
        if let Some(flags) = flags {
            serve.env("OPENCODE_FLAGS", flags);
        }
        let mut child = serve.spawn().unwrap();
        let stdout = child.stdout.take().unwrap();
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines().map_while(Result::ok) {
                let _ = tx.send(line);
            }
        });
        let first = rx.recv_timeout(std::time::Duration::from_secs(120)).expect("server did not start");
        let url = first.strip_prefix("server listening on ").expect(&first).to_string();
        let outputs: Vec<_> = [vec!["machine", "list"], vec!["machine", "add", "gpu"], vec!["machine", "remove", "gpu"]]
            .into_iter()
            .map(|args| {
                let args: Vec<&str> = args.into_iter().chain(["--server", url.as_str()]).collect();
                let out = env.command(&args).env("OPENCODE_PASSWORD", "pw").output().unwrap();
                (out.status.code(), common::text(&out.stdout), common::text(&out.stderr))
            })
            .collect();
        let _ = child.kill();
        let _ = child.wait();
        for output in outputs {
            assert_eq!(output, failed(DISABLED), "{flags:?}");
        }
    }
}

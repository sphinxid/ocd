//! End-to-end tests of the built binary with isolated XDG directories: `debug paths`, the
//! background service (`service set/start/status/stop`), `debug config`, `models` with a custom
//! provider, and `run` against a mock OpenAI-compatible SSE provider.

mod common;

use std::net::TcpListener;
use std::sync::{Arc, Mutex};

use axum::{Router, body::Body, response::Response, routing::post};
use serde_json::{Value, json};

fn free_port() -> u16 {
    TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port()
}

fn sse(chunks: &[Value]) -> Response {
    let mut body = String::new();
    for c in chunks {
        body.push_str(&format!("data: {c}\n\n"));
    }
    body.push_str("data: [DONE]\n\n");
    Response::builder().header("content-type", "text/event-stream").body(Body::from(body)).unwrap()
}

/// A mock OpenAI-compatible provider: a `read` tool call when the prompt says USE_TOOL, else text. Records the
/// kind of each request (`title` or `step`) in arrival order.
fn start_mock() -> (u16, Arc<Mutex<Vec<&'static str>>>) {
    let requests = Arc::new(Mutex::new(Vec::new()));
    let log = requests.clone();
    let app = Router::new().route(
        "/v1/chat/completions",
        post(move |body: axum::Json<Value>| async move {
            let text = body.0.to_string();
            let title = text.contains("You name conversations");
            log.lock().unwrap().push(if title { "title" } else { "step" });
            if title {
                return sse(&[
                    json!({"id":"t","choices":[{"index":0,"delta":{"role":"assistant","content":"Mock title"}}]}),
                    json!({"id":"t","choices":[{"index":0,"delta":{},"finish_reason":"stop"}]}),
                ]);
            }
            if text.contains("REFUSE") {
                return Response::builder()
                    .status(400)
                    .header("content-type", "application/json")
                    .body(Body::from(json!({"error": {"message": "mock refuses", "type": "invalid_request_error"}}).to_string()))
                    .unwrap();
            }
            let has_tool_result = body.0["messages"].as_array().is_some_and(|m| m.iter().any(|m| m["role"] == "tool"));
            if text.contains("USE_TOOL") && !has_tool_result {
                return sse(&[
                    json!({"id":"c","choices":[{"index":0,"delta":{"role":"assistant","tool_calls":[{"index":0,"id":"call_1","type":"function","function":{"name":"read","arguments":"{\"path\":\"hello.txt\"}"}}]}}]}),
                    json!({"id":"c","choices":[{"index":0,"delta":{},"finish_reason":"tool_calls"}],"usage":{"prompt_tokens":5,"completion_tokens":2,"total_tokens":7}}),
                ]);
            }
            sse(&[
                json!({"id":"c","choices":[{"index":0,"delta":{"role":"assistant","content":"Hello "}}]}),
                json!({"id":"c","choices":[{"index":0,"delta":{"content":"from mock."}}]}),
                json!({"id":"c","choices":[{"index":0,"delta":{},"finish_reason":"stop"}],"usage":{"prompt_tokens":5,"completion_tokens":2,"total_tokens":7}}),
            ])
        }),
    );
    // Keep the listener the port came from: probing a free port and binding it again races other tests.
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    listener.set_nonblocking(true).unwrap();
    std::thread::spawn(move || {
        let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        rt.block_on(async move {
            let l = tokio::net::TcpListener::from_std(listener).unwrap();
            axum::serve(l, app).await.unwrap();
        });
    });
    (port, requests)
}

fn write_config(env: &common::Env, provider_port: u16) {
    std::fs::write(
        env.path("work/opencode.json"),
        json!({
            "model": "mock/m1",
            "providers": {"mock": {
                "name": "Mock",
                "package": "@opencode/ai/providers/openai-compatible",
                "settings": {"baseURL": format!("http://127.0.0.1:{provider_port}/v1"), "apiKey": "secret-key"},
                "models": {"m1": {"name": "Mock One", "limit": {"context": 100000, "output": 4096}}}
            }}
        })
        .to_string(),
    )
    .unwrap();
    std::fs::write(env.path("work/hello.txt"), "hello from file\n").unwrap();
}

#[test]
fn debug_paths_lists_global_paths_without_a_server() {
    let env = common::Env::new();
    let out = env.run(&["debug", "paths"]);
    assert_eq!(out.status.code(), Some(0));
    let stdout = common::text(&out.stdout);
    let data = env.path("data").join("opencode");
    let tmp = std::fs::canonicalize(std::env::temp_dir().join("opencode")).unwrap();
    let expected = [
        ("home", env.path("home")),
        ("data", data.clone()),
        ("cache", env.path("cache").join("opencode")),
        ("config", env.path("config").join("opencode")),
        ("state", env.path("state").join("opencode")),
        ("tmp", tmp),
        ("bin", env.path("cache").join("opencode").join("bin")),
        ("log", data.join("log")),
        ("repos", data.join("repos")),
        ("db", data.join("opencode.db")),
    ]
    .iter()
    .map(|(k, v)| format!("{k:<10} {}\n", v.display()))
    .collect::<String>();
    assert_eq!(stdout, expected);
    // directories are created as a side effect; no service was started
    assert!(env.path("state/opencode").is_dir());
    assert!(!env.path("state/opencode/service.json").exists());

    let out = env.run(&["debug", "paths", "db"]);
    assert_eq!(common::text(&out.stdout), format!("{}\n", data.join("opencode.db").display()));
}

#[test]
fn version_and_parse_errors() {
    let env = common::Env::new();
    let out = env.run(&["--version"]);
    assert_eq!(common::text(&out.stdout), common::version_line());
    let out = env.run(&["run", "--format", "xml"]);
    assert_eq!(out.status.code(), Some(1));
    assert!(common::text(&out.stdout).starts_with("DESCRIPTION\n  Run OpenCode with a message\n"));
    assert_eq!(common::text(&out.stderr), "\nERROR\n  Invalid value for flag --format: \"xml\". Expected: \"default\" | \"json\"\n");
}

#[test]
fn service_lifecycle_debug_config_and_models() {
    let env = common::Env::new();
    write_config(&env, 9);
    let port = free_port();
    let set = env.run(&["service", "set", "port", &port.to_string()]);
    assert_eq!(set.status.code(), Some(0), "{}", common::text(&set.stderr));
    assert_eq!(common::text(&env.run(&["service", "get", "port"]).stdout), format!("{port}\n"));
    assert_eq!(common::text(&env.run(&["service", "status"]).stdout), "stopped\n");

    let start = env.run(&["service", "start"]);
    assert_eq!(common::text(&start.stdout), format!("http://127.0.0.1:{port}\n"), "{}", common::text(&start.stderr));
    let registration: Value = serde_json::from_str(&std::fs::read_to_string(env.path("state/opencode/service.json")).unwrap()).unwrap();
    assert_eq!(registration["url"], format!("http://127.0.0.1:{port}"));
    assert_eq!(registration["version"], "2.1.4");
    // the generated password was persisted in the service config
    let config: Value = serde_json::from_str(&std::fs::read_to_string(env.path("config/opencode/service.json")).unwrap()).unwrap();
    assert_eq!(config["password"], registration["password"]);
    assert_eq!(common::text(&env.run(&["service", "status"]).stdout), format!("http://127.0.0.1:{port}\n"));

    let debug = env.run(&["debug", "config"]);
    assert_eq!(debug.status.code(), Some(0), "{}", common::text(&debug.stderr));
    let entries: Value = serde_json::from_str(&common::text(&debug.stdout)).unwrap();
    let doc = entries.as_array().unwrap().iter().find(|e| e["type"] == "document").unwrap();
    assert_eq!(doc["info"]["providers"]["mock"]["settings"]["apiKey"], "***");

    let models = env.run(&["models"]);
    assert_eq!(models.status.code(), Some(0), "{}", common::text(&models.stderr));
    assert_models(&common::text(&models.stdout));

    let stop = env.run(&["service", "stop"]);
    assert_eq!(stop.status.code(), Some(0));
    assert_eq!(common::text(&env.run(&["service", "status"]).stdout), "stopped\n");
    assert!(!env.path("state/opencode/service.json").exists());
}

/// `ocd models` lists the configured provider first, then OpenCode Zen's free models (enabled with the public key,
/// like upstream).
fn assert_models(stdout: &str) {
    let lines: Vec<&str> = stdout.lines().collect();
    assert_eq!(lines.first(), Some(&"mock/m1"), "{stdout}");
    assert!(lines[1..].iter().all(|l| l.starts_with("opencode/")), "{stdout}");
}

#[test]
fn models_with_standalone_server() {
    let env = common::Env::new();
    write_config(&env, 9);
    let out = env.run(&["models", "--standalone"]);
    assert_eq!(out.status.code(), Some(0), "{}", common::text(&out.stderr));
    assert_models(&common::text(&out.stdout));
}

#[test]
fn run_streams_text_and_tools_from_a_mock_provider() {
    let env = common::Env::new();
    let (provider, _) = start_mock();
    write_config(&env, provider);

    // default format, stdout not a TTY: assistant text on stdout. Like upstream's, `run` handles the events of
    // a step that a fast provider answers at once only after `session.wait` answered (outside a git repository
    // the runner never waits for I/O between them and the session going idle), so there is no step header and
    // the text comes from the projected message.
    let out = env.run(&["run", "--standalone", "say", "hi"]);
    assert_eq!(out.status.code(), Some(0), "{}", common::text(&out.stderr));
    assert_eq!(common::text(&out.stdout), "Hello from mock.\n");
    assert_eq!(common::text(&out.stderr), "");

    // JSON format with a tool call: the first step live (its tool ran and the next step's request went out
    // before the session went idle), the last one projected.
    let out = env.run(&["run", "--standalone", "--format", "json", "USE_TOOL", "now"]);
    assert_eq!(out.status.code(), Some(0), "{}", common::text(&out.stderr));
    let lines: Vec<Value> = common::text(&out.stdout).lines().map(|l| serde_json::from_str(l).unwrap()).collect();
    let types: Vec<&str> = lines.iter().map(|l| l["type"].as_str().unwrap()).collect();
    assert_eq!(types, vec!["step_start", "tool_use", "step_finish", "text"]);
    let tool = &lines[1]["part"];
    assert_eq!(tool["tool"], "read");
    assert_eq!(tool["state"]["status"], "completed");
    assert!(tool["state"]["output"].as_str().unwrap().contains("hello from file"));
    let text = &lines[3]["part"];
    assert_eq!(text["text"], "Hello from mock.");
    let message = text["messageID"].as_str().unwrap();
    assert_eq!(text["id"], format!("prt_{}_text-0", message.strip_prefix("msg_").unwrap()));
    let keys: Vec<&String> = lines[3].as_object().unwrap().keys().collect();
    assert_eq!(keys, vec!["type", "timestamp", "sessionID", "part"]);

    // A non-retryable 400 at once: only the error, no step header (`step_start` in JSON), like upstream.
    let out = env.run(&["run", "--standalone", "REFUSE", "now"]);
    assert_eq!(out.status.code(), Some(1));
    assert_eq!(common::text(&out.stdout), "");
    assert_eq!(common::text(&out.stderr), "\x1b[91m\x1b[1mError: \x1b[0mmock refuses\n");
    let out = env.run(&["run", "--standalone", "--format", "json", "REFUSE", "now"]);
    assert_eq!(out.status.code(), Some(1));
    let lines: Vec<Value> = common::text(&out.stdout).lines().map(|l| serde_json::from_str(l).unwrap()).collect();
    assert_eq!(lines.len(), 1, "{}", common::text(&out.stdout));
    assert_eq!(lines[0]["type"], "error");
    assert_eq!(lines[0]["error"]["message"], "mock refuses");
    assert_eq!(lines[0]["error"]["status"], 400);

    // missing message
    let out = env.run(&["run", "--standalone"]);
    assert_eq!(out.status.code(), Some(1));
    assert_eq!(common::text(&out.stderr), "\x1b[91m\x1b[1mError: \x1b[0mYou must provide a message\n");
    let out = env.run(&["run", "--standalone", "--format", "json"]);
    assert_eq!(out.status.code(), Some(1));
    let line: Value = serde_json::from_str(common::text(&out.stdout).trim()).unwrap();
    assert_eq!(line["error"], json!({"type": "unknown", "message": "You must provide a message"}));
    assert_eq!(line["sessionID"], "");

    // --server and --standalone together fail before running (top-level error)
    let out = env.run(&["run", "--standalone", "--server", "http://127.0.0.1:1", "x"]);
    assert_eq!(out.status.code(), Some(1));
    assert!(common::text(&out.stderr).contains("--server and --standalone cannot be combined"));
}

/// In a git repository the step's end snapshot is the runner's last I/O wait before the session goes idle, so
/// upstream's `run` prints the step header (and the text) live and skips only what follows that snapshot (the
/// `step_finish`; for a failed step its `session.step.failed`, the error then coming from the execution's end).
#[test]
fn run_in_a_git_repository_prints_the_last_step_live_up_to_its_end_snapshot() {
    let env = common::Env::new();
    let (provider, _) = start_mock();
    write_config(&env, provider);
    for args in [&["init", "-q"][..], &["add", "-A"], &["-c", "user.name=t", "-c", "user.email=t@t", "commit", "-qm", "init"]] {
        let status = std::process::Command::new("git").args(args).current_dir(env.path("work")).env("HOME", env.path("home")).status().unwrap();
        assert!(status.success());
    }

    let out = env.run(&["run", "--standalone", "say", "hi"]);
    assert_eq!(out.status.code(), Some(0), "{}", common::text(&out.stderr));
    assert_eq!(common::text(&out.stdout), "Hello from mock.\n");
    assert_eq!(common::text(&out.stderr), "\x1b[0m\n> build · m1\n");
    let out = env.run(&["run", "--standalone", "--format", "json", "say", "hi"]);
    assert_eq!(out.status.code(), Some(0), "{}", common::text(&out.stderr));
    let lines: Vec<Value> = common::text(&out.stdout).lines().map(|l| serde_json::from_str(l).unwrap()).collect();
    let types: Vec<&str> = lines.iter().map(|l| l["type"].as_str().unwrap()).collect();
    assert_eq!(types, vec!["step_start", "text"]);
    assert!(lines[0]["part"]["snapshot"].is_string());

    let out = env.run(&["run", "--standalone", "REFUSE", "now"]);
    assert_eq!(out.status.code(), Some(1));
    assert_eq!(common::text(&out.stderr), "\x1b[0m\n> build · m1\n\x1b[91m\x1b[1mError: \x1b[0mmock refuses\n");
    let out = env.run(&["run", "--standalone", "--format", "json", "REFUSE", "now"]);
    assert_eq!(out.status.code(), Some(1));
    let lines: Vec<Value> = common::text(&out.stdout).lines().map(|l| serde_json::from_str(l).unwrap()).collect();
    let types: Vec<&str> = lines.iter().map(|l| l["type"].as_str().unwrap()).collect();
    assert_eq!(types, vec!["step_start", "error"]);
}

/// Like upstream, a new session's title request goes out before its first step's (the title starts before the
/// step loads its model context), and `run` does not wait for it at exit: with a fast provider, a one-step `run`
/// still ends up titled.
#[test]
fn run_sends_the_title_request_before_the_first_step() {
    let env = common::Env::new();
    let (provider, requests) = start_mock();
    write_config(&env, provider);
    for _ in 0..3 {
        requests.lock().unwrap().clear();
        let out = env.run(&["run", "--standalone", "say", "hi"]);
        assert_eq!(out.status.code(), Some(0), "{}", common::text(&out.stderr));
        assert_eq!(*requests.lock().unwrap(), vec!["title", "step"]);
    }
    let out = env.run(&["session", "list", "--standalone"]);
    assert_eq!(out.status.code(), Some(0), "{}", common::text(&out.stderr));
    assert_eq!(common::text(&out.stdout).matches("Mock title").count(), 3, "{}", common::text(&out.stdout));
}

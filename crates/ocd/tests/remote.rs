//! Remote access (`pair --remote`, `service set remote`) of the built binary, against the fake OpenTunnel service
//! from `oc-tunnel` (127.0.0.1 only, through `OCD_TEST_OPENTUNNEL_API`).

mod common;

use std::net::TcpListener;
use std::time::{Duration, Instant};

use oc_tunnel::fake::{FakeRelay, Options};
use serde_json::{Value, json};

const WARNING: &str = "This will expose your OpenCode server to the internet.\nTraffic is end-to-end encrypted, and access requires your server password.\nTo pair locally instead, run `ocd pair` without the --remote flag.";

fn free_port() -> u16 {
    loop {
        let port = TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
        // Never the user's service ports.
        if ![49374, 49375, 28894, 28895].contains(&port) {
            return port;
        }
    }
}

fn read_json(path: &std::path::Path) -> Value {
    serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap()
}

/// The service config without the password the service saves when it first listens.
fn without_password(mut config: Value) -> Value {
    if let Some(o) = config.as_object_mut() {
        o.remove("password");
    }
    config
}

/// Stops the background service when the test ends, even on failure.
struct Service<'a> {
    env: &'a common::Env,
    api: String,
}

impl Service<'_> {
    fn run(&self, args: &[&str]) -> std::process::Output {
        let mut command = self.env.command(args);
        command.env("OCD_TEST_OPENTUNNEL_API", &self.api);
        tokio::task::block_in_place(|| command.output().unwrap())
    }
}

impl Drop for Service<'_> {
    fn drop(&mut self) {
        let _ = self.env.command(&["service", "stop"]).output();
    }
}

fn files_under(dir: &std::path::Path) -> Vec<std::path::PathBuf> {
    let mut out = Vec::new();
    if let Ok(entries) = std::fs::read_dir(dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                out.extend(files_under(&path));
            } else {
                out.push(path);
            }
        }
    }
    out
}

#[test]
fn pair_remote_without_a_terminal_fails_before_enabling_anything() {
    let env = common::Env::new();
    let pair = env.run(&["pair", "--remote"]);
    assert_eq!(pair.status.code(), Some(1));
    assert_eq!(common::text(&pair.stdout), "");
    assert_eq!(common::text(&pair.stderr), format!("{WARNING}\nRun interactively or pass --yes to continue.\n"));
    assert!(files_under(&env.path("config")).iter().all(|p| !p.to_string_lossy().contains("service")));
    assert!(files_under(&env.path("state")).iter().all(|p| !p.to_string_lossy().contains("service")));
    let help = env.run(&["pair", "--help"]);
    assert_eq!(help.status.code(), Some(0));
    assert!(common::text(&help.stdout).contains("--yes"));

    let both = env.run(&["pair", "--remote", "--url", "http://example.test:1"]);
    assert_eq!(both.status.code(), Some(1));
    assert_eq!(common::text(&both.stderr), "Error: --remote cannot be combined with --url\n");
}

#[test]
fn service_remote_accepts_only_booleans_and_persists_across_set_and_unset() {
    let env = common::Env::new();
    let file = env.path("config/opencode/service.json");
    assert_eq!(common::text(&env.run(&["service", "get", "remote"]).stdout), "false\n");
    let on = env.run(&["service", "set", "remote", "on"]);
    assert_eq!(on.status.code(), Some(1));
    assert_eq!(common::text(&on.stderr), "Error: Remote must be true or false\n");
    assert!(!file.exists());
    assert_eq!(env.run(&["service", "set", "remote", "false"]).status.code(), Some(0));
    assert_eq!(read_json(&file), json!({}));
    assert_eq!(common::text(&env.run(&["service", "get", "remote"]).stdout), "false\n");
    assert_eq!(env.run(&["service", "unset", "remote"]).status.code(), Some(0));
    assert_eq!(read_json(&file), json!({}));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn pair_remote_creates_the_tunnel_and_serves_the_route_through_the_relay() {
    let relay = FakeRelay::start(Options::default()).await;
    let env = common::Env::new();
    let service = Service { env: &env, api: relay.api.to_string() };
    let port = free_port();
    assert_eq!(service.run(&["service", "set", "port", &port.to_string()]).status.code(), Some(0));

    let pair = service.run(&["pair", "--remote", "--yes"]);
    let stdout = common::text(&pair.stdout);
    let stderr = common::text(&pair.stderr);
    assert_eq!(pair.status.code(), Some(0), "{stdout}\n{stderr}");
    assert_eq!(stderr, "Setting up remote access; this can take a minute...\n");
    let warning: String = WARNING
        .lines()
        .enumerate()
        .map(|(i, l)| if i == 0 { format!("▲  {l}\n") } else { format!("│  {l}\n") })
        .collect();
    assert!(
        stdout.starts_with(&format!(
            "│\n{warning}\n  Open a link to connect. Links work once and expire in 5 minutes.\n\n"
        )),
        "{stdout}"
    );
    let link = regex::Regex::new(
        r"\n  https://([0-9a-f]{16})\.([a-z0-9]+\.fake-opentunnel\.test)/auth/connect/[A-Za-z0-9_-]+\n\n",
    )
    .unwrap();
    let captures = link.captures(&stdout).unwrap_or_else(|| panic!("{stdout}"));
    let (route, hostname) = (captures[1].to_string(), captures[2].to_string());
    // The QR code follows, framed by its light border.
    assert!(stdout.contains("\n  ██"), "{stdout}");

    let config = without_password(read_json(&env.path("config/opencode/service.json")));
    assert_eq!(config, json!({"remote": {"route": route}, "remoteAccepted": true, "port": port}));
    let identity = read_json(&env.path("data/opentunnel/default/tunnel.json"));
    assert_eq!(identity["hostname"], json!(hostname));
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(env.path("data/opentunnel/default/private-key.pem")).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
    }

    // A public HTTPS request reaches the service through the relay (TLS ends in the service with the tunnel's
    // certificate, so only the fake CA has to be trusted).
    let domain = format!("{route}.{hostname}");
    let client = reqwest::Client::builder()
        .add_root_certificate(reqwest::Certificate::from_pem(relay.ca_pem().as_bytes()).unwrap())
        .resolve(&domain, relay.public)
        .build()
        .unwrap();
    let response = client.get(format!("https://{domain}:{}/api/info", relay.public.port())).send().await.unwrap();
    assert_eq!(response.status().as_u16(), 401);

    // Pairing again goes straight through, and `service get` hides the route.
    let again = service.run(&["pair", "--remote"]);
    assert_eq!(again.status.code(), Some(0));
    assert!(common::text(&again.stdout).starts_with(&format!(
        "\n  Open a link to connect. Links work once and expire in 5 minutes.\n\n  https://{domain}/auth/connect/"
    )));
    let get = common::text(&service.run(&["service", "get"]).stdout);
    assert!(get.contains("\"remote\": {}") && !get.contains(&route), "{get}");
    assert_eq!(common::text(&service.run(&["service", "get", "remote"]).stdout), "true\n");

    // A rejected token stops the tunnel for good and says how to retry.
    relay.options(|o| o.attach_error = Some("bad_token".into()));
    assert_eq!(service.run(&["service", "stop"]).status.code(), Some(0));
    assert_eq!(service.run(&["service", "start"]).status.code(), Some(0));
    let deadline = Instant::now() + Duration::from_secs(20);
    let expected = "remote access tunnel stopped; run `ocd service set remote true` to retry";
    let line = loop {
        let found = files_under(&env.path("data"))
            .iter()
            .filter(|p| p.extension().is_some_and(|e| e == "log"))
            .flat_map(|p| std::fs::read_to_string(p).unwrap_or_default().lines().map(String::from).collect::<Vec<_>>())
            .find(|l| l.contains(expected));
        if let Some(line) = found {
            break line;
        }
        assert!(Instant::now() < deadline, "no fatal log line");
        tokio::time::sleep(Duration::from_millis(200)).await;
    };
    assert!(line.contains("level=ERROR"), "{line}");
    assert!(
        line.contains("cause=\"Cause([Fail(OpenTunnelClientError: Failed to connect tunnel: Bridge attach failed: bad_token (cause: Error: Bridge attach failed: bad_token))])\""),
        "{line}"
    );
    relay.options(|o| o.attach_error = None);

    // Turning remote access off forgets the route but remembers the consent.
    assert_eq!(service.run(&["service", "set", "remote", "false"]).status.code(), Some(0));
    assert_eq!(
        without_password(read_json(&env.path("config/opencode/service.json"))),
        json!({"remoteAccepted": true, "port": port})
    );
    assert_eq!(common::text(&service.run(&["service", "status"]).stdout), "stopped\n");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn service_set_remote_true_fails_when_the_tunnel_cannot_be_created() {
    let relay =
        FakeRelay::start(Options { create_error: Some((503, "ServiceUnavailableError".into())), ..Options::default() })
            .await;
    let env = common::Env::new();
    let service = Service { env: &env, api: relay.api.to_string() };
    let set = service.run(&["service", "set", "remote", "true"]);
    assert_eq!(set.status.code(), Some(1));
    assert_eq!(
        common::text(&set.stderr),
        "Setting up remote access; this can take a minute...\nOpenTunnelClientError: Failed to create tunnel: ServiceUnavailableError: Fake failure {\n  [cause]: ServiceUnavailableError: Fake failure\n}\n"
    );
    // Remote access stays off.
    assert!(!env.path("config/opencode/service.json").exists());
    assert!(!env.path("data/opentunnel/default/tunnel.json").exists());
}

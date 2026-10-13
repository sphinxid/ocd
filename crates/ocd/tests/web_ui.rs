//! `ocd serve` serves the web UI for non-API paths before auth (upstream `services/web-ui.ts`),
//! from `OCD_WEB_UI` here so the test does not depend on whether an archive was embedded.

mod common;

use std::io::{BufRead, BufReader};
use std::process::Stdio;

const INDEX: &str = "<html><script id=\"oc-theme-preload-script\">theme()</script><body>web ui</body></html>";

fn dist(env: &common::Env) -> std::path::PathBuf {
    let dist = env.path("dist");
    std::fs::create_dir_all(dist.join("_assets")).unwrap();
    std::fs::write(dist.join("index.html"), INDEX).unwrap();
    std::fs::write(dist.join("_assets/app-1234.js"), "console.log(1)").unwrap();
    std::fs::write(dist.join("site.webmanifest"), "{}").unwrap();
    dist
}

#[tokio::test]
async fn serve_answers_non_api_paths_with_the_web_ui() {
    let env = common::Env::new();
    let dist = dist(&env);
    let mut child = env
        .command(&["serve", "--port", "0"])
        .env("OCD_WEB_UI", &dist)
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let stdout = child.stdout.take().unwrap();
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        for line in BufReader::new(stdout).lines().map_while(Result::ok) {
            let _ = tx.send(line);
        }
    });
    let first = rx.recv_timeout(std::time::Duration::from_secs(60)).expect("server did not start");
    let url = first.strip_prefix("server listening on ").expect(&first).to_string();

    let result = async {
        let client = reqwest::Client::new();
        let r = client.get(format!("{url}/session/abc/deep")).send().await?;
        assert_eq!(r.status(), 200);
        assert_eq!(r.headers()["content-type"], "text/html");
        assert_eq!(r.headers()["cache-control"], "no-cache");
        assert!(r.headers()["content-security-policy"].to_str()?.contains("'sha256-"));
        assert!(r.headers().get("www-authenticate").is_none());
        assert_eq!(r.text().await?, INDEX);

        let r = client.get(format!("{url}/_assets/app-1234.js")).send().await?;
        assert_eq!(r.headers()["content-type"], "text/javascript");
        assert_eq!(r.headers()["cache-control"], "public, max-age=31536000, immutable");
        // the exact `mime-types` table (oc-tools), not a generic guess
        let r = client.get(format!("{url}/site.webmanifest")).send().await?;
        assert_eq!(r.headers()["content-type"], "application/manifest+json");

        let r = client.get(format!("{url}/_assets/old.js")).send().await?;
        assert_eq!(r.status(), 404);
        assert_eq!(r.headers()["cache-control"], "no-store");

        let r = client.get(format!("{url}/api/info")).send().await?;
        assert_eq!(r.status(), 401);
        anyhow::Ok(())
    }
    .await;
    let _ = child.kill();
    let _ = child.wait();
    result.unwrap();
}

#[test]
fn serve_rejects_an_unreadable_web_ui_override() {
    let env = common::Env::new();
    let missing = env.path("missing.bin");
    let out = env.command(&["serve", "--port", "0"]).env("OCD_WEB_UI", &missing).output().unwrap();
    assert_ne!(out.status.code(), Some(0));
    let stderr = common::text(&out.stderr);
    assert!(stderr.contains(&format!("OCD_WEB_UI={}", missing.display())), "{stderr}");
}

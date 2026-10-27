//! Runs the fake OpenTunnel service on 127.0.0.1 for manual comparisons.
//!
//! usage: fake_relay <info.json>
//! Writes `{"api", "public", "ca"}` (the CA PEM) to <info.json> and serves until killed. Control it with
//! `POST <api>/__fake/<drain|ping|text|close|drop|renew|clear|options|state>` and read `GET <api>/__fake/log`.

#[tokio::main]
async fn main() {
    let path = std::env::args().nth(1).expect("usage: fake_relay <info.json>");
    let relay = oc_tunnel::fake::FakeRelay::start(oc_tunnel::fake::Options::default()).await;
    let info = serde_json::json!({
        "api": relay.api.as_str().trim_end_matches('/'),
        "public": relay.public.to_string(),
        "ca": relay.ca_pem(),
    });
    std::fs::write(&path, serde_json::to_string_pretty(&info).unwrap()).expect("write info");
    println!("{}", info["api"].as_str().unwrap());
    std::future::pending::<()>().await;
}

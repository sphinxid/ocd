//! Connects routes through an OpenTunnel API and prints what happens as JSON lines, like the SDK probe used to
//! compare the two clients: every event as it happens (`onEvent`), then the hostname and public URLs once `connect`
//! resolves, and how the tunnel ended or the error `connect` failed with.
//!
//! usage: tunnel_probe <api-url> <data-home> <routes-json> <seconds> [options-json]
//! `routes-json` maps route names to `host:port` or `{"target", "proxyProtocol"}`. `options-json` may set `proxy`
//! (the `proxy` option), `onRouteConflict` (`"retry"` or `"fail"`) and `ensure` (only ensure the tunnel, printing
//! its progress). The identity is kept in `<data-home>/opentunnel/default`, as with `XDG_DATA_HOME=<data-home>`;
//! proxies come from the environment as usual.

use std::sync::Arc;

use serde_json::{Value, json};

fn describe(e: &oc_tunnel::Error) -> Value {
    let cause = match e {
        oc_tunnel::Error::Client { cause: Some(cause), .. } => match cause {
            oc_tunnel::Cause::Api(api) => api.message.clone(),
            other => other.to_string(),
        },
        oc_tunnel::Error::Storage { cause: Some(cause), .. } => cause.clone(),
        _ => String::new(),
    };
    let mut out = json!({"error": e.to_string(), "tag": e.tag()});
    if let Some(code) = e.code() {
        out["code"] = code.into();
    }
    out["cause"] = cause.into();
    let name = match e {
        oc_tunnel::Error::Client { cause: Some(oc_tunnel::Cause::Attach(_)), .. } => Some("OpenTunnelAttachError"),
        oc_tunnel::Error::Client { cause: Some(oc_tunnel::Cause::Api(api)), .. } => {
            Some(api.tag.as_deref().unwrap_or("HttpClientError"))
        }
        oc_tunnel::Error::Client { cause: Some(oc_tunnel::Cause::Message(_)), .. } => Some("Error"),
        _ => None,
    };
    if let Some(name) = name {
        out["causeName"] = name.into();
    }
    out
}

fn route(value: &Value) -> oc_tunnel::Route {
    match value {
        Value::String(target) => oc_tunnel::Route::new(target.clone()),
        other => oc_tunnel::Route {
            target: other["target"].as_str().unwrap_or("").to_string(),
            proxy_protocol: other["proxyProtocol"].as_str().map(String::from),
        },
    }
}

#[tokio::main]
async fn main() {
    let args: Vec<String> = std::env::args().collect();
    let (api, store, routes_json, seconds) = match args.as_slice() {
        [_, api, store, routes, seconds, ..] => (api, store, routes, seconds),
        _ => {
            eprintln!("usage: tunnel_probe <api-url> <data-home> <routes-json> <seconds> [options-json]");
            std::process::exit(2);
        }
    };
    let options: Value = args.get(5).and_then(|o| serde_json::from_str(o).ok()).unwrap_or(json!({}));
    let storage = oc_tunnel::Storage::at(std::path::Path::new(store).join("opentunnel"));
    let client_options = oc_tunnel::ClientOptions { proxy: options["proxy"].as_str().map(String::from), env: None };
    let client = match oc_tunnel::Client::with_options(url::Url::parse(api).unwrap(), storage, client_options) {
        Ok(client) => client,
        Err(e) => {
            println!("{}", json!({"layer": describe(&e)}));
            return;
        }
    };
    if options["ensure"].as_bool() == Some(true) {
        let on = |stage: oc_tunnel::Progress| {
            let name = format!("{stage:?}");
            let mut kebab = String::new();
            for (i, c) in name.chars().enumerate() {
                if c.is_ascii_uppercase() && i > 0 {
                    kebab.push('-');
                }
                kebab.push(c.to_ascii_lowercase());
            }
            println!("{}", json!({"progress": kebab}));
        };
        match client.ensure_with_progress("default", Some(&on)).await {
            Ok(identity) => println!("{}", json!({"ensured": identity.hostname})),
            Err(e) => println!("{}", describe(&e)),
        }
        return;
    }
    let routes_value: Value = serde_json::from_str(routes_json).unwrap_or(json!({}));
    let routes: oc_tunnel::Routes = routes_value
        .as_object()
        .map(|o| o.iter().map(|(name, value)| (name.clone(), route(value))).collect())
        .unwrap_or_default();
    let on_event = Arc::new(|e: oc_tunnel::Event| println!("{}", json!({"event": e})));
    let connect = oc_tunnel::ConnectOptions {
        routes,
        on_route_conflict: if options["onRouteConflict"] == "fail" {
            oc_tunnel::OnRouteConflict::Fail
        } else {
            oc_tunnel::OnRouteConflict::Retry
        },
        on_event: Some(on_event),
    };
    match client.connect_with("default", connect).await {
        Ok(connection) => {
            println!("{}", json!({"hostname": connection.tunnel.hostname, "urls": connection.urls()}));
            let wait = std::time::Duration::from_secs_f64(seconds.parse().unwrap_or(5.0));
            tokio::select! {
                result = connection.closed() => {
                    let closed = match result {
                        Ok(()) => json!({"closed": null}),
                        Err(e) => json!({"closed": describe(&e)}),
                    };
                    println!("{closed}");
                }
                _ = tokio::time::sleep(wait) => connection.close().await,
            }
        }
        Err(e) => {
            println!("{}", describe(&e));
            eprintln!("fatal: {}", e.is_fatal());
        }
    }
}

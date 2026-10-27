//! Connects one route through an OpenTunnel API and prints the tunnel's events as JSON lines, like the SDK probe
//! used to compare the two clients (events are shown once `connect` resolves, as the SDK's event stream is).
//!
//! usage: tunnel_probe <api-url> <data-home> <route> <target> <seconds>
//! The identity is kept in `<data-home>/opentunnel/default`, as with `XDG_DATA_HOME=<data-home>`.

use std::sync::Arc;

use parking_lot::Mutex;

#[tokio::main]
async fn main() {
    let args: Vec<String> = std::env::args().collect();
    let [_, api, store, route, target, seconds] = args.as_slice() else {
        eprintln!("usage: tunnel_probe <api-url> <data-home> <route> <target> <seconds>");
        std::process::exit(2);
    };
    let storage = oc_tunnel::Storage::at(std::path::Path::new(store).join("opentunnel"));
    let client = oc_tunnel::Client::new(url::Url::parse(api).unwrap(), storage);
    let routes = [(route.clone(), target.clone())].into_iter().collect();
    // Buffered until connect resolves, then printed as they come.
    let buffer: Arc<Mutex<Option<Vec<String>>>> = Arc::new(Mutex::new(Some(Vec::new())));
    let sink = buffer.clone();
    let on_event = Arc::new(move |e: oc_tunnel::Event| {
        let line = serde_json::to_string(&e).unwrap();
        match sink.lock().as_mut() {
            Some(pending) => pending.push(line),
            None => println!("{line}"),
        }
    });
    match client.connect("default", routes, Some(on_event)).await {
        Ok(connection) => {
            println!("{}", serde_json::json!({"url": format!("https://{route}.{}", connection.tunnel.hostname)}));
            for line in buffer.lock().take().unwrap_or_default() {
                println!("{line}");
            }
            let wait = std::time::Duration::from_secs_f64(seconds.parse().unwrap_or(5.0));
            tokio::select! {
                result = connection.closed() => {
                    let closed = match result {
                        Ok(()) => serde_json::json!({"closed": null}),
                        Err(e) => serde_json::json!({"closed": e.to_string(), "cause": cause(&e)}),
                    };
                    println!("{closed}");
                }
                _ = tokio::time::sleep(wait) => connection.close().await,
            }
        }
        Err(e) => {
            let mut out = serde_json::json!({"error": e.to_string(), "tag": e.tag(), "cause": cause(&e)});
            match &e {
                oc_tunnel::Error::Client { cause: Some(oc_tunnel::Cause::Attach(_)), .. } => {
                    out["causeName"] = "OpenTunnelAttachError".into();
                }
                oc_tunnel::Error::Client { cause: Some(oc_tunnel::Cause::Api(api)), .. } => {
                    if let Some(tag) = &api.tag {
                        out["causeName"] = tag.clone().into();
                    }
                }
                _ => {}
            }
            println!("{out}");
            eprintln!("fatal: {}", e.is_fatal());
        }
    }
}

fn cause(e: &oc_tunnel::Error) -> String {
    match e {
        oc_tunnel::Error::Client { cause: Some(cause), .. } => match cause {
            oc_tunnel::Cause::Api(api) => api.message.clone(),
            other => other.to_string(),
        },
        oc_tunnel::Error::Storage { cause: Some(cause), .. } => cause.clone(),
        _ => String::new(),
    }
}

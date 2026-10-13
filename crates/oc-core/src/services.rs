//! Default tool services (formatter, ripgrep, shell, websearch) wired to core storage.

use std::sync::Arc;

use async_trait::async_trait;
use serde_json::Value;

use crate::app::App;

/// `WebSearchStore` over the `kv` table and the integration credentials.
pub struct WebSearchKv {
    pub app: Arc<App>,
}

#[async_trait]
impl oc_tools::websearch::WebSearchStore for WebSearchKv {
    async fn kv_get(&self, key: &str) -> Option<Value> {
        self.app.db.kv_get(key).ok().flatten()
    }
    async fn kv_set(&self, key: &str, value: Value) {
        let _ = self.app.db.kv_set(key, &value);
    }
    async fn kv_remove(&self, key: &str) {
        let _ = self.app.db.kv_remove(key);
    }
    async fn api_key(&self, integration_id: &str) -> Option<String> {
        let row: Option<String> = self
            .app
            .db
            .with(|c| {
                use rusqlite::OptionalExtension;
                c.query_row(
                    "SELECT value FROM credential WHERE integration_id = ? ORDER BY active DESC, time_created DESC, id DESC LIMIT 1",
                    [integration_id],
                    |r| r.get(0),
                )
                .optional()
            })
            .ok()
            .flatten();
        let v: Value = serde_json::from_str(&row?).ok()?;
        (v["type"] == "key").then(|| v["key"].as_str().map(str::to_string)).flatten()
    }
}

pub fn default_services(app: &Arc<App>) -> oc_tools::tools::Services {
    let http = reqwest::Client::new();
    let bus = app.bus.clone();
    // Upstream's Shell service is location-scoped, so its events carry the location of the shell's owner:
    // the session's location for tool and `!` shells, else the working directory. Exits reuse the
    // location recorded at creation.
    let locations: Arc<parking_lot::Mutex<std::collections::HashMap<String, oc_schema::types::LocationRef>>> =
        Arc::default();
    let sink: oc_tools::shell::service::EventSink = Arc::new(move |ev| {
        use oc_tools::shell::service::ShellEvent as E;
        let location = match &ev {
            E::Created { info } => {
                let session = info.metadata.get("sessionID").and_then(Value::as_str);
                let location = session
                    .and_then(|s| bus.db().session_get(s).ok().flatten())
                    .map(|s| s.location)
                    .unwrap_or_else(|| oc_schema::types::LocationRef { directory: info.cwd.clone(), workspace_id: None });
                locations.lock().insert(info.id.clone(), location.clone());
                Some(location)
            }
            E::Exited { id, .. } => locations.lock().get(id).cloned(),
            E::Deleted { id } => locations.lock().remove(id),
        };
        let (kind, data) = shell_event(&ev);
        bus.publish(kind, data, location);
    });
    oc_tools::tools::Services {
        formatter: Arc::new(oc_tools::formatter::Formatter::new()),
        ripgrep: Arc::new(oc_tools::ripgrep::Ripgrep::new(app.global.bin.clone(), http.clone())),
        shell: oc_tools::shell::service::ShellService::new(app.global.data.clone(), Some(sink)),
        websearch: Arc::new(oc_tools::websearch::WebSearch::new(Arc::new(WebSearchKv { app: app.clone() }), http.clone())),
        http,
    }
}

fn shell_event(ev: &oc_tools::shell::service::ShellEvent) -> (&'static str, Value) {
    use oc_tools::shell::service::ShellEvent as E;
    match ev {
        // Upstream announces the info captured before spawning, so `pid` is never part of the event
        // (core/src/shell.ts publishes `info`, while the stored command gets `pid`).
        E::Created { info } => {
            let mut info = serde_json::to_value(info).unwrap_or(Value::Null);
            if let Some(o) = info.as_object_mut() {
                o.remove("pid");
            }
            ("shell.created", serde_json::json!({"info": info}))
        }
        E::Exited { id, exit, status } => ("shell.exited", serde_json::json!({"id": id, "exit": exit, "status": status})),
        E::Deleted { id } => ("shell.deleted", serde_json::json!({"id": id})),
    }
}

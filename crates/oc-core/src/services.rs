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
    let sink: oc_tools::shell::service::EventSink = Arc::new(move |ev| {
        let (kind, data) = shell_event(&ev);
        bus.publish(kind, data, None);
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
        E::Created { info } => ("shell.created", serde_json::json!({"info": info})),
        E::Exited { id, exit, status } => ("shell.exited", serde_json::json!({"id": id, "exit": exit, "status": status})),
        E::Deleted { id } => ("shell.deleted", serde_json::json!({"id": id})),
    }
}

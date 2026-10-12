//! Plan-mode reminders (`core/plugin/plan.ts`): admit a synthetic reminder when a session starts
//! in, or switches to/from, the `plan` agent.

use std::sync::Arc;

use crate::app::App;

pub fn spawn(app: Arc<App>) {
    let mut rx = app.bus.subscribe();
    tokio::spawn(async move {
        loop {
            let ev = match rx.recv().await {
                Ok(ev) => ev,
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                Err(_) => break,
            };
            let data = &ev.data;
            let session = data.get("sessionID").and_then(|v| v.as_str()).unwrap_or_default().to_string();
            let text = match ev.kind.as_str() {
                "session.created" if data.get("agent").and_then(|v| v.as_str()) == Some("plan") => {
                    Some(crate::agent::plan_enter(&app.global.home))
                }
                "session.agent.selected" => {
                    let agent = data.get("agent").and_then(|v| v.as_str());
                    let previous = data.get("previous").and_then(|v| v.as_str());
                    if agent == previous {
                        None
                    } else if agent == Some("plan") {
                        Some(crate::agent::plan_enter(&app.global.home))
                    } else if previous == Some("plan") {
                        Some(crate::agent::PLAN_LEAVE.to_string())
                    } else {
                        None
                    }
                }
                _ => None,
            };
            if let Some(text) = text {
                // `resume: false`: delivered with the next prompt.
                let _ = app.sessions.synthetic(&session, None, text, None, None, None).await;
            }
        }
    });
}

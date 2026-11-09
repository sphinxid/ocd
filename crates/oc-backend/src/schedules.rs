//! `schedule.*` in the `server.experimental` group (`server/src/handlers/schedule.ts`), the prompt path the
//! schedule loop admits wakes through, and the loop itself (`server/src/process.ts`: every server process runs it).

use std::sync::{Arc, Weak};

use futures::future::BoxFuture;
use oc_core::schedule::{CreateInput, ScheduleError, Schedules, StepError};
use oc_server::ApiError;
use oc_server::generated as api;
use serde_json::{Map, Value};

use crate::engine::Engine;
use crate::util::{defect, from_json};

/// `sessions.prompt` for a scheduled wake: the session API's prompt (plugin activation, the `session.prompt` hook,
/// admission and the wake), delivered as a steer.
struct EnginePrompter(Weak<Engine>);

impl oc_core::schedule::Prompter for EnginePrompter {
    fn prompt(&self, session_id: String, id: String, text: String, metadata: Map<String, Value>) -> BoxFuture<'static, Result<(), StepError>> {
        let engine = self.0.upgrade();
        Box::pin(async move {
            let engine = engine.ok_or_else(|| StepError::Other("The server is shutting down".into()))?;
            let session = engine.session(&session_id).map_err(|e| StepError::Other(e.to_string()))?;
            if !std::path::Path::new(&session.location.directory).is_dir() {
                return Err(StepError::DirectoryNotFound(session.location.directory));
            }
            let body = api::SessionPromptBody {
                id: Some(id),
                text,
                files: None,
                agents: None,
                skills: None,
                metadata: Some(from_json(Value::Object(metadata)).map_err(|e| StepError::Other(e.to_string()))?),
                delivery: Some(from_json(Value::String("steer".into())).map_err(|e| StepError::Other(e.to_string()))?),
                resume: None,
            };
            engine.prompt(&session_id, body).await.map(|_| ()).map_err(|e| StepError::Other(e.to_string()))
        })
    }

    fn wake(&self, session_id: &str) {
        if let Some(engine) = self.0.upgrade() {
            engine.wake(session_id, oc_core::inbox::Scope::Input);
        }
    }
}

/// Connects the schedule service to this server, and runs the loop in the background (the lease makes one process
/// per database fire).
pub fn start(engine: &Arc<Engine>) {
    connect(engine);
    tokio::spawn(engine.schedules.clone().run());
}

/// Routes the schedule service's wakes through this server's session API (without starting the loop).
pub fn connect(engine: &Arc<Engine>) {
    engine.schedules.set_prompter(Arc::new(EnginePrompter(Arc::downgrade(engine))));
}

fn disabled() -> ApiError {
    ApiError::forbidden(oc_core::schedule::DISABLED)
}

fn missing(id: &str) -> ApiError {
    ApiError::schedule_not_found(id)
}

fn info(value: Value) -> Result<api::ScheduleInfo, ApiError> {
    from_json(value)
}

impl Engine {
    fn schedule_service(&self) -> Result<Arc<Schedules>, ApiError> {
        Ok(self.schedules.clone())
    }

    /// `allowed(sessionID)`: the session must exist and have `experimental.schedules` on where it lives.
    async fn schedule_allowed(&self, schedules: &Schedules, session_id: &str) -> Result<(), ApiError> {
        match schedules.enabled(session_id).await {
            Ok(true) => Ok(()),
            Ok(false) => Err(disabled()),
            Err(_) => Err(ApiError::session_not_found(session_id)),
        }
    }

    /// `find(id)`: a schedule whose session is gone, or whose location has the flag off, reads as missing or
    /// forbidden like the rest.
    async fn schedule_find(&self, schedules: &Schedules, id: &str) -> Result<Value, ApiError> {
        let info = schedules.get(id).map_err(|_| missing(id))?;
        let Some(session) = info.get("sessionID").and_then(Value::as_str) else { return Err(missing(id)) };
        match schedules.enabled(session).await {
            Ok(true) => Ok(info),
            Ok(false) => Err(disabled()),
            Err(_) => Err(missing(id)),
        }
    }

    pub async fn schedule_create(&self, payload: &api::ScheduleCreatePayload) -> Result<api::ScheduleInfo, ApiError> {
        let schedules = self.schedule_service()?;
        let request = serde_json::to_value(payload).map_err(defect)?;
        let input = CreateInput::from_request(&payload.session_id, &request);
        // Stateless HTTP callers have no guaranteed UI listener to answer an interactive ask, so settle asks
        // unattended.
        let created = oc_core::permission::unattended(Vec::new(), schedules.create(input)).await;
        match created {
            Ok(value) => info(value),
            Err(ScheduleError::Invalid(message)) => Err(ApiError::invalid_request(message).with_kind("schedule")),
            Err(ScheduleError::Disabled) => Err(disabled()),
            Err(ScheduleError::Blocked(message)) | Err(ScheduleError::Corrected(message)) => Err(ApiError::forbidden(message)),
            Err(ScheduleError::SessionNotFound(id)) => Err(ApiError::session_not_found(&id)),
            Err(other) => Err(defect(other.message())),
        }
    }

    pub async fn schedule_list(&self, session_id: &str) -> Result<Vec<api::ScheduleInfo>, ApiError> {
        let schedules = self.schedule_service()?;
        self.schedule_allowed(&schedules, session_id).await?;
        schedules.list(Some(session_id)).into_iter().map(info).collect()
    }

    pub async fn schedule_get(&self, id: &str) -> Result<api::ScheduleInfo, ApiError> {
        let schedules = self.schedule_service()?;
        info(self.schedule_find(&schedules, id).await?)
    }

    pub async fn schedule_pause(&self, id: &str) -> Result<api::ScheduleInfo, ApiError> {
        let schedules = self.schedule_service()?;
        self.schedule_find(&schedules, id).await?;
        info(schedules.pause(id).map_err(|_| missing(id))?)
    }

    pub async fn schedule_resume(&self, id: &str) -> Result<api::ScheduleInfo, ApiError> {
        let schedules = self.schedule_service()?;
        self.schedule_find(&schedules, id).await?;
        info(schedules.resume(id).map_err(|_| missing(id))?)
    }

    pub async fn schedule_delete(&self, id: &str) -> Result<(), ApiError> {
        let schedules = self.schedule_service()?;
        self.schedule_find(&schedules, id).await?;
        schedules.remove(id).map_err(|_| missing(id))
    }
}

//! `impl oc_server::Backend for Engine` — one method per operationId.

use std::sync::Arc;

use async_trait::async_trait;
use futures::StreamExt;
use oc_server::generated::*;
use oc_server::{
    ApiError, Backend, EventSubscribeRequest, EventSubscription, FsFile, FsReadRequest, RequestContext, SessionListRequest,
    SessionLogStream, SessionMessageListRequest,
};
use serde_json::{Value, json};
use tokio::sync::broadcast::error::RecvError;

use crate::engine::Engine;
use oc_core::form::FormError;
use crate::sessions::{active_map, session_error};
use crate::store;
use crate::util::{conv, defect, from_json};

/// Not available until oc-core grows the service (documented in README).
fn unavailable(service: &str) -> ApiError {
    ApiError::service_unavailable(format!("{service} is not available in this server yet"), Some(service.to_string()))
}

#[async_trait]
impl Backend for Engine {
    // ---- location / agent ---------------------------------------------------------------------

    async fn location_get(&self, ctx: &RequestContext, _req: LocationGetRequest) -> Result<LocationPublicInfo, ApiError> {
        let loc = self.locations.get(&ctx.location.directory).await?;
        Ok(LocationPublicInfo {
            directory: loc.key.clone(),
            project: LocationPublicInfoProject {
                id: loc.project_id().to_string(),
                directory: loc.project_directory().display().to_string(),
                canonical: loc.location.project.canonical.display().to_string(),
            },
        })
    }

    async fn location_reload(&self, _ctx: &RequestContext, _req: LocationReloadRequest) -> Result<(), ApiError> {
        self.locations.reload().await
    }

    async fn agent_list(&self, ctx: &RequestContext, _req: AgentListRequest) -> Result<Vec<AgentInfo>, ApiError> {
        let loc = self.locations.get(&ctx.location.directory).await?;
        self.agents(&loc).await.list().into_iter().map(conv).collect()
    }

    async fn agent_get(&self, ctx: &RequestContext, req: AgentGetRequest) -> Result<AgentInfo, ApiError> {
        let loc = self.locations.get(&ctx.location.directory).await?;
        match self.agents(&loc).await.items.get(&req.agent_id) {
            Some(a) => conv(a),
            None => Err(ApiError::agent_not_found(req.agent_id)),
        }
    }

    // ---- plugin -------------------------------------------------------------------------------

    async fn plugin_list(&self, ctx: &RequestContext, _req: PluginListRequest) -> Result<Vec<PluginInfo>, ApiError> {
        let loc = self.locations.get(&ctx.location.directory).await?;
        self.app.plugins.inventory(&loc.key).into_iter().map(from_json).collect()
    }

    async fn plugin_check(&self, ctx: &RequestContext, req: PluginCheckRequest) -> Result<Vec<PluginInfo>, ApiError> {
        let loc = self.locations.get(&ctx.location.directory).await?;
        let inventory = self.app.plugins.inventory(&loc.key);
        let targets = package_targets(&inventory);
        if let Some(t) = req.body.target.as_ref().filter(|t| !targets.contains(t)) {
            return Err(ApiError::invalid_request(format!("Plugin package is not in the current server inventory: {t}")).with_field("target"));
        }
        let selected: Vec<String> = targets.into_iter().filter(|t| req.body.target.as_ref().is_none_or(|s| s == t)).collect();
        let mut outdated = std::collections::HashMap::new();
        for target in &selected {
            outdated.insert(target.clone(), self.plugin_outdated(target).await);
        }
        inventory
            .into_iter()
            .map(|mut plugin| {
                let target = plugin.pointer("/source/target").and_then(Value::as_str).map(String::from);
                if plugin.pointer("/source/type").and_then(Value::as_str) == Some("package")
                    && let Some(target) = target
                    && let Some(flag) = outdated.get(&target)
                {
                    let source = plugin["source"].clone();
                    let mut next = json!({ "type": "package", "target": target });
                    if let Some(v) = source.get("version") {
                        next["version"] = v.clone();
                    }
                    if *flag {
                        next["outdated"] = json!(true);
                    }
                    if source.get("updating").and_then(Value::as_bool) == Some(true) {
                        next["updating"] = json!(true);
                    }
                    plugin["source"] = next;
                }
                from_json(plugin)
            })
            .collect()
    }

    async fn plugin_update(&self, ctx: &RequestContext, req: PluginUpdateRequest) -> Result<(), ApiError> {
        let loc = self.locations.get(&ctx.location.directory).await?;
        let targets = package_targets(&self.app.plugins.inventory(&loc.key));
        let unknown: Vec<String> = req.body.targets.iter().filter(|t| !targets.contains(t)).cloned().collect();
        if !unknown.is_empty() {
            return Err(ApiError::invalid_request(format!("Plugin packages are not in the current server inventory: {}", unknown.join(", ")))
                .with_field("targets"));
        }
        let mut failures = vec![];
        for target in &req.body.targets {
            if let Err(e) = self.plugin_install_update(target).await {
                failures.push(format!("{target}: {e}"));
            }
        }
        if !req.body.targets.is_empty() {
            self.app.plugins.load(loc.location.clone(), true).await;
        }
        if !failures.is_empty() {
            return Err(ApiError::service_unavailable(format!("Failed to update plugin packages: {}", failures.join("; ")), Some("plugin".into())));
        }
        Ok(())
    }

    // ---- session -----------------------------------------------------------------------------

    async fn session_list(&self, _ctx: &RequestContext, req: SessionListRequest) -> Result<Vec<SessionInfo>, ApiError> {
        let rows = store::list_sessions(&self.app.db, &req.query, Some(req.limit)).map_err(defect)?;
        rows.iter().map(|s| self.session_api(s)).collect()
    }

    async fn session_create(&self, ctx: &RequestContext, req: SessionCreateRequest) -> Result<SessionInfo, ApiError> {
        self.create_session(&ctx.server.cwd, req.body).await
    }

    async fn session_active(&self, _ctx: &RequestContext, _req: SessionActiveRequest) -> Result<JsonObject, ApiError> {
        let ids = self.app.coordinator.get().map(|c| c.active()).unwrap_or_default();
        conv(active_map(ids))
    }

    async fn session_get(&self, _ctx: &RequestContext, req: SessionGetRequest) -> Result<SessionInfo, ApiError> {
        let s = self.session(&req.session_id)?;
        self.session_api(&s)
    }

    async fn session_remove(&self, _ctx: &RequestContext, req: SessionRemoveRequest) -> Result<(), ApiError> {
        self.remove_session(&req.session_id).await
    }

    async fn session_update(&self, _ctx: &RequestContext, req: SessionUpdateRequest) -> Result<(), ApiError> {
        self.session_scope(&req.session_id).await?;
        self.update_session(&req.session_id, req.body).await
    }

    async fn session_fork(&self, _ctx: &RequestContext, req: SessionForkRequest) -> Result<SessionInfo, ApiError> {
        self.session_scope(&req.session_id).await?;
        self.fork_session(&req.session_id, req.body.before)
    }

    async fn session_switch_agent(&self, _ctx: &RequestContext, req: SessionSwitchAgentRequest) -> Result<(), ApiError> {
        self.session_scope(&req.session_id).await?;
        self.app.sessions.switch_agent(&req.session_id, &req.body.agent).map_err(session_error)
    }

    async fn session_switch_model(&self, _ctx: &RequestContext, req: SessionSwitchModelRequest) -> Result<(), ApiError> {
        self.session_scope(&req.session_id).await?;
        self.app.sessions.switch_model(&req.session_id, conv(req.body.model)?).map_err(session_error)
    }

    async fn session_move(&self, _ctx: &RequestContext, req: SessionMoveRequest) -> Result<(), ApiError> {
        self.move_session(&req.session_id, req.body).await
    }

    async fn session_prompt(&self, _ctx: &RequestContext, req: SessionPromptRequest) -> Result<SessionInboxUser, ApiError> {
        self.prompt(&req.session_id, req.body).await
    }

    async fn session_command(&self, _ctx: &RequestContext, req: SessionCommandRequest) -> Result<(), ApiError> {
        self.command(&req.session_id, req.body).await
    }

    async fn experimental_session_skill(&self, _ctx: &RequestContext, req: ExperimentalSessionSkillRequest) -> Result<(), ApiError> {
        self.activate_skill(&req.session_id, &req.body.id, req.body.resume).await
    }

    async fn session_synthetic(&self, _ctx: &RequestContext, req: SessionSyntheticRequest) -> Result<SessionInboxSynthetic, ApiError> {
        self.synthetic(&req.session_id, req.body).await
    }

    async fn session_shell(&self, _ctx: &RequestContext, req: SessionShellRequest) -> Result<(), ApiError> {
        self.session_scope(&req.session_id).await?;
        let Some(runner) = &self.runner else {
            return Err(ApiError::defect("session shell requires the session runner"));
        };
        runner
            .session_shell(&req.session_id, req.body.id, &req.body.command)
            .await
            .map_err(|e| match self.session(&req.session_id) {
                Err(nf) => nf,
                Ok(_) => defect(format!("{e:#}")),
            })
    }

    async fn session_compact(&self, _ctx: &RequestContext, req: SessionCompactRequest) -> Result<SessionInboxCompaction, ApiError> {
        self.compact(&req.session_id, req.body).await
    }

    async fn experimental_session_wait(&self, _ctx: &RequestContext, req: ExperimentalSessionWaitRequest) -> Result<(), ApiError> {
        self.session_scope(&req.session_id).await?;
        if let Some(c) = self.app.coordinator.get() {
            c.await_idle(&req.session_id).await;
        }
        Ok(())
    }

    async fn session_revert_stage(&self, _ctx: &RequestContext, req: SessionRevertStageRequest) -> Result<SessionRevert, ApiError> {
        self.revert_stage(&req.session_id, &req.body.message_id, req.body.files).await
    }

    async fn session_revert_clear(&self, _ctx: &RequestContext, req: SessionRevertClearRequest) -> Result<(), ApiError> {
        self.revert_clear(&req.session_id).await
    }

    async fn session_revert_commit(&self, _ctx: &RequestContext, req: SessionRevertCommitRequest) -> Result<(), ApiError> {
        self.revert_commit(&req.session_id).await
    }

    async fn session_context(&self, _ctx: &RequestContext, req: SessionContextRequest) -> Result<Vec<SessionMessageInfo>, ApiError> {
        self.context(&req.session_id)
    }

    async fn session_diff(&self, _ctx: &RequestContext, req: SessionDiffRequest) -> Result<Vec<FileDiffInfo>, ApiError> {
        self.diff(&req.session_id, req.from, req.to, req.context).await
    }

    async fn session_inbox_list(&self, _ctx: &RequestContext, req: SessionInboxListRequest) -> Result<Vec<SessionInboxInfo>, ApiError> {
        self.inbox_list(&req.session_id)
    }

    async fn session_inbox_cancel(&self, _ctx: &RequestContext, req: SessionInboxCancelRequest) -> Result<(), ApiError> {
        self.inbox_cancel(&req.session_id, &req.inbox_id).await
    }

    async fn session_inbox_update(&self, _ctx: &RequestContext, req: SessionInboxUpdateRequest) -> Result<(), ApiError> {
        self.inbox_update(&req.session_id, &req.inbox_id, req.body.delivery).await
    }

    async fn experimental_session_instructions_entry_list(
        &self,
        _ctx: &RequestContext,
        req: ExperimentalSessionInstructionsEntryListRequest,
    ) -> Result<Vec<InstructionEntryInfo>, ApiError> {
        self.instruction_list(&req.session_id).await
    }

    async fn experimental_session_instructions_entry_put(
        &self,
        _ctx: &RequestContext,
        req: ExperimentalSessionInstructionsEntryPutRequest,
    ) -> Result<(), ApiError> {
        let value = serde_json::to_value(&req.body.value).map_err(defect)?;
        self.instruction_put(&req.session_id, &req.key, value).await
    }

    async fn experimental_session_instructions_entry_remove(
        &self,
        _ctx: &RequestContext,
        req: ExperimentalSessionInstructionsEntryRemoveRequest,
    ) -> Result<(), ApiError> {
        self.instruction_remove(&req.session_id, &req.key).await
    }

    async fn session_generate(&self, _ctx: &RequestContext, req: SessionGenerateRequest) -> Result<SessionGenerateResponse, ApiError> {
        self.session_scope(&req.session_id).await?;
        let Some(runner) = &self.runner else {
            return Err(ApiError::service_unavailable("Session generation is not available", Some("session generation".into())));
        };
        match runner.session_generate(&req.session_id, &req.body.prompt).await {
            Ok(text) => Ok(SessionGenerateResponse { data: SessionGenerateResponseData { text } }),
            Err(e) => match self.session(&req.session_id) {
                Err(nf) => Err(nf),
                Ok(_) => Err(ApiError::service_unavailable(e.to_string(), Some("session generation".into()))),
            },
        }
    }

    async fn session_log(&self, _ctx: &RequestContext, req: SessionLogRequest) -> Result<SessionLogStream, ApiError> {
        self.session(&req.session_id)?;
        let id = req.session_id.clone();
        let db = self.app.db.clone();
        // Subscribe before the historical read so commits during replay are not missed.
        let rx = req.follow.unwrap_or(false).then(|| self.app.bus.subscribe());
        let mut sequence = req.after.unwrap_or(-1);
        let target = store::latest_seq(&db, &id).map_err(defect)?;
        let mut replay: Vec<Value> = Vec::new();
        loop {
            let page = store::read_events(&db, &id, sequence, target).map_err(defect)?;
            let Some(last) = page.last().map(|p| p.0) else { break };
            sequence = last;
            replay.extend(page.into_iter().map(|p| p.1));
            if last >= target {
                break;
            }
        }
        let mut marker = json!({"type": "log.synced", "aggregateID": id});
        if target >= 0 {
            marker["seq"] = json!(target);
        }
        replay.push(marker);
        let head = futures::stream::iter(replay.into_iter().map(Ok));
        let Some(rx) = rx else { return Ok(head.boxed()) };
        let live = futures::stream::unfold((rx, sequence, db, id), |(mut rx, mut seq, db, id)| async move {
            loop {
                match rx.recv().await {
                    Ok(env) if env.durable.as_ref().is_some_and(|d| d.aggregate_id == id) => {
                        let target = store::latest_seq(&db, &id).unwrap_or(seq);
                        if target <= seq {
                            continue;
                        }
                        let mut items = Vec::new();
                        while let Ok(page) = store::read_events(&db, &id, seq, target) {
                            let Some(last) = page.last().map(|p| p.0) else { break };
                            seq = last;
                            items.extend(page.into_iter().map(|p| Ok(p.1)));
                            if last >= target {
                                break;
                            }
                        }
                        if items.is_empty() {
                            continue;
                        }
                        return Some((futures::stream::iter(items), (rx, seq, db, id)));
                    }
                    Ok(_) | Err(RecvError::Lagged(_)) => continue,
                    Err(RecvError::Closed) => return None,
                }
            }
        })
        .flatten();
        Ok(head.chain(live).boxed())
    }

    async fn session_interrupt(&self, _ctx: &RequestContext, req: SessionInterruptRequest) -> Result<SessionInterruptResponse, ApiError> {
        let interrupted = self.interrupt(&req.session_id, req.resume.unwrap_or(false)).await?;
        Ok(SessionInterruptResponse { interrupted })
    }

    async fn session_background(&self, _ctx: &RequestContext, req: SessionBackgroundRequest) -> Result<(), ApiError> {
        self.session_scope(&req.session_id).await?;
        self.background(&req.session_id).await
    }

    async fn session_message_get(&self, _ctx: &RequestContext, req: SessionMessageGetRequest) -> Result<SessionMessageInfo, ApiError> {
        self.message_get(&req.session_id, &req.message_id)
    }

    async fn session_message_list(&self, _ctx: &RequestContext, req: SessionMessageListRequest) -> Result<Vec<SessionMessageInfo>, ApiError> {
        self.message_list(req)
    }

    async fn session_environment(&self, _ctx: &RequestContext, req: SessionEnvironmentRequest) -> Result<(), ApiError> {
        self.set_environment(&req.session_id, req.body.variables)
    }

    async fn session_view(&self, _ctx: &RequestContext, req: SessionViewRequest) -> Result<(), ApiError> {
        self.app.sessions.view(&req.session_id, req.body.idle as u64).map_err(session_error)
    }

    // ---- forms -----------------------------------------------------------------------------

    async fn session_form_list(&self, ctx: &RequestContext, req: SessionFormListRequest) -> Result<Vec<FormInfo>, ApiError> {
        self.form_scope(ctx, &req.session_id).await?;
        self.app.forms.list(Some(&req.session_id)).into_iter().map(from_json).collect()
    }

    async fn session_form_create(&self, ctx: &RequestContext, req: SessionFormCreateRequest) -> Result<FormInfo, ApiError> {
        self.form_scope(ctx, &req.session_id).await?;
        let mut input = serde_json::to_value(&req.body).map_err(defect)?;
        input["sessionID"] = json!(req.session_id);
        match self.app.forms.create(&input) {
            Ok(form) => from_json(form),
            Err(FormError::AlreadyExists(id)) => Err(ApiError::conflict(format!("Form already exists: {id}"), Some(id))),
            Err(FormError::InvalidForm(m)) => Err(ApiError::invalid_request(m).with_field("fields")),
            Err(e) => Err(form_error(e)),
        }
    }

    async fn session_form_get(&self, ctx: &RequestContext, req: SessionFormGetRequest) -> Result<FormDetail, ApiError> {
        self.form_scope(ctx, &req.session_id).await?;
        let mut form = self.owned_form(&req.session_id, &req.form_id)?;
        let state = self.app.forms.state(&req.form_id).map_err(|_| ApiError::form_not_found(&req.form_id))?;
        form["state"] = state.to_json();
        from_json(form)
    }

    async fn session_form_cancel(&self, ctx: &RequestContext, req: SessionFormCancelRequest) -> Result<(), ApiError> {
        self.form_scope(ctx, &req.session_id).await?;
        self.owned_form(&req.session_id, &req.form_id)?;
        self.app.forms.cancel(&req.form_id, req.message).map_err(form_error)
    }

    async fn session_form_reply(&self, ctx: &RequestContext, req: SessionFormReplyRequest) -> Result<(), ApiError> {
        self.form_scope(ctx, &req.session_id).await?;
        self.owned_form(&req.session_id, &req.form_id)?;
        let answer = serde_json::to_value(&req.body.answer).map_err(defect)?;
        let answer = answer.as_object().cloned().unwrap_or_default();
        self.app.forms.reply(&req.form_id, answer).map_err(form_error)
    }

    async fn form_list(&self, ctx: &RequestContext, _req: FormListRequest) -> Result<Vec<FormInfo>, ApiError> {
        let loc = self.locations.get(&ctx.location.directory).await?;
        // Form services are per location upstream: keep forms of sessions located here (and the
        // `global` MCP-elicitation forms).
        let here = self.locations.canonical_of(&loc.key);
        self.app
            .forms
            .list(None)
            .into_iter()
            .filter(|f| {
                let sid = f.get("sessionID").and_then(Value::as_str).unwrap_or_default();
                sid == "global"
                    || self
                        .app
                        .db
                        .session_get(sid)
                        .ok()
                        .flatten()
                        .is_some_and(|s| self.locations.canonical_of(&s.location.directory) == here)
            })
            .map(from_json)
            .collect()
    }

    // ---- model / provider / generate ----------------------------------------------------------

    async fn model_list(&self, ctx: &RequestContext, _req: ModelListRequest) -> Result<Vec<ModelInfo>, ApiError> {
        let loc = self.locations.get(&ctx.location.directory).await?;
        let catalog = self.catalog(&loc).await?;
        catalog.available_models().into_iter().map(conv).collect()
    }

    async fn model_default(&self, ctx: &RequestContext, _req: ModelDefaultRequest) -> Result<Option<ModelInfo>, ApiError> {
        let loc = self.locations.get(&ctx.location.directory).await?;
        let catalog = self.catalog(&loc).await?;
        catalog.default_model().map(conv).transpose()
    }

    async fn experimental_generate_text(&self, _ctx: &RequestContext, req: ExperimentalGenerateTextRequest) -> Result<GenerateTextResponse, ApiError> {
        // Runs at the location whose directory is the global config directory.
        let directory = self.app.global.config.display().to_string();
        self.locations.get(&directory).await?;
        let Some(runner) = &self.runner else { return Err(unavailable("generate")) };
        let model = req.body.model.map(conv).transpose()?;
        match runner.generate_text(&directory, &req.body.prompt, model).await {
            Ok(text) => from_json(json!({"data": {"text": text}})),
            Err(oc_core::generate::GenerateError::ModelSelection(m)) => Err(ApiError::invalid_request(m)),
            Err(oc_core::generate::GenerateError::Unavailable { message, service }) => Err(ApiError::service_unavailable(message, service)),
        }
    }

    async fn experimental_session_export(
        &self,
        _ctx: &RequestContext,
        req: ExperimentalSessionExportRequest,
    ) -> Result<SessionTransferData, ApiError> {
        let data = oc_core::transfer::export(&self.app.bus, &self.app.sessions, &req.session_id, req.sanitize.unwrap_or(false))
            .map_err(|e| match e {
                oc_core::session::SessionError::NotFound(id) => ApiError::session_not_found(id),
                other => ApiError::unknown(format!("failed to export session: {other}")),
            })?;
        from_json(data).map_err(|e| ApiError::unknown(format!("failed to decode session message: {e}")))
    }

    async fn experimental_session_import(
        &self,
        ctx: &RequestContext,
        req: ExperimentalSessionImportRequest,
    ) -> Result<SessionInfo, ApiError> {
        let b = req.body;
        let location = oc_schema::types::LocationRef {
            directory: b.location.map(|l| l.directory).unwrap_or_else(|| ctx.server.cwd.clone()),
            workspace_id: None,
        };
        // Transcripts exported before v2.1 carry older names for native data: the wire stays plain JSON, and
        // the messages are upgraded and then validated as the current shape (`ImportedMessages`).
        let messages = oc_schema::legacy::messages(b.messages);
        if let Some(issue) = oc_server::validate::items_issue("Session.Message.Info", "messages", &messages) {
            return Err(ApiError::schema("Payload", issue));
        }
        let data = json!({"info": b.info, "messages": messages});
        match oc_core::transfer::import(&self.app.bus, &self.app.sessions, &data, location).await {
            Ok(s) => self.session_api(&s),
            Err(oc_core::transfer::ImportError::Conflict(id)) => {
                Err(ApiError::conflict(format!("Session already exists: {id}"), Some(id)))
            }
            Err(oc_core::transfer::ImportError::Session(e)) => Err(crate::sessions::session_error(e)),
            Err(oc_core::transfer::ImportError::Other(e)) => Err(defect(format!("{e:#}"))),
        }
    }

    async fn provider_list(&self, ctx: &RequestContext, _req: ProviderListRequest) -> Result<Vec<ProviderInfo>, ApiError> {
        let loc = self.locations.get(&ctx.location.directory).await?;
        let catalog = self.catalog(&loc).await?;
        catalog.available_providers().into_iter().map(conv).collect()
    }

    async fn provider_get(&self, ctx: &RequestContext, req: ProviderGetRequest) -> Result<ProviderInfo, ApiError> {
        let loc = self.locations.get(&ctx.location.directory).await?;
        let catalog = self.catalog(&loc).await?;
        match catalog.provider(&req.provider_id) {
            Some(p) => conv(p),
            None => Err(ApiError::provider_not_found(req.provider_id)),
        }
    }

    // ---- integration ----------------------------------------------------------------------------

    async fn integration_list(&self, ctx: &RequestContext, _req: IntegrationListRequest) -> Result<Vec<IntegrationInfo>, ApiError> {
        let integrations = self.integrations_ready(ctx).await?;
        let list = integrations.list().await.map_err(integration_error)?;
        list.iter().map(conv).collect()
    }

    async fn integration_get(&self, ctx: &RequestContext, req: IntegrationGetRequest) -> Result<IntegrationInfo, ApiError> {
        let integrations = self.integrations_ready(ctx).await?;
        conv(self.integration(&integrations, &req.integration_id).await?)
    }

    async fn experimental_integration_wellknown_add(
        &self,
        ctx: &RequestContext,
        req: ExperimentalIntegrationWellknownAddRequest,
    ) -> Result<(), ApiError> {
        let loc = self.locations.get(&ctx.location.directory).await?;
        let entry = self
            .wellknown
            .add(&req.body.url)
            .await
            .map_err(|e| ApiError::invalid_request(e.message).with_kind("well_known_discovery"))?;
        // `Integration.reload()`: register the new source's login method.
        oc_ai::credentials::wellknown::register(&self.models().integrations_for(Some(&loc.key)), &[entry]);
        Ok(())
    }

    async fn integration_connect_key(&self, ctx: &RequestContext, req: IntegrationConnectKeyRequest) -> Result<(), ApiError> {
        let integrations = self.integrations_ready(ctx).await?;
        self.integration(&integrations, &req.integration_id).await?;
        let answer = req.body.answer.map(conv).transpose()?;
        integrations
            .connect_key(&req.integration_id, &req.body.key, answer, req.body.label)
            .await
            .map_err(authorization_error)
    }

    async fn integration_connect_external(&self, ctx: &RequestContext, req: IntegrationConnectExternalRequest) -> Result<(), ApiError> {
        let integrations = self.integrations_ready(ctx).await?;
        self.integration(&integrations, &req.integration_id).await?;
        let answer = req.body.answer.map(conv).transpose()?;
        integrations
            .connect_external(&req.integration_id, &req.body.method_id, answer, req.body.label)
            .await
            .map_err(authorization_error)
    }

    async fn integration_oauth_connect(&self, ctx: &RequestContext, req: IntegrationOauthConnectRequest) -> Result<IntegrationAttemptEncoded, ApiError> {
        let integrations = self.integrations_ready(ctx).await?;
        let input = oc_ai::credentials::integration::OAuthConnect {
            integration_id: req.integration_id,
            method_id: req.body.method_id,
            answer: req.body.answer.map(conv).transpose()?,
            label: req.body.label,
        };
        let attempt = integrations.connect_oauth(input).await.map_err(authorization_error)?;
        conv(attempt)
    }

    async fn integration_oauth_status(&self, ctx: &RequestContext, req: IntegrationOauthStatusRequest) -> Result<IntegrationAttemptStatus, ApiError> {
        let integrations = self.integrations_ready(ctx).await?;
        self.integration(&integrations, &req.integration_id).await?;
        match integrations.oauth_status(&req.integration_id, &req.attempt_id) {
            Ok(s) => conv(s),
            Err(_) => Err(attempt_not_found(&req.integration_id, &req.attempt_id, "OAuth")),
        }
    }

    async fn integration_oauth_cancel(&self, ctx: &RequestContext, req: IntegrationOauthCancelRequest) -> Result<(), ApiError> {
        let loc = self.locations.get(&ctx.location.directory).await?;
        self.models().integrations_for(Some(&loc.key)).cancel_oauth(&req.integration_id, &req.attempt_id);
        Ok(())
    }

    async fn integration_oauth_complete(&self, ctx: &RequestContext, req: IntegrationOauthCompleteRequest) -> Result<(), ApiError> {
        let integrations = self.integrations_ready(ctx).await?;
        self.integration(&integrations, &req.integration_id).await?;
        integrations
            .complete_oauth(&req.integration_id, &req.attempt_id, req.body.code)
            .await
            .map_err(|e| match e {
                oc_ai::credentials::integration::IntegrationError::AttemptNotFound { integration_id, attempt_id } => {
                    attempt_not_found(&integration_id, &attempt_id, "OAuth")
                }
                oc_ai::credentials::integration::IntegrationError::CodeRequired { .. } => {
                    ApiError::invalid_request("Authorization code is required").with_kind("integration_code_required")
                }
                _ => ApiError::invalid_request("Authentication failed").with_kind("integration_authorization"),
            })
    }

    async fn integration_command_connect(&self, ctx: &RequestContext, req: IntegrationCommandConnectRequest) -> Result<IntegrationCommandAttempt, ApiError> {
        let integrations = self.integrations_ready(ctx).await?;
        let info = self.integration(&integrations, &req.integration_id).await?;
        let has = info.methods.iter().any(|m| m.kind() == "command" && m.id() == Some(req.body.method_id.as_str()));
        if !has {
            return Err(ApiError::integration_method_not_found(&req.integration_id, &req.body.method_id));
        }
        // The location's registry: it holds the wellknown login methods and answers the status polls.
        let attempt = integrations.connect_command(&req.integration_id, &req.body.method_id, req.body.label).await.map_err(authorization_error)?;
        conv(attempt)
    }

    async fn integration_command_status(&self, ctx: &RequestContext, req: IntegrationCommandStatusRequest) -> Result<IntegrationCommandAttemptStatus, ApiError> {
        let integrations = self.integrations_ready(ctx).await?;
        self.integration(&integrations, &req.integration_id).await?;
        match integrations.command_status(&req.integration_id, &req.attempt_id) {
            Ok(s) => conv(s),
            Err(_) => Err(attempt_not_found(&req.integration_id, &req.attempt_id, "Command")),
        }
    }

    async fn integration_command_cancel(&self, ctx: &RequestContext, req: IntegrationCommandCancelRequest) -> Result<(), ApiError> {
        let loc = self.locations.get(&ctx.location.directory).await?;
        self.models().integrations_for(Some(&loc.key)).cancel_command(&req.integration_id, &req.attempt_id);
        Ok(())
    }

    // ---- mcp ----------------------------------------------------------------------------------

    async fn mcp_list(&self, ctx: &RequestContext, _req: McpListRequest) -> Result<Vec<McpServer>, ApiError> {
        let loc = self.locations.get(&ctx.location.directory).await?;
        let Some(m) = self.mcp(&loc).await else { return Ok(vec![]) };
        m.servers().into_iter().map(conv).collect()
    }

    async fn mcp_resource_catalog(&self, ctx: &RequestContext, _req: McpResourceCatalogRequest) -> Result<McpResourceCatalog, ApiError> {
        let loc = self.locations.get(&ctx.location.directory).await?;
        let Some(m) = self.mcp(&loc).await else { return Ok(McpResourceCatalog { resources: vec![], templates: vec![] }) };
        conv(m.resource_catalog().await)
    }

    async fn experimental_mcp_add(&self, ctx: &RequestContext, req: ExperimentalMcpAddRequest) -> Result<(), ApiError> {
        let loc = self.locations.get(&ctx.location.directory).await?;
        let Some(m) = self.mcp(&loc).await else { return Err(unavailable("mcp")) };
        let config: oc_mcp::ServerConfig = conv(req.body.config)?;
        m.add(&req.server, config).await;
        Ok(())
    }

    async fn experimental_mcp_remove(&self, ctx: &RequestContext, req: ExperimentalMcpRemoveRequest) -> Result<(), ApiError> {
        let m = self.mcp_for(ctx, &req.server).await?;
        m.remove(&req.server).await.map_err(|e| mcp_error(&req.server, e))
    }

    async fn experimental_mcp_connect(&self, ctx: &RequestContext, req: ExperimentalMcpConnectRequest) -> Result<(), ApiError> {
        let m = self.mcp_for(ctx, &req.server).await?;
        m.connect(&req.server).await.map_err(|e| mcp_error(&req.server, e))
    }

    async fn experimental_mcp_disconnect(&self, ctx: &RequestContext, req: ExperimentalMcpDisconnectRequest) -> Result<(), ApiError> {
        let m = self.mcp_for(ctx, &req.server).await?;
        m.disconnect(&req.server).await.map_err(|e| mcp_error(&req.server, e))
    }

    // ---- credential ---------------------------------------------------------------------------

    async fn credential_list(&self, _ctx: &RequestContext, _req: CredentialListRequest) -> Result<Vec<CredentialEntry>, ApiError> {
        let items = self.credentials().all().await.map_err(defect)?;
        credential_entries(&items).into_iter().map(from_json).collect()
    }

    async fn credential_create(&self, _ctx: &RequestContext, req: CredentialCreateRequest) -> Result<CredentialEntry, ApiError> {
        let b = req.body;
        let store = self.credentials();
        if let Some(id) = &b.id
            && store.get(id).await.map_err(defect)?.is_some()
        {
            return Err(ApiError::conflict(format!("Credential already exists: {id}"), Some(id.clone())));
        }
        let input = oc_ai::credentials::CreateCredential {
            id: b.id,
            integration_id: b.integration_id,
            value: conv(b.value)?,
            label: b.label,
            activate: b.activate,
        };
        let created = store.create(input).await.map_err(defect)?;
        let items = store.list(&created.integration_id).await.map_err(defect)?;
        let entry = credential_entries(&items).into_iter().find(|e| e["id"] == json!(created.id)).unwrap_or_else(|| {
            json!({"id": created.id, "integrationID": created.integration_id, "label": created.label, "active": false, "value": created.value})
        });
        from_json(entry)
    }

    async fn credential_update(&self, _ctx: &RequestContext, req: CredentialUpdateRequest) -> Result<(), ApiError> {
        let update = oc_ai::credentials::CredentialUpdate { label: Some(req.body.label), value: None };
        self.credentials().update(&req.credential_id, update).await.map_err(defect)
    }

    async fn credential_remove(&self, _ctx: &RequestContext, req: CredentialRemoveRequest) -> Result<(), ApiError> {
        self.credentials().remove(&req.credential_id).await.map_err(defect)
    }

    async fn credential_activate(&self, _ctx: &RequestContext, req: CredentialActivateRequest) -> Result<(), ApiError> {
        self.credentials().activate(&req.credential_id).await.map_err(defect)
    }

    // ---- debug --------------------------------------------------------------------------------

    /// `debug.heapDump`. Upstream writes a V8 heap snapshot; ocd has no JavaScript heap, so after the same
    /// directory check it fails the way upstream reports a snapshot it could not write (`500 UnknownError`).
    async fn debug_heap_dump(&self, _ctx: &RequestContext, req: DebugHeapDumpRequest) -> Result<DebugHeapDumpResponse, ApiError> {
        if let Some(directory) = &req.directory
            && !std::path::Path::new(directory).is_absolute()
        {
            return Err(ApiError::invalid_request("directory must be an absolute path").with_field("directory"));
        }
        Err(ApiError::unknown_with_message(
            "Failed to write heap snapshot: ocd has no JavaScript heap, so it does not support heap snapshots",
        ))
    }

    // ---- project ------------------------------------------------------------------------------

    async fn project_list(&self, ctx: &RequestContext, req: ProjectListRequest) -> Result<Vec<Project>, ApiError> {
        // The project group runs behind the location middleware, so the request's location boots (and its
        // Project resolves, unarchiving it) first.
        self.locations.get(&ctx.location.directory).await?;
        oc_core::project::list(&self.app.bus, req.archived == Some(true)).map_err(defect)?.into_iter().map(from_json).collect()
    }

    async fn project_update(&self, ctx: &RequestContext, req: ProjectUpdateRequest) -> Result<Project, ApiError> {
        self.locations.get(&ctx.location.directory).await?;
        let id = req.project_id.clone();
        let not_found = || ApiError::project_not_found(&id);
        let b = req.body;
        let now = crate::util::now_ms();
        let mut sets: Vec<String> = Vec::new();
        let mut args: Vec<rusqlite::types::Value> = Vec::new();
        use rusqlite::types::Value as Sql;
        let opt = |s: &str| if s.is_empty() { Sql::Null } else { Sql::Text(s.to_string()) };
        if let Some(c) = &b.canonical {
            sets.push("worktree = ?".into());
            args.push(Sql::Text(c.clone()));
        }
        if let Some(n) = &b.name {
            sets.push("name = ?".into());
            args.push(opt(n));
        }
        if let Some(icon) = &b.icon {
            if let Some(o) = &icon.r#override {
                sets.push("icon_url_override = ?".into());
                args.push(opt(o));
            }
            if let Some(c) = &icon.color {
                sets.push("icon_color = ?".into());
                args.push(opt(c));
            }
        }
        if let Some(commands) = &b.commands
            && let Some(start) = &commands.start {
                sets.push("commands = ?".into());
                args.push(if start.is_empty() { Sql::Null } else { Sql::Text(json!({"start": start}).to_string()) });
            }
        sets.push("time_updated = ?".into());
        args.push(Sql::Integer(now));
        args.push(Sql::Text(id.clone()));
        let sql = format!("UPDATE project SET {} WHERE id = ?", sets.join(", "));
        let n = self.app.db.with(|c| c.execute(&sql, rusqlite::params_from_iter(args.iter()))).map_err(|_| not_found())?;
        if n == 0 {
            return Err(not_found());
        }
        let info = oc_core::project::get(&self.app.bus, &id).map_err(|_| not_found())?.ok_or_else(not_found)?;
        self.app.bus.publish("project.updated", info.clone(), None);
        from_json(info)
    }

    // ---- permission ---------------------------------------------------------------------------

    async fn permission_request_list(&self, ctx: &RequestContext, _req: PermissionRequestListRequest) -> Result<Vec<PermissionRequest>, ApiError> {
        let loc = self.locations.get(&ctx.location.directory).await?;
        // Permission services are per location upstream: keep requests of sessions located here.
        let here = self.locations.canonical_of(&loc.key);
        self.app
            .permissions
            .list()
            .into_iter()
            .filter(|r| {
                self.app
                    .db
                    .session_get(&r.session_id)
                    .ok()
                    .flatten()
                    .is_some_and(|s| self.locations.canonical_of(&s.location.directory) == here)
            })
            .map(conv)
            .collect()
    }

    async fn permission_saved_list(&self, ctx: &RequestContext, req: PermissionSavedListRequest) -> Result<Vec<PermissionSavedInfo>, ApiError> {
        let loc = self.locations.get(&ctx.location.directory).await?;
        let project = req.project_id.unwrap_or_else(|| loc.project_id().to_string());
        store::saved_permissions(&self.app.db, Some(&project)).map_err(defect)?.into_iter().map(from_json).collect()
    }

    async fn permission_saved_update(&self, ctx: &RequestContext, req: PermissionSavedUpdateRequest) -> Result<PermissionSavedInfo, ApiError> {
        self.locations.get(&ctx.location.directory).await?;
        let body = req.body;
        match store::update_saved_permission(&self.app.db, &req.id, body.action.as_deref(), body.resource.as_deref()).map_err(defect)? {
            store::SavedUpdate::Updated(v) => from_json(v),
            store::SavedUpdate::NotFound => Err(ApiError::permission_saved_not_found(req.id)),
            store::SavedUpdate::Conflict(action, resource) => Err(ApiError::conflict(
                format!("A saved permission already allows {action} for {resource}"),
                Some(req.id),
            )),
        }
    }

    async fn permission_saved_remove(&self, ctx: &RequestContext, req: PermissionSavedRemoveRequest) -> Result<(), ApiError> {
        self.locations.get(&ctx.location.directory).await?;
        store::remove_saved_permission(&self.app.db, &req.id).map_err(defect)
    }

    async fn session_permission_create(&self, _ctx: &RequestContext, req: SessionPermissionCreateRequest) -> Result<SessionPermissionCreateData, ApiError> {
        self.session_scope(&req.session_id).await?;
        let b = req.body;
        let input = oc_core::permission::AssertInput {
            id: b.id,
            session_id: req.session_id.clone(),
            action: b.action,
            resources: b.resources,
            save: b.save,
            metadata: b.metadata.map(conv).transpose()?,
            source: b.source.map(conv).transpose()?,
            agent: b.agent,
        };
        let (id, effect) = self.app.permissions.ask(input).await;
        Ok(SessionPermissionCreateData { id, effect: conv(effect)? })
    }

    async fn session_permission_list(&self, _ctx: &RequestContext, req: SessionPermissionListRequest) -> Result<Vec<PermissionRequest>, ApiError> {
        self.session_scope(&req.session_id).await?;
        self.app.permissions.for_session(&req.session_id).into_iter().map(conv).collect()
    }

    async fn session_permission_get(&self, _ctx: &RequestContext, req: SessionPermissionGetRequest) -> Result<PermissionRequest, ApiError> {
        self.session_scope(&req.session_id).await?;
        match self.app.permissions.get(&req.request_id) {
            Some(r) if r.session_id == req.session_id => conv(r),
            _ => Err(ApiError::permission_not_found(req.request_id)),
        }
    }

    async fn session_permission_reply(&self, _ctx: &RequestContext, req: SessionPermissionReplyRequest) -> Result<(), ApiError> {
        self.session_scope(&req.session_id).await?;
        match self.app.permissions.get(&req.request_id) {
            Some(r) if r.session_id == req.session_id => {}
            _ => return Err(ApiError::permission_not_found(req.request_id)),
        }
        let reply = match req.body.decision {
            PermissionReply::Once => oc_core::permission::Reply::Once,
            PermissionReply::Always => oc_core::permission::Reply::Always,
            PermissionReply::Reject => oc_core::permission::Reply::Reject,
        };
        self.app
            .permissions
            .reply(&req.request_id, reply, req.body.message)
            .await
            .map_err(|_| ApiError::permission_not_found(&req.request_id))
    }

    // ---- filesystem ---------------------------------------------------------------------------

    async fn fs_read(&self, ctx: &RequestContext, req: FsReadRequest) -> Result<FsFile, ApiError> {
        let loc = self.locations.get(&ctx.location.directory).await?;
        crate::fs::read(&loc, &loc.key, &req.path)
    }

    async fn fs_list(&self, ctx: &RequestContext, req: FsListRequest) -> Result<Vec<FileSystemEntry>, ApiError> {
        let loc = self.locations.get(&ctx.location.directory).await?;
        crate::fs::list(&loc.key, req.path.as_deref())?.into_iter().map(from_json).collect()
    }

    async fn fs_find(&self, ctx: &RequestContext, req: FsFindRequest) -> Result<Vec<FileSystemEntry>, ApiError> {
        let loc = self.locations.get(&ctx.location.directory).await?;
        let kind = req.r#type.map(|t| match t {
            FsFindType::File => "file",
            FsFindType::Directory => "directory",
        });
        let root = std::path::PathBuf::from(&loc.key);
        let vcs = loc.vcs().is_some();
        let query = req.query;
        let limit = req.limit;
        let found = tokio::task::spawn_blocking(move || crate::fs::find(root, &query, kind, limit, vcs)).await.map_err(defect)?;
        found.into_iter().map(from_json).collect()
    }

    async fn experimental_fs_write(&self, ctx: &RequestContext, req: ExperimentalFsWriteRequest) -> Result<FileSystemWrite, ApiError> {
        let loc = self.locations.get(&ctx.location.directory).await?;
        from_json(crate::fs::write(&loc.key, &req.path, &req.body)?)
    }

    // ---- command / skill / reference ----------------------------------------------------------

    async fn command_list(&self, ctx: &RequestContext, _req: CommandListRequest) -> Result<Vec<CommandInfo>, ApiError> {
        let loc = self.locations.get(&ctx.location.directory).await?;
        self.commands(&loc)
            .await
            .into_iter()
            .map(|(name, (description, _))| {
                let mut v = json!({"name": name});
                if let Some(d) = description {
                    v["description"] = json!(d);
                }
                from_json(v)
            })
            .collect()
    }

    async fn skill_list(&self, ctx: &RequestContext, _req: SkillListRequest) -> Result<Vec<SkillInfo>, ApiError> {
        let loc = self.locations.get(&ctx.location.directory).await?;
        let skills = self.skills(&loc).await;
        skills.iter().map(|s| from_json(crate::catalog::skill_json(s))).collect()
    }

    async fn reference_list(&self, ctx: &RequestContext, _req: ReferenceListRequest) -> Result<Vec<ReferenceInfo>, ApiError> {
        let loc = self.locations.get(&ctx.location.directory).await?;
        self.app.plugins.references(&loc.location, &self.app.global).await.iter().map(|r| from_json(r.to_json())).collect()
    }

    // ---- rpc ----------------------------------------------------------------------------------

    async fn rpc_call(&self, ctx: &RequestContext, req: RpcCallRequest) -> Result<RpcOutput, ApiError> {
        let loc = self.locations.get(&ctx.location.directory).await?;
        // `Plugin.awaitActivation`, then `Rpc.call` (server/src/handlers/rpc.ts).
        self.app.plugins.await_activation(&loc.key).await;
        let location = oc_plugin::LocationRef::new(loc.key.clone());
        let input = req.body.input.clone().map(|v| serde_json::to_value(v).unwrap_or(Value::Null)).unwrap_or(Value::Null);
        let result = match self.app.plugins.host() {
            Some(host) => host.rpc_call(&location, &req.rpc_id, &req.method, input).await,
            None => Err(oc_plugin::RpcFailure { kind: "rpc.unavailable".into(), message: format!("RPC is unavailable: {}", req.rpc_id), data: None }),
        };
        match result {
            Ok(Value::Null) => from_json(json!({})),
            Ok(output) => from_json(json!({ "output": output })),
            Err(f) if f.kind == "rpc.invalid_output" || f.kind == "rpc.internal" => Err(ApiError::rpc_internal(f.kind, f.message)),
            Err(f) => Err(ApiError::rpc(f.kind, f.message, f.data)),
        }
    }

    // ---- events -------------------------------------------------------------------------------

    async fn event_subscribe(&self, _ctx: &RequestContext, _req: EventSubscribeRequest) -> Result<EventSubscription, ApiError> {
        let rx = self.app.bus.subscribe();
        let stream = futures::stream::unfold(rx, |mut rx| async move {
            match rx.recv().await {
                Ok(env) => Some((serde_json::to_value(&*env).unwrap_or(Value::Null), rx)),
                // A lagging subscriber is dropped (volatile feed): end the response.
                Err(_) => None,
            }
        });
        Ok(EventSubscription::Stream(stream.boxed()))
    }

    // ---- pty / shell / persistent pty (TODO(core)) --------------------------------------------

    async fn pty_list(&self, ctx: &RequestContext, _req: PtyListRequest) -> Result<Vec<Pty>, ApiError> {
        let loc = self.locations.get(&ctx.location.directory).await?;
        let owned = self.pty_owner.lock().clone();
        self.app
            .ptys
            .list()
            .into_iter()
            .filter(|p| owned.get(&p.id) == Some(&loc.key))
            .map(conv)
            .collect()
    }

    async fn pty_create(&self, ctx: &RequestContext, req: PtyCreateRequest) -> Result<Pty, ApiError> {
        let loc = self.locations.get(&ctx.location.directory).await?;
        let b = req.body;
        // `{...body.env, ...PtyEnvironment.get()}` — the PTY environment contributes nothing locally.
        let input = oc_core::pty::CreateInput {
            command: b.command,
            args: b.args,
            cwd: b.cwd.filter(|c| !c.is_empty()),
            title: b.title,
            env: b.env.map(|e| e.into_iter().collect()),
        };
        let shell = oc_tools::shell::select::resolve(
            oc_tools::shell::select::Priority::Config,
            oc_config::fold::shell(&loc.location.entries),
            Some(&self.app.global.bin),
        );
        let info = self.app.ptys.create(input, &loc.key, &shell).map_err(|e| ApiError::defect(e.to_string()))?;
        self.pty_owner.lock().insert(info.id.clone(), loc.key.clone());
        conv(info)
    }

    async fn pty_get(&self, ctx: &RequestContext, req: PtyGetRequest) -> Result<Pty, ApiError> {
        self.owned_pty(ctx, &req.pty_id).await?;
        conv(self.app.ptys.get(&req.pty_id).map_err(pty_error)?)
    }

    async fn pty_update(&self, ctx: &RequestContext, req: PtyUpdateRequest) -> Result<Pty, ApiError> {
        self.owned_pty(ctx, &req.pty_id).await?;
        let size = req.body.size.map(|s| (s.rows.clamp(1, u16::MAX as i64) as u16, s.cols.clamp(1, u16::MAX as i64) as u16));
        conv(self.app.ptys.update(&req.pty_id, req.body.title, size).map_err(pty_error)?)
    }

    async fn pty_remove(&self, ctx: &RequestContext, req: PtyRemoveRequest) -> Result<(), ApiError> {
        self.owned_pty(ctx, &req.pty_id).await?;
        self.app.ptys.remove(&req.pty_id).map_err(pty_error)?;
        self.pty_owner.lock().remove(&req.pty_id);
        Ok(())
    }

    async fn pty_connect(&self, ctx: &RequestContext, req: oc_server::PtyAttachRequest) -> Result<oc_server::PtyAttachment, ApiError> {
        self.owned_pty(ctx, &req.pty_id).await?;
        let mut attachment = self.app.ptys.attach(&req.pty_id, req.cursor).map_err(|e| match e {
            oc_core::pty::PtyError::Exited(_) => ApiError::pty_exited(),
            other => pty_error(other),
        })?;
        let rx = std::mem::replace(&mut attachment.rx, tokio::sync::mpsc::unbounded_channel().1);
        let replay = std::mem::take(&mut attachment.replay);
        let cursor = attachment.cursor as i64;
        let attachment = Arc::new(attachment);
        let live = attachment.clone();
        // Chunks queue until the socket has sent the replay; the first poll goes live.
        let output = futures::stream::unfold((rx, Some(live)), |(mut rx, activate)| async move {
            if let Some(a) = &activate {
                a.activate();
            }
            match rx.recv().await {
                Some(oc_core::pty::Chunk::Data(d)) => Some((d, (rx, None))),
                Some(oc_core::pty::Chunk::End(_)) | None => None,
            }
        })
        .boxed();
        Ok(oc_server::PtyAttachment { replay, cursor, output, control: Arc::new(PtyHandle(attachment)) })
    }

    async fn server_info(&self, ctx: &RequestContext, _req: ServerInfoRequest) -> Result<ServerInfo, ApiError> {
        let available = self.persistent().await.available();
        Ok(ctx.server.info(Some(available)))
    }

    async fn server_experimental_persistent_pty_read(
        &self,
        _ctx: &RequestContext,
        req: ServerExperimentalPersistentPtyReadRequest,
    ) -> Result<Option<PersistentPtyReadResult>, ApiError> {
        let pty = self.persistent().await;
        pty.read(&req.session_id, req.lines).await.map_err(persistent_error)?.map(conv).transpose()
    }

    async fn server_experimental_persistent_pty_list(
        &self,
        _ctx: &RequestContext,
        req: ServerExperimentalPersistentPtyListRequest,
    ) -> Result<Vec<PersistentPtyInfo>, ApiError> {
        let pty = self.persistent().await;
        pty.list(Some(&req.session_id)).await.map_err(persistent_error)?.into_iter().map(conv).collect()
    }

    async fn server_experimental_persistent_pty_create(
        &self,
        _ctx: &RequestContext,
        req: ServerExperimentalPersistentPtyCreateRequest,
    ) -> Result<PersistentPtyInfo, ApiError> {
        let pty = self.persistent().await;
        let input: oc_persistent_pty::types::CreateInput = conv(req.body)?;
        conv(pty.create(&req.session_id, input).await.map_err(persistent_error)?)
    }

    async fn server_experimental_persistent_pty_shutdown(
        &self,
        _ctx: &RequestContext,
        _req: ServerExperimentalPersistentPtyShutdownRequest,
    ) -> Result<(), ApiError> {
        self.persistent().await.shutdown().await.map_err(persistent_error)
    }

    async fn server_experimental_persistent_pty_handoff(
        &self,
        _ctx: &RequestContext,
        _req: ServerExperimentalPersistentPtyHandoffRequest,
    ) -> Result<ServerExperimentalPersistentPtyHandoffResponse, ApiError> {
        let handoff = self.prepare_pty_handoff().await.map_err(persistent_error)?;
        Ok(ServerExperimentalPersistentPtyHandoffResponse { handoff: handoff.map(conv).transpose()? })
    }

    async fn server_experimental_persistent_pty_get(
        &self,
        _ctx: &RequestContext,
        req: ServerExperimentalPersistentPtyGetRequest,
    ) -> Result<PersistentPtyInfo, ApiError> {
        conv(self.persistent().await.get(&req.pty_id).await.map_err(persistent_error)?)
    }

    async fn server_experimental_persistent_pty_update(
        &self,
        _ctx: &RequestContext,
        req: ServerExperimentalPersistentPtyUpdateRequest,
    ) -> Result<PersistentPtyInfo, ApiError> {
        let pty = self.persistent().await;
        let size = &req.body.size;
        pty.resize(&req.pty_id, size.cols.max(0) as u64, size.rows.max(0) as u64, req.body.attachment_id.as_deref())
            .await
            .map_err(persistent_error)?;
        conv(pty.get(&req.pty_id).await.map_err(persistent_error)?)
    }

    async fn server_experimental_persistent_pty_remove(
        &self,
        _ctx: &RequestContext,
        req: ServerExperimentalPersistentPtyRemoveRequest,
    ) -> Result<(), ApiError> {
        self.persistent().await.remove(&req.pty_id).await.map_err(persistent_error)
    }

    async fn server_experimental_persistent_pty_snapshot(
        &self,
        _ctx: &RequestContext,
        req: ServerExperimentalPersistentPtySnapshotRequest,
    ) -> Result<PersistentPtySnapshot, ApiError> {
        conv(self.persistent().await.snapshot(&req.pty_id).await.map_err(persistent_error)?)
    }

    async fn persistent_pty_connect(
        &self,
        _ctx: &RequestContext,
        req: oc_server::PersistentPtyAttachRequest,
    ) -> Result<oc_server::PersistentPtyAttachment, ApiError> {
        crate::persistent::connect(self.persistent().await, req).await
    }

    async fn shell_list(&self, ctx: &RequestContext, _req: ShellListRequest) -> Result<Vec<ShellInfo>, ApiError> {
        let loc = self.locations.get(&ctx.location.directory).await?;
        let owned = self.shell_owner.lock().clone();
        self.shell().list().into_iter().filter(|s| owned.get(&s.id) == Some(&loc.key)).map(conv).collect()
    }

    async fn shell_create(&self, ctx: &RequestContext, req: ShellCreateRequest) -> Result<ShellInfo, ApiError> {
        let loc = self.locations.get(&ctx.location.directory).await?;
        let b = req.body;
        let metadata: oc_schema::Metadata = b.metadata.map(conv).transpose()?.unwrap_or_default();
        // Session environment when the command belongs to a session, else the process environment.
        let session_env = metadata
            .get("sessionID")
            .and_then(Value::as_str)
            .filter(|s| s.starts_with("ses"))
            .and_then(|s| self.app.environments.lock().get(s).cloned());
        let mut env: std::collections::HashMap<String, String> = session_env.unwrap_or_else(|| std::env::vars().collect());
        env.insert("TERM".into(), "xterm-256color".into());
        env.insert("OPENCODE_TERMINAL".into(), "1".into());
        let mut invocation = oc_tools::host::ShellInvocation {
            command: b.command,
            cwd: b.cwd.filter(|c| !c.is_empty()).unwrap_or_else(|| loc.key.clone()),
            timeout: b.timeout.unwrap_or(0).max(0) as u64,
            shell: oc_tools::shell::select::resolve(
                oc_tools::shell::select::Priority::Config,
                oc_config::fold::shell(&loc.location.entries),
                Some(&self.app.global.bin),
            ),
            env,
        };
        self.app.plugins.shell_before(&loc.key, &mut invocation).await.map_err(ApiError::defect)?;
        // Core errors become defects (500), like upstream's `Effect.orDie`.
        let info = self.shell().spawn(&invocation, metadata, loc.project_id()).await.map_err(ApiError::defect)?;
        self.shell_owner.lock().insert(info.id.clone(), loc.key.clone());
        conv(info)
    }

    async fn shell_get(&self, ctx: &RequestContext, req: ShellGetRequest) -> Result<ShellInfo, ApiError> {
        self.owned_shell(ctx, &req.id).await?;
        conv(self.shell().get(&req.id).map_err(|_| ApiError::shell_not_found(&req.id))?)
    }

    async fn shell_remove(&self, ctx: &RequestContext, req: ShellRemoveRequest) -> Result<(), ApiError> {
        // Missing is a no-op.
        if self.owned_shell(ctx, &req.id).await.is_ok() {
            let _ = self.shell().remove(&req.id);
            self.shell_owner.lock().remove(&req.id);
        }
        Ok(())
    }

    async fn shell_output(&self, ctx: &RequestContext, req: ShellOutputRequest) -> Result<ShellOutputData, ApiError> {
        self.owned_shell(ctx, &req.id).await?;
        let page = self
            .shell()
            .output(&req.id, req.cursor.map(|c| c.max(0) as u64), req.limit.map(|l| l.max(0) as u64))
            .map_err(|_| ApiError::shell_not_found(&req.id))?;
        conv(page)
    }

    // ---- worktree -----------------------------------------------------------------------------

    async fn worktree_create(&self, _ctx: &RequestContext, req: WorktreeCreateRequest) -> Result<WorktreeInfo, ApiError> {
        let b = req.body;
        let configured = self.worktree_parent(&b.project_id).await;
        let strategies = self.worktree_strategies(&b.project_id).await;
        let dir = oc_core::worktree::create(
            &self.app.bus,
            &self.app.global,
            &b.project_id,
            b.from.as_deref(),
            b.branch.as_deref(),
            b.directory.as_deref().map(std::path::Path::new),
            b.name.as_deref(),
            configured.as_deref(),
            &strategies,
        )
        .await
        .map_err(worktree_error)?;
        from_json(json!({"directory": dir.display().to_string()}))
    }

    async fn worktree_refresh(&self, _ctx: &RequestContext, req: WorktreeRefreshRequest) -> Result<(), ApiError> {
        let strategies = self.worktree_strategies(&req.body.project_id).await;
        oc_core::worktree::refresh(&self.app.bus, &req.body.project_id, &strategies).await.map_err(worktree_error)
    }

    async fn worktree_remove(&self, _ctx: &RequestContext, req: WorktreeRemoveRequest) -> Result<(), ApiError> {
        let b = req.body;
        let strategies = self.worktree_strategies(&b.project_id).await;
        oc_core::worktree::remove(&self.app.bus, &b.project_id, &b.directory, b.force, &strategies).await.map_err(worktree_error)
    }

    async fn experimental_session_stats(&self, _ctx: &RequestContext, req: ExperimentalSessionStatsRequest) -> Result<SessionStatsInfo, ApiError> {
        let tz_name = req.timezone.clone().unwrap_or_else(|| "UTC".into());
        let Some(timezone) = crate::stats::parse_timezone(&tz_name) else {
            return Err(ApiError::invalid_request(format!("Invalid time zone: {tz_name}")));
        };
        // `NumberFromString` decodes unparsable input to NaN, which upstream's stats query turns into a
        // defect (500, empty body).
        if req.from.is_some_and(f64::is_nan) || req.to.is_some_and(f64::is_nan) {
            return Err(ApiError::defect("stats range is NaN"));
        }
        let input = crate::stats::Input {
            from: req.from.map(|f| f as i64),
            to: req.to.map(|t| t as i64),
            project: req.project,
            timezone,
            tools: match req.tools {
                Some(ExperimentalSessionStatsTools::None) => crate::stats::ToolMode::None,
                Some(ExperimentalSessionStatsTools::Detail) => crate::stats::ToolMode::Detail,
                _ => crate::stats::ToolMode::Summary,
            },
        };
        let db = self.app.db.clone();
        let result = tokio::task::spawn_blocking(move || crate::stats::get(&db, input)).await.map_err(defect)?.map_err(defect)?;
        match result {
            Ok(v) => from_json(v),
            Err(_) => Err(ApiError::invalid_request("Stats range must end after it starts")),
        }
    }

    async fn worktree_list(&self, _ctx: &RequestContext, req: WorktreeListRequest) -> Result<WorktreeList, ApiError> {
        if oc_core::project::get(&self.app.bus, &req.project_id).map_err(defect)?.is_none() {
            return Err(ApiError::project_not_found(req.project_id));
        }
        let rows: Vec<Value> = self
            .app
            .db
            .with(|c| {
                let mut s = c.prepare(
                    "SELECT directory, strategy FROM worktree WHERE project_id = ? ORDER BY time_created DESC, directory ASC",
                )?;
                
                s
                    .query_map([&req.project_id], |r| {
                        let mut v = json!({"directory": r.get::<_, String>(0)?});
                        if let Some(st) = r.get::<_, Option<String>>(1)? {
                            v["strategy"] = json!(st);
                        }
                        Ok(v)
                    })?
                    .collect()
            })
            .map_err(defect)?;
        rows.into_iter().map(from_json).collect()
    }

    // ---- vcs ----------------------------------------------------------------------------------

    async fn vcs_init(&self, ctx: &RequestContext, req: VcsInitRequest) -> Result<(), ApiError> {
        let loc = self.locations.get(&ctx.location.directory).await?;
        let provider = req.provider.unwrap_or_else(|| "git".into());
        let directory = loc.project_directory().to_path_buf();
        self.app.plugins.await_activation(&loc.key).await;
        if !directory.is_dir() {
            return Err(ApiError::invalid_request("Project directory does not exist").with_field("location"));
        }
        if loc.vcs().is_some() {
            return Err(ApiError::conflict("Project already has version control", Some(directory.display().to_string())));
        }
        let registry = self.app.plugins.vcs(&loc.key, loc.vcs()).await;
        let unavailable = || ApiError::service_unavailable("VCS initialization failed", Some(provider.clone()));
        match registry.get(&provider) {
            Some(p) if !p.has_init() => return Err(ApiError::vcs_init_not_supported(provider)),
            Some(p) => p.init(&oc_core::vcs::Scope::of(&loc.location)).await.map_err(|_| unavailable())?,
            // TODO(core): the hg provider is not ported.
            None if provider == "hg" => return Err(unavailable()),
            None => return Err(ApiError::invalid_request("Unknown VCS provider").with_field("provider")),
        }
        let resolved = oc_core::project::resolve_and_persist(&self.app.bus, &directory).await.map_err(defect)?;
        if resolved.vcs.is_none() {
            return Err(ApiError::service_unavailable("VCS initialization failed", Some(provider)));
        }
        self.locations.evict(&ctx.location.directory).await;
        self.app.bus.publish("worktree.updated", json!({"projectID": resolved.id}), None);
        Ok(())
    }

    async fn vcs_get(&self, ctx: &RequestContext, _req: VcsGetRequest) -> Result<VcsInfo, ApiError> {
        // The info of the last refresh (`current.info`); before the boot refresh, read it now.
        let loc = self.locations.get(&ctx.location.directory).await?;
        match loc.vcs_state.cached() {
            Some(info) => conv(info),
            None => conv(crate::vcs_watch::info(&self.app, &loc).await),
        }
    }

    async fn vcs_base(&self, ctx: &RequestContext, _req: VcsBaseRequest) -> Result<Option<VcsBase>, ApiError> {
        let vcs = self.vcs(ctx).await?;
        match vcs.base().await {
            Ok(b) => b.map(conv).transpose(),
            Err(e) => Err(ApiError::service_unavailable(e.message, Some("vcs".into()))),
        }
    }

    async fn vcs_status(&self, ctx: &RequestContext, _req: VcsStatusRequest) -> Result<Vec<VcsFileStatus>, ApiError> {
        let vcs = self.vcs(ctx).await?;
        vcs.status().await.into_iter().map(conv).collect()
    }

    async fn vcs_branch_list(&self, ctx: &RequestContext, req: VcsBranchListRequest) -> Result<VcsBranchList, ApiError> {
        let vcs = self.vcs(ctx).await?;
        let limit = req.limit.unwrap_or(50).clamp(0, 100) as u64;
        Ok(vcs.branches(&oc_git::vcs::BranchOptions { search: req.search, limit: Some(limit) }).await)
    }

    async fn vcs_diff(&self, ctx: &RequestContext, req: VcsDiffRequest) -> Result<Vec<FileDiffInfo>, ApiError> {
        let vcs = self.vcs(ctx).await?;
        let mode = match req.mode {
            VcsMode::Working => oc_git::vcs::Mode::Working,
            VcsMode::Branch => oc_git::vcs::Mode::Branch,
            VcsMode::Committed => oc_git::vcs::Mode::Committed,
        };
        match vcs.diff(mode, req.context.map(|c| c.max(0) as u64), req.base.as_deref()).await {
            Ok(rows) => conv(rows),
            Err(e) => Err(ApiError::service_unavailable(e.message, Some("vcs".into()))),
        }
    }

    // ---- debug / migration --------------------------------------------------------------------

    async fn debug_location_list(&self, _ctx: &RequestContext, _req: DebugLocationListRequest) -> Result<Vec<LocationPublicRef>, ApiError> {
        self.locations.keys().into_iter().map(|k| from_json(json!({"directory": k}))).collect()
    }

    async fn debug_location_evict(&self, ctx: &RequestContext, _req: DebugLocationEvictRequest) -> Result<(), ApiError> {
        self.locations.evict(&ctx.location.directory).await;
        Ok(())
    }

    async fn experimental_migration_v1_status(
        &self,
        _ctx: &RequestContext,
        _req: ExperimentalMigrationV1StatusRequest,
    ) -> Result<ExperimentalMigrationV1StatusResponse, ApiError> {
        // V1 → V2 history import status (`V1Migration.status()`); the import itself starts with the server.
        let status = oc_core::v1_import::Importer::global().status(&self.app.db).map_err(defect)?;
        from_json(status.to_json())
    }

    // ---- websearch (TODO(core)) ---------------------------------------------------------------

    async fn websearch_providers(&self, ctx: &RequestContext, _req: WebsearchProvidersRequest) -> Result<Vec<WebSearchProvider>, ApiError> {
        let loc = self.locations.get(&ctx.location.directory).await?;
        let websearch = self.configure_websearch(&loc);
        websearch.providers().await.into_iter().map(conv).collect()
    }

    async fn websearch_query(&self, ctx: &RequestContext, req: WebsearchQueryRequest) -> Result<WebSearchResponseEncoded, ApiError> {
        let loc = self.locations.get(&ctx.location.directory).await?;
        // A cold location must finish activating plugins before their providers can be resolved.
        self.await_plugins(&loc).await?;
        let websearch = self.configure_websearch(&loc);
        use oc_tools::websearch::WebSearchError as W;
        match websearch.query(&req.body.query, req.body.provider_id.as_deref(), None, &|_| {}).await {
            Ok(r) => conv(r),
            Err(W::ProviderRequired) => Err(ApiError::invalid_request("Web search provider is required")
                .with_kind("websearch_provider_required")
                .with_field("providerID")),
            Err(W::ProviderNotFound(id)) => Err(ApiError::invalid_request(format!("Web search provider not found: {id}"))
                .with_kind("websearch_provider_not_found")
                .with_field("providerID")),
            Err(W::Disabled) => Err(ApiError::invalid_request("Web search is disabled").with_kind("websearch_disabled")),
            Err(W::Request { provider_id, .. }) => Err(ApiError::service_unavailable(
                format!("Web search request failed: {provider_id}"),
                Some(provider_id),
            )),
        }
    }

    // ---- config -------------------------------------------------------------------------------

    async fn config_get(&self, ctx: &RequestContext, _req: ConfigGetRequest) -> Result<Vec<ConfigEntry>, ApiError> {
        let loc = self.locations.get(&ctx.location.directory).await?;
        loc.location.entries.iter().map(conv).collect()
    }

    async fn config_shells(&self, ctx: &RequestContext, _req: ConfigShellsRequest) -> Result<Vec<ConfigShellOption>, ApiError> {
        self.locations.get(&ctx.location.directory).await?;
        let bin = self.app.global.bin.clone();
        let items = tokio::task::spawn_blocking(move || crate::shells::list(&bin)).await.map_err(defect)?;
        items.into_iter().map(from_json).collect()
    }

    async fn experimental_config_update(&self, ctx: &RequestContext, req: ExperimentalConfigUpdateRequest) -> Result<(), ApiError> {
        self.locations.get(&ctx.location.directory).await?;
        let dir = self.app.global.config.clone();
        let shell = req.body.shell;
        tokio::task::spawn_blocking(move || oc_config::load::update_shell(&dir, shell.as_deref()))
            .await
            .map_err(defect)?
            .map_err(defect)?;
        // Reload every loaded location; core announces `config.updated` (which drops our cached
        // services) when its configuration changed.
        for key in self.locations.keys() {
            match &self.runner {
                Some(r) => {
                    if let Err(e) = r.reload_location(&key).await {
                        tracing::warn!(location = key, error = %e, "configuration reload failed");
                    }
                }
                None => self.locations.forget(&key),
            }
        }
        Ok(())
    }
}

/// Credential listings mark each integration's last credential (in list order) active.
fn credential_entries(items: &[oc_ai::credentials::Credential]) -> Vec<Value> {
    let mut selected = std::collections::HashMap::new();
    for item in items {
        selected.insert(item.integration_id.clone(), item.id.clone());
    }
    items
        .iter()
        .map(|i| {
            json!({
                "id": i.id, "integrationID": i.integration_id, "label": i.label,
                "active": selected.get(&i.integration_id) == Some(&i.id), "value": i.value,
            })
        })
        .collect()
}

fn integration_error(e: oc_ai::credentials::integration::IntegrationError) -> ApiError {
    ApiError::defect(e.to_string())
}

/// `Integration.AuthorizationError` → 400 `integration_authorization`.
fn authorization_error(e: oc_ai::credentials::integration::IntegrationError) -> ApiError {
    use oc_ai::credentials::integration::IntegrationError as E;
    match e {
        E::Authorization(m) if !m.trim().is_empty() => ApiError::invalid_request(m).with_kind("integration_authorization"),
        E::Defect(m) => ApiError::defect(m),
        _ => ApiError::invalid_request("Authentication failed").with_kind("integration_authorization"),
    }
}

/// `PersistentPty.UnavailableError` → 503 `{service:"opencode-pty"}`; `NotFoundError` → 404.
fn persistent_error(e: oc_persistent_pty::Error) -> ApiError {
    match e {
        oc_persistent_pty::Error::Unavailable(m) => ApiError::service_unavailable(m, Some("opencode-pty".into())),
        oc_persistent_pty::Error::NotFound(id) => ApiError::pty_not_found(id),
    }
}

/// `Worktree.Error` → 404 ProjectNotFoundError or 400 `WorktreeError{name, data}`.
fn worktree_error(e: oc_core::worktree::WorktreeError) -> ApiError {
    use oc_core::worktree::WorktreeError as W;
    match e {
        W::ProjectNotFound(id) => ApiError::project_not_found(id),
        W::SourceDirectoryNotFound(d) => ApiError::worktree(format!("Worktree source not found: {d}"), None),
        W::DestinationExists(d) => ApiError::worktree(format!("Worktree destination already exists: {d}"), None),
        W::InvalidDirectory(d) => ApiError::worktree(format!("Invalid worktree directory: {d}"), None),
        W::Git { message, force_required } => ApiError::worktree(message, force_required),
        other => ApiError::worktree(other.to_string(), None),
    }
}

/// Input side of a location PTY attachment.
struct PtyHandle(Arc<oc_core::pty::Attachment>);

impl oc_server::PtyControl for PtyHandle {
    fn write(&self, data: &str) {
        self.0.write(data);
    }
    fn detach(&self) {
        self.0.detach();
    }
}

fn pty_error(e: oc_core::pty::PtyError) -> ApiError {
    match e {
        oc_core::pty::PtyError::NotFound(id) | oc_core::pty::PtyError::Exited(id) => ApiError::pty_not_found(id),
        oc_core::pty::PtyError::Spawn(m) => ApiError::defect(m),
    }
}

fn mcp_not_found(server: &str) -> ApiError {
    ApiError::mcp_server_not_found(server, format!("MCP server not found: {server}"))
}

fn mcp_error(server: &str, e: oc_mcp::ManagerError) -> ApiError {
    match e {
        oc_mcp::ManagerError::NotFound(_) => mcp_not_found(server),
        other => ApiError::defect(other.to_string()),
    }
}

/// `Form.*` errors with upstream tags/messages.
fn form_error(e: FormError) -> ApiError {
    match e {
        FormError::NotFound(id) => ApiError::form_not_found(id),
        FormError::AlreadySettled(id) => {
            let message = format!("Form already settled: {id}");
            ApiError::form_already_settled(id, message)
        }
        FormError::InvalidAnswer { id, message } => ApiError::form_invalid_answer(id, message),
        FormError::AlreadyExists(id) => ApiError::conflict(format!("Form already exists: {id}"), Some(id)),
        FormError::InvalidForm(m) => ApiError::invalid_request(m).with_field("fields"),
    }
}

fn attempt_not_found(integration: &str, attempt: &str, kind: &str) -> ApiError {
    ApiError::integration_attempt_not_found(integration, attempt, format!("{kind} attempt not found: {attempt}"))
}

impl Engine {
    /// Shell commands are per location upstream: one created elsewhere is not found here.
    async fn owned_shell(&self, ctx: &RequestContext, id: &str) -> Result<(), ApiError> {
        let loc = self.locations.get(&ctx.location.directory).await?;
        if self.shell_owner.lock().get(id) != Some(&loc.key) || self.shell().get(id).is_err() {
            return Err(ApiError::shell_not_found(id));
        }
        Ok(())
    }

    /// `config.worktree.directory` of the project's canonical location.
    async fn worktree_parent(&self, project_id: &str) -> Option<String> {
        let project = oc_core::project::get(&self.app.bus, project_id).ok().flatten()?;
        let canonical = project.get("canonical")?.as_str()?.to_string();
        let loc = self.locations.get(&canonical).await.ok()?;
        oc_config::fold::worktree_directory(&loc.location.entries, &self.app.global.home, std::path::Path::new(&canonical))
            .map(|p| p.display().to_string())
    }

    /// The location's VCS service: the provider its registry selects (plugin `vcs` transforms
    /// included) over the location's scope.
    async fn vcs(&self, ctx: &RequestContext) -> Result<oc_core::vcs::Vcs, ApiError> {
        let loc = self.locations.get(&ctx.location.directory).await?;
        let registry = self.app.plugins.vcs(&loc.key, loc.vcs()).await;
        Ok(oc_core::vcs::Vcs { provider: registry.selected(loc.vcs()), scope: oc_core::vcs::Scope::of(&loc.location) })
    }

    /// The worktree strategies of a project's canonical checkout location (booted first, like
    /// upstream's `Worktree.load`).
    async fn worktree_strategies(&self, project_id: &str) -> oc_core::worktree::Strategies {
        if let Some(dir) = oc_core::worktree::project_worktree(&self.app.bus, project_id)
            && std::path::Path::new(&dir).is_dir()
        {
            let _ = self.locations.get(&dir).await;
        }
        oc_core::worktree::strategies(&self.app.plugins, &self.app.bus, project_id).await
    }

    /// `awaitPlugins` (server/src/location.ts): `Plugin.awaitActivation` for at most 5 seconds, then
    /// `503 ServiceUnavailableError`.
    async fn await_plugins(&self, loc: &crate::locations::Loc) -> Result<(), ApiError> {
        tokio::time::timeout(std::time::Duration::from_secs(5), self.app.plugins.await_activation(&loc.key))
            .await
            .map_err(|_| ApiError::service_unavailable("Location plugins did not finish activating", Some("plugin".into())))
    }

    /// The location's web search service with its config `websearch` selection applied
    /// (`false` disables, a provider id pins; the config plugin runs after plugin transforms).
    fn configure_websearch(&self, loc: &crate::locations::Loc) -> Arc<oc_tools::websearch::WebSearch> {
        let websearch = self.app.plugins.websearch_for(&loc.key).unwrap_or_else(|| self.websearch.clone());
        if let Some(selection) = oc_core::plugins::config_websearch_selection(&loc.location.entries) {
            websearch.set_config_selection(Some(selection));
        }
        websearch
    }

    /// PTYs are per location upstream: a PTY created elsewhere is not found here.
    async fn owned_pty(&self, ctx: &RequestContext, id: &str) -> Result<(), ApiError> {
        let loc = self.locations.get(&ctx.location.directory).await?;
        if self.pty_owner.lock().get(id) != Some(&loc.key) || self.app.ptys.get(id).is_err() {
            return Err(ApiError::pty_not_found(id));
        }
        Ok(())
    }

    async fn mcp_for(&self, ctx: &RequestContext, server: &str) -> Result<Arc<oc_mcp::McpManager>, ApiError> {
        let loc = self.locations.get(&ctx.location.directory).await?;
        self.mcp(&loc).await.ok_or_else(|| mcp_not_found(server))
    }

    /// `requireOwnedForm`: the form must exist and belong to the session.
    fn owned_form(&self, session_id: &str, form_id: &str) -> Result<Value, ApiError> {
        let form = self.app.forms.get(form_id).map_err(|_| ApiError::form_not_found(form_id))?;
        if form.get("sessionID").and_then(Value::as_str) != Some(session_id) {
            return Err(ApiError::form_not_found(form_id));
        }
        Ok(form)
    }

    fn credentials(&self) -> &Arc<dyn oc_ai::credentials::CredentialStore> {
        self.models().integrations.store()
    }

    /// `Plugin.awaitActivation`: building the catalog registers integrations and their methods.
    /// Returns the location's integration registry (upstream `Integration` is location-scoped).
    async fn integrations_ready(&self, ctx: &RequestContext) -> Result<Arc<oc_ai::credentials::Integrations>, ApiError> {
        let loc = self.locations.get(&ctx.location.directory).await?;
        self.catalog(&loc).await?;
        let integrations = self.models().integrations_for(Some(&loc.key));
        // `WellKnownPlugin`: persisted sources contribute command login methods.
        match self.wellknown.entries().await {
            Ok(entries) => oc_ai::credentials::wellknown::register(&integrations, &entries),
            Err(e) => tracing::warn!(error = %e, "failed to load wellknown sources"),
        }
        Ok(integrations)
    }

    async fn integration(
        &self,
        integrations: &oc_ai::credentials::Integrations,
        id: &str,
    ) -> Result<oc_ai::credentials::integration::IntegrationInfo, ApiError> {
        match integrations.get(id).await.map_err(integration_error)? {
            Some(i) => Ok(i),
            None => Err(ApiError::integration_not_found(id)),
        }
    }

    /// `FormLocationMiddleware`: `global` selects the request location.
    async fn form_scope(&self, ctx: &RequestContext, session_id: &str) -> Result<Arc<crate::locations::Loc>, ApiError> {
        if session_id == "global" {
            return self.locations.get(&ctx.location.directory).await;
        }
        Ok(self.session_scope(session_id).await?.1)
    }
}

/// Package targets of an inventory (`plugin.check`/`plugin.update`), deduplicated in order.
fn package_targets(inventory: &[Value]) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for plugin in inventory {
        if plugin.pointer("/source/type").and_then(Value::as_str) == Some("package")
            && let Some(t) = plugin.pointer("/source/target").and_then(Value::as_str)
            && !out.iter().any(|o| o == t)
        {
            out.push(t.to_string());
        }
    }
    out
}


//! The session runner (`core/session/runner/llm.ts`, `context.ts`, `model-request.ts`) and the
//! execution driver (`core/session/execution.ts`).

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;
use parking_lot::Mutex;
use tokio_util::sync::CancellationToken;

use oc_ai::types::{ContentPart, GenerationOptions, HttpOptions, Message as LlmMessage, SystemPart, ToolChoice, ToolDefinition, ToolEntry};
use oc_ai::{Client, LLMRequest};
use oc_schema::event::{InterruptReason, SessionEvent};
use oc_schema::message::{CompactionStatus, ToolState};
use oc_schema::types::{Effect, Metadata, ModelRef, SessionError, SessionInfo};
use oc_schema::Message;
use oc_tools::tool::{DefinitionContext, ToolConfig, ToolContext, ToolErrorKind};
use oc_tools::{Registry, Snapshot};

use crate::agent::{Agent, Agents};
use crate::app::App;
use crate::bus::{Bus, Durable1};
use crate::coordinator::{DrainError, Driver, Exit, Reason};
use crate::inbox::{self, Scope};
use crate::instructions::{self, Source};
use crate::location::Location;
use crate::models::Models;
use crate::permission::evaluate;
use crate::retry::{Policy, MAX_STEPS_PROMPT};
use crate::step::{self, Outcome, StepError, ToolCall, ToolRun, ToolRunner};

pub const CONTINUE_AFTER_INCOMPLETE_STREAM: &str =
    "The previous response was interrupted. Continue from where you left off without repeating completed content.";

pub struct Engine {
    pub app: Arc<App>,
    pub models: Models,
    pub client: Client,
    pub registry: Registry,
    pub shell: Arc<oc_tools::shell::service::ShellService>,
    locations: Mutex<HashMap<String, Arc<Location>>>,
    snapshots: Mutex<HashMap<String, Arc<oc_git::Snapshots>>>,
    mcps: tokio::sync::Mutex<HashMap<String, Arc<oc_mcp::manager::McpManager>>>,
    titles: Mutex<std::collections::HashSet<String>>,
    /// Config watch task per location directory.
    watches: Mutex<HashMap<String, tokio::task::JoinHandle<()>>>,
    me: std::sync::Weak<Engine>,
}

pub struct Selected {
    pub session: SessionInfo,
    pub location: Arc<Location>,
    pub agents: Agents,
    pub agent: Agent,
    pub permissions: Vec<oc_schema::types::PermissionRule>,
    pub snapshot: Snapshot,
    pub sources: Vec<Source>,
    pub skills: Vec<oc_config::sources::Skill>,
    pub mcp: Arc<oc_mcp::manager::McpManager>,
}

pub struct Loaded {
    pub sel: Selected,
    pub resolved: oc_ai::catalog::ResolvedModel,
    pub messages: Vec<Message>,
    pub initial: String,
}

fn agent_not_found(id: &str) -> anyhow::Error {
    crate::errors::StepFailed(SessionError::new("unknown", format!("Agent not found: \"{id}\""))).into()
}

impl Engine {
    pub fn new(app: Arc<App>, services: &oc_tools::tools::Services) -> Arc<Engine> {
        let mut registry = Registry::default();
        oc_tools::tools::register_builtins(&mut registry, services);
        let models = Models::new(app.db.clone(), app.bus.clone(), oc_util::version::CHANNEL, &app.version);
        models.set_plugins(app.plugins.clone());
        app.plugins.add_websearch(services.websearch.clone());
        Arc::new_cyclic(|me| Engine {
            app,
            models,
            client: Client::new(),
            registry,
            shell: services.shell.clone(),
            locations: Mutex::new(HashMap::new()),
            snapshots: Mutex::new(HashMap::new()),
            mcps: tokio::sync::Mutex::new(HashMap::new()),
            titles: Mutex::new(Default::default()),
            watches: Mutex::new(HashMap::new()),
            me: me.clone(),
        })
    }

    /// Install this engine as the app's execution driver.
    pub fn install(self: &Arc<Self>) {
        let driver: Arc<dyn Driver> = Arc::new(ExecutionDriver { engine: self.clone() });
        if self.app.coordinator.set(crate::coordinator::Coordinator::new(driver)).is_ok() {
            crate::plan::spawn(self.app.clone());
            self.app.plugins.set_tool_source(Arc::new(RunnerTools(Arc::downgrade(self))));
            // Plugin MCP transforms changed: reconcile the location's servers.
            let mut changes = self.app.plugins.subscribe_changes();
            let me = Arc::downgrade(self);
            tokio::spawn(async move {
                loop {
                    let change = match changes.recv().await {
                        Ok(c) => c,
                        Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                        Err(_) => break,
                    };
                    if change.domain != "mcp" {
                        continue;
                    }
                    let Some(engine) = me.upgrade() else { break };
                    let Some(location) = engine.locations.lock().get(&change.key).cloned() else { continue };
                    let key = location.directory.display().to_string();
                    let mcp = engine.mcps.lock().await.get(&key).cloned();
                    if let Some(mcp) = mcp {
                        mcp.set_servers(crate::mcp::servers(&engine.app.plugins, &location).await).await;
                    }
                }
            });
            // Hourly retention sweeps (`tool-output.ts`, `shell.ts`).
            let data = self.app.global.data.clone();
            tokio::spawn(async move {
                let mut tick = tokio::time::interval(std::time::Duration::from_secs(3600));
                loop {
                    tick.tick().await;
                    let d = data.clone();
                    let _ = tokio::task::spawn_blocking(move || {
                        oc_tools::tool_output::cleanup(&d);
                        oc_tools::shell::service::ShellService::cleanup(&d);
                    })
                    .await;
                }
            });
        }
    }

    pub async fn location(&self, directory: &str) -> anyhow::Result<Arc<Location>> {
        if let Some(l) = self.locations.lock().get(directory).cloned() {
            return Ok(l);
        }
        let loc = Arc::new(Location::load(&self.app.global, &self.app.bus, std::path::Path::new(directory)).await?);
        crate::reference::refresh(&self.app, crate::reference::list(&loc, &self.app.global));
        let loc = {
            let mut locations = self.locations.lock();
            // A concurrent load may have won; keep one location per key.
            locations.entry(directory.to_string()).or_insert(loc).clone()
        };
        self.app.plugins.ensure(&loc);
        self.watch(directory, &loc);
        Ok(loc)
    }

    /// Watch the location's config sources; changes (debounced) reload it.
    fn watch(&self, directory: &str, location: &Location) {
        if oc_config::discovery::filewatcher_disabled() || self.watches.lock().contains_key(directory) {
            return;
        }
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let mut watch = match crate::config_watch::ConfigWatch::new(tx) {
            Ok(w) => w,
            Err(error) => {
                tracing::warn!(error = %error, "config watcher unavailable");
                return;
            }
        };
        watch.reconcile(location.sources.watch_plan());
        let me = self.me.clone();
        let key = directory.to_string();
        let handle = tokio::spawn(async move {
            while crate::config_watch::debounced(&mut rx).await.is_some() {
                let Some(engine) = me.upgrade() else { break };
                match engine.reload_location(&key).await {
                    Ok(location) => watch.reconcile(location.sources.watch_plan()),
                    Err(error) => tracing::error!(directory = %key, error = %error, "failed to reload config"),
                }
            }
        });
        self.watches.lock().insert(directory.to_string(), handle);
    }

    /// A location scope closed (`location.shutdown`): reject its pending permissions, cancel its forms (including
    /// MCP elicitations of its servers), shut its MCP servers down and drop the cached location.
    pub async fn close_location(&self, directory: &str) {
        let location = self.locations.lock().remove(directory);
        if let Some(handle) = self.watches.lock().remove(directory) {
            handle.abort();
        }
        let canonical = location.as_ref().map(|l| l.directory.display().to_string()).unwrap_or_else(|| directory.to_string());
        let in_location = |session: &str| {
            self.app.sessions.get(session).is_ok_and(|s| s.location.directory == directory || s.location.directory == canonical)
        };
        self.app.permissions.reject_where(in_location);
        self.app.forms.cancel_where(in_location);
        self.app.model_transport.close_where(in_location);
        let mcp = self.mcps.lock().await.remove(&canonical);
        if let Some(mcp) = mcp {
            mcp.shutdown().await;
        }
        self.app.plugins.close(directory).await;
        self.models.close_location(directory);
    }

    /// `Config.reload()`: re-run discovery and load; when the entries or compatibility roots changed, replace the
    /// cached location, reconfigure its MCP servers and publish `config.updated`.
    pub async fn reload_location(&self, directory: &str) -> anyhow::Result<Arc<Location>> {
        let previous = self.locations.lock().get(directory).cloned();
        let next = Location::load(&self.app.global, &self.app.bus, std::path::Path::new(directory)).await?;
        if let Some(previous) = &previous
            && previous.entries == next.entries
            && previous.sources.compatibility() == next.sources.compatibility()
        {
            return Ok(previous.clone());
        }
        let next = Arc::new(next);
        crate::reference::refresh(&self.app, crate::reference::list(&next, &self.app.global));
        self.locations.lock().insert(directory.to_string(), next.clone());
        self.app.plugins.ensure(&next);
        let key = next.directory.display().to_string();
        let mcp = self.mcps.lock().await.get(&key).cloned();
        if let Some(mcp) = mcp {
            mcp.set_servers(crate::mcp::servers(&self.app.plugins, &next).await).await;
        }
        self.app.bus.publish(
            "config.updated",
            serde_json::json!({}),
            Some(oc_schema::types::LocationRef { directory: directory.to_string(), workspace_id: None }),
        );
        Ok(next)
    }

    /// The MCP manager for a location (started lazily, events forwarded to the bus).
    pub async fn mcp(&self, location: &Location) -> Arc<oc_mcp::manager::McpManager> {
        let key = location.directory.display().to_string();
        let mut mcps = self.mcps.lock().await;
        if let Some(m) = mcps.get(&key) {
            return m.clone();
        }
        let m = crate::mcp::manager(&self.app.db, &self.app.forms, location, &self.app.version, &self.app.plugins).await;
        let mut rx = m.subscribe();
        let bus = self.app.bus.clone();
        let loc = oc_schema::types::LocationRef { directory: key.clone(), workspace_id: None };
        tokio::spawn(async move {
            while let Ok(ev) = rx.recv().await {
                use oc_mcp::types::McpEvent as E;
                let (E::ToolsChanged { server } | E::ResourcesChanged { server } | E::StatusChanged { server } | E::PromptsChanged { server }) = &ev;
                let data = serde_json::json!({"server": server});
                bus.publish(ev.event_type(), data, Some(loc.clone()));
            }
        });
        mcps.insert(key, m.clone());
        m
    }

    /// Git snapshots for a session location (`Snapshot` service), honoring `snapshots: false`.
    pub fn snapshotter(&self, loaded: &Loaded) -> Option<Arc<dyn step::Snapshotter>> {
        if oc_config::fold::snapshots(&loaded.sel.location.entries) == Some(false) {
            return None;
        }
        let project = &loaded.sel.location.project;
        if !matches!(project.vcs, Some(crate::project::Vcs::Git { .. })) {
            return None;
        }
        let dir = loaded.sel.session.location.directory.clone();
        let snaps = self
            .snapshots
            .lock()
            .entry(dir.clone())
            .or_insert_with(|| {
                Arc::new(oc_git::Snapshots::new(
                    oc_git::Git::new(),
                    oc_git::snapshot::SnapshotLocation {
                        data: self.app.global.data.clone(),
                        project_id: project.id.clone(),
                        project_directory: project.directory.clone(),
                        directory: PathBuf::from(&dir),
                        vcs: Some("git".into()),
                    },
                ))
            })
            .clone();
        Some(Arc::new(GitSnapshots(snaps)))
    }

    pub fn invalidate_locations(&self) {
        self.locations.lock().clear();
    }

    /// `context.select`
    pub async fn select(&self, session_id: &str) -> anyhow::Result<Selected> {
        let session = self.app.sessions.get(session_id)?;
        let location = self.location(&session.location.directory).await?;
        self.app.plugins.await_activation(&location.key).await;
        let agents = self.app.plugins.agents(&location, &self.app.global.home).await;
        *self.app.agents.write() = agents.clone();
        let agent_id = session.agent.clone();
        let agent = agents.select(agent_id.as_deref()).cloned().ok_or_else(|| agent_not_found(agent_id.as_deref().unwrap_or("build")))?;
        let permissions = crate::permission::merge(&[&agent.permissions, session.permissions.as_deref().unwrap_or(&[])]);
        let mcp = self.mcp(&location).await;
        // `mcpTools.flush`: let pending servers finish their first connection attempt.
        for server in mcp.servers() {
            if matches!(server.status, oc_mcp::types::Status::Pending) {
                let _ = mcp.wait_ready(&server.name).await;
            }
        }
        let registry = self.base_registry(&location.key, &mcp);
        let registry = self.app.plugins.tools(&location.key, registry).await;
        let snapshot = registry.snapshot(&permissions);
        let skills = self.app.plugins.skills(&location, &self.app.global, &self.app.version).await;
        let mut sources = self.sources(&session, &location, &permissions, &skills)?;
        // MCP guidance comes first ("most to least shared").
        let tools = mcp.tools();
        let summaries: Vec<oc_mcp::naming::InstructionSummary> = mcp
            .instructions()
            .into_iter()
            .filter_map(|(server, text)| {
                let server_tools: Vec<_> = tools.iter().filter(|t| t.server == server).collect();
                let codemode = server_tools.first().is_none_or(|t| t.codemode != Some(false));
                let visible = server_tools
                    .iter()
                    .any(|t| evaluate(&t.prefixed_name(), "*", &permissions).effect != Effect::Deny)
                    && (!codemode || evaluate("execute", "*", &permissions).effect != Effect::Deny);
                visible.then_some(oc_mcp::naming::InstructionSummary { server, instructions: text, codemode })
            })
            .collect();
        sources.insert(0, instructions::mcp_source(summaries));
        let references = self.app.plugins.references(&location, &self.app.global).await;
        sources.insert(1, crate::reference::source(&references));
        // CodeMode catalog is the most shared source.
        sources.insert(0, crate::codemode::instruction_source(&snapshot));
        Ok(Selected { session, location, agents, agent, permissions, snapshot, sources, skills, mcp })
    }

    /// The native tool registry of a location: built-ins plus its MCP tools.
    pub fn base_registry(&self, key: &str, mcp: &oc_mcp::manager::McpManager) -> Registry {
        let mut registry = self.registry.clone();
        // Built-in tool plugins removed by configuration (`-opencode.tool.read`, `-*`, ...).
        if let Some(enabled) = self.app.plugins.enabled(key) {
            for (id, tool) in registry.list() {
                if crate::plugins::tool_plugin(tool.as_ref()).is_some_and(|p| !enabled.iter().any(|e| e == p)) {
                    registry.remove(&id);
                }
            }
        }
        for t in mcp.tools() {
            let _ = registry.add(Arc::new(oc_tools::tools::mcp::McpTool::new(oc_tools::tools::mcp::McpToolInfo {
                server: t.server.clone(),
                name: t.name.clone(),
                description: t.description.clone(),
                input_schema: t.input_schema.clone(),
                output_schema: t.output_schema.clone(),
                codemode: t.codemode != Some(false),
            })));
        }
        registry
    }

    fn sources(
        &self,
        session: &SessionInfo,
        location: &Location,
        permissions: &[oc_schema::types::PermissionRule],
        all_skills: &[oc_config::sources::Skill],
    ) -> anyhow::Result<Vec<Source>> {
        let g = &self.app.global;
        let skills: Vec<instructions::SkillSummary> = all_skills
            .iter()
            .filter(|s| evaluate("skill", &s.id, permissions).effect != Effect::Deny)
            .filter(|s| s.autoinvoke != Some(false))
            .filter_map(|s| {
                Some(instructions::SkillSummary { id: s.id.clone(), name: s.name.clone(), description: s.description.clone()? })
            })
            .collect();
        let discovery = crate::discovery::discover(&crate::discovery::DiscoveryInput {
            directory: std::path::Path::new(&session.location.directory),
            project_directory: &location.project.canonical,
            home: &g.home,
            global_config: &g.config,
            project: oc_config::discovery::Options::from_env().project != Some(false),
            global: true,
        });
        let mut list = vec![
            instructions::skill_source(skills),
            instructions::discovery_source(discovery),
            instructions::date_source(),
            instructions::environment_source(
                &session.location.directory,
                &location.project.canonical.display().to_string(),
                matches!(location.project.vcs, Some(crate::project::Vcs::Git { .. })),
                &g.tmp.display().to_string(),
            ),
        ];
        let entries: Vec<(String, Option<String>)> = self.app.db.with(|c| {
            let mut s = c.prepare("SELECT key, value FROM instruction_entry WHERE session_id = ? ORDER BY key")?;
            let r = s.query_map([&session.id], |r| Ok((r.get(0)?, r.get(1)?)))?.collect();
            r
        })?;
        for (key, value) in entries {
            list.push(instructions::entry_source(&key, value.and_then(|v| serde_json::from_str(&v).ok())));
        }
        Ok(list)
    }

    /// `context.load`
    pub async fn load(&self, sel: Selected) -> anyhow::Result<Loaded> {
        let catalog = self.models.catalog_at(&sel.location.entries, Some(&sel.location.key)).await?;
        let config_model = oc_config::fold::model(&sel.location.entries).map(|m| ModelRef {
            id: m.model.clone(),
            provider_id: m.provider_id.clone(),
            variant: m.variant.clone(),
        });
        let resolved = self
            .models
            .resolve_for_session(Some(&sel.location.key), &catalog, &sel.session.id, sel.session.model.as_ref(), config_model.as_ref())
            .await
            .map_err(|e| crate::errors::StepFailed(SessionError::new("provider.no-route", e.to_string())))?;
        let messages = history_for_runner(&self.app.bus, &sel.session.id)?;
        let initial = crate::instruction_state::initial(&self.app.bus, &sel.session.id, &sel.sources)?;
        Ok(Loaded { sel, resolved, messages, initial })
    }

    async fn prepare_context(&self, session_id: &str) -> anyhow::Result<Selected> {
        let sel = self.select(session_id).await?;
        crate::instruction_state::prepare(&self.app.bus, &sel.sources, session_id)?;
        Ok(sel)
    }

    fn definition_context(&self, loaded: &Loaded) -> DefinitionContext {
        let subagents: Vec<oc_tools::tool::SubagentSummary> = loaded
            .sel
            .agents
            .items
            .values()
            .filter(|a| a.mode != crate::agent::Mode::Primary && !a.hidden)
            .filter(|a| evaluate("subagent", &a.id, &loaded.sel.agent.permissions).effect != Effect::Deny)
            .map(|a| oc_tools::tool::SubagentSummary { id: a.id.clone(), description: a.description.clone() })
            .collect();
        DefinitionContext {
            model_id: Some(loaded.resolved.model.id.clone()),
            // The shell tool always describes the compatible shell (core/src/tool/plugin/shell.ts context
            // hook: `description(ShellSelect.name(compatibleShell))`), not only a configured one.
            shell: Some(oc_tools::shell::select::resolve(
                oc_tools::shell::select::Priority::Compat,
                oc_config::fold::shell(&loaded.sel.location.entries),
                Some(&self.app.global.bin),
            )),
            subagents: Some(subagents),
            websearch_disabled: false,
        }
    }

    /// Build the primary request (`baseTranscript` + `request.primary`).
    pub fn request(&self, loaded: &Loaded, step_limit: bool) -> (LLMRequest, Vec<String>) {
        let ctx = self.definition_context(loaded);
        let defs = loaded.sel.snapshot.definitions(&ctx, crate::codemode::execute_definition(&loaded.sel.snapshot));
        let names: Vec<String> = defs.iter().map(|d| d.name.clone()).collect();
        let name_refs: Vec<&str> = names.iter().map(String::as_str).collect();
        let model = &loaded.resolved;
        let model_ref = model.model_ref();
        let system = crate::prompt::system_parts(
            loaded.sel.agent.system.as_deref(),
            &name_refs,
            &crate::prompt::ModelIdentity { id: &model.model.id, name: &model.model.name, provider_id: &model.model.provider_id },
            &loaded.initial,
        );
        let key = model.language_model.route.provider_metadata_key.clone().unwrap_or_else(|| model.language_model.provider.clone());
        let mut messages = crate::lower::to_llm_messages(&loaded.messages, &model_ref, Some(&key));
        if step_limit {
            messages.push(LlmMessage::assistant(vec![ContentPart::text(MAX_STEPS_PROMPT)]));
        }
        crate::prompt::unsupported_parts(&mut messages, &model.capabilities().input, &model.model.provider_id);
        crate::prompt::bound_images(&mut messages);
        let mut req = LLMRequest::new(model.language_model.clone());
        req.system = system.into_iter().map(SystemPart::text).collect();
        req.messages = messages;
        req.tools = defs
            .into_iter()
            .map(|d| ToolEntry::Tool(ToolDefinition::new(d.name, d.description, d.input_schema)))
            .collect();
        if step_limit {
            req.tool_choice = Some(ToolChoice::from_str("none"));
        }
        let estimate = crate::compaction::estimate_prompt(loaded, &req.tools);
        let max = crate::prompt::output_limit(model.output_limit().max(0) as u64, model.context_limit().max(0) as u64, false, Some(estimate));
        req.generation = Some(GenerationOptions { max_tokens: Some(max as f64), ..Default::default() });
        self.session_http(&loaded.sel.session, &mut req);
        (req, names)
    }

    /// The per-session request headers and prompt cache key every model request of a session carries
    /// (`SessionModelRequest.prepare`: primary, compaction, title and generate requests alike).
    pub(crate) fn session_http(&self, s: &SessionInfo, req: &mut LLMRequest) {
        let affinity = s.parent_id.clone().or_else(|| s.fork.as_ref().map(|f| f.session_id.clone())).unwrap_or_else(|| s.id.clone());
        let mut headers = indexmap::IndexMap::new();
        headers.insert("x-opencode-session-id".into(), s.id.clone());
        if let Some(p) = &s.parent_id {
            headers.insert("x-opencode-parent-session-id".into(), p.clone());
        }
        headers.insert("x-session-affinity".into(), affinity.clone());
        headers.insert("X-Session-Id".into(), affinity.clone());
        if let Some(p) = &s.parent_id {
            headers.insert("x-parent-session-id".into(), p.clone());
        }
        // `App.useragent`: opencode/<channel>/<version>/<app name> (core/src/app.ts).
        headers.insert(
            "User-Agent".into(),
            format!("opencode/{}/{}/{}", oc_util::version::CHANNEL, self.app.version, oc_util::version::ARTIFACT),
        );
        headers.insert("x-opencode-project".into(), s.project_id.clone());
        headers.insert("x-opencode-session".into(), affinity.clone());
        headers.insert("x-opencode-client".into(), oc_util::version::ARTIFACT.into());
        req.http = Some(HttpOptions { headers: Some(headers), ..Default::default() });
        let re = regex::Regex::new(r"^ses_[0-9a-f]{64}$").unwrap();
        req.prompt_cache_key = Some(if re.is_match(&affinity) { affinity[4..].to_string() } else { affinity });
    }

    /// Runs the `session.context` (and `session.model.request`) plugin hooks on a primary request.
    pub async fn shape_primary(&self, loaded: &Loaded, req: &mut LLMRequest) -> anyhow::Result<crate::plugin_request::Shaped> {
        let s = &loaded.sel.session;
        crate::plugin_request::shape(
            &self.app.plugins,
            &loaded.sel.location.key,
            crate::plugin_request::Kind::Primary,
            &s.id,
            &loaded.sel.agent.id,
            &loaded.resolved.model_ref(),
            req,
            None,
        )
        .await
        .map_err(|e| anyhow::anyhow!("{e}"))
    }

    fn tool_runner(self: &Arc<Self>, loaded: &Loaded, assistant: &str, renamed: Option<HashMap<String, String>>) -> Arc<dyn ToolRunner> {
        let entries = &loaded.sel.location.entries;
        // The location's web search registry with its config selection (`opencode.config.websearch`).
        if let Some(selection) = crate::plugins::config_websearch_selection(entries)
            && let Some(websearch) = self.app.plugins.websearch_for(&loaded.sel.location.key)
        {
            websearch.set_config_selection(Some(selection));
        }
        let config = ToolConfig {
            tool_output_max_lines: oc_config::fold::tool_output(entries).and_then(|t| t.max_lines).map(|v| v as usize),
            tool_output_max_bytes: oc_config::fold::tool_output(entries).and_then(|t| t.max_bytes).map(|v| v as usize),
            portable_shell_scanner: Some(oc_config::fold::portable_shell_scanner(entries)),
            subagent_depth: Some(oc_config::fold::subagent_depth(entries)),
            shell: oc_config::fold::shell(entries).map(str::to_string),
            channel: oc_util::version::CHANNEL.into(),
            ..Default::default()
        };
        // The tools advertised in this request: a call to a known tool that was not offered (e.g. `patch`
        // for models that edit with `edit`/`write`) fails with "Tool is not available for this request"
        // (core/src/tool.ts Snapshot.execute `input.definitions`).
        let ctx = self.definition_context(loaded);
        let advertised: HashMap<String, String> = renamed.unwrap_or_else(|| {
            loaded
                .sel
                .snapshot
                .definitions(&ctx, crate::codemode::execute_definition(&loaded.sel.snapshot))
                .into_iter()
                .map(|d| (d.name.clone(), d.name))
                .collect()
        });
        Arc::new(LocalTools {
            key: loaded.sel.location.key.clone(),
            engine: self.clone(),
            advertised,
            snapshot: loaded.sel.snapshot.clone(),
            config: Arc::new(config),
            session: loaded.sel.session.clone(),
            agent: loaded.sel.agent.id.clone(),
            message_id: assistant.into(),
            project: loaded.sel.location.project.clone(),
            mcp: loaded.sel.mcp.clone(),
            skills: Arc::new(
                loaded
                    .sel
                    .skills
                    .iter()
                    .map(|s| oc_tools::host::SkillInfo {
                        id: s.id.clone(),
                        name: s.name.clone(),
                        description: s.description.clone(),
                        path: s.path.clone(),
                        content: s.content.clone(),
                    })
                    .collect(),
            ),
        })
    }

    /// `drain`
    pub async fn drain(self: &Arc<Self>, session_id: &str, mut force: bool, promotable: Scope, cancel: CancellationToken) -> Result<(), DrainError> {
        let bus = &self.app.bus;
        let locks = &self.app.sessions.locks;
        let mut continuing = false;
        let mut step: u32 = 1;
        let mut entering = true;
        if !force {
            let Some(pending) = inbox::next_promotable(bus, session_id, Scope::Input)? else { return Ok(()) };
            if promotable == Scope::Steer && pending.delivery == oc_schema::event::Delivery::Queue && !pending.is_control() {
                return Ok(());
            }
        }
        settle_stale(bus, session_id)?;
        loop {
            if cancel.is_cancelled() {
                return Err(DrainError::Interrupted);
            }
            // ---- advanceToStep ----
            let ready = loop {
                let pending = {
                    let lock = locks.get(session_id);
                    let _g = lock.lock().await;
                    let next = inbox::next_promotable(bus, session_id, if entering || !continuing { Scope::Input } else { Scope::Steer })?;
                    if let Some(n) = &next {
                        if n.kind == "compaction" {
                            bus.commit(vec![
                                Durable1::from(SessionEvent::InboxDelivered { session_id: session_id.into(), inbox_id: n.id.clone() }),
                                Durable1::from(SessionEvent::CompactionStarted {
                                    session_id: session_id.into(),
                                    reason: oc_schema::message::CompactionReason::Manual,
                                    recent: String::new(),
                                    input_id: Some(n.id.clone()),
                                }),
                            ])?;
                        } else if n.kind == "move" {
                            self.app.model_transport.close(session_id);
                            let p = &n.payload;
                            bus.commit(vec![
                                Durable1::from(SessionEvent::InboxDelivered { session_id: session_id.into(), inbox_id: n.id.clone() }),
                                Durable1::from(SessionEvent::Moved {
                                    session_id: session_id.into(),
                                    location: serde_json::from_value(p["location"].clone())?,
                                    project_id: p["projectID"].as_str().unwrap_or_default().into(),
                                    subpath: p["subpath"].as_str().map(str::to_string),
                                }),
                            ])?;
                        }
                    }
                    next
                };
                if !continuing && pending.as_ref().map(|p| p.delivery) != Some(oc_schema::event::Delivery::Steer) {
                    entering = true;
                    step = 1;
                }
                if let Some(p) = &pending {
                    if p.kind == "move" {
                        self.invalidate_locations();
                        force = false;
                        continue;
                    }
                    if p.kind == "compaction" {
                        let input_id = p.id.clone();
                        let fail = |error: SessionError| {
                            bus.commit_one(SessionEvent::CompactionFailed {
                                session_id: session_id.into(),
                                reason: oc_schema::message::CompactionReason::Manual,
                                error,
                                input_id: Some(input_id.clone()),
                                cost: None,
                                tokens: None,
                            })
                        };
                        let result = async {
                            let sel = self.select(session_id).await?;
                            let loaded = self.load(sel).await?;
                            self.compact(&loaded, crate::compaction::Trigger::Manual { input_id: input_id.clone() }, cancel.clone()).await
                        }
                        .await;
                        match result {
                            Ok(_) => {}
                            Err(e) if e.downcast_ref::<crate::compaction::Interrupted>().is_some() || cancel.is_cancelled() => {
                                fail(SessionError::new("aborted", "Compaction cancelled"))?;
                                return Err(DrainError::Interrupted);
                            }
                            Err(e) => {
                                fail(crate::errors::unknown(&e))?;
                            }
                        }
                        force = false;
                        continue;
                    }
                }
                if !force && !continuing && (pending.is_none() || (pending.as_ref().unwrap().delivery == oc_schema::event::Delivery::Queue && promotable == Scope::Steer)) {
                    return Ok(());
                }
                let selected = self.prepare_context(session_id).await?;
                let scope = if entering && !continuing { promotable } else { Scope::Steer };
                let Some(promoted) = inbox::promote(bus, locks, session_id, scope).await? else { continue };
                if promoted > 0
                    && selected.session.parent_id.is_none()
                    && crate::title::is_untitled(&selected.session)
                    && self.spawn_title(session_id)
                {
                    // Upstream's forked title fiber gets its turn at the step's next yield point, before the
                    // step's model context is loaded. A task spawned from a tokio worker waits in that worker's
                    // LIFO slot (not stealable) until this task yields, which can be only once the step's request
                    // is out; yield once so the title starts first.
                    tokio::task::yield_now().await;
                }
                if promoted > 0 {
                    step = 1;
                }
                break self.load(selected).await?;
            };
            continuing = self.run_step(ready, step, cancel.clone()).await?;
            step += 1;
            force = false;
            entering = false;
        }
    }

    /// `FiberMap.run(titles, sessionID, title.generate(sessionID), { onlyIfMissing: true })`; false when one
    /// already runs.
    fn spawn_title(self: &Arc<Self>, session_id: &str) -> bool {
        if !self.titles.lock().insert(session_id.to_string()) {
            return false;
        }
        let this = self.clone();
        let id = session_id.to_string();
        tokio::spawn(async move {
            if let Err(e) = this.generate_title(&id).await {
                tracing::warn!(sessionID = %id, error = %e, "title generation failed");
            }
            this.titles.lock().remove(&id);
        });
        true
    }

    /// `runStep`
    async fn run_step(self: &Arc<Self>, first: Loaded, step: u32, cancel: CancellationToken) -> Result<bool, DrainError> {
        let session_id = first.sel.session.id.clone();
        let mut assistant = oc_schema::id::message();
        let mut retry = Policy::default();
        let mut initial = Some(first);
        let mut recover_continuation = true;
        let mut recover_overflow = true;
        loop {
            let loaded = match initial.take() {
                Some(l) => l,
                None => {
                    let sel = self.prepare_context(&session_id).await?;
                    self.load(sel).await?
                }
            };
            match self.compact(&loaded, crate::compaction::Trigger::Auto, cancel.clone()).await {
                Ok(crate::compaction::CompactOutcome::Failed(e)) => return Err(DrainError::Failed(crate::errors::StepFailed(e).into())),
                Ok(crate::compaction::CompactOutcome::Completed) => {
                    assistant = oc_schema::id::message();
                    continue;
                }
                Ok(crate::compaction::CompactOutcome::Skipped) => {}
                Err(e) if e.downcast_ref::<crate::compaction::Interrupted>().is_some() => return Err(DrainError::Interrupted),
                Err(e) => return Err(DrainError::Failed(e)),
            }
            let step_limit = loaded.sel.agent.steps.is_some_and(|s| step >= s);
            let (mut request, _) = self.request(&loaded, step_limit);
            let shaped = self.shape_primary(&loaded, &mut request).await.map_err(DrainError::Failed)?;
            let key = loaded.sel.location.key.clone();
            // `webSocket: "session"` + `model.transport === "websocket"` (model-request.ts): the session's socket
            // carries the step when the route offers one; HTTP hooks still wrap any HTTP fallback.
            let web_socket = (loaded.resolved.transport.as_deref() == Some("websocket")).then(|| {
                let interceptor = crate::plugin_request::ws_hooks(
                    &self.app.plugins,
                    &key,
                    crate::plugin_request::Kind::Primary,
                    &session_id,
                    &loaded.sel.agent.id,
                    &loaded.resolved.model_ref(),
                );
                let idle = crate::model_transport::IdleTimeout::from_chunk_timeout(loaded.resolved.chunk_timeout);
                self.app.model_transport.bind(&session_id, interceptor, idle)
            });
            let retry_hook: Option<step::RetryHook> = self.app.plugins.has_hook(&key, "session.retry", Some(&loaded.resolved.model.provider_id)).then(|| {
                let plugins = self.app.plugins.clone();
                let (session, agent, model) = (session_id.clone(), loaded.sel.agent.id.clone(), loaded.resolved.model_ref());
                let hook: step::RetryHook = Arc::new(move |error, attempt, decision| {
                    let (plugins, key, session, agent, model) = (plugins.clone(), key.clone(), session.clone(), agent.clone(), model.clone());
                    Box::pin(async move { crate::plugin_request::retry(&plugins, &key, &session, &agent, &model, &error, attempt, decision).await })
                });
                hook
            });
            let cost: Vec<crate::usage::Cost> = loaded
                .resolved
                .cost()
                .iter()
                .map(|c| crate::usage::Cost {
                    tier_size: c.tier.as_ref().map(|t| t.size.max(0) as u64),
                    input: c.input,
                    output: c.output,
                    cache_read: c.cache.read,
                    cache_write: c.cache.write,
                })
                .collect();
            let resolved = &loaded.resolved;
            let key = resolved.language_model.route.provider_metadata_key.clone().unwrap_or_else(|| resolved.language_model.provider.clone());
            let outcome = step::attempt(step::Input {
                bus: self.app.bus.clone(),
                client: self.client.clone(),
                session_id: session_id.clone(),
                assistant_message_id: assistant.clone(),
                agent: loaded.sel.agent.id.clone(),
                model: resolved.model_ref(),
                provider_metadata_key: key,
                cost,
                request,
                tool_choice_none: step_limit,
                tools: self.tool_runner(&loaded, &assistant, shaped.tools),
                retry_hook,
                http_hook: shaped.http,
                web_socket,
                snapshots: self.snapshotter(&loaded),
                recover_continuation,
                retry: &mut retry,
                cancel: cancel.clone(),
                location_closed: Arc::new(|| false),
                recover_overflow: {
                    let this = self.clone();
                    let loaded_ref = &loaded;
                    let cancel = cancel.clone();
                    let enabled = recover_overflow;
                    Box::new(move || {
                        Box::pin(async move {
                            enabled
                                && matches!(
                                    this.compact(loaded_ref, crate::compaction::Trigger::Overflow, cancel).await,
                                    Ok(crate::compaction::CompactOutcome::Completed)
                                )
                        })
                    })
                },
            })
            .await;
            match outcome {
                Ok(Outcome::Completed { needs_continuation }) => return Ok(needs_continuation),
                Ok(Outcome::Retry { error, decision }) => {
                    crate::retry::wait(&self.app.bus, &session_id, &assistant, decision, error).await?;
                }
                Ok(Outcome::Continue { error, decision }) => {
                    crate::retry::wait(&self.app.bus, &session_id, &assistant, decision, error).await?;
                    self.app.bus.commit_one(SessionEvent::Synthetic {
                        session_id: session_id.clone(),
                        text: CONTINUE_AFTER_INCOMPLETE_STREAM.into(),
                        description: None,
                        metadata: None,
                    })?;
                    assistant = oc_schema::id::message();
                }
                Ok(Outcome::Compacted) => {
                    recover_overflow = false;
                    assistant = oc_schema::id::message();
                }
                Ok(Outcome::RecoverFull) => recover_continuation = false,
                Err(StepError::Interrupted) => return Err(DrainError::Interrupted),
                Err(StepError::Failed(f)) => return Err(DrainError::Failed(f.into())),
                Err(StepError::Defect(e)) => return Err(DrainError::Failed(e)),
            }
            if cancel.is_cancelled() {
                return Err(DrainError::Interrupted);
            }
        }
    }
}

/// `SessionHistory.entriesForRunner`: messages from the latest completed (local) compaction.
pub fn history_for_runner(bus: &Bus, session: &str) -> anyhow::Result<Vec<Message>> {
    let all = bus.db().session_messages(session)?;
    let start = all
        .iter()
        .rposition(|(_, m)| matches!(m, Message::Compaction(c) if matches!(&c.status, CompactionStatus::Completed { provider_context: None, .. })))
        .unwrap_or(0);
    Ok(all.into_iter().skip(start).map(|(_, m)| m).collect())
}

/// `settleStaleCompactions` + `settleStaleToolCalls`
fn settle_stale(bus: &Bus, session: &str) -> anyhow::Result<()> {
    let msgs = bus.db().session_messages(session)?;
    for (_, m) in msgs.iter().rev() {
        if let Message::Compaction(c) = m {
            if let CompactionStatus::Running { reason, .. } = &c.status {
                bus.commit_one(SessionEvent::CompactionFailed {
                    session_id: session.into(),
                    reason: *reason,
                    error: SessionError::new("compaction.interrupted", "Compaction was interrupted"),
                    input_id: Some(c.id.clone()),
                    cost: None,
                    tokens: None,
                })?;
            }
        }
    }
    for (_, m) in &msgs {
        let Message::Assistant(a) = m else { continue };
        for item in &a.content {
            let oc_schema::message::AssistantContent::Tool { id, name, executed, state, .. } = item else { continue };
            let metadata = match state {
                ToolState::Running { metadata, .. } => Some(metadata.clone()),
                ToolState::Streaming { .. } => None,
                _ => continue,
            };
            let child = (name == "subagent")
                .then(|| metadata.as_ref().and_then(|m| m.get("sessionID")).and_then(|v| v.as_str()).map(str::to_string))
                .flatten();
            let message = match child {
                Some(c) => format!("Tool execution interrupted: {name} (sessionID: {c})"),
                None => format!("Tool execution interrupted: {name}"),
            };
            bus.commit_one(SessionEvent::ToolFailed {
                session_id: session.into(),
                assistant_message_id: a.id.clone(),
                id: id.clone(),
                error: SessionError::new("aborted", message),
                content: None,
                metadata: metadata.filter(|m| !m.is_empty()),
                executed: executed.unwrap_or(false),
                result_state: None,
            })?;
        }
    }
    Ok(())
}

struct LocalTools {
    engine: Arc<Engine>,
    /// Plugin location key.
    key: String,
    /// Advertised tool name → registered name (session hooks may rename or remove tools).
    advertised: HashMap<String, String>,
    snapshot: Snapshot,
    config: Arc<ToolConfig>,
    session: SessionInfo,
    agent: String,
    message_id: String,
    project: crate::project::Resolved,
    skills: Arc<Vec<oc_tools::host::SkillInfo>>,
    mcp: Arc<oc_mcp::manager::McpManager>,
}

#[async_trait]
impl ToolRunner for LocalTools {
    async fn run(&self, call: ToolCall, progress: step::Progress, cancel: CancellationToken) -> ToolRun {
        let host: Arc<dyn oc_tools::ToolHost> = Arc::new(crate::host::CoreHost {
            app: self.engine.app.clone(),
            skills: self.skills.clone(),
            mcp: Some(self.mcp.clone()),
            location: Some(self.key.clone()),
        });
        let ctx = ToolContext {
            session_id: self.session.id.clone(),
            message_id: self.message_id.clone(),
            call_id: call.id.clone(),
            agent: self.agent.clone(),
            directory: PathBuf::from(&self.session.location.directory),
            worktree: self.project.canonical.clone(),
            project_id: self.session.project_id.clone(),
            workspace_id: self.session.location.workspace_id.clone(),
            global: self.engine.app.global.clone(),
            cancel,
            progress: Arc::new(move |m: Metadata| progress(m)),
            config: self.config.clone(),
            host,
        };
        let data = self.engine.app.global.data.clone();
        let (max_lines, max_bytes) = (self.config.tool_output_max_lines.unwrap_or(2000), self.config.tool_output_max_bytes.unwrap_or(51200));
        let hooks: Arc<dyn oc_tools::registry::ExecuteHooks> = Arc::new(CoreHooks {
            plan: (self.agent == "plan").then(|| PlanHooks { dir: crate::agent::plan_dir(&self.engine.app.global.home).display().to_string() }),
            plugins: self.engine.app.plugins.clone(),
            key: self.key.clone(),
        });
        // `Snapshot.execute` (core/src/tool.ts): before hook, availability for this request, then the tool.
        let (mut name, mut input) = (call.name.clone(), call.input);
        if let Some(tool) = self.snapshot.direct_tool(&name) {
            input = tool.prepare_input(input);
        }
        let execute_enabled = self.snapshot.execute_enabled;
        let result = match hooks.before(&mut name, &mut input, &ctx).await {
            Err(e) => Err(e),
            Ok(()) => {
                let requested = self.advertised.get(&name).cloned();
                let known = self.snapshot.direct_tool(&name).is_some() || (execute_enabled && name == oc_codemode::execute_tool::NAME);
                if requested.is_none() && known {
                    Err(oc_tools::ToolError::new(format!("Tool is not available for this request: {name}")))
                } else {
                    let real = requested.unwrap_or(name);
                    if real == oc_codemode::execute_tool::NAME && execute_enabled {
                        let tool = crate::codemode::ExecuteTool(self.snapshot.clone(), hooks.clone());
                        oc_tools::registry::execute_tool(&tool, &real, input, ctx, hooks.as_ref()).await
                    } else if let Some(tool) = self.snapshot.direct_tool(&real) {
                        oc_tools::registry::execute_tool(tool.as_ref(), &real, input, ctx, hooks.as_ref()).await
                    } else {
                        Err(oc_tools::ToolError::new(format!(
                            "No tool named \"{real}\" is currently available. Please use a tool from the available tool list."
                        )))
                    }
                }
            }
        };
        match result {
            Ok(result) => {
                let result = oc_tools::tool_output::truncate(result.clone(), max_lines, max_bytes, &data).unwrap_or(result);
                ToolRun::Ok { content: result.content, metadata: result.metadata }
            }
            Err(e) => match e.kind {
                ToolErrorKind::Failure => {
                    let s = e.to_session_error();
                    ToolRun::Failed { error: SessionError::new(s.kind, s.message), metadata: e.metadata.clone() }
                }
                ToolErrorKind::Declined | ToolErrorKind::LocationClosed => ToolRun::Declined { question: None },
                ToolErrorKind::QuestionDismissed => ToolRun::Declined { question: Some(e.message.clone()) },
                ToolErrorKind::Interrupted => ToolRun::Interrupted,
            },
        }
    }
}

/// `tool.list` for plugins: the location's tool domain after every transform.
struct RunnerTools(std::sync::Weak<Engine>);

impl crate::plugins::ToolSource for RunnerTools {
    fn tool_value(&self, key: String) -> futures::future::BoxFuture<'static, Option<serde_json::Value>> {
        let engine = self.0.upgrade();
        Box::pin(async move {
            let engine = engine?;
            let location = engine.location(&key).await.ok()?;
            let mcp = engine.mcp(&location).await;
            let registry = engine.app.plugins.tools(&key, engine.base_registry(&key, &mcp)).await;
            Some(crate::plugins::tool_value(&registry))
        })
    }
}

struct GitSnapshots(Arc<oc_git::Snapshots>);

#[async_trait]
impl step::Snapshotter for GitSnapshots {
    async fn capture(&self) -> Option<String> {
        self.0.capture().await.map(|t| t.0)
    }
    async fn files(&self, from: &str, to: &str) -> Option<Vec<String>> {
        self.0.files(&oc_git::TreeId(from.into()), &oc_git::TreeId(to.into())).await.ok()
    }
}

/// `tool.execute.before/after`: the native internal hooks (plan agent) in their upstream order,
/// then the plugin callbacks.
struct CoreHooks {
    plan: Option<PlanHooks>,
    plugins: Arc<crate::plugins::Plugins>,
    key: String,
}

#[async_trait]
impl oc_tools::registry::ExecuteHooks for CoreHooks {
    async fn before(&self, tool: &mut String, input: &mut serde_json::Value, ctx: &ToolContext) -> Result<(), oc_tools::ToolError> {
        if !self.plugins.has_hook(&self.key, "tool.execute.before", None) {
            return Ok(());
        }
        let event = serde_json::json!({
            "tool": tool,
            "sessionID": ctx.session_id,
            "agent": ctx.agent,
            "messageID": ctx.message_id,
            "id": ctx.call_id,
            "input": input,
        });
        match self.plugins.trigger(&self.key, "tool.execute.before", event).await {
            Ok(out) => {
                if let Some(t) = out.get("tool").and_then(|t| t.as_str()) {
                    *tool = t.to_string();
                }
                *input = out.get("input").cloned().unwrap_or(serde_json::Value::Null);
                Ok(())
            }
            Err(oc_plugin::HookError::Tool { message, metadata }) => {
                let mut e = oc_tools::ToolError::new(message);
                if let Some(m) = metadata.and_then(|m| m.as_object().cloned()) {
                    e = e.with_metadata(m.into_iter().collect());
                }
                Err(e)
            }
            Err(oc_plugin::HookError::Failed(message)) => Err(oc_tools::ToolError::new(message)),
        }
    }

    async fn after(
        &self,
        tool: &str,
        input: &serde_json::Value,
        result: &mut Result<oc_tools::ToolResult, oc_tools::ToolError>,
        ctx: &ToolContext,
    ) {
        if let Some(plan) = &self.plan {
            plan.after(tool, input, result, ctx).await;
        }
        if !self.plugins.has_hook(&self.key, "tool.execute.after", None) {
            return;
        }
        let mut event = serde_json::json!({
            "tool": tool,
            "sessionID": ctx.session_id,
            "agent": ctx.agent,
            "messageID": ctx.message_id,
            "id": ctx.call_id,
            "input": input,
        });
        match result {
            Ok(r) => {
                let mut v = serde_json::Map::new();
                if let Some(o) = &r.output {
                    v.insert("output".into(), o.clone());
                }
                v.insert("content".into(), serde_json::to_value(&r.content).unwrap_or_default());
                if let Some(m) = &r.metadata {
                    v.insert("metadata".into(), serde_json::Value::Object(m.clone().into_iter().collect()));
                }
                event["status"] = "completed".into();
                event["result"] = serde_json::Value::Object(v);
            }
            Err(e) if e.kind == ToolErrorKind::Failure => {
                let mut v = serde_json::json!({ "message": e.message });
                if let Some(m) = &e.metadata {
                    v["metadata"] = serde_json::Value::Object(m.clone().into_iter().collect());
                }
                event["status"] = "error".into();
                event["error"] = v;
            }
            // Declines, interrupts and defects are not `Tool.Error`s: no hook.
            Err(_) => return,
        }
        let out = match self.plugins.trigger(&self.key, "tool.execute.after", event.clone()).await {
            Ok(out) => out,
            Err(error) => {
                *result = Err(oc_tools::ToolError::new(error.to_string()));
                return;
            }
        };
        match result {
            Ok(r) => {
                if out.get("result") == event.get("result") {
                    return;
                }
                let next = out.get("result").cloned().unwrap_or_default();
                r.output = next.get("output").cloned();
                r.content = next.get("content").cloned().and_then(|c| serde_json::from_value(c).ok()).unwrap_or_default();
                r.metadata = next.get("metadata").and_then(|m| m.as_object()).map(|m| m.clone().into_iter().collect());
            }
            Err(e) => {
                if out.get("error") == event.get("error") {
                    return;
                }
                let next = out.get("error").cloned().unwrap_or_default();
                let mut error = oc_tools::ToolError::new(next.get("message").and_then(|m| m.as_str()).unwrap_or_default());
                if let Some(m) = next.get("metadata").and_then(|m| m.as_object()) {
                    error = error.with_metadata(m.clone().into_iter().collect());
                }
                *e = error;
            }
        }
    }
}

/// `tool.execute.after` for the plan agent (`plugin/plan.ts`).
struct PlanHooks {
    dir: String,
}

#[async_trait]
impl oc_tools::registry::ExecuteHooks for PlanHooks {
    async fn after(
        &self,
        tool: &str,
        _input: &serde_json::Value,
        result: &mut Result<oc_tools::ToolResult, oc_tools::ToolError>,
        _ctx: &ToolContext,
    ) {
        if !matches!(tool, "edit" | "write" | "patch") {
            return;
        }
        if let Err(e) = result {
            let blocked = matches!(&e.cause, Some(oc_tools::tool::ErrorCause::PermissionRejected(m)) if m.starts_with("Permission denied:"));
            if blocked {
                *e = oc_tools::ToolError::new(format!("Cannot use {tool} to modify files outside the Plan directory: {}", self.dir));
            }
        }
    }
}

// ---------- execution driver (execution.ts) ----------

struct ExecutionDriver {
    engine: Arc<Engine>,
}

#[async_trait]
impl Driver for ExecutionDriver {
    async fn started(&self, key: &str) {
        let r = self.engine.app.bus.commit_with(
            vec![SessionEvent::ExecutionStarted { session_id: key.into() }.into()],
            |c, _, _| Ok(crate::session::Sessions::claim(c, key)?),
        );
        if let Err(e) = r {
            tracing::error!(sessionID = key, error = %e, "Failed to publish Session execution lifecycle");
        }
    }

    async fn drain(&self, key: &str, force: bool, scope: Scope, cancel: CancellationToken) -> Result<(), DrainError> {
        let r = self.engine.drain(key, force, scope, cancel).await;
        if let Err(DrainError::Failed(e)) = &r {
            tracing::error!(sessionID = key, error = %e, "Failed to drain Session");
        }
        r
    }

    async fn settled(&self, key: &str, exit: &Exit, reason: Option<Reason>) {
        let release = |c: &rusqlite::Connection, _: usize, _: i64| -> anyhow::Result<()> { Ok(crate::session::Sessions::release(c, key)?) };
        let bus = &self.engine.app.bus;
        let r = match exit {
            Exit::Success => bus.commit_with(vec![SessionEvent::ExecutionSucceeded { session_id: key.into() }.into()], release),
            Exit::Interrupted => {
                let reason = match reason.unwrap_or(Reason::Shutdown) {
                    Reason::User => InterruptReason::User,
                    Reason::Shutdown => InterruptReason::Shutdown,
                    Reason::Inactivity => InterruptReason::Inactivity,
                };
                if reason != InterruptReason::Shutdown {
                    self.engine.app.jobs.cancel_session(key);
                }
                let ev = vec![SessionEvent::ExecutionInterrupted { session_id: key.into(), reason }.into()];
                if reason == InterruptReason::Shutdown { bus.commit(ev) } else { bus.commit_with(ev, release) }
            }
            Exit::Failure(e) => {
                let error = crate::errors::unknown(e);
                bus.commit_with(vec![SessionEvent::ExecutionFailed { session_id: key.into(), error }.into()], release)
            }
        };
        if let Err(e) = r {
            tracing::error!(sessionID = key, error = %e, "Failed to publish Session execution lifecycle");
        }
    }
}


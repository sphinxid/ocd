# oc-backend

`oc_server::Backend` implemented over `oc_core::App` (spec 06). Every HTTP operation maps to a
core service, a direct storage/git/config/oc-ai call that mirrors the upstream service, or a
clearly marked `TODO(core)` stub that answers with the upstream-appropriate error or empty state.

## Public API

```rust
// Bind + serve; marks the server ready. `runner` (oc_core::runner::Engine) enables execution,
// the model catalog, integrations and title generation; it is installed as the app's
// coordinator when none is set yet.
oc_backend::run_server(app: Arc<App>, runner: Option<Arc<oc_core::runner::Engine>>, opts: ServerOptions)
    -> anyhow::Result<oc_server::RunningServer>;
// run_server that also returns the engine (CLI: engine.prepare_pty_handoff() on stop/restart).
oc_backend::start(app, runner, opts) -> anyhow::Result<(RunningServer, Arc<Engine>)>;
// Same application without binding (tests / embedding).
oc_backend::router(app, runner, &opts) -> anyhow::Result<(axum::Router, oc_server::RouterHandle)>;
// The engine itself (implements oc_server::Backend).
oc_backend::Engine::new(app)                 // own Models (oc_core::models::Models) for catalog/credentials
oc_backend::Engine::with_runner(runner)      // shares runner.models and runner.location()
engine.environment(session_id)               // app.environments entry (PUT /api/session/{id}/environment)
```

## Modules

| module | contents |
|---|---|
| `engine` | `Engine` (app, optional runner, models, location cache, websearch, PTY ownership), session-scope helper (`SessionNotFoundError`/`LocationNotFoundError`), wake/is_active via `app.coordinator` |
| `locations` | `LocationServiceMap`: per-directory `Loc { location, snapshots }` keyed by the canonical directory (`oc_core::location::canonical_directory`: normalized, no trailing separator, like `LocationServiceMap.canonical`), single-flight build, `Project.activate` (≤ 1/min, `project.updated`), plugin activation (`Plugins::ensure`; with configured plugins the nine boot `*.updated` notifications wait for it, then the plugin domains are rebuilt), `evict` (`location.shutdown`, plugin location closed), `reload` (503 `{service:"location"}`), `is_live` (loaded or building, for form polls), configuration reloads (rebuilt services announce provider/model twice and only changed gated domains) |
| `notify` | `State.changes` projections of the gated catalog domains (integration, agent, command, skill, websearch, reference, plugin) for boot baselines, configuration reloads and plugin domain changes |
| `plugins` | `BackendApi`, the server's `oc_plugin::PluginApi`: plugin `ctx.*` API calls run through an in-process router (the same handlers, validation, errors and envelopes as HTTP; upstream `ClientApi` names mapped to operation ids, path/query/body split from the OpenAPI document, the plugin's location as `x-opencode-directory`, `session.create` defaulting `location`); `integration.connection.*` over oc-ai `Integrations`; host-service methods and storage via `oc_core::plugins::CoreApi`; `rs:command.<name>` handles run native commands. `install(engine, opts)` is called by `start`/`router` |
| `sessions` | `session.*` handlers: create/remove (children, interrupt, `Bus.remove`)/update/fork (instruction state + entries)/prompt (attachments via `oc_core::attachment`, skills)/synthetic/compact (input id honoured)/interrupt (resume semantics)/inbox/move/messages/context/revert stage-clear-commit (oc-git `Snapshots`)/diff (`SessionDiff.turn`)/instruction entries/skill activation/commands (built-ins + config templates with `` !`cmd` ``) |
| `store` | SQL equivalents of `SessionStore.list/messages/message/context`, instruction entries, saved permissions, `Bus.remove`, event-log reads |
| `catalog` | skills (via `oc_core::plugins::Plugins::skills`) and the command registry (`init`, `review`, MCP prompts → plugin `command` transforms → config commands; plugin commands are `CommandDef::Plugin(handle)`) |
| `fs` + `fuzzy` | `fs.read` (confined, realpath), `fs.list` (dirs first, locale order), `fs.write`, `fs.find` (gitignore-aware walk + fuzzy ranking) |
| `stats` | `SessionStats.get` |
| `shells` | `ShellSelect.list` (unix) |
| `persistent` | persistent-PTY attachments → oc-server WebSocket attachments |
| `backend` | the `Backend` trait impl |
| `util` | serde conversion helpers, lifecycle-conflict detection |

Conversion: core/schema/oc-ai values are serialized and decoded into the generated API types, so
responses always match `openapi.json` key order and shapes (a mismatch is a defect → 500).

## Implemented

* **server/location/debug/migration**: `server.info` (default), `location.get`, `location.reload`,
  `debug.location.list/evict`, `experimental.migration.v1.status` (`oc_core::v1_import::Importer::global()
  .status()`: `required | running{progress} | error | completed`).
* **v1 → v2 history import**: `start` launches `oc_core::v1_import` on a background thread right after the
  server is ready (`start_v1_import`, upstream `V1Migration.layer`, `server/src/routes.ts:188`). Nothing is
  gated while it runs (one DB transaction per session, resumable via the `migration.v1-v2` kv cursor); the
  upstream TUI only shows a corner progress badge. `router()` (tests) does not start it.
* **agent**: `agent.list` (default first), `agent.get` (per-location config fold).
* **session**: `list` (directory/project+subpath/search/parentID/anchor paging), `create`, `get`,
  `remove`, `update` (title/metadata/permissions; empty title → `runner.generate_title`), `fork`,
  `switchAgent`, `switchModel`, `move` (destination checks, move inbox item / direct move when the
  source is gone), `prompt`, `command`, `experimental.session.skill`, `synthetic`, `compact`,
  `experimental.session.wait`, `revert.stage/clear/commit`, `context`, `diff`, `inbox.list/cancel/
  update`, instruction entries `list/put/remove` (413 over 256 KiB), `log` (replay + `log.synced`
  + follow), `interrupt`, `background` (no jobs → no-op), `message.get`, `message.list`,
  `environment`, `view`, `active`, `shell` (runner `session_shell`, awaited like upstream; 500
  without a runner), `background` (`app.jobs.background_all` + the synthetic notice + wake).
  Subagent config commands create a child session and start/background a `SubagentJob` through
  `oc_core::host::CoreHost`.
* **generate**: `session.generate` (`runner.session_generate`; NotFound → 404, else 503
  `{service:"session generation"}`), `experimental.generate.text` and its deprecated alias
  `experimental.generate.textLegacy` (`runner.generate_text` at the global config directory after
  its plugins activate; `Generate.Error` → 404 `ModelNotFoundError`, 422 `UnsupportedModelError`, 400
  `InvalidRequestError{field?}`, 502 `ProviderRequestError`, 503 `ServiceUnavailableError`; one-shot
  retries). `experimental.generate.image` / `.decisions` validate (unique question names) and resolve
  the ref like upstream, but image and decision models are not ported, so they end in 404/422.
* **disabled features**: the schedule routes answer as with `experimental.schedules` off (404
  `SessionNotFoundError` / `ScheduleNotFoundError`, else 403 `ForbiddenError`; 400 `{kind:"schedule"}` when
  the session's directory is gone), `plugin.gui` is always 404 `PluginNotFoundError`.
* **media**: `experimental.media.get` serves `blob` rows some message still references
  (`oc_storage::stored::read_blob`); `media:` prompt attachments re-admit stored bytes.
* **transfer**: `experimental.session.export` (`oc_core::transfer::export`, `sanitize`),
  `experimental.session.import` (`body.location ?? cwd`; 409 `Session already exists: <id>`).
* **mcp** (runner `engine.mcp(&location)`): `mcp.list` (`{name,status,integrationID}`),
  `mcp.resource.catalog`, `experimental.mcp.add/remove/connect/disconnect` (404
  `McpServerNotFoundError`); MCP prompts are slash commands `<server>:<prompt>` (arguments parsed
  like upstream, `MCP prompt not found: …` → 500 `CommandExecutionError`). Without a runner the
  lists are empty and server operations 404.
* **pty** (`app.ptys`): `pty.list/create/get/update/remove/connect`, default shell
  `ShellSelect.resolve(Config, config shell, bin)`, PTYs owned by the location that created them;
  connect queues live output until the replay is sent, `Exited` → close 4404 "session exited".
* **stats** (`stats` module, `SessionStats.get` port): `experimental.session.stats` — 31-day query
  windows, fork filter, prompts/steps/tokens/cost, per-model usage (sorted by total tokens),
  tool summary/detail (p50 durations), compaction usage from the event log, activity by local
  date in the requested IANA time zone (chrono-tz), longest streak; `Invalid time zone: <tz>` /
  `Stats range must end after it starts` → 400.
* **worktree**: `worktree.create` / `worktree.remove` / `worktree.refresh` (`oc_core::worktree`, parent directory from
  the canonical location's `worktree.directory`; errors → 404 ProjectNotFoundError or 400
  `WorktreeError{name,data:{message,forceRequired?}}` with upstream messages).
* **reference.list** (`oc_core::reference::list`), **config.shells** (`shells` module, port of
  `ShellSelect.list` over `/etc/shells`), **websearch.providers/query** (`oc_tools::websearch::WebSearch`
  over `oc_core::services::WebSearchKv`, location `websearch` config selection, upstream error kinds).
* **session.environment** stores into core's `app.environments` (shared with the shell tool).
* **persistent PTY** (`persistent` module over `oc-persistent-pty`): one global `PersistentPty`
  built on first use (at boot via `start`/`run_server`, adopting `OPENCODE_PTY_HANDOFF` or
  `Engine::set_persistent_pty_options`), `persistent-pty.added/removed` forwarded to the bus;
  `server.info` reports `capabilities.persistentPty`; terminal list/create/read, persistent-pty
  shutdown/handoff/get/update (resize then get)/remove/snapshot, connect-token (server-owned, uses
  `get`) and connect (attachments mapped to oc-server's `PersistentPtyAttachment`: output → binary,
  resized → JSON + base64 checkpoint, other events → `to_ws_json`; framed/legacy input → `input` /
  `control`). `Unavailable` → 503 `{service:"opencode-pty"}`, `NotFound` → 404 `PtyNotFoundError`,
  `InvalidCwd` → 400 `InvalidRequestError{message: "cwd is not a directory: <cwd>", kind: "invalid_cwd", field: "cwd"}`.
  `Engine::prepare_pty_handoff()` is the hook for the CLI's service stop/restart (spec 01 §7.5).
* **shell** (`ShellService`: the runner's, else `oc_core::services::default_services` whose sink
  publishes `shell.created/exited/deleted` to the bus): `shell.list` (running only), `shell.create`
  (cwd defaults to the location, shell from `select::resolve(Config, …)`, session environment from
  `metadata.sessionID` else the process env + `TERM`/`OPENCODE_TERMINAL`, output under the location's
  project id; failures are defects), `shell.get`, `shell.output` (byte cursor/limit), `shell.remove`
  (missing → 204). Commands belong to the location that created them.
* **wellknown**: `experimental.integration.wellknown.add` (`oc_ai` `WellKnown` over the `kv` table;
  failure → 400 `well_known_discovery`); persisted sources register their `login` command method
  whenever integrations are loaded.
* **MCP integrations**: loading integrations also registers the location's remote MCP servers as
  `mcp_<hash>` OAuth integrations (`oc_core::mcp::register_integrations`, upstream `mcp/index.ts`
  `register`); a `credential.switched` for one reconnects its server.
* **config reload**: `experimental.config.update` reloads every loaded location through
  `runner.reload_location`; `Locations` drops its cached services on `config.updated` and the rebuild
  publishes `provider.updated`/`model.updated` (twice) plus the gated domains whose projection changed.
* **prompt attachments**: png/jpeg/gif/webp/bmp images are normalized with `oc_tools::image::normalize`
  under the location's `media.image` limits (`oc_config::fold::media_image`; HEIC/AVIF and other types go
  unchanged); data attachments keep the client's `source`; failures → 400 `{field:"files"}`.
* **location-free session routes**: `session.update` (except title regeneration), `fork`,
  `switchAgent`, `switchModel`, `experimental.session.wait`, `revert.commit`, instruction entries,
  `interrupt` and `background` only look the session up; `project.list`/`update` and
  `permission.saved.update`/`remove` take no location; a form poll of an unloaded location answers `[]`.
  The events these session routes publish (`session.renamed`, `session.metadata.updated`,
  `session.permissions`, `session.archived`/`unarchived`, `session.agent.selected`, `session.model.selected`,
  `session.forked`, `session.revert.committed`, the background notice's inbox events) carry no envelope
  `location` (`oc_core::bus::without_location`); plugins still receive them in the session's location.
* **MCP at boot**: a booted location starts its MCP servers before the `*.updated` burst, so each server's
  `disabled`/`pending` `mcp.status.changed` precedes it, as upstream's MCP State notifies first.
* **location shutdown**: `debug.location.evict`, `vcs.init` and `location.reload` evict through
  `runner.close_location` (pending permissions rejected, forms cancelled, MCP stopped, watcher
  stopped) before publishing `location.shutdown`; `config.updated` only drops cached services.
* **skills** come from `oc_core::skills::list` (built-ins, configured and remote sources,
  compatibility roots). `opencode.browser` adds the built-in `browser` skill after the plugins' skills and
  denies it, with the browser tools, to every agent (plugin-browser/src/skill.ts, connection.ts).
* **restart recovery**: `run_server` calls `runner.resume_suspended()` after the server is ready.
* **forms** (`app.forms`): `session.form.list/create/get/cancel/reply` (ownership check,
  `global` sentinel, upstream tags: Conflict / InvalidRequest{field:"fields"} /
  FormNotFound / FormAlreadySettled / FormInvalidAnswer), `form.list` (forms of sessions at the
  location + `global` forms).
* **permission**: `permission.request.list` (requests of sessions at the location),
  `permission.saved.list/remove`, `session.permission.create/list/get/reply`.
* **event.subscribe**: bus envelopes as JSON; a lagging subscriber ends the stream.
* **model/provider** (oc-ai `Catalog` via `Models::catalog(location.entries)`): `model.list`
  (available, newest first; `?types=` is passed to `Engine::model_list_of(ctx, types)`, chat when absent),
  `model.default` (both wait for plugins like upstream), `provider.list` (available), `provider.get`.
* **usage limits**: `Engine::usage_limits(ctx)` (`oc_core::usage_limits`) answers `usage.limits`
  (`GET /api/experimental/usage/limits`): `None` (`data: null`) unless the location is signed in to OpenCode
  Console, 503 `{service: "opencode"}` when Console can't answer.
* **integration**: `list`, `get`, `connect.key`, `connect.external`, `oauth.connect/status/cancel/
  complete`, `command.connect/status/cancel` (oc-ai `Integrations`, upstream error mapping).
* **credential**: `list/create/update/remove/activate` over `oc_core::models::DbCredentials`.
* **project**: `project.list`, `project.update` (+ `project.updated`); **worktree.list**.
* **fs**: `fs.read`, `fs.list`, `fs.find`, `experimental.fs.write`, `experimental.fs.watch`/`unwatch`
  (`file_watch::FileWatch`, global on the `Engine`: the file must pass `fs.read` confinement (404
  `FileNotFoundError`), then its directory is watched for 24 h (renewed by watching again; 256 per
  location, the soonest-expiring stopped first; swept every 5 min) and each change publishes
  `filesystem.changed {file, event: "change"|"unlink"}` on the location; a watch whose directory
  vanishes is forgotten; disabled by `OPENCODE_FILEWATCHER_DISABLE` → 503 `{service: "filewatcher"}`;
  unwatch is always 204).
* **command.list**, **skill.list**, **config.get**, **experimental.config.update** (`shell`).
* **vcs** (git via `oc_git::GitVcs`): `vcs.init` (git; hg → 503), `vcs.get`, `vcs.base`,
  `vcs.status`, `vcs.branch.list` (limit ≤ 100), `vcs.diff` (review budget, `?untracked=files|folders`).

## Plugins

Server plugins run in oc-plugin's Bun sidecar, driven by `oc_core::plugins::Plugins` (see
`crates/oc-core` and `crates/oc-plugin/README.md`). The backend's part:

* **`plugin.list`**: the location's inventory (built-ins in upstream order, package/local plugins
  in boot order, failures). **`plugin.check`**: package targets checked for updates
  (`Npm::check`: `bun info` for registry specs, `git ls-remote` for Git specs; failures count as
  current), `outdated` set like upstream. **`plugin.update`**: `Npm::update` per target, then the
  location's plugins reload; failures answer upstream's 503.
* **`rpc.call`**: `Plugin.awaitActivation`, then the built-in goal plugin's `opencode.goal` RPC
  (`get {sessionID}` → `{"output": Goal.State | null}`, answered natively unless `-opencode.goal`), then
  the sidecar registry; failures map to upstream's `RpcError` (400) / `RpcInternalError` (500,
  `rpc.internal`/`rpc.invalid_output`); unknown ids answer `rpc.unavailable` / `RPC is unavailable: <id>`.
* **`session.prompt`**: awaits activation and runs the `session.prompt` hook before attachments
  are materialised (the message id is generated first when the hook exists).
* **`session.command`**: awaits activation; plugin commands execute through their handle. `goal`
  (`opencode.goal`, registered after the command plugin's commands and MCP prompts) runs
  `oc_core::runner::Engine::goal_command` and admits its kickoff (a described synthetic message, or a
  prompt when the command carries attachments) or resume continuation; its errors are
  `CommandExecutionError`s with upstream's texts.
* Domain listings (`agent.*`, `command.list`, `skill.list`, `reference.list`, `model.*`,
  `provider.*`, `integration.*`, `websearch.*`) include plugin transforms; `shell.create` runs
  `shell.create.before`.

Deviations: `fs.find` ranking approximates fuzzysort/fff; the runner's own cache is cleared wholesale
on evict/reload; a loaded location whose directory is deleted keeps serving until evicted (upstream
answers 404 `LocationNotFoundError` on its session-location routes); 

## Tests

`cargo test -p oc-backend`: fuzzy-ranking unit tests and `tests/api.rs`, which drives the real
router over an in-memory database and a temp `Global` (sessions + cursor paging, prompt/inbox/
messages/fork, attachments and skills, commands, permissions, filesystem, projects/config/
credentials, instruction entries + session log, forms (incl. a waiting `ask`) and background, export/import, runner-less generate/MCP, PTY lifecycle + attach, shell commands + bus events, config update/reload eviction, wellknown add, prompt image normalization, stats, shells/websearch/references, git worktrees (+ refresh), persistent PTY without a daemon; `persistent_pty_real_daemon` is `#[ignore]` (needs `opencode-pty`), runner-less interrupt/wait/view/revert, move,
git VCS, models/providers/integrations, event feed). `tests/plugins.rs` runs oc-plugin's fixture
plugins in the real Bun sidecar (bun from `OCD_BUN_PATH`/`OCD_TEST_BUN`, else the scratchpad bun;
skipped without one) against the router with the session runner and a mock provider: a plugin tool
offered to the model and executed with `tool.execute.before` rewriting its input; plugin
commands, agents and skills; a rejecting `tool.execute.before`; `permission.evaluate` allowing and
denying `.env` reads; `tool.execute.after`, `session.prompt`, `session.context` and
`shell.create.before`; storage, `ctx.event.subscribe` and `rpc.call`; provider/model/integration/
websearch transforms and a Code Mode plugin tool with hooks on the nested call; built-in plugin
removal without starting Bun; local plugin hot reload. `tests/aisdk.rs` configures oc-plugin's
fixture AI SDK provider package (`aisdk:file://...`) and runs prompts through the AI SDK bridge:
text, reasoning, a tool call and usage; plugin `aisdk.sdk` / `aisdk.language` and `session.http.*`
hooks; a provider HTTP error and a package that fails to initialize.

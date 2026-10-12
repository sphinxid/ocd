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
| `locations` | `LocationServiceMap`: per-directory (verbatim key) `Loc { location, agents, snapshots }`, single-flight build, `Project.activate` (≤ 1/min, `project.updated`), `evict` (`location.shutdown`), `reload` (503 `{service:"location"}`) |
| `sessions` | `session.*` handlers: create/remove (children, interrupt, `Bus.remove`)/update/fork (instruction state + entries)/prompt (attachments via `oc_core::attachment`, skills)/synthetic/compact (input id honoured)/interrupt (resume semantics)/inbox/move/messages/context/revert stage-clear-commit (oc-git `Snapshots`)/diff (`SessionDiff.turn`)/instruction entries/skill activation/commands (built-ins + config templates with `` !`cmd` ``) |
| `store` | SQL equivalents of `SessionStore.list/messages/message/context`, instruction entries, saved permissions, `Bus.remove`, event-log reads |
| `catalog` | skills (via `oc_core::skills`) and the command registry (`init`, `review`, MCP prompts, config commands) |
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
  `debug.location.list/evict`, `experimental.migration.v1.status` (`completed` without a legacy
  `session` table, else the `migration.v1-v2` kv state).
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
  `{service:"session generation"}`), `experimental.generate.text` (`runner.generate_text` at the
  global config directory; ModelSelection → 400, Unavailable → 503 `{service}`).
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
  `control`). `Unavailable` → 503 `{service:"opencode-pty"}`, `NotFound` → 404 `PtyNotFoundError`.
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
* **config reload**: `experimental.config.update` reloads every loaded location through
  `runner.reload_location`; `Locations` drops its cached services on `config.updated`.
* **prompt attachments**: images are normalized with `oc_tools::image::normalize` under the
  location's `media.image` limits (`oc_config::fold::media_image`); failures → 400 `{field:"files"}`.
* **location shutdown**: `debug.location.evict`, `vcs.init` and `location.reload` evict through
  `runner.close_location` (pending permissions rejected, forms cancelled, MCP stopped, watcher
  stopped) before publishing `location.shutdown`; `config.updated` only drops cached services.
* **skills** come from `oc_core::skills::list` (built-ins, configured and remote sources,
  compatibility roots).
* **restart recovery**: `run_server` calls `runner.resume_suspended()` after the server is ready.
* **forms** (`app.forms`): `session.form.list/create/get/cancel/reply` (ownership check,
  `global` sentinel, upstream tags: Conflict / InvalidRequest{field:"fields"} /
  FormNotFound / FormAlreadySettled / FormInvalidAnswer), `form.list` (forms of sessions at the
  location + `global` forms).
* **permission**: `permission.request.list` (requests of sessions at the location),
  `permission.saved.list/remove`, `session.permission.create/list/get/reply`.
* **event.subscribe**: bus envelopes as JSON; a lagging subscriber ends the stream.
* **model/provider** (oc-ai `Catalog` via `Models::catalog(location.entries)`): `model.list`
  (available, newest first), `model.default`, `provider.list` (available), `provider.get`.
* **integration**: `list`, `get`, `connect.key`, `connect.external`, `oauth.connect/status/cancel/
  complete`, `command.connect/status/cancel` (oc-ai `Integrations`, upstream error mapping).
* **credential**: `list/create/update/remove/activate` over `oc_core::models::DbCredentials`.
* **project**: `project.list`, `project.update` (+ `project.updated`); **worktree.list**.
* **fs**: `fs.read`, `fs.list`, `fs.find`, `experimental.fs.write`.
* **command.list**, **skill.list**, **config.get**, **experimental.config.update** (`shell`).
* **vcs** (git via `oc_git::GitVcs`): `vcs.init` (git; hg → 503), `vcs.get`, `vcs.base`,
  `vcs.status`, `vcs.branch.list` (limit ≤ 100), `vcs.diff` (review budget).

## TODO(core) — stubs and what they need from oc-core

| operation(s) | current answer | needs |
|---|---|---|
| prompt plugin hooks (`session.prompt`) | skipped | `PluginHooks` (plugin host) |
| `plugin.*`, `rpc.call` | `[]` / 400 on any target / 400 `RpcError` | plugin host + RPC |

Deviations: `fs.find` ranking approximates fuzzysort/fff; the location cache key is verbatim but
the runner's own cache is cleared wholesale on evict/reload; 

## Tests

`cargo test -p oc-backend`: fuzzy-ranking unit tests and `tests/api.rs`, which drives the real
router over an in-memory database and a temp `Global` (sessions + cursor paging, prompt/inbox/
messages/fork, attachments and skills, commands, permissions, filesystem, projects/config/
credentials, instruction entries + session log, forms (incl. a waiting `ask`) and background, export/import, runner-less generate/MCP, PTY lifecycle + attach, shell commands + bus events, config update/reload eviction, wellknown add, prompt image normalization, stats, shells/websearch/references, git worktrees (+ refresh), persistent PTY without a daemon; `persistent_pty_real_daemon` is `#[ignore]` (needs `opencode-pty`), runner-less interrupt/wait/view/revert, move,
git VCS, models/providers/integrations, event feed).

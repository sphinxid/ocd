# Port state / handoff (last updated 2026-11-11)

**Name:** the Rust port is **ocd** (user: only the app + binary are ocd; everything else must stay backward-compatible with opencode). TUI logo reads "opencode-d" (oc-tui view_helpers LOGO_RIGHT, breakpoints 31/53). `ocd upgrade` installs ocd's own GitHub releases and the TUI shows an update notice (`crates/ocd/src/release.rs`); `uninstall` unchanged. (binary `ocd`, crate `crates/ocd`). Upstream compatibility surfaces (config/data dirs, opencode.json, .opencode/, OPENCODE_* env, HTTP API, x-opencode-* headers, plugin package names) keep the opencode names on purpose.

Goal (user request): an exact behavioral replica of **opencode v2** in Rust — full CLI + server + **Rust TUI**
(user chose "Full CLI + Rust TUI"). Project root: this repository. Working rules for every crate:
`docs/PORTING.md`. Specs: `docs/spec/01..08` (~18k lines, all complete).

## Upstream reference
- Repo `anomalyco/opencode`, tag `v2.3.1`, commit `c769de83007e3584b5b395176c9fcde2d3231e88`, MIT (`LICENSE-opencode`).
  The port started from branch `v2` at `4617210822bdbb31e5749751afbfb4584ccedac8` (2.0.26) and caught up with
  2.1.4 (`docs/UPGRADE-2.1.4.md`), 2.2.2 (`docs/UPGRADE-2.2.2.md`), 2.2.5 (`docs/UPGRADE-2.2.5.md`), 2.2.7
  (`docs/UPGRADE-2.2.7.md`), 2.3.0 (`docs/UPGRADE-2.3.0.md`) and 2.3.1 (`docs/UPGRADE-2.3.1.md`) in batches; the specs in `docs/spec/` still
  cite the 4617210 sources. The 2.2 tags are not descendants of `v2.1.4`: compare trees, not commit logs.
- Read-only checkout (may vanish with the session scratchpad; re-clone if missing):
  `git clone https://github.com/anomalyco/opencode.git && git -C opencode checkout v2.3.1`
  (was at `$OCD_SCRATCH/src/opencode`).
- Toolchain: rustup in `~/.cargo` (Rust 1.99, edition 2024). Always `export PATH=$HOME/.cargo/bin:$PATH`.
  Machine: 8 cores, 13 GB RAM. No git commits (user's global rule: only commit when asked; never add AI attribution).

## Key facts learned (affect design)
- Upstream ships as a **Bun-compiled single binary**, not Node (Node only as launcher shim / optional build).
- Architecture: background HTTP server (~142 endpoints, `assets/openapi.json` is normative) + TUI client over HTTP/SSE.
  Core is **event-sourced**: durable session events → projectors → SQLite tables; `event` table stays empty locally
  (persist=false); `event_sequence` holds per-aggregate seq (first event seq 0).
- DB: `<XDG_DATA_HOME|~/.local/share>/opencode/opencode.db` (release channels), WAL, pragmas in spec 02 §1.3.
  Fresh DB = run `assets/sql/schema.sql` + insert all 62 ids from `assets/sql/migration_ids.txt`.
- Paths are XDG on every OS. Log: `<data>/log/opencode.log`, logfmt, trimmed >50MiB → ~25MiB.
- IDs: 12 hex (48-bit, ts<<12+counter, truncated) + 14 base62; `ses_` descending, others ascending.
- v2 messages have no "parts": assistant `content[]` items (text/reasoning/tool). Stored `data` excludes `id`/`type`.
- MCP tools reach the model by default only via the Code Mode `execute` tool → **oc-codemode is required**.
- The persistent PTY daemon `opencode-pty` (npm `@opencode/pty@0.3.3` since 2.2, `@opencode-ai/pty@0.3.1` in 2.1, 0.2.0 before) is **already Rust** (portable-pty +
  libghostty) — reuse rather than rewrite. `@` file picker uses `fff` (MIT Rust core) — usable directly.
- No LSP client in v2. No Anthropic OAuth in v2. No CLAUDE.md instruction discovery (only AGENTS.md).
- JS/TS user plugins, TUI plugins and `aisdk:` npm provider packages need a JS runtime — **undecided**
  (options: embed deno_core/boa, or bridge to `bun`). Defer until core runs.
- Session loop details (inbox steer/queue, coordinator, drain, step, retries 2s→10s ×10, compaction threshold
  window − max(10%,16k), keep 15k) are in spec 03.

## Assets copied verbatim (`assets/`)
`prompt/system/*.txt` (default.txt = runner system.txt; anthropic/gpt/gpt-astra/kimi/meta/trinity),
`prompt/command/{initialize,review}.txt`, `prompt/agent/{explore,title,summary}.txt`, `skill/{opencode,report}.md`,
`tool/patch.txt`, `themes/` (34 incl. `v2/opencode.json`), `models-dev/snapshot.txt` (5.3 MB),
`openapi.json`, `sql/schema.sql`, `sql/migration_ids.txt`, `sql/migration/*.sql` (49; statements split by
`\n--> statement-breakpoint\n`).

## Crate status
| crate | owner | status |
|---|---|---|
| oc-util | me | DONE: global paths, db/log path, jsonc, `{env:}/{file:}`, logfmt logger + trim, flock (TS-compatible), hash, version. Tests pass. |
| oc-schema | me | DONE: id gen (bit-exact), types (SessionInfo, ModelRef, TokenUsage, Ruleset, ToolContent, attachments…), `Message` enum (11 types, row encode/decode), `SessionEvent` enum (44 durable events, versions, data()). |
| oc-storage | me | DONE (core): open+pragmas+chmod600, migrations (fresh/upgrade/drizzle seed; special cases session-metadata, loose_psylocke alt branch = first 12 stmts, legacy credential import, fk-off), KV, durable commit (`commit_session_event(s)`), full session projector + message updater port, session reads. Tests pass (`tests/projector.rs`). TODO: worktree.resolved projector, session list/pagination per spec 02 §7.8, export/import (v1→v2 data import: done in oc-core `v1_import`). |
| oc-ai | agent | DONE (231 tests; 226 recorded upstream streams decode): ModelsDev+Catalog+resolve→ResolvedModel, Client::stream, all chat protocols, credentials/OAuth (Integrations), retry helpers. Gaps: aisdk:* packages, Responses websocket, non-chat modalities, some AWS/GCP cred sources. See README. |
| oc-tools | agent | DONE (164 tests, ~21k lines): all tools, Registry/Snapshot, ToolHost (~45 async methods, only ask_permission required), tool_output::truncate, scanners (0 mismatches/81,787 upstream cases), html→md, websearch, formatters, ripgrep. Divergences: image re-encode via `image` crate, localeCompare approx. Core must implement ToolHost job methods. See README. |
| oc-codemode | agent | DONE: ~33k lines, full JS interpreter; 1705/1707 upstream replays identical, test262 sample conformant, acorn-identical parser; README lists unsupported JS + known diffs. Wired into oc-core (runner + ExecuteTool). |
| oc-tui | agent | DONE (136 tests): `run(TuiOptions)`, `run_mini(TuiOptions, MiniOptions)`; all screens incl. tab rail, diff viewer, stats, terminal pane (vt100), open/worktrees/pair dialogs, mouse select, comment-preserving cli.json. Mini mode ported (`src/mini/`). Not yet run against a live server. |
| oc-mcp | agent | DONE (44 tests): McpManager (set_servers/tools/call_tool/prompts/resources/auth_start/finish/logout, event channel), hand-rolled JSON-RPC (stdio + streamable HTTP + 2026-07-28 protocol), OAuth. Core implements McpCredentialStore + ElicitationHandler. See README. |
| oc-server | agent | DONE (78 tests): generated types from openapi.json (codegen/gen.py), Backend trait (138 methods, default 500), envelopes/errors/auth/CORS/pairing/SSE/fs.read/PTY ws/startup/web UI/service helpers. `serve(backend, ServerOptions)`. Core must implement Backend. See README. |
| oc-config | agent | DONE (164 tests): schema, normalize (v1→v2), discovery+watch plan, Loader (WellKnown trait), fold helpers (agents/providers/...), markdown sources, template eval, policy, redact, cli.json (CliConfig), jsonc_edit. Core owns watchers/reload/bus. See README. |
| oc-git | agent | DONE (60 tests): Git, Snapshots, RepositoryCache (Kv trait), worktree strategy, file_retention, patch helpers; see README. vcs module DONE (GitVcs: info/base/branches/status/review_diff/init; 74 tests total). Core still owns Vcs service registry/events and the hg provider. Project id derivation + worktree DB rows stay in oc-core. |
| oc-backend | agent | DONE (35 tests): all 138 Backend methods; `oc_backend::start`; persistent PTY; shell.*; wellknown add; config reload eviction; close_location on evict/reload/vcs.init; prompt image normalisation. Open: plugin.*/rpc.call + prompt plugin hooks (plugin host undecided). |
| oc-persistent-pty | agent | DONE (37 tests; 2 ignored real-daemon tests pass): PersistentPty client for npm `opencode-pty` 0.3.1 (pinned hashes, downloads per platform incl. Windows), protocol 7, ownership/handoff, attach/replay/events, ws helpers. Backend wiring requested. |
| oc-core | me | IN PROGRESS. Done+compiling: wildcard, bus (durable commit, broadcast, usage.updated), permission, agent (built-ins, plan reminders), project (id resolution + persist + durable worktree.resolved), instructions (sources/hash/render), inbox (admit/promote/cancel/delivery), coordinator (wake/run/interrupt/await_idle; tests pass), session service (create/prompt reconcile/synthetic/compact/rename/metadata/permissions/switch agent+model/view/claim), discovery (AGENTS.md walk, nested-on-read, skill content block), attachment (data:/file: URIs, dirs, limits, mime sniff), usage (tokens + tiered cost), publisher (LLM events → durable events, 100ms delta batching), lower (history → oc_ai::Message), errors (toSessionError for AIError, token-sharing messages), retry (policy + wait + MAX_STEPS_PROMPT), models (DbCredentials CredentialStore over `credential` table, KvCatalogCache, Models: catalog build from config+integrations, resolve_for_session), step (attempt: stream→publisher, concurrent tools, retry/continue/overflow/declines settlement, step.ended/failed), runner (Engine: select/load/request assembly w/ headers+cache key+max tokens, drain/advanceToStep/runStep, ExecutionDriver with claim/release, LocalTools ToolRunner over oc-tools Snapshot; E2E TEST PASSES: prompt→mock openai-compatible provider→read tool→answer), instruction_state (prepare/initial/preview with blobs; integration test passes), prompt (system.txt render + model-family prompts + identity + output limit + unsupported/bound images), app (App runtime + PermissionContext over DB), host (ToolHost: permissions, sessions, agents, nested instructions, synthetic), location (Loader entries + agent fold). oc-tools/oc-mcp/oc-config now enabled as deps. Deps on agent crates are commented out in oc-core/Cargo.toml until integration. DONE also: title (title.rs; E2E asserts title), snapshots (oc-git via step::Snapshotter, honors `snapshots:false`), skills (skills.rs: builtin opencode/report verbatim + config dirs + .claude/.agents compat; wired into instructions + CoreHost{app, skills}). job (job.rs: Jobs start/wait/block/background/background_all/cancel/cancel_session/pending/complete; ToolHost job_* + subagent_job_start/background/notify; SUBAGENT E2E TEST PASSES). form (form.rs: Forms create/ask/reply/cancel/list + verbatim validation; ToolHost ask_form), shell_cmd (Engine::session_shell for `!`), sessions fork/remove/revert ops, bus Durable1.id for explicit event ids. compaction (compaction.rs, 14 tests; wired: manual in drain, auto before each step, overflow recovery, output limit uses prompt estimate), mcp (mcp.rs: McpCreds over credential table, per-location McpManager, MCP tools in registry snapshot, CoreHost mcp_* methods, events forwarded; `core/mcp-guidance` instruction source), plan (reminder listener + plan-agent edit/write/patch failure rewrite), restart (Engine::resume_suspended), generate (session_generate, generate_text), transfer (export w/ sanitize, import), watch send_replace bugfix, pty (pty.rs: ephemeral PTY via portable-pty, UTF-16 cursors, attach/replay; test passes; App.ptys), services (default_services: DB-backed websearch store, shell events), reference (list/refresh via RepositoryCache, `core/reference-guidance` source), environments (App.environments SessionEnvironment; used by shell tool + `!`), worktree (create/remove/list/refresh + ToolHost worktree_*), MCP elicitation→forms (mcp.rs Elicitation, global-session forms, URL mode + legacy complete), remote URL skill sources (skill_discovery.rs: index.json pull, versioned atomic refresh, once per URL per process), hourly retention sweeps. CodeMode wired (definition + core/codemode source at index 0 + ExecuteTool through hooks; tests/e2e_codemode.rs passes). Config watching (config_watch.rs, notify; Engine::reload_location → config.updated + MCP set_servers; tests/config_watch.rs passes). Restart recovery of persisted background jobs (shell notices, subagent re-run/delivery; tests/restart.rs). Engine::close_location (reject perms/cancel forms/shutdown MCP). Skills precedence fixed (compat before config). NEXT: plugin host (DECIDED 2026-10-12: Bun sidecar, on demand — see Plan below), real-binary E2E once CLI lands. |
| ocd (bin, crates/ocd) | agent | DONE (36 tests): full CLI as a direct port of effect/cli 4.0.1 (goldens vs real framework: help for every command, parse errors, completions); run/serve/service/auth/mcp/models/session/plugin/stats/acp/upgrade; standalone + service modes; PTY handoff via HTTP. Not implemented: `plugin add` (needs plugin sidecar), --wizard. Identity strings say `ocd`; compat surfaces stay opencode. |

Each agent writes `crates/<crate>/README.md` with its public API when done — read those before integrating.
If an agent was lost (session ended), re-launch it with the same scope; prompts are summarized in the table above
and `docs/PORTING.md`; tell it to continue from the existing code in its crate.

## Plan (decided 2026-10-12 with the user)
1. Parity pass: run real upstream opencode v2 and the Rust port side by side on the same inputs; diff HTTP
   responses, SSE events, CLI output. Fix differences.
2. Plugins phase 1: Bun sidecar (downloaded on first use, only when config lists plugins) running server plugins via
   the public `@opencode/plugin` API; bridge tools/commands/hooks/transforms/storage/rpc to the Rust server.
3. Plugins phase 2: AI SDK bridge through the same sidecar (plugin `aisdk.*` hooks + `aisdk:*` provider packages).
4. TUI plugins (Solid/OpenTUI `tui` entrypoint): NOT supported (documented incompatibility).

## Notes for oc-core (already researched)
- Permission (`core/permission.ts`): `evaluate` = last rule where `Wildcard.match(action, rule.action) &&
  match(resource, rule.resource)`, default `{action, resource:"*", effect:"ask"}`. Wildcard: `\`→`/`, regex-escape,
  `*`→`.*`, `?`→`.`, trailing `" .*"` → `( .*)?`, anchored, dotall (case-insensitive on win32).
  Missing agent → `[{*,*,deny}]`. Deny check before saved rules; saved rules (project, effect allow) appended.
  Reply reject (with message → CorrectedError feedback, else Declined) fans out to all other pending of same session;
  "always" saves `save[]` resources then re-evaluates other pending and auto-approves those now allowed.
  BlockedError message = reason ?? `Permission denied: {action}`.
- Agents: `Info.default` permissions `[*,* allow],[read,*.env ask],[read,*.env.* ask],[read,*.env.example allow]`;
  built-ins build/plan/general/explore/compaction/title/summary per spec 03 §5.2 (prompts in assets/prompt/agent).
  Plan: `edit * deny`, `edit {home}/.opencode/plan/* allow`, plan reminders spec 03 §2.8.5.
- Request assembly: system = [agent.system || system.txt with `${OPENCODE_TOOL_GUIDANCE}` lines, identity block
  `# Your Model` at index 1, instruction baseline]; model-family prompt overrides by catalog id substring
  (gpt/gpt+astra override, claude append, kimi, trinity, muse=meta with `{{MODEL_NAME}}`).
- Headers: x-opencode-session-id, x-opencode-parent-session-id, x-session-affinity, X-Session-Id,
  x-parent-session-id, User-Agent, x-opencode-project, x-opencode-session, x-opencode-client.
- Output limit formula, media rules, history lowering: spec 03 §2.8.6–2.8.7.

## Session log
- 2026-11-20: OPENCODE CONSOLE WATCHES AND ARTIFACT PROVIDER PORTED (`opencode-watches.ts` 2.2.7, `opencode-artifacts.ts`
  2.3.1 #63929): `oc_core::console_watches` (the `console_watch` Code Mode tools, one shared event connection per
  workspace over WebSocket or long polls, synthetic-steer wakes with the 5-in-10-minutes and 20-since-prompt limits,
  deleted-session cleanup, the prompt hook, the shell auto-watch that like 2.3.1 reads `exit` where the shell result has
  `exitCode`) and `oc_core::console_artifacts` (the default `console` provider while `x-opencode-artifacts` is sent),
  synced by the runner's `console_features` after each Console config load. Upstream's tests ported against in-process
  fake Consoles (`tests/console_watches.rs` 32, `tests/console_artifacts.rs` 6); parity group `console231` (5
  scenarios, a mock Console) identical against 2.3.1.
- 2026-11-20: OPT-IN SESSION WARMING PORTED (`opencode.warming`, 2.3.1 behaviour): `oc_core::warming` (decision,
  per-location loops, end events from the bus, cache-miss stop), hooked in `plugin_request::shape` for primary,
  compaction and generate requests; pings go through `Engine::session_generate` in the scheduling request's trace.
  Loops stop on session deletion, location close and a restart or disable of the plugin. `fold::warming` now returns
  the latest top-level value. Upstream's 17 plugin tests ported on paused time (`tests/warming.rs`, 3 more) plus an engine test
  against a mock provider; parity group `warming231` (4 scenarios) identical against 2.3.1.
- 2026-11-18: CAUGHT UP WITH 2.3.1 (`docs/UPGRADE-2.3.1.md`): four reviews, seven batches (foundation with the three
  automation migrations, Code Mode, core tools, providers/server, TUI/ACP, automations, process-daemon shells on
  `@opencode/pty` 0.6.0 protocol 9) and fixes for a macOS test race, Windows file names, a Windows stack overflow (large
  futures boxed; 8 MiB main-thread stack on Windows) and two final parity diffs. ocd reports 2.3.1; releases and the
  web UI build from v2.3.1 (`c769de8`). Tests 2,876 passed; side by side 257 of 263 identical. Not ported:
  full_codemode, video generation, the Console artifact provider, automation event triggers, MCP Events.
- 2026-11-18: 2.3.1 CATCH-UP, PROVIDERS AND SERVER (Q3): reasoning replay on OpenAI Chat routes (#64243,
  `openai_chat.rs` `lower_assistant`: a declared field other than `reasoning_details` drops replayed details/opaque,
  `reasoning_details` never gets the text written over the array; `tests/wire_2_3_1.rs` compiles 512 replays with
  2.3.1's own routes under Bun); the OpenAI sign-in read from the stored credential (#64470, `models.rs`
  `openai_sign_in`, no refresh at load) and GPT 6.2 Sol/Astra in `FALLBACK_MODELS` (#64508); readable 500s (#64022:
  `ApiError::Defect` → `UnknownError{message, ref}`, `ApiError::Storage` from `oc_storage::failure` (SQLite primary
  code in the anyhow chain, via oc-backend `util::defect`/`defect_chain`), a panic-catch layer, failed commits roll
  back); the local agent CLI's chat reports run one full check per model without a context size (#64107,
  `claude_agent::Location::checked`); unreachable well-known sources skipped (#62578); models.dev `video` type kept
  (`TYPES`, `CATALOG_PATH`), `xai/videos` bundled, video refs 422 in `generate.rs`, hourly `video_*` blob cleanup
  (`automations::start_blob_cleanup`). Video routes still answer 403 even with the flag on (no video protocols).
  Parity group `providers231` and `claude_new_model_context`.
- 2026-11-18: 2.3.1 CATCH-UP, TUI AND ACP (Q4): the assistant footer drops `· N ttft` (#63759). A permission
  prompt wins over the composer (#64343, `composer_open` = upstream's `composerVisible`): a subagent session shows its
  own permission prompt instead of the Subagents composer, an open composer gives way to an arriving prompt and comes
  back after it, and `active_permission()` for a subagent session reads only that session (a root still gathers its
  descendants'). ACP follows background shells like children (#64262): `TurnState.shells` (shell ID to the inbox ID
  of its notice), `pending()` gates the hand-off and `follow_background`, and the next prompt inherits the shells.
  `run` does not wait on `codemode` jobs after a cancel. Tests in `app/catchup231_tests.rs`, `acp::tests` and
  `tests/acp.rs`; side by side: TUI captures against 2.3.1 and the parity scenario `acp_background_shell_wakes_turn`.
- 2026-11-18: 2.3.1 CATCH-UP, FOUNDATION (Q0; ocd still reports 2.3.0 until the catch-up ends): the three
  migrations `20261116043204_automation`, `…181958_automation_event_position`, `…213417_automation_active_index`
  (byte-equivalent SQL from upstream's `.ts`; 62 ids; `schema.sql` regenerated from 2.3.1's `schema.gen.ts`): live
  prompt schedules without `check`/`remaining_runs` become `aut_…` automations prompting their session, the rest are
  dropped with the `schedule` table. Storage tests port `database-migration.test.ts` (copy/drop rules, in-memory DBs
  never import `auth.json`) and the schema fixture is `schema-2.3.1.txt`, dumped from a 2.1.4 DB the 2.3.1 binary
  migrated; side by side, a 2.3.0 DB with real schedules migrates to the same schema and rows with ocd and with
  upstream, and each opens the other's database. Schedules are retired like upstream: no loop, tools, `/schedule` or
  routes; `oc_core::schedule` stays unwired (its tests run over the 2.3.0 table) as the base of the automations port
  (Q5; the old HTTP/prompter glue is `crates/oc-backend/src/schedules.rs` in git history). `assets/openapi.json` is
  v2.3.1's (173 ops, 518 schemas; `gen.py` adds `AutomationNotFoundError`/`VideoNotFoundError`/`BlobNotFoundError`
  and makes `experimental.blob.get` a hand-written route): `oc-backend/src/automations.rs` answers the automation
  routes like a release build with the flag off (checks, then 403), video 403, and serves `<data>/blobs/<id>`
  (`fsread::serve_blob`). Flags: 2.3.1's registry (`automations` with alias `schedules`, `video_generate`,
  opt-in-only `full_codemode`; `background_compaction`/`shell_timeout_background` gone), `compaction.background`
  (default true) replaces the flag in `compaction.rs` with 2.3.1's rules (no model/instructions invalidation, used
  for overflow, starts past the ceiling). `plugin.list` has `opencode.tool.automation`. Parity group `api231`.
- 2026-11-17: CAUGHT UP WITH 2.3.0 (`docs/UPGRADE-2.3.0.md`): a triage of upstream dev after 2.2.7 (180 commits) plus
  the 11 commits 2.3.0 added, then eleven batches in three waves (API/config, Code Mode engine replay, freeform tools,
  subagents, step lifecycle, MCP OAuth and pasted sign-ins, Code Mode search/fetch, small items, TUI, local discovery,
  client certificates). ocd reports 2.3.0; releases and the web UI build from v2.3.0 (`a6b15d3`). Tests 2,751 passed;
  side by side 241 of 246 identical (the same five known differences). Not ported: shell commands in the process
  daemon, MCP Events (flag), background shell timeouts (flag), video generation (library only), P-521 client keys.
- 2026-11-17: 2.3.0 CATCH-UP, STEP LIFECYCLE: a retryable failure after output started retries into a fresh
  assistant message (no `Continue` outcome, no "The previous response was interrupted" synthetic); `lower.rs` replays
  a failed or interrupted assistant cut at its last locally-run call (unrun calls dropped, `tool.input*` errors kept,
  reasoning only when output follows, no provider state on the last block). `session.step.first-token` (durable, once
  per step) sets `time.firstToken`, cleared on a step restart; the TUI's tokens/s counts from it and the footer adds
  `· N ttft` (dropped again in 2.3.1, #63759). Cooperative stop: `ToolContext.stop` (calls that listen settle on
  their own; the others are interrupted,
  the rest after 5 s; a shutdown interrupts all). Shell: `ShellService::stop` (SIGTERM, SIGKILL after 3 s, the command
  stays listed as killed); a stopped call fails with the output so far, the file and the shell ID
  (`metadata.interrupted/notice`, rendered by the TUI like a finished call); a cancelled shell job stops its command
  instead of removing it. Nested Code Mode calls never get the stop. The plugin host is rebuilt on 2.3.0's plugin
  package: a Promise tool that reads `signal` tells the core it listens and is stopped through it. Parity:
  `stop_shell_keeps_output`, `stop_first_token`, `plugin_tool_stop`.
- 2026-11-17: 2.3.0 CATCH-UP, MCP OAUTH AND PASTED PROVIDER SIGN-IN (#63028, #63403, #63315): a connection's auth is
  `oc_mcp::oauth::ConnectAuth` (upstream's `AuthProvider`: `token()` + `onUnauthorized`) over the credential the
  manager selected; the SDK provider (`oauth::Provider`, `auth()`) is used only by `authorize`. `oauth::refresh::exchange`
  refreshes with the login's `metadata.discovery` (saved by `authorize` and by the first refresh), 30 s ahead of
  expiry from `token()`, refuses another issuer, removes rejected credentials; the transport turns 403
  `insufficient_scope` into `McpError::InsufficientScope` (needs_auth). The fetch-level refresh share
  (`LockedRefresh`, `REFRESHES`) is gone with upstream's. Logins register both loopback redirects and redeem with the
  `Host` the browser came back on. `oc_ai::credentials::redirect` is `OauthRedirect`; ChatGPT, Codex browser,
  GitLab, Poe and Snowflake sign-ins have `submit` (`paste: true`); `Received.responder` is optional. Plugin OAuth
  passes a `submit` handle through (promise plugins need the sidecar rebuilt on 2.3.0's plugin package). Parity group
  `auth230`.
- 2026-11-17: 2.3.0 CATCH-UP, API/CONFIG FOUNDATION (first batch; ocd still reports 2.2.7 until the catch-up ends):
  `assets/openapi.json` is v2.3.0's (170 operations, 501 schemas: `session.step.first-token`,
  `experimental.mcp.event`, `Tool.FreeformFormat`, `Provider.Tls`), `generated.rs` regenerated with `gen.py`
  unchanged. The new fields exist but stay unset until their batches: `time.firstToken` and the first-token event
  (step lifecycle), `capabilities.freeform` on `/api/model` (freeform tools), the client certificate itself (P10), MCP
  Events (deferred). `PUBLIC_EVENT_TYPES` gains both new types (a test checks it against the document). Config: flag
  registry `shell_timeout_background` and `mcp_events`, `codemode_vm` a deprecated ignored key (the `codemodeVm` alias
  is ignored), provider `settings.tls` (`oc_config::schema::Tls`; in `SETTINGS_KEYS`/`PROVIDER_ONLY_KEYS`, so never a
  provider option or a model setting), model `capabilities.freeform`. `plugin.list` lists `opencode.tools.mcp-events`
  (a no-op without the flag). Parity group `api230`.
- 2026-11-13: LOCAL MODEL DISCOVERY (2.2.7's `opencode.provider.{lmstudio,ollama,vllm}`): `oc_core::local_providers`
  (discovery per location from its boot, every 30 s and on `config.updated`; 1 s requests; answers cached per root and
  key across locations; Ollama's `/api/show` cached by digest) and `oc_ai::catalog::local` (endpoint rules and the
  provider transform; the integration is removed in `Models::build_catalog_at`). Runs only in servers
  (`LocalProviders::enable` in `oc_backend::start`). `OCD_TEST_LOCAL_PROVIDER_ORIGIN` (test only) replaces the default
  origins; the ocd and oc-backend test harnesses set it to a dead port, and the parity harness writes a global
  `opencode.jsonc` pointing all three providers at `127.0.0.1:9` (never ask 127.0.0.1:8000 on this machine: another
  user's service listens there). Parity group `local`.
- 2026-11-13: TUI SCROLLING AND MESSAGE NAVIGATION: the first "previous message" / "Jump to" in a long session no
  longer renders the whole transcript first (it rendered 1,500 messages in 5.2 s); it searches from the bottom
  (`TranscriptCache::marks_from_bottom`) and renders only as far up as the target (10 ms). syntect now runs on
  Oniguruma (`regex-onig`), about 2.5x faster on first renders (also checked under `cargo zigbuild` for glibc 2.17:
  linked statically, no new glibc symbols). Wheel scrolling follows OpenTUI: fractional `scroll.speed` carries over and
  `scroll.acceleration` uses `MacOSScrollAccel` (both were ignored or rounded). The upstream Bun comparisons in
  `oc-ai/tests` now write their scripts inside the checkout: from a temp dir Bun auto-installed `effect` from npm, so they
  failed under the network fence.
- 2026-11-11: CAUGHT UP WITH 2.2.7 (`docs/UPGRADE-2.2.7.md`): four reviews, five batches and three fix batches, all
  compared with an upstream 2.2.7 binary built from the tag. Storage/API/config (OpenAPI 170 ops incl.
  `experimental.job.list`, fresh-DB `auth.json` import, part rows read/insert/fork, cross-site 403, the flag registry
  `oc_config::flag`, `websearch.use`, sidecar on 2.2.7); providers (Copilot native routes + Copilot Auto in
  `catalog/copilot.rs`, `SharedCache`, Azure Messages, unreachable-host retry, Console instructions/skills,
  `experimental.model.resolve`); tools/Code Mode/jobs (`read` offsets and suggestions, `skill://`, shell exit notices,
  `fetch` redirects, Code Mode `import`/`require` and catalog deltas, background compaction tuning); MCP (OAuth refresh
  under `credential-refresh:<id>` with rotation, temporary failures, per-tool checks, pasted redirects); TUI/CLI (`run`
  rewritten after `noninteractive.ts`, the 150 ms HOLD removed, 130/143 on signals, effect/cli 4.1.0 goldens, ACP
  steering). Fixes after the first full run: per-drain trace roots (`oc_util::trace::{root, inherit, span, reroot}`),
  the browser plugin's `machine.ts` (`oc_core::browser::machine`), `session.moved` once, 416 headers, three Windows CI
  failures, a hanging background-compaction test, and `run` exiting at once on a signal (stream closed when over;
  job sessions interrupted after delivery with `resume: true`). OPT-IN and not ported: MCP `pool`, the `part_rows` writer, Console
  watches, session warming, the Code Mode bytecode VM (keys accepted and echoed). Every cargo test run now uses the
  network fence too (`HTTPS_PROXY=http://127.0.0.1:9` etc.): one test had reached for api.github.com. Tests 2547
  passed; parity 202/207 against 2.2.7
  (left: cli_help, cli_inventory, stop_retry_exhausted, loop_http_400, loop_tool_error).
- 2026-10-30: CAUGHT UP WITH 2.2.2 (`docs/UPGRADE-2.2.2.md`): seven batches, all compared with an upstream 2.2.2
  binary built from the tag (`OPENCODE_CHANNEL=detached bun run script/build.ts --single --skip-web-ui` in
  packages/cli; an exported tree has no git branch). Storage/API (58 migrations, session_part, media blob reads,
  OpenAPI 162 ops, canonical locations, generate/text), models.dev v2 catalog + Console usage limits/config layer,
  Code Mode job queue (corpus re-recorded from 2.2.2 with `crates/oc-codemode/tests/recorder`, 1800/1802),
  provider wire (typed Go/budget/ChatGPT limits, throttling, costs, video), git/fs/tools (fs/watch, vcs folders,
  icons, pty 0.3.3), TUI/CLI (goal footer/dialog, /usage, effect 4.0.6 global server flags, tunnel proxies),
  session/plugins (/goal native in `oc-core/src/goal.rs`, experimental.idle, background compaction, sidecar on
  2.2.2). LATER: image generation, schedules, media blob writes (stubs answer as disabled). The guard wrapper now
  routes non-loopback traffic to a dead proxy (one early run reached opencode.ai's free tier anonymously).
- 2026-11-01: MERCURIAL, --WIZARD, BROWSER ATTACHMENT: `oc-git/src/hg.rs` (`HgVcs`, the `opencode.vcs.hg` port; git
  and hg registered natively, built-ins filtered per plugin id), `crates/ocd/src/cli/{wizard,prompt}.rs` (effect 4.0.6
  wizard and prompts; 25 upstream transcripts in `tests/golden/wizard.json`), `oc-core/src/browser/` (the
  `opencode.browser` plugin: `experimental.browser` RPC, control events, session allows, 45 `browser.*` tools with
  schemas generated by `scripts/browser-schemas.ts`, files, tunnels; the desktop-only `proxy.ts` is not part of the
  server). Code Mode now shows a nested tool's own failure message. Parity 110/115 (groups `hg` 7/7, `browser`
  10/10).
  Tests 2139 passed. `hg` for tests: a venv under the scratch folder.
- 2026-11-01: TOOL INPUT REPAIR + PLUGIN RELOAD FIX: `oc-tools/src/repair.rs` ports `plugin/tool-input-repair.ts`
  (run first in `CoreHooks::before`, on the registered schema; stored inputs stay as sent). Live `plugins` changes:
  `Plugins::load` returned early without settling every activation after the first (a lost wakeup: agent.list 503,
  RPC hung); activations now run one at a time with a settle-on-drop guard, and reload notifications follow 2.2.2's
  fixed batch order. Parity 118/121; tests 2160 passed.
- 2026-11-09: WINDOWS, MACOS, LINUX ARM GREEN IN CI (`.github/workflows/test.yml`; Windows runs under nextest's `ci`
  profile with a 4-minute test limit; `.github/ci-annotate.sh` turns failures into public annotations, since job logs
  need admin rights). Windows fixes: `oc_util::fs::canonicalize` strips the `\\?\` prefix like libuv, `has_root()`
  ports `path.isAbsolute`, `oc_util::global::os_home()` reads `USERPROFILE`, `.gitattributes` keeps LF, win32 TUI
  path formatting, PTY locale/Ctrl+C, detached service spawn. Libm-dependent Math cases are checked only on Linux
  x86_64 (the recording platform). Open: nextest LEAK on two service tests (inherited handles), TUI autocomplete file
  URIs are not a `pathToFileURL` port.
- 2026-11-09: OPT-IN 2.2.x FEATURES: image generation and decisions (oc-ai `image`/`decision`/`route::media`/
  `providers::media`, core `generate`, the image tool and jobs), schedules (oc-core `schedule/` with Effect cron/date
  parsing in `time.rs`, Code Mode `scan_tools`/bindings, `Permission.Unattended`, oc-backend `schedules.rs`), artifacts
  (`oc-core/src/artifact.rs`, sidecar `artifact` domain) and media blob writes (`Committing.media`, `*_media` reads,
  TUI `app/media.rs`). CI `test.yml` runs the tests on macOS/Windows/Linux ARM and annotates failing tests (logs need
  admin rights; annotations are public). Tests 2407 passed; parity 173/177 (groups schedule, image, artifact, media).
- 2026-11-09: CAUGHT UP WITH 2.2.5 (`docs/UPGRADE-2.2.5.md`): four reviews, five batches (storage/API/policy, the
  Claude Agent provider in `oc-core/src/claude_agent`, the shell scanner without tree-sitter, session/tools/terminals,
  TUI/CLI incl. the tab pulse) and one fix batch; upstream binary built from the tag like 2.2.2's. Migration 59 (`job`
  table), OpenAPI 169 ops, `@opencode/pty` 0.4.0, web UI from 2.2.5. The guard forces `OPENCODE_CLAUDE_PATH` to a
  refusing stub (`$S/guard/no-claude`); the parity group uses a scripted stand-in and the SDK package from the 2.2.5
  checkout. Tests 2287 passed; parity 145/149 against 2.2.5.
- 2026-10-30: 2.2.2 FINAL CHECK: full side-by-side 95/98 identical against the v2.2.2 build (cli_help renames,
  cli_inventory detached db path, stop_retry_exhausted upstream race). Follow-up fixes: browser skill + denies
  (`location::browser_denies`, `assets/skill/browser.md`), unlocated events for location-free session routes
  (`bus::without_location_async`, `Bus::route`), MCP started before the boot burst, failed-plugin domain notifies
  gated and ordered, prompt estimate from registered tools (`Engine::estimate_tools`). Tests 2076 passed.
- 2026-10-28: ACP + CLI PROMPTS: ACP permission previews and completed edit/patch diffs (`acp/permission.ts`;
  jsdiff parse/reverse/apply in `oc-tools/src/jsdiff.rs`), identical to 2.1.4 over stdio. Every clack prompt in the
  CLI (confirm/select/multiselect/text/password/autocomplete) and upstream's integration picker are ported
  (`crates/ocd/src/clack.rs`, golden frames from @clack/prompts 1.0.0-alpha.1 under Bun; resize, ctrl+d, NFC);
  tmux-identical to 2.1.4. Remote MCP servers register as `mcp_<hash>` OAuth integrations
  (`oc_core::mcp::register_integrations`, reconnect on `credential.switched`); `mcp auth` used to find none.
  `run` hold raised to 150 ms (50 ms split a failing step's burst under load).
- 2026-10-28: PARALLEL TOOLS: `step.rs` keeps tool executions unstarted until the provider stream would wait
  (`start_tools`), else until after `step.streamed`, like Effect's `setImmediate` fork scheduling; `loop_tool_error`
  was this, not a race. Parity `loop_parallel_settle` added; settle order sorted by call ID in the harness.
- 2026-10-28: MINI MODE PORTED: `crates/oc-tui/src/mini/` (~20k lines, one module per upstream
  `packages/tui/src/mini/` file plus `cli/src/mini.ts`, `mini-host.ts`): OpenTUI 0.6.8 split-footer renderer
  (`split.rs`), footer/composer/panels/docks, scrollback with a `marked` lexer port, fuzzysort 3.1.0, V2 event
  transport, subagents, queue, replay, `mini.*` settings. Goldens from upstream's own modules under Bun
  (`mini/testdata/{scrollback,footer}.json`, compared cell by cell). tmux comparison with the 2.1.4 binary: 12
  scenarios, 63 screens, text identical except one startup race (agent list not loaded yet upstream). Left: TS/JS
  highlight colours (syntect vs tree-sitter). Seen there: the server finishes parallel tool calls in a different
  order than upstream. `run`: last-burst skipping emulated in `Shared::consume` (150 ms hold), parity
  `cli_run_last_step` added.
- 2026-10-28: CLOUD CREDENTIALS: Bedrock's AWS chain is the SDK's `defaultProvider` (`oc-ai/src/protocols/
  bedrock_converse/aws/`: smithy INI, assume-role via `source_profile`/`credential_source`, web identity, SSO +
  SSO-OIDC refresh, legacy SSO, `aws login` with DPoP, container, IMDSv2/v1, endpoint overrides) and Vertex ADC
  follows google-auth-library 10.5.0 (`gemini/adc/external.rs`: identity-pool file/URL/certificate, AWS and
  executable sources, impersonation, `external_account_authorized_user`, `impersonated_service_account`, gtoken
  assertions, GCE detection). Environment via `protocols/cloud_env.rs` so tests use mocks. Compared with the 2.1.4
  binary against mock STS/SSO/IMDS/Google endpoints. Leftovers in `crates/oc-ai/README.md`.
- 2026-10-28: WS PROXY/DEFLATE/ENTRA: oc-ai has its own Responses WebSocket client (`route/websocket/client.rs`,
  Bun 1.4.4's upgrade request, HTTP CONNECT tunnel incl. `https:` proxies and credentials, permessage-deflate via
  flate2 + zlib-rs) and upstream's proxy selection (`route/websocket/proxy.rs`); `oc_util::proxy` holds Bun's
  `NO_PROXY` matcher and fetch proxy rules, used by `oc_util::http::client_builder` instead of reqwest's. The Azure
  plugin's Entra hooks (`http.request`, `experimental.ws.handshake`) are ported (`oc_core::azure`). 20 proxy/deflate
  cases identical to the 2.1.4 binary; parity group `websocket` 4/4 identical.
- 2026-10-27: `ocd upgrade` FROM GITHUB RELEASES + UPDATE NOTICE (`crates/ocd/src/release.rs`, `commands/upgrade.rs`,
  `oc-tui/src/app/update.rs`): latest or `v<target>` from the GitHub API, semver against the build, "already
  installed", local builds confirm or `--yes`, `SHA256SUMS` check, in-memory extraction, `--version` check, atomic
  rename (Windows `ocd.exe.old`), sudo hint before downloading, running service restarted. TUI notice and `/update`
  with a once-a-day check and upstream's `notify` default (`auto` installs, never restarts the service).
  `OCD_TEST_RELEASES_API`/`OCD_TEST_BUILD_VERSION` are test only. Replayed the real v0.1.1 release
  (`$OCD_SCRATCH/releases/v0.1.1/`) and ran a copy read-only against the real API. Workspace tests: 1,675 passed,
  9 ignored. Details: `docs/UPGRADE-2.1.4.md` progress row 8d.
- 2026-10-27: 2.1.4 CATCH-UP 8c (`pair --remote`): new crate `oc-tunnel`, a native OpenTunnel client
  (wire-compatible with `@opentunnel/client` 0.4.1, same XDG identity files, TLS ended locally with rustls, plus
  `oc_tunnel::fake` for tests); `ocd/src/remote_tunnel.rs` (service route, retries, fatal errors, logs);
  `pair --remote` consent, `service set remote true`, the tunnel URL in server info; clack-style `confirm`; QR codes
  rendered like uqr (`oc_util::qr`). Compared with the real SDK and upstream from source against a local fake relay
  (`OCD_TEST_OPENTUNNEL_API` is test only); never run against opentunnel.xyz. Details: `docs/UPGRADE-2.1.4.md` row 8c.
- 2026-10-27: 2.1.4 CATCH-UP BATCH 8b (`/connect`): the TUI's integration dialog is ported (`oc-tui/src/app/integration.rs`,
  `ui/choice.rs`): list, accounts (switch, rename, delete), sign-in forms, API keys, browser/device OAuth, pasted codes,
  external sources and login commands, with every entry point (palette, model list, prompt without a model, MCP
  sign-in, sidebar "Getting started"). Shared dialog fixes: centered keyboard scrolling, tall dialogs run off the
  bottom, OpenTUI-style wrapping of links. Server fix: login commands run in the location's registry. Checked against
  the 2.1.4 build in a pty (fake credentials, outbound requests fenced): `docs/UPGRADE-2.1.4.md` progress row 8b.
- 2026-10-27: 2.1.4 CATCH-UP BATCH 8a (experimental todo list): the `todowrite` tool (`oc-tools/src/tools/todo.rs`,
  `assets/tool/todowrite.txt`) behind `experimental.todo` (gated per location in `Engine::base_registry`), the
  `opencode.tool.todo` built-in plugin listed in upstream's position with the flag on or off, the TUI sidebar section
  (`oc-tui/src/app/sidebar_todo.rs`, `util/todo.rs`) and the transcript's "# Todos" block and inline rows. Parity
  group `todo` (3 scenarios) and the four plugin-listing scenarios are identical to the 2.1.4 build. Mini mode keeps
  the full transcript's rendering (upstream mini hides tools by default and draws its own card). Details:
  `docs/UPGRADE-2.1.4.md` progress row 8a.
- 2026-10-27: 2.1.4 CATCH-UP BATCH 7 (release): ocd reports 2.1.4 (workspace version); the golden CLI help,
  completions and parse fixtures come from v2.1.4's `commands.ts` (`pair --yes`, `upgrade -m winget`, the `mcp auth`
  example) and match the upstream 2.1.4 binary; uninstall detects WinGet installs; the plugin sidecar runs on Bun
  1.4.4; persistent terminals use `opencode-pty` 0.3.1 (Windows named pipes, not run on Windows); the release
  workflow builds the web UI from `v2.1.4`. Batches 1 to 7 are done; batch 8 (experimental features) is not.
  Details and the parity results: `docs/UPGRADE-2.1.4.md` progress row 7, `docs/PARITY.md`.
- 2026-10-15: CRASH/STOP HARDENING: early stops fixed in d0bdd76 (split escape sequences acted as Esc twice over SSH; error after streamed text; reload with a pending ask; tool/drain panics; Retry-After interrupt; sidecar death in a hook), parity group `stop`, `scripts/why-stopped.sh`. Terminal restore (`oc-tui/src/restore.rs`): OpenTUI exit signals + UI-thread panics turn raw mode/mouse/focus/paste/kitty/alt screen off and restore termios, then die of the same signal; all panics logged as `panicked` (`oc-util/src/panic.rs`). Fuzz tests `app/fuzz_tests.rs` (SSE events × sizes, terminal emulator) found and fixed vt100 resize panics (guarded), non-ASCII hex colors, parent-cycle recursion, leader timeout overflow, narrow dialogs. Two TUIs dying together on the user's server: not OOM, not the service; cause still unknown (needs the new `panicked` log line).
- 2026-10-13 (later): LATENCY STUDY vs upstream (no plugins: ocd equal or faster at every stage; same 100 ms delta batching). Fixed: catalog plugin transforms decode only touched records and the catalog input is sent once per fingerprint (`PluginHost::transform_keyed`; rebuild 1.45 s -> 0.17 s with models-discovery); TLS trust like Bun `--use-system-ca` (`oc-util/src/http.rs`: webpki roots + system bundles/dirs + SSL_CERT_FILE/SSL_CERT_DIR + NODE_EXTRA_CA_CERTS + NODE_TLS_REJECT_UNAUTHORIZED=0, also the WS connector; sidecar runs `bun --use-system-ca`); title request sent before the first step (`title.rs` selectTitle equivalent, runner yields once). TUI: no live transcript "Thinking" row while the busy line shows it (busy line carries the thought title), shell command on its own row under the status, blank rows around the status. Left: `bun add` TLS, macOS/Windows trust stores, TUI `wss://` to a remote server, a short catalog-rebuild window while plugins activate.
- 2026-10-13: PERF + MEMORY: (1) catalog cache per location (`oc-core/src/models.rs` fingerprint of config/connections/models.dev/`Plugins::catalog_state`/`Copilot::revision`, serialized builds, `malloc_trim`; `Catalog` data behind an Arc; sidecar `transform_ref`). Fixes the user's idle service at 1.8 GB (now ~180 MB with plugins) and repeated `/api/model` (now ~5 ms). Tests `oc-core/tests/catalog_{cache,builds}.rs`. The suspected Copilot/WS startup regression was box load, not code. Left: while plugins are still activating `catalog_state` changes often, so early requests can rebuild (~2 s with plugin transforms). (2) TUI long-session fix: `oc-tui/src/app/event_loop.rs` (input first, batched events, 16 ms frame cap) and `app/tcache.rs` (per-row transcript cache keyed by `Message::rev`, lazy off-window rows; `render_transcript_lines` kept as the reference, checked by `tcache_tests.rs`). Frame after a delta ~1 ms at 3,000 messages (was 10 s at 1,500). `scripts/tui_latency.py` measures echo/key latency end to end. Left: the streaming row re-parses its markdown each frame; `App::render_rev` is now unused; optional server-side delta coalescing.
- 2026-10-13: COPILOT + WEBSOCKET: GitHub Copilot routing like upstream (`oc-ai/src/catalog/copilot.rs`, `oc-ai/src/js_date.rs`, `oc-core/src/copilot.rs`, upstream's in-tree Copilot AI SDK provider bundled in the sidecar); OpenAI Responses WebSocket transport (`oc-ai/src/route/websocket.rs`, `protocols/open_responses_channel.rs`, `oc-core/src/model_transport.rs`). Parity websocket 4/4, plugin 11/11, loop 18/19 (known race). Left: WS proxies, permessage-deflate, Azure Entra on the handshake. Image/speech/video models skipped (unused by upstream's app). Repo pushed to git@github.com:sphinxid/ocd.git.
- 2026-10-13: AI SDK BRIDGE (plugin phase 2): `oc-ai/src/aisdk.rs` (ai-sdk route, call options, parts→events, errors), `oc-plugin/src/aisdk.rs` + `host/src/aisdk.ts` (per-location AISDK service in the sidecar, npm.add, internal cohere/perplexity/vercel/gitlab/sap-ai-core/dynamic hooks), `oc-core/src/aisdk.rs` (LocationBridge, eager load in `Models::resolve_model`). Parity `plugin_aisdk`, `plugin_aisdk_hooks` identical. Left: GitHub Copilot aisdk hooks + Copilot provider transforms; bundled packages are fetched from npm on first use; npm failure text differs; non-AI-SDK `@opencode/ai` runtime packages from npm unsupported. Dev profile now line-tables-only (full workspace test build: target/debug 9.6 GB, was 47 GB).
- 2026-10-13: TUI V1 IMPORT BADGE: `oc-tui/src/app/migration.rs` ports upstream MigrationOverlay (1 s poll of `experimental/migration/v1`, spinner badge top-right, error toast "Data migration failed", full TUI only); matches upstream captures for running/done/error/none and narrow widths. Toast border cells now keep the underlying background. Note: on a 20k-session synthetic v1 DB ocd (debug) showed home ~2 s later than upstream and imported slower; check with a release build.
- 2026-10-13: PARITY GAPS CLOSED: b3/traceparent on all outgoing HTTP (`oc-util/src/trace.rs`), `session.http.*` hooks (oc-ai `HttpHook`), vcs/worktree plugin transforms (`oc-core/src/vcs.rs`, `worktree.rs` strategies), ChatGPT client id from the active connection, Azure `az` resource listing, per-location plugin integrations/websearch, "always" re-check through `permission.evaluate`. Parity loop+readonly+plugin 31/32 identical (known `loop_tool_error` race). Left: no Mercurial provider; `vcs.get` uncached and no `vcs.branch.updated`; worktree tools use plugin strategies only once the project's canonical location is loaded; `session.http.response` reads the body before the hook. Tests: `export OCD_SCRATCH=...` on its own line before `OCD_BUN_PATH=$OCD_SCRATCH/bun142/bun-linux-x64/bun`.
- 2026-10-13: WEB UI SHIPPED: `scripts/build-web-ui.sh $OCD_SCRATCH/src/opencode` builds upstream's app archive (24 MB, git-ignored at `crates/ocd/web-ui/app-archive.bin`), embedded by `crates/ocd/build.rs` (or `OCD_WEB_UI_ARCHIVE` at build time); runtime lookup `OCD_WEB_UI` > embedded > `app-archive.bin` next to the binary > none (404 like upstream `--skip-web-ui`). 88/88 request cases identical to an upstream build with the UI (`$OCD_SCRATCH/up-dist-web`); `Vary` now matches upstream. Noticed: authenticated 404 on a missing `/api/fs/read` path lacks upstream's `content-type: application/json`.
- 2026-10-13: PLUGINS WIRED (phase 1): `oc-core/src/plugins.rs` (host on demand, activation/await, hooks incl. permission/tool/session/shell, transforms tool/agent/command/skill/reference/mcp/provider/model/integration/websearch, gating, hot reload), `plugin_request.rs`, `oc-backend/src/plugins.rs` (PluginApi over the in-process router), `plugin.*`/`rpc.call`, `ocd plugin add` + TUI inspection; tests `oc-backend/tests/plugins.rs`; parity group `plugin` 5/5 identical. Gaps: vcs/worktree transforms, `session.http.*` hooks (see oc-plugin README).
- 2026-10-13: PARITY SWEEP DONE (docs/PARITY.md): 38/45 scenarios identical to upstream; remaining = races/by-design + missing b3/traceparent tracing headers, ChatGPT client id from env connections, Azure resource list in key form. Suite: `python3 scripts/parity.py -q` (~25 min debug). Fixes: JS number formatting (oc-server JsFormatter), openapi body validation (oc-server/validate.rs), "always" permission persistence bug, location boot events, Zen free models, 229 integrations (oc-core/integrations.rs), mime table, interrupt semantics.
- 2026-10-12 (5th): V1→V2 HISTORY IMPORT DONE: `oc-core/src/v1_import.rs` (port of `v1-migration.bun.ts`: transformSession, legacy-tool notice, synthetic IDs, opencode-next.db, KV cursor, status) started by `oc_backend::start` in the background; `experimental.migration.v1.status` reports it. Byte-identical to upstream v2 on real v1 1.18.31 histories + synthetic edge cases (`scripts/v1_import_diff.py`; `cargo test -p oc-core --test v1_import`). TUI progress badge (upstream MigrationOverlay): done 2026-10-13, see below.
- 2026-10-12 (4th): NAME: the Rust port is "ocd" (binary `ocd`, crate crates/ocd — rename delegated to CLI agent; compat surfaces stay "opencode"). Upstream real binary built at scratchpad/up-dist/cli-linux-x64/bin/opencode (bun 1.4.2 in scratchpad/bun142). Benchmark `run` with mock: upstream ~1.6 s, ocd ~0.6 s. Parity harness scripts/parity.py (first scenario: only benign races remain after fixing execution.*/usage.updated envelope location). Launched: parity-sweep agent (docs/PARITY.md, fixes in lib crates) and plugin sidecar agent (crates/oc-plugin, Bun sidecar; core wiring by main afterwards).
- 2026-10-12 (3rd, cont.): TUI E2E WORKS against the Rust server: home renders, prompt → session view → read tool → answer (release: 312 ms). Helpers in scripts/ (mock_provider.py, vt_render.py; see scripts/README.md).
- 2026-10-12 (3rd, cont.): REAL BINARY WORKS: `target/release/ocd run --standalone` (then still named opencode) against mock provider (scratchpad/tools/mock.py) → read tool → answer → exit. Release timing: ~0.6 s total (startup ~450 ms incl. private server spawn, LLM→tool→LLM ~130 ms). Debug first run once hung at exit (not reproduced in release).
- 2026-10-12 (3rd): agents hit session limit again (resets 01:10 UTC 10-13); resumed CLI/backend/codemode via SendMessage. Core: worktree refresh, MCP elicitation, remote skills, config watching, codemode wiring.
- 2026-10-12 (2nd): oc-ai/oc-codemode/oc-tui/oc-backend + codemode helper agents hit the rate limit again; resumed via SendMessage.
- 2026-10-12: all 8 implementation agents hit an API rate limit mid-work; resumed via SendMessage after reset (crates had 3–11k lines each, all compiling except oc-codemode). oc-git had not started.

# Port state / handoff (last updated 2026-10-27)

**Name:** the Rust port is **ocd** (user: only the app + binary are ocd; everything else must stay backward-compatible with opencode). TUI logo reads "opencode-d" (oc-tui view_helpers LOGO_RIGHT, breakpoints 31/53). `ocd upgrade` installs ocd's own GitHub releases and the TUI shows an update notice (`crates/ocd/src/release.rs`); `uninstall` unchanged. (binary `ocd`, crate `crates/ocd`). Upstream compatibility surfaces (config/data dirs, opencode.json, .opencode/, OPENCODE_* env, HTTP API, x-opencode-* headers, plugin package names) keep the opencode names on purpose.

Goal (user request): an exact behavioral replica of **opencode v2** in Rust — full CLI + server + **Rust TUI**
(user chose "Full CLI + Rust TUI"). Project root: this repository. Working rules for every crate:
`docs/PORTING.md`. Specs: `docs/spec/01..08` (~18k lines, all complete).

## Upstream reference
- Repo `anomalyco/opencode`, tag `v2.1.4`, commit `68bc26b7da1990ce10ffecfbc1e09eb13aa5d824`, MIT (`LICENSE-opencode`).
  The port started from branch `v2` at `4617210822bdbb31e5749751afbfb4584ccedac8` (2.0.26) and caught up with
  2.1.4 in batches (`docs/UPGRADE-2.1.4.md`); the specs in `docs/spec/` still cite the 4617210 sources.
- Read-only checkout (may vanish with the session scratchpad; re-clone if missing):
  `git clone https://github.com/anomalyco/opencode.git && git -C opencode checkout v2.1.4`
  (was at `$OCD_SCRATCH/src/opencode`).
- Toolchain: rustup in `~/.cargo` (Rust 1.99, edition 2024). Always `export PATH=$HOME/.cargo/bin:$PATH`.
  Machine: 8 cores, 13 GB RAM. No git commits (user's global rule: only commit when asked; never add AI attribution).

## Key facts learned (affect design)
- Upstream ships as a **Bun-compiled single binary**, not Node (Node only as launcher shim / optional build).
- Architecture: background HTTP server (~142 endpoints, `assets/openapi.json` is normative) + TUI client over HTTP/SSE.
  Core is **event-sourced**: durable session events → projectors → SQLite tables; `event` table stays empty locally
  (persist=false); `event_sequence` holds per-aggregate seq (first event seq 0).
- DB: `<XDG_DATA_HOME|~/.local/share>/opencode/opencode.db` (release channels), WAL, pragmas in spec 02 §1.3.
  Fresh DB = run `assets/sql/schema.sql` + insert all 49 ids from `assets/sql/migration_ids.txt`.
- Paths are XDG on every OS. Log: `<data>/log/opencode.log`, logfmt, trimmed >50MiB → ~25MiB.
- IDs: 12 hex (48-bit, ts<<12+counter, truncated) + 14 base62; `ses_` descending, others ascending.
- v2 messages have no "parts": assistant `content[]` items (text/reasoning/tool). Stored `data` excludes `id`/`type`.
- MCP tools reach the model by default only via the Code Mode `execute` tool → **oc-codemode is required**.
- The persistent PTY daemon `opencode-pty` (npm `@opencode-ai/pty@0.3.1` since 2.1, 0.2.0 before) is **already Rust** (portable-pty +
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
| oc-tui | agent | DONE (136 tests): `run(TuiOptions)`, `run_mini(TuiOptions, MiniOptions)`; all screens incl. tab rail, diff viewer, stats, terminal pane (vt100), open/worktrees/pair dialogs, mouse select, comment-preserving cli.json. Mini mode simplified. Not yet run against a live server. |
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

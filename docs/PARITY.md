# Differential parity: upstream opencode v2 vs ocd

Upstream reference: since the 2.3.2 catch-up, a local build of `anomalyco/opencode` at tag `v2.3.2` (commit
`c0ad18b`, built in `packages/cli` with `OPENCODE_CHANNEL=detached bun run script/build.ts --single --skip-web-ui`;
reports `0.0.0-detached-<timestamp>`, channel `detached`). Port: `target/debug/ocd` (workspace version 2.3.2,
channel `latest`). From 2026-11-18 to 2026-11-21 the reference was the same kind of build at tag `v2.3.1` (commit
`c769de8`); from 2026-11-17 to 2026-11-18 the reference was the same kind of build at tag `v2.3.0` (commit
`a6b15d3`); from 2026-11-11 to 2026-11-16 the reference was the same kind of build at tag `v2.2.7` (commit
`fedc6f2`); from 2026-11-07 to 2026-11-10 the reference was the same kind of build at tag `v2.2.5` (commit
`9bbb8ce`); from 2026-10-30 to 2026-11-06 the reference was the same kind of build at tag `v2.2.2`
(commit `098aa7f`); from 2026-10-27 to 2026-10-29 it was the same kind of build at tag `v2.1.4` (commit
`68bc26b`; ocd then reported 2.1.4). Results before 2026-10-27 were against the build of
branch `v2` at commit `4617210` (reports `0.0.0-v2-<timestamp>`, channel `v2`; ocd then reported 2.0.26), and the
upstream sources cited below are relative to `packages/` of that commit unless they say otherwise.

## Running the suite

```sh
export PATH=$HOME/.cargo/bin:$PATH
cargo build -p ocd                          # debug build is enough
python3 scripts/parity.py --list            # scenarios and groups
python3 scripts/parity.py -q                # everything (about 25 minutes with a debug build)
python3 scripts/parity.py --group session   # one group: loop session permission form readonly errors cli plugin websocket stop todo wire22 goal background tools22 hg browser shell225 api225 session225 claude automation image artifact media api227 wire227 tools227 mcp227 run227 acp local api230 freeform230 auth230 subagent tls230 codemode231 api231 providers231 daemon231 partrows mcpopt231 mcp241 warming231 console231 clientmcp231 fcm231
python3 scripts/parity.py 'loop_*' cli_run  # by name or glob
```

Options: `--upstream BIN` (default: `$OCD_SCRATCH/up-dist/cli-linux-x64/bin/opencode`, or
`$PARITY_UPSTREAM`), `--rust BIN` (default `target/debug/ocd`), `--out DIR` (default `$OCD_SCRATCH/parity-out`),
`-q` prints only the per-scenario status. `PARITY_CONFIG_EXTRA` (JSON) adds config to every scenario's default project config, merging object values (`{"experimental": {"part_rows": true}}` reruns the loop and session scenarios with the part-row writer). Each scenario writes `<out>/<name>/{upstream,rust}.txt` and `diff.txt`;
the exit code is 0 only when every selected scenario is identical. `PARITY_TRACE=1` prints Python tracebacks.

Layout:

| file | role |
|---|---|
| `scripts/parity.py` | runner: selection, per-binary runs, diff, summary |
| `scripts/parity/harness.py` | `Server` (isolated `serve --port 0`, SSE capture), `Ctx` (per-run API used by scenarios), the normalizer, the dump format |
| `scripts/parity/mock.py` | scriptable provider: chat completions, Responses and Anthropic Messages SSE streams by request path, Open Responses over WebSocket, non-streaming completions, GET routes (also behind `scripts/mock_provider.py`) |
| `scripts/parity/{loop,sessions,readonly,errors,cli,plugins,copilot,websocket,stops,todo}.py` | scenarios, one function each, registered with `@scenario(group)` |

How a scenario runs, for each binary in turn: the project directory and the XDG state root are recreated at the
**same paths** (so path-derived values such as project IDs are directly comparable), the mock starts on one
port shared by both runs with the scenario's script, the binary runs `serve --port 0` with isolated
`XDG_*`, `HOME` and `OPENCODE_TEST_HOME` (the background service is also disabled in the isolated config, so CLI
commands can never reach a service on the default port), and the scenario records HTTP responses (status,
content type, body), CLI results (exit code, stdout, stderr), SSE events and the requests the binary sent to the
mock. Durable events are dumped per aggregate in `seq` order, ephemeral events in arrival order.

### Mock script format

A script is a list of turns; each non-title request consumes the first unused turn (optionally filtered by a
`match` substring). Title requests always answer `Smoke test`. Turn fields: `text`, `reasoning` (string or
chunks), `tools` (`[{name, args, id}]`, several = parallel calls), `finish`, `usage`, `status` + `body` +
`headers` (HTTP error, e.g. 429 with `retry-after`), `cut` (drop the connection mid-stream), `delay`, `stall`
(pause after the first chunk), `hang` (never answer), `coalesce` (chat completions: the whole answer in one write, so
the client reads its frames together), `raw` (chat completions and Anthropic Messages: exactly these SSE payloads,
then the end of the response; no finish chunk or `[DONE]` unless listed). On `.../messages`, `finish` sets the
`stop_reason` and `cut`/`stall` apply too. `"$PROJECT"` in any string is replaced by the project path.
With a title expected, the first main response is held until the title was answered (+0.5 s), which makes the
title/step ordering deterministic. The wire format follows the request path: `.../messages` answers Anthropic
Messages events (text and tool calls), `.../responses` Open Responses / OpenAI Responses events
(`response.created`, output items, deltas, `response.completed` with output and usage; response IDs `resp_<n>`
counted per mock, `resp_t<n>` for titles so a title racing the first step never shifts them), other paths
chat-completions chunks; a request with `"stream": false` gets one JSON chat completion. The Responses events also
go out as text frames on a WebSocket (a GET upgrade; every `response.create` frame consumes a turn). WebSocket-only
turn fields: `error_frame` (send this frame instead of a response), `drop` (send the content, then drop the TCP
connection), `close: [code, reason]` (the same with a close frame), `reject_upgrade: <status>` (refuse the next
upgrade while this is the first unused turn). The mock records every upgrade (headers, connection number), frame
(parsed and raw), client close (code) and drop. `mock.routes[path] = (status, json)` answers other GET requests
(e.g. a Copilot `/models` list); the rest are 404.

### What the normalizer masks (and nothing else)

- generated IDs (`<prefix>_` + 12 hex + 14 base62, also with derived suffixes like `msg_…_4`, a schedule or
  automation ID inside a derived message or session ID, `msg_sch_…_<n>`, `msg_aut_…_<n>_send`, `ses_aut_…_<n>`, and the
  fork's ID in its fork notice's message ID,
  `msg_fork_notice_ses_…`), renumbered by first appearance; UUIDs;
- epoch-millisecond numbers (any key), ISO timestamps, `duration`/`elapsed`-style keys, `pid` (and the number of a
  backgrounded shell result's `PID:` line);
- session `slug`s;
- the build version (`0.0.0-v2-…` or `0.0.0-detached-…` upstream, `2.0.26`, `2.1.4`, `2.2.2`, `2.2.5` or `2.2.7` port) wherever it appears,
  the channel in the User-Agent and in the report skill's diagnostics (`v2`/`detached` vs `latest`);
- the per-run project/state/server paths and URL (identical between runs; masked for readability);
- pagination cursors are base64url-decoded and normalized instead of masked;
- CLI output: the program name `ocd` → `opencode` in command position and in completion-script identifiers,
  epoch-ms numbers in JSON lines, local date-times (`session list`);
- plugin failure references (`err_` + 8 random hex digits);
- the importer named in a missing AI SDK module's message (`imported from <bundle>`: upstream's compiled chunk
  `/$bunfs/root/chunk-<hash>.js` vs ocd's sidecar bundle `.../ocd-plugin-host/<hash>/host.js`);
- the random IDs inside `b3`/`traceparent` header values: trace and span IDs are renumbered by first appearance
  (`<trace1>`, `<span1>`, ...), so which requests share a trace or a span stays visible and the presence,
  absence and format of the headers are compared.

Harness waits that work around upstream laziness (documented below as races, not masked): the first session
creation waits until `agent.list` answers the same twice and `plugin.updated` was seen; `readonly_server` settles
the agent/command/skill/model/provider/integration lists the same way; `readonly_vcs*` issue one unrecorded
`vcs.get` first.

## Code Mode engine: replayed recordings, not side by side

The Code Mode interpreter (`crates/oc-codemode`) is checked against recordings of upstream's own `packages/codemode`
test suite rather than by these scenarios: `crates/oc-codemode/tests/recorder/record.sh` instruments upstream, runs
its suite and the vendored test262 corpus, and records every program with its tools, limits, host calls and
`Result`; `cargo test -p oc-codemode` replays them (see that crate's README, Tests). Two corpora are replayed:
upstream 2.2.2 (legacy interpreter) and upstream 2.3.0, where the bytecode VM is the only engine. On 2.3.0, 1 841 of
1 843 recorded programs and 3 339 of the 3 340 sampled test262 files give the identical `Result`; the rest are the
two permanent divergences of the 2.2 corpus (`crates/oc-codemode/tests/fixtures/replay-skips-2.3.0.txt`). 2.3.0's
BM25 `search` ranking and namespace-call suggestions are also checked on a generated 400-tool catalog
(`crates/oc-codemode/tests/search_ranking.rs`, 272 programs). Streaming `fetch` responses are checked against upstream core 2.3.0's `execute` outputs on 46 programs read from a local server (`crates/oc-codemode/tests/fetch_stream.rs`). No recorded program differs because of the engine change.
Probing past upstream's tests (the `tests/stdlib_*.rs` cases and targeted programs, run through 2.2.7 and 2.3.0) did
find VM differences: the VM's standard library is partly program code, so some error texts, coercions and nested
imports changed. ocd follows them (`crates/oc-codemode/tests/upstream_2_3_0.rs`) except the VM's lazy check of an
iterator's `next`.

## Scenarios (288)

| group | scenarios |
|---|---|
| loop | trace headers of every outgoing request (title, steps, `webfetch`) over two prompts incl. an incoming `traceparent`, text only, read tool, reasoning, three parallel tools (read/glob/grep), write + edit, patch, shell tool, tool errors (missing file, unknown tool, malformed JSON args), HTTP 500 (retry scheduled, then interrupt), HTTP 400, 429 + Retry-After, stream cut mid-way, context overflow → compaction, manual compaction, steer/queue inbox (list/update/cancel), session `!` shell + synthetic, slash command + skill activation, full first LLM request (system prompt, tool schemas, headers), text followed by a failing finish reason in the same read (`network_error`, an unknown one: the text is kept and the run retries into a fresh message; since 2.3.0 without a "continue" notice, the failed message replayed cut at its last local call that ran), a location reload while a permission ask is pending (the run continues) |
| stop | how a run ends (slow, real-time retries and timeouts): chat-completions finish reasons (`stop` with tool calls, `tool_calls` without, `length` mid tool call and on text, `content_filter`, unknown, `network_error`, an empty stream, no `finish_reason`, no `[DONE]`), Anthropic stop reasons (`end_turn` with a tool call, `max_tokens`, `pause_turn`, unknown, a stream cut before `message_delta`, an `error` event after text), `chunkTimeout`/`headerTimeout` stalls up to the timeout-retry cap, HTTP 503 until the 10 retries are spent, a 24-step tool run with an auto compaction and an overflow compaction in the middle, a background shell command resuming a finished turn, `stop_shell_keeps_output` (2.3.0: a stopped shell call fails with the output so far, the output file and the shell ID, its command stays listed as killed, the next request replays the failed call; a silent command), `stop_first_token` (2.3.0: `session.step.first-token` and `time.firstToken` for reasoning, a tool call and a step retried after a 429) |
| session | CRUD (create/get/update title/metadata/permissions, list with limit/cursor next+previous/order/search/parentID/directory, bad cursor/limit, active, remove incl. children), messages (paging, get, context, inbox, diff, log, export, instruction entries, stats in three tool modes), export/import, fork (all / before), revert (stage with files, clear, stage, commit) in a git repo, interrupt of a hanging request, experimental wait, agent/model switch incl. plan reminders |
| permission | read `.env` → ask → once / always (second read auto-approved) / reject / reject with feedback; client-created requests, saved list/remove, errors |
| form | form API (create, list, get, invalid answers, reply, reply again, cancel, global list, invalid field), question tool round trip |
| readonly | server info, location, config, shells, agents, commands, skills, models (+default), providers, integrations, MCP, plugins, credentials, websearch providers, references, forms, permissions, PTYs, shells, projects, debug locations, v1 migration status; fs list/read/find (incl. traversal and content types); vcs in a git repo (status, branches, base, three diff modes, bad mode, worktree list) and outside one (incl. `vcs.init`); instruction entries + full LLM request with AGENTS.md/skill/command present; debug location evict + reload |
| errors | 25 not-found shapes, 42 validation cases (params, query, payload, invalid JSON, empty body, wrong content types), 7 auth cases |
| cli | `--help` for the root and all 17 subcommands and 29 nested ones, `--version`, unknown flag, bash completions; `run` default and `--format json` with a tool call, `run` without a message; `models`, `stats` (+`--json`, `--all`), `session list`, `debug paths/config/agents`, `mcp list`, `plugin list`, `auth list`, `service status/get` |
| websocket | a configured provider on the native OpenAI package with `transport: "websocket"` against the mock's Open Responses endpoint; every upgrade (headers without the random key, host and Bun's `permessage-deflate` offer), frame (body with instructions/tool schemas as digests, key order) and close is compared. `ws_session`: five prompts on one session, a read tool round trip and the answer as continuation frames (`previous_response_id` + only the new input) over one socket, a `previous_response_not_found` error frame (rejected → step retried in full on a new socket), a server error frame (retry on a new socket), a socket dropped mid-answer (`WebSocket closed with code 1006`, continue), a close frame 1011 before any output (retry). `ws_interrupt`: an interrupt while the socket waits closes it; the next prompt reconnects in full. `ws_fallback`: a refused upgrade pins the session to HTTP (both steps POST with tracing headers), a second session gets its own socket, a provider with `transport: "http"` never opens one. `ws_plugin_hooks`: oc-plugin's `ws-plugin` fixture (`experimental.ws.handshake/send/receive` rewriting the upgrade URL and headers, each sent frame and the received text; `session.http.request` marking HTTP requests): the title and, after an error frame and a refused reconnect, the HTTP fallback carry the HTTP hook's header, the socket traffic carries only the ws hooks' marks |
| plugin | oc-plugin's fixture plugins (`crates/oc-plugin/tests/fixtures`) copied into the project and configured as local plugins; the port runs them in its Bun sidecar (`OCD_BUN_PATH`, default `$OCD_SCRATCH/bun144/bun-linux-x64/bun`), upstream in-process. `plugin_inventory`: Promise + Effect + invalid + failing-transform plugins → `plugin.list` (order, sources, failures, the `skill.transform` failure disabling its plugin), `command.list`, `agent.list`, `skill.list`, boot and failure events. `plugin_tool`: a plugin tool offered to the model (tool names and full definitions of the first request) and executed with `tool.execute.before` rewriting its input, tool progress, a plugin command writing storage, the storage rows. `plugin_tool_stop` (2.3.0): Promise tools of the `stoppable-plugin` fixture that read their `signal`, stopped by the user while they run: one rejects with its own error, one resolves with what it had. `plugin_hooks`: `permission.evaluate` allowing/denying `.env` reads, `tool.execute.after` replacing a result, `session.prompt` rewriting the prompt, `session.context` adding a system part, `shell.create.before` adding an env variable. `plugin_effect`: agent/skill transforms and a rejecting `tool.execute.before`. `plugin_skill_files`: a plugin adds an in-memory skill with files (`add(skill, files)`); the model loads it and reads its folder, one file with offset/limit, `SKILL.md` and a missing file (2.2.7's plugin editor drops the files, so the skill lists none and only `SKILL.md` reads). `plugin_rpc_storage`: `rpc.call` (success, declared error, invalid input, unknown method, unknown RPC), storage incl. `ctx.event.subscribe` rows. `plugin_http_hooks`: `session.http.request` rewriting headers and body of every model request (title + steps; what the hook saw is echoed in a header) and `session.http.response` rewriting the streamed body. `plugin_vcs_worktree`: a plugin VCS provider selected with `default.set` (get/base/status/branches with search+limit/three diff modes incl. a failing one, `vcs.init` conflict) and a plugin worktree strategy (create, list, refresh, remove without and with `force`). `plugin_aisdk`: a configured provider whose package is oc-plugin's fixture AI SDK provider (`aisdk:file://.../aisdk-provider/index.js`, a LanguageModelV3 package talking to the mock through the SDK `fetch`): a read tool call, reasoning + text with cached/reasoning usage, an HTTP 400 (`APICallError`), a stream cut mid-way, then a model whose package does not exist (initialization failure); every request the provider sent (V3 prompt, tools, call headers, provider options, SDK options) is compared. `plugin_aisdk_hooks`: the same provider with plugin `aisdk.sdk` / `aisdk.language` hooks (wrapped SDK and language model) and `session.http.*` hooks around the SDK's fetch. `plugin_copilot`: the built-in GitHub Copilot plugin against the mock as a Copilot API (a fake OAuth credential created through the API, its `apiEndpoint` metadata = the mock; HTTPS goes to a dead proxy): the `/models` sync, the derived catalog, a `/responses` tool round trip (gpt-5.4), `/chat/completions` (gemini) and the native `/v1/messages` route (claude) after model switches, the utility-model title; every request the mock received (headers lowercased and sorted, transport headers dropped). `plugin_copilot_unsynced`: the same with a failing sync (404): every models.dev model through the in-tree AI SDK provider at the connection's API base |
| todo | the experimental todo list (`experimental.todo`). `todo_loop`: the model writes a three-item list, sends an invalid status (input validation error), updates it (a completed long item, a cancelled one, quotes in the in-progress summary), clears it and answers; every full model request (the `todowrite` definition and description, the `execute` catalog without it), the tool results, stored parts, durable and ephemeral events and `plugin.list`. `todo_off`: with `todo: false` the tool is not offered and a call is an unknown tool while `opencode.tool.todo` stays listed; then the config file turns it on (file watching enabled for this scenario) and the next request offers and runs it. `todo_permission`: `permission: {todowrite: "ask"}`, the first ask rejected with feedback, the second answered `always` (saved `todowrite *`) |
| wire22 | provider wire changes of 2.2: typed Go, budget, model-budget and ChatGPT limit errors, provider-reported costs (incl. a reported 0), shared OpenAI Chat options, an unknown finish reason, text after the finish reason, throttling that mentions tokens (retried, not an overflow), the 1% output headroom, and an MCP server with malformed tools |
| goal | `/goal`: the reviewer loop to "met", pause/resume/clear, no verdict, pausing by command and by interrupt, and the command/plugin inventory; command replies and errors, `opencode.goal` RPC replies, `rpc.opencode.goal.changed` sequences and the reviewer requests |
| background | background compaction on (the 2.3.1 default; the retired `experimental.background_compaction: false` no longer turns it off) and off (`compaction.background: false`): the event sequence and the requests the provider receives (group `compaction`) |
| tools22 | tool changes of 2.2: webfetch redirects (a chain, a denied target, a `file:` target), patch moves, a subagent asked to use a decision model, `$0` in commands |
| hg | Mercurial repositories (needs `hg` on `PATH`, otherwise recorded as skipped; the helper fixes author, date and config so changeset and project IDs match): a clean repo with every vcs endpoint and mode, worktree list/create, every kind of change (modified, added, removed, missing, moved, untracked files and folders, binary and BOM files), named branches and `.hg/branch` changes, a repo with no commits, git and hg nested both ways, `vcs.init?provider=hg`, a session with a write tool |
| browser | the desktop browser attachment against a stand-in desktop (`scripts/parity/browser.py`): attach, replace and detach with the control events and session permissions, the `browser.*` Code Mode tools and what the model receives, tool errors, a desktop leaving mid-request, the 60 s timeout, tunnels, the attachment ending with its session, its location or the plugin |

| shell225 | shell permission asks and saved patterns under the 2.2.5 scanner, bash and PowerShell (redirects, dynamic `cd`, an eval sink, method calls) |
| api225 | the 2.2.5 policy split (connect and use, per account, legacy conversion), artifact and schedule answers with the features off, and the catalog notifications of credential changes |
| session225 | reasoning across model switches and an interrupt, `read`/`edit` misses and `bash` as `shell`, MCP stdio framing, quiet location close, Code Mode `fetch` permissions, well-known config, terminal rename and removal with the session, restart recovery only under `serve --service` |
| claude | the Claude Agent provider against a scripted stand-in for `claude` (both binaries use the same stand-in, which logs its arguments, environment and every message): sign-in states, prices, a missing or crashing executable, text and tool turns, an unoffered tool, errors, cancel, fork and compaction; 2.3.1: a chat's `claude` reporting a model the saved catalog doesn't know (one full check for its context size, none on the next chat) |

| automation | automations, on by default since 2.4.1 (`scripts/parity/automation.py`; `OPENCODE_FLAGS=automations` against 2.3.1): the automation API and its errors (triggers, targets, sessionless automations, shell actions, list filters, the command list), unattended creation over HTTP, `automations: false` turning them off while the removed `experimental` keys don't, automations whose folder is gone staying manageable where they are off, a prompt automation firing into its session and into a new subagent child, `/automation` and the `automation` tools through Code Mode (`tools.automation.create`) and directly (`automation_create`; summaries, input errors), code actions (programs refused at creation, state, sends to the creator and to a child, an invalid result pausing, failures), shell actions (`automation_shell`: exit codes and output recorded, refused asks, no prompt), two servers on one data directory running an automation once (KV `automation:leader`) |
| image | image generation and decisions over HTTP (with the one-shot retry), the image tool by default, edits, model choice, turned off, failures, restart recovery, the plugin API, background jobs at the idle boundary |
| video | 2.4.1 video generation (`scripts/parity/video.py`, an xAI video model on the mock): `POST /generate/video` at the base config (the submit body, typed errors for unknown and chat models and unknown jobs), the server polling the job and storing the video as a `blob_` blob, `GET /generate/video/{id}` and `/blob/{id}`; the `generate_video` tool offered with a video model, its background job saving the file and the `<background-result kind="video">` notice resuming the session; `experimental.video_generate: false` |
| artifact | `experimental.artifacts`: the HTTP routes and `view`, location resolution, the tools directly and through Code Mode, a plugin provider and its failures |
| media | `experimental.media_blob`: images stored from a prompt and from the read tool, reads with and without the media capability, the media route, fork, import, cancel, revert and delete, a fork from a location without the flag |

| api227 | 2.2.7's cross-site 403, `session.generate` tokens and cost, the legacy `auth.json` import into a fresh database, the new config keys and their order, download headers, input for a session whose directory is gone |
| wire227 | unreachable and refused hosts (the retry schedule and the error text), the ChatGPT workspace limit, Azure Messages for Foundry models; plus `plugin_model_resolve` (the `experimental.model.resolve` hook) and `plugin_copilot_auto` (Copilot Auto's router, picks, fallback and compaction reroute) |
| tools227 | 2.2.7's `read` (past the end, negative offsets, suggestions, `skill://`), shell exit and partial-line notices, webfetch fragments and credentials, Code Mode `import` and `fetch` redirects, the job list, `session_move` |
| mcp227 | MCP OAuth refresh rotation across two locations, temporary refresh failures (503, 429) keeping the login, a pasted redirect URL, stdio probes, `~` in `cwd`, an invalid URL, a slow server missing the first step |
| mcpopt231 | 2.3.1's opt-in MCP features against `scripts/parity/fake_mcp_modern.py`, a stdio server speaking the 2026-07-28 revision (or only the legacy handshake): `mcpopt231_pool_shares_local_servers`, two Locations of one server with `pool: true` servers (a modern one with a fixed `cwd` spawned once, a legacy one private per Location after one shared attempt, one without a fixed `cwd` per directory, an unpooled one per Location; spawn counts, wire methods, the `mcp.*` events per Location), and the shared process kept by one Location after the other disconnects, stopped by the last; `mcpopt231_automation_event_fires` (`automations: true`): the model creates event automations with `opencode_automation_create` (a poll-only type and bad arguments refused, then a watch of `job.finished`), the event fires the prompt into the session, `event_position` before and after, and an event the payload schema refuses dropped by the MCP client (2.4.1). 2.3.x's `mcpopt231_watch_wakes_session` went with the `watch_mcp_event` tool |
| mcp241 | 2.4.1's MCP changes (`scripts/parity/mcp241.py`, on `fake_mcp_modern.py`): `mcp241_unset_protocol_negotiates_auto`, unpooled local servers with no `protocol` (a 2026-07-28 one, a legacy one refusing the probe, one exiting on it and spawned again, one never answering it with a 2 s startup timeout, an explicit `legacy` one; statuses, spawn counts, wire methods); `mcp241_list_mcp_events`, `opencode_list_mcp_events` with no flag (Code Mode off) listing a server's push types and failing for a legacy connection, with the tool's definition in the recorded model requests (automations and media generation off, so other batches' tools stay out of the requests); `mcp241_event_watch_wire`, an event automation created through Code Mode's `tools.automation.create`: `events/list` and `events/stream` params, the event firing the prompt, `event_position`, and an event the payload schema refuses dropped while the watch stays active |
| run227 | the rewritten `run`: message arguments, waiting for background jobs, retry notices, a permission question it cannot answer, `--worktree`, `--archived`, SIGTERM |
| acp | `opencode acp` over stdio (`scripts/parity/acp227.py`, a minimal ACP client with the child-session capability that answers permission requests `once`): a background subagent outliving the root turn (2.3.0: and the root's answer to its report, streamed after the prompt's response), outliving a cancelled turn with its `read: ask` permission request, and a session closed while it runs. Every JSON-RPC message the client received is compared in three ordered lists (the root's updates with responses and requests, the child updates, everything after the prompt response), plus the root's and the child's model requests each in order: the server publishes a background child's start concurrently with the root's tool result, so how those interleave during the turn is a race in both binaries. 2.3.1 (`acp231.py`): a prompt that ends while its background shell runs, the turn the shell's notice wakes streamed after the response, a fast shell's reaction and a follow-up prompt |
| local | the LM Studio, Ollama and vLLM discovery plugins against mock servers (`scripts/parity/local.py`, ephemeral ports shared by both runs, configured `baseURL`s only): the discovery requests (paths, bearer keys, Ollama's `/api/show` bodies), the provider, model and integration lists they produce (embedding, foreign and failing models left out, contexts, families, capabilities), `apiKey` and `/api`/`/api/v1` base URLs, a chat through a discovered vLLM model (path, key, model), a 30 s outage keeping the inventory and the next answer replacing it, a config change moving LM Studio to another server and key and dropping vLLM's endpoint; the `*.updated` counts of the boot and of each change. 2.3.0: an LM Studio decision model (`max_context_length: null`, `has_vision_adapter`) in the lists and through `/generate/decisions` (the System One request and answer), a key saved through `/api/credential` while LM Studio refuses requests without it, and the integration's env key `LMSTUDIO_API_KEY` |
| api230 | 2.3.0's config keys: the flags `shell_timeout_background` and `mcp_events`, the deprecated `codemode_vm` and the retired `codemodeVm` alias, provider `settings.tls` (echoed by `/api/config` and `/api/provider`, never a model setting; malformed ones invalidate the provider), model `capabilities.freeform` in config (and on the listed model), and the event filter with the new public types |
| api231 | 2.3.1's API and config (`scripts/parity/api231.py`): `compaction.background` and the reworked flag keys (`automations`, `video_generate`, `full_codemode`, `schedules` after the registry; the retired `background_compaction` and `shell_timeout_background` dropped) echoed by `/api/config`, invalid values; the schedule routes gone; the automation routes' checks and 403s with the flag off; the tool plugins in `plugin.list` and the commands; the video routes' decoding and 403s; blobs served from `<data>/blobs/` (type sniffing, ETag, 304, ranges, `If-Range`, 416, 404) |
| providers231 | 2.3.1 provider and server changes (`scripts/parity/providers231.py`): reasoning replay on an OpenRouter route at the mock (signed `reasoning_details` with scalar reasoning, one session per model declaring no field, `reasoning_content` or `reasoning_details`, and a mid-session model switch) and through an OpenAI-compatible gateway whose Kimi-style answers carry `reasoning_content` and details, every replayed assistant message compared; a ChatGPT token-sharing login whose expired token can't be refreshed, over a models.dev catalog (`OPENCODE_MODELS_PATH`) with GPT 6.1/6.2 Sol, Astra and Terra and an xAI video model: the OpenAI model list and provider settings before and after the login, video models listed only by type and a video model's ref sent to image generation (422); another process holding the database's write lock past the busy timeout (500 "Storage is in use by another process", then a write once it is released) |
| daemon231 | 2.3.1 shell commands in the process daemon (`scripts/parity/daemon231.py`): a server replaced through the PTY handoff (`POST /api/experimental/persistent-pty/handoff`, then `serve --service` with `OPENCODE_PTY_HANDOFF`) while a foreground shell call or a background shell job runs: the next service settles the call as moved to the background, resumes the session, and the command's completion notice (its whole output) wakes it again |
| partrows | the opt-in `experimental.part_rows` writer (`scripts/parity/partrows.py`), comparing the stored `session_message` and `session_part` rows (each part's data and key order, whether it was updated after it was created) as well as the reads: `partrows_loop` (reasoning, text and two parallel tools, a failed tool, a step retried after a 500, the replayed requests), `partrows_fork_revert` (a fork of everything and one before the second prompt, a prompt in the fork, a committed revert, the session deleted), `partrows_compaction` (a manual compaction and a prompt after it), `partrows_move` (a session moved from a project without the flag to a directory with it and back, a step in each), `partrows_roundtrip` (one database, `OPENCODE_DB`, for both binaries: this binary writes with the flag on, the other reads it with the flag off and on and prompts on it, this binary reads and prompts again; the upstream run is upstream → ocd → upstream, the port's the reverse) |
| warming231 | Opt-in session warming (`scripts/parity/warming231.py`, `core/src/plugin/warming.ts` as of 2.3.1) with second-long intervals: every request the mock receives over a warming cycle (bodies and headers) and what is stored. An OpenAI-compatible model pinged at 2, 4 and 6 s of a 7 s window and then no more; Anthropic Messages with a provider `cache.warming` (the pings carry the turn's prompt-cache markers) where a ping that writes more cache than it reads stops warming until the next prompt; which sessions get warmed (a model's `cache.warming: false`, `"auto"` on a `cache.ttl` of an hour outliving the window, an explicit interval as long as the window, an invalid `cache.warming` falling back to the top level); an interrupted turn whose pings come on schedule until the session is deleted, and a manual compaction restarting warming with a failed ping followed by the next one an interval later |
| auth230 | 2.3.0 MCP OAuth and provider sign-in (`scripts/parity/auth230.py`, on mcp227's fake server): a token due within 30 s refreshed with the login's saved discovery before it is sent, a login whose issuer discovery no longer names (no refresh sent, login kept), a 403 `insufficient_scope` (needs_auth, no step-up), both loopback redirects registered and the code redeemed with the localhost one the browser came back on (the saved discovery included); the Poe, Snowflake, Codex browser and ChatGPT sign-ins with `paste: true` and their pasted-URL checks (Poe's issuer and state-echo rules), using only pastes that end or keep the attempt before any token request |
| subagent | 2.3.0 subagents (`scripts/parity/subagent230.py`): a report each time the child goes idle (the launch, then a follow-up prompted to the child over HTTP), a child whose background shell is still running when its report goes out (`state="running"`, "Background work still running:") and its second report once the shell's notice woke it, a child created and prompted directly over HTTP reporting to its parent, a stopped child ("Subagent cancelled"), and a restart with a child in flight (`serve --service` on a free port, SIGKILL mid-turn, a second service resuming the child). Compared: the parent's and the child's messages, the job list (also mid-run), durable events per aggregate, and each session's model requests in order |
| tls230 | 2.3.0 provider client certificates (`scripts/parity/tls230.py`): an HTTPS OpenAI-compatible server that requires a client certificate signed by a CA made with `openssl` for the run (trusted through `NODE_EXTRA_CA_CERTS`), called through `/generate/text`, `/generate/decisions` and sessions: a certificate presented (its common name echoed), none, a missing `{file:}`, a plain HTTP endpoint with an unusable key; another certificate's key, a certificate as the key, a wrong or missing passphrase, a scrypt-encrypted key, a redirect; PKCS#1, PKCS#8 encrypted with PBES2 AES and 3DES and PKCS#12 3DES, legacy OpenSSL AES and 3DES, SEC1 P-384, Ed25519, a chain, a passphrase for a plain key; a decision model; the fixture AI SDK provider (certificate, none, redirect); in sessions the first failure of each kind (retried as a transport error) |
| freeform230 | 2.3.0 freeform tool input against the mock's Responses and Chat endpoints (`scripts/parity/freeform230.py`; the mock streams `custom_tool_call` items): `execute` and `patch` as custom tools (free text, and the Lark patch grammar) on a model configured with `capabilities.freeform`, custom calls run with their object input, the stored parts, the files the patch wrote, and the replay of the calls as `custom_tool_call`/`custom_tool_call_output` in later requests; the formats a model lists (`["text"]`, `[]`, unset) deciding which tools go out custom; `openai/gpt-5.4-mini` freeform from the bundled models.dev catalog (and its `/api/model` capabilities); Chat Completions custom tools and a streamed `custom` call. Every request body (custom tools in full, function tools as one digest each, input/messages in full) is compared |
| codemode230 | 2.3.0 Code Mode (`scripts/parity/codemode230.py`): `search` over the built-in and fake MCP tools (whole and partial words, plurals, a namespace filter), an unknown tool's suggestion and namespace calls naming up to three tools, a compact JSON result; streaming `fetch` against mock routes that send chunks 80 ms apart (a reader, `for await` with a streaming `TextDecoder`, a second whole-body read failing, the response's keys and `Headers`, `bodyUsed`), a redirect whose open body the mock sees discarded, and a body left open at the program's end that the mock sees cancelled |
| codemode231 | 2.3.1 Code Mode (`scripts/parity/codemode231.py`): `Buffer` (text in each encoding, number reads and writes, views, Node's error codes, the `buffer` module, a Buffer returned and logged, a logging-only program with no `null` line); arguments of fake MCP calls crossing as host copies then JSON (a `toJSON` not run, functions left out, promises awaited, Dates/Sets/URLs/errors converted; an iterator, a symbol, `tools`, a Uint8Array and a Buffer refused; a rejected promise), `search("text")` and its input error's suggestion, the reworded diagnostics; a plugin (`crates/oc-plugin/tests/fixtures/codemode-rename-plugin`) renaming calls inside a program to another Code Mode tool (structured output returned) or to a direct or unknown tool (not available); a nested `worktree_remove` asking `edit`, whose `permission.asked` names the `execute` call as its source |
| console231 | OpenCode Console opt-in features against a local mock Console signed in with a fake API key (`scripts/parity/console231.py`; the mock answers the config with the headers a scenario turns on, the event stream over a WebSocket or long polls, the watch and artifact routes, and records every request with its sign-in headers). `console_watches` (2.2.7, `experimental.console_watches` + `x-opencode-event-watches`): the `console_watch` tools through Code Mode (a target, an MCP event type with minutes, the list, an unwatch of another session's watch, both kinds at once, the event types) and the catalog's `console_watch` namespace; a teammate's review waking the session (the steer and the model request it resumes), another machine's event left unacknowledged, a lease end reconnecting at once, two shell calls that printed a pull request URL (2.3.1's shell result has `exitCode` where the watch reads `exit`: neither binary watches them), a deleted session's watches cancelled and its later event acknowledged, the upgrade's sign-in headers. `console_watches_poll`: a refused upgrade, long polls with Console's cursor, a CI failure waking the session. `console_watches_off`: no flag, or no header: no socket, no request, no tools. `console_artifacts` (2.3.1, `artifacts` + `x-opencode-artifacts`): the providers, a publish over HTTP (prepare, only missing blobs, the publish with the session), an update over the same slug, a read Console can't serve, a name another member took, `artifact_create` from a session, the list and a delete, `artifact.updated`. `console_artifacts_quiet`: without watches the publish names no session; without the header `local` stays the default |
| clientmcp231 | Experimental client-hosted MCP (`scripts/parity/clientmcp231.py`; a scripted client over the harness's own WebSocket client, `scripts/parity/wsclient.py`): `clientmcp231_model_calls_client_tool`, a client named `tui` hosting `open_terminal` and `write_terminal` (the `initialize` it is sent, `/api/mcp` not listing it, the model calling both and a missing one through Code Mode with the session in `_meta`, the catalog update after it disconnects); `clientmcp231_stacked_clients`, two clients under one name (the newer, answering in binary frames, takes the calls; the older one after the newer disconnects); `clientmcp231_tickets_and_refusals`, the ticket route's checks and envelope, the socket's 401/403/400/404 refusals in order, a ticket presented for another name or directory staying valid for its own, a used one refused, a rejected `initialize` closing with 1011 and a reload with 1012 before the client attaches again |
| fcm231 | 2.3.1 opt-in `full_codemode` (`scripts/parity/fullcodemode231.py`): with `experimental.full_codemode` the tool definitions and Code Mode catalog the model sees (shell, read, edit, write, patch and websearch in Code Mode, the first five pinned; grep, glob and webfetch gone), two parallel `tools.shell` calls, a read, a write and an edit through Code Mode, a `background` input (ignored), Node's `path` (both flavours, `node:path`, `require("path")`); `OPENCODE_FLAGS=full_codemode`; `experimental.all_flags` leaving the tools direct and programs without `path`; a script still running after 10 s returning its receipt, listed as a running `codemode` job, and its `<codemode>` result waking the session; a session-wide background request moving a script at once (only the parts are recorded: where the request's own notice lands relative to the receipt is a race) |

## Results

**2.4.1 video generation and the media tool renames (batch R6; debug build, 2026-11-27):** against the opencode 2.4.1
binary. The new `video` group (an xAI video model on the mock): **3 of 3 identical** (`video_http_generate`: the submit
body, typed errors, the polled job, the stored `blob_` blob and its download; `video_tool_generate`: the tool's
definition, the background job, the saved file and the `<background-result kind="video">` notice; `video_tool_off`).
The media tools (`generate_image`/`generate_video`) now match everywhere they differed before: the whole `automation`
group but `automation_shell` (whose `outside` automation fires later than the scenario reads it in ocd, a timing
difference), `mcpopt231_automation_event_fires`, all five `mcp227`, the three `clientmcp231`, `mcp241_list_mcp_events`
(`opencode_models` takes `type: "video"`), `loop_write_edit`, `loop_patch`, `loop_llm_request` and the other `loop_*`
are identical, as are `image_*` with the tool renamed (`image_idle_background` differs in plugin
`experimental.job.updated` deliveries, a later batch). `loop_shell_tool`, `loop_tool_error`, `session241_tool_repair` and
`session241_background_result` differ only in the order of concurrent events (a usage update against a job update, the
idle marker between two turns); `session241_structured_order` differs in the Code Mode instructions' hash (batch R2's
Code Mode surface). 2.4.1's generate routes no longer wait for the base location's plugins, so a server's first generate
request finds no configured models there; `boot_global` now boots that location (unrecorded) before the recorded calls,
which makes `image_http_*` and `tls230_*` identical again.

**2.4.1 MCP (R3; debug build, 2026-11-26, on top of the automation batch):** against the opencode 2.4.1 binary:
`mcp241_unset_protocol_negotiates_auto` (a modern, a legacy, an exiting, a silent and a pinned-legacy server:
statuses, spawn counts and wire methods) and `mcp241_event_watch_wire` (the event automation's rows, `events/*`
params and wire methods) identical. `mcp241_list_mcp_events` matches in the tool's definition, its place in the tool
list, the listing and the failure; what differs is `opencode_models`' `video` type. In the `automation` group
`automation_api`, `automation_api_unattended`, `automation_config_off`, `automation_folder_gone`, `automation_lease`
and `automation_shell` are identical, and `automation_fires`, `automation_subagent`, `automation_checks`,
`automation_command` and `automation_tools_direct` differ only in the media tools (`generate_image`/`generate_video`
against ocd's `experimental_image_generate`); `mcpopt231_automation_event_fires` likewise differs only there.
`mcpopt231_pool_shares_local_servers`, four of five `mcp227` and `clientmcp231_tickets_and_refusals` are identical;
`mcp227_slow_server_misses_the_first_step` differs in the media tools and in the slow server already being connected
when ocd's prompt ends (ocd's session start and prompt took about 2 s longer here, with or without the probe), and
the other `clientmcp231` diffs are the media tools.

**Opt-in full Code Mode (debug build, 2026-11-20):** the new `fcm231` group (5 scenarios) against the opencode 2.3.1
binary: **5 identical**. The tool definitions, system prompt and Code Mode catalog the model sees with the config key
and with `OPENCODE_FLAGS=full_codemode` (the pinned `shell`, `read`, `edit`, `write` and `patch` listings with their
annotated signatures, `websearch` in Code Mode, no `grep`, `glob` or `webfetch`), `all_flags` leaving the tools direct,
parallel `tools.shell` calls, read/write/edit through Code Mode, a `background` input dropped, the `path` global (and
none without the flag), a script backgrounded
after 10 s and by a session-wide background request (the receipt, the `codemode` job in `experimental.job.list`, the
`<codemode>` message and the step it wakes) match.

**2.4.1 catch-up, automations and the Code Mode surface (batch R2; debug build, 2026-11-26):** against the opencode
2.4.1 binary through the isolation guard. The `automation` group, rewritten for automations on by default (no
`OPENCODE_FLAGS`; `automation_config_alias` became `automation_config_off`, and the new `automation_shell`):
**4 of 11 identical** (`automation_api_unattended`, `_config_off`, `_folder_gone`, `_shell`), and the other 7 differ
only by later batches' tools: the code actions of `_checks` (refused programs, state, sends, pauses), the system prompt's Code Mode catalog (the `automation` namespace and its five
tools, `opencode.session.move` pinned, `opencode.session.rename`), `/automation`'s text, the direct
`automation_create` summaries and input errors, the shell action's `Exit code: N` outputs and the hidden/refused
automations all match. What still differs belongs to other batches: 2.4.1 always offers
`opencode.list_mcp_events` and checks an event trigger without `mcp_events` (batch R3: the catalog hash, `event.off`'s
`MCP server not found: ci`), and offers `generate_image`/`generate_video` (batch R6). The same holds for `loop_write_edit`,
`loop_patch`, `loop_llm_request`, `tools227_session_move` (`tools.opencode.session.move`), `codemode231_*` (plus batch
R10's import messages), `plugin_hooks`, `plugin_tool` and `api231_automation_routes` (plugin listing of later batches).
`fcm231_tools` (opt-in `full_codemode`) differs as expected: 2.4.1's Code Mode description overrides of `edit`/`patch`,
the pinned `websearch` and the model's tool policy applied to the catalog are not ported.

**Client-hosted MCP (debug build, 2026-11-20):** the new `clientmcp231` group (3 scenarios) against the opencode
2.3.1 binary through the isolation guard: **3 identical** (two runs). The handshake opencode sends the client, the
tool calls and their `_meta`, the stored parts, events and model requests, the stacking and fallback, and every
refusal and close code match. Probing the binary first showed that a ticket presented for another name, location or
PTY is not used up (2.3.1's `Cache.invalidateWhen`); ocd's PTY tickets were used up by any attempt and now behave the
same. Rerun with this change: `mcpopt231` and `api231` identical, `mcp227` 4 of 5 (`mcp227_slow_server_misses_the_first_step`
found the slow server already connected on a loaded machine, a timing difference of the debug build).

**OpenCode Console opt-in features (debug build, 2026-11-20):** the new `console231` group (5 scenarios) against the
opencode 2.3.1 binary with a local mock Console and a fake API key: **5 identical**. The watch tools, the catalog
namespace, a wake and the model request it resumes, the WebSocket lease reconnect, the long polls, the dormant cases,
the `console` artifact provider's requests (blobs compared sorted, since they upload concurrently; slug suffixes come
from the random artifact ID and are masked) and the `artifact.updated` notifications match. Two shell calls that
printed a pull request URL are watched by neither binary: 2.3.1's shell result reports `exitCode` where the watch reads
`exit` (the ported unit tests trigger the hook with `exit` like upstream's own). Code Mode's ephemeral tool progress is
left out of `console_watches`, where upstream sometimes sends none.

**Opt-in part rows (`experimental.part_rows`; debug build, 2026-11-20):** against the opencode 2.3.1 binary, the new
`partrows` group (5 scenarios, comparing the stored `session_message` and `session_part` rows down to each part's key
order) is **identical**, including `partrows_roundtrip`, where each binary reads, with the flag off and on, and
continues a database the other wrote. Groups `loop` and `session` rerun with
`PARITY_CONFIG_EXTRA='{"experimental": {"part_rows": true}}'` (30 scenarios): **28 identical**, the two left being the
known `loop_tool_error` and `loop_http_400` event-order races; with the flag off (`loop`, `session`, `media`, 34
scenarios) the same two differ and the rest are identical. Through the isolation guard, sessions written by each
binary with the flag on (two prompts with reasoning, a tool call and text: 8 part rows) export identically from both
binaries with the flag on and off, before and after the other binary continued them with the flag off and then on.

**Opt-in MCP features (debug build, 2026-11-20):** the new group `mcpopt231` against the opencode 2.3.1 binary,
through the isolation guard: **3 of 3 identical**. `mcpopt231_pool_shares_local_servers` (one process for both
Locations, the legacy server's single probe and three handshakes, the per-directory server, the status and catalog
events, the shared process outliving one Location's disconnect), `mcpopt231_watch_wakes_session` (the watch tool's
definition in the full request, its listing, the wake message and its ID, the stream cancelled after the first
event) and `mcpopt231_automation_event_fires` (the creation checks, the fired prompt quoting the event,
`event_position` as 2.3.1 writes it, the pause on a payload the schema refuses). The wake's message ID hashes the
session ID, so the normalizer masks `msg_mcp_<hash>` and the scenario checks the hash itself. A first run showed that 2.3.1's SDK fails a 2026-07-28
`tools/list` result without `ttlMs` and `cacheScope` (a Zod error dump as the status); the fake now sends them as
the SDK's server does. ocd does not check those fields (P3, left).

**2.3.1 catch-up, core tools and sessions (Q2; debug build, 2026-11-18):** against the opencode 2.3.1 binary,
groups `loop`, `permission`, `stop`, `tools22`, `tools227` and `shell225` plus `readonly_*` (57 scenarios): **53
identical**, among them `readonly_instructions` and `loop_llm_request` (the 2.3.1 `read`/`shell` descriptions and
schemas in the request), `loop_shell_tool`, `tools227_read`, `tools227_shell`, `stop_background_shell`,
`stop_shell_keeps_output` and every `permission_*`. Left: `loop_tool_error`, `loop_http_400` and
`stop_retry_exhausted`, the known races below, and `readonly_fs`, where both now index hidden files and only the
fuzzy ranking of two equally scored directories differs (`fs.find` approximates fff). With the tool texts in place,
`automation_tools_direct`, `claude_*` (13), `plugin_aisdk*`, `plugin_copilot*` and
`wire22_{chat_options,mcp_malformed_tools,output_headroom}` are identical too; `automation_command` differs only by an
ephemeral Code Mode tool-progress event and `subagent_report_running_work` only by the order of ephemeral usage events
(its durable reports, built from the outstanding-work snapshot, match).

**Session warming (debug build, 2026-11-20):** the new `warming231` group (4 scenarios) against the opencode 2.3.1
binary is **identical**: the same pings at the same points of each cycle with the same bodies, headers (a ping stays in
the trace of the request that scheduled it) and prompt-cache markers, the same usage on the session and no stored
messages. The first run differed only by the pings' trace IDs (ocd gave each ping a trace of its own).

**2.3.1 catch-up, automations (Q5; debug build, 2026-11-18):** the new `automation` group (10 scenarios, replacing
`schedule`) against the opencode 2.3.1 binary with `OPENCODE_FLAGS=automations`: **8 identical** (`automation_api`,
`_api_unattended`, `_config_alias`, `_folder_gone`, `_fires`, `_subagent`, `_checks`, `_lease`). `automation_command`
and `automation_tools_direct` sent the same automation tool calls, results, input errors and Code Mode catalog, and
differ only by the `read` and `shell` tool texts of another 2.3.1 batch.

**2.3.1 catch-up, providers and server (Q3; debug build, 2026-11-18):** against the opencode 2.3.1 binary, the new
`providers231` group (reasoning replay on OpenRouter and gateway routes, the ChatGPT token-sharing model list and
settings with an expired unrefreshable token, video models listed by type and their 422, a database locked by another
process) is **identical**, as are `errors_{not_found,validation,auth}` (the validation cases' 500s are now
`UnknownError` with a ref in both), `claude_{connect,logged_out,api_key_prices,missing_binary,exit_on_start}`, all of
`wire227` and `auth230`, and `wire22` except three scenarios. The new `claude_new_model_context` (a chat reporting a
model the catalog doesn't know runs one full check: four `claude` spawns and Sonnet at 1M in both) and the other
`claude_*`, `plugin_aisdk*`, `plugin_copilot*`, `wire22_{chat_options,mcp_malformed_tools,output_headroom}` differ
only by the `read`/`shell` tool texts of the core-tools batch (Q2), which also move `output_headroom`'s
`max_completion_tokens` by 4. Below the server, 512 replayed request bodies (`crates/oc-ai/tests/wire_2_3_1.rs`) and
the models.dev normalization (`catalog_models_dev_upstream.rs`, now on the 2.3.1 checkout, with a video model) match
2.3.1's own code under Bun.

**2.3.1 catch-up, foundation (Q0: migrations, API, flags, background compaction; debug build, 2026-11-18):** against
the opencode 2.3.1 binary (`up-dist-2.3.1`), groups `api225`, `api227`, `api230`, the new `api231`, `readonly`,
`errors`, `cli`, `compaction` and `schedule` (36 scenarios), then the scenarios a boot-event fix touched again:
**27 identical**, among them all of `api231` (config keys, automation routes with the flag off, video and blob
routes), `api225`, `api227`, `api230_config_keys`, `readonly_{server,vcs,vcs_none,debug}`, `errors_{not_found,auth}`,
`cli_run`, `cli_run_last_step`, `schedule_api`, `schedule_api_unattended` and `schedule_checks` (the schedule routes
are gone in both). Every location boot of 2.3.1 announces `artifact.updated` after `skill.updated` (the built-in
`opencode` provider plugin transforms the artifact providers); ocd does the same now. The rest differ only by later
batches or known causes: `compaction_background_{swap,off}` sent the same requests (background summary on by
default, off with `compaction.background: false`) and differed only by the Code Mode instruction digest, and are
identical once rebased on the 2.3.1 Code Mode batch (as are `api231`, `readonly_server`, `readonly_debug` and
`schedule_api`, rerun then);
`readonly_instructions` by the `read`/`shell` tool texts (Q2) and Code Mode's 500-character tool descriptions in the system prompt (Q1);
`readonly_fs` by hidden files in file search (Q2); `errors_validation` by readable 500 bodies (Q3);
`schedule_{fires,command,lease}` wait for a schedule that fires in neither and differed only by that boot event
(fixed; not rerun);
`schedule_tools_direct` by the automation tools 2.3.1 offers with `experimental.schedules: true` (Q5); `cli_help` and
`cli_inventory` as before (version, wording, the channel's database name). Databases: a 2.3.0 database with real
schedules (created through 2.3.0's API) migrates to the same schema and the same `automation` rows with ocd and with
2.3.1; 2.3.1 lists and resumes the automations ocd migrated, and each binary opens a fresh database of the other.

**Small 2.3.0 items (P7: tool result shapes, fork notice, hook metadata, provider fixes; debug build, 2026-11-17):**
groups `loop`, `stop`, `session`, `readonly` and `plugin` plus `subagent_report_running_work` against the local v2.3.0
build (`up-dist-2.3.0`): **58 of 62 identical**. `loop_shell_tool`, `loop_session_shell`, `stop_shell_keeps_output`,
`stop_long_run_compaction`, `stop_background_shell` and `subagent_report_running_work` now match (the `PID:` line,
`pid` metadata and structured output; `shell.created` with the `pid`; the shell's completion notice admitted without
a location), `readonly_server` lists `opencode.fork-notice`, and `session_fork` has the same fork notices once the
normalizer masks the fork ID inside `msg_fork_notice_<id>`. Left: `loop_tool_error`, `loop_http_400` and
`stop_retry_exhausted`, the scheduler and title races described below.

**Provider client certificates (debug build, 2026-11-17):** the new group `tls230` against the local v2.3.0 build
(`up-dist-2.3.0`): **6 of 6 identical** (`tls230_chat`, `tls230_bad_keys`, `tls230_key_formats`, `tls230_decisions`,
`tls230_aisdk`, `tls230_session`). The certificate reaches the server on chat, decision and AI SDK requests; every
unusable key fails with `FailedToOpenSocket: Was there a typo in the url or port?`, a refused handshake with
`EPROTO: The TLS handshake failed. Does the server speak TLS on this port?`, a redirect with Bun's `UnexpectedRedirect
fetching "…"` text, and each of them is retried as a transport error in sessions. P-521 keys were left out: Bun
presents them, ocd can't (COMPATIBILITY.md).

**Local model discovery, 2.3.0 (debug build, 2026-11-17):** the new `local` scenarios against the local v2.3.0 build:
**3 of 3 identical** (`local_decision`: the decision model in the lists and its System One request and answer without
a key; `local_saved_key`; `local_env_key`).

**Code Mode features of 2.3.0 (debug build, 2026-11-17):** the new group `codemode230` and the Code Mode scenarios
`tools227_codemode` and `tools22_repair_codemode_mcp` against the local v2.3.0 build (`up-dist-2.3.0`): 3 of 4
identical (`codemode230_search`, `codemode230_fetch_stream` with both bodies seen cancelled by the mock in both runs,
`tools227_codemode`). `tools22_repair_codemode_mcp` no longer differs by its result's JSON (compact now); it still
has one more `session.tool.progress` than upstream, a race with the 100 ms progress throttle: upstream's three
nested MCP calls finish in about 34 ms, before the trailing update is due, while the debug build's take about 80 ms
plus the program's teardown (measured in-program with `Date.now()`), so its trailing update fires first. The
throttle is the same as upstream's (`codemode/tool.ts` `record`/`publish`).

**ACP background shells, 2.3.1 (debug build, 2026-11-18):** the `acp` group with the new
`acp_background_shell_wakes_turn` against the 2.3.1 release (`up-dist-2.3.1`), through the isolation guard: **4 of 4
identical** (the woken turn's `slow shell reaction` arrives after the prompt's response in both, never before the
release file exists).

**Final 2.3.1 differences (debug build, 2026-11-18):** `hg_session` and `acp_background_shell_wakes_turn` against the
2.3.1 release, five runs each: identical in all. A Mercurial location now announces its branch right after
`skill.updated`, before `artifact.updated`, where 2.3.1's hg plugin first changes the Vcs State (a git location still
announces it after `model.updated`; `session_revert` identical). The fast background shell's prompt response differed
by its token counts (2.3.1 30/15, ocd 20/10): `usage` sums every step of the prompt's execution, and 2.3.1's shell
notice lands about 30 ms after the step that ran the shell, after the next step took its input, so it wakes a third
step, while ocd's lands within a few ms and the second step answers it. A build of the ACP batch shows 20/10 too, so
the scenario now records only the usage field names of that response. The neighbouring groups: `hg` 7 of 7, `acp` 4
of 4, `loop` 20 of 22 and `readonly` 5 of 6, the same three differences as the full run (`loop_tool_error` and
`loop_http_400` orderings, `readonly_fs`'s directory order).

**Code Mode features of 2.3.1 (debug build, 2026-11-18):** the new group `codemode231` and the Code Mode
scenarios `codemode230_*`, `tools227_codemode`, `session225_codemode_*` and `tools22_repair_codemode_mcp` against
the 2.3.1 release (`up-dist-2.3.1`): every Code Mode part, tool result, permission request and model request is
identical in all ten. Each still differs by one ephemeral `artifact.updated` event that 2.3.1 publishes when a
location loads (another batch's catch-up), which also renumbers the later event ids. Left out of the scenario:
`search.name`, which the source names `searchCall` (as ocd does) but the minified release build names `R`.

**Subagent lifecycle (debug build, 2026-11-17):** the new `subagent` group and the `acp` group against the local
v2.3.0 build (`up-dist-2.3.0`), both through the isolation guard: **8 of 9 identical**. `acp` 3 of 3 (the root's answer
to the subagent's report is now streamed after the prompt's response, like 2.3.0) and `subagent` 5 of 6:
`subagent_report_per_idle`, `subagent_direct_child`, `subagent_stopped_child`, `subagent_restart_child_in_flight` and
`subagent_run_waits_for_report` identical. `subagent_report_running_work` has the same reports (the first tagged
`state="running"` with the still-running shell listed, the second after the shell's notice), job lists and requests;
its diff is only the background shell's `PID:` line and `pid` metadata (2.3.0's shell result shape, another batch)
and the order of two racing `session.usage.updated` events. In the `goal`, `compaction`, `tools22`, `run227` and
`loop` groups, run before the step-lifecycle batch landed, every difference was `firstToken`/`session.step.first-token`,
the shell `PID:` line, Code Mode's compact JSON or the fork notice (other batches); nothing from subagents.

**MCP OAuth and pasted provider sign-in (debug build, 2026-11-17):** groups `mcp227` and the new `auth230` against
the local v2.3.0 build (`up-dist-2.3.0`): `auth230` **5 of 5 identical** (refresh ahead of expiry, issuer mismatch,
insufficient scope, the localhost redirect, the provider pastes) and `mcp227` 4 of 5 identical, the logins now saving
`metadata.discovery` as upstream's do; `mcp227_slow_server_misses_the_first_step` differs only by `time.firstToken`
and `session.step.first-token` (P3). `mcp227_oauth_refresh_unavailable` now records the status after the
`credential.switched` reconnect settles: before it, upstream sometimes still showed the refused attempt's message.
In one run of four, upstream's `auth230_oauth_refresh_ahead` listed the login unrefreshed (`old`/`ref-1`) half a
second after connecting with the refreshed token; the scenario now waits 1.5 s.

**Freeform tool input (debug build, 2026-11-17):** the new group `freeform230` against the local v2.3.0 build
(`up-dist-2.3.0`), with `api230` now comparing the listed models' `capabilities`: `api230_config_keys` identical; in
the four `freeform230` scenarios every freeform part is identical (the custom `execute` and `patch` definitions with
the grammar text, the custom calls and their stored parts, the files the patch wrote, the replayed
`custom_tool_call`/`custom_tool_call_output` items, which tools go out custom per listed format, the Chat custom
tools, and `/api/model`'s `freeform` capabilities). They differ only by two 2.3.0 changes of other batches:
`time.firstToken` on assistant messages (P3) and the `subagent` tool's schema and description (P2).

**2.3.0 catch-up, API/config batch (debug build, 2026-11-17):** against an opencode 2.3.0 binary
(`up-dist-2.3.0`), groups `api227`, `readonly`, `errors`, `cli` and the new `api230` (20 scenarios): 15 identical
(`api230_config_keys`, `api227_config_keys`, `api227_{download_headers,cross_site,stranded_session_input,
legacy_credentials_fresh}`, `readonly_{fs,vcs,vcs_none,debug}`, `errors_*`, `cli_run`, `cli_run_last_step`);
`cli_help` and `cli_inventory` differ as before (version, wording, the channel's database name). The other three
differ only by what later 2.3.0 batches port: `time.firstToken` on assistant messages (`api227_session_generate`,
`readonly_instructions`: step lifecycle), the `subagent` and `execute` tool texts (`readonly_instructions`: subagent
lifecycle, Code Mode) and the `opencode.fork-notice` plugin in `plugin.list` (`readonly_server`: fork notice; the
`opencode.tools.mcp-events` id is listed now). `debug config --standalone` with valid and malformed `tls` and
`freeform` keys prints the same document and the same diagnostics.

**Local model discovery (debug build, 2026-11-13):** the new group `local` against the local v2.2.7 build: **5 of 5
identical** (`local_discovery`, `local_api_key`, `local_chat`, `local_outage`, `local_config_reload`). The first runs
differed only in the `*.updated` counts: upstream's plugins start inside the boot batch, so an inventory found at once
is part of the boot's notifications (ocd's boot now waits up to 250 ms for the first round), and each reload's
`integration.updated` compares with the list at that moment (ocd publishes a reload's notifications before the plugin
goes on). With the harness's new global `opencode.jsonc` (the three providers at a dead port), the groups `readonly`,
`cli`, `session`, `permission`, `form`, `api225` and `api227` (35 scenarios) keep their status: 33 identical, plus the
known `cli_help` and `cli_inventory`. Their outputs change the same way on both sides: the config document is listed,
LM Studio's models.dev provider and models become available and `ollama`/`vllm` integrations appear.

**ACP background following (debug build, 2026-11-13):** the new `acp` group (`acp_child_after_turn`,
`acp_child_outlives_cancel`, `acp_child_close_stops`) against the local v2.2.7 build, both through the isolation
guard: **3 of 3 identical**. After the prompt response both send the child's updates (and its permission request)
until it completes, nothing of the root's answer to the report, and nothing after `session/close`.
`plugin_skill_files` is identical too: like 2.2.7, the files a third-party plugin passes to `add(skill, files)` are
dropped (only the Console plugin attaches files, which ocd now keeps through the plugin transform and serves from
the `skill` and `read` tools).

**Run after the 2.3.2 catch-up (debug build, 2026-11-22):** all 288 scenarios against the local v2.3.2 build:
**282 of 288 identical**. The full run showed nine more until the harness masked the port's new version `2.3.2`
like the earlier ones (the built-in report skill and the User-Agent carry it); `subagent_run_waits_for_report` then
differed once in three reruns, by when the `waiting for 1 background job` line prints relative to the next step,
a timing race. Left: the six known `cli_help`, `cli_inventory`, `stop_retry_exhausted`, `loop_http_400`,
`loop_tool_error` and `readonly_fs`.

**Run with full Code Mode, client-hosted MCP and image previews (debug build, 2026-11-21):** all 288 scenarios against the
local v2.3.1 build: **282 of 288 identical** (the full run, then `tools227_codemode`, `tools227_session_move`,
`hg_changes`, `hg_nested` and `subagent_run_waits_for_report`, which raced while another build loaded the machine,
identical in 3 of 3 reruns). Left: the six known `cli_help`, `cli_inventory`, `stop_retry_exhausted`, `loop_http_400`,
`loop_tool_error` and `readonly_fs`.

**Run with the opt-in features of 2.2.6–2.3.1 (debug build, 2026-11-20):** all 280 scenarios against the local v2.3.1
build after the MCP pooling, MCP Events and automation event triggers, part-row writer, session warming and Console
watches/artifacts batches: 274 identical; the six left are the known `cli_help`, `cli_inventory`,
`stop_retry_exhausted`, `loop_tool_error` and `readonly_fs`, plus `partrows_loop`, whose two parallel tools included a
`glob` of two files that rg lists in directory order (upstream's glob does not sort); the part-row scenarios now glob a
single file and were identical in 4 of 4 reruns (`loop_http_400`, a known race, matched in this run).

**Final run against 2.3.1 (debug build, 2026-11-18):** all 263 scenarios against the local v2.3.1 build, ocd
reporting 2.3.1: **257 of 263 identical** (the full run, then `hg_session` and `acp_background_shell_wakes_turn`
rerun after their fixes, identical in 5 of 5 runs each). Left: `cli_help`, `cli_inventory`, `stop_retry_exhausted`,
`loop_http_400` and `loop_tool_error` as before, and `readonly_fs` (two directories with the same fuzzy score come
back in a different order; both binaries index hidden files).

**Final run against 2.3.0 (debug build, 2026-11-17):** all 246 scenarios against the local v2.3.0 build, ocd
reporting 2.3.0: **241 of 246 identical**. Left: `cli_help` (the intended renames), `cli_inventory` (the upstream
build's database file name), `stop_retry_exhausted` and `loop_http_400` (upstream races) and `loop_tool_error`
(Effect's scheduler), the same five as against 2.2.7. The normalizer now masks 2.3.x versions and the shell PID
inside `run --format json` lines.

**Process daemon 0.6.0 (2026-11-18):** the harness installs the 0.6.0 `opencode-pty` from a 2.3.x checkout at
`<cache>/opencode/bin/opencode-pty/0.6.0-3a781ac5ff8bc189/` for every scenario, where upstream 2.3.1 puts its embedded
copy, so both binaries run terminals (and, since the 2.3.1 catch-up, shell commands) in the same daemon without a
download. `session225_terminal_rename_and_delete` is identical against the 2.3.1 build.

**Shell commands in the process daemon (2.3.1 catch-up, debug build, 2026-11-18):** against the 2.3.1 build, with
both binaries running shell commands in their daemons: `loop_shell_tool`, every `stop_*` scenario but
`stop_retry_exhausted` (its known timing difference), `shell225_bash_permissions`, `shell225_powershell_permissions`,
`tools227_shell`, `subagent_report_running_work`, `run227_background`, `session225_restart_recovery` and the new
`daemon231` group are identical. `daemon231_restart_foreground_shell` replaces a server through the handoff while a
foreground shell call runs (the next service settles the call as moved to the background, resumes the session, and
the command's notice wakes it again); `daemon231_restart_background_shell` does the same with a background job. The
foreground command sleeps 12 s because ocd's server takes longer to stop (it waits up to 2 s for open requests).
`shell225_bash_permissions` also showed that ocd announced `permission.asked` before registering the request, so an
immediate reply could get a 404; the request is now registered first, as upstream does.

**Run after closing the 2.2.7 gaps (debug build, 2026-11-15):** all 216 scenarios against a fresh local v2.2.7
build: **211 of 216 identical**, including the new `acp` (3), `local` (5) and `plugin_skill_files` scenarios. Left:
the same five as the final run below (`cli_help`, `cli_inventory`, `stop_retry_exhausted`, `loop_http_400`,
`loop_tool_error`). The harness now gives ocd an empty web UI by itself, and `session225_terminal_rename_and_delete`
finds the 0.4.0 terminal daemon in any 2.2.x checkout.

**Final run against 2.2.7 (debug build, 2026-11-11):** all 207 scenarios against the local v2.2.7 build, `hg` 7.2.4
on `PATH`, the scripted stand-in for the Claude Agent provider: **202 of 207 identical**. Left: `cli_help`,
`cli_inventory`, `stop_retry_exhausted` and `loop_http_400` as before (in 10 extra upstream runs of `loop_http_400`,
2 had ocd's order), `loop_tool_error` (Effect's fiber scheduler yields after 2,048 operations, so upstream settles
instantly failing tool calls in the middle of a read; ocd keeps the 2.2.5-era order). The first
full run found the per-drain trace roots, the browser plugin's state machine, `session.moved` sent once, the
destination's boot events after a tool move, `project.updated` for a missing directory, the 416 headers, and
`run227_signal` exiting 1 under load (a stopped `run` kept its event stream open and could strand a cancelled job's
notice); all fixed. `tools227_session_move` once showed an extra `tool.progress` event in five runs (timing).

**Run with the opt-in features of 2.2.x (debug build, 2026-11-09):** all 177 scenarios against the local v2.2.5
build: **173 of 177 identical**, including every `schedule` (7), `image` (11), `artifact` (6) and `media` (4) scenario.
Left: `cli_help`, `cli_inventory`, `stop_retry_exhausted`, and `loop_http_400` (the title rename vs failed step race,
on both sides).

**Final run against 2.2.5 (debug build, 2026-11-09):** all 149 scenarios against the local v2.2.5 build, `hg`
7.2.4 on `PATH`, the scripted stand-in for the Claude Agent provider: **145 of 149 identical**. Left: `cli_help`,
`cli_inventory`, `stop_retry_exhausted` as before, and `tools22_repair_builtin`, which was identical in three reruns
(two files created in the same millisecond sort by modification time either way). The first full run found the
credential-change notifications, the plugin restart order, Copilot's per-location model sync, invalid browser
results' decode errors and the MCP tool list after a server closes; all fixed. Two upstream races are recorded
robustly: Copilot sometimes syncs its models a third time, and `mcp.*` events race the failed step's usage event.

**Run after the tool input repair and the plugin reload fix (debug build, 2026-11-01):** all 121 scenarios
against the local v2.2.2 build, `hg` 7.2.4 on `PATH`: **118 of 121 identical**. Left: `cli_help`, `cli_inventory` and
`stop_retry_exhausted`. New: `tools22_repair_builtin`, `tools22_repair_disabled`, `tools22_repair_codemode_mcp`,
`tools22_repair_direct_mcp`, `plugin_tool_repair` and `plugin_live_changes` (eight successive live `plugins`
changes); `browser_location_end` again changes the config twice on one server. `loop_http_400` and
`cli_run_last_step` were identical in this run; both stay timing-sensitive.

**Run after Mercurial, `--wizard` and the browser attachment (debug build, 2026-11-01):** all 115 scenarios
against the local v2.2.2 build, `hg` 7.2.4 on `PATH`: **110 of 115 identical**, including all 7 `hg` and 10
`browser` scenarios. Left: `cli_help`, `cli_inventory` and `stop_retry_exhausted` as before, `loop_http_400` (the
title rename vs failed step race, on both sides) and `cli_run_last_step`, where ocd's 150 ms hold for `run`'s last
burst missed the final step header once in five reruns on a busy machine. The CLI timestamp mask no longer matches
digits inside IDs (a message ID whose hex part was all digits was masked as a timestamp).

**Final run against 2.2.2 (debug build, 2026-10-30):** all 98 scenarios against the local v2.2.2 build, both
binaries through the isolation guard (which now also fences off the network), ocd pointed at an empty web UI
archive: **95 of 98 identical**. The 3 left are `cli_help` (intended wording), `cli_inventory` (the upstream build's
`opencode-detached.db` path) and `stop_retry_exhausted` (upstream's extra ephemeral `project.updated`). The first
run after the seven port batches found five more differences, all fixed: the built-in `browser` skill and its deny
rules, events of location-free session routes carrying a location, the MCP startup order, an extra
`skill.updated` after a failing plugin transform, and the prompt estimate behind the output limit. `loop_http_400`
(the title rename vs failed step race) was identical in this run; it still flips between runs on both sides.

**Final run (debug build, 2026-10-28, after the parallel tool, ACP and prompt fixes):** all 74 scenarios (`loop_parallel_settle` added) against the local v2.1.4 build, both binaries through the isolation guard, ocd pointed at an empty web UI archive: **69 of 74 identical**. `loop_tool_error` is identical now; the 5 left are `loop_http_400` (title rename vs failed step race), `cli_help` (intended wording), `cli_inventory` (`debug paths`/`debug agents`), `plugin_copilot_unsynced` (network error wording; in this run upstream also timed out once waiting for idle) and `stop_retry_exhausted` (the extra ephemeral `project.updated`).

**Parallel tool calls (2026-10-28):** upstream forks each tool execution as a fiber that first runs when the provider loop yields to the event loop, so every `tool.called` (and, for a response read at once, `step.streamed`) is published before any tool starts; ocd now defers its executions the same way (`oc-core/src/step.rs`, `start_tools`). Results settle in completion order in both, which is a timing race (upstream shuffles reads between runs too), so the harness sorts consecutive `tool.success`/`tool.failed` events by call ID (`record_events(race_sort=...)`) in `loop_parallel_tools`, `loop_tool_error` and the new `loop_parallel_settle` (unknown tool + read in one step, identical 15/15).

**Release-candidate run (debug build, 2026-10-28):** all 73 scenarios (the new `cli_run_last_step` included) against the local v2.1.4 build, both binaries through the isolation guard, ocd pointed at an empty web UI archive: **67 of 73 identical**. `cli_run` and `cli_run_last_step` are identical since `run` skips the last burst of events like upstream (below), and the WebSocket group stays identical with the new proxy-aware client. The 6 left are the two races, the intended help wording, `debug paths`/`debug agents`, the extra ephemeral `project.updated` and the network error wording listed below.

**Todo list run (batch 8a, debug build, 2026-10-27):** all 72 scenarios (the new group `todo` included) against the same local v2.1.4 build, both binaries through the isolation guard, ocd pointed at an empty web UI archive: **65 of 72 identical**. `todo_loop`, `todo_off`, `todo_permission` and the four plugin-listing scenarios (`readonly_server`, `plugin_inventory`, `plugin_effect`, `plugin_rpc_storage`, which differed only by `opencode.tool.todo`) are identical; the 7 left are the races, the intended help wording, `run`'s last step, `debug paths`/`debug agents` and the network error wording listed below.

**2.1.4 release run (batch 7, debug build, 2026-10-27):** all 69 scenarios against the local v2.1.4 build, both
binaries started through the isolation guard. **55 of 69 identical** in the full run; two diffs were real and are
fixed (#50, #51), and `errors_not_found` differed only because the upstream build has no web UI while ocd embeds
one (identical with ocd pointed at an empty web UI archive, `OCD_WEB_UI`). After the fixes, **57 of 69 identical**
(reruns of the differing scenarios). The 12 left:

| scenario | cause |
|---|---|
| ~~readonly_server, plugin_inventory, plugin_effect, plugin_rpc_storage~~ | ~~2.1.4 lists the built-in plugin `opencode.tool.todo`, which ocd did not have~~; identical since batch 8a |
| ~~loop_tool_error~~, loop_http_400 | ~~unknown-tool failure vs `step.streamed`~~: not a race but upstream's fork scheduling, fixed 2026-10-28 (tool executions start only when the provider loop waits); title rename vs failed step (race), both runs |
| stop_retry_exhausted | the known extra ephemeral `project.updated` at the end of upstream's run (timing) |
| cli_help | by design: ocd's root and `upgrade` descriptions, `upgrade --yes`, the version |
| ~~cli_run~~, cli_inventory | ~~`run` output for the last step of a run (below)~~ identical since 2026-10-28; `debug paths` db name (channel), `debug agents` (upstream laziness race) |
| plugin_copilot_unsynced | network error wording: Bun's `ECONNREFUSED: Unable to connect. …` vs reqwest's message for the same refused proxy connection (by design, see "Error texts" in the README) |

`run` and the last step: upstream's `run` prints the `> agent · model` header and, with `--format json`, the
`step_start`/`text`/`step_finish` events only for steps whose live events reach its event stream before
`session.wait` answers. Upstream's server writes the event stream only while its runner waits for I/O, so the events
published after the runner's last wait (a step that fails at once, the last step of a fast provider) arrive after
`wait` and `run` prints that step from the projected messages instead (no header, no `step_start`/`step_finish`).
Fixed on 2026-10-28: ocd's `run` holds each live event for 150 ms and treats the held ones as arriving after `wait`
when the execution ends meanwhile (`crates/ocd/src/run.rs`, `Shared::consume`); scenario `cli_run_last_step`.

The CLI help, completions and parse errors of every `cli.json` golden case (help of every command, completion
scripts, ~70 parse errors) were also compared directly against the 2.1.4 binary: identical apart from the version
and the program name.

Run-ending paths (debug build, 2026-10-13): group `stop` 6 of 6 identical after the fixes #37-#38
(`stop_finish_reasons`, `stop_anthropic_finish`, `stop_stall_timeouts`, `stop_retry_exhausted`,
`stop_long_run_compaction`, `stop_background_shell`; `stop_retry_exhausted` once differed by an ephemeral
`project.updated`); group `loop` 18 of 21 identical, the three diffs being the known `loop_parallel_tools`,
`loop_tool_error` and `loop_http_400` ordering races. The new `loop_error_after_text` and `loop_location_reload_ask`
differ on the pre-fix binary (text dropped and a full retry instead of a continuation; the run interrupted as a
"declined" tool call).

Copilot + Responses WebSocket combined (debug build, 2026-10-13): groups websocket + plugin + loop in one run,
**33 of 34 identical** (`ws_*` 4 of 4, `plugin_copilot` and `plugin_copilot_unsynced` identical); the one diff is
the known `loop_tool_error` race. The mock now serves both batches' Responses scenarios with one event generator
(`created_at`/`model` in the response objects, which the AI SDK's Responses parser requires; separate title IDs).

GitHub Copilot (debug build, 2026-10-13): `plugin_copilot` and `plugin_copilot_unsynced` identical; groups plugin
(11 of 11) and loop (18 of 19, the known `loop_tool_error` race) show no new diffs. Also compared by hand per
route (gpt-5.4 `/responses`, grok without advertised endpoints `/responses`, gemini and gpt-5-mini
`/chat/completions`, claude native `/v1/messages`, claude unsynced `/chat/completions`): identical catalogs and
request bodies; on the AI SDK routes even header name case and order match (both send through Bun's `fetch`).

Responses WebSocket (debug build, 2026-10-13): group `websocket` **4 of 4 identical** (`ws_session`,
`ws_interrupt`, `ws_fallback`, `ws_plugin_hooks`): the same upgrades, the same frames (content and key order;
continuation frames with `previous_response_id`, full frames after a rejection or on a new socket), the same
closes, HTTP fallbacks and session events. Groups loop + plugin + websocket in one run: 30 of 32 identical; the two diffs
are the known `loop_tool_error` race and a `loop_http_400` title-rename vs failed-step race (identical in reruns;
the pre-change binary shows the same race in 2 of 4 runs).

AI SDK bridge (debug build, 2026-10-13): `plugin_aisdk` and `plugin_aisdk_hooks` identical (repeated runs);
upstream runs the fixture package in-process, ocd in its Bun sidecar. Both scenarios create the session with a
title: on the AI SDK route the step starts before the provider request (upstream `streamLanguage` emits
`step-start` first), so the mock's title hold cannot order title and first step, and the order is a race (see
"Inherent races"). By hand (network): `@ai-sdk/deepseek` under a provider id other than `deepseek` (so it is not
rewritten to a native package) installs `@ai-sdk/deepseek@ai-v6` (2.0.72) into the same cache layout in both
binaries and answers `ocd run` identically against the mock; `ocd run` with the fixture package prints the same
as upstream.

Remaining-items pass (debug build, 2026-10-13): `loop_llm_request`, `readonly_instructions` and the new
`loop_trace_headers`, `plugin_http_hooks` and `plugin_vcs_worktree` are identical; see "Fixed" #24-#32 for what
changed. Groups loop + readonly + plugin: 31 of 32 identical, the remaining diff being the known `loop_tool_error`
race; `plugin_hooks` once differed because upstream's own `glob` call failed (`ripgrep execution failed` on the
upstream side) and was identical in two reruns.

Regression run after the plugin integration (debug build, 2026-10-13, groups loop + session + plugin): 29 of 31
identical; the two diffs are the known `loop_tool_error` race and `loop_llm_request` tracing headers below.

Plugin group (debug build, 2026-10-13): **5 of 5 identical** (`plugin_inventory`, `plugin_tool`,
`plugin_hooks`, `plugin_effect`, `plugin_rpc_storage`), repeated runs stable except one
`plugin_hooks` diff seen once in seven runs and not reproduced (a timing race between hook events
and the step). Fixed on the way: plugin tool progress carried a location (upstream publishes it
from plugin code, outside the location scope), the inventory did not reflect a plugin disabled by
a failing transform, and the refresh events after that failure (`command.updated`,
`skill.updated`, `plugin.updated`) were missing because ocd only rebuilt domains lazily (it now
rebuilds every plugin-transformed domain once after a location's first activation).

Final full run (debug build, 2026-10-13): **38 of 45 scenarios identical**. The 7 remaining diffs are all
classified below; none is an unexplained behavioral difference.

| scenario | status | cause |
|---|---|---|
| loop_parallel_tools | diff | ripgrep walk order (race, changes between runs) |
| loop_tool_error | diff | unknown-tool failure vs `step.streamed` ordering (race) |
| loop_llm_request, readonly_instructions | identical since #24 | were: upstream's `b3`/`traceparent` tracing headers |
| cli_help | diff | by design: description/upgrade wording, version |
| cli_inventory | diff | `debug paths` db file name (channel), `debug agents` (upstream laziness race) |
| everything else (38) | identical | |

`cargo test -p oc-schema -p oc-server -p oc-tools -p oc-ai -p oc-backend -p oc-core` is green (47 result lines,
0 failures).

## Differences

### Fixed

| # | difference | upstream reference | fix |
|---|---|---|---|
| 1 | Numbers serialized as `0.0` (e.g. `cost`) instead of JS `0` | `JSON.stringify` number formatting | `oc-server/src/support.rs` `JsFormatter`/`js_number_string` (ECMAScript `Number::toString`) used for every JSON response, error body, SSE frame and PTY JSON message; unit tests for integral, small/large exponent, `-0`, NaN |
| 2 | `/api/experimental/session/stats` 500 (reported by the CLI agent) | protocol/src/groups/session.ts `session.stats` | not reproducible any more: every query the CLI sends (`from`/`to`/`timezone`/`tools` in all three modes, `--json`, `--all`) answers like upstream in `session_messages` and `cli_inventory`. A `NaN` range (unparsable `from`/`to`) now answers upstream's 500 (empty body) instead of a 400 |
| 3 | No `*.updated` notifications when a location boots | core/src/state.ts `batch`/`notify`, core/src/plugin.ts `activate` | `oc-backend/src/locations.rs` publishes `integration/provider/model/agent/command/skill/websearch/reference/plugin.updated` (and `vcs.branch.updated` for git repos, core/src/vcs.ts `refresh`) with the location |
| 4 | `project.updated` from location activation had no location | core/src/location-lifecycle.ts:32, core/src/bus.ts `publish` | `Locations::activate` takes the location |
| 5 | `project.updated` came later than upstream after `session.create` | core/src/session.ts `create` | `oc-backend/src/sessions.rs` boots the session's location right after creating it |
| 6 | `session.deleted`, `session.inbox.cancelled`, `session.inbox.delivery.changed` carried a location | routes without location middleware (protocol/src/groups/session.ts `session.remove`, `session.inbox.cancel`, `session.inbox.update`) | `oc-core/src/bus.rs` publishes them without location |
| 7 | "Always" permission replies were not saved (asked again) | core/src/permission.ts `reply`, `savedRules` | `PermissionContext::saved/save` take the session ID instead of a thread-local; regression test `oc-core/tests/permission_always.rs` |
| 8 | Validation errors used serde wording (`missing field ...`), accepted bad IDs (`id: "nope"`), rejected nothing upstream rejects | effect@4 `SchemaParser`/`SchemaIssue.makeFormatterDefault`, `SchemaAST.getCandidateIndex` | new `oc-server/src/validate.rs`: Effect-style check of every JSON body against the operation's request schema in the embedded openapi.json (missing keys, identifiers, union candidate selection, `string \| null`, brand prefixes, numeric/length checks, tuple `prefixItems`), `Expected a valid JSON body`, `Expected object`; query/params messages (`Expected an integer`, `a value greater than 0`, `"asc" \| "desc"`, `Vcs.Mode`, `a string starting with "ses"`) in `router.rs` + codegen (`query_enum` expected text, `query_pos_int`); unit tests with upstream's exact strings; `oc-backend` `forms_flow` test updated to upstream's empty-form answer |
| 9 | Relative `directory` (session list) / `location.directory` (session create) succeeded | upstream fails with a defect (500, empty body); `AbsolutePath` has no check | emulated in `oc-server/src/handlers.rs` and `oc-backend/src/sessions.rs` |
| 10 | Shell tool description lacked `Commands run on Linux using bash.` | core/src/tool/plugin/shell.ts `description(ShellSelect.name(compatibleShell))` | `oc-core/src/runner.rs` passes the compatible shell |
| 11 | Missing system part about `tools.opencode.session_move` | core/src/tool/plugin/opencode.ts:96 (context/compaction/generate hooks) | `oc-core/src/prompt.rs` `SESSION_MOVE_GUIDANCE` |
| 12 | Model request `User-Agent: opencode/<version>` | core/src/app.ts `useragent` = `opencode/<channel>/<version>/<name>` | `oc-core/src/runner.rs` |
| 13 | `fs.read` content type from `mime_guess` (`.rs` → `text/x-rust`) | util/src/fs-util.ts `mimeType` = npm `mime-types@3.0.2` | generated `oc-tools/src/mime_db.rs` (1239 extensions) + exact `lookup` algorithm (`extname("x." + path)`), used by `oc-tools` and `oc-backend` |
| 14 | New config/Markdown agents had no default permissions; no `browser` deny | core/src/agent.ts `editor.update` → `Info.default`; plugin-browser/src/connection.ts:27 | `oc-core/src/location.rs` `agents()` |
| 15 | OpenCode Zen provider absent without credentials | core/src/plugin/provider/opencode.ts:329-354 | `oc-ai` catalog plugin `OpencodeZen` (public key, paid models disabled) wired in `oc-core/src/models.rs` |
| 16 | `model.default` ignored the configured `model` | core/src/config/plugin/provider.ts:99 | `oc-core/src/models.rs` passes it to `ConfigProviders::with_default_model` |
| 17 | `localeCompare` approximation sorted punctuation by code point (`aiand` before `ai-router`) | V8/ICU root collation | `oc-schema/src/collate.rs` (DUCET punctuation order, accents, case), used by oc-ai, oc-core and oc-tools |
| 18 | `plugin.list` was empty | core/src/plugin/internal.ts (`pre`, provider/websearch plugins, `post`) | `oc-backend` reports the 88 built-in plugins as active |
| 19 | `azure-cognitive-services` / `google-vertex-anthropic` listed as integrations | core/src/plugin/models-dev.ts `snapshots` filter | `oc-ai` `models_dev::integrations` |
| 20 | Calls to known tools that were not offered (e.g. `patch` for non-GPT models) executed | core/src/tool.ts:275 | `oc-core/src/runner.rs` passes the advertised set to `Snapshot::execute` |
| 21 | `shell.created`/`shell.exited` had no location and `shell.created` included `pid` | core/src/shell.ts:424 publishes the pre-spawn `info` from a location service | `oc-core/src/services.rs` |
| 22 | Interrupt before any provider output left no step; the aborted request was retried | session/runner/step.ts (`pendingStartSnapshot` forked; `llmFailure` only for `AIError`) | `oc-core/src/step.rs` drives the start snapshot eagerly and ignores the abort error of an interrupted stream |
| 23 | Provider integration methods (key labels/forms, env names, removed key method for github-copilot, OAuth methods) and the exa/firecrawl/parallel/tavily/tinyfish integrations were missing; the oc-ai OAuth flows were never registered | core/src/plugin/provider/*.ts, core/src/plugin/websearch/*.ts, core/src/plugin/models-dev.ts | new `oc-core/src/integrations.rs` (registrations replayed after models.dev on every catalog build, in plugin order; the 10 oc-ai OAuth flows registered once; ChatGPT plugin storage in the `plugin:<hex id>:` kv namespace) wired from `oc-core/src/models.rs`; all 229 non-config integrations now match upstream in content and order; unit tests for copilot/openai/bedrock/exa method JSON |
| 24 | Outgoing requests lacked the `b3`/`traceparent` headers of Effect's traced HTTP client | effect 4.0.1 `HttpClient.make` (`http.client <METHOD>` span, `HttpTraceContext.toHeaders`), native tracer IDs (`Hex.random`) | measured on the upstream binary: one random 32-hex trace ID per **process** (shared by every execution, session and location; an incoming `traceparent` is not adopted), a fresh 16-hex span ID and a fresh 16-hex parent span ID per request (retries included), always sampled: `b3: <trace>-<span>-1-<parent>`, `traceparent: 00-<trace>-<span>-01`. New `oc-util/src/trace.rs`; set after the request hooks in `oc-ai` `transport::execute_with` (all model requests: steps, titles, compaction, generate) and on the other Effect-HttpClient requests (`webfetch`, web search providers incl. the MCP-based one, models.dev, remote skills, well-known manifests, ripgrep download, the Azure resource listing); not on upstream's plain-`fetch` requests (OAuth flow helpers, MCP transports). Scenario `loop_trace_headers`; `oc-ai/tests/http_hooks.rs` |
| 25 | Title requests had none of the session headers (`x-opencode-session-id`, `User-Agent`, `x-opencode-project`, ...) nor the prompt cache key | session/model-request.ts `prepare` (titles go through it too) | `Engine::session_http` shared by step/compaction/generate/title requests (`oc-core/src/runner.rs`, `title.rs`) |
| 26 | `session.http.request` / `session.http.response` plugin hooks were not run | session/model-request.ts:337-366, ai/src/route/executor.ts `executeOnce` | `oc-ai` `HttpHook` (per call, `Client::stream_with`/`generate_with`): the request hook sees the prepared request (`content-length` included, before trace headers), the response hook sees status, empty `statusText`, headers and the body (read in full; only when a plugin registered `http.response`), then the status check runs on the hook's response; hook failures are transport errors. `oc-core/src/plugin_request.rs` `http_hooks` builds it from the plugins (`{sessionID, agent, model, kind, request[, response]}`) for primary, compaction, generate and title requests. Scenario `plugin_http_hooks`; `oc-ai/tests/http_hooks.rs` |
| 27 | Hook events carried `model: {providerID, modelID}` | `Model.Ref` = `{id, providerID, variant?}` | `plugin_request.rs` `model_json` (all `session.*` hooks) |
| 28 | No VCS provider / worktree strategy registry: plugin `vcs` and `worktree` transforms had no effect | core/src/vcs.ts, core/src/worktree/strategies.ts, core/src/worktree.ts, core/src/plugin/host.ts:475-532 | new `oc-core/src/vcs.rs` (`Registry`, `Provider`, `Vcs` with upstream's `protect`/`review` fallbacks and the 10 MB budget) and a strategy registry in `oc-core/src/worktree.rs` (`Strategies`, `Strategy`; create uses the selected strategy, remove the stored one, refresh asks every strategy newest first; `Worktree strategy <id> failed to <op>: ...`, `Worktree strategy unavailable: ...`, `Worktree directory unavailable: ...`); `Plugins::vcs` / `Plugins::worktree_strategies` run the transforms with the built-ins as `rs:vcs.git.*` / `rs:worktree.git.*` handles (plugins can wrap them); the backend's `vcs.*` and `worktree.*` endpoints and the worktree tools use them (strategies from the project's canonical location). `WorktreeError.data.forceRequired` is now `null` when unset, like upstream. Scenario `plugin_vcs_worktree`; `oc-backend/tests/plugins.rs` `vcs_and_worktree_transforms`; `vcs.rs` unit tests |
| 29 | ChatGPT token sharing read the saved client id from the selected `openai` credential row | core/src/plugin/provider/chatgpt.ts:218-237 | `oc-ai` `active_connection_client_id`: `Integration.connection.active("openai")` (stored credentials, then env-var connections) → `resolve` (expiring tokens refreshed; a failure means none) → `metadata.clientID` of a `chatgpt-token-sharing` OAuth value that passes `decodeMetadata`; `SavedClientId` is async. Test `credentials_integration.rs` `chatgpt_saved_client_id_follows_the_active_connection` |
| 30 | Azure `azure-cli` connect form never listed resources | core/src/plugin/provider/azure.ts:374-408 | `oc-core/src/integrations.rs` `list_azure_resources` (signed-in `azureProfile.json` under `$AZURE_CONFIG_DIR` or `<home>/.azure`, BOM stripped; management token from `az account get-access-token`; Azure Resource Graph query with `$top: 1000`; options `{value, label, description: "<group> · <location>"}` for names matching the resource pattern), started once per location in the background like upstream; the form then offers the resources (`custom: true`) or says none were found. `configuredResource` now treats an empty `AZURE_RESOURCE_NAME` as winning (`??`). Test `oc-core/tests/azure_resources.rs` with a fake `az` on `PATH` and a local Resource Graph mock |
| 31 | Plugin-registered integrations and web search providers were process-wide; the "always" re-check of other pending permission requests skipped the `permission.evaluate` hook and dropped their metadata/source | core/src/integration.ts and core/src/websearch.ts are location nodes; core/src/permission.ts `reply` | `Models` keeps one `Integrations` registry per location (credentials stay global; OAuth implementations, the ChatGPT client lookup and the Azure listing per registry; dropped on location close); backend integration endpoints and the plugin `integration.connection.*` API use the location's registry. `Plugins` keeps one web search service per location forked from the built-ins (plugin providers, plugin `default.set` selection, config selection, cooldowns, random affinity per location; built-ins a plugin re-registers keep their executor through `rs:websearch.<id>`); the tool resolves it through `ToolHost::websearch`. `Permissions::reply` is async and re-evaluates through `evaluate_hooked` with the stored request. Tests: `oc-backend/tests/plugins.rs` (second location sees neither the plugin's integration nor its provider), `permission.rs` unit test `always_recheck_runs_the_evaluate_hook` |
| 32 | A location's plugins could load twice when the runner and the backend booted it at the same time (a fixture plugin's `boots` counter read 2) | core/src/plugin.ts (one activation per location) | `Plugins::ensure` claims the configuration before spawning the load |
| 33 | `aisdk:` packages (AI SDK providers without a native implementation) failed with `Unsupported package`; plugin `aisdk.*` hooks were inert | core/src/aisdk.ts, core/src/model-resolver.ts `loadAISDK`, core/src/plugin/provider/{dynamic,sdk-factory,cohere,perplexity,vercel,gitlab,sap-ai-core}.ts, core/src/plugin/host.ts:157 | AI SDK bridge: oc-ai `src/aisdk.rs` (route, `callOptions` lowering, stream-part raising, `llmError` from error shapes), oc-plugin `src/aisdk.rs` + `host/src/aisdk.ts` (sidecar: package install/import, internal and plugin `aisdk` hooks, `prepareOptions` fetch with timeouts/extra body/HTTP hooks, `doStream`), oc-core `src/aisdk.rs` (`LocationBridge`) and `Models::resolve_model` (eager load at resolution, `ModelInitializationError` phase `init`). Scenarios `plugin_aisdk`, `plugin_aisdk_hooks`; `oc-plugin/tests/aisdk.rs`, `oc-backend/tests/aisdk.rs` |
| 34 | GitHub Copilot models kept models.dev's packages (native OpenAI/Anthropic/OpenAI-compatible routes at api.githubcopilot.com, without Copilot headers); the plugin's `/models` sync, `sdk`/`language` hooks, `session.model.request` / `session.title` / `session.http.request` hooks and the `gpt-5-chat-latest` model transform were missing | core/src/plugin/provider/github-copilot.ts:159-345, core/src/github-copilot/ | oc-ai `catalog/copilot.rs` (live-model decoding, `derive`/`build`, the provider/model transforms) and `js_date.rs` (`Date.parse` incl. the legacy format, for release dates from model versions; 106k-string differential test against Bun); oc-core `copilot.rs` (per-location loaded state: initial load when the location's catalog is first built, reload on `credential.switched`; interaction headers, utility title, native-route HTTP hook, which also routes every Copilot request through the HTTP middleware as upstream); sidecar `host/src/aisdk.ts` internal plugin `opencode.provider.github.copilot` with upstream's in-tree provider (`core/src/github-copilot`) bundled from the checkout. Also: the sidecar's `ctx.app.name` is now `cli` (was `opencode`; upstream's server passes the CLI's name), a `session.title` hook `result` is used verbatim (was: its first non-empty line), and a switched session model keeps `variant: "default"` in the step's model ref. Scenarios `plugin_copilot`, `plugin_copilot_unsynced`; `oc-backend/tests/copilot.rs` |
| 35 | No Responses WebSocket transport: every model streamed over HTTP SSE | ai/src/protocols/open-responses-channel.ts, open-responses-continuation.ts, openai-responses.ts `channelTransport`, providers/{azure,xai}.ts, route/transport/websocket*.ts; core/src/session/model-transport.ts, session/model-request.ts:367-391, session/runner/llm.ts, plugin/provider/{openai,xai,azure}.ts | oc-ai `route/websocket.rs` (channel exchange types, tokio-tungstenite connector, close codes as Bun reports them) and `protocols/open_responses_channel.rs` (`response.create` message, base and continuation drivers, rejections); the `channel` of the openai/azure/xai Responses routes; `Client::stream_opts` (`StreamOptions.web_socket`; the HTTP fallback keeps the `session.http.*` hook and trace headers, the handshake has neither); catalog plugin `ResponsesWebSockets` (`transport: "websocket"` default for openai, xai and capable azure providers). oc-core `model_transport.rs` (`SessionModelTransport`: one socket per session, reuse, rotation on handshake change or age, checkpoint staged on completion and committed after the route stream ends, close after error frames, poison on dropped/incomplete exchanges, HTTP pinning after a failed connect, a 1009 rejection or 5 lost exchanges, 15 s connect and 30 min / `chunkTimeout` idle limits); the runner binds it for primary steps of `transport: "websocket"` models with the `experimental.ws.*` plugin hooks (`plugin_request.rs` `ws_hooks`) and closes it on session remove/move, location close and server shutdown (normal closure). Group `websocket`; `oc-core/tests/model_transport.rs`, unit tests in oc-ai |
| 36 | Step events of a session created with a model but no variant lacked `variant: "default"` | core/src/model-resolver.ts `resolveModel` (`...(variant === undefined ? {} : { variant })`) | `oc-core/src/models.rs` `resolve_for_session` keeps the requested variant (`"default"` included) in the model ref, the same change as in #34 (seen in `ws_fallback`) |
| 37 | An error frame read together with earlier frames (text, then `finish_reason: network_error`/unknown, or an Anthropic `error` event) dropped the earlier frames' events: no text was recorded and the step was retried from scratch instead of continued; the error body was the re-serialized event instead of the wire text | ai/src/route/client.ts (per-frame `Stream.mapEffect`, `frameError`) | `oc-ai/src/client.rs` `StreamDriver::push_events`/`finish_events` yield every event decoded before the failure, then the failure; a body equal to the re-serialized frame is replaced by the frame text. Scenario `loop_error_after_text`; tests in `oc-ai/tests/openai_chat.rs`, `anthropic_messages.rs` |
| 38 | A location shut down (`location.reload`, `debug.location.evict`, `vcs.init`) while a permission ask or question was pending ended the run as a user decline (`execution.interrupted`, reason `shutdown`) | session/runner/step.ts:211-219, 278-279 (`isLocationClosed`), llm.ts:75-78 (`Reloaded`) | `oc-core` `Location::is_closed` (set by `Engine::close_location`), passed to the step; the run continues on a fresh load. Scenario `loop_location_reload_ask`; `oc-core/tests/stops.rs` |
| 39 | A panicking tool task read as an interrupted step (the run ended silently, reason `shutdown`); a panicking drain left the session active forever with no terminal event | step.ts `classifyToolExits` (a dying tool fiber fails its call), execution.ts `terminal` (a dying drain → `execution.failed`) | `oc-core/src/step.rs` (panic → tool defect, the call fails with the message, the run continues), `coordinator.rs` (`catch_unwind` → failed execution). `oc-core/tests/stops.rs`, `tests/coordinator.rs` |
| 40 | An interrupt during a retry wait (up to 15 min with `Retry-After`) took effect only after the wait | retry.ts `wait` (an interrupted `Effect.sleep`) | `oc-core/src/retry.rs` `wait` selects on the cancellation; no synthetic continue notice after an interrupted wait. `oc-core/tests/stops.rs` |
| 41 | TUI: an escape sequence split across terminal reads (mouse motion, focus, kitty key reports; common over SSH) became an Esc press plus typed text, so two of them within 5 s interrupted the running session and put `[<35;10;10M` into the prompt | OpenTUI `StdinParser` holds a lone ESC 20 ms (`DEFAULT_TIMEOUT_MS`) | `oc-tui/src/app/escape.rs` `JoinEscapes` (20 ms window, reassembled sequences decoded or dropped) around crossterm's `EventStream` in both modes; unit tests there; reproduced with `tui_capture.py` (ESC, 5 ms, rest) |
| 42 | A plugin hook call that lost its sidecar (crash mid-call) failed the caller, e.g. a step's `session.context` hook ended the run with `Plugin host is not running: exited` (upstream runs plugins in-process, so this cannot happen there) | plugin/src/promise/adapter.ts (hooks in-process) | `oc-plugin/src/host.rs` `trigger_hook` treats a closed sidecar like one that is not running (hook skipped, warning logged); `oc-plugin/tests/sidecar.rs` `hook_is_skipped_when_the_sidecar_dies_during_the_call` |
| 43 | A ChatGPT sign-in changed nothing but the credential: Codex requests went to api.openai.com without `originator`/`chatgpt-account-id`, every OpenAI model stayed listed with its price, token sharing used the WebSocket route and native compaction, and provider `openai` requests carried `max_output_tokens` | core/src/plugin/provider/openai.ts, chatgpt.ts, openai-context-caps.ts (v2.1.4) | oc-ai `catalog/openai.rs` (`ChatGpt`, `OpenAi`, `OpenAiContextCaps` catalog plugins keyed on the active connection's `SignIn`; credential-metadata migration helpers), oc-core `openai.rs` (per-location loaded sign-in), `models.rs` (load on catalog build and on active-connection change, the `chatgpt-browser` → `chatgpt-token-sharing` move), `plugin_request.rs` (output limit, `session.model.request` base URL and headers, token-sharing retry stop; `has_retry_hook` for the runner). Checked against the 2.1.4 build with fake credentials: catalog listings and mock-received requests identical. `oc-ai/tests/catalog_openai.rs`, `oc-core/tests/openai_sign_in.rs` |
| 44 | `opencode.prompt.verbosity` was listed but did nothing: OpenAI Responses requests lacked `text.verbosity: "low"` | core/src/plugin/verbosity.ts | oc-core `verbosity.rs` (`defaults_to_low`), applied in `plugin_request.rs` `shape` from the location's latest catalog (`Plugins::catalog`) on step, compaction, generate and title requests. Unit test ported from `verbosity.test.ts`; `oc-core/tests/openai_sign_in.rs` |
| 45 | Config `policies` did nothing: `provider.use`, `integration.use` (MCP servers, skills) and `tool.use` statements were parsed but never applied | core/src/config/plugin/policy.ts, managed-policy.ts (v2.1.4) | oc-core `policy.rs` (catalog plugin after the config providers, `Plugins::mcp_servers`/`skills` filters, `permission.evaluate` deny with "Blocked by configuration policy" in `app.rs`), oc-config `policy.rs` reading top-level `policies`. Checked against the 2.1.4 build (batch 4): provider list, MCP list and a denied shell call identical |
| 46 | Config documents went into the agent fold twice, so top-level and agent `permissions` were appended twice to every agent | core/src/config/plugin/agent.ts | oc-core `location.rs` `agents_from` folds `agent_documents(entries)` only; agent list identical to 2.1.4 |
| 47 | `shell.exited` carried `"exit": null` for a command without an exit code (killed, timed out) | core/src/shell.ts `Exited` | oc-core `services.rs` omits `exit` when absent |
| 48 | Plugin tool failures were always `tool.execution`; upstream takes the session error from the `Tool.Error` cause (`unknown` for a thrown Error, `permission.rejected` for a rejected `ctx.permission.assert`) | core/src/session/to-session-error.ts, plugin/src/promise/adapter.ts (v2.1.4) | oc-plugin host `location.ts` `sessionCause` sent as `data.cause`, oc-core `plugins.rs` `with_cause` |
| 50 | A session without its own model failed with `Model unavailable: <config model>` when the configured model was not in the catalog (2.1.4's models.dev snapshot dropped `github-copilot/gpt-5.4`); upstream falls back to the first available text model | core/src/model.ts `Model.default`, model-resolver.ts `resolve` (v2.1.4) | oc-core `models.rs` `resolve_for_session` takes the configured model only when the catalog has it enabled with a package, else `Catalog::select(None)` |
| 51 | A Responses WebSocket dropped without a close frame reported the body `Connection ended`; 2.1.4 (Bun 1.4.4) reports an empty close reason | ai/src/route/transport/websocket.ts `onClose` (`body: event.reason`) | oc-ai `route/websocket.rs` sends an empty reason for 1006 |
| 49 | `project.list`/`project.update` did not boot the request's location, so `GET /api/project` neither resolved nor (2.1) unarchived the caller's project first | protocol/src/api.ts `ProjectGroup.middleware(locationMiddleware)` | `oc-backend` `project_list`/`project_update` call `locations.get` first. Checked against the 2.1.4 build (batch 5) |

### By design (not fixed)

| difference | reason |
|---|---|
| The GitHub Copilot plugin's own HTTP calls (the `/models` sync, the utility title) are sent by ocd's Rust HTTP client: lowercase header names, `accept-encoding: gzip,br`, no `Connection: keep-alive` (upstream: Bun's `fetch`); the first catalog build of a location waits for the initial model sync (at most 5 s), where upstream syncs in the background after activation and then reloads the providers (core/src/plugin/provider/github-copilot.ts:172-251) | same URLs, header values and bodies; the model requests go through Bun in both (AI SDK routes) or the native routes as for every provider |
| `--version`, report-skill diagnostics, User-Agent channel/version | local build `0.0.0-detached-…`/channel `detached` (before: `0.0.0-v2-…`/`v2`) vs port 2.1.4/`latest`; masked |
| `debug paths` db `opencode-detached.db` (before: `opencode-v2.db`) vs `opencode.db`; service config file name | derived from the channel of the local build |
| Web UI | the local upstream build is made with `--skip-web-ui` (404 for web pages); ocd embeds the UI, so the harness points ocd at an empty web UI directory (`OCD_WEB_UI`, unless already set) |
| Program name `ocd`: help `DESCRIPTION` reads `ocd command line interface` (upstream `OpenCode command line interface`), completion function names | rename; masked where it is just the name |
| `upgrade` help: ocd's description ("Upgrade ocd to the latest GitHub release, or to the given version"), the `--method` description ("Ignored: ocd installs only from its GitHub releases") and the extra `--yes, -y` flag | ocd upgrades from its own GitHub releases, not through upstream's install methods |
| `pair --yes` parses but has nothing to accept | ocd has no OpenTunnel, so `pair --remote` stops before upstream's consent prompt |
| `POST /api/debug/heap-dump` writes a V8 heap snapshot upstream; ocd answers `500 UnknownError` ("Failed to write heap snapshot: ocd has no JavaScript heap, …"), after the same `400` for a relative directory | no JavaScript heap in ocd |
| Upstream 500s on bad input reproduced on purpose (relative directories, `NaN` stats range) | deterministic upstream behavior kept |

### Inherent races and upstream quirks (not fixed, worked around in the harness)

| behavior | upstream reference |
|---|---|
| Title rename vs first step: commit order depends on which LLM call finishes first | core/src/session/runner/llm.ts:174 forks `title.generate` |
| On the AI SDK route the step starts before the provider request goes out, so title vs first step is a race even with the mock's title hold (ocd's title round trip through the sidecar is slower than upstream's in-process call); the `plugin_aisdk*` scenarios use titled sessions | core/src/aisdk.ts `streamLanguage` (`stepStart` first) |
| Location services are built lazily and filled by asynchronously activated plugins: the first `agent.list` of a location answers `[]` (so `ocd debug agents` prints agents where upstream prints `[]`), early permission asks evaluate to `deny`, the plan plugin misses an agent switch right after boot, config agents/commands/skills appear only after the config plugins' debounced reload | core/src/state.ts, core/src/plugin.ts `activate`, core/src/config/plugin/agent.ts reload |
| First `vcs.get` of a location answers `{"branch": {}}` while the forked refresh runs | core/src/vcs.ts `refresh` via `State.reconcile` |
| `location.reload` boots the replacement location's plugins asynchronously; an evict right after it cancels that (no boot notifications) | core/src/location-service-map.ts `reload` |
| `glob`/`grep` result order is ripgrep's parallel walk order (both binaries run `rg` with the same arguments; order changes between runs) | core/src/ripgrep.ts |
| An unknown-tool failure and `session.step.streamed` race (concurrent tool fibers vs stream end) | session/runner/step.ts |
| An HTTP 400 on the first step and the forked title rename can commit in either order (`loop_http_400`, occasionally) | core/src/session/runner/llm.ts:174 |
| `b3`/`traceparent` header values are random (one trace ID per session drain and the process's own outside one, fresh span IDs per request except a compaction's requests, which share one) | Effect's native tracer; normalized by the harness, see #24 |
| `run` without `-m` used to pick OpenCode Zen's free model in both (network) | caused by the harness passing the parent's `PWD`, which both binaries prefer over the working directory; the harness now sets `PWD` to the project (2026-10-25) |
| Plugins activate asynchronously after a location boots: an early `plugin.list` (e.g. `ocd plugin list` against a fresh standalone server) shows only built-ins in both binaries; the plugin scenarios wait for `plugin.updated` and settle the lists | core/src/plugin/supervisor.ts |

### Remaining

`plugin add` was compared by hand (network): same output and npm cache layout
(`opencode-models-discovery@1.10.0`).

| difference | upstream reference | note |
|---|---|---|
| Session creation boots the location synchronously, so the nine boot notifications follow `project.updated` immediately; upstream publishes them ~0.5 s later | core/src/plugin.ts `activate` (async) | timing only; same events, same order |
| `session.http.response` reads the response body in full before the hook runs (upstream hands the hook a streaming `Response`); bodies are passed as UTF-8 text | session/model-request.ts:355 | only when a plugin registered `http.response`; the hook sees the same text |
| No Mercurial VCS provider: an hg repository has no built-in provider (vcs endpoints answer empty, `vcs.init?provider=hg` answers 503) | core/src/plugin/vcs/hg.ts | the registry is ready for it (`oc-core/src/vcs.rs` `Registry::native`) |
| Before a location's first VCS refresh, `vcs.get` reads the info from the provider; upstream answers `{branch:{}}` until its refresh (about 1.5 to 2 s after the first request). After that both answer the cached info, refreshed when `HEAD` (hg: `.hg/branch`) changes, and announce `filesystem.changed` and `vcs.branch.updated` the same way | core/src/vcs.ts `info`, filesystem/location-watcher.ts | timing only; checked side by side with checkout, commit, detach and re-checkout |
| AI SDK packages upstream bundles in its binary (`@ai-sdk/cohere`, `@ai-sdk/perplexity`, `@ai-sdk/vercel`, `gitlab-ai-provider`) are installed from npm on first use, at upstream's pinned versions | core/package.json, core/src/plugin/provider/{cohere,perplexity,vercel,gitlab}.ts | first use needs the registry |
| An AI SDK package install failure reads `Failed to install <spec>: <bun output>` in the initialization detail (upstream: the npm/arborist error message) | util/src/npm.ts `InstallFailedError` | ocd installs with `bun add` |
| Third-party packages for upstream's own runtime (exporting `model(modelID, settings)` for `@opencode/ai`, loaded by `Provider.loadPackage` from npm) are not supported: `Provider package <x> is not installed` | core/src/provider.ts `loadPackage` | AI SDK packages (`aisdk:`) are |
| The worktree tools use the strategies of the project's canonical location only when that location is already loaded (the HTTP endpoints boot it first) | core/src/worktree.ts `load` | the tool path falls back to the git strategy |

## CLI-crate differences (for the CLI owner; `crates/ocd` was not edited)

Everything else in `cli_help` (root, 17 subcommands, 29 nested subcommands, unknown flag, bash completions),
`cli_run` (`run` default and `--format json` with a tool call, `run` without a message) and `cli_inventory`
(`models`, `stats` three ways, `session list`, `debug config`, `mcp list`, `plugin list`, `auth list`,
`service status/get`) is identical after masking the program name.

| command | upstream | ocd | classification |
|---|---|---|---|
| `--help` / `-h` DESCRIPTION | `OpenCode command line interface` | `ocd command line interface` | upstream prints the literal product name regardless of the binary name; by design if the port wants its own name, otherwise use the literal |
| `upgrade --help` and the root SUBCOMMANDS line | `Upgrade OpenCode to the latest or a specific version` | `Show how to upgrade opencode (it is built from source)` | by design |
| `--version` | `opencode v0.0.0-detached-…` | `opencode v2.1.4` | build version |
| `--completions bash` | `_opencode…` identifiers | `_ocd…` identifiers | by design (derived from the program name; masked) |
| `debug paths` | `db …/opencode-detached.db` | `db …/opencode.db` | channel (`detached` local build vs `latest`) |
| `debug agents` | `[]` | the agent list | upstream race: the first `agent.list` of a fresh standalone server answers `[]` (lazy Agent state); ocd's answer is the intended one |

## Safety notes

All runs use isolated `XDG_DATA_HOME`, `XDG_CONFIG_HOME`, `XDG_STATE_HOME`, `XDG_CACHE_HOME`, `HOME` and
`OPENCODE_TEST_HOME` under the scratchpad; servers bind `--port 0`; the background service is disabled in the
isolated config (`service.json` and `service-<channel>.json` for `v2`, `local`, `detached`, `latest`, `dev` and `beta`,
with `{"disabled": true}`) and
server-backed CLI commands also get `--standalone`. The real `~/.local/share/opencode` is never touched. Both
binaries run the local model discovery plugins by default, which ask `127.0.0.1:1234`, `:11434` and `:8000`; the
isolated global config therefore has an `opencode.jsonc` (a file no scenario writes) setting the `baseURL` of
`lmstudio`, `ollama` and `vllm` to the dead port `127.0.0.1:9`, and the `local` scenarios configure their own mock
servers on top. (The configured providers are `enabled`, so every scenario's provider, model and integration lists now
include LM Studio's models.dev models and `ollama`/`vllm` entries, the same on both sides.)

# Differential parity: upstream opencode v2 vs ocd

Upstream reference: since the 2.1.4 catch-up, a local build of `anomalyco/opencode` at tag `v2.1.4` (commit
`68bc26b`, built with `--skip-web-ui`; reports `0.0.0-detached-<timestamp>`, channel `detached`). Port:
`target/debug/ocd` (workspace version 2.1.4, channel `latest`). Results before 2026-10-27 were against the build of
branch `v2` at commit `4617210` (reports `0.0.0-v2-<timestamp>`, channel `v2`; ocd then reported 2.0.26), and the
upstream sources cited below are relative to `packages/` of that commit unless they say otherwise.

## Running the suite

```sh
export PATH=$HOME/.cargo/bin:$PATH
cargo build -p ocd                          # debug build is enough
python3 scripts/parity.py --list            # scenarios and groups
python3 scripts/parity.py -q                # everything (about 25 minutes with a debug build)
python3 scripts/parity.py --group session   # one group: loop session permission form readonly errors cli plugin websocket stop todo
python3 scripts/parity.py 'loop_*' cli_run  # by name or glob
```

Options: `--upstream BIN` (default: `$OCD_SCRATCH/up-dist/cli-linux-x64/bin/opencode`, or
`$PARITY_UPSTREAM`), `--rust BIN` (default `target/debug/ocd`), `--out DIR` (default `$OCD_SCRATCH/parity-out`),
`-q` prints only the per-scenario status. Each scenario writes `<out>/<name>/{upstream,rust}.txt` and `diff.txt`;
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

- generated IDs (`<prefix>_` + 12 hex + 14 base62, also with derived suffixes like `msg_…_4`), renumbered by
  first appearance; UUIDs;
- epoch-millisecond numbers (any key), ISO timestamps, `duration`/`elapsed`-style keys, `pid`;
- session `slug`s;
- the build version (`0.0.0-v2-…` or `0.0.0-detached-…` upstream, `2.0.26` or `2.1.4` port) wherever it appears,
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

## Scenarios (72)

| group | scenarios |
|---|---|
| loop | trace headers of every outgoing request (title, steps, `webfetch`) over two prompts incl. an incoming `traceparent`, text only, read tool, reasoning, three parallel tools (read/glob/grep), write + edit, patch, shell tool, tool errors (missing file, unknown tool, malformed JSON args), HTTP 500 (retry scheduled, then interrupt), HTTP 400, 429 + Retry-After, stream cut mid-way, context overflow → compaction, manual compaction, steer/queue inbox (list/update/cancel), session `!` shell + synthetic, slash command + skill activation, full first LLM request (system prompt, tool schemas, headers), text followed by a failing finish reason in the same read (`network_error`, an unknown one: the text is kept and the run continues), a location reload while a permission ask is pending (the run continues) |
| stop | how a run ends (slow, real-time retries and timeouts): chat-completions finish reasons (`stop` with tool calls, `tool_calls` without, `length` mid tool call and on text, `content_filter`, unknown, `network_error`, an empty stream, no `finish_reason`, no `[DONE]`), Anthropic stop reasons (`end_turn` with a tool call, `max_tokens`, `pause_turn`, unknown, a stream cut before `message_delta`, an `error` event after text), `chunkTimeout`/`headerTimeout` stalls up to the timeout-retry cap, HTTP 503 until the 10 retries are spent, a 24-step tool run with an auto compaction and an overflow compaction in the middle, a background shell command resuming a finished turn |
| session | CRUD (create/get/update title/metadata/permissions, list with limit/cursor next+previous/order/search/parentID/directory, bad cursor/limit, active, remove incl. children), messages (paging, get, context, inbox, diff, log, export, instruction entries, stats in three tool modes), export/import, fork (all / before), revert (stage with files, clear, stage, commit) in a git repo, interrupt of a hanging request, experimental wait, agent/model switch incl. plan reminders |
| permission | read `.env` → ask → once / always (second read auto-approved) / reject / reject with feedback; client-created requests, saved list/remove, errors |
| form | form API (create, list, get, invalid answers, reply, reply again, cancel, global list, invalid field), question tool round trip |
| readonly | server info, location, config, shells, agents, commands, skills, models (+default), providers, integrations, MCP, plugins, credentials, websearch providers, references, forms, permissions, PTYs, shells, projects, debug locations, v1 migration status; fs list/read/find (incl. traversal and content types); vcs in a git repo (status, branches, base, three diff modes, bad mode, worktree list) and outside one (incl. `vcs.init`); instruction entries + full LLM request with AGENTS.md/skill/command present; debug location evict + reload |
| errors | 25 not-found shapes, 42 validation cases (params, query, payload, invalid JSON, empty body, wrong content types), 7 auth cases |
| cli | `--help` for the root and all 17 subcommands and 29 nested ones, `--version`, unknown flag, bash completions; `run` default and `--format json` with a tool call, `run` without a message; `models`, `stats` (+`--json`, `--all`), `session list`, `debug paths/config/agents`, `mcp list`, `plugin list`, `auth list`, `service status/get` |
| websocket | a configured provider on the native OpenAI package with `transport: "websocket"` against the mock's Open Responses endpoint; every upgrade (headers without the random key, host and Bun's `permessage-deflate` offer), frame (body with instructions/tool schemas as digests, key order) and close is compared. `ws_session`: five prompts on one session, a read tool round trip and the answer as continuation frames (`previous_response_id` + only the new input) over one socket, a `previous_response_not_found` error frame (rejected → step retried in full on a new socket), a server error frame (retry on a new socket), a socket dropped mid-answer (`WebSocket closed with code 1006`, continue), a close frame 1011 before any output (retry). `ws_interrupt`: an interrupt while the socket waits closes it; the next prompt reconnects in full. `ws_fallback`: a refused upgrade pins the session to HTTP (both steps POST with tracing headers), a second session gets its own socket, a provider with `transport: "http"` never opens one. `ws_plugin_hooks`: oc-plugin's `ws-plugin` fixture (`experimental.ws.handshake/send/receive` rewriting the upgrade URL and headers, each sent frame and the received text; `session.http.request` marking HTTP requests): the title and, after an error frame and a refused reconnect, the HTTP fallback carry the HTTP hook's header, the socket traffic carries only the ws hooks' marks |
| plugin | oc-plugin's fixture plugins (`crates/oc-plugin/tests/fixtures`) copied into the project and configured as local plugins; the port runs them in its Bun sidecar (`OCD_BUN_PATH`, default `$OCD_SCRATCH/bun144/bun-linux-x64/bun`), upstream in-process. `plugin_inventory`: Promise + Effect + invalid + failing-transform plugins → `plugin.list` (order, sources, failures, the `skill.transform` failure disabling its plugin), `command.list`, `agent.list`, `skill.list`, boot and failure events. `plugin_tool`: a plugin tool offered to the model (tool names and full definitions of the first request) and executed with `tool.execute.before` rewriting its input, tool progress, a plugin command writing storage, the storage rows. `plugin_hooks`: `permission.evaluate` allowing/denying `.env` reads, `tool.execute.after` replacing a result, `session.prompt` rewriting the prompt, `session.context` adding a system part, `shell.create.before` adding an env variable. `plugin_effect`: agent/skill transforms and a rejecting `tool.execute.before`. `plugin_rpc_storage`: `rpc.call` (success, declared error, invalid input, unknown method, unknown RPC), storage incl. `ctx.event.subscribe` rows. `plugin_http_hooks`: `session.http.request` rewriting headers and body of every model request (title + steps; what the hook saw is echoed in a header) and `session.http.response` rewriting the streamed body. `plugin_vcs_worktree`: a plugin VCS provider selected with `default.set` (get/base/status/branches with search+limit/three diff modes incl. a failing one, `vcs.init` conflict) and a plugin worktree strategy (create, list, refresh, remove without and with `force`). `plugin_aisdk`: a configured provider whose package is oc-plugin's fixture AI SDK provider (`aisdk:file://.../aisdk-provider/index.js`, a LanguageModelV3 package talking to the mock through the SDK `fetch`): a read tool call, reasoning + text with cached/reasoning usage, an HTTP 400 (`APICallError`), a stream cut mid-way, then a model whose package does not exist (initialization failure); every request the provider sent (V3 prompt, tools, call headers, provider options, SDK options) is compared. `plugin_aisdk_hooks`: the same provider with plugin `aisdk.sdk` / `aisdk.language` hooks (wrapped SDK and language model) and `session.http.*` hooks around the SDK's fetch. `plugin_copilot`: the built-in GitHub Copilot plugin against the mock as a Copilot API (a fake OAuth credential created through the API, its `apiEndpoint` metadata = the mock; HTTPS goes to a dead proxy): the `/models` sync, the derived catalog, a `/responses` tool round trip (gpt-5.4), `/chat/completions` (gemini) and the native `/v1/messages` route (claude) after model switches, the utility-model title; every request the mock received (headers lowercased and sorted, transport headers dropped). `plugin_copilot_unsynced`: the same with a failing sync (404): every models.dev model through the in-tree AI SDK provider at the connection's API base |
| todo | the experimental todo list (`experimental.todo`). `todo_loop`: the model writes a three-item list, sends an invalid status (input validation error), updates it (a completed long item, a cancelled one, quotes in the in-progress summary), clears it and answers; every full model request (the `todowrite` definition and description, the `execute` catalog without it), the tool results, stored parts, durable and ephemeral events and `plugin.list`. `todo_off`: with `todo: false` the tool is not offered and a call is an unknown tool while `opencode.tool.todo` stays listed; then the config file turns it on (file watching enabled for this scenario) and the next request offers and runs it. `todo_permission`: `permission: {todowrite: "ask"}`, the first ask rejected with feedback, the second answered `always` (saved `todowrite *`) |

## Results

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
| Web UI | the local upstream build is made with `--skip-web-ui` (404 for web pages); ocd embeds the UI built from v2.1.4, so the parity runs point ocd at an empty archive (`OCD_WEB_UI`) for `errors_not_found` |
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
| `b3`/`traceparent` header values are random (one trace ID per process, fresh span IDs per request) | Effect's native tracer; normalized by the harness, see #24 |
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
server-backed CLI commands also get `--standalone`. The real `~/.local/share/opencode` is never touched.

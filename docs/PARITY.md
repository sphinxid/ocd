# Differential parity: upstream opencode v2 vs ocd

Upstream reference: the shipped build of `anomalyco/opencode` branch `v2` at commit `4617210` (reports
`0.0.0-v2-<timestamp>`, channel `v2`). Port: `target/debug/ocd` (workspace version 2.0.26, channel `latest`).
Upstream sources cited below are relative to `packages/` of that commit.

## Running the suite

```sh
export PATH=$HOME/.cargo/bin:$PATH
cargo build -p ocd                          # debug build is enough
python3 scripts/parity.py --list            # scenarios and groups
python3 scripts/parity.py -q                # everything (about 25 minutes with a debug build)
python3 scripts/parity.py --group session   # one group: loop session permission form readonly errors cli plugin
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
| `scripts/parity/mock.py` | scriptable OpenAI-compatible SSE provider (also behind `scripts/mock_provider.py`) |
| `scripts/parity/{loop,sessions,readonly,errors,cli,plugins}.py` | scenarios, one function each, registered with `@scenario(group)` |

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
(pause after the first chunk), `hang` (never answer). `"$PROJECT"` in any string is replaced by the project path.
With a title expected, the first main response is held until the title was answered (+0.5 s), which makes the
title/step ordering deterministic.

### What the normalizer masks (and nothing else)

- generated IDs (`<prefix>_` + 12 hex + 14 base62, also with derived suffixes like `msg_…_4`), renumbered by
  first appearance; UUIDs;
- epoch-millisecond numbers (any key), ISO timestamps, `duration`/`elapsed`-style keys, `pid`;
- session `slug`s;
- the build version (`0.0.0-v2-…` upstream, `2.0.26` port) wherever it appears, the channel in the
  User-Agent and in the report skill's diagnostics (`v2` vs `latest`);
- the per-run project/state/server paths and URL (identical between runs; masked for readability);
- pagination cursors are base64url-decoded and normalized instead of masked;
- CLI output: the program name `ocd` → `opencode` in command position and in completion-script identifiers,
  epoch-ms numbers in JSON lines;
- plugin failure references (`err_` + 8 random hex digits).

Harness waits that work around upstream laziness (documented below as races, not masked): the first session
creation waits until `agent.list` answers the same twice and `plugin.updated` was seen; `readonly_server` settles
the agent/command/skill/model/provider/integration lists the same way; `readonly_vcs*` issue one unrecorded
`vcs.get` first.

## Scenarios (50)

| group | scenarios |
|---|---|
| loop | text only, read tool, reasoning, three parallel tools (read/glob/grep), write + edit, patch, shell tool, tool errors (missing file, unknown tool, malformed JSON args), HTTP 500 (retry scheduled, then interrupt), HTTP 400, 429 + Retry-After, stream cut mid-way, context overflow → compaction, manual compaction, steer/queue inbox (list/update/cancel), session `!` shell + synthetic, slash command + skill activation, full first LLM request (system prompt, tool schemas, headers) |
| session | CRUD (create/get/update title/metadata/permissions, list with limit/cursor next+previous/order/search/parentID/directory, bad cursor/limit, active, remove incl. children), messages (paging, get, context, inbox, diff, log, export, instruction entries, stats in three tool modes), export/import, fork (all / before), revert (stage with files, clear, stage, commit) in a git repo, interrupt of a hanging request, experimental wait, agent/model switch incl. plan reminders |
| permission | read `.env` → ask → once / always (second read auto-approved) / reject / reject with feedback; client-created requests, saved list/remove, errors |
| form | form API (create, list, get, invalid answers, reply, reply again, cancel, global list, invalid field), question tool round trip |
| readonly | server info, location, config, shells, agents, commands, skills, models (+default), providers, integrations, MCP, plugins, credentials, websearch providers, references, forms, permissions, PTYs, shells, projects, debug locations, v1 migration status; fs list/read/find (incl. traversal and content types); vcs in a git repo (status, branches, base, three diff modes, bad mode, worktree list) and outside one (incl. `vcs.init`); instruction entries + full LLM request with AGENTS.md/skill/command present; debug location evict + reload |
| errors | 25 not-found shapes, 42 validation cases (params, query, payload, invalid JSON, empty body, wrong content types), 7 auth cases |
| cli | `--help` for the root and all 17 subcommands and 29 nested ones, `--version`, unknown flag, bash completions; `run` default and `--format json` with a tool call, `run` without a message; `models`, `stats` (+`--json`, `--all`), `session list`, `debug paths/config/agents`, `mcp list`, `plugin list`, `auth list`, `service status/get` |
| plugin | oc-plugin's fixture plugins (`crates/oc-plugin/tests/fixtures`) copied into the project and configured as local plugins; the port runs them in its Bun sidecar (`OCD_BUN_PATH`, default `$OCD_SCRATCH/bun142/bun-linux-x64/bun`), upstream in-process. `plugin_inventory`: Promise + Effect + invalid + failing-transform plugins → `plugin.list` (order, sources, failures, the `skill.transform` failure disabling its plugin), `command.list`, `agent.list`, `skill.list`, boot and failure events. `plugin_tool`: a plugin tool offered to the model (tool names and full definitions of the first request) and executed with `tool.execute.before` rewriting its input, tool progress, a plugin command writing storage, the storage rows. `plugin_hooks`: `permission.evaluate` allowing/denying `.env` reads, `tool.execute.after` replacing a result, `session.prompt` rewriting the prompt, `session.context` adding a system part, `shell.create.before` adding an env variable. `plugin_effect`: agent/skill transforms and a rejecting `tool.execute.before`. `plugin_rpc_storage`: `rpc.call` (success, declared error, invalid input, unknown method, unknown RPC), storage incl. `ctx.event.subscribe` rows |

## Results

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
| loop_llm_request, readonly_instructions | diff | upstream's `b3`/`traceparent` tracing headers (remaining, below) |
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

### By design (not fixed)

| difference | reason |
|---|---|
| `--version`, report-skill diagnostics, User-Agent channel/version | dev build `0.0.0-v2-…`/channel `v2` vs port 2.0.26/`latest`; masked |
| `debug paths` db `opencode-v2.db` vs `opencode.db`; service config file name | derived from the channel (`v2` dev build) |
| Program name `ocd`: help `DESCRIPTION` reads `ocd command line interface` (upstream `OpenCode command line interface`), completion function names | rename; masked where it is just the name |
| `upgrade` description ("Show how to upgrade opencode (it is built from source)") | the port is built from source |
| Upstream 500s on bad input reproduced on purpose (relative directories, `NaN` stats range) | deterministic upstream behavior kept |

### Inherent races and upstream quirks (not fixed, worked around in the harness)

| behavior | upstream reference |
|---|---|
| Title rename vs first step: commit order depends on which LLM call finishes first | core/src/session/runner/llm.ts:174 forks `title.generate` |
| Location services are built lazily and filled by asynchronously activated plugins: the first `agent.list` of a location answers `[]` (so `ocd debug agents` prints agents where upstream prints `[]`), early permission asks evaluate to `deny`, the plan plugin misses an agent switch right after boot, config agents/commands/skills appear only after the config plugins' debounced reload | core/src/state.ts, core/src/plugin.ts `activate`, core/src/config/plugin/agent.ts reload |
| First `vcs.get` of a location answers `{"branch": {}}` while the forked refresh runs | core/src/vcs.ts `refresh` via `State.reconcile` |
| `location.reload` boots the replacement location's plugins asynchronously; an evict right after it cancels that (no boot notifications) | core/src/location-service-map.ts `reload` |
| `glob`/`grep` result order is ripgrep's parallel walk order (both binaries run `rg` with the same arguments; order changes between runs) | core/src/ripgrep.ts |
| An unknown-tool failure and `session.step.streamed` race (concurrent tool fibers vs stream end) | session/runner/step.ts |
| `b3`/`traceparent` request headers with random trace IDs | Effect HTTP client tracing propagation; the port has no OpenTelemetry tracer |
| `run` without `-m` picks OpenCode Zen's free model in both (network) | the suite passes `-m mock/m1` |
| Plugins activate asynchronously after a location boots: an early `plugin.list` (e.g. `ocd plugin list` against a fresh standalone server) shows only built-ins in both binaries; the plugin scenarios wait for `plugin.updated` and settle the lists | core/src/plugin/supervisor.ts |

### Remaining

Plugins: `vcs` and `worktree` plugin transforms and the `session.http.request/response` hooks are not
wired (see crates/oc-plugin/README.md "Core integration"); plugin-registered integrations and web
search providers live in ocd's process-wide registries instead of per location. `plugin add` was
compared by hand (network): same output and npm cache layout (`opencode-models-discovery@1.10.0`).

| difference | upstream reference | note |
|---|---|---|
| Outgoing LLM requests lack `b3` and `traceparent` headers | Effect HTTP client tracer propagation | needs a trace/span model (one trace per execution, one span per request); providers ignore them |
| ChatGPT token-sharing saved client id is read from the selected `openai` credential row only | core/src/plugin/provider/chatgpt.ts:219-224 (active connection, may be an env var) | approximation in `oc-core/src/integrations.rs` |
| Azure key form with `az` installed lists no resources (only the `az login` description) | core/src/plugin/provider/azure.ts:374-402 | Azure CLI resource listing not ported |
| Session creation boots the location synchronously, so the nine boot notifications follow `project.updated` immediately; upstream publishes them ~0.5 s later | core/src/plugin.ts `activate` (async) | timing only; same events, same order |

## CLI-crate differences (for the CLI owner; `crates/ocd` was not edited)

Everything else in `cli_help` (root, 17 subcommands, 29 nested subcommands, unknown flag, bash completions),
`cli_run` (`run` default and `--format json` with a tool call, `run` without a message) and `cli_inventory`
(`models`, `stats` three ways, `session list`, `debug config`, `mcp list`, `plugin list`, `auth list`,
`service status/get`) is identical after masking the program name.

| command | upstream | ocd | classification |
|---|---|---|---|
| `--help` / `-h` DESCRIPTION | `OpenCode command line interface` | `ocd command line interface` | upstream prints the literal product name regardless of the binary name; by design if the port wants its own name, otherwise use the literal |
| `upgrade --help` and the root SUBCOMMANDS line | `Upgrade OpenCode to the latest or a specific version` | `Show how to upgrade opencode (it is built from source)` | by design |
| `--version` | `opencode v0.0.0-v2-…` | `opencode v2.0.26` | build version |
| `--completions bash` | `_opencode…` identifiers | `_ocd…` identifiers | by design (derived from the program name; masked) |
| `debug paths` | `db …/opencode-v2.db` | `db …/opencode.db` | channel (`v2` dev build vs `latest`) |
| `debug agents` | `[]` | the agent list | upstream race: the first `agent.list` of a fresh standalone server answers `[]` (lazy Agent state); ocd's answer is the intended one |
| `run` without `-m` | uses OpenCode Zen `exo-free` despite the configured `mock/m1` | same | identical; noted because it reaches the network |

## Safety notes

All runs use isolated `XDG_DATA_HOME`, `XDG_CONFIG_HOME`, `XDG_STATE_HOME`, `XDG_CACHE_HOME`, `HOME` and
`OPENCODE_TEST_HOME` under the scratchpad; servers bind `--port 0`; the background service is disabled in the
isolated config (`service.json`, `service-v2.json`, `service-local.json` with `{"disabled": true}`) and
server-backed CLI commands also get `--standalone`. The real `~/.local/share/opencode` is never touched.

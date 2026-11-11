# Catching up with opencode 2.2.7

[← Compatibility](COMPATIBILITY.md)

Status: done (2026-11-11); ocd now reports 2.2.7, and its releases and web UI are built from the `v2.2.7` tag.
Before, ocd tracked opencode 2.2.5 ([UPGRADE-2.2.5.md](UPGRADE-2.2.5.md)); upstream released 2.2.6 (Nov 6) and 2.2.7
(Nov 10, tag `v2.2.7`, commit `fedc6f2`).

How it was planned: four read-only reviews of the tree diff `v2.2.5..v2.2.7`, one per area, each checked against
ocd's code. Every change that is on by default is ported; each batch is checked against an upstream 2.2.7 build from
the tag and against upstream's own tests. The database schema is unchanged (59 migrations), so 2.2.5, 2.2.7 and ocd
keep reading each other's data.

## What changes upstream

- **`opencode run`** (2.2.6): rewritten. `--worktree` (`-w`) starts the session in a new git worktree, `--archived`
  keeps scripted runs out of the session lists, the run waits for its background jobs, SIGINT/SIGTERM cancel the
  session and its jobs, and every command exits 130/143 on those signals.
- **GitHub Copilot**: native Chat, Responses and Messages routes instead of the AI SDK provider, Copilot Auto (the
  router picks the model per session), costs from what Copilot billed, and endpoint discovery from
  `/copilot_internal/user`.
- **Providers**: Azure Messages for Foundry models, Chat `reasoning_opaque`, content-filter categories, an
  unreachable-host retry schedule, OpenCode Console instructions and skills, and the `experimental.model.resolve`
  plugin hook.
- **MCP**: OAuth refresh under a shared lock with refresh-token rotation, temporary refresh failures that keep the
  login, sign-in by pasting the redirect URL, per-tool checks of tool lists, and a short grace for servers still
  starting.
- **Tools and Code Mode**: `read` with negative and past-the-end offsets, name suggestions and `skill://` files;
  `Exit code: N` at the end of shell results; webfetch and Code Mode `fetch` refuse credentials in URLs and check
  every redirect; Code Mode `import`/`require` and catalog deltas; Microsoft Store PowerShell 7.
- **Server and config**: the feature-flag registry (`OPENCODE_FLAGS`, `experimental.all_flags`), the `websearch.use`
  policy action, cross-site API requests refused, sandboxed downloads, fewer endpoints waiting for plugins, a fresh
  database importing the legacy `auth.json`, part rows read, written and forked, `session.generate` usage, and the
  `experimental.job.list` endpoint (170 operations).
- **TUI**: the archived filter, the spacing, model picker, open menu and dialog button fixes, and OAuth paste.

Opt-in features of these releases are not part of this catch-up; their config keys are accepted, normalized and
echoed like 2.2.7: MCP connection pooling (`pool: true`), the `part_rows` writer, Console watches, session warming
(`interval: "auto"`, `cache.warming`) and the experimental Code Mode bytecode VM (`experimental.codemode_vm`, which
changes nothing in ocd: its own interpreter runs either way).

## Batches

| Batch | Scope | Status |
|---|---|---|
| Storage, API and config | OpenAPI 2.2.7, the legacy credential import, part rows, stats, `session.generate` usage, `credential.updated` and the refresh rework, `.well-known` logout, plugin waits, cross-site 403, download headers, PTY 1012, the flag registry, new config keys, `websearch.use`, v1 config and markdown agent fixes, the plugin helper rebuilt on 2.2.7 | Done. Databases checked in both directions: a 2.2.7 database with part rows reads in ocd as upstream reads it, and upstream reads sessions ocd forked and prompted in it. Side-by-side group `api227` 4 of 6 identical, see below |
| Providers | Copilot native routes and Copilot Auto, wire changes (Chat, Responses, Anthropic, Cohere, Google Interactions, Gemini), Azure Messages, `gatewayApiKey`, `criteria`, ChatGPT workspace limits, Bun-style connect errors, the unreachable-host retry, title commentary, the local agent CLI fixes, OpenCode Console instructions and skills, `experimental.model.resolve` | Done: 245 recorded streams and 8 request bodies identical to 2.2.7. Group `wire227` and `plugin_model_resolve` identical |
| Tools, Code Mode and session | `read`, shell, webfetch, `skill://` paths, `session_move`, plugin tool inputs, PowerShell 7 aliases (unit-tested; CI runs Windows), the Code Mode catalog delta, `fetch` redirects and `import`/`require`, the job list, overflow compaction in text mode, background compaction tuning | Done: 240 catalog updates and 28 summaries match goldens regenerated from 2.2.7. Group `tools227` 5 of 6 identical, see below |
| MCP | OAuth refresh rotation under the shared lock, temporary refresh failures, per-tool checks, prompt refresh, startup grace, `~` in `cwd`, stdio probes, `logo_uri`, URL checks, pasted redirect URLs | Done. Group `mcp227` 5 of 5 identical, including a pasted sign-in driven in a terminal against both binaries |
| TUI and CLI | The `run` rewrite, signal exit codes, effect/cli 4.1.0 parsing and help, recaptured CLI goldens, the TUI rows, OAuth paste in the TUI and `auth login`, plugin waits in one-shot commands, ACP steering and unused sessions | Done: the old 150 ms hold that emulated upstream's last `run` output is gone, and `ocd run a -- b` no longer sends `b` twice. Group `run227` 7 of 7 identical; 18 signal cases exit like 2.2.7 |
| Final check | The full test suite and the full side-by-side suite against the 2.2.7 build | Done: tests 2,547 passed; 202 of 207 scenarios identical, see below |

## Final check

Tests: 2,547 passed with every non-loopback connection sent to a dead proxy. CI runs the suite on macOS, Windows
and Linux ARM: its first runs after the batches found five Windows-only failures (`read`'s suggestions split paths
only on `/`, two tests expecting `/` where Node's `path` uses `\`, a Ctrl+Z test where Windows has no terminal
suspend, and a PTY test that never answered ConPTY's cursor query, so its shell never started); all fixed. A background compaction test that hung on busy machines now waits for the failed summary to
settle instead of a fixed 200 ms.

The first full side-by-side run against the 2.2.7 build found what the batches had not ported yet, all fixed:

- each `SessionRunner.drain` starts its own trace in 2.2.7 (title, tools, jobs and background compaction inherit
  it, and a compaction's requests share one parent span), so later turns' requests carry a new trace id;
- the browser plugin's new state machine: one set of browser allows, granted before `attached`, all revoked on
  detach, and built-in RPCs unavailable from a call that boots their location;
- `session.moved` once, from the old location; the destination's boot events after a tool move;
- `project.updated` for a session whose directory is gone, and no `content-length` on an empty 416.

Then **202 of 207 scenarios identical**. Left: `cli_help` (the intended renames), `cli_inventory` (the upstream build's
database file name), `stop_retry_exhausted` and `loop_http_400` (upstream races: in 10 extra upstream runs, 2 had
ocd's order), `loop_tool_error` (Effect's scheduler yields after 2,048 operations, so upstream settles instantly
failing tools in the middle of a read; not modelled).

That run also showed `run227_signal` exiting 1 on a busy machine: every stopped `run` kept its event stream open,
so the private server waited 2 s to shut down, and an interrupt could claim the wake that should deliver a cancelled
job's notice, stranding the cancel until the 10 s deadline. `run` now closes its stream when it is over and
interrupts a job session once its input is delivered (with `resume: true`), so a stopped run exits at once with
130/143; identical in 60 parallel runs.

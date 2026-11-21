# Catching up with opencode 2.3.0

[← Compatibility](COMPATIBILITY.md)

Status: done (2026-11-17); ocd now reports 2.3.0, and its releases and web UI are built from the `v2.3.0` tag
(commit `a6b15d3`). Before, ocd tracked opencode 2.2.7 ([UPGRADE-2.2.7.md](UPGRADE-2.2.7.md)); upstream released
2.3.0 on Nov 13.

How it was planned: four read-only reviews of upstream's development branch after 2.2.7 (180 commits), then a
review of the 11 commits 2.3.0 added and of the tag itself. Every change that is on by default is ported; each batch
is checked against an upstream 2.3.0 build from the tag and against upstream's own tests. The database schema is
unchanged (59 migrations), so 2.2.7, 2.3.0 and ocd keep reading each other's data; the new durable event
`session.step.first-token` reads in both directions.

## What changes upstream

- **Freeform tool input**: on models whose models.dev route advertises it (94 GPT-5 models across OpenAI, Azure,
  OpenCode, OpenCode Go and Cloudflare AI Gateway), Code Mode's `execute` is sent as a free-text custom tool and
  `patch` as a custom tool with a Lark grammar; custom tool calls are streamed and replayed.
- **Subagents**: always run in the background (the `background` input is gone); the parent gets a report every
  time a child goes idle, tagged `state="running"` while it still has background work; restart recovery is reworked.
- **Step lifecycle**: a failed or interrupted answer is replayed cut at its last local tool call (no more
  "previous response was interrupted" message); a `session.step.first-token` event, `time.firstToken`, tokens/s from
  the first token and time to first token in the TUI; stopping a shell call keeps its output; promise plugin tools
  stop through their `signal`.
- **Code Mode**: the bytecode VM is the only engine (no visible change: see below); compact JSON results, BM25
  tool search, tool suggestions when a namespace is called, `TextDecoder` streaming and streaming `fetch` responses.
- **MCP and sign-in**: OAuth tokens refreshed 30 s before they expire with the discovery saved at login, issuer
  checks, `insufficient_scope` as needs-auth, both loopback redirects registered; pasted redirect URLs for the
  ChatGPT, OpenAI, GitLab, Poe and Snowflake sign-ins.
- **Providers**: client certificates (`settings.tls`), LM Studio decision models and 15 s local discovery,
  Google Decisions, System One refusals and PDFs, rate-limit and content-policy error classes, the Copilot login
  endpoint, Console instructions and skills only when the workspace has them, `gitlab-ai-provider` 6.24.0.
- **Tools and sessions**: the shell result's `PID:` line and structured output, new edit/write/skill/patch result
  shapes, the truncation preview kept when output can't be saved, case-only renames in `patch`, the fork notice and
  the `fork` plugin hook, request hooks seeing message metadata, instruction updates left behind by a revert hidden.
- **TUI**: low transcript verbosity by default, clickable activity tabs and footer count, session links in
  messages, the prompt history keeping the draft, copying a prompt selection with its pastes, paste chip colours.
- **API and config**: 170 operations (two changed), 501 schemas; the flags `mcp_events` and
  `shell_timeout_background`; `codemode_vm` deprecated and ignored.

Not ported, as in upstream's release builds these are off or not used: MCP Events and the watch tool
(`mcp_events`, ported after 2.3.1), shell timeouts moving to the background (`shell_timeout_background`), video generation (library
code nothing calls). Shell commands in upstream's process daemon, which let them survive a server restart, are not
ported either (see [COMPATIBILITY.md](COMPATIBILITY.md#not-implemented-yet)).

## Batches

| Batch | Scope | Result |
|---|---|---|
| API and config | 2.3.0's API document and generated types, the new events, flags, `tls` and `freeform` config keys | `api230` identical; `debug config` matches |
| P0 Code Mode engine | The recorder rebased on 2.3.0's VM; the corpus re-recorded | No program differs because of the engine change; edge differences of the VM's standard library found by probing are followed |
| P1 Freeform tools | Custom tools on every route that uses the OpenAI protocols, the Lark `patch` grammar, freeform plugin tools | 208 request bodies and 33 streams identical to 2.3.0; `freeform230` 4 of 4 |
| P2 Subagents | Always background, a report per idle, restart rework, TUI and ACP | `subagent` 5 of 6 and `acp` 3 of 3 before the shell result change, all identical after it |
| P3 Step lifecycle | Replay cut, first token, cooperative stop, plugin `signal` | `loop`, `stop` and the first-token scenarios identical |
| P4 MCP OAuth and sign-in | Refresh ahead of expiry, saved discovery, issuer checks, pasted provider redirects | `auth230` 5 of 5, `mcp227` 5 of 5 |
| P5/P6 Code Mode features | Compact JSON, BM25 search, namespace suggestions, `TextDecoder` streaming, streaming `fetch` | 1,841 of 1,843 recorded programs; 272 search and 46 fetch programs identical to 2.3.0 |
| P7 Small items | Tool texts and shapes, fork notice, provider fixes, `gitlab-ai-provider` 6.24.0 | The shell `PID:` scenarios, `session_fork` and `readonly_server` identical |
| P8 TUI | Verbosity, tabs, session links, history draft, paste copying | Screens captured side by side match |
| P9/P10 Local discovery and client certificates | LM Studio decisions, keys during discovery, 15 s timeout; `settings.tls` | `local` 8 of 8, `tls230` 6 of 6 |
| Final check | Version 2.3.0, web UI and plugin helper from the tag, full tests, full side by side | Tests 2,751 passed; 241 of 246 scenarios identical |

## Final check

Tests: 2,751 passed with every non-loopback connection sent to a dead proxy. Load-sensitive timing tests
(`cooperative_stop`, `permission_always`, a Code Mode progress test, the TUI frame-cost test) failed occasionally
while several batches built at once and passed on reruns.

Side by side: **241 of 246 scenarios identical** against an upstream 2.3.0 build. Left: `cli_help` (the intended
renames), `cli_inventory` (the upstream build's database file name), `stop_retry_exhausted` and `loop_http_400`
(upstream races) and `loop_tool_error` (Effect's scheduler yields after 2,048 operations), the same five as against
2.2.7.

Code Mode: 2.3.0's bytecode VM changes nothing a program sees in upstream's own test suite. Replaying 1,843 programs
recorded from it, 1,841 give the identical result (the other two are a cleanup timing difference also seen with
2.2); probing past the suite found edge differences in the VM's standard library (error texts, coercions, nested
imports), which ocd follows, except the VM's lazy check of an iterator's `next`.

Known gaps: P-521 client keys (ocd's TLS library cannot sign with them), PKCS#12 RC2/RC4 encrypted keys, and the
items listed under "Not ported" above.

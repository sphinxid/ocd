# Compatibility with opencode

[← README](../README.md)

ocd targets opencode **2.2.7** (`anomalyco/opencode`, tag `v2.2.7`, commit `fedc6f2`). It was first ported from
branch `v2` at commit `4617210` (2.0.26). It then caught up with 2.1.4, 2.2.2, 2.2.5 and 2.2.7 in batches, listed with
what each one checked in [UPGRADE-2.1.4.md](UPGRADE-2.1.4.md), [UPGRADE-2.2.2.md](UPGRADE-2.2.2.md),
[UPGRADE-2.2.5.md](UPGRADE-2.2.5.md) and [UPGRADE-2.2.7.md](UPGRADE-2.2.7.md). Wherever it has to look like opencode,
ocd reports version 2.2.7.

- [How it is compared](#how-it-is-compared)
- [Same as upstream](#same-as-upstream)
- [Works, with a small difference](#works-with-a-small-difference)
- [Not implemented yet](#not-implemented-yet)
- [Different on purpose](#different-on-purpose)

## How it is compared

- **Side by side:** both binaries run the same 207 scenarios, and `scripts/parity.py` diffs their outputs. Results
  are in [PARITY.md](PARITY.md). The scenarios cover:
  - the agent loop, sessions, permissions and forms;
  - the read-only endpoints and 74 error cases;
  - the CLI and plugins;
  - GitHub Copilot, the OpenAI WebSocket transport and the experimental todo list;
  - the 2.2 changes: provider wire, `/goal`, background compaction and tools;
  - Mercurial repositories and the desktop browser attachment;
  - the 2.2.7 changes: `run`, MCP OAuth, Copilot Auto, tools, the API and config.
- **Model requests:** every request ocd sends to the model is compared too: system prompt, tool definitions,
  headers and prompt-cache markers.
- **Result:** 202 of the 207 scenarios are identical. The other five differ in ways that are expected or harmless:
  - the intended renames;
  - the upstream test build's database file name;
  - two timing races that also vary between runs of upstream (one is the title rename racing a failed step);
  - the order of tool-failure events for calls that fail instantly, which upstream gets from its runtime's scheduler.

- **Per-crate tests:** each crate has its own test suite, many replaying recorded upstream data.

## Same as upstream

| Area | Notes |
|---|---|
| Sessions and history | Create, fork, revert, export, import and search. The same database file and schema |
| Tool input repair | Malformed tool inputs are repaired against the tool's schema before validation, as upstream's `opencode.tool.input.repair` does: numbers and booleans sent as strings, extra keys on closed schemas, stringified JSON, empty placeholders, single values for arrays. For built-in, Code Mode, MCP and plugin tools; `-opencode.tool.input.repair` turns it off |
| Agent loop | Steps, tool calls, reasoning, retries, compaction (including background compaction, `experimental.background_compaction`), titles, background jobs and subagents |
| `/goal` | The command and its controls, the reviewer loop, the `opencode.goal` RPC and its event, and the TUI's footer status and dialog |
| Permissions and forms | Ask, once, always, and reject with feedback; the question tool |
| Built-in tools | read, write, edit, patch, glob, grep, shell, webfetch, websearch, todo, task and others, with 2.2.7's `read` offsets from the end, name suggestions and `skill://` files (the in-memory files of OpenCode Console workspace skills, also listed by the `skill` tool and in skill attachments; like 2.2.7, files a third-party plugin passes to its skill editor's `add` are dropped), `Exit code: N` after shell output, and webfetch refusing credentials in URLs. Output limits and scanners checked against 81,787 upstream cases |
| Code Mode | The JavaScript `execute` tool has its own interpreter. 1,800 of 1,802 programs recorded from upstream 2.2.2 replay identically (the other two depend on how a timed-out tool call is cleaned up). 2.2.7's catalog deltas, `import`/`require` of built-in modules and `fetch` redirect checks are in |
| Project setup | Agents, commands, skills (including remote skill discovery), `AGENTS.md` instructions and references, config files and live reload, configured `instructions` (files, globs and URLs), `policies` (with 2.2.5's `integration.connect`, `integration.use` per account, `mcp.use`, `skill.use` and `plugin.use`, and old `mcp:`/`skill:`/`plugin:` rules converted), well-known organization config, and managed system config (`/etc/opencode` and its platform equivalents) |
| Claude Agent provider | Runs sessions through the local Claude Code CLI (`claude auth login`, `OPENCODE_CLAUDE_PATH`) with OpenCode's tools: the integration and its sign-in check, the model catalog, the stdio and control protocol, sessions, forks and compaction. Checked side by side against a scripted stand-in for `claude`. Transcript forks and deletes use the real Claude Agent SDK in the plugin helper (Bun, installed on first use) |
| opencode v1 import | Imports v1 history into the v2 tables byte-identically to upstream, with the same progress badge in the TUI |
| Chat providers | Anthropic, OpenAI, Google, Bedrock, Mistral, Cohere, OpenAI-compatible, OpenCode Zen, and every models.dev provider using those protocols. 229 recorded upstream streams decode identically |
| Sign-in | API keys and OAuth. ChatGPT, with token sharing, the Codex browser and device-code logins, and upstream's model list, endpoints and headers. Also GitHub Copilot and the Azure CLI. Available from `ocd auth` or the TUI's `/connect` (accounts, sign-in forms, browser and device OAuth, pasted codes and redirect URLs, login commands). Azure Messages for Foundry models |
| GitHub Copilot models | The account's model list and endpoint (`/copilot_internal/user`), native Chat, Responses and Messages routes, Copilot Auto (the router picks the model per session, with its fallback), costs from what Copilot billed, content-filter categories and Copilot's request headers |
| OpenAI Responses over WebSocket | For OpenAI, Azure and xAI: one socket per session, incremental turns with `previous_response_id`, and the same fallback to HTTP. The same proxy variables as upstream (`WSS_PROXY`, `WS_PROXY`, `HTTPS_PROXY`, `HTTP_PROXY`, `ALL_PROXY`, `NO_PROXY`) and `permessage-deflate` compression |
| Cloud credentials | Bedrock uses the AWS SDK's full default chain: environment, profiles with role assumption, web identity, `credential_process`, SSO with token refresh, `aws login`, container and EC2 instance roles. Vertex uses Google's application default credentials, including workload identity federation (`external_account`) and service-account impersonation. Checked against upstream with stand-in AWS and Google endpoints |
| MCP servers | stdio and HTTP transports, OAuth and elicitation. Remote servers are sign-in integrations, so `ocd mcp auth` and `/connect` can sign in to them, also by pasting the URL the browser was redirected to. OAuth refreshes run one at a time per credential across processes and keep rotated refresh tokens; a temporary refresh failure keeps the login. Invalid tools are skipped one by one, and model calls give a starting server up to 2 s |
| Server plugins | Tools, hooks, transforms, storage, rpc and events, plus `ocd plugin add/check/update`. They run in a small Bun helper process. Changing `plugins` while the server runs reloads them live, any number of times, with upstream's notifications |
| AI SDK provider packages | Such as `@ai-sdk/deepseek`, through the same Bun helper |
| Clients and API | The full TUI. The web UI served by `ocd serve`, built from upstream's 2.2.7 app (88 of 88 compared requests were identical with the 2.0.26 app; the serving code is unchanged since apart from its content-security policy). The HTTP API (170 endpoints, from upstream 2.2.7's OpenAPI document) and the event stream. The background service on port 49374 (28894 on Windows) |
| Image generation and decisions | The `experimental_image_generate` tool (on while a connected provider has image models, unless `experimental.image_generate` is `false`), background image jobs and their recovery, `/api/experimental/generate/image` and `/generate/decisions`, `ctx.experimental.generate.image`/`.decisions`, every image and decision provider route 2.2.5 ships, and GPT Image 2 after a Codex sign-in |
| Scheduled prompts | `experimental.schedules`: `/schedule`, the schedule tools and API, shell and Code Mode `if`/`until` checks, unattended permissions, and the `schedule:leader` lease so a schedule fires once across processes |
| Artifacts | `experimental.artifacts`: the local and plugin providers, the `artifact_*` tools, the routes including `view`, and `artifact.updated` |
| Session media storage | `experimental.media_blob`: images stored as blobs with `media:` references, read back under `X-OpenCode-Capabilities: experimental-media-uri`, kept through fork and import and released with their messages; the TUI restores them into prompts |
| Git | Status, diffs, branches, snapshots and worktrees |
| Shell permissions | The built-in shell scanner of 2.2.5 (no tree-sitter): redirects, PowerShell method calls and dynamic `cd` are permission resources. 95,128 calls recorded from upstream's own scanner match |
| Platforms | Linux, macOS and Windows. CI runs the whole test suite on macOS, Windows and Linux ARM on every push to main; on Windows ocd follows upstream's Node behaviour: paths without the `\\?\` prefix, `path.isAbsolute`, `os.homedir`, win32 path formatting in the TUI, the terminal locale and Ctrl+C, and the detached background service. The side-by-side comparison runs on Linux x86_64 |
| Persistent terminals | `@opencode/pty` 0.4.0: terminals belong to their session, are removed with it, and can be renamed |
| Mercurial | Project identity, status, working and branch diffs, `vcs.init` for hg and branch watching. Like upstream, snapshots and worktrees are git-only. Needs `hg` on `PATH` |
| Desktop browser attachment | The built-in `opencode.browser` plugin: the `experimental.browser` RPC (attach, state, commands and results, tunnels), its control events, the session permissions an attached desktop grants, the 45 `browser.*` Code Mode tools with file uploads and captures, and the 60 s request timeout. The `browser` skill is denied to every agent until a desktop attaches. The desktop's own proxy is part of the desktop app, not the server |
| Editor integration (ACP) | `ocd acp`, including the file diffs on permission requests and completed edits, and following subagents that outlive the turn (or its cancellation) until they finish |
| CLI prompts | `ocd auth`, `ocd mcp auth` and `ocd session export` use the same prompts as upstream (pickers, search lists, forms), down to resizing and key handling. `--wizard` builds any command interactively with upstream's prompts, byte for byte |
| Remote access | `ocd pair --remote` |
| Scripted runs | `ocd run` as rewritten in 2.2.7: `--worktree`, `--archived`, waiting for the run's background jobs, retry notices, and SIGINT/SIGTERM cancelling the session and its jobs. Every command exits 130/143 on those signals |
| Feature flags | 2.2.7's registry: `OPENCODE_FLAGS` (`all`, a key, or `-key`), then `experimental.<key>`, then `experimental.all_flags`. The `websearch.use` policy action |
| Experimental todo list | `experimental.todo`: the `todowrite` tool, the sidebar section and its transcript rendering |

## Works, with a small difference

| Area | Difference |
|---|---|
| Plugins and AI SDK packages | Upstream runs them inside its Bun-based binary. ocd runs them in a Bun helper process and downloads Bun 1.4.4 the first time one is used (or set `OCD_BUN_PATH`) |
| Built-in AI SDK packages | Upstream bundles `@ai-sdk/cohere`, `@ai-sdk/perplexity`, `@ai-sdk/vercel` and `gitlab-ai-provider`. ocd installs them from npm on first use, at the same versions; if an install fails, the error wording differs |
| `session.http.response` plugin hook | Receives the whole response body as text instead of a stream |
| Plugin worktree strategies | Used only after the project's location has loaded; before that, worktree tools fall back to git |
| Persistent terminals | The same `opencode-pty` helper (0.3.1), downloaded once instead of carried inside the binary. Enabled on Windows like upstream 2.1, but not tested there |
| Mini mode (`ocd mini`) | A port of upstream's mini frontend. In 63 screens compared side by side the text was identical, except one startup race; code blocks are highlighted with syntect instead of tree-sitter, so some TypeScript and JavaScript token colours differ |
| Error texts | A few come from Rust instead of Bun or Node: file-system errors, MCP JSON-schema validation and network errors |
| Start-up events | A new session's start-up notifications arrive about 0.5 s sooner; same events, same order |
| Stopping `ocd run` | On SIGINT/SIGTERM ocd interrupts a background job's session once its input is delivered, with `resume: true` (upstream interrupts when the session starts), so a cancelled job's notice can't hold up the exit. Same output and exit codes |
| Tool failure events | When several tool calls in one response fail instantly (unknown tool, unparseable input), upstream records some of the failures in the middle of the response, where its runtime's scheduler happens to yield; ocd records them after it |
| `pair --remote` | A native OpenTunnel client (`crates/oc-tunnel`) replaces upstream's JavaScript SDK. It keeps the tunnel in the same place, so ocd, upstream and the opentunnel CLI share a device's tunnel. Tested against a local stand-in for the service, not yet against opentunnel.xyz |

## Not implemented yet

| Feature | Notes |
|---|---|
| TUI plugins (the `tui` entrypoint) | Not planned: they need upstream's SolidJS terminal UI |
| Third-party packages for upstream's model runtime (`@opencode/ai`) | AI SDK packages do work |
| Image previews in the TUI | `session.image_preview` and `prompt.image_preview` (off by default): ocd draws no images in the terminal |
| Speech, video and transcription models | Upstream has these only as library code that nothing in the app calls |
| Experimental voice and client-hosted MCP endpoints | Listed in the API document like upstream's, but answer 500 |
| Opt-in features of 2.2.6 and 2.2.7 | MCP connection pooling (`pool: true`), the `experimental.part_rows` writer (part rows written by 2.2.7 are read), Console watches, session warming and the Code Mode bytecode VM (`experimental.codemode_vm`; ocd's interpreter runs either way). Their config keys are accepted and echoed like 2.2.7 |
| Other experimental features | `location_idle_timeout` and the new designs. Their config keys are accepted and do nothing |

## Different on purpose

| Area | Difference |
|---|---|
| Name | The program name, help text and logo say `ocd` / `opencode-d`. Config, data folders, `OPENCODE_*` variables, the HTTP API and plugin package names keep opencode's names |
| TUI working status line | After you press Enter, a line above the prompt shows what's happening: waiting for the model, thinking, writing, running a tool, retrying or compacting. It also shows a timer ticking every second, the output token count and the interrupt key. A running shell command goes on the row below. Upstream shows only its moving progress bar, which ocd keeps, plus a live "Thinking" row in the transcript, which ocd leaves out while the status line shows it |
| `ocd upgrade` and update checks | Installs from ocd's GitHub releases with checksum verification, and checks once a day instead of every 10 minutes ([details](INSTALL.md#upgrading)) |
| Heap snapshots | The server has no JavaScript heap, so `POST /api/debug/heap-dump` answers `500 UnknownError`. `ocd debug heap-dump --server URL` still captures one from an upstream server |
| Version | `ocd --version` shows ocd's own build next to the compatible opencode version. Clients, plugins and the service see 2.2.7 on the `latest` channel ([details](INSTALL.md#versions)) |

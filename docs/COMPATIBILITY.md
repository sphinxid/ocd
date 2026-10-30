# Compatibility with opencode

[← README](../README.md)

ocd targets opencode **2.2.2** (`anomalyco/opencode`, tag `v2.2.2`, commit `098aa7f`). It was first ported from
branch `v2` at commit `4617210` (2.0.26). It then caught up with 2.1.4 and 2.2.2 in batches, listed with what each
one checked in [UPGRADE-2.1.4.md](UPGRADE-2.1.4.md) and [UPGRADE-2.2.2.md](UPGRADE-2.2.2.md). Wherever it has to
look like opencode, ocd reports version 2.2.2.

- [How it is compared](#how-it-is-compared)
- [Same as upstream](#same-as-upstream)
- [Works, with a small difference](#works-with-a-small-difference)
- [Not implemented yet](#not-implemented-yet)
- [Different on purpose](#different-on-purpose)

## How it is compared

- **Side by side:** both binaries run the same 98 scenarios, and `scripts/parity.py` diffs their outputs. Results
  are in [PARITY.md](PARITY.md). The scenarios cover:
  - the agent loop, sessions, permissions and forms;
  - the read-only endpoints and 74 error cases;
  - the CLI and plugins;
  - GitHub Copilot, the OpenAI WebSocket transport and the experimental todo list;
  - the 2.2 changes: provider wire, `/goal`, background compaction and tools.
- **Model requests:** every request ocd sends to the model is compared too: system prompt, tool definitions,
  headers and prompt-cache markers.
- **Result:** 95 of the 98 scenarios are identical. The other three differ in ways that are expected or harmless:
  - the intended renames;
  - the upstream test build's database file name;
  - a timing race that also varies between two runs of upstream.
- **Per-crate tests:** each crate has its own test suite, many replaying recorded upstream data.

## Same as upstream

| Area | Notes |
|---|---|
| Sessions and history | Create, fork, revert, export, import and search. The same database file and schema |
| Agent loop | Steps, tool calls, reasoning, retries, compaction (including background compaction, `experimental.background_compaction`), titles, background jobs and subagents |
| `/goal` | The command and its controls, the reviewer loop, the `opencode.goal` RPC and its event, and the TUI's footer status and dialog |
| Permissions and forms | Ask, once, always, and reject with feedback; the question tool |
| Built-in tools | read, write, edit, patch, glob, grep, shell, webfetch, websearch, todo, task and others. Output limits and scanners checked against 81,787 upstream cases |
| Code Mode | The JavaScript `execute` tool has its own interpreter. 1,800 of 1,802 programs recorded from upstream 2.2.2 replay identically (the other two depend on how a timed-out tool call is cleaned up) |
| Project setup | Agents, commands, skills (including remote skill discovery), `AGENTS.md` instructions and references, config files and live reload, configured `instructions` (files, globs and URLs), `policies`, and managed system config (`/etc/opencode` and its platform equivalents) |
| opencode v1 import | Imports v1 history into the v2 tables byte-identically to upstream, with the same progress badge in the TUI |
| Chat providers | Anthropic, OpenAI, Google, Bedrock, Mistral, Cohere, OpenAI-compatible, OpenCode Zen, and every models.dev provider using those protocols. 229 recorded upstream streams decode identically |
| Sign-in | API keys and OAuth. ChatGPT, with token sharing, the Codex browser and device-code logins, and upstream's model list, endpoints and headers. Also GitHub Copilot and the Azure CLI. Available from `ocd auth` or the TUI's `/connect` (accounts, sign-in forms, browser and device OAuth, pasted codes, login commands) |
| GitHub Copilot models | The account's model list, routing between the AI SDK, Responses, Chat and native Anthropic routes, and Copilot's request headers. Upstream's in-house Copilot provider runs in the Bun helper |
| OpenAI Responses over WebSocket | For OpenAI, Azure and xAI: one socket per session, incremental turns with `previous_response_id`, and the same fallback to HTTP. The same proxy variables as upstream (`WSS_PROXY`, `WS_PROXY`, `HTTPS_PROXY`, `HTTP_PROXY`, `ALL_PROXY`, `NO_PROXY`) and `permessage-deflate` compression |
| Cloud credentials | Bedrock uses the AWS SDK's full default chain: environment, profiles with role assumption, web identity, `credential_process`, SSO with token refresh, `aws login`, container and EC2 instance roles. Vertex uses Google's application default credentials, including workload identity federation (`external_account`) and service-account impersonation. Checked against upstream with stand-in AWS and Google endpoints |
| MCP servers | stdio and HTTP transports, OAuth and elicitation. Remote servers are sign-in integrations, so `ocd mcp auth` and `/connect` can sign in to them |
| Server plugins | Tools, hooks, transforms, storage, rpc and events, plus `ocd plugin add/check/update`. They run in a small Bun helper process |
| AI SDK provider packages | Such as `@ai-sdk/deepseek`, through the same Bun helper |
| Clients and API | The full TUI. The web UI served by `ocd serve`, built from upstream's 2.2.2 app (88 of 88 compared requests were identical with the 2.0.26 app; the serving code is unchanged since). The HTTP API (162 endpoints, from upstream 2.2.2's OpenAPI document) and the event stream. The background service on port 49374 (28894 on Windows) |
| Git | Status, diffs, branches, snapshots and worktrees |
| Editor integration (ACP) | `ocd acp`, including the file diffs on permission requests and completed edits |
| CLI prompts | `ocd auth`, `ocd mcp auth` and `ocd session export` use the same prompts as upstream (pickers, search lists, forms), down to resizing and key handling |
| Remote access | `ocd pair --remote` |
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
| `pair --remote` | A native OpenTunnel client (`crates/oc-tunnel`) replaces upstream's JavaScript SDK. It keeps the tunnel in the same place, so ocd, upstream and the opentunnel CLI share a device's tunnel. Tested against a local stand-in for the service, not yet against opentunnel.xyz |

## Not implemented yet

| Feature | Notes |
|---|---|
| TUI plugins (the `tui` entrypoint) | Not planned: they need upstream's SolidJS terminal UI |
| Third-party packages for upstream's model runtime (`@opencode/ai`) | AI SDK packages do work |
| Image generation | The `image_generate` tool (on by default in 2.2.2 when a connected provider has image models), the image and decision APIs and models. The endpoints answer as when the feature is off |
| Scheduled prompts | `/schedule` and `experimental.schedules`. The endpoints answer as when the feature is off |
| Session media storage | `experimental.media_blob` writes and media URIs. ocd reads media blobs that upstream wrote |
| Speech, video and transcription models | Upstream has these only as library code that nothing in the app calls |
| Desktop browser attachment | The built-in `browser` skill is listed and denied to every agent like upstream, but the `experimental.browser` RPC and the `browser.*` Code Mode tools, which work only once the desktop app attaches, are not served |
| Mercurial repositories | No version-control provider, so its endpoints answer empty |
| `--wizard` mode | Prints an error |
| Experimental voice and client-hosted MCP endpoints | Listed in the API document like upstream's, but answer 500 |
| Other experimental features | `location_idle_timeout` and the new designs. Their config keys are accepted and do nothing |
| Testing on macOS, Windows and Linux ARM | Release binaries are built for them, but only Linux x86_64 is tested |

## Different on purpose

| Area | Difference |
|---|---|
| Name | The program name, help text and logo say `ocd` / `opencode-d`. Config, data folders, `OPENCODE_*` variables, the HTTP API and plugin package names keep opencode's names |
| TUI working status line | After you press Enter, a line above the prompt shows what's happening: waiting for the model, thinking, writing, running a tool, retrying or compacting. It also shows a timer ticking every second, the output token count and the interrupt key. A running shell command goes on the row below. Upstream shows only its moving progress bar, which ocd keeps, plus a live "Thinking" row in the transcript, which ocd leaves out while the status line shows it |
| `ocd upgrade` and update checks | Installs from ocd's GitHub releases with checksum verification, and checks once a day instead of every 10 minutes ([details](INSTALL.md#upgrading)) |
| Heap snapshots | The server has no JavaScript heap, so `POST /api/debug/heap-dump` answers `500 UnknownError`. `ocd debug heap-dump --server URL` still captures one from an upstream server |
| Version | `ocd --version` shows ocd's own build next to the compatible opencode version. Clients, plugins and the service see 2.2.2 on the `latest` channel ([details](INSTALL.md#versions)) |

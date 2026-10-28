# Compatibility with opencode

[← README](../README.md)

ocd targets opencode **2.1.4** (`anomalyco/opencode`, tag `v2.1.4`, commit `68bc26b`). It was first ported from
branch `v2` at commit `4617210` (2.0.26). It then caught up with 2.1.4 in batches, listed with what each one
checked in [UPGRADE-2.1.4.md](UPGRADE-2.1.4.md). Wherever it has to look like opencode, ocd reports version
2.1.4.

- [How it is compared](#how-it-is-compared)
- [Same as upstream](#same-as-upstream)
- [Works, with a small difference](#works-with-a-small-difference)
- [Not implemented yet](#not-implemented-yet)
- [Different on purpose](#different-on-purpose)

## How it is compared

- **Side by side:** both binaries run the same 72 scenarios, and `scripts/parity.py` diffs their outputs. Results
  are in [PARITY.md](PARITY.md). The scenarios cover:
  - the agent loop, sessions, permissions and forms;
  - the read-only endpoints and 74 error cases;
  - the CLI and plugins;
  - GitHub Copilot, the OpenAI WebSocket transport and the experimental todo list.
- **Model requests:** every request ocd sends to the model is compared too: system prompt, tool definitions,
  headers and prompt-cache markers.
- **Result:** 65 of the 72 scenarios are identical. The other seven differ in ways that are expected or harmless:
  - timing races that also vary between two runs of upstream;
  - the intended renames;
  - the wording of one network error;
  - `ocd run` printing the last step of a run live, where upstream prints it afterwards.
- **Per-crate tests:** each crate has its own test suite, many replaying recorded upstream data.

## Same as upstream

| Area | Notes |
|---|---|
| Sessions and history | Create, fork, revert, export, import and search. The same database file and schema |
| Agent loop | Steps, tool calls, reasoning, retries, compaction, titles, background jobs and subagents |
| Permissions and forms | Ask, once, always, and reject with feedback; the question tool |
| Built-in tools | read, write, edit, patch, glob, grep, shell, webfetch, websearch, todo, task and others. Output limits and scanners checked against 81,787 upstream cases |
| Code Mode | The JavaScript `execute` tool has its own interpreter. 1,704 of 1,707 recorded upstream programs replay identically; one of the others changed on purpose in 2.1 and is checked against 2.1.4 instead |
| Project setup | Agents, commands, skills (including remote skill discovery), `AGENTS.md` instructions and references, config files and live reload, configured `instructions` (files, globs and URLs), `policies`, and managed system config (`/etc/opencode` and its platform equivalents) |
| opencode v1 import | Imports v1 history into the v2 tables byte-identically to upstream, with the same progress badge in the TUI |
| Chat providers | Anthropic, OpenAI, Google, Bedrock, Mistral, Cohere, OpenAI-compatible, OpenCode Zen, and every models.dev provider using those protocols. 229 recorded upstream streams decode identically |
| Sign-in | API keys and OAuth. ChatGPT, with token sharing, the Codex browser and device-code logins, and upstream's model list, endpoints and headers. Also GitHub Copilot and the Azure CLI. Available from `ocd auth` or the TUI's `/connect` (accounts, sign-in forms, browser and device OAuth, pasted codes, login commands) |
| GitHub Copilot models | The account's model list, routing between the AI SDK, Responses, Chat and native Anthropic routes, and Copilot's request headers. Upstream's in-house Copilot provider runs in the Bun helper |
| OpenAI Responses over WebSocket | For OpenAI, Azure and xAI: one socket per session, incremental turns with `previous_response_id`, and the same fallback to HTTP. The same proxy variables as upstream (`WSS_PROXY`, `WS_PROXY`, `HTTPS_PROXY`, `HTTP_PROXY`, `ALL_PROXY`, `NO_PROXY`) and `permessage-deflate` compression |
| MCP servers | stdio and HTTP transports, OAuth and elicitation |
| Server plugins | Tools, hooks, transforms, storage, rpc and events, plus `ocd plugin add/check/update`. They run in a small Bun helper process |
| AI SDK provider packages | Such as `@ai-sdk/deepseek`, through the same Bun helper |
| Clients and API | The full TUI. The web UI served by `ocd serve`, built from upstream's 2.1.4 app (88 of 88 compared requests were identical with the 2.0.26 app; the serving code is unchanged in 2.1.4). The HTTP API (149 endpoints, from upstream 2.1.4's OpenAPI document) and the event stream. The background service on port 49374 (28894 on Windows) |
| Git | Status, diffs, branches, snapshots and worktrees |
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
| Mini mode (`ocd mini`) | Works, but simplified and not compared screen by screen |
| `ocd auth login` pickers | Multi-select asks for comma-separated values |
| Editor integration (ACP) | Doesn't add the diff for a completed edit |
| Error texts | A few come from Rust instead of Bun or Node: file-system errors, MCP JSON-schema validation and network errors |
| Start-up events | A new session's start-up notifications arrive about 0.5 s sooner; same events, same order |
| `pair --remote` | A native OpenTunnel client (`crates/oc-tunnel`) replaces upstream's JavaScript SDK. It keeps the tunnel in the same place, so ocd, upstream and the opentunnel CLI share a device's tunnel. Tested against a local stand-in for the service, not yet against opentunnel.xyz |

## Not implemented yet

| Feature | Notes |
|---|---|
| TUI plugins (the `tui` entrypoint) | Not planned: they need upstream's SolidJS terminal UI |
| Third-party packages for upstream's model runtime (`@opencode/ai`) | AI SDK packages do work |
| Image, speech, video and transcription models | Upstream has these only as library code that nothing in the app calls |
| Mercurial repositories | No version-control provider, so its endpoints answer empty |
| `--wizard` mode | Prints an error |
| Experimental voice and client-hosted MCP endpoints | Listed in the API document like upstream's, but answer 500 |
| Other experimental 2.1 features | Background compaction, `location_idle_timeout` and the new designs. Their config keys are accepted and do nothing |
| Some cloud credentials | AWS SSO, web identity and instance metadata, and Google workload identity federation. Env vars, shared config files, `credential_process` and ECS credentials work |
| Testing on macOS, Windows and Linux ARM | Release binaries are built for them, but only Linux x86_64 is tested |

## Different on purpose

| Area | Difference |
|---|---|
| Name | The program name, help text and logo say `ocd` / `opencode-d`. Config, data folders, `OPENCODE_*` variables, the HTTP API and plugin package names keep opencode's names |
| TUI working status line | After you press Enter, a line above the prompt shows what's happening: waiting for the model, thinking, writing, running a tool, retrying or compacting. It also shows a timer ticking every second, the output token count and the interrupt key. A running shell command goes on the row below. Upstream shows only its moving progress bar, which ocd keeps, plus a live "Thinking" row in the transcript, which ocd leaves out while the status line shows it |
| `ocd upgrade` and update checks | Installs from ocd's GitHub releases with checksum verification, and checks once a day instead of every 10 minutes ([details](INSTALL.md#upgrading)) |
| Heap snapshots | The server has no JavaScript heap, so `POST /api/debug/heap-dump` answers `500 UnknownError`. `ocd debug heap-dump --server URL` still captures one from an upstream server |
| Version | `ocd --version` shows ocd's own build next to the compatible opencode version. Clients, plugins and the service see 2.1.4 on the `latest` channel ([details](INSTALL.md#versions)) |

# Catching up with opencode 2.2.2

[← Compatibility](COMPATIBILITY.md)

Status: done (2026-10-30) for everything upstream turns on by default, plus `/goal` and background compaction.
Image generation, scheduled prompts and session media storage are left for a later batch: their endpoints,
config keys and database tables exist and answer like 2.2.2 does with the feature turned off.

Before this catch-up ocd replicated upstream tag `v2.1.4` (commit `68bc26b`, see [UPGRADE-2.1.4.md](UPGRADE-2.1.4.md)).
Upstream released 2.2.0 (Oct 27), 2.2.1 and 2.2.2 (Oct 29). The 2.2 tags are not descendants of `v2.1.4`, so the
work was planned from the tree diff `v2.1.4..v2.2.2` (about 143,000 added lines, much of it the desktop app and
the `@opencode/ai` library) and upstream's release notes.

How it was made: four read-only reviews of the diff, one per area (core session and Code Mode, providers and the
model catalog, HTTP API/storage/plugins, TUI and CLI), each checked against ocd's code. They found about 120
developer-days of changes that are on by default. The work was then done in seven batches, each checked against
an upstream 2.2.2 binary built from the tag and, where upstream has them, against upstream's own tests.

## Batches

| Batch | What it ported | How it was checked |
|---|---|---|
| Storage and API | The 8 new migrations (58 in all), `time.busy` on sessions, reading and updating `session_part` rows, reading media blobs a newer release wrote, the HTTP API regenerated from 2.2.2's OpenAPI document (162 operations), `hostname` in `/api/info`, `?types=` on `/api/event` and `/api/model`, MCP server titles, `source` on prompt attachments, equivalent location paths, location-free session routes, `generate/text` with typed errors and one-shot retries, fewer redundant `*.updated` events, every new config key | A 2.1.4 database migrated by 2.2.2 and by ocd has the same schema, and each opens the other's database. The changed endpoints and event sequences are identical to the 2.2.2 binary |
| Model catalog | The models.dev v2 catalog (decode, ETag cache with versioned keys, the bundled 2.2.2 snapshot, templated per-model URLs and the provider transforms behind them), model types, prompt-cache TTLs, credential metadata kept out of model settings, the OpenCode Console usage limits and workspace config layer, GPT-6.1 Terra, the removed Cohere/Perplexity/Vercel plugins, coordinated OAuth refreshes | Upstream's own catalog decoder (run with Bun) gives the same result for all 230 bundled providers. `/api/provider`, `/api/model` and the Console config requests are identical to the 2.2.2 binary |
| Code Mode | Promise jobs on one queue in V8 order, stalled awaits failing at once, located parse errors, the nesting-depth message, timeouts inside built-ins, awaitable `search`, inherited `for...in` keys, extension `before` hooks running first, Code Mode tools exposed directly when `execute` is denied, the new instruction texts | 1,800 of 1,802 executions recorded from upstream 2.2.2 replay identically (recorder in `crates/oc-codemode/tests/recorder`); test262 sample 3,339 of 3,340 |
| Provider wire | Throttling no longer reported as context overflow, AWS error types, typed Go/budget/model-budget/ChatGPT limit errors, provider-reported costs, video input, Anthropic `speed`, Bedrock options, Responses reasoning mode, xAI `top_k`, Kimi tool schemas, AVIF/HEIC/QuickTime detection, Responses for OpenRouter/DigitalOcean/Venice, malformed MCP tools skipped, 1% output headroom | 242 recorded upstream responses decode identically to upstream's own routes, and 22 request cases are byte-identical; side-by-side group `wire22` |
| Git, files and tools | Coalesced snapshot captures, split-index repositories, remote branch listing, finding git on macOS, cached project discovery, project icons, `vcs.diff ?untracked=folders`, `fs/watch`, the persistent-terminal cwd check and `@opencode/pty` 0.3.3, webfetch checking every redirect, patch moves, subagent texts, `$0` in commands, JPEG screenshots in the read tool | All compared endpoint cases identical to the 2.2.2 binary; side-by-side group `tools22` |
| TUI and CLI | `/goal` status and dialog, the OpenCode Console usage limits (`/usage`, model picker, prompt label, sidebar, warnings, refusal notices), errors before the first step, continued-subagent spinner, attachment source paths, the CLI's server flags accepted anywhere (effect 4.0.6, PowerShell completions), JSON connection errors in `run`, service and shutdown fixes, `pair --remote` through HTTP proxies (`@opentunnel/client` 0.6.1), QR byte mode, mini goldens on OpenTUI 0.7 | CLI help and errors byte-identical to the 2.2.2 binary (142 cases, with and without colour); `/diff`, `/usage` and `/models` screens identical in tmux |
| Session and plugins | `/goal` (command, `opencode.goal` RPC, `rpc.opencode.goal.changed`, reviewer loop, reminder, watchdog, pause on restart), the `experimental.idle` hook, background compaction (`experimental.background_compaction`), compaction sized from the provider's reported tokens, plugin session instruction entries, event subscription filters, the plugin sidecar rebuilt on 2.2.2's plugin package | Side-by-side groups `goal` and `background`: all 8 scenarios identical |

## Final check

The full side-by-side suite against the 2.2.2 build: **95 of 98 scenarios identical** (details in
[PARITY.md](PARITY.md#results)). Its first run after the seven batches found five more differences, fixed in a
follow-up batch: the built-in `browser` skill (from `packages/plugin-browser`, which the reviews had not covered)
and the rule denying it to every agent, events of the session routes that no longer run in a location carrying
one, MCP servers starting after the boot notifications instead of before, an extra `skill.updated` after a
failing plugin transform, and the prompt estimate behind the output limit. Workspace tests: 2,076 passed.

## Left for later

- **Image generation** (`image_generate` tool, on by default in 2.2.2 when a connected provider has image
  models), the image and decision APIs and models.
- **Scheduled prompts** (`/schedule`, `experimental.schedules`).
- **Session media storage** (`experimental.media_blob` writes and media URIs). ocd reads blobs written by upstream.

## Known differences

- Freeform (custom) tools of the OpenAI Chat and Responses protocols are not ported; upstream core never declares
  them.
- The OpenCode Console's `http.response` explain hook (rewriting 401/403 inference errors) is not ported.
- One process-wide `/goal` instance instead of one per location; the visible behaviour is the same.
- `ocd plugin list` still doesn't list TUI plugins (an older gap), so 2.2's path matching there doesn't apply.
- The shutdown deadline is armed only by `serve`.
- A loaded location whose directory is deleted keeps serving session prompts; upstream answers 404. This
  predates 2.2.
- Error texts that come from Rust instead of Bun (transport and strict provider-option errors) differ.

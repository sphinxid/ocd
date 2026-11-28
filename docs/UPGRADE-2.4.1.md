# Catching up with opencode 2.4.1

[← Compatibility](COMPATIBILITY.md)

Status: done (2026-11-28); ocd now reports 2.4.1, and its releases, web UI and plugin helper are built from the
`v2.4.1` tag (commit `6f05140`). Before, ocd tracked opencode 2.3.2 ([UPGRADE-2.3.2.md](UPGRADE-2.3.2.md)). Upstream
released 2.4.0 (commit `44aed4f`) and 2.4.1 (commit `6f05140`) on Nov 23.

How it was planned: five read-only reviews of the tree diff `v2.3.2..v2.4.1` (354 commits, about 210 of them in
areas ocd ports), one each for core, Code Mode and automations, providers and MCP, the TUI and CLI, and machines.
Each review checked every change against ocd's code. The desktop app, the web app, the stats site, Nix and upstream's
own Rust client (`packages/cli-rs`, a client of the TypeScript server that does not ship yet) are out of scope.
Only released behaviour counts, so a feature that is on only in local and dev builds is opt-in for ocd.

## What changes upstream

- **On by default now**: automations (a top-level `automations` setting, default `true`, replaces the experimental
  flag), MCP Events (the beta wire; the `mcp_events` flag is gone), video generation (`generate_video` while a
  connected provider has a video model), assistant parts written as `session_part` rows, and the TUI's new designs
  with a new `opencode` theme (the old palette ships as `opencode-classic`; `experimental["new-designs"]: false`
  keeps the old layout).
- **Flags**: `automations`, `video_generate` and `mcp_events` are removed, and so is the `schedules` alias. New:
  `recommended_api`, `model_speeds`, `machines`, `home_memory`, `project_overlay`, `sqlite_fts`, and `opencode_api`
  (never on in a release build). `todo` is opt-in only; `full_codemode` is no longer opt-in only, so
  `OPENCODE_FLAGS=all` and `experimental.all_flags` turn it on.
- **Database**: two migrations (`machine`, `machine-key`), applied whatever the flags say.
- **API**: 173 → 178 operations (machine list, add, remove and link; job cancel) and 518 → 528 schemas, none
  removed; a new `experimental.job.updated` event.
- **Code Mode**: the language moved to the npm package `psychoscript` 0.2.1 and gained getters and setters, classes,
  V8-style `stack`s, locale-aware `localeCompare` and Date formatting, and more Buffer and base64 options. The
  automation tools move to their own `automation` namespace; `execute` reports the files a script changed; a
  top-level call to a Code Mode tool runs inside `execute`.
- **Automations**: a fixed shell command as an action, scheduled code gets the session's Code Mode tools, plugins
  can create, list and delete automations.
- **Providers**: Google Interactions continuation, safety settings and Office documents; timeouts on every request
  (media, embeddings, downloads); tool namespaces flattened once for every protocol; ChatGPT plan model lists from
  models.dev, sign-out revoking the token, a clear message when a refresh is rejected; `COPILOT_GITHUB_TOKEN`; the
  local agent CLI provider's `configDir` folders, rotation and web search; MCP negotiating the newest protocol.
- **Session and tools**: background results as `<background-result>` blocks, `<system-update>` in place of the
  old reminder tags, small fields before large output in tool results, shell asking permission for commands it can't
  analyse, edit and patch refusing non-UTF-8 files, a compaction tail that stays within `keep.tokens`, config
  reloads only when a config input changes, plugins on `@latest`, a range or a branch updating themselves.
- **TUI and CLI**: `/login` and `/auth`, a hidden `login` command, running scripts in the composer picker, script
  diffs under `execute`, OpenTUI 0.9.4's Markdown engine.
- **Machines** (flag `machines`, off in release builds): only the home side ships; there is no worker yet.

## Batches

| # | Batch | Days |
|---|---|---|
| R0 | Foundation: flag registry and resolution, the `automations` setting, `part_rows` by default, the two migrations, OpenAPI, skipping plugin generations 2.4.1 rejected, `<system-update>` | 4 |
| R1 | Session and tools: compaction tail, goal reminder, `<background-result>`, tool-input repair, result field order, shell changes, UTF-8 refusal, config reload inputs, job cancel and `job.updated`, the rest of the core fixes | 8 |
| R2 | Automations and the Code Mode surface: namespace, shell action, scheduled code tools, plugin automation API, execute file diffs, catalog limits | 13 |
| R3 | MCP: newest protocol by default, the MCP Events beta without a flag, `list_mcp_events` | 10 |
| R4 | Providers: small fixes, tool-name flattening, Google Interactions, timeouts, catalog refresh, sign-out revoke | 14 |
| R5 | The local agent CLI provider's `configDir`, rotation, renamed models and web search | 11 |
| R6 | Video generation | 15 |
| R7 | TUI and CLI small changes, job tracking and script diffs in the TUI | 6 |
| R8 | New designs | 20 |
| R9 | OpenTUI 0.9.4 rendering differences | 3.5 |
| R10 | Code Mode language (psychoscript 0.2.1) and a re-recorded corpus | 26–31 |
| R11 | Machines as a release build shows them with the flag off (403 routes, `ocd machine` commands) | 2 |
| R12 | Version, web UI, plugin helper, side-by-side checks | 3 |

Order: R0, then R1, R4, R7 and R11 side by side; then R2 with R3 (2.4.1 turns both on at once) and R9; then R8
(themes first, since the default palette changes even with the new designs turned off) and R6; then R10 and R5.

Opt-in features (about 28 days: the rest of `full_codemode`, `sqlite_fts`, `home_memory`, `project_overlay`,
`model_speeds`, `recommended_api`, Console watch fixes) come after, on request. The full machines feature (about
24–30 days) waits until upstream ships a worker.

## Checks

Filled in as batches land.

- **R0** (done): the flag registry follows 2.4.1, including `localOnly` and the opt-in-only list. The top-level
  `automations` setting is read with 2.4.1's diagnostics, but automations stay off until R2. `mcp_events` stays
  gated until R3 and video until R6. The OpenAPI document is regenerated (178 operations, 528 schemas). Both
  migrations apply. Plugin generations that 2.4.1 rejected are skipped.
- **R1** (done): checked side by side against the 2.4.1 binary (`scripts/parity/session241.py`). Unanalysable shell
  commands and job cancel are identical. Tool repair, result field order and background results match on everything
  R1 changed; what still differs belongs to R2 (the Code Mode catalog) and R6 (`generate_video`). Not ported:
  `session_search`'s field order, because ocd has no such tool.
- **R2** (done): automations are on by default and the `automation` namespace, shell actions, scheduled code with
  Code Mode tools, script diffs (core and TUI), the catalog limits and the `opencode://` refusal follow 2.4.1. The
  plugin side of `experimental.automation` came with the plugin helper rebuild in R12. The opt-in `full_codemode`
  parts (native description overrides, the model policy in the Code Mode catalog, Bun services for automation code)
  are not ported.
- **R3** (done): MCP negotiates `auto` by default with the legacy fallback, the MCP Events beta is on without a flag,
  `list_mcp_events` replaces `watch_mcp_event`, and event watches are held while automations are off. The new
  `mcp241` parity scenarios are identical; the remaining automation and MCP differences are the media tool names (R6).
  Not ported: mirroring `Mcp-Param` headers on tool calls (event streams do).
- **R4** (done): the recorded request bodies for flattening, Google Interactions continuation and the catalog refresh
  are checked against 2.4.1 (`scripts/parity/wire241.py`). Token revocation on sign-out covers plugin sign-in
  methods since the plugin helper rebuild in R12.
- **R5** (done): the local agent CLI provider keeps renamed model IDs, runs in `configDir` folders and rotates over a
  list of them on subscription limits, stops a deleted session's process, and can serve web search. The six new
  `claude241` parity scenarios are identical with 2.4.1, checked against a stand-in CLI.
- **R6** (done): `generate_image` and `generate_video` (old names rewritten in permissions), the xAI, MiniMax,
  OpenRouter, Seedance and Google video protocols (upstream's tests and recorded replays ported), durable video jobs
  with restart recovery, the `/generate/video` routes, and `blob_<id>` blobs deleted a week after writing. The new
  `video` parity scenarios are identical, and the media-name differences left by R2 and R3 are closed.
- **R7** (done): the CLI help, the hidden `login` command, log redaction, the TUI's `/login` and `/auth`, and job
  tracking.
- **R8** (done): `opencode` is the new palette, `opencode-classic` the old one and `opencode2` an alias. Every
  built-in theme, the system theme and mini match upstream's recorded theme tokens. The new designs are on by default
  and `experimental["new-designs"]: false` restores the old layout; 20 of upstream's cell-exact screen fixtures pass,
  and pty captures of the main screens and dialogs match 2.4.1 with both values. Not ported: hover highlights in
  dialog footers and the usage model dialog's model facts. The command palette's group order below Suggested
  differs, in both designs.
- **R9** (done): Markdown and reasoning in the TUI and mini follow OpenTUI 0.9.4's engine. Pty captures against 2.4.1
  with `experimental["new-designs"]: false` show identical text; colours differ only by the new palette (R8). Tests
  include 2.4.1's own frames and OpenTUI's conformance rows. Not changed: link URLs are always shown (upstream hides
  them on terminals with OSC 8), and wrapping outside Markdown still breaks only at spaces.
- **R10** (done): the Code Mode language follows psychoscript 0.2.1: getters and setters with descriptors, classes,
  V8-format stacks, the Buffer and base64 options, the import error texts, and Intl in `localeCompare` and Date
  `toLocale*`. 278 programs recorded through 2.4.1's Code Mode, about 1,300 programs from psychoscript's own tests
  (MIT, Anomaly), 400 random Intl date calls and the test262 sample all match. Gaps: Intl data covers 20 locales
  (others fall back to the nearest one with the same language, else en-US), zone history before about 1970 can
  differ, other languages collate in root order, and the ~7,300 test262 files 2.4.1 added to its subset are not
  recorded.
- **R11** (done): 22 CLI cases and the machine routes match a 2.4.1 release build with the `machines` flag off.
  ocd keeps answering 403 when the flag is on (see COMPATIBILITY.md).
- **R12** (done): ocd reports 2.4.1; the release workflows, the web UI build and `host.js` use the `v2.4.1` tag.
  The plugin helper bundles 2.4.1's plugin package and follows its host: `ctx.experimental.automation`, Code Mode
  overrides in a tool's `options.codemode` (description and schemas, `context.codemode`, the new registration
  error), `info` and `codemode` on tool hooks, and `revoke` through the Promise adapter; upstream's plugin automation,
  Promise tool override and integration revoke tests are ported against the real helper. The side-by-side run against
  the 2.4.1 build: **281 of 309 identical** (`fcm231_all_flags` not run: `all_flags` turns on `machines`). It found
  and fixed five differences: the five flag-gated built-in plugins 2.4.1 lists (`opencode.home-memory`,
  `opencode.tool.machine`, `opencode.tool.session-interrupt`, `opencode.tool.session-search`,
  `opencode.config.session-search`), the tool policy reading the session's selected model before Copilot Auto routes
  it, the prompt estimate counting the shell's name (2.4.1 registers it in the description), a stopped shell
  command's job settling `completed` from its exit, and a cancelled subagent's job update carrying no location. Left
  ([PARITY.md](PARITY.md)): 2.4.1's desktop browser plugin changes (its skill text and tools, about nine scenarios),
  the order of a shell job's update against the step's usage update, opt-in full Code Mode and Console features, and
  the known renames, races and orderings. Tests: 3,463 passed, none failed.

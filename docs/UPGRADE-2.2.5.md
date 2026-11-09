# Catching up with opencode 2.2.5

[← Compatibility](COMPATIBILITY.md)

Status: done (2026-11-09); ocd now reports 2.2.5, and its releases and web UI are built from the `v2.2.5` tag.
Before, ocd tracked opencode 2.2.2 ([UPGRADE-2.2.2.md](UPGRADE-2.2.2.md)); upstream released 2.2.3 and 2.2.4 (Oct 30)
and 2.2.5 (Oct 30, tag `v2.2.5`, commit `9bbb8ce`).

How it was planned: four read-only reviews of the tree diff `v2.2.2..v2.2.5` (about 33,000 added lines, much of it
the desktop app), one per area, each checked against ocd's code. Every change that is on by default is ported;
each batch is checked against an upstream 2.2.5 build from the tag and against upstream's own tests.

## What changes upstream

- **Claude Agent provider** (2.2.3): a built-in provider that drives the local Claude Code CLI (`claude auth
  login`, `OPENCODE_CLAUDE_PATH`), with its own integration in `/connect`. It is on by default.
- **Managed policy split** (2.2.3): `integration.use` covers integrations only; new `integration.connect`,
  `mcp.use`, `skill.use` and `plugin.use` actions, with old `mcp:`/`skill:`/`plugin:` rules converted.
- **Background jobs in a `job` table** (2.2.4): a new migration moves background-job markers out of the key-value
  store, and deleting a session cancels its jobs.
- **Persistent terminals** (2.2.3, 2.2.4): removed with their session, renameable (`userTitle`,
  `persistent-pty.updated`), `@opencode/pty` 0.4.0.
- **Session and tools** (2.2.5): reasoning kept when switching models or resuming, compaction capped at the context
  window, `compaction.buffer` as a percentage, LiteLLM overflow triggers compaction, clearer `read` and `edit` miss
  errors, `bash` calls run as `shell`, large MCP stdio messages, no MCP restart on idle locations, Code Mode
  `fetch` checked against webfetch permissions, the shell permission scanner rewritten without tree-sitter.
- **TUI and CLI**: `/connect` without MCP servers, padded edit diffs, the full-width session-tab pulse, `#` in
  file search, undo and revert returning the prompt, `run --format json` step events after idle, local service
  requests kept out of proxies.

Opt-in features of these releases (scheduled prompts under `serve`, artifacts, media references) are not part of
this catch-up; their config keys are accepted and their endpoints answer as when the feature is off.

## Opt-in features (after the catch-up)

The opt-in features of 2.2.x were ported next, each checked against the 2.2.5 build:

| Feature | Status |
|---|---|
| Image generation and decisions | Done: every image and decision provider route of 2.2.5, the generate endpoints, the plugin API, the image tool with its on-by-default rule, background image jobs and recovery, GPT Image 2 after a Codex sign-in. Side-by-side group `image` 11 of 11 identical |
| Scheduled prompts (`experimental.schedules`) | Done: `/schedule`, the schedule tools and API, shell and Code Mode checks (Code Mode's `scanTools` and bindings), unattended permissions, cron/duration/date parsing as Effect does it, and the `schedule:leader` lease. Group `schedule` 7 of 7 identical |
| Artifacts (`experimental.artifacts`) | Done: the local and plugin providers, tools, routes and `view`, `artifact.updated`. Group `artifact` 6 of 6 identical |
| Session media (`experimental.media_blob`) | Done: blob writes, `media:` references under the media capability, fork and import, release, and the TUI's restore of stored images. Group `media` 4 of 4 identical. The TUI's image previews are not ported (ocd draws no images in the terminal) |
| Platforms | A CI workflow runs the whole test suite on macOS, Windows and Linux ARM. Its first runs found the retired `macos-14` runner (the release workflow now uses `macos-15`) and two kinds of platform-dependent test expectations (libm digits of the transcendental `Math` functions, and macOS's physical temporary paths); the tests now account for both |

Afterwards: tests 2,407 passed; the full side-by-side suite **173 of 177 identical**.

## Batches

| Batch | Scope | Status |
|---|---|---|
| Storage, API and policy | Job table and migration, OpenAPI 2.2.5 (169 operations), the policy split, `compaction.buffer` percentages, new config keys, disabled-feature answers, `NO_PROXY`, web UI CSP | Done: databases written by either side open in the other, including job rows migrated by 2.2.5; every artifact, schedule, connect, well-known and credential policy answer compared is identical; side-by-side group `api225` 3 of 3 identical. Left: restart recovery also runs outside `serve --service`, and well-known config documents are still not loaded (both older) |
| Claude Agent | The provider, its integration and probe, the stdio protocol, sessions and transcripts, model catalog, plugin list order, plugin variants, dynamic provider order, the plugin helper on 2.2.5 | Done: native port (`oc-core/src/claude_agent`), tested only against a scripted stand-in executable; transcript fork and delete use the real SDK 0.3.301 in the plugin helper (no native binary is installed). Side-by-side group `claude`: in all 12 scenarios the traffic with the stand-in, stored messages and durable events are identical; 8 still differ in when `provider.updated`/`model.updated` fire after a credential is created, a generic credential gap left for the final check |
| Shell scanner | The tree-sitter-free shell permission scanner | Done: tree-sitter removed (about 2.4 MB smaller), the built-in scanner always runs. 95,128 calls recorded from upstream 2.2.5's own parse and scan code match, apart from two that read the recording process's environment; side-by-side group `shell225` 2 of 2 identical |
| Session, tools and terminals | Reasoning across model switches, compaction cap, tool errors, MCP stdio and idle shutdown, Code Mode changes, provider wire changes, terminal rename and cleanup | Done, plus two older gaps: well-known organization config now loads, and restart recovery runs only under `serve --service`. `@opencode/pty` 0.4.0 pinned (its linux-x64 hash matches the one 2.2.5 embeds). Side-by-side group `session225` 20 of 21 identical (the MCP framing scenario differs only by event timing); every older tools, MCP and reasoning scenario rerun against 2.2.5 is identical |
| Final check | The full test suite and the full side-by-side suite against the 2.2.5 build | Done: tests 2,287 passed. The first full run found the remaining differences in when credential changes announce `provider.updated`/`model.updated`, the order of plugin restart notifications, Copilot's model sync per location, invalid browser results' decode errors and the MCP tool list after a server closes; all fixed. Then **145 of 149 scenarios identical**; left are `cli_help` (intended renames), `cli_inventory` (the upstream build's database file name) and `stop_retry_exhausted` (an upstream race) |
| TUI and CLI | Every on-by-default TUI change, the tab pulse, undo and revert, file search, `run` JSON steps | Done: `/connect` without MCP servers, diff padding (and an older one-cell gutter offset), `#` line ranges in the full TUI and mini, the cursor style and colour, undo/revert/queued undo returning the whole prompt, missing-session eviction, `persistent-pty.updated`, `run --format json` step events after idle, and the session-tab pulse and title shimmer, which ocd never had. In tmux against the 2.2.5 binary: diffs, file search, `/connect` (also policy-filtered and with the new provider), undo and the shimmer identical; the pulse within a channel step or two per frame; PHP token colours differ (syntect vs tree-sitter) |

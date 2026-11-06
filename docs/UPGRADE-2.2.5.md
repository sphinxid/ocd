# Catching up with opencode 2.2.5

[← Compatibility](COMPATIBILITY.md)

Status: planned (2026-11-06). ocd tracks opencode 2.2.2 ([UPGRADE-2.2.2.md](UPGRADE-2.2.2.md)); upstream released
2.2.3 and 2.2.4 (Oct 30) and 2.2.5 (Oct 30, tag `v2.2.5`, commit `9bbb8ce`).

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

## Batches

| Batch | Scope | Status |
|---|---|---|
| Storage, API and policy | Job table and migration, OpenAPI 2.2.5 (169 operations), the policy split, `compaction.buffer` percentages, new config keys, disabled-feature answers, `NO_PROXY`, web UI CSP | Planned |
| Claude Agent | The provider, its integration and probe, the stdio protocol, sessions and transcripts, model catalog, plugin list order, plugin variants, dynamic provider order, the plugin helper on 2.2.5 | Planned |
| Shell scanner | The tree-sitter-free shell permission scanner | Done: tree-sitter removed (about 2.4 MB smaller), the built-in scanner always runs. 95,128 calls recorded from upstream 2.2.5's own parse and scan code match, apart from two that read the recording process's environment; side-by-side group `shell225` 2 of 2 identical |
| Session, tools and terminals | Reasoning across model switches, compaction cap, tool errors, MCP stdio and idle shutdown, Code Mode changes, provider wire changes, terminal rename and cleanup | Planned |
| TUI and CLI | Every on-by-default TUI change, the tab pulse, undo and revert, file search, `run` JSON steps | Planned |

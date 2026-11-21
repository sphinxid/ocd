# Catching up with opencode 2.3.1

[← Compatibility](COMPATIBILITY.md)

Status: done (2026-11-18); ocd now reports 2.3.1, and its releases and web UI are built from the `v2.3.1` tag
(commit `c769de8`). Before, ocd tracked opencode 2.3.0 ([UPGRADE-2.3.0.md](UPGRADE-2.3.0.md)); upstream released
2.3.1 on Nov 17.

How it was planned: four read-only reviews of the tree diff `v2.3.0..v2.3.1` (189 commits), one per area, each
checked against ocd's code. Every change that is on by default is ported, plus automations (opt-in, because they
replace the experimental schedules ocd already had) and the process-daemon shells left over from 2.3.0. Each batch
is checked against an upstream 2.3.1 build from the tag and against upstream's own tests.

## What changes upstream

- **Database**: three migrations (59 → 62) add the `automation` table, copy simple schedules into it and drop the
  `schedule` table. They run on every open, flag or not. ocd and 2.3.1 migrate a 2.3.0 database to the same rows and
  open each other's databases.
- **Automations** replace the experimental schedules (`experimental.automations`, old name `schedules`): at, every
  and cron triggers, prompt and code actions, runs leased across processes, the automation API, tools and
  `/automation`. Event triggers need MCP Events (ported later, see below).
- **On by default**: background compaction (`compaction.background`); a shell command that outlives its timeout
  moves to the background; the read tool's new output shapes and limits; `CLAUDE.md` used where no `AGENTS.md`
  exists (turned off by `OPENCODE_DISABLE_CLAUDE_CODE` or `OPENCODE_DISABLE_CLAUDE_CODE_PROMPT`); grep keeping
  non-UTF-8 matches; hidden files in file search; a permission rejection cascading only within its step; readable
  500 bodies; reasoning replayed as the field a model declares; ChatGPT token-sharing safeguards kept when a refresh
  fails; GPT 6.2 models; no time to first token in the TUI footer; permission prompts over the composer; ACP turns
  woken by background shells.
- **Code Mode**: Node's `Buffer`; tool arguments cross to the host as copies (no `toJSON`, new errors for values that
  can't cross); about ten reworded messages; `search("x")`; no `null` line before logs; 500-character tool
  summaries; plugin tool renames; a nested call's permission request on the `execute` row.
- **Process daemon**: `@opencode/pty` 0.6.0 with protocol 9.
- **API and config**: 173 operations (video, blob and automation routes added; schedule routes removed), 518
  schemas; `experimental.full_codemode` (opt-in only), `video_generate`; `background_compaction` and
  `shell_timeout_background` removed.

Ported later (all opt-in upstream): `full_codemode` (marked work in progress upstream; see COMPATIBILITY's Code Mode
row), MCP Events with automation event triggers, the Console artifact provider and watches, the MCP pool, the
`part_rows` writer, session warming, client-hosted MCP and the TUI's image previews. Not ported: video generation
itself (its routes answer 403 as in a release build; video models stay in the catalog).

## Batches

| Batch | Scope | Result |
|---|---|---|
| Foundation | Migrations, OpenAPI 2.3.1, flags and config, background compaction on, schedules retired, artifact boot event | Databases checked in both directions; `api231` identical |
| Code Mode | `Buffer`, argument crossing, messages, search, logs, renames, nested permission rows; recorder rebased on 2.3.1 | 1,873 of 1,875 recorded programs; 1,188 new programs identical to 2.3.1 |
| Core tools | Shell timeouts, read shapes, `CLAUDE.md` fallback, search and grep fixes, session fixes | 53 of 57 scenarios in its groups identical (the rest known races and one ranking approximation) |
| Providers and server | Reasoning replay, ChatGPT safeguards, GPT 6.2, readable 500s, the local agent CLI's context sizes, video catalog | 512 request bodies identical to 2.3.1; `providers231` identical |
| TUI and ACP | Footer, permission over composer, ACP background shells, `run` and Code Mode jobs | `acp` identical when merged; screens match |
| Automations | The automation service, API, tools, `/automation`, leasing, recovery | 8 of 10 identical when merged, the other two by the core tools' texts (identical after) |
| Daemon shells | Terminal client on protocol 9; shell commands in the daemon, surviving a restart, with a spawn fallback | Every shell and terminal scenario identical, including two restart scenarios |
| Final check | Version 2.3.1, web UI and plugin helper from the tag, full tests, full side by side | Tests 2,876 passed; 257 of 263 scenarios identical; CI green on macOS, Windows and Linux ARM |

Along the way: a macOS-only race in an automation test, Windows file names in `read` and `grep`, and a Windows stack
overflow (large futures now boxed; ocd's main thread on Windows reserves 8 MiB) were fixed, and the side-by-side
harness now forces the dead proxy itself.

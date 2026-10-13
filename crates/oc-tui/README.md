# oc-tui

A ratatui 0.29 + crossterm 0.28 terminal UI for the opencode v2 port. It is a client
of the opencode HTTP API and nothing else: it uses `/api/*` REST calls and the SSE
event stream `GET /api/event`. Spec: `docs/spec/07-tui.md`.

## Public API

```rust
pub async fn oc_tui::run(opts: oc_tui::TuiOptions) -> anyhow::Result<i32>

#[derive(Clone, Debug, Default)]
pub struct TuiOptions {
    pub url: String,                      // server base URL, e.g. "http://127.0.0.1:4096"
    pub password: Option<String>,         // Basic auth password (the username is always "opencode")
    pub directory: String,                // launch directory, used as the default location
    pub session: Option<String>,          // --session <id>
    pub continue_last: bool,              // --continue
    pub fork: bool,                       // --fork (combined with --session or --continue)
    pub prompt: Option<String>,           // --prompt <text>
    pub model: Option<String>,            // --model provider/model
    pub agent: Option<String>,            // --agent <id>
    pub auto: bool,                       // --auto: accept permission requests automatically
    pub channel: String,                  // storage channel for <state>/<channel>/tui/*.json
    pub version: String,                  // version shown in the home footer and debug dialog
    pub environment: HashMap<String, String>, // env forwarded to new sessions
}
```

`run` takes over the terminal: it switches to the alternate screen and enables raw
mode, mouse capture, bracketed paste and focus reporting, plus the Kitty keyboard
flags when the terminal supports them. It returns the exit code: 0 on a normal exit,
1 on a fatal I/O error. When the TUI exits from a session, it prints the session
epilogue (the wordmark, `Session <title>` and `Continue opencode -s <id>`) to stdout.

A typical call from the CLI crate:

```rust
let code = oc_tui::run(oc_tui::TuiOptions {
    url: server_url,
    password: Some(password),
    directory: std::env::current_dir()?.to_string_lossy().into(),
    session: args.session,
    continue_last: args.r#continue,
    prompt: args.prompt,
    channel: "latest".into(),
    version: env!("CARGO_PKG_VERSION").into(),
    ..Default::default()
}).await?;
```

### Mini mode

```rust
pub async fn oc_tui::run_mini(opts: oc_tui::TuiOptions, mini: oc_tui::MiniOptions) -> anyhow::Result<i32>

#[derive(Clone, Debug, Default)]
pub struct MiniOptions {
    pub replay: Option<bool>,       // --replay / --no-replay (falls back to `mini.replay`, then true)
    pub replay_limit: Option<i64>,  // --replay-limit N (falls back to `mini.replay_limit`, then 200)
}
```

`run_mini` is the `opencode mini` frontend. It does not use the alternate screen or mouse
capture. Finished transcript rows go to the terminal's native scrollback, and only a live
footer is redrawn below them. The footer holds the in-progress tail of the current turn
(at most 8 rows), then the prompt (or the permission or form dock), dialogs and toasts.

- **Output:** prints the entry header `▪ oc mini v<version> · <path>`. On exit it prints the
  `go` logo with `Session <title>` and `Continue opencode mini -s <id>`. `mini.splash: "hide"`
  turns both off.
- **Replay:** a resumed session's history is replayed according to `replay` / `replay_limit`.
  `ctrl+l` (`app.clear`) clears the screen and replays. A width change does the same when
  replay is on.
- **Validation:** the same messages and exit code 1 as upstream (`opencode mini requires a TTY
  stdout`, `--replay-limit must be a positive integer`, `--fork requires --continue or
  --session`).
- **Piped input:** piped stdin is merged with `--prompt` (`piped + "\n" + prompt`), and keys
  are then read from the controlling terminal.

## Features

The modules below cover the main session and home screens and all dialogs. They also cover:

- **Session tab rail:** vertical rail plus horizontal strip, with drag-reorder, rail resize
  and the tab context menu.
- **Full-screen views:** the diff viewer route (`diff.open`: tree, unified or split, base
  picker) and the stats route.
- **Embedded terminal pane:** uses the server PTY WebSocket (input protocol 1) and renders
  with `vt100`. Supports multiple terminals, focus switching and pane resize.
- **Session tools:** message next/previous/first/last navigation, the export options
  dialog, and the subagent/shell composer drawer.
- **Open dialog** (`ctrl+o`): recent sessions and projects. `→` on a git project shows its
  worktrees; `ctrl+n` creates a new worktree.
- **Worktrees dialog** (`/worktrees`): move, new, delete (press twice to confirm,
  force-delete prompt) and refresh.
- **Pair dialog** (`/pair`): one-time links and a QR code from the `qrcode` crate, drawn
  like `uqr` `renderUnicodeCompact`.
- **Copy:** mouse drag-select copies on release when `terminal.copy` is `"select"`. In
  `"manual"` mode, right-click copies. While a selection exists, `esc` and `ctrl+c` follow
  `handleSelectionKey`.
- **V1 history import badge** (upstream `MigrationOverlay`, full TUI only, not mini): 1 s after start the TUI
  polls `GET /api/experimental/migration/v1` every second until it reports `completed`. While the status is
  `running`, a badge sits at the top right (`top 1`, `right 2`, drawn over toasts, dialogs and the reconnect
  overlay). It has a `┃` left border and the spinner and `<label>[ <n>[/<d>]]` text, all in
  `text.feedback.info.base` on `background.raised.high`, with padding 2×1. With animations off the spinner becomes
  `⋯ `. Any other status hides the badge. `error`, or a failed request, stops polling and shows the 10 s error
  toast "Data migration failed" with the error text. The badge has no keybindings. On terminals too narrow for
  it, it uses the same layout as upstream's Yoga quirk: the box extends past the left edge and only the first
  wrapped line shows.
- **Busy status line** (intentional difference from upstream, `app/busy.rs`): upstream only shows the
  KnightRider bar in the prompt footer while a session runs, which stays the same whether the server is still
  setting up, the model has not sent its first token yet, or a tool is running. ocd adds one status line, like
  Claude Code's, from the moment Enter is pressed until the session goes idle:
  `⠹ Waiting for Claude Sonnet 4.5… (12s · ↓ 1.2k tokens · esc to interrupt)`. Upstream's footer bar is kept.
  - **Where:** in the full TUI it goes on the left of the 1-row strip between the transcript and the bottom dock,
    which upstream leaves blank. "Jump to latest ↓" keeps the right side, and the status line is shortened to fit
    next to it. In mini mode it is the footer row right above the prompt, permission or form dock.
  - **Parts:** the spinner (braille frames at 80 ms in the agent colour, or a static `⋯` with animations off), the
    phrase (`text.base`; `text.feedback.warning.base` for retries and for permission/question waits), then in
    `text.muted`: the elapsed time, the output token count and the interrupt hint. The elapsed time counts from
    Enter (`12s`, `1m 05s`, `1h 03m`). For a run the TUI did not start (a resumed or remotely started session), it
    counts from the first message of the run. The token count is output plus reasoning tokens of the run's finished
    steps, plus `chars / 4` for the step in progress; it is shown with `~` while part of it is estimated. The hint
    uses the `session.interrupt` binding (`esc`, shown as upstream's footer shows it) and turns into
    `esc again to interrupt` (warning colour) once armed. It is hidden while a permission or form prompt has focus,
    and in shell mode.
  - **Refresh:** the line redraws at least every 40 ms while it is shown, even with animations off, so the timer
    keeps ticking during a stall.
  - **Narrow widths:** the token count is dropped first, then the hint. After that the phrase is truncated, and
    the elapsed time stays as long as a few phrase cells fit.
  - **Phases:** they are derived only from data the TUI already has, with no new endpoint or event:
    | phrase | when |
    |---|---|
    | `Sending…` | the prompt/command request is in flight (optimistic outbox) |
    | `Setting up…` / `Connecting to MCP servers…` | accepted, but the input is still in the inbox (the server is loading the location, activating plugins, connecting MCP servers). The MCP variant is used while the location's MCP catalog has `pending` servers |
    | `Waiting for <model>…` | the input was delivered (`session.inbox.delivered`) or a step finished, and no output has arrived yet. The server publishes `session.step.started` only with the first provider event, so this covers request assembly and the wait for the first token |
    | `Thinking…` / `Writing…` | reasoning / text is streaming (the stream is open, `time.streamed` unset) |
    | `Preparing <tool>…` | a tool call's input is streaming |
    | `Running shell: <cmd>…`, `Reading <path>…`, `Editing <path>…`, `Running Explore subagent: <description>…`, … | a tool is executing, with `(+N more)` for parallel tools. A user `!` shell command shows as `Running shell: <cmd>…` |
    | `Retrying in Ns (attempt k)…` | `session.retry.scheduled`. The countdown matches the transcript's retry row |
    | `Compacting…` | a compaction is pending or running |
    | `Waiting for permission…` / `Waiting for your answer…` | a permission or form prompt is open |
  - **Plugin setup:** there is no event that marks plugin activation starting. The boot `*.updated` batch only
    arrives after activation has settled, and `plugin.list` shows configured plugins only after activation. So
    plugin setup is reported as the generic `Setting up…`.
  - **Lifecycle:** a turn is created on submit (or on `session.execution.started` / when a running session is
    viewed) and ended by `session.execution.{succeeded,failed,interrupted}`. It also ends on a failed submission
    while idle, when a `!` command's request returns, or when a turn that was acknowledged has no running execution
    within 10 s.
- **`cli.json` writes:** they keep comments and formatting. `util::jsonc_edit` is a local
  copy of `oc-config`'s jsonc-parser `modify` port, and writes fall back to a pretty rewrite
  if an edit fails.

## Modules

| module | contents |
|---|---|
| `api` | typed HTTP client (`Client`), wire types, and the SSE connection: 2000 ms connect timeout, 1000 ms reconnect delay, 45 s idle watchdog |
| `store` | event projection into sessions, messages, inbox, permissions and forms, plus per-location catalogs (port of `client/src/solid/data.ts`) |
| `theme` | V1→V2 migration (OKLCH), V2 resolution with hue provenance for `increase`/`decrease`, the `@dialog` surface, the generated `system` theme, syntax scopes, custom theme discovery |
| `keymap` | key-string parser, the 241 default bindings (generated from upstream `keybind.ts`), overrides, shortcut formatting, leader matching |
| `config` | `cli.json` (JSONC) with resolved defaults and atomic, comment-preserving writes |
| `state` | `model.json`, `session.json`, `prompt-history.jsonl`, `prompt-stash.jsonl`, `frecency.jsonl`, and the keyed `<state>/<channel>/tui/<key>.json` store, all at the upstream paths |
| `ui` | text and wrapping primitives, markdown rendering, syntect highlighting, unified and split diffs, the transcript row model and renderers, the prompt editor with chips, dialogs, form and permission helpers, the logo and epilogue |
| `app` | app state, command registry, key, mouse and paste dispatch, prompt submission, and the screen layout. Submodules: `tabs` (rail, strip and menu), `diffview`, `stats`, `terminal` (PTY pane), `composer` (subagent/shell drawer), `workspace` (open, worktrees and pair dialogs), `selection` (drag-select copy), `mini` (scrollback and footer rendering), `migration` (v1 import badge and status poller), `busy` (busy status line: turn tracking, phase derivation, rendering) |
| `mini` | the `run_mini` event loop and footer painter |
| `clipboard`, `attention`, `terminal` | clipboard (OSC 52 plus host tools), notifications and sounds, terminal palette probe (OSC 4/10/11) |

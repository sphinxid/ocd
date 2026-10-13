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
| `app` | app state, command registry, key, mouse and paste dispatch, prompt submission, and the screen layout. Submodules: `tabs` (rail, strip and menu), `diffview`, `stats`, `terminal` (PTY pane), `composer` (subagent/shell drawer), `workspace` (open, worktrees and pair dialogs), `selection` (drag-select copy), `mini` (scrollback and footer rendering), `migration` (v1 import badge and status poller) |
| `mini` | the `run_mini` event loop and footer painter |
| `clipboard`, `attention`, `terminal` | clipboard (OSC 52 plus host tools), notifications and sounds, terminal palette probe (OSC 4/10/11) |

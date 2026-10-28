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
    pub service: Option<ServiceHooks>,    // managed background service: reconnect / restart
    pub updater: Option<UpdaterHooks>,    // update notice and `/update` (see below)
}

pub struct UpdaterHooks {
    pub notices: tokio::sync::watch::Receiver<Option<UpdateNotice>>, // background check: Available / Installed
    pub check: Arc<dyn Fn() -> BoxFuture<'static, anyhow::Result<Option<UpdateCheck>>> + Send + Sync>,
    pub apply: Arc<dyn Fn(String) -> BoxFuture<'static, anyhow::Result<()>> + Send + Sync>,
}
```

`run` takes over the terminal: it switches to the alternate screen and enables raw
mode, mouse capture, bracketed paste and focus reporting, plus the Kitty keyboard
flags when the terminal supports them. It returns the exit code: 0 on a normal exit,
1 on a fatal I/O error. When the TUI exits from a session, it prints the session
epilogue (the wordmark, `Session <title>` and `Continue ocd -s <id>`) to stdout. After an update was
installed it also prints `Run ocd to use <version>`.

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
    pub replay: Option<bool>,       // --replay / --no-replay (default true)
    pub replay_limit: Option<i64>,  // --replay-limit N (default 200)
}
```

`run_mini` is `ocd mini`, a port of upstream's `opencode mini` (`packages/tui/src/mini/` plus
`packages/cli/src/mini.ts` and `mini-host.ts`). It lives in its own module tree, `src/mini/`, with
one file per upstream module (`footer`, `prompt`, `panels`, `blocker`, `scrollback`, `split`,
`transport`, `subagents`, `splash`, `tool`, `structured`, `markdown`, `fuzzysort`, ...), and shares
only the API client, the config store, the theme manager and the keymap with the full TUI, which it
does not touch.

- **Screen:** no alternate screen or mouse capture. It is a Rust port of OpenTUI's split-footer
  renderer (`split.rs`): committed rows are written into a scroll region above the footer so they
  end up in the terminal's native scrollback, and only the footer (prompt, permission and question
  docks, command, agent, model, variant, settings, subagent and queued-prompt panels, queued prompts, statusline)
  is redrawn. The startup cursor position comes from a CPR query, and the footer height follows
  upstream's rules for each view.
- **Scrollback:** upstream's entry rendering (markdown through a `marked` lexer port, code, tools
  with diffs, shell output, reasoning, todo and question summaries), streaming tails, the quiet mode
  that hides tool output, verbosity settings and the `ocd mini -s <id>` exit splash.
- **Transport:** the V2 event stream is reduced to commits, patches and blocker views the same way
  as upstream (fragment reconciler, tool versions, subagent tracker with child discovery, pending
  prompt inbox, settle by `session.wait`). Prompts queue while a turn runs; `!cmd` runs a shell turn.
- **Replay:** a resumed session's history is replayed according to `replay` / `replay_limit`.
  `ctrl+l` clears the screen and replays; a width change resets the split footer and replays.
- **Settings:** `/settings` and the variant and model pickers save to `mini.*` in the config and to
  `model.json`, as upstream does. `OPENCODE_DIRECT_TRACE` writes the same JSONL trace.
- **Validation:** the same messages and exit code 1 as upstream (`ocd mini requires a TTY stdout`,
  `--replay-limit must be a positive integer`, `--fork requires --continue or --session`).
- **Piped input:** piped stdin is merged with `--prompt` (`piped + "\n" + prompt`), and keys are
  then read from the controlling terminal.

Upstream's mini tests are ported next to each module. `testdata/scrollback.json` and
`testdata/footer.json` are goldens produced by running upstream's own modules under Bun with
OpenTUI's test renderer; the tests compare every cell (text, colors, attributes).

Remaining differences from upstream:

- Code blocks are highlighted with syntect scopes mapped to upstream's tree-sitter highlight
  groups (the workspace has no tree-sitter grammars for TypeScript and most other languages), so
  some token colors and italics differ, e.g. in TypeScript. Text and layout are the same.
- The command names say `ocd` (`ocd mini -s <id>`, `ocd mini requires ...`).
- The ocd server runs parallel tool calls in a different order than upstream's, so scrollback
  order for parallel tools can differ; that is a server difference, not a mini one.
- Upstream's narrow permission dock can leave a stale scrollbar thumb for one frame; ocd does not.

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
- **Update notice and `/update`** (upstream `UpdateNotificationProvider`, `DialogUpdate`, the home
  `UpdateNotification`; `app/update.rs`): with `TuiOptions.updater` set, the newest notice from `notices` shows
  one row under the home logo, centered on it: `/update` (or `/exit`) in `text.action.primary.selected`, then
  ` to install vX` (or ` restart to use vX` after an automatic install) in `text.muted`. Clicking it opens the
  dialog, or quits after an install. `/update` (palette "Update ocd", after "View status") opens the dialog:
  "Checking for updates…" while `check` runs, then "Update available", "ocd is already up to date.", the
  unavailable message or the error. The footer is `skip` plus `update` or `restart`; Enter runs the last one.
  `update` calls `apply` ("Updating ocd" / "Installing ocd X…"), then offers `restart` (quit). `skip` hides the
  notice and records `client:<version>` in the `update-notifications` store (last 100), which keeps it hidden.
  No fade-in animation; the footer items are not clickable.
- **Busy status line** (intentional difference from upstream, `app/busy.rs`): upstream only shows the
  KnightRider bar in the prompt footer while a session runs, which stays the same whether the server is still
  setting up, the model has not sent its first token yet, or a tool is running. ocd adds one status line, like
  Claude Code's, from the moment Enter is pressed until the session goes idle:
  `⠹ Waiting for Claude Sonnet 4.5… (12s · ↓ 1.2k tokens · esc to interrupt)`. Upstream's footer bar is kept.
  - **Where:** in the full TUI it goes between the transcript and the bottom dock, with a blank row above and
    below it (upstream has one blank strip row there, which is what remains when idle). "Jump to latest ↓" goes
    on the right of the blank row above, and the status line is shortened to fit next to it. Mini mode uses
    upstream's own statusline instead.
  - **Shell commands** go on their own row under the status: `⠦ Running shell… (7s · …)` then `  $ npm test`.
  - **Thinking once:** while a thought is in progress the status line says `Thinking…` (or `Thinking: <title>…`
    with the thought's bold title), and the transcript does not draw upstream's live `Thinking` row (thought group
    or activity label) at the same time. The finished `+ Thought · 5.0s` row appears as upstream's does. With
    `session.thinking` set to `show`, reasoning text still streams into the transcript.
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

## Rendering performance

Long sessions must not slow the TUI down: the cost of a frame and of an event does not grow with the transcript
(ocd-specific; the output is identical to rendering everything from scratch).

- **Event loop** (`app/event_loop.rs`): each iteration waits for anything
  (terminal input, a queued SSE event or async result, the 40 ms animation tick, the next frame), then drains
  everything that is ready — all pending terminal input first, then queued messages in batches of 64, re-checking
  input between batches and stopping after 8 ms. At most one frame is drawn per 16 ms (~60 fps), and only when
  something changed, an animation is visible (`App::animating`) or transcript rows are still being rendered. A flood
  of streaming deltas therefore costs one frame per 16 ms, not one per delta, and keys never wait behind a backlog.
- **Transcript cache** (`app/tcache.rs`): rendered lines are cached per transcript row (`transcript::Row`: a
  message, a part, a group or a footer). Every in-place edit of a message in the store calls `Message::touch`
  (revision 0); the cache stamps fresh revisions and, per frame, finds the first changed message and rebuilds the
  row list only from the start of that turn (a user message right after an `idle` marker). A row is reused when its
  *fingerprint* (row structure, revisions of the messages it reads — a footer reads its whole turn —, the per-row UI
  state it reads: expanded toggles, pending permission, pending inputs, background shells and their output, running
  subagent sessions, and the retry countdown second) or its *content key* (the same with the part's content instead
  of the message revision) is unchanged, so a text delta re-renders only the streaming part. Width, terminal width,
  theme, config, models, agent colours and the session agent drop all rows; the revert boundary, permission blocking
  and pending inputs rebuild the row list (rows are still reused). The background-work hint and the revert banner
  are recomputed each frame. Spinners stay out of cached lines (a placeholder glyph replaced at draw time).
- **Lazy rows:** rows are rendered bottom-up, only as far as the window reaches, and the rest within 8 ms per frame
  afterwards. Opening, resizing or re-theming a long session shows the visible part at once; until every row is
  rendered the line count and the message marks are unknown, the window is addressed from the bottom
  (`TranscriptCache::window_bottom`) and scrolling is unclamped. Message navigation (`App::transcript_complete`)
  finishes the render first. The window is drawn by reference (no copy of the transcript per frame).
- **Highlighting:** `ui/highlight.rs` caches syntect tokenization (style-independent) per (language, code), up to
  32 MB of code; finished code blocks of a streaming answer and re-wraps never parse again. The diff viewer caches
  each file's rendered diff by content, width and theme.
- **Other per-frame paths** read the tail only: the busy line's phase scans back to the last `idle` marker; the
  sidebar and usage footer search the last assistant message from the end; model lookups borrow the catalog
  (`App::models_ref`) instead of cloning it.
- **Tests and benchmarks** (`app/perf.rs`, `app/tcache_tests.rs`): the cache must equal `render_transcript_lines`
  (the from-scratch reference) line for line and mark for mark through a scripted session (streaming, tools,
  permissions, retries, queued prompts, compaction, revert, older pages, toggles, width/config/theme changes), and a
  lazily drawn frame must equal a complete one. `perf_frame_cost_independent_of_transcript_length` fails if a frame
  after one delta in a 1,500-message session costs more than 4 ms (release) or 3× a 16-message one;
  `perf_flood_keys_responsive` streams 200 deltas/s while pressing ↓ in a question dialog. Heavier, ignored:
  `cargo test -p oc-tui --release -- --ignored --nocapture perf_bench perf_flood_bench perf_pieces`.
  End to end: `scripts/tui_latency.py`.

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
| `app` | app state, command registry, key, mouse and paste dispatch, prompt submission, and the screen layout. Submodules: `tabs` (rail, strip and menu), `diffview`, `stats`, `terminal` (PTY pane), `composer` (subagent/shell drawer), `workspace` (open, worktrees and pair dialogs), `selection` (drag-select copy), `migration` (v1 import badge and status poller), `busy` (busy status line: turn tracking, phase derivation, rendering), `event_loop` (input-first batching, frame cap), `tcache` (incremental, lazy transcript rendering) |
| `mini` | `ocd mini`: a port of upstream's mini frontend (split-footer renderer, footer, panels, docks, scrollback rendering, transport, subagents, splash), one file per upstream module |
| `clipboard`, `attention`, `terminal` | clipboard (OSC 52 plus host tools), notifications and sounds, terminal palette probe (OSC 4/10/11) |

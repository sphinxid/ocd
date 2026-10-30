# ocd (CLI binary)

`ocd` is the Rust port of upstream `packages/cli` (opencode v2, `@opencode/cli` 2.1.4), built as if
upstream's `OPENCODE_CLI_NAME` build constant were `ocd`. Spec: `docs/spec/01-cli-config-service.md`
§0–§5, §7–§16. Build/test: `cargo build -p ocd`, `cargo test -p ocd`.

## Identity vs. compatibility

The program identity is `ocd`: root command name in help/usage/errors (`ocd <subcommand>`,
`Unrecognized flag: --x in command ocd run`), `--version` (`ocd v2.1.4`), completion scripts
(`###-begin-ocd-completions-###`), the root description (`ocd command line interface`), and every
message naming the executable (`Usage: ocd service ...`, `` run `ocd service unset disabled` ``,
`ocd acp: ...`, `ocd mini requires a TTY stdout`, `ocd upgrade` retries, `ocd stats` card heading,
ACP `Run \`ocd auth login\`` / terminal-auth `command: "ocd"`). Self-spawning (standalone
`serve --stdio`, service contenders) uses `std::env::current_exe()`.

Everything that makes ocd interchangeable with opencode keeps the upstream value: XDG
`…/opencode/` directories, `opencode.json[c]`/`.opencode/` discovery, `OPENCODE_*` env vars, the DB
file (`opencode.db`), log file, service registration/config files (`service*.json`) and port, lock
files, the HTTP API and `x-opencode-*` headers, Basic-auth username `opencode`, ACP protocol ids
(`opencode-login`, `opencode/...` `_meta` keys and extension method), plugin package names, the
`opencode-pty` daemon, the uninstall install-detection surface (`~/.opencode/bin/opencode`, Homebrew
`opencode*` formulae, `@opencode/cli` npm packages), and the `update`/`autoupdate` config keys and
`OPENCODE_DISABLE_AUTOUPDATE`. `ocd upgrade` and the update check use ocd's own GitHub releases instead of
`opencode.ai/update` (`src/release.rs`).

## Layout

| module | upstream | contents |
|---|---|---|
| `cli/spec.rs` | `commands/commands.ts` | `CLI_NAME = "ocd"` and the command tree as data: every command, alias, flag, argument, default, hidden flag and verbatim description |
| `cli/parse.rs` | `effect/cli` 4.0.6 `internal/lexer.ts`, `internal/parser.ts`, `Primitive.ts`, `CliError.ts` | Effect-compatible lexer (4.0.6: `--=x` is a value, `-abc=v` is `-a -b -c=v`), global-flag extraction, per-level scanning (subcommands, parent-flag errors, `--no-` negation, boolean literal consumption, `-abc` splitting, `--` operands), suggestions (Levenshtein ≤ 2), typed decoding (int/choice/key=value/non-empty/pair URL), exact error messages |
| `cli/help.rs` | `CliOutput.ts`, `internal/help.ts` | help renderer (DESCRIPTION/USAGE/ARGUMENTS/FLAGS/GLOBAL FLAGS/SUBCOMMANDS, column rules; colors as `TerminalCapabilities.canColor`: never with `TERM=dumb` or a non-empty `NO_COLOR`, else when `FORCE_COLOR` is set to anything but `0`/`false` or stdout is a TTY), `ERROR`/`ERRORS` blocks (grouped by tag), `--version` |
| `cli/mod.rs` | `Command.runWith` | control flow: the tree-wide global flags (built-ins, `--print-logs`, and every command's `withGlobalFlags` declarations), globals outside the active command's scope as `Unrecognized flag` errors, action flags (help > version > wizard > completions), parse errors → help + stderr + exit 1, settings (`--log-level`, `--print-logs`, and the `--standalone`/`--server` server settings read by `connection::resolve`) |
| `main.rs` | `src/index.ts` | SSH askpass mode, `Global.acquire`, logging (`role=cli`/`server`), `cli starting`, dispatch, `Error: <msg>` + exit 1 for handler failures |
| `client.rs` | `client/src/promise/generated/client.ts` | HTTP client keyed by operationId (from `assets/openapi.json`): declared vs `UnexpectedStatus`/`Transport` errors, `appendQuery` encoding, SSE decoding of `/api/event` |
| `service.rs` | `service-config.ts`, `client/src/effect/service.ts`, `service-probe.ts`, `service-contender.ts`, `pty-handoff.ts` | service config file (schema order, legacy repair/migration), probe, `discover`, `incumbent`, `ensure` (contender pool, 25 ms poll, 5→30 s spawn backoff, 120 s limit, 3× timeout recovery, version replacement), `stop`/terminate (SIGTERM, 5 s, SIGKILL), PTY handoff sidecar |
| `connection.rs` | `server-connection.ts`, `standalone.ts`, `env.ts` | `ServerConnection.resolve` (the command's own values, else the global server settings; `--server` probing + messages with the fetch error's text, e.g. `Could not reach server at <url>: ECONNREFUSED: Unable to connect. Is the computer able to access the url?`, `--standalone`/`disabled`, mismatch policies), standalone `serve --stdio --port 0` child with stdin lease |
| `web_ui.rs`, `build.rs` | `app-assets.ts`, `app-archive.ts`, `script/app-assets.ts` | locating the web UI that `serve` serves (see "Web UI") |
| `serve.rs`, `runtime.rs` | `server-process.ts` | `serve` default/stdio/service modes; service election by binding the fixed port, incumbent check, password persistence, registration + 5 s watchdog; runtime = `oc_storage::Database` → `oc_core::app::App` → `oc_core::runner::Engine` (`oc_core::services::default_services`) → `oc_backend::run_server` |
| `run.rs` | `run/run.ts`, `run/noninteractive.ts`, `session-target.ts`, `run/ui.ts`, `tui/src/mini/tool.ts` | `ocd run`: message/stdin merge, `--file` preparation, session selection (`--continue`/`--session`/`--fork`), title, prompt + SSE consumption, permission auto-reject/`--auto`, form cancellation, reconcile, default and JSON output, tool inline rendering, exit codes 0/1/130 |
| `acp.rs` | `acp/*.ts` | ACP JSON-RPC over stdio (see below) |
| `commands/*.rs` | `commands/handlers/**` | all other handlers |
| `ui.rs`, `clack.rs` | `@clack/core` + `@clack/prompts` 1.0.0-alpha.1, `wrap-ansi` 8.1.0, picocolors 1.1.1 | intro/outro/log/spinner with clack glyphs; `confirm`, `select`, `multiselect`, `text`, `password`, `autocomplete` and upstream's integration picker (`cli/src/ui/integration-picker.ts`, on `AutocompletePrompt`) on a port of clack's `Prompt`: keypresses as node `readline` parses them (50 ms escape timeout) and its line editing, frames NFC-normalised, hard-wrapped to the terminal and redrawn from the first changed line, re-rendered on `SIGWINCH` like `output.on("resize")`, `limitOptions` scrolling, validation errors; ctrl+d on an empty readline line leaves raw mode and stops reading until ctrl+c, as upstream's closed readline does |
| `askpass.rs` | `ssh-askpass.ts` | `OPENCODE_SSH_ASKPASS_PORT` helper mode |

`clap` is not used: its parser cannot reproduce Effect CLI semantics (parent flags given before a
subcommand are errors, `--flag true|yes|1` literal consumption, `-abc` → three short flags each
taking values, Effect's error texts and suggestion lists, `--help=false` still showing help, ...),
so the parser is a direct port and is verified against the real framework.

## Verification against the real Effect CLI

`tests/golden/*.json` were captured by running upstream's `commands.ts` + `framework/spec.ts`
(v2.2.2) under `effect@4.0.6` (bun, `--define OPENCODE_CLI_NAME='"ocd"'`) for ~250 command lines, with ocd's
root and `upgrade` descriptions, ocd's `upgrade --yes` flag and `--method` description patched into the
harness's copy of `commands.ts`. Apart from the version and those `upgrade` texts, the help, completions and
parse errors match the upstream 2.2.2 binary with `opencode` read as `ocd` (all 142 non-handler cases checked):

* `cli.json`: `--help` of every command, `--version`, `--completions bash|zsh|fish|powershell|sh|pwsh`, group
  commands without subcommand, ~70 parse-error cases (stdout, stderr, exit code), and the `--server`/`--standalone`
  scope matrix of `cli/test/server-connection.test.ts`.
* `parse.json`: the decoded handler input (`JSON.stringify(input)` incl. Effect `Option` encoding) and the
  server settings the handler sees (`{"server", "standalone"}`, `null` outside their scope) for ~110 valid
  command lines. The binary prints the same with the internal `OPENCODE_CLI_PARSE_DUMP=1`.
* `assets/completions/{bash,zsh,fish,powershell}` are the framework's generated scripts, embedded verbatim
  (`sh` prints bash, `pwsh` powershell).

Observed oracle behaviours reproduced: a group without subcommand prints help and exits **0**
(the spec says 1; Effect's `ShowHelp` without errors exits 0); `run ... -- x` passes `x`
twice (parsed operands + upstream's own `process.argv` re-read), exactly like upstream.

## Commands

| command | status |
|---|---|
| `$` (TUI) | `ServerConnection.resolve(mismatch: replace)` with the `Starting/Restarting background server...` notices, then `oc_tui::run`; exits with its code |
| `mini` | TTY check before resolution, `oc_tui::run_mini` with `--replay/--no-replay/--replay-limit` |
| `run` | complete (native path; the unused V1 bridge is not ported) |
| `serve` | default / `--stdio` / `--service` complete; remote tunnel (OpenTunnel) not supported |
| `service start/stop/restart/status/get/set/unset` | complete; `set remote true` and `pair --remote` report that OpenTunnel is unsupported (so `pair --yes`, upstream's consent to expose the server, is parsed but has nothing to accept) |
| `debug paths/config/agents` | complete |
| `auth list/export/import/login/logout/switch` | complete; login supports key/external/oauth/command methods and form answers (`--answer`); all prompts are the ported clack ones (the integration choice of `auth login` and `mcp auth` is upstream's integration picker; account and `session export` pickers are `autocomplete`) |
| `mcp list/add/auth/logout` | complete (strings from `oc_mcp::cli`; `add` uses `oc_config::jsonc_edit`) |
| `plugin list/check/update/remove` | server plugins via the API; TUI package plugins from `cli.json` are inspected too (installed version from upstream's npm cache layout, update check with `oc_plugin::npm::Npm::check`, `update` reinstalls with `Npm::update`); `remove` edits the global config and `cli.json` |
| `plugin add` | complete: installs with Bun into upstream's npm cache (`oc_plugin::npm`), resolves the `server`/root, `tui` and `gui-experimental` (CommonJS only) entrypoints with Bun's module resolution (upstream `Host.resolve`, including a local directory's own `name`/`exports`) and writes the global config (`plugins`, for a server or GUI part) or `cli.json` (TUI only), with upstream's messages. Bun comes from `OCD_BUN_PATH`/`OPENCODE_BUN_PATH` or the pinned 1.4.2 download. TUI plugins are recorded but ocd's TUI does not run them (documented incompatibility) |
| `models`, `reload`, `api`, `pair`, `session list/delete/export/import`, `stats` | complete (`pair` renders the QR code like `@opencode/util/qr`: one byte-mode segment, level L, via the `qrcodegen` crate) |
| `upgrade` | ocd's GitHub releases (`src/release.rs`): `GET /repos/sphinxid/ocd/releases/latest` or `/releases/tags/v<target>` (User-Agent, optional `GITHUB_TOKEN`/`GH_TOKEN`), semver comparison with the build's version part (`0.2.0+3.abc` counts as 0.2.0; a date-stamped or missing build is a local build and needs a confirmation or `--yes`), "already installed" skip, an explicit older target downgrades, the archive for `PLATFORM` checked against `SHA256SUMS` (and GitHub's `digest`), extracted in memory, written next to the real executable path with its permissions, run once (`--version`, empty directories) and renamed over it (Windows: the running exe moves to `ocd.exe.old`, removed on the next start). Fails before downloading when the folder is not writable. A running background service of the channel is restarted through `service restart`'s code path. `--method` is ignored with a warning. Errors use upstream's `UpgradeError` layout |
| update check | `release::run`/`check`/`apply` behind the TUI's `UpdaterHooks` (`commands/mod.rs` `updater_hooks`): every 10 minutes, the release lookup cached a day in `<cache>/ocd-update.json` (an hour after a failure); policy from `update`/`autoupdate` in the global config (default `notify`; `auto` installs without restarting the service), `OPENCODE_DISABLE_AUTOUPDATE`; local builds never check |
| `uninstall` | native adaptation: method detection recognises `~/.opencode/bin/opencode` (curl, compared after resolving links), Homebrew cellars, WinGet's `Anomaly.OpenCode` package directory and an npm `package.json` owning the executable (probing the package manager named in its path first). Package-manager commands and the uninstall plan (services, cache, shell PATH lines, package) follow upstream |
| `acp` | see below |
| `--wizard` | not supported (prints an error, exit 1) |

## Web UI

Every `serve` mode (foreground, `--stdio` for `--standalone` clients, `--service` for the
background service) serves upstream's web app (`packages/app`) on non-API paths before auth,
exactly like upstream's `WebUi.handler()` (behaviour in `crates/oc-server/README.md`, "Web UI").
Upstream has no `web` command and opens no browser for it (`auth login` and form URLs are the
only browser openers); neither does ocd. Open the server URL yourself and sign in, or pair with
`ocd pair` / `/auth/connect/<code>`.

**Building it.** Upstream embeds the app in the binary (`virtual:opencode-app-assets`). The
archive is ~24 MB (1 690 files, 87 MB unpacked: the Office preview engines and fonts dominate),
so it is **not committed**:

```sh
scripts/build-web-ui.sh <upstream-checkout>     # BUN=<bun 1.4.4>, OPENCODE_CHANNEL (default latest)
cargo build -p ocd                              # embeds crates/ocd/web-ui/app-archive.bin
```

The script calls upstream's own `buildAppArchive` (`packages/cli/script/app-assets.ts`): `vite
build` of `packages/app` with `OPENCODE_CHANNEL` and `VITE_OPENCODE_SERVER_MODE=origin` (written
to the checkout's git-ignored `packages/app/dist`, as upstream's build does), every file but
source maps brotli-compressed at quality 11, sorted, in `app-archive.ts`'s format. The result is
byte-identical to the archive upstream's `script/build.ts` embeds for the same channel; the script
refuses output that mentions the checkout, the repo or `$HOME`. `build.rs` embeds
`OCD_WEB_UI_ARCHIVE` (build-time env) or `crates/ocd/web-ui/app-archive.bin` with
`include_bytes!` after checking that its index decodes (a bad file is skipped with a cargo
warning); `crates/ocd/web-ui/` is watched, so adding or replacing the archive rebuilds ocd.

**Lookup at runtime** (`web_ui::locate`): `OCD_WEB_UI` (an archive file or an unpacked `dist`
directory, like upstream local builds reading `packages/app/dist`; unreadable → `serve` fails
with `OCD_WEB_UI=<path>: ...`) → the embedded archive → `app-archive.bin` next to the executable
(unreadable → warning, skipped) → none. Without a web UI every non-API path answers an empty 404
(`_assets/*`, `icons/*` with `no-store`), the same as an upstream build made with
`--skip-web-ui`. `content-type` uses upstream's `mime-types` table (`oc_tools::mime`).

## ACP (`ocd acp`)

Newline-delimited JSON-RPC 2.0 on stdio; a private standalone server (`OPENCODE_CLIENT=acp`).
Implemented: `initialize`, `authenticate`, `session/new|load|list|delete|resume|close|fork|
set_config_option|set_mode|prompt`, `session/cancel`, inbound `$/cancel_request` for prompts,
outbound `session/request_permission` / `elicitation/create` with `$/cancel_request` withdrawal,
`opencode/session/child_update`, catalog (models/modes/commands, reload on `*.updated`), config
options (model/effort/mode), the event fold (text, reasoning, tools, retries, compaction standard and
marker forms, child sessions), turn stop reasons/usage/`usage_update`, error mapping (`Invalid params:`
/ `Internal error:` / `Authentication required:` / `"Method not found"`), history replay,
additional directories, MCP registration, 32 MiB message limit, exit codes (`ocd acp: server
exited unexpectedly (...)`), and file diffs (`acp/permission.ts`): permission requests carry a
`diff` preview per file (`N files` title for several) and completed edit/patch calls (live and in
history replay) append a `diff` per changed file, the old text rebuilt by reversing the recorded
patch against the file (jsdiff `parsePatch`/`reversePatch`/`applyPatch`, ported in
`oc_tools::jsdiff`) or, for the patch tool's trimmed diffs, from its hunks. Not ported: background
following of child sessions after the root turn ends.

## Notes / deviations

* `run` reports a server it cannot reach like any run error (`{"type":"error",...,"sessionID":""}` with
  `--format json`), exit 1.
* Service files: the config is written to `<file>.<pid>.tmp` and renamed, retried 20 times 10 ms apart (also the
  registration write); `XDG_STATE_HOME` is dropped from the saved environment the service is started with. A
  service port held by another process names it (`PortOwner`: `lsof -nP -iTCP:<port> -sTCP:LISTEN -Fpn`, then
  `ps -o command=`, 1 s each), e.g. `... already in use by PID 123 (node server.js)`.
* Shutdown deadline (`shutdown-deadline.ts`): once `serve` gets a stop signal its teardown has 10 s, then
  `shutdown did not finish within 10s; exiting. Still pending:` and exit 1; a second SIGINT/SIGTERM exits at once
  with `received <SIG> again during shutdown; ...`. ocd tracks no teardown labels, so the list is always
  upstream's "nothing tracked" line. Other commands keep the default signal handling (they have no teardown that
  can hang), and SIGHUP is not turned into SIGTERM.
* `plugin list` shows server plugins only; upstream also lists configured and discovered TUI plugins (and 2.2
  only hides a local one whose path a server plugin reports exactly). ocd's TUI does not load TUI plugins.

* Uncaught handler failures print `Error: <message>` (no JS stack); CLI errors that escape the help
  channel print `~effect/cli/CliError/<Tag>: <message>` (first line of `Cause.pretty`).
* `toLocaleString()` (session list/export) is emulated for the en-US locale; `localeCompare` uses an
  ICU-root approximation (`term::locale_cmp`).
* The background service election probes the port with a plain bind before building the runtime
  (oc-server binds only once the backend exists) and then retries on `EADDRINUSE` as upstream.
* `run` and the last events of a run: upstream's server writes events to the event stream only when its
  runner waits for I/O, after the HTTP answers settled before that, so `session.wait` answers before the
  events published since the runner's last I/O wait and `run` skips them as finalizing (no `> agent · model`
  header / `step_start` for a step that fails at once, the last step's text from the projected message,
  no last `step_finish`). ocd's server delivers events at once, so `run.rs` (`Shared::consume`) emulates
  this: it holds each live event for 150 ms and handles the held ones as finalizing when the execution ends
  meanwhile, releasing them at once where upstream's runner always waits for I/O (a new step's provider
  request, the end snapshot of a step that started with one, permission and form requests). Upstream's own
  output there depends on timing (e.g. a provider whose whole answer arrives in one read); the emulation
  matches it for providers that answer at once and for streams slower than the hold.
* `OPENCODE_PTY_HANDOFF` is validated at boot and left in the environment for the backend
  (`oc_persistent_pty::handoff::take_from_env()`); the client side publishes/consumes the
  `<registration>.pty-handoff` sidecar via `POST /api/experimental/persistent-pty/handoff`.

## Tests

* unit tests in each module (parser, primitives, help, client query encoding, service config, run
  helpers/tool rendering, stats rendering, auth form/integration resolution, plugin table, upgrade
  formatting, ACP catalog/config options/prompt conversion/fold/elicitation);
* `tests/golden.rs`: help/version/completions/errors and decoded inputs vs. the Effect oracle;
* `tests/e2e.rs`: `debug paths`, version + parse errors, background service lifecycle with
  `debug config` and `models`, `models --standalone`, `run` (default and JSON) against a mock
  OpenAI-compatible SSE provider with a tool call and a non-retryable 400, outside and inside a git
  repository (which steps print live), missing-message and flag-conflict errors;
* `tests/web_ui.rs`: `serve` answers non-API paths with the web UI from `OCD_WEB_UI` (SPA
  fallback, headers, 404 `no-store`, API still 401) and rejects an unreadable override;
  `web_ui::tests` cover the lookup order;
* `tests/acp.rs`: ACP initialize/authenticate/new/prompt/list/unknown-method and EOF exit over stdio.
* `tests/upgrade.rs` (Unix): `ocd upgrade` of a copy of the binary against a local GitHub stand-in
  (`tests/common/releases.rs`, `OCD_TEST_RELEASES_API` and `OCD_TEST_BUILD_VERSION`, both test only): same,
  newer, older and explicit versions, local builds, checksum and missing-asset failures, a binary that does not
  run, an unwritable folder, the service restart, and a replay of the real v0.1.1 release when
  `$OCD_SCRATCH/releases/v0.1.1/` has it; `release::tests` cover parsing, ordering, archives, errors, the daily
  cache and the notify/auto policy.

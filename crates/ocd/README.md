# ocd (CLI binary)

`ocd` is the Rust port of upstream `packages/cli` (opencode v2, `@opencode/cli` 2.0.26), built as if
upstream's `OPENCODE_CLI_NAME` build constant were `ocd`. Spec: `docs/spec/01-cli-config-service.md`
§0–§5, §7–§16. Build/test: `cargo build -p ocd`, `cargo test -p ocd`.

## Identity vs. compatibility

The program identity is `ocd`: root command name in help/usage/errors (`ocd <subcommand>`,
`Unrecognized flag: --x in command ocd run`), `--version` (`ocd v2.0.26`), completion scripts
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
`opencode-pty` daemon, and the upgrade/uninstall install-detection surface (`~/.opencode/bin/opencode`,
Homebrew `opencode*` formulae, `@opencode/cli` npm packages, `opencode.ai` update API).

## Layout

| module | upstream | contents |
|---|---|---|
| `cli/spec.rs` | `commands/commands.ts` | `CLI_NAME = "ocd"` and the command tree as data: every command, alias, flag, argument, default, hidden flag and verbatim description |
| `cli/parse.rs` | `effect/cli` 4.0.1 `internal/lexer.ts`, `internal/parser.ts`, `Primitive.ts`, `CliError.ts` | Effect-compatible lexer, global-flag extraction, per-level scanning (subcommands, parent-flag errors, `--no-` negation, boolean literal consumption, `-abc` splitting, `--` operands), suggestions (Levenshtein ≤ 2), typed decoding (int/choice/key=value/non-empty/pair URL), exact error messages |
| `cli/help.rs` | `CliOutput.ts`, `internal/help.ts` | help renderer (DESCRIPTION/USAGE/ARGUMENTS/FLAGS/GLOBAL FLAGS/SUBCOMMANDS, column rules, colors when stdout is a TTY and `NO_COLOR` unset), `ERROR`/`ERRORS` blocks (grouped by tag), `--version` |
| `cli/mod.rs` | `Command.runWith` | control flow: action flags (help > version > wizard > completions), parse errors → help + stderr + exit 1, settings (`--log-level`, `--print-logs`) |
| `main.rs` | `src/index.ts` | SSH askpass mode, `Global.acquire`, logging (`role=cli`/`server`), `cli starting`, dispatch, `Error: <msg>` + exit 1 for handler failures |
| `client.rs` | `client/src/promise/generated/client.ts` | HTTP client keyed by operationId (from `assets/openapi.json`): declared vs `UnexpectedStatus`/`Transport` errors, `appendQuery` encoding, SSE decoding of `/api/event` |
| `service.rs` | `service-config.ts`, `client/src/effect/service.ts`, `service-probe.ts`, `service-contender.ts`, `pty-handoff.ts` | service config file (schema order, legacy repair/migration), probe, `discover`, `incumbent`, `ensure` (contender pool, 25 ms poll, 5→30 s spawn backoff, 120 s limit, 3× timeout recovery, version replacement), `stop`/terminate (SIGTERM, 5 s, SIGKILL), PTY handoff sidecar |
| `connection.rs` | `server-connection.ts`, `standalone.ts`, `env.ts` | `ServerConnection.resolve` (`--server` probing + messages, `--standalone`/`disabled`, mismatch policies), standalone `serve --stdio --port 0` child with stdin lease |
| `serve.rs`, `runtime.rs` | `server-process.ts` | `serve` default/stdio/service modes; service election by binding the fixed port, incumbent check, password persistence, registration + 5 s watchdog; runtime = `oc_storage::Database` → `oc_core::app::App` → `oc_core::runner::Engine` (`oc_core::services::default_services`) → `oc_backend::run_server` |
| `run.rs` | `run/run.ts`, `run/noninteractive.ts`, `session-target.ts`, `run/ui.ts`, `tui/src/mini/tool.ts` | `ocd run`: message/stdin merge, `--file` preparation, session selection (`--continue`/`--session`/`--fork`), title, prompt + SSE consumption, permission auto-reject/`--auto`, form cancellation, reconcile, default and JSON output, tool inline rendering, exit codes 0/1/130 |
| `acp.rs` | `acp/*.ts` | ACP JSON-RPC over stdio (see below) |
| `commands/*.rs` | `commands/handlers/**` | all other handlers |
| `ui.rs` | `@clack/prompts` | intro/outro/log/spinner/select/confirm/text with clack glyphs |
| `askpass.rs` | `ssh-askpass.ts` | `OPENCODE_SSH_ASKPASS_PORT` helper mode |

`clap` is not used: its parser cannot reproduce Effect CLI semantics (parent flags given before a
subcommand are errors, `--flag true|yes|1` literal consumption, `-abc` → three short flags each
taking values, Effect's error texts and suggestion lists, `--help=false` still showing help, ...),
so the parser is a direct port and is verified against the real framework.

## Verification against the real Effect CLI

`tests/golden/*.json` were captured by running upstream's `commands.ts` + `framework/spec.ts`
under `effect@4.0.1` (bun, `--define OPENCODE_CLI_NAME='"ocd"'`) for ~200 command lines (the root
description was then substituted with ocd's):

* `cli.json`: `--help` of every command, `--version`, `--completions bash|zsh|fish|sh`, group commands
  without subcommand, and ~70 parse-error cases (stdout, stderr, exit code).
* `parse.json`: the decoded handler input (`JSON.stringify(input)` incl. Effect `Option` encoding) for
  ~90 valid command lines. The binary prints the same with the internal `OPENCODE_CLI_PARSE_DUMP=1`.
* `assets/completions/{bash,zsh,fish}` are the framework's generated scripts, embedded verbatim.

Observed oracle behaviours reproduced: a group without subcommand prints help and exits **0**
(the spec says 1; Effect 4.0.1's `ShowHelp` without errors exits 0); `run ... -- x` passes `x`
twice (parsed operands + upstream's own `process.argv` re-read), exactly like upstream.

## Commands

| command | status |
|---|---|
| `$` (TUI) | `ServerConnection.resolve(mismatch: replace)` with the `Starting/Restarting background server...` notices, then `oc_tui::run`; exits with its code |
| `mini` | TTY check before resolution, `oc_tui::run_mini` with `--replay/--no-replay/--replay-limit` |
| `run` | complete (native path; the unused V1 bridge is not ported) |
| `serve` | default / `--stdio` / `--service` complete; remote tunnel (OpenTunnel) not supported |
| `service start/stop/restart/status/get/set/unset` | complete; `set remote true` and `pair --remote` report that OpenTunnel is unsupported |
| `debug paths/config/agents` | complete |
| `auth list/export/import/login/logout/switch` | complete; login supports key/external/oauth/command methods and form answers (`--answer`); interactive pickers use a simplified clack-style UI (multiselect asks for comma-separated values) |
| `mcp list/add/auth/logout` | complete (strings from `oc_mcp::cli`; `add` uses `oc_config::jsonc_edit`) |
| `plugin list/check/update/remove` | server plugins via the API; TUI package plugins from `cli.json` are inspected too (installed version from upstream's npm cache layout, update check with `oc_plugin::npm::Npm::check`, `update` reinstalls with `Npm::update`); `remove` edits the global config and `cli.json` |
| `plugin add` | complete: installs with Bun into upstream's npm cache (`oc_plugin::npm`), resolves the `server`/root and `tui` entrypoints with Bun's module resolution (upstream `Host.resolve`) and writes the global config (`plugins`) or `cli.json`, with upstream's messages. Bun comes from `OCD_BUN_PATH`/`OPENCODE_BUN_PATH` or the pinned 1.4.2 download. TUI plugins are recorded but ocd's TUI does not run them (documented incompatibility) |
| `models`, `reload`, `api`, `pair`, `session list/delete/export/import`, `stats` | complete (`pair` renders the QR code with the `qrcode` crate; module layout may differ from `uqr`) |
| `upgrade` / `uninstall` | native adaptation: method detection recognises `~/.opencode/bin/opencode` (curl), Homebrew cellars and an npm `package.json` owning the executable; otherwise `--method` is required. Release lookup, package-manager commands, `UpgradeError` formatting and the uninstall plan (services, cache, shell PATH lines, package) follow upstream |
| `acp` | see below |
| `--wizard` | not supported (prints an error, exit 1) |

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
exited unexpectedly (...)`). Not ported: completed-edit diff enrichment (`acp/permission.ts`
previews/diffs) and background following of child sessions after the root turn ends.

## Notes / deviations

* Uncaught handler failures print `Error: <message>` (no JS stack); CLI errors that escape the help
  channel print `~effect/cli/CliError/<Tag>: <message>` (first line of `Cause.pretty`).
* `toLocaleString()` (session list/export) is emulated for the en-US locale; `localeCompare` uses an
  ICU-root approximation (`term::locale_cmp`).
* The background service election probes the port with a plain bind before building the runtime
  (oc-server binds only once the backend exists) and then retries on `EADDRINUSE` as upstream.
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
  OpenAI-compatible SSE provider with a tool call, missing-message and flag-conflict errors;
* `tests/acp.rs`: ACP initialize/authenticate/new/prompt/list/unknown-method and EOF exit over stdio.
